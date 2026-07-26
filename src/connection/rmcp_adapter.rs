//! rmcp stdio transport adapter boundary.
//!
//! All rmcp model, service, and transport types are intentionally confined to
//! this module. Command and domain code interact only through `DirectConnector`
//! and `McpConnection`.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    error::Error,
    ffi::OsString,
    fmt,
    future::Future,
    io,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::Duration,
};

use futures::{StreamExt, stream::BoxStream};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use rmcp::{
    ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, ClientCapabilities, ClientInfo,
        ClientJsonRpcMessage, ClientRequest, Implementation, PaginatedRequestParams,
        ServerJsonRpcMessage, ServerResult, Tool,
    },
    service::{PeerRequestOptions, RoleClient, RunningService, RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{
        Transport,
        streamable_http_client::{
            StreamableHttpClient, StreamableHttpClientTransport,
            StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
        },
    },
};
use serde_json::Value;
use sse_stream::{Error as SseError, Sse, SseStream};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, Command},
    sync::Mutex,
    task::JoinHandle,
    time::timeout,
};

use crate::{
    config::{ServerDefinition, TransportConfig},
    domain::{ConnectionMode, JsonObject, ToolInfo, ToolResult},
    output::DiagnosticSink,
    runtime::{BoxFuture, CommandContext},
};

use super::{ConnectionError, DirectConnector, McpConnection, direct::merge_stdio_environment};

const CLOSE_GRACE: Duration = Duration::from_secs(2);
const HTTP_CLOSE_GRACE: Duration = Duration::from_secs(6);
const STDERR_BUFFER_SIZE: usize = 8 * 1024;
const EVENT_STREAM_MIME_TYPE: &str = "text/event-stream";
const JSON_MIME_TYPE: &str = "application/json";
const HEADER_SESSION_ID: &str = "Mcp-Session-Id";
const HEADER_LAST_EVENT_ID: &str = "Last-Event-Id";
const PROTOCOL_MANAGED_HEADERS: [&str; 4] = [
    "accept",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

/// rmcp-backed direct connector for stdio and Streamable HTTP.
#[derive(Clone, Copy, Debug, Default)]
pub struct RmcpDirectConnector;

#[derive(Clone, Debug, PartialEq, Eq)]
struct StdioLaunchPlan {
    executable: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    environment: BTreeMap<String, String>,
}

impl StdioLaunchPlan {
    fn from_transport(
        transport: &TransportConfig,
        parent_environment: &BTreeMap<String, String>,
    ) -> Option<Self> {
        let TransportConfig::Stdio {
            command,
            args,
            env,
            cwd,
        } = transport
        else {
            return None;
        };

        Some(Self {
            executable: command.clone(),
            args: args.clone(),
            cwd: cwd.clone(),
            // This is the sole stdio environment merge boundary. The complete
            // result is installed after env_clear(), so the child cannot inherit
            // an unplanned value and configured entries always win.
            environment: merge_stdio_environment(parent_environment, env),
        })
    }
}

fn capture_parent_environment() -> Result<BTreeMap<String, String>, ConnectionError> {
    std::env::vars_os()
        .map(|(key, value)| {
            Ok((
                unicode_environment_component(key)?,
                unicode_environment_component(value)?,
            ))
        })
        .collect()
}

fn unicode_environment_component(value: OsString) -> Result<String, ConnectionError> {
    value.into_string().map_err(|_| {
        ConnectionError::new(
            "cannot launch stdio server because the parent environment contains non-UTF-8 data",
        )
    })
}

fn configure_stdio_command(plan: &StdioLaunchPlan) -> Command {
    // Security boundary: the configured command is the executable and every
    // configured argument is passed independently. No shell is involved.
    let mut command = Command::new(&plan.executable);
    command.args(&plan.args);
    if let Some(cwd) = &plan.cwd {
        command.current_dir(cwd);
    }
    command
        .env_clear()
        .envs(&plan.environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

#[derive(Clone)]
struct HttpTransportPlan {
    uri: Arc<str>,
    headers: HashMap<HeaderName, HeaderValue>,
}

impl fmt::Debug for HttpTransportPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpTransportPlan")
            .field("header_count", &self.headers.len())
            .finish_non_exhaustive()
    }
}

impl HttpTransportPlan {
    fn from_transport(
        transport: &TransportConfig,
        server_name: &str,
    ) -> Option<Result<Self, ConnectionError>> {
        let TransportConfig::Http { url, headers } = transport else {
            return None;
        };

        let result = (|| {
            if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
                return Err(ConnectionError::new(format!(
                    "invalid Streamable HTTP URL for server {server_name:?}"
                )));
            }
            let headers = build_http_headers(headers).map_err(|source| {
                ConnectionError::with_source(
                    format!("invalid HTTP headers for server {server_name:?}"),
                    source,
                )
            })?;
            Ok(Self {
                uri: Arc::from(url.as_str()),
                headers,
            })
        })();
        Some(result)
    }

    fn transport_config(&self) -> StreamableHttpClientTransportConfig {
        StreamableHttpClientTransportConfig::with_uri(Arc::clone(&self.uri))
            .custom_headers(self.headers.clone())
    }
}

#[derive(Debug)]
enum HttpHeaderError {
    InvalidName(reqwest::header::InvalidHeaderName),
    InvalidValue(reqwest::header::InvalidHeaderValue),
    ProtocolManaged,
}

impl fmt::Display for HttpHeaderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(_) => formatter.write_str("invalid header name"),
            Self::InvalidValue(_) => formatter.write_str("invalid header value"),
            Self::ProtocolManaged => formatter.write_str("protocol-managed header"),
        }
    }
}

impl Error for HttpHeaderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidName(source) => Some(source),
            Self::InvalidValue(source) => Some(source),
            Self::ProtocolManaged => None,
        }
    }
}

fn build_http_headers(
    configured: &BTreeMap<String, String>,
) -> Result<HashMap<HeaderName, HeaderValue>, HttpHeaderError> {
    configured
        .iter()
        .map(|(name, value)| {
            let name =
                HeaderName::from_bytes(name.as_bytes()).map_err(HttpHeaderError::InvalidName)?;
            if PROTOCOL_MANAGED_HEADERS
                .iter()
                .any(|reserved| name.as_str().eq_ignore_ascii_case(reserved))
            {
                return Err(HttpHeaderError::ProtocolManaged);
            }
            let value = HeaderValue::from_str(value).map_err(HttpHeaderError::InvalidValue)?;
            Ok((name, value))
        })
        .collect()
}

