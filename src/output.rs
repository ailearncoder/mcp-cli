//! Business-output and diagnostic presentation boundary.

use std::{
    cmp::Ordering,
    collections::BTreeSet,
    io::{self, Write},
    sync::Mutex,
};

use serde::Serialize;
use serde_json::Value;

use crate::{
    domain::{
        CommandOutcome, PerServer, SearchHit, ServerSnapshot, ToolInfo, ToolResult,
        TransportSummary,
    },
    error::{CliError, ErrorKind},
    policy::redact::{SecretSet, WriterDiagnosticSink},
};

/// Renders one stable user-facing structured error to an injected writer.
///
/// The complete error is assembled before writing so formatting is independent
/// of writer buffering. The renderer owns neither process termination nor exit
/// code selection; the process boundary remains responsible for both.
pub fn render_structured_error(
    writer: &mut (impl Write + ?Sized),
    error: &CliError,
) -> io::Result<()> {
    render_structured_error_with_style(writer, error, StylePolicy::plain())
}

/// Renders a structured error using the policy of its actual destination
/// stream. ANSI wraps only the semantic error heading; all text is identical
/// after ANSI removal.
pub fn render_structured_error_with_style(
    writer: &mut (impl Write + ?Sized),
    error: &CliError,
    style: StylePolicy,
) -> io::Result<()> {
    let heading = format!("Error [{}]:", error.machine_kind());
    let mut rendered = format!(
        "{} {}",
        style_fragment(&heading, ANSI_RED_BOLD, style),
        error.message
    );
    if let Some(details) = &error.details {
        rendered.push_str("\n  Details: ");
        rendered.push_str(details);
    }
    if let Some(suggestion) = &error.suggestion {
        rendered.push_str("\n  Suggestion: ");
        rendered.push_str(suggestion);
    }
    rendered.push('\n');
    writer.write_all(rendered.as_bytes())
}

/// Formats one server snapshot for the compact list view.
///
/// Tool order is derived from tool names rather than transport completion order.
/// The formatter never adds ANSI escapes and always returns exactly one trailing
/// newline.
pub fn format_server_snapshot(snapshot: &ServerSnapshot, with_descriptions: bool) -> String {
    finish_lines(server_snapshot_lines(
        &snapshot.server,
        snapshot,
        with_descriptions,
    ))
}

/// Formats all list results, preserving successful snapshots when another
/// server failed. Servers and their tools are sorted before rendering.
pub fn format_server_list(
    results: &[PerServer<ServerSnapshot>],
    with_descriptions: bool,
) -> String {
    if results.is_empty() {
        return finish_lines(vec!["No servers configured.".to_owned()]);
    }

    let mut sorted = results.iter().collect::<Vec<_>>();
    sorted.sort_by(|left, right| compare_per_server(left, right));

    let mut lines = Vec::new();
    for (index, result) in sorted.into_iter().enumerate() {
        if index != 0 {
            lines.push(String::new());
        }
        match result {
            PerServer::Success { server, value } => {
                lines.extend(server_snapshot_lines(server, value, with_descriptions));
            }
            PerServer::Failure { server, error } => {
                lines.push(server.clone());
                lines.push(format!("  <error: {}>", single_line(&error.message)));
            }
        }
    }

    finish_lines(lines)
}

/// Formats the human-readable `info <server>` view.
///
/// Tool names and JSON Schema property names are sorted. Instructions are
/// server metadata rather than tool descriptions, so they remain visible for
/// both values of `with_descriptions`.
pub fn format_server_info(snapshot: &ServerSnapshot, with_descriptions: bool) -> String {
    let mut lines = vec![format!("Server: {}", snapshot.server)];
    match &snapshot.transport_summary {
        TransportSummary::Stdio { command } => {
            lines.push("Transport: stdio".to_owned());
            lines.push(format!("Command: {command}"));
        }
        TransportSummary::Http { url } => {
            lines.push("Transport: HTTP".to_owned());
            lines.push(format!("URL: {url}"));
        }
    }

    if let Some(instructions) = &snapshot.instructions {
        lines.push(String::new());
        lines.push("Instructions:".to_owned());
        push_block(&mut lines, instructions, "  ");
    }

    let tools = sorted_tools(&snapshot.tools);
    lines.push(String::new());
    lines.push(format!("Tools ({}):", tools.len()));
    if tools.is_empty() {
        lines.push("  (none)".to_owned());
    } else {
        for tool in tools {
            append_info_tool(&mut lines, tool, with_descriptions);
        }
    }

    finish_lines(lines)
}

