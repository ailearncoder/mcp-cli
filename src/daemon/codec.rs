#![cfg(unix)]
//! Bounded, fail-closed NDJSON framing for daemon IPC.
//!
//! [`IPC_MAX_FRAME_SIZE`] applies to the JSON payload bytes only. The trailing
//! LF, or both bytes of a CRLF terminator, are not counted toward the limit.

use std::fmt;

use serde::{Serialize, de::DeserializeOwned};

/// Maximum UTF-8 JSON payload size for both requests and responses (1 MiB).
pub const IPC_MAX_FRAME_SIZE: usize = 1024 * 1024;

/// Stable, payload-free NDJSON framing failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    FrameTooLarge,
    EmptyFrame,
    BareCarriageReturn,
    InvalidUtf8,
    InvalidJson,
    InvalidMessage,
    EmbeddedNewline,
    TruncatedFrame,
    CodecFailed,
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::FrameTooLarge => "IPC frame exceeds the maximum payload size",
            Self::EmptyFrame => "empty IPC frame",
            Self::BareCarriageReturn => "bare carriage return in IPC frame",
            Self::InvalidUtf8 => "IPC frame is not valid UTF-8",
            Self::InvalidJson => "IPC frame is not valid JSON",
            Self::InvalidMessage => "IPC frame has an invalid message shape",
            Self::EmbeddedNewline => "encoded IPC frame contains an embedded newline",
            Self::TruncatedFrame => "IPC stream ended with an unterminated frame",
            Self::CodecFailed => "IPC codec is unavailable after a framing failure",
        })
    }
}

impl std::error::Error for FrameError {}

/// Incremental NDJSON decoder with bounded memory and fail-closed semantics.
///
/// A codec that reports any error is poisoned: its buffered bytes are erased
/// and all later operations return [`FrameError::CodecFailed`]. This prevents
/// callers from accidentally resynchronizing an untrusted stream after a
/// malformed or oversized frame.
#[derive(Debug, Default)]
pub struct NdjsonCodec {
    buffer: Vec<u8>,
    failed: bool,
}

impl NdjsonCodec {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_failed(&self) -> bool {
        self.failed
    }

    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    /// Feeds arbitrary bytes and returns every complete JSON payload.
    ///
    /// Returned payloads exclude the LF/CRLF terminator. UTF-8 and JSON syntax
    /// are validated before a frame is returned. Use [`Self::push_request_frames`]
    /// at an RPC boundary that must recover from request-level JSON errors.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
        let frames = self.push_request_frames(bytes)?;
        for frame in &frames {
            if frame.is_empty() {
                return self.fail(FrameError::EmptyFrame);
            }
            if serde_json::from_slice::<serde_json::Value>(frame).is_err() {
                return self.fail(FrameError::InvalidJson);
            }
        }
        Ok(frames)
    }

    /// Feeds arbitrary bytes and returns complete UTF-8 request frames without
    /// validating their JSON syntax or model shape.
    ///
    /// This is the daemon worker boundary: malformed JSON is a recoverable RPC
    /// error, while framing violations (oversize, invalid UTF-8, or bare CR)
    /// poison the codec and require closing only that client connection.
    pub fn push_request_frames(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
        self.ensure_usable()?;
        let mut frames = Vec::new();

        for &byte in bytes {
            if byte == b'\n' {
                let frame = self.complete_request_frame()?;
                frames.push(frame);
                continue;
            }

            if self.buffer.last() == Some(&b'\r') {
                return self.fail(FrameError::BareCarriageReturn);
            }

            if byte == b'\r' {
                // One provisional CR beyond the payload limit is safe because
                // it can only be the excluded half of a CRLF terminator.
                if self.buffer.len() > IPC_MAX_FRAME_SIZE {
                    return self.fail(FrameError::FrameTooLarge);
                }
                self.buffer.push(byte);
                continue;
            }

            if self.buffer.len() >= IPC_MAX_FRAME_SIZE {
                return self.fail(FrameError::FrameTooLarge);
            }
            self.buffer.push(byte);
        }

        Ok(frames)
    }

    /// Feeds bytes and deserializes all completed frames into one IPC model.
    pub fn push_messages<T>(&mut self, bytes: &[u8]) -> Result<Vec<T>, FrameError>
    where
        T: DeserializeOwned,
    {
        let frames = self.push(bytes)?;
        let mut messages = Vec::with_capacity(frames.len());
        for frame in frames {
            match decode_message(&frame) {
                Ok(message) => messages.push(message),
                Err(error) => return self.fail(error),
            }
        }
        Ok(messages)
    }

    /// Completes the stream. Only an empty buffer is a valid EOF.
    pub fn finish(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        self.ensure_usable()?;
        if self.buffer.is_empty() {
            return Ok(None);
        }
        if self.buffer.last() == Some(&b'\r') {
            return self.fail(FrameError::BareCarriageReturn);
        }
        self.fail(FrameError::TruncatedFrame)
    }

    fn complete_request_frame(&mut self) -> Result<Vec<u8>, FrameError> {
        let had_crlf = self.buffer.last() == Some(&b'\r');
        if had_crlf {
            self.buffer.pop();
        }

        if self.buffer.len() > IPC_MAX_FRAME_SIZE {
            return self.fail(FrameError::FrameTooLarge);
        }
        if self.buffer.contains(&b'\r') {
            return self.fail(FrameError::BareCarriageReturn);
        }
        if std::str::from_utf8(&self.buffer).is_err() {
            return self.fail(FrameError::InvalidUtf8);
        }

        Ok(std::mem::take(&mut self.buffer))
    }

    fn ensure_usable(&self) -> Result<(), FrameError> {
        if self.failed {
            Err(FrameError::CodecFailed)
        } else {
            Ok(())
        }
    }

    fn fail<T>(&mut self, error: FrameError) -> Result<T, FrameError> {
        self.buffer.clear();
        self.failed = true;
        Err(error)
    }
}

