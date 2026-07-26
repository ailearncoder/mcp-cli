//! Transport-independent MCP connection boundary.

use std::{error::Error, fmt};

use crate::{
    config::ServerDefinition,
    domain::{ConnectionMode, JsonObject, ToolInfo, ToolResult},
    error::CliError,
    policy::retry::{ClassifyError, ErrorClass, classify_http_status, classify_io_error},
    runtime::{BoxFuture, CommandContext},
};

pub mod direct;
pub mod manager;
pub mod rmcp_adapter;

pub use direct::{ConnectionResourceRegistry, DirectConnectionManager};
pub use manager::ManagedConnectionManager;

/// Adapter-level failure that can retain an external source without exposing
/// an rmcp type in public command or domain APIs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ConnectionFailureKind {
    #[default]
    Operation,
    Timeout,
    Cancelled,
}

pub struct ConnectionError {
    message: String,
    http_status: Option<u16>,
    class: ErrorClass,
    failure_kind: ConnectionFailureKind,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ConnectionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            http_status: None,
            class: ErrorClass::NonTransient,
            failure_kind: ConnectionFailureKind::Operation,
            source: None,
        }
    }

    pub fn with_source(
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        let class = classify_error_source(&source);
        Self {
            message: message.into(),
            http_status: None,
            class,
            failure_kind: ConnectionFailureKind::Operation,
            source: Some(Box::new(source)),
        }
    }

    pub fn with_http_status(mut self, status: u16) -> Self {
        self.http_status = Some(status);
        self.class = classify_http_status(status);
        self
    }

    pub fn with_class(mut self, class: ErrorClass) -> Self {
        self.class = class;
        self
    }

    pub fn timed_out(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            http_status: None,
            class: ErrorClass::NonTransient,
            failure_kind: ConnectionFailureKind::Timeout,
            source: None,
        }
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            http_status: None,
            class: ErrorClass::Cancelled,
            failure_kind: ConnectionFailureKind::Cancelled,
            source: None,
        }
    }

    pub const fn http_status(&self) -> Option<u16> {
        self.http_status
    }

    pub const fn is_timeout(&self) -> bool {
        matches!(self.failure_kind, ConnectionFailureKind::Timeout)
    }

    pub const fn is_cancelled(&self) -> bool {
        matches!(self.failure_kind, ConnectionFailureKind::Cancelled)
    }

    pub const fn error_class(&self) -> ErrorClass {
        self.class
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl Default for ConnectionError {
    fn default() -> Self {
        Self::new("")
    }
}

impl fmt::Debug for ConnectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectionError")
            .field("message", &self.message)
            .field("http_status", &self.http_status)
            .field("class", &self.class)
            .field("failure_kind", &self.failure_kind)
            .field("has_source", &self.source.is_some())
            .finish()
    }
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ConnectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

impl ClassifyError for ConnectionError {
    fn class(&self) -> ErrorClass {
        self.class
    }
}

fn classify_error_source(source: &(dyn Error + 'static)) -> ErrorClass {
    let mut current = Some(source);
    while let Some(error) = current {
        if let Some(error) = error.downcast_ref::<std::io::Error>() {
            let class = classify_io_error(error);
            if class.is_retryable() {
                return class;
            }
        }
        if let Some(error) = error.downcast_ref::<reqwest::Error>() {
            if let Some(status) = error.status() {
                return classify_http_status(status.as_u16());
            }
            if error.is_timeout() {
                return ErrorClass::Transient;
            }
        }
        current = error.source();
    }
    ErrorClass::NonTransient
}

/// Uniform MCP operations available to command handlers.
///
/// Boxed standard-library futures keep this trait object-safe without leaking
/// rmcp service, transport, or model types.
pub trait McpConnection: Send + Sync {
    fn list_tools<'a>(
        &'a self,
        ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>>;

    fn call_tool<'a>(
        &'a self,
        ctx: &'a CommandContext,
        name: &'a str,
        args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>>;

    fn instructions(&self) -> Option<&str>;

    fn close<'a>(
        self: Box<Self>,
        ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>>;

    fn mode(&self) -> ConnectionMode;
}

/// Injectable direct-transport factory.
pub trait DirectConnector: Send + Sync {
    fn connect<'a>(
        &'a self,
        ctx: &'a CommandContext,
        server: &'a ServerDefinition,
    ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, ConnectionError>>;
}

/// Platform-neutral connection selection boundary used by commands.
pub trait ConnectionManager: Send + Sync {
    fn acquire<'a>(
        &'a self,
        ctx: &'a CommandContext,
        server: &'a ServerDefinition,
    ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, CliError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepts_connection_object(_: &dyn McpConnection) {}
    fn accepts_connector_object(_: &dyn DirectConnector) {}
    fn accepts_manager_object(_: &dyn ConnectionManager) {}

    #[test]
    fn asynchronous_boundaries_are_object_safe() {
        let connection_check: fn(&dyn McpConnection) = accepts_connection_object;
        let connector_check: fn(&dyn DirectConnector) = accepts_connector_object;
        let manager_check: fn(&dyn ConnectionManager) = accepts_manager_object;

        let _ = (connection_check, connector_check, manager_check);
    }
}