/// Formats grep hits in stable `(server, tool)` order.
pub fn format_grep_hits(hits: &[SearchHit], with_descriptions: bool) -> String {
    if hits.is_empty() {
        return finish_lines(vec!["No matching tools found.".to_owned()]);
    }

    let mut sorted = hits.iter().collect::<Vec<_>>();
    sorted.sort_by(|left, right| {
        left.server
            .cmp(&right.server)
            .then_with(|| compare_tools(&left.tool, &right.tool))
    });

    let mut lines = Vec::with_capacity(sorted.len());
    for hit in sorted {
        let mut line = format!("{} {}", hit.server, hit.tool.name);
        if with_descriptions && let Some(description) = &hit.tool.description {
            line.push_str(" - ");
            line.push_str(&single_line(description));
        }
        lines.push(line);
    }
    finish_lines(lines)
}

/// Serializes any JSON value as exactly one compact JSON document followed by
/// one newline. No labels, diagnostics, or ANSI escapes are added.
pub fn format_json_value(value: &Value) -> Result<Vec<u8>, CliError> {
    serialize_json(value)
}

/// Formats the complete JSON Schema advertised for an MCP tool.
///
/// The schema remains an unmodified [`Value`], including non-object schemas
/// and extension keywords unknown to this crate.
pub fn format_json_schema(schema: &Value) -> Result<Vec<u8>, CliError> {
    format_json_value(schema)
}

/// Formats the complete MCP tool result without extracting text content or
/// rewriting protocol and extension fields.
pub fn format_tool_result(result: &ToolResult) -> Result<Vec<u8>, CliError> {
    format_json_value(result)
}

fn serialize_json(value: &(impl Serialize + ?Sized)) -> Result<Vec<u8>, CliError> {
    let mut rendered = serde_json::to_vec(value).map_err(json_serialization_error)?;
    rendered.push(b'\n');
    Ok(rendered)
}

fn json_serialization_error(source: serde_json::Error) -> CliError {
    CliError::from_kind(ErrorKind::InvalidJson, "Failed to serialize JSON output")
        .with_details("The JSON output value could not be serialized")
        .with_source(source)
}

fn server_snapshot_lines(
    server: &str,
    snapshot: &ServerSnapshot,
    with_descriptions: bool,
) -> Vec<String> {
    let tools = sorted_tools(&snapshot.tools);
    let mut lines = vec![server.to_owned()];
    if tools.is_empty() {
        lines.push("  (no tools)".to_owned());
        return lines;
    }

    for tool in tools {
        let mut line = format!("  • {}", tool.name);
        if with_descriptions && let Some(description) = &tool.description {
            line.push_str(" - ");
            line.push_str(&single_line(description));
        }
        lines.push(line);
    }
    lines
}

fn append_info_tool(lines: &mut Vec<String>, tool: &ToolInfo, with_descriptions: bool) {
    lines.push(format!("  {}", tool.name));
    if with_descriptions && let Some(description) = &tool.description {
        push_block(lines, description, "    ");
    }

    let Some(properties) = tool
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
    else {
        return;
    };
    if properties.is_empty() {
        return;
    }

    let required = tool
        .input_schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    let mut parameters = properties.iter().collect::<Vec<_>>();
    parameters.sort_by_key(|(name, _)| *name);

    lines.push("    Parameters:".to_owned());
    for (name, schema) in parameters {
        let kind = schema.get("type").and_then(Value::as_str).unwrap_or("any");
        let necessity = if required.contains(name.as_str()) {
            "required"
        } else {
            "optional"
        };
        let mut line = format!("      • {name} ({kind}, {necessity})");
        if with_descriptions
            && let Some(description) = schema.get("description").and_then(Value::as_str)
        {
            line.push_str(" - ");
            line.push_str(&single_line(description));
        }
        lines.push(line);
    }
}