/// Serializes one model as a bounded LF-terminated NDJSON frame.
///
/// Model validation performed by custom `Serialize` implementations is part
/// of this boundary. Serializer diagnostics are deliberately not surfaced.
pub fn encode_message<T>(message: &T) -> Result<Vec<u8>, FrameError>
where
    T: Serialize,
{
    let mut payload = serde_json::to_vec(message).map_err(|_| FrameError::InvalidMessage)?;
    if payload.contains(&b'\n') || payload.contains(&b'\r') {
        return Err(FrameError::EmbeddedNewline);
    }
    if payload.len() > IPC_MAX_FRAME_SIZE {
        return Err(FrameError::FrameTooLarge);
    }
    payload.push(b'\n');
    Ok(payload)
}

/// Deserializes one already-delimited payload and enforces the same size and
/// newline rules used by the streaming decoder.
pub fn decode_message<T>(payload: &[u8]) -> Result<T, FrameError>
where
    T: DeserializeOwned,
{
    if payload.len() > IPC_MAX_FRAME_SIZE {
        return Err(FrameError::FrameTooLarge);
    }
    if payload.is_empty() {
        return Err(FrameError::EmptyFrame);
    }
    if payload.contains(&b'\n') || payload.contains(&b'\r') {
        return Err(FrameError::EmbeddedNewline);
    }
    if std::str::from_utf8(payload).is_err() {
        return Err(FrameError::InvalidUtf8);
    }
    serde_json::from_slice(payload).map_err(|error| {
        if error.is_syntax() || error.is_eof() {
            FrameError::InvalidJson
        } else {
            FrameError::InvalidMessage
        }
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::daemon::{IpcOperation, IpcRequest, IpcResponse};

    fn json_payload_of_size(size: usize) -> Vec<u8> {
        assert!(size >= 8);
        let mut payload = b"{\"x\":\"".to_vec();
        payload.extend(std::iter::repeat_n(b'a', size - 8));
        payload.extend_from_slice(b"\"}");
        assert_eq!(payload.len(), size);
        payload
    }

    #[test]
    fn accepts_lf_and_crlf_but_does_not_count_terminators() {
        let payload = json_payload_of_size(IPC_MAX_FRAME_SIZE);
        let mut input = payload.clone();
        input.extend_from_slice(b"\r\n");

        let mut codec = NdjsonCodec::new();
        assert_eq!(codec.push(&input).expect("maximum CRLF frame"), [payload]);
        assert_eq!(codec.finish(), Ok(None));
    }

    #[test]
    fn parses_byte_by_byte_and_preserves_a_partial_frame() {
        let request = IpcRequest::new("byte-1", IpcOperation::Ping).expect("valid request");
        let encoded = encode_message(&request).expect("encode request");
        let mut codec = NdjsonCodec::new();
        let mut decoded = Vec::new();

        for byte in encoded {
            decoded.extend(
                codec
                    .push_messages::<IpcRequest>(&[byte])
                    .expect("byte chunk"),
            );
        }

        assert_eq!(decoded, [request]);
        assert_eq!(codec.buffered_len(), 0);
    }

    #[test]
    fn handles_split_and_coalesced_multiple_frames() {
        let one = IpcRequest::new("one", IpcOperation::Ping).expect("request");
        let two = IpcRequest::new("two", IpcOperation::GetInstructions).expect("request");
        let mut wire = encode_message(&one).expect("encode");
        wire.extend(encode_message(&two).expect("encode"));
        let split = wire.len() / 3;
        let mut codec = NdjsonCodec::new();

        assert!(
            codec
                .push_messages::<IpcRequest>(&wire[..split])
                .expect("first chunk")
                .is_empty()
        );
        assert_eq!(
            codec
                .push_messages::<IpcRequest>(&wire[split..])
                .expect("coalesced remainder"),
            [one, two]
        );
    }

    #[test]
    fn eof_requires_an_empty_buffer() {
        let mut empty = NdjsonCodec::new();
        assert_eq!(empty.finish(), Ok(None));

        let mut truncated = NdjsonCodec::new();
        assert!(
            truncated
                .push(br#"{\"id\":\"x\"}"#)
                .expect("partial")
                .is_empty()
        );
        assert_eq!(truncated.finish(), Err(FrameError::TruncatedFrame));
    }

    #[test]
    fn rejects_invalid_utf8_json_empty_lines_and_bare_cr() {
        let cases: &[(&[u8], FrameError)] = &[
            (&[0xff, b'\n'], FrameError::InvalidUtf8),
            (b"{]\n", FrameError::InvalidJson),
            (b"\n", FrameError::EmptyFrame),
            (b"{}\rX", FrameError::BareCarriageReturn),
        ];

        for &(input, expected) in cases {
            let mut codec = NdjsonCodec::new();
            assert_eq!(codec.push(input), Err(expected));
            assert!(codec.is_failed());
        }

        let mut trailing_cr = NdjsonCodec::new();
        assert!(
            trailing_cr
                .push(b"{}\r")
                .expect("provisional CR")
                .is_empty()
        );
        assert_eq!(trailing_cr.finish(), Err(FrameError::BareCarriageReturn));
    }

    #[test]
    fn accepts_exact_limit_and_rejects_limit_plus_one_without_unbounded_growth() {
        let exact = json_payload_of_size(IPC_MAX_FRAME_SIZE);
        let mut exact_wire = exact.clone();
        exact_wire.push(b'\n');
        let mut codec = NdjsonCodec::new();
        assert_eq!(codec.push(&exact_wire).expect("exact limit"), [exact]);

        let oversized = json_payload_of_size(IPC_MAX_FRAME_SIZE + 1);
        let mut codec = NdjsonCodec::new();
        assert_eq!(codec.push(&oversized), Err(FrameError::FrameTooLarge));
        assert_eq!(codec.buffered_len(), 0);
        assert!(codec.is_failed());
    }

    #[test]
    fn any_error_permanently_poisoned_the_codec_and_erases_state() {
        let mut codec = NdjsonCodec::new();
        assert_eq!(codec.push(b"not-json\n"), Err(FrameError::InvalidJson));
        assert_eq!(codec.buffered_len(), 0);
        assert_eq!(codec.push(b"{}\n"), Err(FrameError::CodecFailed));
        assert_eq!(codec.finish(), Err(FrameError::CodecFailed));
    }

    #[test]
    fn request_and_response_round_trip_through_codec() {
        let request = IpcRequest::new(
            "call-1",
            IpcOperation::CallTool {
                tool_name: "lookup".into(),
                args: serde_json::from_value(json!({"query": "rust"})).expect("object"),
            },
        )
        .expect("request");
        let response = IpcResponse::success("call-1", json!({"content": []})).expect("response");

        let mut request_codec = NdjsonCodec::new();
        assert_eq!(
            request_codec
                .push_messages::<IpcRequest>(&encode_message(&request).expect("encode request"))
                .expect("decode request"),
            [request]
        );
        let mut response_codec = NdjsonCodec::new();
        assert_eq!(
            response_codec
                .push_messages::<IpcResponse>(&encode_message(&response).expect("encode response"))
                .expect("decode response"),
            [response]
        );
    }

    #[test]
    fn decode_message_enforces_utf8_json_shape_and_size() {
        assert_eq!(
            decode_message::<IpcRequest>(&[0xff]),
            Err(FrameError::InvalidUtf8)
        );
        assert_eq!(
            decode_message::<IpcRequest>(b"{"),
            Err(FrameError::InvalidJson)
        );
        assert_eq!(
            decode_message::<IpcRequest>(br#"{"id":"x","type":"bogus"}"#),
            Err(FrameError::InvalidMessage)
        );
        assert_eq!(
            decode_message::<serde_json::Value>(&vec![b' '; IPC_MAX_FRAME_SIZE + 1]),
            Err(FrameError::FrameTooLarge)
        );
    }
}
