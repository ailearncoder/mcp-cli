//! Deterministic configuration discovery and bounded JSON loading.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::{error::CliError, output::DiagnosticSink, policy::redact::SecretSet};

use super::{substitute::substitute_environment, validate::validate_mcp_servers};

/// Maximum accepted configuration size (16 MiB).
///
/// Reads are capped at one byte beyond this value so a file cannot cause an
/// unbounded allocation even if its metadata is absent or changes while read.
pub const MAX_CONFIG_BYTES: usize = 16 * 1024 * 1024;
const CONFIG_FILE_NAME: &str = "mcp_servers.json";
const CONFIG_PATH_ENV: &str = "MCP_CONFIG_PATH";

/// Injectable environment lookup used by configuration loading.
///
/// Returning `OsString` preserves non-UTF-8 paths on platforms that support
/// them. Tests can provide a map-backed implementation without changing the
/// process environment.
pub trait EnvSource: Send + Sync {
    fn var_os(&self, name: &str) -> Option<OsString>;
}

/// Environment source for normal process execution.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn var_os(&self, name: &str) -> Option<OsString> {
        std::env::var_os(name)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct NullDiagnostics;

impl DiagnosticSink for NullDiagnostics {
    fn warning(&self, _message: &str) {}
    fn debug(&self, _message: &str) {}
    fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
}

static NULL_DIAGNOSTICS: NullDiagnostics = NullDiagnostics;

/// Inputs whose ambient values must be supplied by the caller.
pub struct LoadRequest<'a> {
    pub cli_path: Option<&'a Path>,
    /// Optional already-resolved value of `MCP_CONFIG_PATH`.
    ///
    /// When absent, `env` is queried. This supports callers that snapshot the
    /// environment once while retaining a fully injectable test API.
    pub env_path: Option<&'a OsStr>,
    pub cwd: &'a Path,
    pub home: &'a Path,
    pub env: &'a dyn EnvSource,
    pub strict_env: bool,
    pub diagnostics: &'a dyn DiagnosticSink,
}

impl<'a> LoadRequest<'a> {
    pub fn new(cwd: &'a Path, home: &'a Path, env: &'a dyn EnvSource) -> Self {
        Self {
            cli_path: None,
            env_path: None,
            cwd,
            home,
            env,
            strict_env: true,
            diagnostics: &NULL_DIAGNOSTICS,
        }
    }

    pub fn with_cli_path(mut self, path: &'a Path) -> Self {
        self.cli_path = Some(path);
        self
    }

    pub fn with_env_path(mut self, path: &'a OsStr) -> Self {
        self.env_path = Some(path);
        self
    }

    pub fn with_strict_env(mut self, strict_env: bool) -> Self {
        self.strict_env = strict_env;
        self
    }

    pub fn with_diagnostics(mut self, diagnostics: &'a dyn DiagnosticSink) -> Self {
        self.diagnostics = diagnostics;
        self
    }
}

/// Parsed and validated configuration after environment substitution.
/// `BTreeMap` provides deterministic server-name order.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedConfig {
    pub source: PathBuf,
    pub document: Value,
    pub servers: BTreeMap<String, super::ServerDefinition>,
    pub missing_env: BTreeSet<String>,
    pub secrets: SecretSet,
}

/// Configuration discovery/loading interface used by command code and tests.
pub trait ConfigurationLoader {
    fn discover(&self, request: &LoadRequest<'_>) -> Result<PathBuf, CliError>;
    fn load(&self, request: &LoadRequest<'_>) -> Result<LoadedConfig, CliError>;
}

/// Filesystem-backed configuration loader.
#[derive(Clone, Copy, Debug)]
pub struct FileConfigurationLoader {
    max_bytes: usize,
}

impl Default for FileConfigurationLoader {
    fn default() -> Self {
        Self {
            max_bytes: MAX_CONFIG_BYTES,
        }
    }
}

impl FileConfigurationLoader {
    /// Builds a loader with a smaller limit, useful for focused tests and
    /// embedding. Values above the project hard limit are clamped.
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self {
            max_bytes: max_bytes.min(MAX_CONFIG_BYTES),
        }
    }

    fn explicit_path(&self, request: &LoadRequest<'_>) -> Option<PathBuf> {
        if let Some(path) = request.cli_path {
            return Some(resolve_from_cwd(path, request.cwd));
        }

        request
            .env_path
            .map(PathBuf::from)
            .or_else(|| request.env.var_os(CONFIG_PATH_ENV).map(PathBuf::from))
            .map(|path| resolve_from_cwd(&path, request.cwd))
    }

    fn default_paths(&self, request: &LoadRequest<'_>) -> [PathBuf; 3] {
        let cwd = resolve_from_cwd(request.cwd, request.cwd);
        let home = resolve_from_cwd(request.home, request.cwd);
        [
            cwd.join(CONFIG_FILE_NAME),
            home.join(".mcp_servers.json"),
            home.join(".config").join("mcp").join(CONFIG_FILE_NAME),
        ]
    }

    fn path_exists(path: &Path) -> Result<bool, CliError> {
        path.try_exists()
            .map_err(|error| CliError::config_read_error(path, error))
    }

    fn read_bounded(&self, path: &Path) -> Result<Vec<u8>, CliError> {
        let mut file = File::open(path).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => CliError::config_not_found(path),
            _ => CliError::config_read_error(path, error),
        })?;

