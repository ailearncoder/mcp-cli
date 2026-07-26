//! Secret redaction and diagnostic policy.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    io::Write,
    sync::Mutex,
};

use crate::output::{ANSI_MAGENTA, ANSI_YELLOW, DiagnosticSink, StylePolicy, style_fragment};

/// Stable marker used in place of one or more overlapping secrets.
pub const REDACTED: &[u8] = b"[REDACTED]";

/// Deduplicated non-empty byte strings that must never reach diagnostics.
///
/// The custom `Debug` implementation deliberately reports only aggregate
/// metadata: formatting a `SecretSet` must not itself disclose credentials.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretSet {
    values: BTreeSet<Vec<u8>>,
    max_secret_len: usize,
}

impl fmt::Debug for SecretSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretSet")
            .field("len", &self.len())
            .field("max_secret_len", &self.max_secret_len)
            .finish()
    }
}

impl SecretSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a secret. Empty values are ignored and duplicate values are
    /// stored only once. Returns whether a new value was inserted.
    pub fn insert(&mut self, value: impl AsRef<[u8]>) -> bool {
        let value = value.as_ref();
        if value.is_empty() {
            return false;
        }

        let inserted = self.values.insert(value.to_vec());
        if inserted {
            self.max_secret_len = self.max_secret_len.max(value.len());
        }
        inserted
    }

    /// Registers a configured environment value. The name is accepted for a
    /// convenient loader API but is intentionally not retained.
    pub fn register_env(&mut self, _name: impl AsRef<str>, value: impl AsRef<[u8]>) -> bool {
        self.insert(value)
    }

    /// Registers a configured HTTP header value. All configured header values
    /// are sensitive, including but not limited to Authorization and Cookie.
    pub fn register_header(&mut self, _name: impl AsRef<str>, value: impl AsRef<[u8]>) -> bool {
        self.insert(value)
    }

    pub fn register_authorization(&mut self, value: impl AsRef<[u8]>) -> bool {
        self.insert(value)
    }

    pub fn register_cookie(&mut self, value: impl AsRef<[u8]>) -> bool {
        self.insert(value)
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn max_secret_len(&self) -> usize {
        self.max_secret_len
    }

    /// Redacts text without assuming registered secrets align to UTF-8 scalar
    /// boundaries. Lossy conversion is intentional for that pathological case.
    pub fn redact(&self, text: &str) -> String {
        String::from_utf8_lossy(&self.redact_bytes(text.as_bytes())).into_owned()
    }

    /// Redacts arbitrary bytes. Invalid UTF-8 is never decoded here, so server
    /// stderr cannot panic or bypass matching through a decoding failure.
    pub fn redact_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        redact_covered(bytes, &self.coverage(bytes))
    }

    fn coverage(&self, bytes: &[u8]) -> Vec<bool> {
        let mut covered = vec![false; bytes.len()];
        for secret in &self.values {
            if secret.len() > bytes.len() {
                continue;
            }
            for start in 0..=bytes.len() - secret.len() {
                if bytes[start..].starts_with(secret) {
                    covered[start..start + secret.len()].fill(true);
                }
            }
        }
        covered
    }
}

/// Incremental byte redactor that prevents credentials spanning input chunks
/// from being emitted before enough bytes are available to decide safely.
#[derive(Clone, Debug)]
pub struct StreamingRedactor {
    secrets: SecretSet,
    pending: Vec<u8>,
    /// Tracks retained source bytes already represented by a redaction marker.
    /// This is needed when a later occurrence overlaps an earlier match.
    accounted: Vec<bool>,
}

impl StreamingRedactor {
    pub fn new(secrets: SecretSet) -> Self {
        Self {
            secrets,
            pending: Vec::new(),
            accounted: Vec::new(),
        }
    }

    pub fn from_secret_set(secrets: &SecretSet) -> Self {
        Self::new(secrets.clone())
    }