fn sorted_tools(tools: &[ToolInfo]) -> Vec<&ToolInfo> {
    let mut sorted = tools.iter().collect::<Vec<_>>();
    sorted.sort_by(|left, right| compare_tools(left, right));
    sorted
}

fn compare_tools(left: &ToolInfo, right: &ToolInfo) -> Ordering {
    left.name
        .cmp(&right.name)
        .then_with(|| left.description.cmp(&right.description))
        .then_with(|| {
            left.input_schema
                .to_string()
                .cmp(&right.input_schema.to_string())
        })
}

fn compare_per_server<T>(left: &PerServer<T>, right: &PerServer<T>) -> Ordering {
    left.server().cmp(right.server()).then_with(|| {
        let left_rank = matches!(left, PerServer::Failure { .. });
        let right_rank = matches!(right, PerServer::Failure { .. });
        left_rank.cmp(&right_rank)
    })
}

fn push_block(lines: &mut Vec<String>, text: &str, indentation: &str) {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    for line in normalized.trim_end_matches('\n').split('\n') {
        lines.push(format!("{indentation}{line}"));
    }
}

fn single_line(text: &str) -> String {
    text.replace("\r\n", " ").replace(['\r', '\n'], " ")
}

fn finish_lines(lines: Vec<String>) -> String {
    let mut rendered = lines.join("\n");
    while rendered.ends_with(['\r', '\n']) {
        rendered.pop();
    }
    rendered.push('\n');
    rendered
}

/// A sendable diagnostics boundary kept separate from business output.
pub trait DiagnosticSink: Send + Sync {
    fn warning(&self, message: &str);
    fn debug(&self, message: &str);
    fn server_stderr(&self, server: &str, bytes: &[u8]);

    /// Flushes bytes retained to detect secrets spanning server stderr chunks.
    /// Existing sinks need no special behavior, preserving trait compatibility.
    fn server_stderr_flush(&self, _server: &str) {}
}

/// Styling inputs are computed independently for each output stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StylePolicy {
    pub is_tty: bool,
    /// True when `NO_COLOR` is present, regardless of its value.
    pub no_color: bool,
}

impl StylePolicy {
    pub const fn new(is_tty: bool, no_color_present: bool) -> Self {
        Self {
            is_tty,
            no_color: no_color_present,
        }
    }

    pub const fn plain() -> Self {
        Self::new(false, false)
    }

    pub const fn allows_ansi(self) -> bool {
        self.is_tty && !self.no_color
    }
}

/// Independently computed policies for the two process output streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamStylePolicies {
    pub stdout: StylePolicy,
    pub stderr: StylePolicy,
}

impl StreamStylePolicies {
    pub const fn new(stdout_is_tty: bool, stderr_is_tty: bool, no_color_present: bool) -> Self {
        Self {
            stdout: StylePolicy::new(stdout_is_tty, no_color_present),
            stderr: StylePolicy::new(stderr_is_tty, no_color_present),
        }
    }
}

pub(crate) const ANSI_RESET: &str = "\x1b[0m";
pub(crate) const ANSI_CYAN_BOLD: &str = "\x1b[1;36m";
pub(crate) const ANSI_YELLOW: &str = "\x1b[33m";
pub(crate) const ANSI_MAGENTA: &str = "\x1b[35m";
const ANSI_RED_BOLD: &str = "\x1b[1;31m";

/// Styles one already-formatted semantic fragment without changing its text.
pub(crate) fn style_fragment(fragment: &str, ansi: &str, policy: StylePolicy) -> String {
    if policy.allows_ansi() && !fragment.is_empty() {
        format!("{ansi}{fragment}{ANSI_RESET}")
    } else {
        fragment.to_owned()
    }
}

/// Injectable business-output renderer.
pub trait Presenter: Send + Sync {
    fn render(&self, outcome: CommandOutcome, style: StylePolicy) -> Result<Vec<u8>, CliError>;
}

/// Dedicated pure presenter for JSON business output.
///
/// Style is deliberately ignored: machine-readable JSON never contains ANSI
/// escapes, even when stdout is a TTY.
#[derive(Clone, Copy, Debug, Default)]
pub struct JsonPresenter;

