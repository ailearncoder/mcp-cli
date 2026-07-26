#![deny(unsafe_code)]

pub mod cli;
pub mod commands;
pub mod config;
pub mod connection;
pub mod daemon;
pub mod domain;
pub mod error;
pub mod output;
pub mod policy;
pub mod runtime;

pub use cli::{CliInvocation, CommandSpec, cli_command, parse_cli};
pub use commands::{
    CommandDispatcher,
    call::{CALL_INPUT_MAX_SIZE, CallHandler, CallInput, read_call_input},
    dispatch_call_command, dispatch_grep_command, dispatch_info_command, dispatch_list_command,
    grep::GrepHandler,
    info::InfoHandler,
    list::ListHandler,
};
pub use config::{
    ConfigHash, ConfigurationLoader, EnvSource, FileConfigurationLoader, LoadRequest, LoadedConfig,
    ProcessEnv, ServerDefinition, ServerId, ToolFilterConfig, TransportConfig,
};
pub use connection::{
    ConnectionError, ConnectionManager, ConnectionResourceRegistry, DirectConnectionManager,
    DirectConnector, ManagedConnectionManager, McpConnection,
};
pub use domain::{
    CommandOutcome, ConnectionMode, JsonObject, PerServer, SearchHit, ServerSnapshot, ToolInfo,
    ToolResult, TransportSummary,
};
pub use error::{CliError, ErrorKind, ExitCode};
pub use output::{
    DiagnosticSink, DualStreamWriter, JsonPresenter, PlainTextPresenter, Presenter,
    StreamStylePolicies, StylePolicy, format_grep_hits, format_json_schema, format_json_value,
    format_server_info, format_server_list, format_server_snapshot, format_tool_result,
    render_structured_error, render_structured_error_with_style,
};
pub use policy::redact::{SecretSet, StreamingRedactor, WriterDiagnosticSink};
pub use policy::retry::{
    Attempt, ClassifyError, ErrorClass, RetryError, RetryPolicy, classify_errno,
    classify_http_status, classify_io_error, retry,
};
pub use runtime::{
    BoxFuture, CancellationFlag, CancellationToken, CleanupTimeout, Clock, CommandContext,
    Deadline, JitterSource, RuntimeConfig, SystemClock,
};