    /// Adds a chunk and returns all bytes now known to be safe. At most
    /// `max_secret_len - 1` undecided source bytes remain buffered.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.secrets.is_empty() {
            return chunk.to_vec();
        }

        self.pending.extend_from_slice(chunk);
        self.accounted.resize(self.pending.len(), false);
        let retained = self.secrets.max_secret_len().saturating_sub(1);
        let safe_end = self.pending.len().saturating_sub(retained);
        if safe_end == 0 {
            return Vec::new();
        }

        let coverage = self.secrets.coverage(&self.pending);
        let mut output = Vec::new();
        let mut index = 0;

        while index < safe_end {
            if coverage[index] {
                let start = index;
                while index < coverage.len() && coverage[index] {
                    index += 1;
                }
                if !self.accounted[start..index]
                    .iter()
                    .any(|accounted| *accounted)
                {
                    output.extend_from_slice(REDACTED);
                }
                self.accounted[start..index].fill(true);
            } else {
                if !self.accounted[index] {
                    output.push(self.pending[index]);
                    self.accounted[index] = true;
                }
                index += 1;
            }
        }

        // Keep exactly the suffix that could participate in a future match.
        // Accounted flags suppress bytes already represented by a marker if a
        // later occurrence overlaps that marker's source range.
        self.pending.drain(..safe_end);
        self.accounted.drain(..safe_end);
        debug_assert_eq!(self.pending.len(), self.accounted.len());
        debug_assert!(self.pending.len() <= retained);
        output
    }

    /// Alias useful at stream-oriented adapter call sites.
    pub fn redact_chunk(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.push(chunk)
    }

    /// Safely emits all remaining bytes at end-of-stream.
    pub fn flush(&mut self) -> Vec<u8> {
        let coverage = self.secrets.coverage(&self.pending);
        let mut output = Vec::new();
        let mut index = 0;

        while index < self.pending.len() {
            if coverage[index] {
                let start = index;
                while index < coverage.len() && coverage[index] {
                    index += 1;
                }
                if !self.accounted[start..index]
                    .iter()
                    .any(|accounted| *accounted)
                {
                    output.extend_from_slice(REDACTED);
                }
            } else {
                if !self.accounted[index] {
                    output.push(self.pending[index]);
                }
                index += 1;
            }
        }

        self.pending.clear();
        self.accounted.clear();
        output
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// A stderr-oriented diagnostic sink with an injectable writer.
///
/// The writer is the sink's only output capability, which structurally keeps
/// diagnostics away from business stdout. Every event is redacted before it is
/// written. Server stderr uses one `StreamingRedactor` per server.
pub struct WriterDiagnosticSink<W: Write + Send> {
    writer: Mutex<W>,
    debug_enabled: bool,
    style: StylePolicy,
    secrets: Mutex<SecretSet>,
    server_redactors: Mutex<BTreeMap<String, StreamingRedactor>>,
}

impl<W: Write + Send> WriterDiagnosticSink<W> {
    pub fn new(writer: W, debug_enabled: bool, secrets: SecretSet) -> Self {
        Self::new_styled(writer, debug_enabled, secrets, StylePolicy::plain())
    }

    pub fn new_styled(
        writer: W,
        debug_enabled: bool,
        secrets: SecretSet,
        style: StylePolicy,
    ) -> Self {
        Self {
            writer: Mutex::new(writer),
            debug_enabled,
            style,
            secrets: Mutex::new(secrets),
            server_redactors: Mutex::new(BTreeMap::new()),
        }
    }

    /// Adds secrets discovered after configuration parsing but before command
    /// execution. The process dispatcher uses this two-phase setup so
    /// non-strict environment warnings can share the final stderr sink while
    /// stdio/HTTP diagnostics are protected by the fully loaded secret set.
    pub fn register_secrets(&self, secrets: &SecretSet) {
        let mut current = self
            .secrets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for value in &secrets.values {
            current.insert(value);
        }
        drop(current);

        // Registration occurs before transports can emit server stderr. Clear
        // any empty, pre-registration stream state defensively so no redactor
        // can retain an obsolete secret snapshot.
        self.server_redactors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Redacts text with the sink's currently registered secret set. The main
    /// boundary uses this before rendering a typed error exactly once.
    pub fn redact_text(&self, text: &str) -> String {
        self.secrets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .redact(text)
    }

    /// Redacts every user-visible field of a typed error while preserving its
    /// stable kind, exit classification, and retained non-rendered source.
    pub fn redact_error(&self, mut error: crate::error::CliError) -> crate::error::CliError {
        error.message = self.redact_text(&error.message);
        if !error.details_are_redacted() {
            error.details = error
                .details
                .take()
                .map(|details| self.redact_text(&details));
            error.set_details_redacted();
        }
        error.suggestion = error
            .suggestion
            .take()
            .map(|suggestion| self.redact_text(&suggestion));
        error
    }

    pub fn into_inner(self) -> W {
        match self.writer.into_inner() {
            Ok(writer) => writer,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn emit_text(&self, prefix: &str, ansi: &str, text: &str) {
        let redacted = self
            .secrets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .redact(text);
        let prefix = style_fragment(prefix, ansi, self.style);
        self.emit_prefixed(&prefix, &redacted);
    }

    fn emit_server_bytes(&self, server: &str, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let redacted_server = self
            .secrets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .redact(server);
        let safe_server = sanitize_server_name(&redacted_server);
        let marker = style_fragment("[server]", ANSI_MAGENTA, self.style);
        let prefix = format!("{marker} {safe_server}: ");
        let text = String::from_utf8_lossy(bytes);
        self.emit_prefixed(&prefix, &text);
    }

    fn emit_prefixed(&self, prefix: &str, text: &str) {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if text.is_empty() {
            let _ = writer.write_all(prefix.as_bytes());
            let _ = writer.write_all(b"\n");
            return;
        }

        for line in text.split_inclusive('\n') {
            let line = line.strip_suffix('\n').unwrap_or(line);
            let line = line.strip_suffix('\r').unwrap_or(line);
            let _ = writer.write_all(prefix.as_bytes());
            let _ = writer.write_all(line.as_bytes());
            let _ = writer.write_all(b"\n");
        }
    }
}

impl<W: Write + Send> DiagnosticSink for WriterDiagnosticSink<W> {
    fn warning(&self, message: &str) {
        self.emit_text("[mcp-cli] warning:", ANSI_YELLOW, &format!(" {message}"));
    }

    fn debug(&self, message: &str) {
        if self.debug_enabled {
            self.emit_text("[mcp-cli] debug:", ANSI_MAGENTA, &format!(" {message}"));
        }
    }

    fn redact_text(&self, text: &str) -> String {
        WriterDiagnosticSink::redact_text(self, text)
    }

    fn server_stderr(&self, server: &str, bytes: &[u8]) {
        let safe = {
            let mut redactors = self
                .server_redactors
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            redactors
                .entry(server.to_owned())
                .or_insert_with(|| {
                    let secrets = self
                        .secrets
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    StreamingRedactor::from_secret_set(&secrets)
                })
                .push(bytes)
        };
        self.emit_server_bytes(server, &safe);
    }

    fn server_stderr_flush(&self, server: &str) {
        let safe = {
            let mut redactors = self
                .server_redactors
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            redactors
                .remove(server)
                .map(|mut redactor| redactor.flush())
                .unwrap_or_default()
        };
        self.emit_server_bytes(server, &safe);
    }
}

fn redact_covered(bytes: &[u8], coverage: &[bool]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if coverage[index] {
            output.extend_from_slice(REDACTED);
            while index < coverage.len() && coverage[index] {
                index += 1;
            }
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    output
}

fn sanitize_server_name(server: &str) -> String {
    use std::fmt::Write as _;

    let mut safe = String::with_capacity(server.len());
    for character in server.chars() {
        if character.is_control() || matches!(character, '[' | ']' | '\\') {
            let _ = write!(safe, "\\u{{{:x}}}", u32::from(character));
        } else {
            safe.push(character);
        }
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collected_stream(redactor: &mut StreamingRedactor, chunks: &[&[u8]]) -> Vec<u8> {
        let mut output = Vec::new();
        for chunk in chunks {
            output.extend(redactor.push(chunk));
        }
        output.extend(redactor.flush());
        output
    }

    #[test]
    fn registers_non_empty_env_and_header_secrets_once() {
        let mut secrets = SecretSet::new();

        assert!(!secrets.register_env("EMPTY", ""));
        assert!(secrets.register_env("TOKEN", "same-secret"));
        assert!(!secrets.register_header("X-Token", "same-secret"));
        assert!(secrets.register_authorization("Bearer abc"));
        assert!(secrets.register_cookie("session=xyz"));
        assert_eq!(secrets.len(), 3);
        assert_eq!(secrets.max_secret_len(), "same-secret".len());
        assert_eq!(
            format!("{secrets:?}"),
            "SecretSet { len: 3, max_secret_len: 11 }"
        );
    }

    #[test]
    fn redacts_text_bytes_and_overlapping_secrets() {
        let mut secrets = SecretSet::new();
        secrets.insert("abc");
        secrets.insert("bcd");

        assert_eq!(secrets.redact("xabcdy"), "x[REDACTED]y");
        assert_eq!(
            secrets.redact_bytes(b"abc and bcd"),
            b"[REDACTED] and [REDACTED]"
        );
    }

    #[test]
    fn empty_secrets_do_not_redact_or_force_buffering() {
        let mut secrets = SecretSet::new();
        secrets.insert("");
        let mut redactor = StreamingRedactor::new(secrets);

        assert_eq!(redactor.push(b"unchanged"), b"unchanged");
        assert_eq!(redactor.pending_len(), 0);
        assert!(redactor.flush().is_empty());
    }

    #[test]
    fn redacts_a_secret_across_every_possible_chunk_boundary() {
        let input = b"before credential after";
        let mut secrets = SecretSet::new();
        secrets.insert("credential");
        let expected = secrets.redact_bytes(input);

        for split in 0..=input.len() {
            let mut redactor = StreamingRedactor::from_secret_set(&secrets);
            let pieces = vec![
                redactor.push(&input[..split]),
                redactor.push(&input[split..]),
                redactor.flush(),
            ];

            for piece in &pieces {
                assert!(
                    !piece
                        .windows(b"credential".len())
                        .any(|window| window == b"credential"),
                    "secret leaked at split {split}"
                );
            }
            assert_eq!(pieces.concat(), expected, "split {split}");
        }
    }

    #[test]
    fn byte_at_a_time_streaming_handles_overlap_and_bounds_pending_tail() {
        let mut secrets = SecretSet::new();
        secrets.insert("aba");
        let mut redactor = StreamingRedactor::from_secret_set(&secrets);
        let mut output = Vec::new();

        for byte in b"zababaq" {
            output.extend(redactor.push(std::slice::from_ref(byte)));
            assert!(redactor.pending_len() < secrets.max_secret_len());
        }
        output.extend(redactor.flush());

        assert_eq!(output, b"z[REDACTED]q");
    }

    #[test]
    fn flush_safely_emits_an_incomplete_secret_prefix() {
        let mut secrets = SecretSet::new();
        secrets.insert("credential");
        let mut redactor = StreamingRedactor::from_secret_set(&secrets);

        assert!(redactor.push(b"cred").is_empty());
        assert_eq!(redactor.flush(), b"cred");
        assert_eq!(redactor.pending_len(), 0);
    }

    #[test]
    fn arbitrary_non_utf8_bytes_are_redacted_before_lossy_conversion() {
        let mut secrets = SecretSet::new();
        secrets.insert([0xff, 0xfe]);

        let redacted = secrets.redact_bytes(&[b'a', 0xff, 0xfe, 0x80]);
        let visible = String::from_utf8_lossy(&redacted);
        assert!(visible.contains("[REDACTED]"));
        assert!(!redacted.windows(2).any(|window| window == [0xff, 0xfe]));
    }

    #[test]
    fn diagnostics_redact_and_debug_flag_only_suppresses_debug() {
        let mut secrets = SecretSet::new();
        secrets.insert("token-value");
        let sink = WriterDiagnosticSink::new(Vec::new(), false, secrets.clone());

        sink.warning("warning token-value");
        sink.debug("debug token-value");
        sink.server_stderr("alpha", b"server token-value");
        sink.server_stderr_flush("alpha");
        let disabled = String::from_utf8(sink.into_inner()).expect("diagnostics are UTF-8");

        assert!(disabled.contains("[mcp-cli] warning: warning [REDACTED]"));
        assert!(disabled.contains("[server] alpha: server [REDACTED]"));
        assert!(!disabled.contains("debug"));
        assert!(!disabled.contains("token-value"));

        let sink = WriterDiagnosticSink::new(Vec::new(), true, secrets);
        sink.debug("debug token-value");
        let enabled = String::from_utf8(sink.into_inner()).expect("diagnostics are UTF-8");
        assert_eq!(enabled, "[mcp-cli] debug: debug [REDACTED]\n");

        let mut marker_secret = SecretSet::new();
        marker_secret.insert("R");
        let sink = WriterDiagnosticSink::new(Vec::<u8>::new(), false, marker_secret);
        let error = crate::error::CliError::network_error("remote", "unused")
            .with_details("[REDACTED]")
            .mark_details_redacted();
        let error = sink.redact_error(error);
        assert_eq!(error.details.as_deref(), Some("[REDACTED]"));
    }

    #[test]
    fn server_stderr_has_safe_prefix_and_redacts_across_calls() {
        let mut secrets = SecretSet::new();
        secrets.insert("hunter2");
        let sink = WriterDiagnosticSink::new(Vec::new(), false, secrets);

        sink.server_stderr("bad]\nname", b"prefix hun");
        sink.server_stderr("bad]\nname", &[b't', b'e', b'r', b'2', 0xff]);
        sink.server_stderr_flush("bad]\nname");
        let output = String::from_utf8(sink.into_inner()).expect("lossy diagnostics are UTF-8");

        assert!(output.contains("[server] bad\\u{5d}\\u{a}name: "));
        assert!(output.contains("[REDACTED]"));
        assert!(output.contains('\u{fffd}'));
        assert!(!output.contains("hunter2"));
        assert!(!output.contains("[server] bad]\nname:"));
    }

    #[test]
    fn helper_collects_chunks_without_changing_stateless_result() {
        let mut secrets = SecretSet::new();
        secrets.insert("secret");
        let mut redactor = StreamingRedactor::from_secret_set(&secrets);

        assert_eq!(
            collected_stream(&mut redactor, &[b"one se", b"cret two"]),
            secrets.redact_bytes(b"one secret two")
        );
    }
}