enum SafeHttpClientError {
    Request(reqwest::Error),
    Status(u16),
    Cancelled,
}

impl fmt::Debug for SafeHttpClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SafeHttpClientError")
            .field("status", &self.status())
            .field("cancelled", &matches!(self, Self::Cancelled))
            .field("has_source", &matches!(self, Self::Request(_)))
            .finish()
    }
}

impl SafeHttpClientError {
    fn status(&self) -> Option<u16> {
        match self {
            Self::Request(source) => source.status().map(|status| status.as_u16()),
            Self::Status(status) => Some(*status),
            Self::Cancelled => None,
        }
    }
}

impl fmt::Display for SafeHttpClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(_) => formatter.write_str("HTTP request failed"),
            Self::Status(status) => write!(formatter, "HTTP status {status}"),
            Self::Cancelled => formatter.write_str("HTTP request cancelled"),
        }
    }
}

impl Error for SafeHttpClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Request(source) => Some(source),
            Self::Status(_) | Self::Cancelled => None,
        }
    }
}

impl From<reqwest::Error> for SafeHttpClientError {
    fn from(source: reqwest::Error) -> Self {
        Self::Request(source)
    }
}

#[derive(Default)]
struct HttpRequestShutdown {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl HttpRequestShutdown {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    async fn cancelled(&self) {
        if self.cancelled.load(Ordering::SeqCst) {
            return;
        }
        let notified = self.notify.notified();
        if self.cancelled.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }
}

#[derive(Clone)]
struct TrackingHttpClient {
    client: reqwest::Client,
    wire: Arc<StdMutex<WireTracker>>,
    shutdown: Arc<HttpRequestShutdown>,
    last_status: Arc<AtomicU16>,
}

impl TrackingHttpClient {
    fn new(
        wire: Arc<StdMutex<WireTracker>>,
        shutdown: Arc<HttpRequestShutdown>,
        last_status: Arc<AtomicU16>,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            wire,
            shutdown,
            last_status,
        })
    }

    fn status_error(
        &self,
        status: reqwest::StatusCode,
    ) -> StreamableHttpError<SafeHttpClientError> {
        let status = status.as_u16();
        self.last_status.store(status, Ordering::SeqCst);
        StreamableHttpError::Client(SafeHttpClientError::Status(status))
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        cancellable: bool,
    ) -> Result<reqwest::Response, StreamableHttpError<SafeHttpClientError>> {
        if cancellable {
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    Err(StreamableHttpError::Client(SafeHttpClientError::Cancelled))
                }
                result = request.send() => result
                    .map_err(SafeHttpClientError::from)
                    .map_err(StreamableHttpError::Client),
            }
        } else {
            request
                .send()
                .await
                .map_err(SafeHttpClientError::from)
                .map_err(StreamableHttpError::Client)
        }
    }

    fn apply_headers(
        mut request: reqwest::RequestBuilder,
        auth_token: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> reqwest::RequestBuilder {
        if let Some(auth_token) = auth_token {
            request = request.bearer_auth(auth_token);
        }
        for (name, value) in headers {
            request = request.header(name, value);
        }
        request
    }

    fn track_sse(&self, response: reqwest::Response) -> BoxStream<'static, Result<Sse, SseError>> {
        let wire = Arc::clone(&self.wire);
        SseStream::from_bytes_stream(response.bytes_stream())
            .inspect(move |event| {
                if let Ok(event) = event
                    && let Some(data) = &event.data
                    && let Ok(value) = serde_json::from_str::<Value>(data)
                {
                    wire.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .observe_inbound(&value);
                }
            })
            .boxed()
    }

    fn observe_json(&self, value: &Value) {
        self.wire
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .observe_inbound(value);
    }
}

impl StreamableHttpClient for TrackingHttpClient {
    type Error = SafeHttpClientError;

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let mut request = self
            .client
            .get(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "))
            .header(HEADER_SESSION_ID, session_id.as_ref());
        if let Some(last_event_id) = last_event_id {
            request = request.header(HEADER_LAST_EVENT_ID, last_event_id);
        }
        request = Self::apply_headers(request, auth_token, custom_headers);
        let response = self.send(request, true).await?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        if !response.status().is_success() {
            return Err(self.status_error(response.status()));
        }
        validate_response_content_type(response.headers())?;
        Ok(self.track_sse(response))
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let request = self
            .client
            .delete(uri.as_ref())
            .header(HEADER_SESSION_ID, session_id.as_ref());
        let request = Self::apply_headers(request, auth_token, custom_headers);
        let response = self.send(request, false).await?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Ok(());
        }
        if !response.status().is_success() {
            return Err(self.status_error(response.status()));
        }
        Ok(())
    }

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let raw_request = serde_json::to_value(&message)?;
        self.wire
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .observe_outbound(&raw_request);

        let mut request = self
            .client
            .post(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "));
        request = Self::apply_headers(request, auth_token, custom_headers);
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        let request = request.json(&message);
        let response = self.send(request, true).await?;
        let status = response.status();
        if matches!(
            status,
            reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::NO_CONTENT
        ) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == reqwest::StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        if !status.is_success() {
            return Err(self.status_error(status));
        }

        let content_type = content_type(response.headers());
        let content_length = response.content_length();
        let returned_session = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if content_length == Some(0)
            && matches!(
                message,
                ClientJsonRpcMessage::Notification(_)
                    | ClientJsonRpcMessage::Response(_)
                    | ClientJsonRpcMessage::Error(_)
            )
        {
            return Ok(StreamableHttpPostResponse::Accepted);
        }

        match content_type.as_deref() {
            Some(value) if value.starts_with(EVENT_STREAM_MIME_TYPE) => Ok(
                StreamableHttpPostResponse::Sse(self.track_sse(response), returned_session),
            ),
            Some(value) if value.starts_with(JSON_MIME_TYPE) => {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(SafeHttpClientError::from)
                    .map_err(StreamableHttpError::Client)?;
                let raw = serde_json::from_slice::<Value>(&bytes)?;
                self.observe_json(&raw);
                let message = serde_json::from_value::<ServerJsonRpcMessage>(raw)?;
                Ok(StreamableHttpPostResponse::Json(message, returned_session))
            }
            _ => Err(StreamableHttpError::UnexpectedContentType(content_type)),
        }
    }
}

