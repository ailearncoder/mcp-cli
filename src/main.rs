#![forbid(unsafe_code)]

use std::{
    env,
    ffi::OsString,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
};

use mcp_cli::{
    CallInput, CancellationFlag, CliError, CliInvocation, CommandContext, CommandDispatcher,
    CommandOutcome, ConfigurationLoader, ConnectionResourceRegistry, DiagnosticSink, EnvSource,
    ErrorKind, FileConfigurationLoader, LoadRequest, LoadedConfig, ManagedConnectionManager,
    PlainTextPresenter, Presenter, ProcessEnv, RuntimeConfig, SecretSet, StreamStylePolicies,
    StylePolicy, SystemClock, TransportConfig, WriterDiagnosticSink, cli_command, parse_cli,
    render_structured_error_with_style,
};
use mcp_cli::{DirectConnector, connection::rmcp_adapter::RmcpDirectConnector};

const SUCCESS_EXIT: i32 = 0;
const SIGINT_EXIT: i32 = 130;
#[cfg(unix)]
const SIGTERM_EXIT: i32 = 143;

/// Result of the sole process boundary. A failed write is retained so tests can
/// assert that the boundary never retries by emitting a second diagnostic.
struct BoundaryOutcome {
    exit_code: i32,
    write_error: Option<io::Error>,
}

fn help_output() -> Vec<u8> {
    let mut command = cli_command();
    let mut output = command.render_long_help().to_string();
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.into_bytes()
}

fn version_output() -> Vec<u8> {
    let command = cli_command();
    let name = command.get_name();
    let version = command
        .get_version()
        .expect("clap command metadata must define a version");
    format!("{name} {version}\n").into_bytes()
}

fn write_success(writer: &mut (impl Write + ?Sized), bytes: &[u8]) -> Result<(), CliError> {
    writer.write_all(bytes).map_err(|source| {
        CliError::from_kind(ErrorKind::NetworkError, "Failed to write command output")
            .with_details("The destination output stream could not be written")
            .with_source(source)
    })
}

fn configured_secrets(loaded: &LoadedConfig) -> SecretSet {
    let mut secrets = loaded.secrets.clone();
    for server in loaded.servers.values() {
        match &server.transport {
            TransportConfig::Stdio { env, .. } => {
                for (name, value) in env {
                    secrets.register_env(name, value);
                }
            }
            TransportConfig::Http { headers, .. } => {
                for (name, value) in headers {
                    secrets.register_header(name, value);
                }
            }
        }
    }
    secrets
}

#[allow(clippy::too_many_arguments)]
async fn execute_business_command<R, W>(
    invocation: &CliInvocation,
    runtime: &RuntimeConfig,
    loader: &dyn ConfigurationLoader,
    environment: &dyn EnvSource,
    cwd: &Path,
    home: &Path,
    connector: Arc<dyn DirectConnector>,
    resources: ConnectionResourceRegistry,
    cancellation: Arc<CancellationFlag>,
    diagnostics: Arc<WriterDiagnosticSink<W>>,
    stdin: R,
    stdin_is_tty: bool,
) -> Result<CommandOutcome, CliError>
where
    R: Read,
    W: Write + Send + 'static,
{
    let request = LoadRequest::new(cwd, home, environment)
        .with_strict_env(runtime.strict_env)
        .with_diagnostics(diagnostics.as_ref());
    let request = match invocation.config_path.as_deref() {
        Some(path) => request.with_cli_path(path),
        None => request,
    };

    // This is the only configuration load in the command path.
    let loaded = loader.load(&request)?;
    diagnostics.register_secrets(&configured_secrets(&loaded));

    let context_diagnostics: Arc<dyn DiagnosticSink> = diagnostics;
    let context = CommandContext {
        deadline: runtime.deadline(&SystemClock),
        cancellation,
        diagnostics: context_diagnostics,
    };
    let manager: Arc<dyn mcp_cli::ConnectionManager> = Arc::new(ManagedConnectionManager::new(
        connector,
        resources.clone(),
        runtime,
    ));
    let dispatcher = CommandDispatcher::managed(manager, runtime);
    let mut input = CallInput::new(stdin, stdin_is_tty);
    let result = dispatcher
        .dispatch(&context, &loaded.servers, &invocation.command, &mut input)
        .await;

    // Handlers and direct wrappers explicitly close on every route. The
    // registry remains a last-resort observable guard, not a second owner that
    // could mask the primary outcome with a cleanup error.
    if resources.active_resource_count() != 0 {
        context
            .diagnostics
            .debug("command returned while direct resource cleanup was still in progress");
    }
    result
}

fn process_home(cwd: &Path) -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.to_path_buf())
}

