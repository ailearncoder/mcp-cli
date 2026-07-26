//! Daemon IPC and worker lifecycle.

#[cfg(unix)]
pub mod client;
#[cfg(unix)]
pub mod codec;
#[cfg(unix)]
pub mod metadata;
#[cfg(unix)]
pub mod paths;
#[cfg(unix)]
pub mod worker;

#[cfg(unix)]
use std::fmt;

#[cfg(unix)]
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _, ser::SerializeMap};
#[cfg(unix)]
use serde_json::{Map, Value};

#[cfg(unix)]
pub use codec::{FrameError, IPC_MAX_FRAME_SIZE, NdjsonCodec, decode_message, encode_message};
#[cfg(unix)]
pub use metadata::{MetadataError, MetadataStore, PID_METADATA_MAX_BYTES, PidMetadata};
#[cfg(unix)]
pub use paths::{DaemonPathError, DaemonPaths};

/// Maximum UTF-8 byte length of an IPC request identifier.
#[cfg(unix)]
pub const IPC_REQUEST_ID_MAX_BYTES: usize = 128;
/// Maximum UTF-8 byte length of a tool name carried by `callTool`.
#[cfg(unix)]
pub const IPC_TOOL_NAME_MAX_BYTES: usize = 1024;

/// Request operations exposed by the daemon wire protocol.
///
/// These are transport-independent domain values and intentionally contain no
/// rmcp SDK types.
#[cfg(unix)]
#[derive(Clone, Debug, PartialEq)]
pub enum IpcOperation {
    Ping,
    ListTools,
    CallTool {
        tool_name: String,
        args: crate::domain::JsonObject,
    },
    GetInstructions,
    Close,
}

#[cfg(unix)]
impl IpcOperation {
    fn validate(&self) -> Result<(), IpcModelError> {
        if let Self::CallTool { tool_name, .. } = self {
            validate_tool_name(tool_name)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Serialize for IpcOperation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        let field_count = if matches!(self, Self::CallTool { .. }) {
            3
        } else {
            1
        };
        let mut map = serializer.serialize_map(Some(field_count))?;
        match self {
            Self::Ping => map.serialize_entry("type", "ping")?,
            Self::ListTools => map.serialize_entry("type", "listTools")?,
            Self::CallTool { tool_name, args } => {
                map.serialize_entry("type", "callTool")?;
                map.serialize_entry("toolName", tool_name)?;
                map.serialize_entry("args", args)?;
            }
            Self::GetInstructions => map.serialize_entry("type", "getInstructions")?,
            Self::Close => map.serialize_entry("type", "close")?,
        }
        map.end()
    }
}

#[cfg(unix)]
impl<'de> Deserialize<'de> for IpcOperation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        parse_operation(value).map_err(D::Error::custom)
    }
}

/// Validated daemon request. Deserialization rejects unknown fields, invalid
/// IDs, unknown operation names, and malformed operation parameters.
#[cfg(unix)]
#[derive(Clone, Debug, PartialEq)]
pub struct IpcRequest {
    id: String,
    operation: IpcOperation,
}

#[cfg(unix)]
impl IpcRequest {
    pub fn new(id: impl Into<String>, operation: IpcOperation) -> Result<Self, IpcModelError> {
        let request = Self {
            id: id.into(),
            operation,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn operation(&self) -> &IpcOperation {
        &self.operation
    }

    pub fn into_parts(self) -> (String, IpcOperation) {
        (self.id, self.operation)
    }

    fn validate(&self) -> Result<(), IpcModelError> {
        validate_request_id(&self.id)?;
        self.operation.validate()
    }
}

#[cfg(unix)]
impl Serialize for IpcRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        let field_count = if matches!(&self.operation, IpcOperation::CallTool { .. }) {
            4
        } else {
            2
        };
        let mut map = serializer.serialize_map(Some(field_count))?;
        map.serialize_entry("id", &self.id)?;
        match &self.operation {
            IpcOperation::Ping => map.serialize_entry("type", "ping")?,
            IpcOperation::ListTools => map.serialize_entry("type", "listTools")?,
            IpcOperation::CallTool { tool_name, args } => {
                map.serialize_entry("type", "callTool")?;
                map.serialize_entry("toolName", tool_name)?;
                map.serialize_entry("args", args)?;
            }
            IpcOperation::GetInstructions => map.serialize_entry("type", "getInstructions")?,
            IpcOperation::Close => map.serialize_entry("type", "close")?,
        }
        map.end()
    }
}

