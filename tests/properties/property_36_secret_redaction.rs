#![cfg(unix)]
#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    time::{Duration, UNIX_EPOCH},
};

use mcp_cli::{
    CliError, CommandOutcome, DiagnosticSink, DualStreamWriter, PlainTextPresenter, SecretSet,
    TransportConfig, WriterDiagnosticSink,
    config::{config_hash, server_id},
    daemon::{DaemonPaths, MetadataStore, PidMetadata},
    output::render_structured_error,
};
use proptest::prelude::*;
use serde_json::{Value, json};
use tempfile::TempDir;
use url::Url;

const CASES: u32 = 128;

#[derive(Clone, Debug)]
struct GeneratedSecrets {
    env: String,
    header: String,
    authorization: String,
    cookie: String,
}

impl GeneratedSecrets {
    fn from_token(token: &str) -> Self {
        Self {
            env: format!("ENV://{token}/OVERLAP"),
            header: format!("OVERLAP::HEADER/{token}"),
            authorization: format!("Bearer AUTH/{token}/credential"),
            cookie: format!("session=COOKIE/{token}/value"),
        }
    }

    fn values(&self) -> [&str; 4] {
        [&self.env, &self.header, &self.authorization, &self.cookie]
    }

    fn registered(&self) -> SecretSet {
        let mut secrets = SecretSet::new();
        secrets.register_env("PROPERTY36_ENV", &self.env);
        secrets.register_header("X-Property-Secret", &self.header);
        secrets.register_authorization(&self.authorization);
        secrets.register_cookie(&self.cookie);
        secrets
    }

    fn overlap_payload(&self, safe_context: &str) -> Vec<u8> {
        let shared_suffix = self
            .header
            .strip_prefix("OVERLAP")
            .expect("generated header has overlap prefix");
        format!(
            "{safe_context}: {}{} | {} | {} | done",
            self.env, shared_suffix, self.authorization, self.cookie
        )
        .into_bytes()
    }
}

#[derive(Debug)]
struct SecretSource(String);

impl fmt::Display for SecretSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for SecretSource {}

fn generated_case() -> impl Strategy<Value = (GeneratedSecrets, String, u16, Vec<usize>, u32, u64)>
{
    (
        proptest::string::string_regex("[A-Za-z0-9]{12,24}").expect("secret token regex is valid"),
        proptest::string::string_regex("srv-[a-z][a-z0-9-]{2,20}")
            .expect("safe server label regex is valid"),
        prop::sample::select(vec![400_u16, 401, 403, 429, 502, 503, 504]),
        prop::collection::vec(1_usize..=31, 1..=24),
        1_u32..=1_000_000,
        1_u64..=4_000_000_000,
    )
        .prop_map(|(token, server, status, widths, pid, started)| {
            (
                GeneratedSecrets::from_token(&token),
                server,
                status,
                widths,
                pid,
                started,
            )
        })
}

fn split_by_widths<'a>(bytes: &'a [u8], widths: &[usize]) -> Vec<&'a [u8]> {
    let mut chunks = Vec::new();
    let mut offset = 0;
    let mut width_index = 0;
    while offset < bytes.len() {
        let width = widths[width_index % widths.len()];
        let end = offset.saturating_add(width).min(bytes.len());
        chunks.push(&bytes[offset..end]);
        offset = end;
        width_index += 1;
    }
    chunks
}

fn oracle_assert_no_secret(
    channel: &str,
    bytes: &[u8],
    secrets: &GeneratedSecrets,
) -> Result<(), TestCaseError> {
    for secret in secrets.values() {
        if bytes
            .windows(secret.len())
            .any(|candidate| candidate == secret.as_bytes())
        {
            return Err(TestCaseError::fail(format!(
                "{channel} leaked a generated secret"
            )));
        }
    }
    Ok(())
}

fn oracle_assert_context(
    channel: &str,
    text: &str,
    server: &str,
    status: u16,
) -> Result<(), TestCaseError> {
    if !text.contains(server) {
        return Err(TestCaseError::fail(format!(
            "{channel} removed safe server context {server:?}"
        )));
    }
    if !text.contains(&status.to_string()) {
        return Err(TestCaseError::fail(format!(
            "{channel} removed safe HTTP status {status}"
        )));
    }
    Ok(())
}