fn content_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
}

fn validate_response_content_type(
    headers: &HeaderMap,
) -> Result<(), StreamableHttpError<SafeHttpClientError>> {
    let content_type = content_type(headers);
    if content_type.as_deref().is_some_and(|value| {
        value.starts_with(EVENT_STREAM_MIME_TYPE) || value.starts_with(JSON_MIME_TYPE)
    }) {
        Ok(())
    } else {
        Err(StreamableHttpError::UnexpectedContentType(content_type))
    }
}

/// Owns stdio resources while MCP initialization is in flight.
///
/// The outer command deadline may cancel `DirectConnector::connect` at any
/// await point. Keeping the child and stderr task in this guard makes that
/// cancellation path schedule the same kill/wait/flush sequence as an explicit
/// initialization failure instead of relying only on `Child::kill_on_drop`.
struct PendingStdioResources {
    child: Option<Child>,
    stderr_task: Option<JoinHandle<io::Result<()>>>,
    server_name: String,
    diagnostics: Arc<dyn DiagnosticSink>,
}

impl PendingStdioResources {
    fn new(child: Child, server_name: String, diagnostics: Arc<dyn DiagnosticSink>) -> Self {
        Self {
            child: Some(child),
            stderr_task: None,
            server_name,
            diagnostics,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child
            .as_mut()
            .expect("pending stdio child is present until connection handoff")
    }

    async fn cleanup(&mut self) -> io::Result<()> {
        let mut first_error = None;
        if let Some(mut child) = self.child.take() {
            let mut state = CloseState::new();
            state.transport_closed();
            if let Err(error) = shutdown_child(&mut child, &mut state).await {
                first_error = Some(error);
            }
        }
        if let Some(stderr_task) = self.stderr_task.take()
            && let Err(error) = finish_stderr_task(
                stderr_task,
                &self.server_name,
                &self.diagnostics,
                CLOSE_GRACE,
            )
            .await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn take_initialized(&mut self) -> (Child, JoinHandle<io::Result<()>>) {
        (
            self.child
                .take()
                .expect("initialized stdio connection retains its child"),
            self.stderr_task
                .take()
                .expect("initialized stdio connection retains stderr forwarding"),
        )
    }
}

impl Drop for PendingStdioResources {
    fn drop(&mut self) {
        if self.child.is_none() && self.stderr_task.is_none() {
            return;
        }

        let mut pending = Self {
            child: self.child.take(),
            stderr_task: self.stderr_task.take(),
            server_name: self.server_name.clone(),
            diagnostics: Arc::clone(&self.diagnostics),
        };
        let diagnostics = Arc::clone(&self.diagnostics);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if pending.cleanup().await.is_err() {
                        diagnostics.debug(
                            "stdio initialization cancellation cleanup did not complete cleanly",
                        );
                    }
                });
            }
            Err(_) => {
                if let Some(child) = pending.child.as_mut() {
                    let _ = child.start_kill();
                }
                diagnostics
                    .debug("stdio initialization cancellation cleanup could not be scheduled");
            }
        }
    }
}

impl RmcpDirectConnector {
    /// Connects with an explicit parent-environment snapshot.
    ///
    /// Daemon workers use this boundary so their own process environment can
    /// remain empty: the parent snapshot travels inside the protected stdin
    /// bootstrap envelope and is applied only to configured stdio backends.
    pub fn connect_with_parent_environment<'a>(
        &'a self,
        ctx: &'a CommandContext,
        server: &'a ServerDefinition,
        parent_environment: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, ConnectionError>> {
        Box::pin(connect_with_parent_environment(
            ctx,
            server,
            parent_environment,
        ))
    }
}

impl DirectConnector for RmcpDirectConnector {
    fn connect<'a>(
        &'a self,
        ctx: &'a CommandContext,
        server: &'a ServerDefinition,
    ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, ConnectionError>> {
        Box::pin(async move {
            let parent_environment = capture_parent_environment()?;
            connect_with_parent_environment(ctx, server, &parent_environment).await
        })
    }
}

async fn connect_with_parent_environment(
    ctx: &CommandContext,
    server: &ServerDefinition,
    parent_environment: &BTreeMap<String, String>,
) -> Result<Box<dyn McpConnection>, ConnectionError> {
    if let Some(plan) = HttpTransportPlan::from_transport(&server.transport, &server.name) {
        return connect_http(ctx, server, plan?).await;
    }

    let plan = StdioLaunchPlan::from_transport(&server.transport, parent_environment)
        .expect("non-HTTP transport is stdio");

    let child = configure_stdio_command(&plan)
        .spawn()
        .map_err(|source| adapter_error("start stdio server", &server.name, source))?;
    let mut pending =
        PendingStdioResources::new(child, server.name.clone(), Arc::clone(&ctx.diagnostics));
    let stdin = pending.child_mut().stdin.take().ok_or_else(|| {
        ConnectionError::new(format!(
            "failed to acquire stdin for stdio server {:?}",
            server.name
        ))
    })?;
    let stdout = pending.child_mut().stdout.take().ok_or_else(|| {
        ConnectionError::new(format!(
            "failed to acquire stdout for stdio server {:?}",
            server.name
        ))
    })?;
    let stderr = pending.child_mut().stderr.take().ok_or_else(|| {
        ConnectionError::new(format!(
            "failed to acquire stderr for stdio server {:?}",
            server.name
        ))
    })?;

    pending.stderr_task = Some(tokio::spawn(forward_server_stderr(
        stderr,
        server.name.clone(),
        Arc::clone(&ctx.diagnostics),
    )));
    let wire = Arc::new(StdMutex::new(WireTracker::default()));
    let transport = DirectStdioTransport::new(stdout, stdin, Arc::clone(&wire));
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("mcp-cli", env!("CARGO_PKG_VERSION")),
    );

    let initialization = async {
        if ctx.is_cancelled() {
            return Err(ConnectionError::cancelled(format!(
                "cancelled while initializing MCP stdio connection for server {:?}",
                server.name
            )));
        }
        if ctx.deadline.expires_at() <= std::time::Instant::now() {
            return Err(ConnectionError::timed_out(format!(
                "timed out while initializing MCP stdio connection for server {:?}",
                server.name
            )));
        }
        tokio::select! {
            biased;
            _ = wait_for_context_cancellation(ctx) => Err(ConnectionError::cancelled(format!(
                "cancelled while initializing MCP stdio connection for server {:?}",
                server.name
            ))),
            _ = tokio::time::sleep_until(ctx.deadline.expires_at().into()) => Err(ConnectionError::timed_out(format!(
                "timed out while initializing MCP stdio connection for server {:?}",
                server.name
            ))),
            result = client_info.serve(transport) => result.map_err(|source| adapter_error(
                "initialize MCP stdio connection",
                &server.name,
                source,
            )),
        }
    };

    let running = match initialization.await {
        Ok(running) => running,
        Err(error) => {
            if pending.cleanup().await.is_err() {
                ctx.diagnostics.debug(&format!(
                            "stdio server {:?} cleanup after initialization failure did not complete cleanly",
                            server.name
                        ));
            }
            return Err(error);
        }
    };

    let instructions = running
        .peer_info()
        .and_then(|info| info.instructions.clone());
    let (child, stderr_task) = pending.take_initialized();
    Ok(Box::new(StdioMcpConnection {
        server_name: server.name.clone(),
        instructions,
        service: Mutex::new(Some(running)),
        child: Mutex::new(Some(child)),
        stderr_task: Mutex::new(Some(stderr_task)),
        diagnostics: Arc::clone(&ctx.diagnostics),
        wire,
    }) as Box<dyn McpConnection>)
}