#[cfg(unix)]
impl<'de> Deserialize<'de> for IpcRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        parse_request(value).map_err(D::Error::custom)
    }
}

/// Stable daemon error codes. Their serialized spellings are API and must not
/// be derived from implementation error text.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IpcErrorCode {
    InvalidJson,
    MissingId,
    UnknownType,
    InvalidArguments,
    NotConnected,
    ExecutionError,
    FrameTooLarge,
    InvalidUtf8,
    TruncatedFrame,
    Internal,
}

#[cfg(unix)]
impl IpcErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "INVALID_JSON",
            Self::MissingId => "MISSING_ID",
            Self::UnknownType => "UNKNOWN_TYPE",
            Self::InvalidArguments => "INVALID_ARGUMENTS",
            Self::NotConnected => "NOT_CONNECTED",
            Self::ExecutionError => "EXECUTION_ERROR",
            Self::FrameTooLarge => "FRAME_TOO_LARGE",
            Self::InvalidUtf8 => "INVALID_UTF8",
            Self::TruncatedFrame => "TRUNCATED_FRAME",
            Self::Internal => "INTERNAL",
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::InvalidJson => "Invalid JSON request",
            Self::MissingId => "Request ID is required",
            Self::UnknownType => "Unknown request type",
            Self::InvalidArguments => "Invalid request arguments",
            Self::NotConnected => "Daemon backend is not connected",
            Self::ExecutionError => "Daemon operation failed",
            Self::FrameTooLarge => "IPC frame exceeds the maximum size",
            Self::InvalidUtf8 => "IPC frame is not valid UTF-8",
            Self::TruncatedFrame => "IPC frame was truncated",
            Self::Internal => "Internal daemon error",
        }
    }
}

#[cfg(unix)]
impl fmt::Display for IpcErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable error body. Only the canonical message for a code is accepted on
/// the wire, preventing arbitrary payload or secret text from being reflected.
#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpcErrorBody {
    code: IpcErrorCode,
}

#[cfg(unix)]
impl IpcErrorBody {
    pub const fn new(code: IpcErrorCode) -> Self {
        Self { code }
    }

    pub const fn code(&self) -> IpcErrorCode {
        self.code
    }

    pub const fn message(&self) -> &'static str {
        self.code.message()
    }
}

#[cfg(unix)]
impl Serialize for IpcErrorBody {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("code", &self.code)?;
        map.serialize_entry("message", self.code.message())?;
        map.end()
    }
}

#[cfg(unix)]
impl<'de> Deserialize<'de> for IpcErrorBody {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        parse_error_body(value).map_err(D::Error::custom)
    }
}

/// Exactly one response outcome: either `success + data` or `failure + error`.
#[cfg(unix)]
#[derive(Clone, Debug, PartialEq)]
pub enum IpcOutcome {
    Success(Value),
    Failure(IpcErrorBody),
}

#[cfg(unix)]
impl Serialize for IpcOutcome {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2))?;
        match self {
            Self::Success(data) => {
                map.serialize_entry("success", &true)?;
                map.serialize_entry("data", data)?;
            }
            Self::Failure(error) => {
                map.serialize_entry("success", &false)?;
                map.serialize_entry("error", error)?;
            }
        }
        map.end()
    }
}

#[cfg(unix)]
impl<'de> Deserialize<'de> for IpcOutcome {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        parse_outcome(value).map_err(D::Error::custom)
    }
}

/// Correlated daemon response with a validated request ID and strict outcome.
#[cfg(unix)]
#[derive(Clone, Debug, PartialEq)]
pub struct IpcResponse {
    id: String,
    outcome: IpcOutcome,
}

#[cfg(unix)]
impl IpcResponse {
    pub fn new(id: impl Into<String>, outcome: IpcOutcome) -> Result<Self, IpcModelError> {
        let response = Self {
            id: id.into(),
            outcome,
        };
        validate_response(&response.id, &response.outcome)?;
        Ok(response)
    }

    pub fn success(id: impl Into<String>, data: Value) -> Result<Self, IpcModelError> {
        Self::new(id, IpcOutcome::Success(data))
    }

