use std::{
    collections::BTreeMap,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Notify, watch},
    task::{JoinHandle, JoinSet},
};

const MAX_REQUEST_HEAD: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestMatcher {
    pub method: String,
    pub rpc_method: Option<String>,
    pub cursor: Option<Option<String>>,
}

impl RequestMatcher {
    pub fn http(method: &str) -> Self {
        Self {
            method: method.to_owned(),
            rpc_method: None,
            cursor: None,
        }
    }

    pub fn rpc(method: &str) -> Self {
        Self {
            method: "POST".to_owned(),
            rpc_method: Some(method.to_owned()),
            cursor: None,
        }
    }

    pub fn rpc_cursor(method: &str, cursor: Option<&str>) -> Self {
        Self {
            method: "POST".to_owned(),
            rpc_method: Some(method.to_owned()),
            cursor: Some(cursor.map(str::to_owned)),
        }
    }

    fn matches(&self, request: &CapturedRequest) -> bool {
        self.method == request.method
            && self
                .rpc_method
                .as_deref()
                .is_none_or(|method| request.rpc_method() == Some(method))
            && self.cursor.as_ref().is_none_or(|cursor| {
                request
                    .body
                    .as_ref()
                    .and_then(|body| body.pointer("/params/cursor"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    == *cursor
            })
    }
}

#[derive(Clone, Debug)]
pub enum MockResponse {
    Json {
        body: Value,
        session_id: Option<String>,
    },
    Sse {
        messages: Vec<Value>,
        session_id: Option<String>,
    },
    Accepted,
    Empty,
    Status(u16),
    /// Keep the request pending until the client closes its socket or the
    /// fixture is shut down. This deterministically exercises cancellation.
    Hold,
    /// Close the socket without writing an HTTP response.
    Disconnect,
    /// Open the server-side GET event stream and keep it alive until the
    /// client closes it or the fixture is shut down.
    OpenGetSse,
}

#[derive(Clone, Debug)]
pub struct ScriptedResponse {
    pub matcher: RequestMatcher,
    pub response: MockResponse,
}

impl ScriptedResponse {
    pub fn new(matcher: RequestMatcher, response: MockResponse) -> Self {
        Self { matcher, response }
    }
}

#[derive(Clone, Debug, Default)]
pub struct MockHttpScript {
    pub responses: Vec<ScriptedResponse>,
}

impl MockHttpScript {
    pub fn new(responses: Vec<ScriptedResponse>) -> Self {
        Self { responses }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CapturedRequest {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub session_id: Option<String>,
    pub protocol_version: Option<String>,
    pub body: Option<Value>,
}

impl CapturedRequest {
    pub fn rpc_method(&self) -> Option<&str> {
        self.body
            .as_ref()
            .and_then(|body| body.get("method"))
            .and_then(Value::as_str)
    }
}

#[derive(Debug, Default)]
struct State {
    responses: Mutex<Vec<ScriptedResponse>>,
    requests: Mutex<Vec<CapturedRequest>>,
    protocol_errors: Mutex<Vec<String>>,
    changed: Notify,
    active_connections: AtomicUsize,
}

pub struct MockHttpServer {
    url: String,
    state: Arc<State>,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl MockHttpServer {
    pub async fn start(script: MockHttpScript) -> io::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let state = Arc::new(State {
            responses: Mutex::new(script.responses),
            ..State::default()
        });
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_server(listener, Arc::clone(&state), shutdown_rx));
        Ok(Self {
            url: format!("http://{address}/mcp"),
            state,
            shutdown,
            task: Some(task),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.state
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn protocol_errors(&self) -> Vec<String> {
        self.state
            .protocol_errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn active_connections(&self) -> usize {
        self.state.active_connections.load(Ordering::SeqCst)
    }

    pub async fn wait_for_requests(&self, count: usize) {
        loop {
            let notified = self.state.changed.notified();
            if self.requests().len() >= count {
                return;
            }
            notified.await;
        }
    }

    pub async fn wait_for_no_connections(&self) {
        loop {
            let notified = self.state.changed.notified();
            if self.active_connections() == 0 {
                return;
            }
            notified.await;
        }
    }

    pub async fn shutdown(&mut self) -> io::Result<()> {
        let _ = self.shutdown.send(true);
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.await.map_err(io::Error::other)??;
        if self.active_connections() != 0 {
            return Err(io::Error::other("mock HTTP server leaked a client socket"));
        }
        Ok(())
    }
}

impl Drop for MockHttpServer {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run_server(
    listener: TcpListener,
    state: Arc<State>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                let _ = changed;
                break;
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                state.active_connections.fetch_add(1, Ordering::SeqCst);
                connections.spawn(handle_connection(
                    stream,
                    Arc::clone(&state),
                    shutdown.clone(),
                ));
            }
            joined = connections.join_next(), if !connections.is_empty() => {
                if let Some(result) = joined {
                    result.map_err(io::Error::other)??;
                }
            }
        }
    }

    while let Some(result) = connections.join_next().await {
        result.map_err(io::Error::other)??;
    }
    Ok(())
}

struct ConnectionGuard(Arc<State>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.active_connections.fetch_sub(1, Ordering::SeqCst);
        self.0.changed.notify_waiters();
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    state: Arc<State>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let _guard = ConnectionGuard(Arc::clone(&state));
    let Some(request) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let response = {
        let mut responses = state
            .responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        responses
            .iter()
            .position(|candidate| candidate.matcher.matches(&request))
            .map(|index| responses.remove(index).response)
    };
    state
        .requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(request.clone());
    state.changed.notify_waiters();

    let Some(response) = response else {
        state
            .protocol_errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(format!(
                "no response scripted for {} {} {:?}",
                request.method,
                request.path,
                request.rpc_method()
            ));
        state.changed.notify_waiters();
        return write_status(&mut stream, 500).await;
    };

    match response {
        MockResponse::Json {
            mut body,
            session_id,
        } => {
            attach_request_id(&mut body, &request);
            write_body(
                &mut stream,
                200,
                "application/json",
                &body.to_string(),
                session_id.as_deref(),
            )
            .await
        }
        MockResponse::Sse {
            mut messages,
            session_id,
        } => {
            for message in &mut messages {
                attach_request_id(message, &request);
            }
            let body = messages
                .into_iter()
                .map(|message| format!("event: message\ndata: {message}\n\n"))
                .collect::<String>();
            write_body(
                &mut stream,
                200,
                "text/event-stream",
                &body,
                session_id.as_deref(),
            )
            .await
        }
        MockResponse::Accepted => write_status(&mut stream, 202).await,
        MockResponse::Empty => write_status(&mut stream, 204).await,
        MockResponse::Status(status) => write_status(&mut stream, status).await,
        MockResponse::Disconnect => Ok(()),
        MockResponse::Hold => wait_for_peer_close(&mut stream, &mut shutdown).await,
        MockResponse::OpenGetSse => {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                )
                .await?;
            stream.flush().await?;
            wait_for_peer_close(&mut stream, &mut shutdown).await
        }
    }
}

fn attach_request_id(message: &mut Value, request: &CapturedRequest) {
    let Some(request_id) = request
        .body
        .as_ref()
        .and_then(|body| body.get("id"))
        .cloned()
    else {
        return;
    };
    if let Some(object) = message.as_object_mut()
        && !object.contains_key("id")
    {
        object.insert("id".to_owned(), request_id);
    }
}

async fn wait_for_peer_close(
    stream: &mut TcpStream,
    shutdown: &mut watch::Receiver<bool>,
) -> io::Result<()> {
    let mut byte = [0_u8; 1];
    tokio::select! {
        _ = shutdown.changed() => Ok(()),
        result = stream.read(&mut byte) => match result {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => Ok(()),
            Err(error) => Err(error),
        },
    }
}

async fn read_request(stream: &mut TcpStream) -> io::Result<Option<CapturedRequest>> {
    let mut bytes = Vec::new();
    let head_end = loop {
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() >= MAX_REQUEST_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP request head exceeded fixture limit",
            ));
        }
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated HTTP request",
                ))
            };
        }
        bytes.extend_from_slice(&chunk[..read]);
    };

    let head = std::str::from_utf8(&bytes[..head_end])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_owned();
    let path = request_parts.next().unwrap_or_default().to_owned();
    let mut headers = BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP header"))?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let content_length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        .unwrap_or(0);
    while bytes.len() - head_end < content_length {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated HTTP body",
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let body = if content_length == 0 {
        None
    } else {
        Some(
            serde_json::from_slice(&bytes[head_end..head_end + content_length])
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        )
    };
    Ok(Some(CapturedRequest {
        method,
        path,
        session_id: headers.get("mcp-session-id").cloned(),
        protocol_version: headers.get("mcp-protocol-version").cloned(),
        headers,
        body,
    }))
}

async fn write_status(stream: &mut TcpStream, status: u16) -> io::Result<()> {
    let reason = match status {
        202 => "Accepted",
        204 => "No Content",
        401 => "Unauthorized",
        403 => "Forbidden",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Scripted Status",
    };
    let response =
        format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

async fn write_body(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
    session_id: Option<&str>,
) -> io::Result<()> {
    let session_header = session_id
        .map(|session_id| format!("Mcp-Session-Id: {session_id}\r\n"))
        .unwrap_or_default();
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{session_header}Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}
