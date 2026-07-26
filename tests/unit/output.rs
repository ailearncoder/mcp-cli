use mcp_cli::{
    CliError, CommandOutcome, DiagnosticSink, DualStreamWriter, ErrorKind, JsonPresenter,
    PerServer, PlainTextPresenter, SearchHit, SecretSet, ServerSnapshot, StreamStylePolicies,
    ToolInfo, TransportSummary, format_grep_hits, format_json_schema, format_server_info,
    format_server_list, format_tool_result, render_structured_error,
};
use serde_json::{Value, json};
use url::Url;

fn tool(name: &str, description: Option<&str>, schema: Value) -> ToolInfo {
    ToolInfo {
        name: name.to_owned(),
        description: description.map(str::to_owned),
        input_schema: schema,
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
fn structured_error_renders_optional_lines_exactly() {
    let complete = CliError::from_kind(ErrorKind::NetworkError, "connection failed")
        .with_details("HTTP status: 503")
        .with_suggestion("Try again later");
    let minimal = CliError::from_kind(ErrorKind::InvalidArguments, "bad input");

    let mut output = Vec::new();
    render_structured_error(&mut output, &complete).unwrap();
    assert_eq!(
        output,
        b"Error [NETWORK_ERROR]: connection failed\n  Details: HTTP status: 503\n  Suggestion: Try again later\n"
    );

    output.clear();
    render_structured_error(&mut output, &minimal).unwrap();
    assert_eq!(output, b"Error [INVALID_ARGUMENTS]: bad input\n");
    assert!(!String::from_utf8(output).unwrap().contains("Details:"));
}

#[test]
fn text_formatters_sort_keep_partial_failures_and_report_zero_results() {
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
    assert_eq!(format_server_list(&[], false), "No servers configured.\n");
    assert_eq!(format_grep_hits(&[], false), "No matching tools found.\n");
}

#[test]
fn description_switch_only_controls_available_descriptions() {
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

    for hidden in [
        format_server_list(&list, false),
        format_server_info(&snapshot, false),
        format_grep_hits(&hits, false),
    ] {
        assert!(!hidden.contains("Search repositories"));
        assert!(!hidden.contains("Search query"));
    }

    assert!(format_server_list(&list, true).contains("Search repositories"));
    let info = format_server_info(&snapshot, true);
    assert!(info.contains("Search repositories"));
    assert!(info.contains("Search query"));
    assert!(format_grep_hits(&hits, true).contains("Search repositories"));
}

#[test]
fn server_info_has_deterministic_tool_and_parameter_order() {
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
fn schema_and_complete_tool_result_are_single_pure_json_values() {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {"query": {"type": ["string", "null"]}},
        "x-extension": {"kept": true}
    });
    let result = json!({
        "content": [
            {"type": "text", "text": "complete text"},
            {"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"}
        ],
        "isError": false,
        "structuredContent": {"items": [1, null, {"nested": true}]},
        "vendorExtension": {"futureField": ["kept", 7]}
    });

    for (rendered, expected) in [
        (format_json_schema(&schema).unwrap(), schema),
        (format_tool_result(&result).unwrap(), result),
    ] {
        assert_eq!(rendered.last(), Some(&b'\n'));
        assert_eq!(rendered.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert!(!rendered.contains(&0x1b));

        let mut values = serde_json::Deserializer::from_slice(&rendered).into_iter::<Value>();
        assert_eq!(values.next().unwrap().unwrap(), expected);
        assert!(
            values.next().is_none(),
            "output contained a second JSON value"
        );
    }
}

#[test]
fn diagnostics_stay_on_stderr_while_json_stdout_remains_exact() {
    let mut secrets = SecretSet::new();
    secrets.insert("credential");
    let streams =
        DualStreamWriter::new(Vec::new(), Vec::new(), false, false, false, false, secrets);

    streams
        .write_outcome(&JsonPresenter, CommandOutcome::Json(json!({"ok": true})))
        .unwrap();
    streams.warning("credential warning");
    streams.debug("suppressed debug");
    streams.server_stderr("alpha", b"credential from child");
    streams.server_stderr_flush("alpha");

    let (stdout, stderr) = streams.into_writers();
    assert_eq!(stdout, b"{\"ok\":true}\n");
    let stderr = String::from_utf8(stderr).unwrap();
    assert!(stderr.contains("[mcp-cli] warning: [REDACTED] warning"));
    assert!(stderr.contains("[server] alpha:"));
    assert!(stderr.contains("[REDACTED]"));
    assert!(stderr.contains("child"));
    assert!(!stderr.contains("credential"));
    assert!(!stderr.contains("suppressed debug"));
    assert!(!stdout.contains(&0x1b));
}

#[test]
fn color_policy_is_per_stream_and_no_color_presence_including_empty_disables_ansi() {
    let mixed = StreamStylePolicies::new(true, false, false);
    assert!(mixed.stdout.allows_ansi());
    assert!(!mixed.stderr.allows_ansi());

    let colored = DualStreamWriter::new(
        Vec::new(),
        Vec::new(),
        true,
        true,
        false,
        true,
        SecretSet::new(),
    );
    colored
        .write_outcome(
            &PlainTextPresenter,
            CommandOutcome::HumanText("ready".to_owned()),
        )
        .unwrap();
    colored.warning("careful");
    let (colored_stdout, colored_stderr) = colored.into_writers();
    assert!(colored_stdout.contains(&0x1b));
    assert!(colored_stderr.contains(&0x1b));

    // `true` represents environment-variable presence, including `NO_COLOR=`.
    let no_color_from_empty_value = DualStreamWriter::new(
        Vec::new(),
        Vec::new(),
        true,
        true,
        true,
        true,
        SecretSet::new(),
    );
    no_color_from_empty_value
        .write_outcome(
            &PlainTextPresenter,
            CommandOutcome::HumanText("ready".to_owned()),
        )
        .unwrap();
    no_color_from_empty_value.warning("careful");
    let (plain_stdout, plain_stderr) = no_color_from_empty_value.into_writers();
    assert_eq!(plain_stdout, b"ready\n");
    assert_eq!(plain_stderr, b"[mcp-cli] warning: careful\n");
    assert!(!plain_stdout.contains(&0x1b));
    assert!(!plain_stderr.contains(&0x1b));
}