async fn connect_http(
    ctx: &CommandContext,
    server: &ServerDefinition,
    plan: HttpTransportPlan,
) -> Result<Box<dyn McpConnection>, ConnectionError> {
    let wire = Arc::new(StdMutex::new(WireTracker::default()));
    let request_shutdown = Arc::new(HttpRequestShutdown::default());
    let last_status = Arc::new(AtomicU16::new(0));
    let client = TrackingHttpClient::new(
        Arc::clone(&wire),
        Arc::clone(&request_shutdown),
        Arc::clone(&last_status),
    )
    .map_err(|source| http_adapter_error("build Streamable HTTP client", &server.name, source))?;
    let transport = StreamableHttpClientTransport::with_client(client, plan.transport_config());
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("mcp-cli", env!("CARGO_PKG_VERSION")),
    );
    let running =
        match await_http_operation(ctx, "initialize MCP HTTP connection", &server.name, async {
            client_info.serve(transport).await.map_err(|source| {
                let error =
                    http_adapter_error("initialize MCP HTTP connection", &server.name, source);
                let status = last_status.load(Ordering::SeqCst);
                if status == 0 {
                    error
                } else {
                    error.with_http_status(status)
                }
            })
        })
        .await
        {
            Ok(running) => running,
            Err(error) => {
                request_shutdown.cancel();
                return Err(error);
            }
        };
    let instructions = running
        .peer_info()
        .and_then(|info| info.instructions.clone());

    Ok(Box::new(HttpMcpConnection {
        server_name: server.name.clone(),
        instructions,
        service: Mutex::new(Some(running)),
        wire,
        request_shutdown,
    }))
}

async fn await_http_operation<T, F>(
    ctx: &CommandContext,
    operation: &str,
    server_name: &str,
    future: F,
) -> Result<T, ConnectionError>
where
    F: Future<Output = Result<T, ConnectionError>>,
{
    if ctx.is_cancelled() {
        return Err(ConnectionError::cancelled(format!(
            "cancelled while attempting to {operation} for server {server_name:?}"
        )));
    }
    if ctx.deadline.expires_at() <= std::time::Instant::now() {
        return Err(ConnectionError::timed_out(format!(
            "timed out while attempting to {operation} for server {server_name:?}"
        )));
    }

    tokio::select! {
        biased;
        _ = wait_for_context_cancellation(ctx) => Err(ConnectionError::cancelled(format!(
            "cancelled while attempting to {operation} for server {server_name:?}"
        ))),
        _ = tokio::time::sleep_until(ctx.deadline.expires_at().into()) => Err(ConnectionError::timed_out(format!(
            "timed out while attempting to {operation} for server {server_name:?}"
        ))),
        result = future => result,
    }
}

async fn wait_for_context_cancellation(ctx: &CommandContext) {
    let mut poll = tokio::time::interval(Duration::from_millis(5));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        poll.tick().await;
        if ctx.is_cancelled() {
            return;
        }
    }
}

struct HttpMcpConnection {
    server_name: String,
    instructions: Option<String>,
    service: Mutex<Option<RunningService<RoleClient, ClientInfo>>>,
    wire: Arc<StdMutex<WireTracker>>,
    request_shutdown: Arc<HttpRequestShutdown>,
}

impl McpConnection for HttpMcpConnection {
    fn list_tools<'a>(
        &'a self,
        ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        Box::pin(async move {
            let service_guard = self.service.lock().await;
            let service = service_guard.as_ref().ok_or_else(|| {
                ConnectionError::new(format!(
                    "MCP HTTP connection for server {:?} is closed",
                    self.server_name
                ))
            })?;
            await_http_operation(
                ctx,
                "list tools",
                &self.server_name,
                list_tools(service, &self.server_name, true),
            )
            .await
        })
    }

    fn call_tool<'a>(
        &'a self,
        ctx: &'a CommandContext,
        name: &'a str,
        args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        Box::pin(async move {
            let service_guard = self.service.lock().await;
            let service = service_guard.as_ref().ok_or_else(|| {
                ConnectionError::new(format!(
                    "MCP HTTP connection for server {:?} is closed",
                    self.server_name
                ))
            })?;
            await_http_operation(
                ctx,
                "call tool",
                &self.server_name,
                call_tool(service, &self.wire, &self.server_name, name, args, true),
            )
            .await
        })
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    fn close<'a>(
        self: Box<Self>,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>> {
        Box::pin(async move {
            self.request_shutdown.cancel();
            let Some(mut service) = self.service.lock().await.take() else {
                return Ok(());
            };
            match service.close_with_timeout(HTTP_CLOSE_GRACE).await {
                Ok(Some(_)) => Ok(()),
                Ok(None) => Err(ConnectionError::new(format!(
                    "timed out closing MCP HTTP transport for server {:?}",
                    self.server_name
                ))),
                Err(source) => Err(http_adapter_error(
                    "close MCP HTTP transport",
                    &self.server_name,
                    source,
                )),
            }
        })
    }

    fn mode(&self) -> ConnectionMode {
        ConnectionMode::Direct
    }
}

