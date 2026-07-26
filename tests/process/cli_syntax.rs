use std::process::Output;

use assert_cmd::Command;
use mcp_cli::cli_command;

const CLIENT_EXIT: i32 = 1;

fn run(args: &[&str], no_color: Option<&str>) -> Output {
    let mut command = Command::cargo_bin("mcp-cli").expect("mcp-cli binary");
    command.env_remove("NO_COLOR").args(args);
    if let Some(value) = no_color {
        command.env("NO_COLOR", value);
    }
    command.output().expect("mcp-cli process should run")
}

fn assert_success_stdout_only(args: &[&str], expected_stdout: &[u8]) {
    let output = run(args, None);
    assert_eq!(output.status.code(), Some(0), "args: {args:?}");
    assert_eq!(output.stdout, expected_stdout, "args: {args:?}");
    assert!(output.stderr.is_empty(), "args: {args:?}");
}

fn assert_client_error(args: &[&str], expected_stderr: &str) {
    let output = run(args, None);
    assert_eq!(output.status.code(), Some(CLIENT_EXIT), "args: {args:?}");
    assert!(output.stdout.is_empty(), "args: {args:?}");
    assert_eq!(output.stderr, expected_stderr.as_bytes(), "args: {args:?}");
    assert_eq!(
        expected_stderr.matches("Error [").count(),
        1,
        "fixture must describe exactly one structured error"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr)
            .matches("Error [")
            .count(),
        1,
        "top-level boundary must render exactly once"
    );
}

fn expected_help() -> Vec<u8> {
    let mut command = cli_command();
    let mut help = command.render_long_help().to_string();
    if !help.ends_with('\n') {
        help.push('\n');
    }
    help.into_bytes()
}

#[test]
fn help_short_and_long_are_exact_stdout_only_and_hide_daemon() {
    let expected = expected_help();
    assert!(expected.windows(b"info".len()).any(|part| part == b"info"));
    assert!(expected.windows(b"grep".len()).any(|part| part == b"grep"));
    assert!(expected.windows(b"call".len()).any(|part| part == b"call"));
    assert!(
        !expected
            .windows(b"__daemon".len())
            .any(|part| part == b"__daemon")
    );

    assert_success_stdout_only(&["--help"], &expected);
    assert_success_stdout_only(&["-h"], &expected);
}

#[test]
fn version_short_and_long_are_exact_stdout_only() {
    let expected = format!("mcp-cli {}\n", env!("CARGO_PKG_VERSION"));
    assert_success_stdout_only(&["--version"], expected.as_bytes());
    assert_success_stdout_only(&["-v"], expected.as_bytes());
}

#[test]
fn unknown_options_are_single_structured_client_errors() {
    assert_client_error(
        &["--server", "filesystem"],
        "Error [INVALID_ARGUMENTS]: Unknown option: \"--server\"\n  Details: Valid options: -c/--config, -d/--with-descriptions, -h/--help, -v/--version\n  Suggestion: Run 'mcp-cli --help' to see valid public options\n",
    );
    assert_client_error(
        &["call", "filesystem", "read_file", "--args", "{}"],
        "Error [INVALID_ARGUMENTS]: Unknown option: \"--args\"\n  Details: Valid options: -c/--config, -d/--with-descriptions, -h/--help, -v/--version\n  Suggestion: Run 'mcp-cli --help' to see valid public options\n",
    );
}

#[test]
fn common_aliases_are_rejected_with_exact_public_replacements() {
    for (alias, replacement) in [
        ("run", "call"),
        ("execute", "call"),
        ("exec", "call"),
        ("invoke", "call"),
        ("list", "info"),
        ("ls", "info"),
        ("get", "info"),
        ("show", "info"),
        ("describe", "info"),
        ("search", "grep"),
        ("find", "grep"),
        ("query", "grep"),
    ] {
        let expected = format!(
            "Error [UNKNOWN_COMMAND]: Unknown command: \"{alias}\"\n  Details: Valid commands: info, grep, call\n  Suggestion: Use 'mcp-cli {replacement}' instead\n"
        );
        assert_client_error(&[alias], &expected);
    }
}

#[test]
fn missing_arguments_are_single_structured_client_errors() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["call"],
            "Error [INVALID_ARGUMENTS]: Missing required argument for call: server and tool\n  Details: The call command requires server and tool\n  Suggestion: Use 'mcp-cli call <server> <tool> [json]'\n",
        ),
        (
            &["call", "filesystem"],
            "Error [INVALID_ARGUMENTS]: Missing required argument for call: tool\n  Details: The call command requires tool\n  Suggestion: Use 'mcp-cli call <server> <tool> [json]' or 'mcp-cli call <server>/<tool> [json]'\n",
        ),
        (
            &["grep"],
            "Error [INVALID_ARGUMENTS]: Missing required argument for grep: pattern\n  Details: The grep command requires pattern\n  Suggestion: Use 'mcp-cli grep <pattern>'\n",
        ),
        (
            &["-c"],
            "Error [INVALID_ARGUMENTS]: Missing required path for -c/--config\n  Details: The config option requires one path value\n  Suggestion: Use '-c <path>' or '--config <path>'\n",
        ),
    ];

    for (args, expected) in cases {
        assert_client_error(args, expected);
    }
}

#[test]
fn extra_arguments_are_single_structured_client_errors() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["grep", "one", "two"],
            "Error [INVALID_ARGUMENTS]: Too many positional arguments for grep\n  Details: Shell-quote JSON or patterns containing spaces so they remain one argument\n  Suggestion: Use 'mcp-cli grep <pattern>' with one shell-quoted pattern\n",
        ),
        (
            &["info", "server", "tool", "extra"],
            "Error [INVALID_ARGUMENTS]: Too many positional arguments for info\n  Details: Shell-quote JSON or patterns containing spaces so they remain one argument\n  Suggestion: Use 'mcp-cli info <server> [tool]' or 'mcp-cli info <server>/<tool>'\n",
        ),
        (
            &["call", "server", "tool", "{}", "extra"],
            "Error [INVALID_ARGUMENTS]: Too many positional arguments for call\n  Details: Shell-quote JSON or patterns containing spaces so they remain one argument\n  Suggestion: Use one JSON argument: 'mcp-cli call <server> <tool> [json]'\n",
        ),
    ];

    for (args, expected) in cases {
        assert_client_error(args, expected);
    }
}

#[test]
fn commandless_tool_syntax_is_an_exact_ambiguous_client_error() {
    let expected = "Error [INVALID_ARGUMENTS]: Ambiguous command syntax\n  Details: 'server tool' could mean tool information or a tool call\n  Suggestion: Use 'mcp-cli info server tool' or 'mcp-cli call server tool [json]'\n";
    assert_client_error(&["server", "tool"], expected);
    assert_client_error(&["server", "tool", "{}"], expected);
}

#[test]
fn no_color_presence_including_empty_value_keeps_errors_ansi_free() {
    let expected = "Error [INVALID_ARGUMENTS]: Unknown option: \"--bad\"\n  Details: Valid options: -c/--config, -d/--with-descriptions, -h/--help, -v/--version\n  Suggestion: Run 'mcp-cli --help' to see valid public options\n";

    for value in ["", "1"] {
        let output = run(&["--bad"], Some(value));
        assert_eq!(output.status.code(), Some(CLIENT_EXIT));
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, expected.as_bytes());
        assert!(!output.stderr.contains(&0x1b));
    }
}