impl JsonPresenter {
    pub fn render_value(&self, value: &Value) -> Result<Vec<u8>, CliError> {
        format_json_value(value)
    }
}

impl Presenter for JsonPresenter {
    fn render(&self, outcome: CommandOutcome, _style: StylePolicy) -> Result<Vec<u8>, CliError> {
        match outcome {
            CommandOutcome::Json(value) => self.render_value(&value),
            CommandOutcome::Empty => Ok(Vec::new()),
            CommandOutcome::HumanText(_) => Err(CliError::from_kind(
                ErrorKind::InvalidArguments,
                "Text outcomes cannot be rendered by the JSON presenter",
            )),
        }
    }
}

/// Presenter for successful human-readable business outcomes.
///
/// When styling is allowed, ANSI wraps only the completed semantic text
/// fragment. Removing the wrapper yields byte-for-byte the plain text output.
/// JSON outcomes always use [`JsonPresenter`] and therefore stay unstyled.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlainTextPresenter;

impl Presenter for PlainTextPresenter {
    fn render(&self, outcome: CommandOutcome, style: StylePolicy) -> Result<Vec<u8>, CliError> {
        match outcome {
            CommandOutcome::HumanText(text) => {
                let plain = finish_lines(vec![text]);
                let fragment = plain.strip_suffix('\n').unwrap_or(&plain);
                let mut rendered = style_fragment(fragment, ANSI_CYAN_BOLD, style).into_bytes();
                rendered.push(b'\n');
                Ok(rendered)
            }
            CommandOutcome::Empty => Ok(Vec::new()),
            json @ CommandOutcome::Json(_) => JsonPresenter.render(json, style),
        }
    }
}

/// Injectable two-stream presentation boundary.
///
/// The pure layer receives TTY and `NO_COLOR` presence as values and never
/// probes the terminal or environment. Business outcomes can only reach the
/// stdout writer; diagnostics can only reach the stderr-backed sink.
pub struct DualStreamWriter<Out: Write + Send, Err: Write + Send> {
    stdout: Mutex<Out>,
    diagnostics: WriterDiagnosticSink<Err>,
    styles: StreamStylePolicies,
}

impl<Out: Write + Send, Err: Write + Send> DualStreamWriter<Out, Err> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stdout: Out,
        stderr: Err,
        stdout_is_tty: bool,
        stderr_is_tty: bool,
        no_color_present: bool,
        debug_enabled: bool,
        secrets: SecretSet,
    ) -> Self {
        let styles = StreamStylePolicies::new(stdout_is_tty, stderr_is_tty, no_color_present);
        Self {
            stdout: Mutex::new(stdout),
            diagnostics: WriterDiagnosticSink::new_styled(
                stderr,
                debug_enabled,
                secrets,
                styles.stderr,
            ),
            styles,
        }
    }

    pub const fn styles(&self) -> StreamStylePolicies {
        self.styles
    }

    /// Renders and writes one business result exclusively to stdout.
    pub fn write_outcome(
        &self,
        presenter: &dyn Presenter,
        outcome: CommandOutcome,
    ) -> Result<(), CliError> {
        let rendered = presenter.render(outcome, self.styles.stdout)?;
        self.stdout
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .write_all(&rendered)
            .map_err(output_write_error)
    }

    pub fn into_writers(self) -> (Out, Err) {
        let stdout = match self.stdout.into_inner() {
            Ok(writer) => writer,
            Err(poisoned) => poisoned.into_inner(),
        };
        (stdout, self.diagnostics.into_inner())
    }
}

impl<Out: Write + Send, Err: Write + Send> DiagnosticSink for DualStreamWriter<Out, Err> {
    fn warning(&self, message: &str) {
        self.diagnostics.warning(message);
    }

    fn debug(&self, message: &str) {
        self.diagnostics.debug(message);
    }

    fn server_stderr(&self, server: &str, bytes: &[u8]) {
        self.diagnostics.server_stderr(server, bytes);
    }

    fn server_stderr_flush(&self, server: &str) {
        self.diagnostics.server_stderr_flush(server);
    }
}