struct StdioMcpConnection {
    server_name: String,
    instructions: Option<String>,
    service: Mutex<Option<RunningService<RoleClient, ClientInfo>>>,
    child: Mutex<Option<Child>>,
    stderr_task: Mutex<Option<JoinHandle<io::Result<()>>>>,
    diagnostics: Arc<dyn DiagnosticSink>,
    wire: Arc<StdMutex<WireTracker>>,
}

impl McpConnection for StdioMcpConnection {
    fn list_tools<'a>(
        &'a self,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        Box::pin(async move {
            // Holding this lock serializes requests with call/close and also
            // makes raw-result correlation deterministic.
            let service_guard = self.service.lock().await;
            let service = service_guard.as_ref().ok_or_else(|| {
                ConnectionError::new(format!(
                    "MCP stdio connection for server {:?} is closed",
                    self.server_name
                ))
            })?;

            let mut pagination = PaginationGuard::default();
            let mut cursor = None;
            let mut tools = Vec::new();
            loop {
                let page = service
                    .list_tools(Some(
                        PaginatedRequestParams::default().with_cursor(cursor.clone()),
                    ))
                    .await
                    .map_err(|source| adapter_error("list tools", &self.server_name, source))?;
                tools.extend(page.tools.into_iter().map(map_tool));
                cursor = pagination.advance(page.next_cursor, &self.server_name)?;
                if cursor.is_none() {
                    break;
                }
            }
            Ok(tools)
        })
    }

    fn call_tool<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        name: &'a str,
        args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        Box::pin(async move {
            let service_guard = self.service.lock().await;
            let service = service_guard.as_ref().ok_or_else(|| {
                ConnectionError::new(format!(
                    "MCP stdio connection for server {:?} is closed",
                    self.server_name
                ))
            })?;

            let params = CallToolRequestParams::new(name.to_owned()).with_arguments(args);
            let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
            let handle = service
                .send_request_with_option(request, PeerRequestOptions::no_options())
                .await
                .map_err(|source| adapter_error("call tool", &self.server_name, source))?;
            let request_key = request_id_key(&handle.id).map_err(|source| {
                adapter_error("correlate tool result", &self.server_name, source)
            })?;
            let response = handle
                .await_response()
                .await
                .map_err(|source| adapter_error("call tool", &self.server_name, source))?;
            let ServerResult::CallToolResult(typed_result) = response else {
                return Err(ConnectionError::new(format!(
                    "server {:?} returned an unexpected tools/call response",
                    self.server_name
                )));
            };

            let raw_result = self
                .wire
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take_call_result(&request_key);
            preserve_tool_result(raw_result, &typed_result)
                .map_err(|source| adapter_error("serialize tool result", &self.server_name, source))
        })
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    fn close<'a>(
        self: Box<Self>,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>> {
        Box::pin(async move {
            let mut first_error = None;
            let mut state = CloseState::new();

            if let Some(mut service) = self.service.lock().await.take() {
                match timeout(CLOSE_GRACE, service.close()).await {
                    Ok(Ok(_)) => state.transport_closed(),
                    Ok(Err(source)) => {
                        state.transport_closed();
                        first_error = Some(adapter_error(
                            "close MCP stdio transport",
                            &self.server_name,
                            source,
                        ));
                    }
                    Err(_) => {
                        state.transport_closed();
                        first_error = Some(ConnectionError::new(format!(
                            "timed out closing MCP stdio transport for server {:?}",
                            self.server_name
                        )));
                    }
                }
            } else {
                state.transport_closed();
            }

            if let Some(mut child) = self.child.lock().await.take()
                && let Err(source) = shutdown_child(&mut child, &mut state).await
                && first_error.is_none()
            {
                first_error = Some(adapter_error(
                    "reap stdio server process",
                    &self.server_name,
                    source,
                ));
            }

            if let Some(stderr_task) = self.stderr_task.lock().await.take() {
                let stderr_result = finish_stderr_task(
                    stderr_task,
                    &self.server_name,
                    &self.diagnostics,
                    CLOSE_GRACE,
                )
                .await;
                if let Err(source) = stderr_result
                    && first_error.is_none()
                {
                    first_error = Some(adapter_error(
                        "forward stdio server stderr",
                        &self.server_name,
                        source,
                    ));
                }
            } else {
                self.diagnostics.server_stderr_flush(&self.server_name);
            }

            debug_assert_eq!(state, CloseState::Reaped);
            match first_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }

    fn mode(&self) -> ConnectionMode {
        ConnectionMode::Direct
    }
}

async fn list_tools(
    service: &RunningService<RoleClient, ClientInfo>,
    server_name: &str,
    http: bool,
) -> Result<Vec<ToolInfo>, ConnectionError> {
    let mut pagination = PaginationGuard::default();
    let mut cursor = None;
    let mut tools = Vec::new();
    loop {
        let page = service
            .list_tools(Some(
                PaginatedRequestParams::default().with_cursor(cursor.clone()),
            ))
            .await
            .map_err(|source| transport_adapter_error("list tools", server_name, source, http))?;
        tools.extend(page.tools.into_iter().map(map_tool));
        cursor = pagination.advance(page.next_cursor, server_name)?;
        if cursor.is_none() {
            break;
        }
    }
    Ok(tools)
}

async fn call_tool(
    service: &RunningService<RoleClient, ClientInfo>,
    wire: &Arc<StdMutex<WireTracker>>,
    server_name: &str,
    name: &str,
    args: JsonObject,
    http: bool,
) -> Result<ToolResult, ConnectionError> {
    let params = CallToolRequestParams::new(name.to_owned()).with_arguments(args);
    let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
    let handle = service
        .send_request_with_option(request, PeerRequestOptions::no_options())
        .await
        .map_err(|source| transport_adapter_error("call tool", server_name, source, http))?;
    let request_key = request_id_key(&handle.id)
        .map_err(|source| adapter_error("correlate tool result", server_name, source))?;
    let response = handle
        .await_response()
        .await
        .map_err(|source| transport_adapter_error("call tool", server_name, source, http))?;
    let ServerResult::CallToolResult(typed_result) = response else {
        return Err(ConnectionError::new(format!(
            "server {server_name:?} returned an unexpected tools/call response"
        )));
    };
    let raw_result = wire
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take_call_result(&request_key);
    preserve_tool_result(raw_result, &typed_result)
        .map_err(|source| adapter_error("serialize tool result", server_name, source))
}

fn map_tool(tool: Tool) -> ToolInfo {
    ToolInfo {
        name: tool.name.into_owned(),
        description: tool.description.map(|description| description.into_owned()),
        input_schema: Value::Object(tool.input_schema.as_ref().clone()),
    }
}

#[derive(Default)]
struct PaginationGuard {
    observed: BTreeSet<String>,
}

impl PaginationGuard {
    fn advance(
        &mut self,
        next_cursor: Option<String>,
        server_name: &str,
    ) -> Result<Option<String>, ConnectionError> {
        if let Some(cursor) = &next_cursor
            && !self.observed.insert(cursor.clone())
        {
            return Err(ConnectionError::new(format!(
                "server {server_name:?} repeated a tools/list cursor"
            )));
        }
        Ok(next_cursor)
    }
}

fn preserve_tool_result(
    raw_result: Option<Value>,
    typed_result: &rmcp::model::CallToolResult,
) -> Result<Value, serde_json::Error> {
    match raw_result {
        Some(raw_result) => Ok(raw_result),
        None => serde_json::to_value(typed_result),
    }
}

fn transport_adapter_error(
    operation: &str,
    server_name: &str,
    source: impl Error + Send + Sync + 'static,
    http: bool,
) -> ConnectionError {
    if http {
        http_adapter_error(operation, server_name, source)
    } else {
        adapter_error(operation, server_name, source)
    }
}

fn http_adapter_error(
    operation: &str,
    server_name: &str,
    source: impl Error + Send + Sync + 'static,
) -> ConnectionError {
    let status = http_status_from_error(&source);
    let error = adapter_error(operation, server_name, source);
    match status {
        Some(status) => error.with_http_status(status),
        None => error,
    }
}

fn http_status_from_error(source: &(dyn Error + 'static)) -> Option<u16> {
    let mut current = Some(source);
    while let Some(error) = current {
        if let Some(http_error) = error.downcast_ref::<StreamableHttpError<SafeHttpClientError>>() {
            match http_error {
                StreamableHttpError::Client(source) => {
                    if let Some(status) = source.status() {
                        return Some(status);
                    }
                }
                StreamableHttpError::SessionExpired => return Some(404),
                _ => {}
            }
        }
        if let Some(error) = error.downcast_ref::<SafeHttpClientError>()
            && let Some(status) = error.status()
        {
            return Some(status);
        }
        if let Some(error) = error.downcast_ref::<reqwest::Error>()
            && let Some(status) = error.status()
        {
            return Some(status.as_u16());
        }
        current = error.source();
    }
    None
}

fn adapter_error(
    operation: &str,
    server_name: &str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> ConnectionError {
    // The source remains available for typed classification, but the public
    // message never copies transport text that could contain configured env
    // values or server-provided secrets.
    ConnectionError::with_source(
        format!("failed to {operation} for server {server_name:?}"),
        source,
    )
}

async fn forward_server_stderr(
    mut stderr: ChildStderr,
    server_name: String,
    diagnostics: Arc<dyn DiagnosticSink>,
) -> io::Result<()> {
    let mut buffer = vec![0_u8; STDERR_BUFFER_SIZE];
    let result = loop {
        match stderr.read(&mut buffer).await {
            Ok(0) => break Ok(()),
            Ok(read) => diagnostics.server_stderr(&server_name, &buffer[..read]),
            Err(error) => break Err(error),
        }
    };
    diagnostics.server_stderr_flush(&server_name);
    result
}

async fn finish_stderr_task(
    mut task: JoinHandle<io::Result<()>>,
    server_name: &str,
    diagnostics: &Arc<dyn DiagnosticSink>,
    grace: Duration,
) -> Result<(), io::Error> {
    match timeout(grace, &mut task).await {
        Ok(Ok(result)) => result,
        Ok(Err(join_error)) => Err(io::Error::other(join_error)),
        Err(_) => {
            task.abort();
            let _ = task.await;
            diagnostics.server_stderr_flush(server_name);
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "stderr forwarding task did not finish after child exit",
            ))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloseState {
    ClosingTransport,
    WaitingForChild,
    KillingChild,
    Reaped,
}

impl CloseState {
    fn new() -> Self {
        Self::ClosingTransport
    }

    fn transport_closed(&mut self) {
        debug_assert_eq!(*self, Self::ClosingTransport);
        *self = Self::WaitingForChild;
    }

    fn child_exited(&mut self) {
        debug_assert!(matches!(*self, Self::WaitingForChild | Self::KillingChild));
        *self = Self::Reaped;
    }

    fn grace_expired(&mut self) {
        debug_assert_eq!(*self, Self::WaitingForChild);
        *self = Self::KillingChild;
    }
}

async fn shutdown_child(child: &mut Child, state: &mut CloseState) -> io::Result<()> {
    debug_assert_eq!(*state, CloseState::WaitingForChild);
    if child.try_wait()?.is_some() {
        state.child_exited();
        return Ok(());
    }

    match timeout(CLOSE_GRACE, child.wait()).await {
        Ok(status) => {
            status?;
            state.child_exited();
            Ok(())
        }
        Err(_) => {
            state.grace_expired();
            child.start_kill()?;
            // Always wait after kill: this is the zombie-prevention boundary.
            child.wait().await?;
            state.child_exited();
            Ok(())
        }
    }
}

#[derive(Default)]
struct WireTracker {
    call_request_ids: HashSet<String>,
    call_results: HashMap<String, Value>,
}

impl WireTracker {
    fn observe_outbound(&mut self, message: &Value) {
        if message.get("method").and_then(Value::as_str) != Some("tools/call") {
            return;
        }
        if let Some(id) = message.get("id").and_then(value_key) {
            self.call_request_ids.insert(id);
        }
    }

    fn outbound_failed(&mut self, message: &Value) {
        if let Some(id) = message.get("id").and_then(value_key) {
            self.call_request_ids.remove(&id);
        }
    }

    fn observe_inbound(&mut self, message: &Value) {
        let Some(id) = message.get("id").and_then(value_key) else {
            return;
        };
        if !self.call_request_ids.remove(&id) {
            return;
        }
        if let Some(result) = message.get("result") {
            self.call_results.insert(id, result.clone());
        }
    }

    fn take_call_result(&mut self, request_id: &str) -> Option<Value> {
        self.call_results.remove(request_id)
    }
}

fn request_id_key(id: &rmcp::model::RequestId) -> Result<String, serde_json::Error> {
    serde_json::to_string(id)
}

fn value_key(value: &Value) -> Option<String> {
    if value.is_string() || value.is_number() {
        serde_json::to_string(value).ok()
    } else {
        None
    }
}

struct DirectStdioTransport<R, W> {
    reader: BufReader<R>,
    writer: Arc<Mutex<Option<W>>>,
    wire: Arc<StdMutex<WireTracker>>,
}

impl<R: AsyncRead, W> DirectStdioTransport<R, W> {
    fn new(reader: R, writer: W, wire: Arc<StdMutex<WireTracker>>) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer: Arc::new(Mutex::new(Some(writer))),
            wire,
        }
    }
}

impl<R, W> Transport<RoleClient> for DirectStdioTransport<R, W>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let writer = Arc::clone(&self.writer);
        let wire = Arc::clone(&self.wire);
        async move {
            let message = serde_json::to_value(&item).map_err(invalid_wire_json)?;
            let mut bytes = serde_json::to_vec(&message).map_err(invalid_wire_json)?;
            bytes.push(b'\n');
            wire.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .observe_outbound(&message);

            let result = async {
                let mut writer = writer.lock().await;
                let writer = writer.as_mut().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "stdio transport is closed")
                })?;
                writer.write_all(&bytes).await?;
                writer.flush().await
            }
            .await;
            if result.is_err() {
                wire.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .outbound_failed(&message);
            }
            result
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        let mut frame = Vec::new();
        match self.reader.read_until(b'\n', &mut frame).await {
            Ok(0) | Err(_) => None,
            Ok(_) => {
                while matches!(frame.last(), Some(b'\n' | b'\r')) {
                    frame.pop();
                }
                let raw = serde_json::from_slice::<Value>(&frame).ok()?;
                let typed =
                    serde_json::from_value::<RxJsonRpcMessage<RoleClient>>(raw.clone()).ok()?;
                self.wire
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .observe_inbound(&raw);
                Some(typed)
            }
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let writer = Arc::clone(&self.writer);
        async move {
            let mut writer = writer.lock().await;
            writer.take();
            Ok(())
        }
    }
}

