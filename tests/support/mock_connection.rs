use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use mcp_cli::{
    BoxFuture, CliError, CommandContext, ConnectionError, ConnectionManager, ConnectionMode,
    DirectConnector, JsonObject, McpConnection, ServerDefinition, ToolInfo, ToolResult,
};

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionCall {
    ListTools,
    CallTool { name: String, args: JsonObject },
    Close,
}

struct MockConnectionState {
    calls: Mutex<Vec<ConnectionCall>>,
    list_results: Mutex<VecDeque<Result<Vec<ToolInfo>, ConnectionError>>>,
    call_results: Mutex<VecDeque<Result<ToolResult, ConnectionError>>>,
    close_results: Mutex<VecDeque<Result<(), ConnectionError>>>,
    closed: AtomicBool,
}

impl Default for MockConnectionState {
    fn default() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            list_results: Mutex::new(VecDeque::new()),
            call_results: Mutex::new(VecDeque::new()),
            close_results: Mutex::new(VecDeque::new()),
            closed: AtomicBool::new(false),
        }
    }
}

#[derive(Clone)]
pub struct MockConnectionHandle {
    state: Arc<MockConnectionState>,
}

impl MockConnectionHandle {
    pub fn queue_list_result(&self, result: Result<Vec<ToolInfo>, ConnectionError>) {
        self.state
            .list_results
            .lock()
            .expect("mock list lock poisoned")
            .push_back(result);
    }

    pub fn queue_call_result(&self, result: Result<ToolResult, ConnectionError>) {
        self.state
            .call_results
            .lock()
            .expect("mock call lock poisoned")
            .push_back(result);
    }

    pub fn queue_close_result(&self, result: Result<(), ConnectionError>) {
        self.state
            .close_results
            .lock()
            .expect("mock close lock poisoned")
            .push_back(result);
    }

    pub fn calls(&self) -> Vec<ConnectionCall> {
        self.state
            .calls
            .lock()
            .expect("mock calls lock poisoned")
            .clone()
    }

    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::SeqCst)
    }
}

pub struct MockMcpConnection {
    state: Arc<MockConnectionState>,
    instructions: Option<String>,
    mode: ConnectionMode,
}

impl MockMcpConnection {
    pub fn new(mode: ConnectionMode) -> (Self, MockConnectionHandle) {
        let state = Arc::new(MockConnectionState::default());
        (
            Self {
                state: Arc::clone(&state),
                instructions: None,
                mode,
            },
            MockConnectionHandle { state },
        )
    }

    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }
}

impl McpConnection for MockMcpConnection {
    fn list_tools<'a>(
        &'a self,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        let result = self
            .state
            .list_results
            .lock()
            .expect("mock list lock poisoned")
            .pop_front()
            .unwrap_or_else(|| Err(ConnectionError::new("no scripted list_tools result")));
        self.state
            .calls
            .lock()
            .expect("mock calls lock poisoned")
            .push(ConnectionCall::ListTools);
        Box::pin(async move { result })
    }

    fn call_tool<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        name: &'a str,
        args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        let result = self
            .state
            .call_results
            .lock()
            .expect("mock call lock poisoned")
            .pop_front()
            .unwrap_or_else(|| Err(ConnectionError::new("no scripted call_tool result")));
        self.state
            .calls
            .lock()
            .expect("mock calls lock poisoned")
            .push(ConnectionCall::CallTool {
                name: name.to_owned(),
                args,
            });
        Box::pin(async move { result })
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    fn close<'a>(
        self: Box<Self>,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>> {
        self.state
            .calls
            .lock()
            .expect("mock calls lock poisoned")
            .push(ConnectionCall::Close);
        self.state.closed.store(true, Ordering::SeqCst);
        let result = self
            .state
            .close_results
            .lock()
            .expect("mock close lock poisoned")
            .pop_front()
            .unwrap_or(Ok(()));
        Box::pin(async move { result })
    }

    fn mode(&self) -> ConnectionMode {
        self.mode
    }
}

pub struct MockConnector {
    calls: Mutex<Vec<String>>,
    responses: Mutex<VecDeque<Result<Box<dyn McpConnection>, ConnectionError>>>,
}

impl MockConnector {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::new()),
        }
    }

    pub fn queue_connection(&self, connection: impl McpConnection + 'static) {
        self.responses
            .lock()
            .expect("mock connector lock poisoned")
            .push_back(Ok(Box::new(connection)));
    }

    pub fn queue_error(&self, error: ConnectionError) {
        self.responses
            .lock()
            .expect("mock connector lock poisoned")
            .push_back(Err(error));
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("mock connector calls lock poisoned")
            .clone()
    }
}

impl Default for MockConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl DirectConnector for MockConnector {
    fn connect<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        server: &'a ServerDefinition,
    ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, ConnectionError>> {
        self.calls
            .lock()
            .expect("mock connector calls lock poisoned")
            .push(server.name.clone());
        let result = self
            .responses
            .lock()
            .expect("mock connector lock poisoned")
            .pop_front()
            .unwrap_or_else(|| Err(ConnectionError::new("no scripted connector response")));
        Box::pin(async move { result })
    }
}

pub struct MockConnectionManager {
    calls: Mutex<Vec<String>>,
    responses: Mutex<VecDeque<Result<Box<dyn McpConnection>, CliError>>>,
}

impl MockConnectionManager {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::new()),
        }
    }

    pub fn queue_connection(&self, connection: impl McpConnection + 'static) {
        self.responses
            .lock()
            .expect("mock manager lock poisoned")
            .push_back(Ok(Box::new(connection)));
    }

    pub fn queue_error(&self, error: CliError) {
        self.responses
            .lock()
            .expect("mock manager lock poisoned")
            .push_back(Err(error));
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("mock manager calls lock poisoned")
            .clone()
    }
}

impl Default for MockConnectionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionManager for MockConnectionManager {
    fn acquire<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        server: &'a ServerDefinition,
    ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, CliError>> {
        self.calls
            .lock()
            .expect("mock manager calls lock poisoned")
            .push(server.name.clone());
        let result = self
            .responses
            .lock()
            .expect("mock manager lock poisoned")
            .pop_front()
            .unwrap_or_else(|| {
                Err(CliError::new(
                    mcp_cli::ErrorKind::NetworkError,
                    "no scripted manager response",
                    mcp_cli::ExitCode::Network,
                ))
            });
        Box::pin(async move { result })
    }
}