        let metadata = file
            .metadata()
            .map_err(|error| CliError::config_read_error(path, error))?;
        if !metadata.is_file() {
            return Err(CliError::config_read_error(
                path,
                io::Error::new(io::ErrorKind::InvalidInput, "configuration is not a file"),
            ));
        }

        if metadata.len() > self.max_bytes as u64 {
            return Err(config_too_large(path, self.max_bytes));
        }

        let initial_capacity = usize::try_from(metadata.len())
            .unwrap_or(self.max_bytes)
            .min(self.max_bytes)
            .saturating_add(1);
        let mut bytes = Vec::with_capacity(initial_capacity);
        file.by_ref()
            .take(self.max_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| CliError::config_read_error(path, error))?;

        if bytes.len() > self.max_bytes {
            return Err(config_too_large(path, self.max_bytes));
        }
        Ok(bytes)
    }

    fn parse(
        &self,
        source: PathBuf,
        bytes: &[u8],
        request: &LoadRequest<'_>,
    ) -> Result<LoadedConfig, CliError> {
        let document: Value = serde_json::from_slice(bytes).map_err(|error| {
            CliError::invalid_config(
                &source,
                format!(
                    "File: {}; line: {}; column: {}",
                    source.display(),
                    error.line(),
                    error.column()
                ),
            )
            .with_source(error)
        })?;

        let substituted = substitute_environment(
            &document,
            request.strict_env,
            |name| {
                request
                    .env
                    .var_os(name)
                    .map(|value| value.to_string_lossy().into_owned())
            },
            request.diagnostics,
        )?;
        let document = substituted.value;

        let Some(mcp_servers) = document.get("mcpServers").and_then(Value::as_object) else {
            return Err(CliError::invalid_config(
                &source,
                format!(
                    "File: {}; field mcpServers must be an object",
                    source.display()
                ),
            ));
        };
        let servers = validate_mcp_servers(mcp_servers)?;

        Ok(LoadedConfig {
            source,
            document,
            servers,
            missing_env: substituted.missing,
            secrets: substituted.secrets,
        })
    }
}

impl ConfigurationLoader for FileConfigurationLoader {
    fn discover(&self, request: &LoadRequest<'_>) -> Result<PathBuf, CliError> {
        if let Some(path) = self.explicit_path(request) {
            return if Self::path_exists(&path)? {
                Ok(path)
            } else {
                Err(CliError::config_not_found(&path))
            };
        }

        let search_paths = self.default_paths(request);
        for path in &search_paths {
            if Self::path_exists(path)? {
                return Ok(path.clone());
            }
        }

        Err(CliError::config_not_found_in(
            search_paths.iter().map(PathBuf::as_path),
        ))
    }

    fn load(&self, request: &LoadRequest<'_>) -> Result<LoadedConfig, CliError> {
        let source = self.discover(request)?;
        let bytes = self.read_bounded(&source)?;
        self.parse(source, &bytes, request)
    }
}