async fn run_application<I, R, O, E>(
    args: I,
    stdin: R,
    stdin_is_tty: bool,
    stdout: &mut O,
    stdout_style: StylePolicy,
    diagnostics: Arc<WriterDiagnosticSink<E>>,
    cancellation: Arc<CancellationFlag>,
) -> Result<(), CliError>
where
    I: IntoIterator<Item = OsString>,
    R: Read,
    O: Write + ?Sized,
    E: Write + Send + 'static,
{
    let invocation = parse_cli(args)?;

    // Help and version deliberately precede runtime env parsing, cwd/home
    // discovery, configuration loading, connector construction, and signals.
    match invocation.command {
        mcp_cli::CommandSpec::Help => return write_success(stdout, &help_output()),
        mcp_cli::CommandSpec::Version => return write_success(stdout, &version_output()),
        _ => {}
    }

    let runtime = RuntimeConfig::from_current_env()?;
    let cwd = env::current_dir().map_err(|source| {
        CliError::from_kind(
            ErrorKind::ConfigReadError,
            "Could not determine current directory",
        )
        .with_details("The current working directory is unavailable")
        .with_source(source)
    })?;
    let home = process_home(&cwd);
    let resources = ConnectionResourceRegistry::new();
    let connector: Arc<dyn DirectConnector> = Arc::new(RmcpDirectConnector);
    let outcome = execute_business_command(
        &invocation,
        &runtime,
        &FileConfigurationLoader::default(),
        &ProcessEnv,
        &cwd,
        &home,
        connector,
        resources,
        cancellation,
        Arc::clone(&diagnostics),
        stdin,
        stdin_is_tty,
    )
    .await
    .map_err(|error| diagnostics.redact_error(error))?;

    let bytes = PlainTextPresenter.render(outcome, stdout_style)?;
    write_success(stdout, &bytes)
}

fn finish_at_top_level_with_style(
    result: Result<(), CliError>,
    stderr: &mut (impl Write + ?Sized),
    style: StylePolicy,
) -> BoundaryOutcome {
    match result {
        Ok(()) => BoundaryOutcome {
            exit_code: SUCCESS_EXIT,
            write_error: None,
        },
        Err(error) => {
            let exit_code = i32::from(error.canonical_exit_code().as_u8());
            let write_error = render_structured_error_with_style(stderr, &error, style).err();
            BoundaryOutcome {
                exit_code,
                write_error,
            }
        }
    }
}