fn invalid_wire_json(source: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, source)
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, path::Path};

    use rmcp::model::CallToolResult;
    use serde_json::json;

    use super::*;

    fn environment(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn stdio_transport() -> TransportConfig {
        TransportConfig::Stdio {
            command: "literal-command;not-a-shell".to_owned(),
            args: vec![
                "--mode".to_owned(),
                "stdio".to_owned(),
                "$(must-not-expand)".to_owned(),
            ],
            env: environment(&[("SHARED", "configured"), ("TOKEN", "secret")]),
            cwd: Some(PathBuf::from("/planned/workspace")),
        }
    }

    #[test]
    fn command_plan_and_command_use_executable_and_independent_args_without_shell() {
        let parent = environment(&[("PATH", "/usr/bin"), ("SHARED", "parent")]);
        let plan = StdioLaunchPlan::from_transport(&stdio_transport(), &parent).unwrap();
        let command = configure_stdio_command(&plan);
        let standard = command.as_std();

        assert_eq!(standard.get_program(), "literal-command;not-a-shell");
        assert_eq!(
            standard.get_args().collect::<Vec<_>>(),
            ["--mode", "stdio", "$(must-not-expand)"]
        );
        assert_eq!(
            standard.get_current_dir(),
            Some(Path::new("/planned/workspace"))
        );
        assert_ne!(standard.get_program(), "sh");
        assert_ne!(standard.get_program(), "cmd");
    }

    #[test]
    fn launch_plan_installs_complete_merged_environment_with_config_overrides() {
        let parent = environment(&[("HOME", "/home/test"), ("SHARED", "parent")]);
        let plan = StdioLaunchPlan::from_transport(&stdio_transport(), &parent).unwrap();

        assert_eq!(
            plan.environment,
            environment(&[
                ("HOME", "/home/test"),
                ("SHARED", "configured"),
                ("TOKEN", "secret"),
            ])
        );
    }

    #[test]
    fn rmcp_tool_mapping_is_transport_independent() {
        let tool = Tool::new_with_raw(
            "search",
            Some(Cow::Borrowed("Search repositories")),
            Arc::new(serde_json::Map::from_iter([
                ("type".to_owned(), json!("object")),
                (
                    "properties".to_owned(),
                    json!({"query": {"type": "string"}}),
                ),
            ])),
        );

        assert_eq!(
            map_tool(tool),
            ToolInfo {
                name: "search".to_owned(),
                description: Some("Search repositories".to_owned()),
                input_schema: json!({
                    "type": "object",
                    "properties": {"query": {"type": "string"}}
                }),
            }
        );
    }

    #[test]
    fn pagination_helper_accepts_new_cursors_and_rejects_repeats() {
        let mut pagination = PaginationGuard::default();

        assert_eq!(
            pagination
                .advance(Some("page-2".to_owned()), "alpha")
                .unwrap(),
            Some("page-2".to_owned())
        );
        assert_eq!(
            pagination
                .advance(Some("page-3".to_owned()), "alpha")
                .unwrap(),
            Some("page-3".to_owned())
        );
        let error = pagination
            .advance(Some("page-2".to_owned()), "alpha")
            .unwrap_err();
        assert!(error.message().contains("repeated"));
        assert_eq!(pagination.advance(None, "alpha").unwrap(), None);
    }

    #[test]
    fn raw_tool_result_preserves_unknown_extension_fields() {
        let raw = json!({
            "content": [{"type": "text", "text": "ok"}],
            "isError": false,
            "vendorExtension": {
                "traceId": "trace-123",
                "futureField": [null, 7]
            }
        });
        let typed = CallToolResult::success(Vec::new());

        assert_eq!(
            preserve_tool_result(Some(raw.clone()), &typed).unwrap(),
            raw
        );
    }

    #[test]
    fn wire_tracker_correlates_only_tools_call_results() {
        let mut tracker = WireTracker::default();
        tracker.observe_outbound(&json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {"name": "echo"}
        }));
        tracker.observe_outbound(&json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/list"
        }));
        tracker.observe_inbound(&json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": {"content": [], "x-extra": true}
        }));
        tracker.observe_inbound(&json!({
            "jsonrpc": "2.0",
            "id": 8,
            "result": {"tools": []}
        }));

        assert_eq!(
            tracker.take_call_result("7"),
            Some(json!({"content": [], "x-extra": true}))
        );
        assert_eq!(tracker.take_call_result("8"), None);
    }

    #[test]
    fn adapter_error_keeps_secret_source_text_out_of_public_message() {
        let error = adapter_error(
            "initialize MCP stdio connection",
            "alpha",
            io::Error::other("server included top-secret-token"),
        );

        assert_eq!(
            error.to_string(),
            "failed to initialize MCP stdio connection for server \"alpha\""
        );
        assert!(!error.message().contains("top-secret-token"));
    }

    fn http_transport(url: &str, headers: &[(&str, &str)]) -> TransportConfig {
        TransportConfig::Http {
            url: url::Url::parse(url).expect("valid test URL"),
            headers: environment(headers),
        }
    }

    #[test]
    fn connector_plans_dispatch_stdio_and_http_without_crossing_transport_state() {
        let parent = environment(&[("PATH", "/usr/bin")]);
        let stdio = stdio_transport();
        assert!(StdioLaunchPlan::from_transport(&stdio, &parent).is_some());
        assert!(HttpTransportPlan::from_transport(&stdio, "local").is_none());

        for url in ["http://127.0.0.1:8123/mcp", "https://mcp.example.test/api"] {
            let http = http_transport(url, &[("X-Client", "mcp-cli")]);
            assert!(StdioLaunchPlan::from_transport(&http, &parent).is_none());
            let plan = HttpTransportPlan::from_transport(&http, "remote")
                .expect("HTTP branch")
                .expect("valid plan");
            assert_eq!(plan.uri.as_ref(), url);
            assert_eq!(plan.headers.len(), 1);
        }
    }

    #[test]
    fn http_header_map_preserves_all_configured_values() {
        let configured = environment(&[
            ("Authorization", "Bearer secret-token"),
            ("Cookie", "session=secret-cookie"),
            ("X-Tenant", "tenant-a"),
        ]);
        let headers = build_http_headers(&configured).expect("valid headers");

        assert_eq!(headers.len(), configured.len());
        assert_eq!(
            headers[&HeaderName::from_static("authorization")],
            "Bearer secret-token"
        );
        assert_eq!(
            headers[&HeaderName::from_static("cookie")],
            "session=secret-cookie"
        );
        assert_eq!(headers[&HeaderName::from_static("x-tenant")], "tenant-a");
    }

    #[test]
    fn invalid_or_protocol_managed_headers_are_rejected_without_value_disclosure() {
        let cases = [
            environment(&[("bad header", "top-secret")]),
            environment(&[("X-Test", "bad\nsecret-value")]),
            environment(&[("Mcp-Session-Id", "session-secret")]),
            environment(&[("Accept", "private-content-type")]),
        ];

        for configured in cases {
            let error = build_http_headers(&configured).expect_err("must reject invalid header");
            let visible = format!("{error} {error:?}");
            assert!(!visible.contains("top-secret"));
            assert!(!visible.contains("secret-value"));
            assert!(!visible.contains("session-secret"));
            assert!(!visible.contains("private-content-type"));
        }
    }

    #[test]
    fn http_plan_debug_and_errors_do_not_disclose_url_credentials_or_headers() {
        let transport = http_transport(
            "https://url-user:url-password@mcp.example.test/api",
            &[
                ("Authorization", "Bearer header-secret"),
                ("Cookie", "cookie-secret"),
            ],
        );
        let plan = HttpTransportPlan::from_transport(&transport, "safe-server")
            .expect("HTTP branch")
            .expect("valid plan");
        let debug = format!("{plan:?} {transport:?}");

        for secret in ["url-user", "url-password", "header-secret", "cookie-secret"] {
            assert!(!debug.contains(secret), "leaked {secret} through Debug");
        }
        assert!(debug.contains("authorization"));
        assert!(debug.contains("cookie"));
    }

    #[test]
    fn safe_http_status_is_retained_for_classification_without_response_text() {
        let source = StreamableHttpError::Client(SafeHttpClientError::Status(503));
        let error = http_adapter_error("list tools", "safe-server", source);
        let visible = format!("{} {error:?}", error.message());

        assert_eq!(error.http_status(), Some(503));
        assert!(visible.contains("safe-server"));
        assert!(visible.contains("503"));
        assert!(!visible.contains("response-body-secret"));
    }

    #[tokio::test]
    async fn http_shutdown_wakes_in_flight_request_waiters() {
        let shutdown = Arc::new(HttpRequestShutdown::default());
        let waiter = Arc::clone(&shutdown);
        let task = tokio::spawn(async move { waiter.cancelled().await });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());

        shutdown.cancel();
        timeout(Duration::from_secs(1), task)
            .await
            .expect("shutdown waiter woke")
            .expect("shutdown waiter joined");
    }

    #[test]
    fn close_state_requires_transport_close_then_wait_or_kill_and_reap() {
        let mut graceful = CloseState::new();
        graceful.transport_closed();
        graceful.child_exited();
        assert_eq!(graceful, CloseState::Reaped);

        let mut forced = CloseState::new();
        forced.transport_closed();
        forced.grace_expired();
        assert_eq!(forced, CloseState::KillingChild);
        forced.child_exited();
        assert_eq!(forced, CloseState::Reaped);
    }
}
