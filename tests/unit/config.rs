use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use mcp_cli::{
    config::{
        ConfigurationLoader, EnvSource, FileConfigurationLoader, LoadRequest, LoadedConfig,
        TransportConfig,
    },
    error::{CliError, ErrorKind},
    output::DiagnosticSink,
};
use serde_json::{Value, json};
use tempfile::TempDir;

const CONFIG_FILE: &str = "mcp_servers.json";

#[derive(Default)]
struct MapEnv(BTreeMap<String, OsString>);

impl MapEnv {
    fn from_pairs<const N: usize>(pairs: [(&str, &str); N]) -> Self {
        Self(
            pairs
                .into_iter()
                .map(|(name, value)| (name.to_owned(), OsString::from(value)))
                .collect(),
        )
    }

    fn with_path(path: &Path) -> Self {
        Self(BTreeMap::from([(
            "MCP_CONFIG_PATH".to_owned(),
            path.as_os_str().to_owned(),
        )]))
    }
}

impl EnvSource for MapEnv {
    fn var_os(&self, name: &str) -> Option<OsString> {
        self.0.get(name).cloned()
    }
}

#[derive(Default)]
struct RecordingDiagnostics {
    warnings: Mutex<Vec<String>>,
}

impl RecordingDiagnostics {
    fn warnings(&self) -> Vec<String> {
        self.warnings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl DiagnosticSink for RecordingDiagnostics {
    fn warning(&self, message: &str) {
        self.warnings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(message.to_owned());
    }

    fn debug(&self, _message: &str) {}

    fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
}

struct Fixture {
    _root: TempDir,
    cwd: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("create isolated config fixture");
        let cwd = root.path().join("cwd");
        let home = root.path().join("home");
        fs::create_dir_all(&cwd).expect("create isolated cwd");
        fs::create_dir_all(home.join(".config/mcp")).expect("create isolated config home");
        Self {
            _root: root,
            cwd,
            home,
        }
    }

    fn cwd_config(&self) -> PathBuf {
        self.cwd.join(CONFIG_FILE)
    }

    fn home_config(&self) -> PathBuf {
        self.home.join(".mcp_servers.json")
    }

    fn xdg_config(&self) -> PathBuf {
        self.home.join(".config/mcp").join(CONFIG_FILE)
    }

    fn path(&self, name: &str) -> PathBuf {
        self._root.path().join(name)
    }

    fn write_value(&self, path: &Path, value: &Value) {
        fs::write(path, serde_json::to_vec(value).expect("serialize fixture"))
            .expect("write fixture config");
    }

    fn write_server(&self, path: &Path, server: &str) {
        self.write_value(
            path,
            &json!({"mcpServers": {server: {"command": "runner"}}}),
        );
    }

    fn load(
        &self,
        cli_path: Option<&Path>,
        env_path: Option<&OsStr>,
        env: &dyn EnvSource,
        diagnostics: &dyn DiagnosticSink,
        strict: bool,
    ) -> Result<LoadedConfig, CliError> {
        let mut request = LoadRequest::new(&self.cwd, &self.home, env)
            .with_strict_env(strict)
            .with_diagnostics(diagnostics);
        if let Some(path) = cli_path {
            request = request.with_cli_path(path);
        }
        if let Some(path) = env_path {
            request = request.with_env_path(path);
        }
        FileConfigurationLoader::default().load(&request)
    }
}

fn assert_error_field(error: CliError, kind: ErrorKind, fragments: &[&str]) {
    assert_eq!(error.kind, kind, "unexpected error: {error:?}");
    let details = error
        .details
        .expect("error must identify the invalid field");
    for fragment in fragments {
        assert!(
            details.contains(fragment),
            "expected {fragment:?} in {details:?}"
        );
    }
}

#[test]
fn loads_an_explicit_config_and_cli_path_wins_over_environment_and_defaults() {
    let fixture = Fixture::new();
    let cli = fixture.path("cli.json");
    let env_path = fixture.path("env.json");
    fixture.write_server(&cli, "cli");
    fixture.write_server(&env_path, "env");
    fixture.write_server(&fixture.cwd_config(), "default");
    let env = MapEnv::with_path(&env_path);
    let diagnostics = RecordingDiagnostics::default();

    let loaded = fixture
        .load(Some(&cli), None, &env, &diagnostics, true)
        .expect("explicit CLI config loads");

    assert_eq!(loaded.source, cli);
    assert_eq!(loaded.servers.keys().collect::<Vec<_>>(), vec!["cli"]);
    assert!(diagnostics.warnings().is_empty());
}

#[test]
fn explicit_environment_path_is_used_when_cli_path_is_absent() {
    let fixture = Fixture::new();
    let env_path = fixture.path("env.json");
    fixture.write_server(&env_path, "environment");
    fixture.write_server(&fixture.cwd_config(), "default");
    let env = MapEnv::with_path(&env_path);
    let diagnostics = RecordingDiagnostics::default();

    let loaded = fixture
        .load(None, None, &env, &diagnostics, true)
        .expect("MCP_CONFIG_PATH config loads");

    assert_eq!(loaded.source, env_path);
    assert!(loaded.servers.contains_key("environment"));
}

#[test]
fn missing_or_unreadable_explicit_paths_never_fall_back_to_defaults() {
    let fixture = Fixture::new();
    fixture.write_server(&fixture.cwd_config(), "must-not-load");
    let missing = fixture.path("missing.json");
    let unreadable = fixture.path("not-a-regular-file");
    fs::create_dir(&unreadable).expect("create explicit directory path");
    let env = MapEnv::default();
    let diagnostics = RecordingDiagnostics::default();

    let missing_error = fixture
        .load(Some(&missing), None, &env, &diagnostics, true)
        .expect_err("missing explicit path must not fall back");
    assert_eq!(missing_error.kind, ErrorKind::ConfigNotFound);
    assert!(
        missing_error
            .message
            .contains(&missing.display().to_string())
    );

    let unreadable_error = fixture
        .load(Some(&unreadable), None, &env, &diagnostics, true)
        .expect_err("non-file explicit path must not fall back");
    assert_eq!(unreadable_error.kind, ErrorKind::ConfigReadError);
    assert!(
        unreadable_error
            .message
            .contains(&unreadable.display().to_string())
    );
}

#[test]
fn searches_every_default_path_in_the_required_order() {
    let fixture = Fixture::new();
    let env = MapEnv::default();
    let diagnostics = RecordingDiagnostics::default();
    let cwd = fixture.cwd_config();
    let home = fixture.home_config();
    let xdg = fixture.xdg_config();
    fixture.write_server(&cwd, "cwd");
    fixture.write_server(&home, "home");
    fixture.write_server(&xdg, "xdg");

    assert_eq!(
        fixture
            .load(None, None, &env, &diagnostics, true)
            .expect("cwd default")
            .source,
        cwd
    );
    fs::remove_file(&cwd).expect("remove first default");
    assert_eq!(
        fixture
            .load(None, None, &env, &diagnostics, true)
            .expect("home default")
            .source,
        home
    );
    fs::remove_file(&home).expect("remove second default");
    assert_eq!(
        fixture
            .load(None, None, &env, &diagnostics, true)
            .expect("XDG default")
            .source,
        xdg
    );
}

#[test]
fn missing_defaults_report_all_absolute_search_paths() {
    let fixture = Fixture::new();
    let env = MapEnv::default();
    let diagnostics = RecordingDiagnostics::default();

    let error = fixture
        .load(None, None, &env, &diagnostics, true)
        .expect_err("all default paths are absent");

    assert_eq!(error.kind, ErrorKind::ConfigNotFound);
    let details = error.details.expect("searched paths");
    for path in [
        fixture.cwd_config(),
        fixture.home_config(),
        fixture.xdg_config(),
    ] {
        assert!(path.is_absolute());
        assert!(details.contains(&path.display().to_string()), "{details}");
    }
}

#[test]
fn invalid_json_reports_source_line_and_column() {
    let fixture = Fixture::new();
    let config = fixture.path("invalid.json");
    fs::write(
        &config,
        "{\n  \"mcpServers\": {\n    \"broken\": ]\n  }\n}\n",
    )
    .expect("write malformed JSON");
    let env = MapEnv::default();
    let diagnostics = RecordingDiagnostics::default();

    let error = fixture
        .load(Some(&config), None, &env, &diagnostics, true)
        .expect_err("malformed JSON is rejected");

    assert_eq!(error.kind, ErrorKind::InvalidConfig);
    let details = error.details.expect("JSON location details");
    assert!(details.contains(&config.display().to_string()), "{details}");
    assert!(details.contains("line: 3"), "{details}");
    assert!(details.contains("column:"), "{details}");
}

#[test]
fn mcp_servers_must_be_present_and_an_object() {
    let fixture = Fixture::new();
    let config = fixture.path("structure.json");
    let env = MapEnv::default();
    let diagnostics = RecordingDiagnostics::default();

    for document in [
        json!({}),
        json!({"mcpServers": null}),
        json!({"mcpServers": []}),
    ] {
        fixture.write_value(&config, &document);
        let error = fixture
            .load(Some(&config), None, &env, &diagnostics, true)
            .expect_err("invalid mcpServers shape is rejected");
        assert_error_field(error, ErrorKind::InvalidConfig, &["mcpServers", "object"]);
    }
}

#[test]
fn strict_and_non_strict_environment_substitution_are_isolated_and_diagnostic() {
    let fixture = Fixture::new();
    let config = fixture.path("environment.json");
    fixture.write_value(
        &config,
        &json!({
            "mcpServers": {
                "remote": {
                    "url": "https://example.test/${PATH_PART}",
                    "headers": {
                        "Authorization": "Bearer ${TOKEN}",
                        "X-Missing": "${MISSING}",
                        "X-Again": "prefix-${MISSING}"
                    }
                }
            }
        }),
    );
    let env = MapEnv::from_pairs([("PATH_PART", "mcp"), ("TOKEN", "secret-token")]);

    let strict_diagnostics = RecordingDiagnostics::default();
    let strict_error = fixture
        .load(Some(&config), None, &env, &strict_diagnostics, true)
        .expect_err("strict mode rejects missing variables");
    assert_eq!(strict_error.kind, ErrorKind::MissingEnvVar);
    assert_eq!(
        strict_error.details.as_deref(),
        Some("Missing variables: MISSING")
    );
    assert!(strict_diagnostics.warnings().is_empty());
    assert!(!format!("{strict_error:?}").contains("secret-token"));

    let non_strict_diagnostics = RecordingDiagnostics::default();
    let loaded = fixture
        .load(Some(&config), None, &env, &non_strict_diagnostics, false)
        .expect("non-strict mode substitutes empty strings");
    match &loaded.servers["remote"].transport {
        TransportConfig::Http { url, headers } => {
            assert_eq!(url.as_str(), "https://example.test/mcp");
            assert_eq!(headers["Authorization"], "Bearer secret-token");
            assert_eq!(headers["X-Missing"], "");
            assert_eq!(headers["X-Again"], "prefix-");
        }
        transport => panic!("expected HTTP transport, got {transport:?}"),
    }
    assert_eq!(
        non_strict_diagnostics.warnings(),
        vec!["Environment variable MISSING is not set; substituting an empty string"]
    );
    assert_eq!(
        loaded.missing_env.into_iter().collect::<Vec<_>>(),
        vec!["MISSING"]
    );
    assert_eq!(
        loaded.secrets.redact("Bearer secret-token"),
        "Bearer [REDACTED]"
    );
}

#[test]
fn server_null_and_non_object_values_identify_the_server() {
    let fixture = Fixture::new();
    let config = fixture.path("server-shape.json");
    let env = MapEnv::default();
    let diagnostics = RecordingDiagnostics::default();

    for value in [Value::Null, json!([])] {
        fixture.write_value(&config, &json!({"mcpServers": {"bad-server": value}}));
        let error = fixture
            .load(Some(&config), None, &env, &diagnostics, true)
            .expect_err("server must be a non-null object");
        assert_eq!(error.kind, ErrorKind::InvalidServerConfig);
        assert!(error.message.contains("bad-server"));
        assert_error_field(error, ErrorKind::InvalidServerConfig, &["bad-server"]);
    }
}

#[test]
fn command_and_url_are_mutually_exclusive_and_one_is_required() {
    let fixture = Fixture::new();
    let config = fixture.path("transport-shape.json");
    let env = MapEnv::default();
    let diagnostics = RecordingDiagnostics::default();

    for (server, value) in [
        (
            "both",
            json!({"command": "runner", "url": "https://example.test/mcp"}),
        ),
        ("neither", json!({"args": []})),
    ] {
        fixture.write_value(&config, &json!({"mcpServers": {server: value}}));
        let error = fixture
            .load(Some(&config), None, &env, &diagnostics, true)
            .expect_err("transport discriminator is invalid");
        assert_error_field(
            error,
            ErrorKind::InvalidServerConfig,
            &[server, "mcpServers"],
        );
    }
}

#[test]
fn empty_or_non_string_command_and_invalid_url_are_rejected() {
    let fixture = Fixture::new();
    let config = fixture.path("transport-value.json");
    let env = MapEnv::default();
    let diagnostics = RecordingDiagnostics::default();

    for command in [json!(""), Value::Null, json!(7)] {
        fixture.write_value(
            &config,
            &json!({"mcpServers": {"stdio": {"command": command}}}),
        );
        let error = fixture
            .load(Some(&config), None, &env, &diagnostics, true)
            .expect_err("invalid command is rejected");
        assert_error_field(error, ErrorKind::InvalidServerConfig, &[".command"]);
    }

    for url in [
        json!("ftp://example.test/mcp"),
        json!("relative/path"),
        json!(9),
    ] {
        fixture.write_value(&config, &json!({"mcpServers": {"http": {"url": url}}}));
        let error = fixture
            .load(Some(&config), None, &env, &diagnostics, true)
            .expect_err("invalid URL is rejected");
        assert_error_field(error, ErrorKind::InvalidServerConfig, &[".url"]);
    }
}

#[test]
fn args_env_headers_and_filter_fields_reject_wrong_container_types() {
    let fixture = Fixture::new();
    let config = fixture.path("field-containers.json");
    let env = MapEnv::default();
    let diagnostics = RecordingDiagnostics::default();

    for (field, invalid) in [
        ("args", json!({})),
        ("env", json!([])),
        ("headers", json!([])),
        ("allowedTools", json!({})),
        ("disabledTools", json!(false)),
    ] {
        let mut server = json!({"command": "runner"});
        server
            .as_object_mut()
            .expect("server object")
            .insert(field.to_owned(), invalid);
        fixture.write_value(&config, &json!({"mcpServers": {"broken": server}}));
        let error = fixture
            .load(Some(&config), None, &env, &diagnostics, true)
            .expect_err("known field container type is validated");
        assert_error_field(
            error,
            ErrorKind::InvalidServerConfig,
            &[&format!(".{field}")],
        );
    }
}

#[test]
fn args_env_headers_and_filter_fields_reject_wrong_member_types() {
    let fixture = Fixture::new();
    let config = fixture.path("field-members.json");
    let env = MapEnv::default();
    let diagnostics = RecordingDiagnostics::default();

    for (field, invalid, member) in [
        ("args", json!(["ok", 7]), "[1]"),
        ("env", json!({"TOKEN": 7}), "TOKEN"),
        ("headers", json!({"Authorization": null}), "Authorization"),
        ("allowedTools", json!(["read_*", false]), "[1]"),
        ("disabledTools", json!([null]), "[0]"),
    ] {
        let mut server = json!({"command": "runner"});
        server
            .as_object_mut()
            .expect("server object")
            .insert(field.to_owned(), invalid);
        fixture.write_value(&config, &json!({"mcpServers": {"broken": server}}));
        let error = fixture
            .load(Some(&config), None, &env, &diagnostics, true)
            .expect_err("known field member type is validated");
        assert_error_field(
            error,
            ErrorKind::InvalidServerConfig,
            &[&format!(".{field}"), member],
        );
    }
}

#[test]
fn claude_vscode_and_gemini_fields_and_extensions_are_compatible() {
    let fixture = Fixture::new();
    let config = fixture.path("client-compatible.json");
    let document = json!({
        "inputs": [{"id": "api-key", "type": "promptString", "password": true}],
        "mcpServers": {
            "claude-desktop": {
                "command": "npx",
                "args": ["-y", "@example/server"],
                "env": {"MODE": "claude"},
                "cwd": "/workspace",
                "disabledTools": ["danger_*"],
                "autoApprove": ["read_file"],
                "description": "Claude extension field"
            },
            "vscode": {
                "type": "stdio",
                "command": "node",
                "args": ["server.js"],
                "envFile": "${workspaceFolder}/.env",
                "gallery": {"version": 1}
            },
            "gemini": {
                "url": "https://example.test/mcp",
                "headers": {"Authorization": "Bearer token"},
                "allowedTools": ["read_*", "search"],
                "disabledTools": ["write_*"],
                "timeout": 30000,
                "trust": true,
                "httpUrl": "ignored compatibility extension"
            }
        }
    });
    fixture.write_value(&config, &document);
    let env = MapEnv::from_pairs([("workspaceFolder", "/workspace")]);
    let diagnostics = RecordingDiagnostics::default();

    let loaded = fixture
        .load(Some(&config), None, &env, &diagnostics, true)
        .expect("common client fields and unknown extensions are accepted");

    assert_eq!(
        loaded.servers.keys().collect::<Vec<_>>(),
        vec!["claude-desktop", "gemini", "vscode"]
    );
    assert!(matches!(
        loaded.servers["claude-desktop"].transport,
        TransportConfig::Stdio { .. }
    ));
    assert!(matches!(
        loaded.servers["vscode"].transport,
        TransportConfig::Stdio { .. }
    ));
    assert!(matches!(
        loaded.servers["gemini"].transport,
        TransportConfig::Http { .. }
    ));
    assert_eq!(
        loaded.servers["claude-desktop"].filter.disabled_tools,
        vec!["danger_*"]
    );
    assert_eq!(
        loaded.servers["gemini"].filter.allowed_tools,
        vec!["read_*", "search"]
    );
    let mut expected_document = document;
    expected_document["mcpServers"]["vscode"]["envFile"] = json!("/workspace/.env");
    assert_eq!(
        loaded.document, expected_document,
        "extension fields must survive environment substitution"
    );
}