fn spawn_signal_coordinator(
    cancellation: Arc<CancellationFlag>,
    signal_exit: Arc<AtomicI32>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let code = wait_for_shutdown_signal().await;
        if signal_exit
            .compare_exchange(0, code, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            cancellation.cancel();
        }
    })
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() -> i32 {
    use tokio::signal::unix::{SignalKind, signal};

    let mut interrupt = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = interrupt.recv() => SIGINT_EXIT,
        _ = terminate.recv() => SIGTERM_EXIT,
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() -> i32 {
    let _ = tokio::signal::ctrl_c().await;
    SIGINT_EXIT
}

#[cfg(unix)]
fn is_hidden_daemon_invocation(args: &[OsString]) -> bool {
    args.first().is_some_and(|argument| argument == "__daemon")
}

#[cfg(unix)]
async fn run_hidden_daemon_entry(args: &[OsString]) -> Option<i32> {
    if !is_hidden_daemon_invocation(args) {
        return None;
    }

    let result = if args.len() == 1 {
        let stdin = io::stdin();
        let stdout = io::stdout();
        mcp_cli::daemon::worker::run_worker(&mut stdin.lock(), &mut stdout.lock()).await
    } else {
        Err(mcp_cli::daemon::worker::WorkerBootstrapError::InvalidInput)
    };
    match result {
        Ok(_) => Some(SUCCESS_EXIT),
        Err(error) => {
            // The bootstrap error vocabulary is payload-free by construction;
            // never render serde, transport, path, or configuration sources.
            let mut stderr = io::stderr().lock();
            let _ = writeln!(stderr, "mcp-cli daemon worker failed: {error}");
            Some(1)
        }
    }
}

#[tokio::main]
async fn main() {
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    #[cfg(unix)]
    if let Some(exit_code) = run_hidden_daemon_entry(&args).await {
        process::exit(exit_code);
    }

    let stdout = io::stdout();
    let stderr = io::stderr();
    let stdin = io::stdin();
    let styles = StreamStylePolicies::new(
        stdout.is_terminal(),
        stderr.is_terminal(),
        env::var_os("NO_COLOR").is_some(),
    );
    let stdin_is_tty = stdin.is_terminal();
    let cancellation = Arc::new(CancellationFlag::default());
    let signal_exit = Arc::new(AtomicI32::new(0));
    let signal_task = spawn_signal_coordinator(Arc::clone(&cancellation), Arc::clone(&signal_exit));
    let diagnostics = Arc::new(WriterDiagnosticSink::new_styled(
        io::stderr(),
        env::var_os("MCP_DEBUG").is_some_and(|value| !value.is_empty()),
        SecretSet::new(),
        styles.stderr,
    ));

    let mut stdout = stdout.lock();
    let result = run_application(
        args,
        stdin.lock(),
        stdin_is_tty,
        &mut stdout,
        styles.stdout,
        diagnostics,
        cancellation,
    )
    .await;
    signal_task.abort();
    let _ = signal_task.await;

    let signalled = signal_exit.load(Ordering::SeqCst);
    let outcome = if signalled != 0 {
        BoundaryOutcome {
            exit_code: signalled,
            write_error: None,
        }
    } else {
        let mut stderr = stderr.lock();
        finish_at_top_level_with_style(result, &mut stderr, styles.stderr)
    };

    // Never attempt a second diagnostic when either destination is closed.
    let _write_error = outcome.write_error;
    process::exit(outcome.exit_code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        io::Cursor,
        sync::{Mutex, atomic::AtomicUsize},
        time::Duration,
    };

    use mcp_cli::{
        BoxFuture, ConfigHash, ConnectionError, ConnectionMode, McpConnection, ServerDefinition,
        ServerId, ToolFilterConfig, ToolInfo, ToolResult,
    };
    use serde_json::{Value, json};

    #[derive(Default)]
    struct EmptyEnv;

    impl EnvSource for EmptyEnv {
        fn var_os(&self, _name: &str) -> Option<OsString> {
            None
        }
    }

    struct CountingLoader {
        loads: AtomicUsize,
        loaded: LoadedConfig,
    }

    impl ConfigurationLoader for CountingLoader {
        fn discover(&self, _request: &LoadRequest<'_>) -> Result<PathBuf, CliError> {
            Ok(self.loaded.source.clone())
        }

        fn load(&self, _request: &LoadRequest<'_>) -> Result<LoadedConfig, CliError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Ok(self.loaded.clone())
        }
    }

    #[derive(Default)]
    struct ContextTrace {
        connects: AtomicUsize,
        lists: AtomicUsize,
        calls: AtomicUsize,
        closes: AtomicUsize,
        mismatch: std::sync::atomic::AtomicBool,
        expected: Mutex<Option<CommandContext>>,
    }

    struct RecordingConnector(Arc<ContextTrace>);

    impl DirectConnector for RecordingConnector {
        fn connect<'a>(
            &'a self,
            ctx: &'a CommandContext,
            _server: &'a ServerDefinition,
        ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, ConnectionError>> {
            self.0.connects.fetch_add(1, Ordering::SeqCst);
            *self.0.expected.lock().expect("context lock") = Some(ctx.clone());
            let connection = RecordingConnection(Arc::clone(&self.0));
            Box::pin(async move { Ok(Box::new(connection) as Box<dyn McpConnection>) })
        }
    }

    struct RecordingConnection(Arc<ContextTrace>);

    impl RecordingConnection {
        fn observe(&self, ctx: &CommandContext) {
            let expected = self.0.expected.lock().expect("context lock");
            let expected = expected.as_ref().expect("connector context");
            if expected.deadline != ctx.deadline
                || !Arc::ptr_eq(&expected.cancellation, &ctx.cancellation)
                || !Arc::ptr_eq(&expected.diagnostics, &ctx.diagnostics)
            {
                self.0.mismatch.store(true, Ordering::SeqCst);
            }
        }
    }

    impl McpConnection for RecordingConnection {
        fn list_tools<'a>(
            &'a self,
            ctx: &'a CommandContext,
        ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
            self.observe(ctx);
            self.0.lists.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(vec![ToolInfo {
                    name: "echo".to_owned(),
                    description: None,
                    input_schema: json!({"type": "object"}),
                }])
            })
        }

        fn call_tool<'a>(
            &'a self,
            ctx: &'a CommandContext,
            _name: &'a str,
            args: mcp_cli::JsonObject,
        ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
            self.observe(ctx);
            self.0.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(json!({"arguments": args})) })
        }

        fn instructions(&self) -> Option<&str> {
            None
        }

        fn close<'a>(
            self: Box<Self>,
            ctx: &'a CommandContext,
        ) -> BoxFuture<'a, Result<(), ConnectionError>> {
            self.observe(ctx);
            self.0.closes.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }

        fn mode(&self) -> ConnectionMode {
            ConnectionMode::Direct
        }
    }

    fn loaded_config() -> LoadedConfig {
        let server = ServerDefinition {
            name: "fixture".to_owned(),
            id: ServerId("a".repeat(64)),
            config_hash: ConfigHash([0; 32]),
            transport: TransportConfig::Stdio {
                command: "fixture".to_owned(),
                args: Vec::new(),
                env: BTreeMap::from([("TOKEN".to_owned(), "secret-value".to_owned())]),
                cwd: None,
            },
            filter: ToolFilterConfig::default(),
        };
        LoadedConfig {
            source: PathBuf::from("/fixture/config.json"),
            document: json!({"mcpServers": {}}),
            servers: BTreeMap::from([("fixture".to_owned(), server)]),
            missing_env: Default::default(),
            secrets: SecretSet::new(),
        }
    }

    #[tokio::test]
    async fn business_path_loads_once_shares_context_and_releases_resources() {
        let loader = CountingLoader {
            loads: AtomicUsize::new(0),
            loaded: loaded_config(),
        };
        let invocation = CliInvocation {
            command: mcp_cli::CommandSpec::Call {
                server: "fixture".to_owned(),
                tool: "echo".to_owned(),
                inline_json: Some("{\"value\":7}".to_owned()),
            },
            config_path: Some(PathBuf::from("/fixture/config.json")),
        };
        let trace = Arc::new(ContextTrace::default());
        let connector: Arc<dyn DirectConnector> = Arc::new(RecordingConnector(Arc::clone(&trace)));
        let resources = ConnectionResourceRegistry::new();
        let diagnostics = Arc::new(WriterDiagnosticSink::new(
            Vec::<u8>::new(),
            true,
            SecretSet::new(),
        ));

        let outcome = execute_business_command(
            &invocation,
            &RuntimeConfig {
                timeout: Duration::from_secs(30),
                max_retries: 0,
                ..RuntimeConfig::default()
            },
            &loader,
            &EmptyEnv,
            Path::new("/fixture"),
            Path::new("/home/fixture"),
            connector,
            resources.clone(),
            Arc::new(CancellationFlag::default()),
            diagnostics,
            Cursor::new(Vec::<u8>::new()),
            false,
        )
        .await
        .expect("call succeeds");

        assert_eq!(
            outcome,
            CommandOutcome::Json(json!({"arguments": {"value": 7}}))
        );
        assert_eq!(loader.loads.load(Ordering::SeqCst), 1);
        assert_eq!(trace.connects.load(Ordering::SeqCst), 1);
        assert_eq!(trace.lists.load(Ordering::SeqCst), 1);
        assert_eq!(trace.calls.load(Ordering::SeqCst), 1);
        assert_eq!(trace.closes.load(Ordering::SeqCst), 1);
        assert!(!trace.mismatch.load(Ordering::SeqCst));
        assert_eq!(resources.active_resource_count(), 0);
    }

    #[test]
    fn top_level_renders_once_and_preserves_canonical_exit_codes() {
        for (kind, expected) in [
            (ErrorKind::InvalidArguments, 1),
            (ErrorKind::ToolExecutionFailed, 2),
            (ErrorKind::NetworkError, 3),
            (ErrorKind::AuthError, 4),
        ] {
            let mut stderr = Vec::new();
            let outcome = finish_at_top_level_with_style(
                Err(CliError::from_kind(kind, "failed")),
                &mut stderr,
                StylePolicy::plain(),
            );
            assert_eq!(outcome.exit_code, expected);
            assert!(outcome.write_error.is_none());
            assert_eq!(
                String::from_utf8(stderr)
                    .unwrap()
                    .matches("Error [")
                    .count(),
                1
            );
        }
    }

    #[test]
    fn configured_env_and_headers_are_registered_for_boundary_redaction() {
        let loaded = loaded_config();
        let secrets = configured_secrets(&loaded);
        assert_eq!(secrets.redact("token=secret-value"), "token=[REDACTED]");

        let sink = WriterDiagnosticSink::new(Vec::<u8>::new(), true, SecretSet::new());
        sink.register_secrets(&secrets);
        let error = sink.redact_error(CliError::network_error(
            "fixture",
            "secret-value must not leak",
        ));
        assert!(!format!("{error:?}").contains("secret-value"));
    }

    #[test]
    fn json_presenter_path_keeps_call_stdout_machine_readable() {
        let bytes = PlainTextPresenter
            .render(
                CommandOutcome::Json(json!({"ok": true})),
                StylePolicy::plain(),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"ok": true})
        );
        assert_eq!(bytes, b"{\"ok\":true}\n");
    }

    #[cfg(unix)]
    #[test]
    fn hidden_daemon_dispatch_is_exact_and_does_not_change_public_help() {
        assert!(is_hidden_daemon_invocation(&[OsString::from("__daemon")]));
        assert!(is_hidden_daemon_invocation(&[
            OsString::from("__daemon"),
            OsString::from("unexpected")
        ]));
        assert!(!is_hidden_daemon_invocation(&[OsString::from("info")]));
        assert!(
            !String::from_utf8(help_output())
                .unwrap()
                .contains("__daemon")
        );
    }
}