    pub fn failure(id: impl Into<String>, code: IpcErrorCode) -> Result<Self, IpcModelError> {
        Self::new(id, IpcOutcome::Failure(IpcErrorBody::new(code)))
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn outcome(&self) -> &IpcOutcome {
        &self.outcome
    }

    pub fn into_parts(self) -> (String, IpcOutcome) {
        (self.id, self.outcome)
    }
}

#[cfg(unix)]
impl Serialize for IpcResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_response(&self.id, &self.outcome).map_err(serde::ser::Error::custom)?;
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("id", &self.id)?;
        match &self.outcome {
            IpcOutcome::Success(data) => {
                map.serialize_entry("success", &true)?;
                map.serialize_entry("data", data)?;
            }
            IpcOutcome::Failure(error) => {
                map.serialize_entry("success", &false)?;
                map.serialize_entry("error", error)?;
            }
        }
        map.end()
    }
}

#[cfg(unix)]
impl<'de> Deserialize<'de> for IpcResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        parse_response(value).map_err(D::Error::custom)
    }
}

/// Payload-free IPC model validation error suitable for safe diagnostics.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcModelError {
    ExpectedObject,
    UnknownField,
    MissingId,
    InvalidId,
    MissingType,
    UnknownType,
    InvalidArguments,
    InvalidOutcome,
    InvalidError,
}

#[cfg(unix)]
impl fmt::Display for IpcModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ExpectedObject => "IPC message must be an object",
            Self::UnknownField => "IPC message contains an unknown field",
            Self::MissingId => "IPC request ID is required",
            Self::InvalidId => "IPC request ID is invalid",
            Self::MissingType => "IPC request type is required",
            Self::UnknownType => "IPC request type is unknown",
            Self::InvalidArguments => "IPC operation arguments are invalid",
            Self::InvalidOutcome => "IPC response outcome is invalid",
            Self::InvalidError => "IPC error body is invalid",
        })
    }
}

#[cfg(unix)]
impl std::error::Error for IpcModelError {}