fn assert_exact_pid_keys(value: &Value) -> Result<(), TestCaseError> {
    let keys = value
        .as_object()
        .ok_or_else(|| TestCaseError::fail("PID metadata was not a JSON object"))?
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    let approved = BTreeSet::from([
        "config_hash".to_owned(),
        "pid".to_owned(),
        "started_at".to_owned(),
    ]);
    if keys != approved {
        return Err(TestCaseError::fail(format!(
            "PID metadata key set was not the approved fixed set: {keys:?}"
        )));
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: CASES,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 36: 敏感信息跨通道脱敏且保留安全上下文
    // **Validates: Requirements 7.4, 16.5, 16.8**
    #[test]
    fn property_36_secrets_are_redacted_across_channels_while_safe_context_survives(
        (generated, server, status, widths, pid, started_at) in generated_case(),
    ) {
        let distinct = generated.values().into_iter().collect::<BTreeSet<_>>();
        prop_assert_eq!(distinct.len(), generated.values().len());
        prop_assert!(generated.values().into_iter().all(|secret| !secret.is_empty()));

        let secrets = generated.registered();
        prop_assert_eq!(secrets.len(), generated.values().len());
        let stderr_payload = generated.overlap_payload("safe-stderr-context");

        // Exercise warning, debug, and actual per-server streaming stderr boundaries
        // for both debug states. Business stdout is deliberately not redacted.
        for debug_enabled in [false, true] {
            let streams = DualStreamWriter::new(
                Vec::new(),
                Vec::new(),
                false,
                false,
                false,
                debug_enabled,
                secrets.clone(),
            );
            let business = format!("legitimate-business-output:{}", generated.env);
            streams
                .write_outcome(
                    &PlainTextPresenter,
                    CommandOutcome::HumanText(business.clone()),
                )
                .expect("in-memory business output succeeds");
            streams.warning(&format!(
                "safe-warning-context {} {} {} {}",
                generated.env, generated.header, generated.authorization, generated.cookie
            ));
            streams.debug(&format!(
                "safe-debug-context {} {} {} {}",
                generated.env, generated.header, generated.authorization, generated.cookie
            ));
            for chunk in split_by_widths(&stderr_payload, &widths) {
                streams.server_stderr(&server, chunk);
            }
            streams.server_stderr_flush(&server);

            let (stdout, stderr) = streams.into_writers();
            let stdout_text = String::from_utf8(stdout)
                .map_err(|error| TestCaseError::fail(error.to_string()))?;
            prop_assert!(stdout_text.contains(&business));
            prop_assert!(stdout_text.contains(&generated.env));

            oracle_assert_no_secret("diagnostic stderr", &stderr, &generated)?;
            let stderr_text = String::from_utf8(stderr)
                .map_err(|error| TestCaseError::fail(error.to_string()))?;
            prop_assert!(stderr_text.contains("safe-warning-context"));
            prop_assert!(stderr_text.contains(&server));
            prop_assert_eq!(stderr_text.contains("safe-debug-context"), debug_enabled);
            prop_assert!(stderr_text.contains("[REDACTED]"));
        }

        // Exercise the same production redaction composition used by main before
        // Structured_Error rendering. The retained source contains credentials,
        // but Display/Debug and rendered fields must never expose it.
        let source_text = format!(
            "transport source: {} {} {} {}",
            generated.env, generated.header, generated.authorization, generated.cookie
        );
        let http_error = CliError::http_status(&server, status)
            .with_source(SecretSource(source_text.clone()));
        prop_assert!(Error::source(&http_error).is_some());
        let error_debug = format!("{http_error:?}");
        oracle_assert_no_secret("CliError Debug", error_debug.as_bytes(), &generated)?;
        oracle_assert_context("CliError Debug", &error_debug, &server, status)?;

        let tainted_error = http_error
            .with_details(format!(
                "HTTP status: {status}; transport={} header={} auth={} cookie={}",
                generated.env, generated.header, generated.authorization, generated.cookie
            ))
            .with_suggestion(format!(
                "Retry {server} without {} or {}",
                generated.authorization, generated.cookie
            ));
        let error_sink = WriterDiagnosticSink::new(Vec::new(), true, secrets.clone());
        error_sink.debug(&source_text);
        let safe_error = error_sink.redact_error(tainted_error);
        let safe_error_debug = format!("{safe_error:?}");
        oracle_assert_no_secret("redacted CliError Debug", safe_error_debug.as_bytes(), &generated)?;
        oracle_assert_context("redacted CliError Debug", &safe_error_debug, &server, status)?;

        let mut structured = Vec::new();
        render_structured_error(&mut structured, &safe_error)
            .expect("in-memory structured error rendering succeeds");
        oracle_assert_no_secret("Structured_Error", &structured, &generated)?;
        let structured_text = String::from_utf8(structured)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        oracle_assert_context("Structured_Error", &structured_text, &server, status)?;
        prop_assert!(structured_text.contains("[REDACTED]"));

        let error_diagnostics = error_sink.into_inner();
        oracle_assert_no_secret("error debug diagnostics", &error_diagnostics, &generated)?;
        prop_assert!(String::from_utf8_lossy(&error_diagnostics).contains("[REDACTED]"));

        // Transport Debug is a production diagnostic context boundary: names and
        // safe endpoint shape remain useful, while all configured values stay out.
        let stdio_transport = TransportConfig::Stdio {
            command: "safe-property-server".to_owned(),
            args: vec!["--stdio".to_owned()],
            env: BTreeMap::from([
                ("PROPERTY36_ENV".to_owned(), generated.env.clone()),
                ("PROPERTY36_OTHER".to_owned(), generated.header.clone()),
            ]),
            cwd: None,
        };
        let http_transport = TransportConfig::Http {
            url: Url::parse("https://example.invalid/mcp").expect("static URL is valid"),
            headers: BTreeMap::from([
                ("Authorization".to_owned(), generated.authorization.clone()),
                ("Cookie".to_owned(), generated.cookie.clone()),
                ("X-Property-Secret".to_owned(), generated.header.clone()),
            ]),
        };
        for (name, debug_text) in [
            ("stdio transport Debug", format!("{stdio_transport:?}")),
            ("HTTP transport Debug", format!("{http_transport:?}")),
        ] {
            oracle_assert_no_secret(name, debug_text.as_bytes(), &generated)?;
        }
        let http_debug = format!("{http_transport:?}");
        prop_assert!(http_debug.contains("https"));
        prop_assert!(http_debug.contains("authorization"));
        prop_assert!(http_debug.contains("cookie"));

        // Derive the persisted hash from configuration that contains every secret.
        // The metadata schema and persisted bytes must still expose only fixed safe fields.
        let secret_config = json!({
            "command": "safe-property-server",
            "env": {"PROPERTY36_ENV": generated.env, "PROPERTY36_HEADER": generated.header},
            "headers": {
                "Authorization": generated.authorization,
                "Cookie": generated.cookie,
            },
        });
        let metadata = PidMetadata {
            pid,
            config_hash: config_hash(&secret_config),
            started_at: UNIX_EPOCH + Duration::from_secs(started_at),
        };
        let serialized = serde_json::to_vec(&metadata).expect("PID metadata serializes");
        oracle_assert_no_secret("serialized PID metadata", &serialized, &generated)?;
        let serialized_value: Value = serde_json::from_slice(&serialized)
            .expect("serialized PID metadata is JSON");
        assert_exact_pid_keys(&serialized_value)?;

        let temp = TempDir::new().expect("isolated metadata property directory");
        let paths = DaemonPaths::from_runtime_parent(temp.path(), &server_id(&server))
            .expect("safe generated server ID and isolated runtime path");
        let store = MetadataStore::new(paths);
        store.write(&metadata).expect("persist PID metadata");
        let persisted = fs::read(&store.paths().pid).expect("read persisted PID metadata");
        oracle_assert_no_secret("persisted PID metadata", &persisted, &generated)?;
        let persisted_value: Value = serde_json::from_slice(&persisted)
            .expect("persisted PID metadata is JSON");
        assert_exact_pid_keys(&persisted_value)?;
        prop_assert_eq!(store.read().expect("read metadata through production store"), metadata);
    }
}