fn resolve_from_cwd(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn config_too_large(path: &Path, maximum: usize) -> CliError {
    CliError::config_read_error(
        path,
        io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration exceeds maximum size",
        ),
    )
    .with_details(format!(
        "File: {}; maximum size: {maximum} bytes",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString, fs};

    use tempfile::TempDir;

    use super::*;
    use crate::{error::ErrorKind, policy::redact::WriterDiagnosticSink};

    #[derive(Default)]
    struct MapEnv(BTreeMap<String, OsString>);

    impl MapEnv {
        fn with_path(path: &Path) -> Self {
            Self(BTreeMap::from([(
                CONFIG_PATH_ENV.to_owned(),
                path.as_os_str().to_owned(),
            )]))
        }
    }

    impl EnvSource for MapEnv {
        fn var_os(&self, name: &str) -> Option<OsString> {
            self.0.get(name).cloned()
        }
    }

    struct Fixture {
        root: TempDir,
        cwd: PathBuf,
        home: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("create tempdir");
            let cwd = root.path().join("cwd");
            let home = root.path().join("home");
            fs::create_dir_all(&cwd).expect("create cwd");
            fs::create_dir_all(home.join(".config/mcp")).expect("create home config");
            Self { root, cwd, home }
        }

        fn write_config(&self, path: &Path, server: &str) {
            fs::write(
                path,
                format!(r#"{{"mcpServers":{{"{server}":{{"command":"runner"}}}}}}"#),
            )
            .expect("write config");
        }

        fn cwd_path(&self) -> PathBuf {
            self.cwd.join(CONFIG_FILE_NAME)
        }

        fn home_path(&self) -> PathBuf {
            self.home.join(".mcp_servers.json")
        }

        fn xdg_path(&self) -> PathBuf {
            self.home.join(".config/mcp").join(CONFIG_FILE_NAME)
        }
    }

    #[test]
    fn priority_is_cli_then_env_then_each_default_path() {
        let fixture = Fixture::new();
        let cli = fixture.root.path().join("cli.json");
        let env_path = fixture.root.path().join("env.json");
        let cwd_path = fixture.cwd_path();
        let home_path = fixture.home_path();
        let xdg_path = fixture.xdg_path();
        for (path, server) in [
            (&cli, "cli"),
            (&env_path, "env"),
            (&cwd_path, "cwd"),
            (&home_path, "home"),
            (&xdg_path, "xdg"),
        ] {
            fixture.write_config(path, server);
        }

        let loader = FileConfigurationLoader::default();
        let env = MapEnv::with_path(&env_path);
        let request = LoadRequest::new(&fixture.cwd, &fixture.home, &env).with_cli_path(&cli);
        assert_eq!(loader.load(&request).expect("CLI config").source, cli);

        let request = LoadRequest::new(&fixture.cwd, &fixture.home, &env);
        assert_eq!(loader.load(&request).expect("env config").source, env_path);

        let no_env = MapEnv::default();
        let request = LoadRequest::new(&fixture.cwd, &fixture.home, &no_env);
        assert_eq!(loader.load(&request).expect("cwd config").source, cwd_path);

        fs::remove_file(&cwd_path).expect("remove cwd config");
        assert_eq!(
            loader.load(&request).expect("home config").source,
            home_path
        );

        fs::remove_file(&home_path).expect("remove home config");
        assert_eq!(loader.load(&request).expect("XDG config").source, xdg_path);
    }

    #[test]
    fn missing_explicit_paths_do_not_fall_back() {
        let fixture = Fixture::new();
        fixture.write_config(&fixture.cwd_path(), "fallback");
        let missing_cli = fixture.root.path().join("missing-cli.json");
        let no_env = MapEnv::default();
        let cli_request =
            LoadRequest::new(&fixture.cwd, &fixture.home, &no_env).with_cli_path(&missing_cli);
        let error = FileConfigurationLoader::default()
            .load(&cli_request)
            .expect_err("missing CLI path must fail");
        assert_eq!(error.kind, ErrorKind::ConfigNotFound);
        assert!(error.message.contains(&missing_cli.display().to_string()));

        let missing_env = fixture.root.path().join("missing-env.json");
        let env = MapEnv::with_path(&missing_env);
        let env_request = LoadRequest::new(&fixture.cwd, &fixture.home, &env);
        let error = FileConfigurationLoader::default()
            .load(&env_request)
            .expect_err("missing env path must fail");
        assert_eq!(error.kind, ErrorKind::ConfigNotFound);
        assert!(error.message.contains(&missing_env.display().to_string()));
    }

    #[test]
    fn unreadable_explicit_path_is_a_read_error_without_fallback() {
        let fixture = Fixture::new();
        fixture.write_config(&fixture.cwd_path(), "fallback");
        let not_a_file = fixture.root.path().join("config-directory");
        fs::create_dir(&not_a_file).expect("create directory path");
        let env = MapEnv::default();
        let request =
            LoadRequest::new(&fixture.cwd, &fixture.home, &env).with_cli_path(&not_a_file);

        let error = FileConfigurationLoader::default()
            .load(&request)
            .expect_err("directory cannot be read as config");
        assert_eq!(error.kind, ErrorKind::ConfigReadError);
        assert!(error.message.contains(&not_a_file.display().to_string()));
    }

    #[test]
    fn missing_defaults_list_every_absolute_search_path() {
        let fixture = Fixture::new();
        let env = MapEnv::default();
        let request = LoadRequest::new(&fixture.cwd, &fixture.home, &env);
        let error = FileConfigurationLoader::default()
            .load(&request)
            .expect_err("defaults are absent");

        assert_eq!(error.kind, ErrorKind::ConfigNotFound);
        let details = error.details.expect("search details");
        for path in [fixture.cwd_path(), fixture.home_path(), fixture.xdg_path()] {
            assert!(path.is_absolute());
            assert!(
                details.contains(&path.display().to_string()),
                "missing {} from {details}",
                path.display()
            );
        }
    }

    #[test]
    fn valid_json_is_validated_and_server_names_are_sorted() {
        let fixture = Fixture::new();
        let config = fixture.cwd_path();
        fs::write(
            &config,
            r#"{"mcpServers":{"zeta":{"url":"https://example.test/mcp"},"alpha":{"command":"runner"}}}"#,
        )
        .expect("write valid config");
        let env = MapEnv::default();
        let request = LoadRequest::new(&fixture.cwd, &fixture.home, &env);

        let loaded = FileConfigurationLoader::default()
            .load(&request)
            .expect("parse valid config");
        assert_eq!(loaded.source, config);
        assert_eq!(
            loaded.servers.keys().collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
        assert!(matches!(
            loaded.servers["zeta"].transport,
            super::super::TransportConfig::Http { .. }
        ));
    }

    #[test]
    fn loader_substitutes_before_exposing_servers_and_returns_metadata() {
        let fixture = Fixture::new();
        let config = fixture.cwd_path();
        fs::write(
            &config,
            r#"{
                "mcpServers": {
                    "${SERVER_KEY}": {
                        "command": "${COMMAND}",
                        "args": ["${MISSING}", "${COMMAND}"]
                    }
                }
            }"#,
        )
        .expect("write substitutable config");
        let env = MapEnv(BTreeMap::from([
            ("COMMAND".to_owned(), OsString::from("runner-secret")),
            ("SERVER_KEY".to_owned(), OsString::from("must-not-replace")),
        ]));
        let diagnostics = WriterDiagnosticSink::new(Vec::new(), false, SecretSet::new());
        let loaded = {
            let request = LoadRequest::new(&fixture.cwd, &fixture.home, &env)
                .with_strict_env(false)
                .with_diagnostics(&diagnostics);
            FileConfigurationLoader::default()
                .load(&request)
                .expect("load and substitute config")
        };

        match &loaded.servers["${SERVER_KEY}"].transport {
            super::super::TransportConfig::Stdio { command, args, .. } => {
                assert_eq!(command, "runner-secret");
                assert_eq!(args[0], "");
            }
            transport => panic!("expected stdio transport, got {transport:?}"),
        }
        assert_eq!(loaded.missing_env, BTreeSet::from(["MISSING".to_owned()]));
        assert_eq!(loaded.secrets.len(), 1);
        assert_eq!(
            loaded.secrets.redact("value=runner-secret"),
            "value=[REDACTED]"
        );

        let warnings = String::from_utf8(diagnostics.into_inner()).expect("warnings are UTF-8");
        assert_eq!(
            warnings,
            "[mcp-cli] warning: Environment variable MISSING is not set; substituting an empty string\n"
        );
    }

    #[test]
    fn invalid_json_reports_path_line_and_column() {
        let fixture = Fixture::new();
        let config = fixture.cwd_path();
        fs::write(&config, "{\n  \"mcpServers\": {\n    \"broken\": ]\n  }\n}")
            .expect("write invalid config");
        let env = MapEnv::default();
        let request = LoadRequest::new(&fixture.cwd, &fixture.home, &env);

        let error = FileConfigurationLoader::default()
            .load(&request)
            .expect_err("JSON is invalid");
        assert_eq!(error.kind, ErrorKind::InvalidConfig);
        let details = error.details.expect("syntax details");
        assert!(details.contains(&config.display().to_string()));
        assert!(details.contains("line: 3"), "{details}");
        assert!(details.contains("column:"), "{details}");
    }

    #[test]
    fn mcp_servers_must_be_present_and_an_object() {
        let fixture = Fixture::new();
        let config = fixture.cwd_path();
        let env = MapEnv::default();
        let request = LoadRequest::new(&fixture.cwd, &fixture.home, &env);

        for invalid in [r#"{}"#, r#"{"mcpServers":null}"#, r#"{"mcpServers":[]}"#] {
            fs::write(&config, invalid).expect("write invalid structure");
            let error = FileConfigurationLoader::default()
                .load(&request)
                .expect_err("mcpServers must be an object");
            assert_eq!(error.kind, ErrorKind::InvalidConfig);
            assert!(
                error
                    .details
                    .expect("structure details")
                    .contains("mcpServers must be an object")
            );
        }
    }

    #[test]
    fn loader_rejects_invalid_server_fields_after_substitution() {
        let fixture = Fixture::new();
        let config = fixture.cwd_path();
        fs::write(
            &config,
            r#"{"mcpServers":{"broken":{"command":"runner","args":[1]}}}"#,
        )
        .expect("write invalid server config");
        let env = MapEnv::default();
        let request = LoadRequest::new(&fixture.cwd, &fixture.home, &env);

        let error = FileConfigurationLoader::default()
            .load(&request)
            .expect_err("typed server validation must run in the loader");
        assert_eq!(error.kind, ErrorKind::InvalidServerConfig);
        let details = error.details.expect("server field details");
        assert!(details.contains("broken"), "{details}");
        assert!(details.contains(".args[0]"), "{details}");
    }

    #[test]
    fn loader_wires_stable_ids_and_canonical_hashes_after_substitution() {
        let fixture = Fixture::new();
        let config = fixture.cwd_path();
        let env = MapEnv(BTreeMap::from([(
            "COMMAND".to_owned(),
            OsString::from("runner"),
        )]));
        let request = LoadRequest::new(&fixture.cwd, &fixture.home, &env);

        fs::write(
            &config,
            r#"{"mcpServers":{"unsafe/../name":{"env":{"Z":"last","A":"first"},"command":"${COMMAND}"}}}"#,
        )
        .expect("write first key order");
        let first = FileConfigurationLoader::default()
            .load(&request)
            .expect("load first key order");

        fs::write(
            &config,
            r#"{"mcpServers":{"unsafe/../name":{"command":"${COMMAND}","env":{"A":"first","Z":"last"}}}}"#,
        )
        .expect("write second key order");
        let second = FileConfigurationLoader::default()
            .load(&request)
            .expect("load second key order");

        let first_server = &first.servers["unsafe/../name"];
        let second_server = &second.servers["unsafe/../name"];
        assert_eq!(first_server.id, super::super::server_id("unsafe/../name"));
        assert_eq!(first_server.id, second_server.id);
        assert_eq!(first_server.config_hash, second_server.config_hash);
        assert_eq!(
            first_server.config_hash,
            super::super::config_hash(&first.document["mcpServers"]["unsafe/../name"])
        );
        assert_eq!(first_server.id.0.len(), super::super::SHA256_HEX_LENGTH);
        assert!(
            first_server
                .id
                .0
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        );

        fs::write(
            &config,
            r#"{"mcpServers":{"unsafe/../name":{"command":"different","env":{"A":"first","Z":"last"}}}}"#,
        )
        .expect("write changed config");
        let changed = FileConfigurationLoader::default()
            .load(&request)
            .expect("load changed config");
        assert_eq!(first_server.id, changed.servers["unsafe/../name"].id);
        assert_ne!(
            first_server.config_hash,
            changed.servers["unsafe/../name"].config_hash
        );
    }

    #[test]
    fn bounded_reader_rejects_a_file_above_the_hard_limit() {
        let fixture = Fixture::new();
        let config = fixture.cwd_path();
        File::create(&config)
            .expect("create oversized config")
            .set_len(MAX_CONFIG_BYTES as u64 + 1)
            .expect("size oversized config");
        let env = MapEnv::default();
        let request = LoadRequest::new(&fixture.cwd, &fixture.home, &env);

        let error = FileConfigurationLoader::default()
            .load(&request)
            .expect_err("oversized config must fail");
        assert_eq!(error.kind, ErrorKind::ConfigReadError);
        assert!(
            error
                .details
                .expect("size details")
                .contains(&MAX_CONFIG_BYTES.to_string())
        );
    }
}