fn output_write_error(source: io::Error) -> CliError {
    CliError::from_kind(ErrorKind::NetworkError, "Failed to write command output")
        .with_details("The destination output stream could not be written")
        .with_source(source)
}

#[cfg(test)]
mod tests {
    use std::io;

    use serde_json::json;
    use url::Url;

    use super::*;
    use crate::error::ErrorKind;

    fn tool(name: &str, description: Option<&str>, input_schema: Value) -> ToolInfo {
        ToolInfo {
            name: name.to_owned(),
            description: description.map(str::to_owned),
            input_schema,
        }
    }

    fn snapshot(server: &str, tools: Vec<ToolInfo>) -> ServerSnapshot {
        ServerSnapshot {
            server: server.to_owned(),
            transport_summary: TransportSummary::Stdio {
                command: format!("run-{server}"),
            },
            instructions: None,
            tools,
        }
    }

    #[test]
    fn structured_error_has_stable_lines_indentation_and_one_trailing_newline() {
        let error = CliError::from_kind(ErrorKind::NetworkError, "connection failed")
            .with_details("HTTP status: 503")
            .with_suggestion("Try again later");
        let mut output = Vec::new();

        render_structured_error(&mut output, &error).expect("render error");

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Error [NETWORK_ERROR]: connection failed\n  Details: HTTP status: 503\n  Suggestion: Try again later\n"
        );
    }

    #[test]
    fn structured_error_omits_absent_optional_fields() {
        let error = CliError::from_kind(ErrorKind::InvalidArguments, "bad input");
        let mut output = Vec::new();

        render_structured_error(&mut output, &error).expect("render error");

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Error [INVALID_ARGUMENTS]: bad input\n"
        );
    }

    #[test]
    fn structured_error_propagates_writer_failures() {
        struct FailingWriter;

        impl io::Write for FailingWriter {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let error = CliError::from_kind(ErrorKind::InvalidArguments, "bad input");
        let failure = render_structured_error(&mut FailingWriter, &error).unwrap_err();

        assert_eq!(failure.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn list_sorts_servers_and_tools_and_keeps_partial_failures() {
        let results = vec![
            PerServer::Success {
                server: "beta".to_owned(),
                value: snapshot(
                    "beta",
                    vec![
                        tool("zeta", Some("last"), json!({})),
                        tool("alpha", Some("first"), json!({})),
                    ],
                ),
            },
            PerServer::Failure {
                server: "charlie".to_owned(),
                error: CliError::network_error("charlie", "connection refused"),
            },
            PerServer::Success {
                server: "alpha".to_owned(),
                value: snapshot("alpha", vec![tool("middle", None, json!({}))]),
            },
        ];

        assert_eq!(
            format_server_list(&results, false),
            "alpha\n  • middle\n\nbeta\n  • alpha\n  • zeta\n\ncharlie\n  <error: Failed to communicate with server \"charlie\">\n"
        );
    }

    #[test]
    fn description_switch_controls_list_info_and_grep_description_text() {
        let described = tool(
            "search",
            Some("Search repositories"),
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"}
                },
                "required": ["query"]
            }),
        );
        let snapshot = snapshot("github", vec![described.clone()]);
        let list = vec![PerServer::Success {
            server: "github".to_owned(),
            value: snapshot.clone(),
        }];
        let hits = vec![SearchHit {
            server: "github".to_owned(),
            tool: described,
        }];

        for without in [
            format_server_list(&list, false),
            format_server_info(&snapshot, false),
            format_grep_hits(&hits, false),
        ] {
            assert!(!without.contains("Search repositories"));
            assert!(!without.contains("Search query"));
            assert!(without.ends_with('\n'));
            assert!(!without.ends_with("\n\n"));
        }
        for with in [
            format_server_list(&list, true),
            format_server_info(&snapshot, true),
            format_grep_hits(&hits, true),
        ] {
            assert!(with.contains("Search repositories"));
            assert!(with.ends_with('\n'));
            assert!(!with.ends_with("\n\n"));
        }
        assert!(format_server_info(&snapshot, true).contains("Search query"));
    }

    #[test]
    fn info_sorts_tools_and_parameters_and_preserves_instructions() {
        let snapshot = ServerSnapshot {
            server: "remote".to_owned(),
            transport_summary: TransportSummary::Http {
                url: Url::parse("https://example.test/mcp").unwrap(),
            },
            instructions: Some("first line\nsecond line".to_owned()),
            tools: vec![
                tool("zeta", None, json!({})),
                tool(
                    "alpha",
                    Some("Alpha tool"),
                    json!({
                        "properties": {
                            "z": {"type": "number"},
                            "a": {"type": "string"}
                        },
                        "required": ["z"]
                    }),
                ),
            ],
        };

        assert_eq!(
            format_server_info(&snapshot, false),
            "Server: remote\nTransport: HTTP\nURL: https://example.test/mcp\n\nInstructions:\n  first line\n  second line\n\nTools (2):\n  alpha\n    Parameters:\n      • a (string, optional)\n      • z (number, required)\n  zeta\n"
        );
    }

    #[test]
    fn grep_sorts_hits_and_has_a_successful_zero_result_message() {
        let hits = vec![
            SearchHit {
                server: "beta".to_owned(),
                tool: tool("zeta", Some("Z"), json!({})),
            },
            SearchHit {
                server: "alpha".to_owned(),
                tool: tool("zeta", None, json!({})),
            },
            SearchHit {
                server: "beta".to_owned(),
                tool: tool("alpha", Some("A"), json!({})),
            },
        ];

        assert_eq!(
            format_grep_hits(&hits, true),
            "alpha zeta\nbeta alpha - A\nbeta zeta - Z\n"
        );
        assert_eq!(format_grep_hits(&[], false), "No matching tools found.\n");
        assert_eq!(format_server_list(&[], false), "No servers configured.\n");
    }

    #[test]
    fn plain_text_presenter_preserves_text_and_routes_json_outcomes() {
        let presenter = PlainTextPresenter;
        let style = StylePolicy {
            is_tty: true,
            no_color: false,
        };

        assert_eq!(
            presenter
                .render(CommandOutcome::HumanText("ready\n\n".to_owned()), style)
                .unwrap(),
            b"\x1b[1;36mready\x1b[0m\n"
        );
        assert_eq!(
            presenter
                .render(CommandOutcome::Json(json!({"ok": true})), style)
                .unwrap(),
            b"{\"ok\":true}\n"
        );
    }

    #[test]
    fn json_schema_formatter_preserves_object_array_scalar_and_null_values() {
        let schemas = [
            json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"query": {"type": ["string", "null"]}},
                "x-vendor-keyword": {"enabled": true}
            }),
            json!([{"type": "string"}, false]),
            json!("https://example.test/schema"),
            json!(42),
            json!(true),
            Value::Null,
        ];

        for schema in schemas {
            let output = format_json_schema(&schema).expect("schema should serialize");
            assert_eq!(
                serde_json::from_slice::<Value>(&output).expect("valid single JSON value"),
                schema
            );
            assert_eq!(output.last(), Some(&b'\n'));
            assert!(!output[..output.len() - 1].ends_with(b"\n"));
        }
    }

    #[test]
    fn tool_result_formatter_preserves_complete_result_and_unknown_extensions() {
        let result = json!({
            "content": [
                {"type": "text", "text": "complete text"},
                {"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"}
            ],
            "isError": false,
            "structuredContent": {
                "items": [1, null, {"nested": true}]
            },
            "vendorExtension": {
                "traceId": "trace-123",
                "futureField": ["kept", 7]
            }
        });

        let output = format_tool_result(&result).expect("tool result should serialize");

        assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), result);
        assert_eq!(output.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert_eq!(output.last(), Some(&b'\n'));
        assert!(!String::from_utf8(output).unwrap().contains("Tool result:"));
    }

    #[test]
    fn json_presenter_emits_one_value_one_newline_without_style_or_prefixes() {
        let outcome = CommandOutcome::Json(json!([null, 1, "value"]));
        let styled = JsonPresenter
            .render(
                outcome,
                StylePolicy {
                    is_tty: true,
                    no_color: false,
                },
            )
            .expect("JSON outcome should render");

        assert_eq!(styled, b"[null,1,\"value\"]\n");
        let mut values = serde_json::Deserializer::from_slice(&styled).into_iter::<Value>();
        assert_eq!(values.next().unwrap().unwrap(), json!([null, 1, "value"]));
        assert!(
            values.next().is_none(),
            "stdout must contain only one JSON value"
        );
        assert!(!styled.contains(&0x1b));
    }

    #[test]
    fn dual_stream_writer_computes_stdout_and_stderr_style_independently() {
        let streams = DualStreamWriter::new(
            Vec::new(),
            Vec::new(),
            true,
            false,
            false,
            true,
            SecretSet::new(),
        );

        assert!(streams.styles().stdout.allows_ansi());
        assert!(!streams.styles().stderr.allows_ansi());
        streams
            .write_outcome(
                &PlainTextPresenter,
                CommandOutcome::HumanText("ready".to_owned()),
            )
            .unwrap();
        streams.warning("careful");
        streams.debug("trace");
        let (stdout, stderr) = streams.into_writers();

        assert_eq!(strip_ansi(&stdout), b"ready\n");
        assert!(stdout.contains(&0x1b));
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "[mcp-cli] warning: careful\n[mcp-cli] debug: trace\n"
        );
    }

    #[test]
    fn no_color_presence_disables_both_streams_even_when_value_was_empty() {
        // `true` represents presence, not truthiness; callers pass true for
        // `NO_COLOR=` as well as for any non-empty value.
        let streams = DualStreamWriter::new(
            Vec::new(),
            Vec::new(),
            true,
            true,
            true,
            true,
            SecretSet::new(),
        );
        streams
            .write_outcome(
                &PlainTextPresenter,
                CommandOutcome::HumanText("plain".to_owned()),
            )
            .unwrap();
        streams.warning("plain warning");
        let (stdout, stderr) = streams.into_writers();

        assert_eq!(stdout, b"plain\n");
        assert_eq!(stderr, b"[mcp-cli] warning: plain warning\n");
        assert!(!stdout.contains(&0x1b));
        assert!(!stderr.contains(&0x1b));
    }

    #[test]
    fn diagnostics_are_stderr_only_debug_is_suppressed_and_json_stays_pure() {
        let mut secrets = SecretSet::new();
        secrets.insert("credential");
        let streams =
            DualStreamWriter::new(Vec::new(), Vec::new(), false, true, false, false, secrets);

        streams
            .write_outcome(&JsonPresenter, CommandOutcome::Json(json!({"ok": true})))
            .unwrap();
        streams.warning("credential warning");
        streams.debug("must not appear");
        streams.server_stderr("alpha", b"credential from child");
        streams.server_stderr_flush("alpha");
        let (stdout, stderr) = streams.into_writers();

        assert_eq!(stdout, b"{\"ok\":true}\n");
        assert!(!stdout.contains(&0x1b));
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(stderr.contains("[mcp-cli] warning:"));
        assert!(stderr.contains("[server]"));
        assert!(stderr.contains("alpha: [REDACTED]"));
        assert!(!stderr.contains("credential"));
        assert!(!stderr.contains("must not appear"));
        assert!(stderr.contains("\x1b["), "stderr TTY should style prefixes");
    }

    fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
        let mut plain = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
                index += 2;
                while index < bytes.len() && !bytes[index].is_ascii_alphabetic() {
                    index += 1;
                }
                index += usize::from(index < bytes.len());
            } else {
                plain.push(bytes[index]);
                index += 1;
            }
        }
        plain
    }

    #[test]
    fn serialization_failures_map_to_a_stable_cli_error() {
        struct FailingValue;

        impl Serialize for FailingValue {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                Err(serde::ser::Error::custom("unstable internal cause"))
            }
        }

        let error = serialize_json(&FailingValue).unwrap_err();

        assert_eq!(error.kind, ErrorKind::InvalidJson);
        assert_eq!(error.message, "Failed to serialize JSON output");
        assert_eq!(
            error.details.as_deref(),
            Some("The JSON output value could not be serialized")
        );
        assert!(!format!("{error:?}").contains("unstable internal cause"));
    }
}