#[cfg(unix)]
pub fn validate_request_id(id: &str) -> Result<(), IpcModelError> {
    if id.is_empty() || id.len() > IPC_REQUEST_ID_MAX_BYTES || id.chars().any(char::is_control) {
        Err(IpcModelError::InvalidId)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn validate_response(id: &str, outcome: &IpcOutcome) -> Result<(), IpcModelError> {
    if id.is_empty() {
        return match outcome {
            IpcOutcome::Failure(_) => Ok(()),
            IpcOutcome::Success(_) => Err(IpcModelError::InvalidId),
        };
    }
    validate_request_id(id)
}

#[cfg(unix)]
fn validate_tool_name(tool_name: &str) -> Result<(), IpcModelError> {
    if tool_name.is_empty()
        || tool_name.len() > IPC_TOOL_NAME_MAX_BYTES
        || tool_name.chars().any(char::is_control)
    {
        Err(IpcModelError::InvalidArguments)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn into_object(value: Value) -> Result<Map<String, Value>, IpcModelError> {
    value
        .as_object()
        .cloned()
        .ok_or(IpcModelError::ExpectedObject)
}

#[cfg(unix)]
fn reject_unknown(map: &Map<String, Value>, allowed: &[&str]) -> Result<(), IpcModelError> {
    if map.keys().any(|key| !allowed.contains(&key.as_str())) {
        Err(IpcModelError::UnknownField)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn take_required_string(
    map: &mut Map<String, Value>,
    field: &str,
    missing: IpcModelError,
    invalid: IpcModelError,
) -> Result<String, IpcModelError> {
    match map.remove(field) {
        None => Err(missing),
        Some(Value::String(value)) => Ok(value),
        Some(_) => Err(invalid),
    }
}

#[cfg(unix)]
fn parse_operation(value: Value) -> Result<IpcOperation, IpcModelError> {
    let mut map = into_object(value)?;
    let operation_type = take_required_string(
        &mut map,
        "type",
        IpcModelError::MissingType,
        IpcModelError::UnknownType,
    )?;
    match operation_type.as_str() {
        "ping" => {
            reject_unknown(&map, &[])?;
            Ok(IpcOperation::Ping)
        }
        "listTools" => {
            reject_unknown(&map, &[])?;
            Ok(IpcOperation::ListTools)
        }
        "callTool" => {
            reject_unknown(&map, &["toolName", "args"])?;
            let tool_name = take_required_string(
                &mut map,
                "toolName",
                IpcModelError::InvalidArguments,
                IpcModelError::InvalidArguments,
            )?;
            validate_tool_name(&tool_name)?;
            let args = match map.remove("args") {
                Some(Value::Object(args)) => args,
                _ => return Err(IpcModelError::InvalidArguments),
            };
            Ok(IpcOperation::CallTool { tool_name, args })
        }
        "getInstructions" => {
            reject_unknown(&map, &[])?;
            Ok(IpcOperation::GetInstructions)
        }
        "close" => {
            reject_unknown(&map, &[])?;
            Ok(IpcOperation::Close)
        }
        _ => Err(IpcModelError::UnknownType),
    }
}

#[cfg(unix)]
fn parse_request(value: Value) -> Result<IpcRequest, IpcModelError> {
    let mut map = into_object(value)?;
    let id = take_required_string(
        &mut map,
        "id",
        IpcModelError::MissingId,
        IpcModelError::InvalidId,
    )?;
    validate_request_id(&id)?;
    let operation = parse_operation(Value::Object(map))?;
    IpcRequest::new(id, operation)
}

#[cfg(unix)]
fn parse_error_body(value: Value) -> Result<IpcErrorBody, IpcModelError> {
    let mut map = into_object(value).map_err(|_| IpcModelError::InvalidError)?;
    reject_unknown(&map, &["code", "message"]).map_err(|_| IpcModelError::InvalidError)?;
    let code_value = map.remove("code").ok_or(IpcModelError::InvalidError)?;
    let code = serde_json::from_value::<IpcErrorCode>(code_value)
        .map_err(|_| IpcModelError::InvalidError)?;
    let message = match map.remove("message") {
        Some(Value::String(message)) => message,
        _ => return Err(IpcModelError::InvalidError),
    };
    if message != code.message() {
        return Err(IpcModelError::InvalidError);
    }
    Ok(IpcErrorBody::new(code))
}

#[cfg(unix)]
fn parse_outcome(value: Value) -> Result<IpcOutcome, IpcModelError> {
    let mut map = into_object(value).map_err(|_| IpcModelError::InvalidOutcome)?;
    let success = match map.remove("success") {
        Some(Value::Bool(success)) => success,
        _ => return Err(IpcModelError::InvalidOutcome),
    };
    if success {
        reject_unknown(&map, &["data"]).map_err(|_| IpcModelError::InvalidOutcome)?;
        let data = map.remove("data").ok_or(IpcModelError::InvalidOutcome)?;
        Ok(IpcOutcome::Success(data))
    } else {
        reject_unknown(&map, &["error"]).map_err(|_| IpcModelError::InvalidOutcome)?;
        let error = map.remove("error").ok_or(IpcModelError::InvalidOutcome)?;
        Ok(IpcOutcome::Failure(parse_error_body(error)?))
    }
}

#[cfg(unix)]
fn parse_response(value: Value) -> Result<IpcResponse, IpcModelError> {
    let mut map = into_object(value)?;
    reject_unknown(&map, &["id", "success", "data", "error"])?;
    let id = take_required_string(
        &mut map,
        "id",
        IpcModelError::MissingId,
        IpcModelError::InvalidId,
    )?;
    let outcome = parse_outcome(Value::Object(map))?;
    IpcResponse::new(id, outcome)
}

/// Returns the explicit platform boundary used by direct-only builds.
#[cfg(not(unix))]
pub fn unsupported_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Unix daemon paths and PID metadata are unsupported on this platform",
    )
}

#[cfg(all(test, unix))]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn request_json(operation: IpcOperation) -> Value {
        serde_json::to_value(IpcRequest::new("request-1", operation).expect("valid request"))
            .expect("serialize request")
    }

    #[test]
    fn every_operation_uses_the_stable_camel_case_wire_shape_and_round_trips() {
        let operations = [
            (IpcOperation::Ping, json!({"id":"request-1","type":"ping"})),
            (
                IpcOperation::ListTools,
                json!({"id":"request-1","type":"listTools"}),
            ),
            (
                IpcOperation::CallTool {
                    tool_name: "search".into(),
                    args: serde_json::from_value(json!({"query":"rust"})).expect("object"),
                },
                json!({
                    "id":"request-1",
                    "type":"callTool",
                    "toolName":"search",
                    "args":{"query":"rust"}
                }),
            ),
            (
                IpcOperation::GetInstructions,
                json!({"id":"request-1","type":"getInstructions"}),
            ),
            (
                IpcOperation::Close,
                json!({"id":"request-1","type":"close"}),
            ),
        ];

        for (operation, expected) in operations {
            let request = IpcRequest::new("request-1", operation).expect("valid request");
            let encoded = serde_json::to_value(&request).expect("serialize");
            assert_eq!(encoded, expected);
            assert_eq!(
                serde_json::from_value::<IpcRequest>(encoded).expect("deserialize"),
                request
            );
        }
    }

    #[test]
    fn request_ids_enforce_utf8_byte_length_and_control_character_grammar() {
        let ascii_max = "a".repeat(IPC_REQUEST_ID_MAX_BYTES);
        assert!(IpcRequest::new(ascii_max, IpcOperation::Ping).is_ok());
        assert_eq!(
            IpcRequest::new("a".repeat(IPC_REQUEST_ID_MAX_BYTES + 1), IpcOperation::Ping),
            Err(IpcModelError::InvalidId)
        );
        let unicode_max = "界".repeat(42); // 126 UTF-8 bytes
        assert!(IpcRequest::new(unicode_max, IpcOperation::Ping).is_ok());
        for invalid in ["", "line\nbreak", "nul\0byte"] {
            assert_eq!(
                IpcRequest::new(invalid, IpcOperation::Ping),
                Err(IpcModelError::InvalidId)
            );
        }
    }

    #[test]
    fn requests_deny_unknown_fields_unknown_types_and_invalid_parameters() {
        let invalid = [
            json!({"id":"x","type":"ping","extra":true}),
            json!({"id":"x","type":"unknown"}),
            json!({"id":"x","type":"callTool","toolName":"tool"}),
            json!({"id":"x","type":"callTool","toolName":"","args":{}}),
            json!({"id":"x","type":"callTool","toolName":"tool","args":[]}),
            json!({"id":"x","type":"callTool","tool_name":"tool","args":{}}),
            json!({"type":"ping"}),
        ];

        for value in invalid {
            assert!(serde_json::from_value::<IpcRequest>(value).is_err());
        }
    }

    #[test]
    fn response_success_and_failure_round_trip_with_same_id() {
        let success = IpcResponse::success("same-id", json!({"tools":[]})).expect("success");
        let failure = IpcResponse::failure("same-id", IpcErrorCode::NotConnected).expect("failure");

        assert_eq!(success.id(), "same-id");
        assert_eq!(failure.id(), "same-id");
        assert_eq!(
            serde_json::to_value(&success).expect("serialize"),
            json!({"id":"same-id","success":true,"data":{"tools":[]}})
        );
        assert_eq!(
            serde_json::to_value(&failure).expect("serialize"),
            json!({
                "id":"same-id",
                "success":false,
                "error":{"code":"NOT_CONNECTED","message":"Daemon backend is not connected"}
            })
        );
        for response in [success, failure] {
            let bytes = serde_json::to_vec(&response).expect("serialize");
            assert_eq!(
                serde_json::from_slice::<IpcResponse>(&bytes).expect("deserialize"),
                response
            );
        }
    }

    #[test]
    fn response_outcomes_are_strictly_mutually_exclusive_and_deny_unknown_fields() {
        let invalid = [
            json!({"id":"x","success":true}),
            json!({"id":"x","success":true,"data":null,"error":{"code":"INTERNAL","message":"Internal daemon error"}}),
            json!({"id":"x","success":false}),
            json!({"id":"x","success":false,"data":null,"error":{"code":"INTERNAL","message":"Internal daemon error"}}),
            json!({"id":"x","success":false,"error":{"code":"INTERNAL","message":"arbitrary secret"}}),
            json!({"id":"x","success":true,"data":null,"extra":1}),
            json!({"id":"x","success":"true","data":null}),
        ];

        for value in invalid {
            assert!(serde_json::from_value::<IpcResponse>(value).is_err());
        }
    }

    #[test]
    fn error_codes_have_stable_spellings_and_canonical_safe_messages() {
        let cases = [
            (IpcErrorCode::InvalidJson, "INVALID_JSON"),
            (IpcErrorCode::MissingId, "MISSING_ID"),
            (IpcErrorCode::UnknownType, "UNKNOWN_TYPE"),
            (IpcErrorCode::InvalidArguments, "INVALID_ARGUMENTS"),
            (IpcErrorCode::NotConnected, "NOT_CONNECTED"),
            (IpcErrorCode::ExecutionError, "EXECUTION_ERROR"),
            (IpcErrorCode::FrameTooLarge, "FRAME_TOO_LARGE"),
            (IpcErrorCode::InvalidUtf8, "INVALID_UTF8"),
            (IpcErrorCode::TruncatedFrame, "TRUNCATED_FRAME"),
            (IpcErrorCode::Internal, "INTERNAL"),
        ];

        for (code, spelling) in cases {
            assert_eq!(code.as_str(), spelling);
            assert_eq!(
                serde_json::to_string(&code).expect("serialize"),
                format!("\"{spelling}\"")
            );
            assert!(!code.message().is_empty());
        }
    }

    fn padded_request(target: usize) -> IpcRequest {
        let base = IpcRequest::new(
            "boundary",
            IpcOperation::CallTool {
                tool_name: "tool".into(),
                args: serde_json::from_value(json!({"padding":""})).expect("object"),
            },
        )
        .expect("base request");
        let base_len = serde_json::to_vec(&base).expect("serialize base").len();
        let padding = "a".repeat(target - base_len);
        IpcRequest::new(
            "boundary",
            IpcOperation::CallTool {
                tool_name: "tool".into(),
                args: serde_json::from_value(json!({"padding":padding})).expect("object"),
            },
        )
        .expect("padded request")
    }

    fn padded_response(target: usize) -> IpcResponse {
        let base = IpcResponse::success("boundary", json!({"padding":""})).expect("base");
        let base_len = serde_json::to_vec(&base).expect("serialize base").len();
        IpcResponse::success("boundary", json!({"padding":"a".repeat(target - base_len)}))
            .expect("padded response")
    }

    #[test]
    fn request_encoding_and_decoding_enforce_exact_one_mib_boundary() {
        let exact = padded_request(IPC_MAX_FRAME_SIZE);
        let encoded = encode_message(&exact).expect("exact request");
        assert_eq!(encoded.len(), IPC_MAX_FRAME_SIZE + 1);
        assert_eq!(
            decode_message::<IpcRequest>(&encoded[..IPC_MAX_FRAME_SIZE]).expect("decode exact"),
            exact
        );

        let oversized = padded_request(IPC_MAX_FRAME_SIZE + 1);
        assert_eq!(encode_message(&oversized), Err(FrameError::FrameTooLarge));
        let bytes = serde_json::to_vec(&oversized).expect("serialize oversized");
        assert_eq!(
            decode_message::<IpcRequest>(&bytes),
            Err(FrameError::FrameTooLarge)
        );
    }

    #[test]
    fn response_encoding_and_decoding_enforce_exact_one_mib_boundary() {
        let exact = padded_response(IPC_MAX_FRAME_SIZE);
        let encoded = encode_message(&exact).expect("exact response");
        assert_eq!(encoded.len(), IPC_MAX_FRAME_SIZE + 1);
        assert_eq!(
            decode_message::<IpcResponse>(&encoded[..IPC_MAX_FRAME_SIZE]).expect("decode exact"),
            exact
        );

        let oversized = padded_response(IPC_MAX_FRAME_SIZE + 1);
        assert_eq!(encode_message(&oversized), Err(FrameError::FrameTooLarge));
        let bytes = serde_json::to_vec(&oversized).expect("serialize oversized");
        assert_eq!(
            decode_message::<IpcResponse>(&bytes),
            Err(FrameError::FrameTooLarge)
        );
    }

    #[test]
    fn standalone_operation_and_outcome_serde_remain_strict() {
        let operation = IpcOperation::CallTool {
            tool_name: "tool".into(),
            args: Map::new(),
        };
        let value = serde_json::to_value(&operation).expect("serialize operation");
        assert_eq!(
            serde_json::from_value::<IpcOperation>(value).expect("deserialize operation"),
            operation
        );
        assert!(serde_json::from_value::<IpcOperation>(json!({"type":"ping","x":1})).is_err());

        let outcome = IpcOutcome::Success(json!(null));
        let value = serde_json::to_value(&outcome).expect("serialize outcome");
        assert_eq!(
            serde_json::from_value::<IpcOutcome>(value).expect("deserialize outcome"),
            outcome
        );
    }

    #[test]
    fn helper_keeps_request_serialization_covered() {
        assert_eq!(
            request_json(IpcOperation::Ping),
            json!({"id":"request-1","type":"ping"})
        );
    }
}
