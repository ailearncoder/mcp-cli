//! Command-line parsing boundary.
//!
//! [`cli_command`] owns clap's public help/version metadata. [`parse_cli`]
//! deliberately parses `OsString` values itself so configuration paths remain
//! lossless on platforms where paths need not be UTF-8. Neither function
//! performs process or file-system I/O.

use std::{ffi::OsString, path::PathBuf};

use clap::{Arg, ArgAction, Command, ValueHint};

use crate::error::CliError;

const PUBLIC_COMMANDS: &str = "info, grep, call";
const PUBLIC_OPTIONS: &str = "-c/--config, -d/--with-descriptions, -h/--help, -v/--version";

/// A parsed public command, independent of clap and process I/O.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandSpec {
    List {
        with_descriptions: bool,
    },
    Info {
        server: String,
        tool: Option<String>,
        with_descriptions: bool,
    },
    Grep {
        pattern: String,
        with_descriptions: bool,
    },
    Call {
        server: String,
        tool: String,
        inline_json: Option<String>,
    },
    Help,
    Version,
}

/// Process-independent result of parsing an invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliInvocation {
    pub command: CommandSpec,
    pub config_path: Option<PathBuf>,
}

/// Build the clap metadata used to render public help and version output.
///
/// Parsing remains in [`parse_cli`]. The hidden daemon command is registered so
/// a later internal entry point can share the binary, but clap omits it from
/// every public help listing.
pub fn cli_command() -> Command {
    let config = Arg::new("config")
        .short('c')
        .long("config")
        .value_name("PATH")
        .value_hint(ValueHint::FilePath)
        .help("Path to mcp_servers.json")
        .global(true);
    let descriptions = Arg::new("with-descriptions")
        .short('d')
        .long("with-descriptions")
        .help("Include tool descriptions")
        .action(ArgAction::SetTrue)
        .global(true);

    Command::new("mcp-cli")
        .version(env!("CARGO_PKG_VERSION"))
        .about("A lightweight CLI for MCP servers")
        .disable_help_flag(true)
        .disable_version_flag(true)
        .arg(config)
        .arg(descriptions)
        .arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .help("Print help")
                .action(ArgAction::Help),
        )
        .arg(
            Arg::new("version")
                .short('v')
                .long("version")
                .help("Print version")
                .action(ArgAction::Version),
        )
        .arg(
            Arg::new("server")
                .value_name("SERVER")
                .help("Show information for a server when no command is given"),
        )
        .subcommand(
            Command::new("info")
                .about("Show server details or a tool schema")
                .arg(Arg::new("target").value_name("SERVER[/TOOL]").required(true))
                .arg(Arg::new("tool").value_name("TOOL")),
        )
        .subcommand(
            Command::new("grep")
                .about("Search tools by glob pattern")
                .arg(Arg::new("pattern").value_name("PATTERN").required(true)),
        )
        .subcommand(
            Command::new("call")
                .about("Call a tool")
                .arg(Arg::new("target").value_name("SERVER[/TOOL]").required(true))
                .arg(Arg::new("tool").value_name("TOOL"))
                .arg(Arg::new("json").value_name("JSON")),
        )
        .subcommand(Command::new("__daemon").hide(true))
        .after_help(
            "Target forms:\n  info SERVER TOOL\n  info SERVER/TOOL\n  call SERVER TOOL [JSON]\n  call SERVER/TOOL [JSON]",
        )
}

/// Parse command arguments without consulting process state or performing I/O.
///
/// The iterator must contain arguments after the executable name. Config path
/// values are retained as `PathBuf`; every command, target, pattern, and JSON
/// token must be UTF-8.
pub fn parse_cli<I>(args: I) -> Result<CliInvocation, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut config_path = None;
    let mut with_descriptions = false;
    let mut positional = Vec::new();
    let mut options_ended = false;
    let mut args = args.into_iter();

    while let Some(argument) = args.next() {
        if !options_ended {
            if argument == "--" {
                options_ended = true;
                continue;
            }
            if argument == "-h" || argument == "--help" {
                return Ok(CliInvocation {
                    command: CommandSpec::Help,
                    config_path,
                });
            }
            if argument == "-v" || argument == "--version" {
                return Ok(CliInvocation {
                    command: CommandSpec::Version,
                    config_path,
                });
            }
            if argument == "-d" || argument == "--with-descriptions" {
                with_descriptions = true;
                continue;
            }
            if argument == "-c" || argument == "--config" {
                let Some(path) = args.next() else {
                    return Err(missing_config_path());
                };
                if path.is_empty() {
                    return Err(missing_config_path());
                }
                config_path = Some(PathBuf::from(path));
                continue;
            }
            if argument.to_string_lossy().starts_with('-') {
                return Err(unknown_option(&argument.to_string_lossy()));
            }
        }
        positional.push(require_utf8(argument)?);
    }

    parse_positionals(positional, with_descriptions, config_path)
}

fn parse_positionals(
    positional: Vec<String>,
    with_descriptions: bool,
    config_path: Option<PathBuf>,
) -> Result<CliInvocation, CliError> {
    let command = match positional.as_slice() {
        [] => CommandSpec::List { with_descriptions },
        [server] if !is_public_command(server) => {
            reject_alias_or_internal(server)?;
            validate_bare_server(server)?;
            CommandSpec::Info {
                server: server.clone(),
                tool: None,
                with_descriptions,
            }
        }
        [first, rest @ ..] if first == "info" => parse_info(rest, with_descriptions)?,
        [first, rest @ ..] if first == "grep" => parse_grep(rest, with_descriptions)?,
        [first, rest @ ..] if first == "call" => parse_call(rest)?,
        [first, ..] => {
            reject_alias_or_internal(first)?;
            return Err(ambiguous_command(&positional));
        }
    };

    Ok(CliInvocation {
        command,
        config_path,
    })
}

fn parse_info(args: &[String], with_descriptions: bool) -> Result<CommandSpec, CliError> {
    match args {
        [] => Err(missing_argument(
            "info",
            "server",
            "Use 'mcp-cli info <server>'",
        )),
        [target] => {
            let (server, tool) = parse_target(target, false, "info")?;
            Ok(CommandSpec::Info {
                server,
                tool,
                with_descriptions,
            })
        }
        [server, tool] if !server.contains('/') => {
            validate_component(server, "server", "info")?;
            validate_component(tool, "tool", "info")?;
            Ok(CommandSpec::Info {
                server: server.clone(),
                tool: Some(tool.clone()),
                with_descriptions,
            })
        }
        _ => Err(too_many_arguments(
            "info",
            "Use 'mcp-cli info <server> [tool]' or 'mcp-cli info <server>/<tool>'",
        )),
    }
}

fn parse_grep(args: &[String], with_descriptions: bool) -> Result<CommandSpec, CliError> {
    match args {
        [] => Err(missing_argument(
            "grep",
            "pattern",
            "Use 'mcp-cli grep <pattern>'",
        )),
        [pattern] if !pattern.is_empty() => Ok(CommandSpec::Grep {
            pattern: pattern.clone(),
            with_descriptions,
        }),
        [..] if args.len() > 1 => Err(too_many_arguments(
            "grep",
            "Use 'mcp-cli grep <pattern>' with one shell-quoted pattern",
        )),
        _ => Err(missing_argument(
            "grep",
            "pattern",
            "Use 'mcp-cli grep <pattern>'",
        )),
    }
}

fn parse_call(args: &[String]) -> Result<CommandSpec, CliError> {
    match args {
        [] => Err(missing_argument(
            "call",
            "server and tool",
            "Use 'mcp-cli call <server> <tool> [json]'",
        )),
        [target] => {
            let (server, tool) = parse_target(target, true, "call")?;
            Ok(CommandSpec::Call {
                server,
                tool: tool.expect("call target validation requires a tool"),
                inline_json: None,
            })
        }
        [target, inline_json] if target.contains('/') => {
            let (server, tool) = parse_target(target, true, "call")?;
            Ok(CommandSpec::Call {
                server,
                tool: tool.expect("call target validation requires a tool"),
                inline_json: Some(inline_json.clone()),
            })
        }
        [server, tool] if !server.contains('/') => {
            validate_component(server, "server", "call")?;
            validate_component(tool, "tool", "call")?;
            Ok(CommandSpec::Call {
                server: server.clone(),
                tool: tool.clone(),
                inline_json: None,
            })
        }
        [server, tool, inline_json] if !server.contains('/') => {
            validate_component(server, "server", "call")?;
            validate_component(tool, "tool", "call")?;
            Ok(CommandSpec::Call {
                server: server.clone(),
                tool: tool.clone(),
                inline_json: Some(inline_json.clone()),
            })
        }
        _ => Err(too_many_arguments(
            "call",
            "Use one JSON argument: 'mcp-cli call <server> <tool> [json]'",
        )),
    }
}

fn parse_target(
    target: &str,
    require_tool: bool,
    command: &str,
) -> Result<(String, Option<String>), CliError> {
    if let Some((server, tool)) = target.split_once('/') {
        validate_component(server, "server", command)?;
        validate_component(tool, "tool", command)?;
        return Ok((server.to_owned(), Some(tool.to_owned())));
    }

    validate_component(target, "server", command)?;
    if require_tool {
        return Err(missing_argument(
            command,
            "tool",
            "Use 'mcp-cli call <server> <tool> [json]' or 'mcp-cli call <server>/<tool> [json]'",
        ));
    }
    Ok((target.to_owned(), None))
}

fn validate_bare_server(server: &str) -> Result<(), CliError> {
    validate_component(server, "server", "info")?;
    if server.contains('/') {
        return Err(invalid_syntax(
            "Ambiguous target without a command",
            "A server/tool target requires an explicit info or call command",
            "Use 'mcp-cli info <server>/<tool>' or 'mcp-cli call <server>/<tool> [json]'",
        ));
    }
    Ok(())
}

fn validate_component(value: &str, component: &str, command: &str) -> Result<(), CliError> {
    if value.is_empty() {
        return Err(invalid_syntax(
            format!("Empty {component} in {command} target"),
            format!("The {component} name must not be empty"),
            format!("Run 'mcp-cli {command} --help' for valid target syntax"),
        ));
    }
    Ok(())
}

fn require_utf8(argument: OsString) -> Result<String, CliError> {
    argument.into_string().map_err(|_| {
        invalid_syntax(
            "Command argument is not valid UTF-8",
            "Commands, server names, tool names, patterns, and JSON must be UTF-8",
            "Use valid UTF-8 arguments; only -c/--config paths may contain non-UTF-8 bytes",
        )
    })
}

fn is_public_command(value: &str) -> bool {
    matches!(value, "info" | "grep" | "call")
}

fn reject_alias_or_internal(value: &str) -> Result<(), CliError> {
    if value == "__daemon" {
        return Err(CliError::from_kind(
            crate::error::ErrorKind::UnknownCommand,
            "Unknown command",
        )
        .with_details(format!("Valid commands: {PUBLIC_COMMANDS}"))
        .with_suggestion("Run 'mcp-cli --help' to see public commands"));
    }

    let replacement = match value.to_ascii_lowercase().as_str() {
        "run" | "execute" | "exec" | "invoke" => "call",
        "list" | "ls" | "get" | "show" | "describe" => "info",
        "search" | "find" | "query" => "grep",
        _ => return Ok(()),
    };

    Err(CliError::unknown_command(value)
        .with_suggestion(format!("Use 'mcp-cli {replacement}' instead")))
}

fn unknown_option(option: &str) -> CliError {
    invalid_syntax(
        format!("Unknown option: \"{option}\""),
        format!("Valid options: {PUBLIC_OPTIONS}"),
        "Run 'mcp-cli --help' to see valid public options",
    )
}

fn missing_config_path() -> CliError {
    invalid_syntax(
        "Missing required path for -c/--config",
        "The config option requires one path value",
        "Use '-c <path>' or '--config <path>'",
    )
}

fn missing_argument(command: &str, argument: &str, suggestion: &str) -> CliError {
    invalid_syntax(
        format!("Missing required argument for {command}: {argument}"),
        format!("The {command} command requires {argument}"),
        suggestion,
    )
}

fn too_many_arguments(command: &str, suggestion: &str) -> CliError {
    invalid_syntax(
        format!("Too many positional arguments for {command}"),
        "Shell-quote JSON or patterns containing spaces so they remain one argument",
        suggestion,
    )
}

fn ambiguous_command(arguments: &[String]) -> CliError {
    let first = arguments.first().map(String::as_str).unwrap_or("<server>");
    let second = arguments.get(1).map(String::as_str).unwrap_or("<tool>");
    invalid_syntax(
        "Ambiguous command syntax",
        format!("'{first} {second}' could mean tool information or a tool call"),
        format!("Use 'mcp-cli info {first} {second}' or 'mcp-cli call {first} {second} [json]'"),
    )
}

fn invalid_syntax(
    message: impl Into<String>,
    details: impl Into<String>,
    suggestion: impl Into<String>,
) -> CliError {
    CliError::invalid_arguments(message, details).with_suggestion(suggestion)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ErrorKind, ExitCode};

    fn parse(args: &[&str]) -> Result<CliInvocation, CliError> {
        parse_cli(args.iter().map(OsString::from))
    }

    fn assert_recoverable(error: &CliError) {
        assert!(matches!(
            error.kind,
            ErrorKind::InvalidArguments | ErrorKind::UnknownCommand
        ));
        assert_eq!(error.exit_code, ExitCode::Client);
        let suggestion = error.suggestion.as_deref().expect("recovery suggestion");
        assert!(!suggestion.is_empty());
        assert!(!suggestion.contains("__daemon"));
    }

    #[test]
    fn no_arguments_and_global_options_parse_as_list() {
        assert_eq!(
            parse(&[]).unwrap(),
            CliInvocation {
                command: CommandSpec::List {
                    with_descriptions: false
                },
                config_path: None,
            }
        );
        assert_eq!(
            parse(&["-d", "-c", "custom.json"]).unwrap(),
            CliInvocation {
                command: CommandSpec::List {
                    with_descriptions: true
                },
                config_path: Some(PathBuf::from("custom.json")),
            }
        );
    }

    #[test]
    fn one_server_and_explicit_info_parse_to_info() {
        let expected = CommandSpec::Info {
            server: "alpha".into(),
            tool: None,
            with_descriptions: true,
        };
        assert_eq!(parse(&["-d", "alpha"]).unwrap().command, expected);
        assert_eq!(parse(&["info", "alpha", "-d"]).unwrap().command, expected);
    }

    #[test]
    fn info_accepts_equivalent_split_and_slash_targets() {
        let split = parse(&["info", "alpha", "read_file"]).unwrap();
        let slash = parse(&["info", "alpha/read_file"]).unwrap();
        assert_eq!(split, slash);
        assert_eq!(
            split.command,
            CommandSpec::Info {
                server: "alpha".into(),
                tool: Some("read_file".into()),
                with_descriptions: false,
            }
        );
    }

    #[test]
    fn targets_split_only_at_the_first_slash() {
        assert_eq!(
            parse(&["info", "alpha/group/read"]).unwrap().command,
            CommandSpec::Info {
                server: "alpha".into(),
                tool: Some("group/read".into()),
                with_descriptions: false,
            }
        );
    }

    #[test]
    fn call_accepts_both_targets_with_optional_single_json_argument() {
        let split = parse(&["call", "alpha", "read", r#"{"path":"x"}"#]).unwrap();
        let slash = parse(&["call", "alpha/read", r#"{"path":"x"}"#]).unwrap();
        assert_eq!(split, slash);
        assert_eq!(
            split.command,
            CommandSpec::Call {
                server: "alpha".into(),
                tool: "read".into(),
                inline_json: Some(r#"{"path":"x"}"#.into()),
            }
        );
        assert_eq!(
            parse(&["call", "alpha/read"]).unwrap().command,
            CommandSpec::Call {
                server: "alpha".into(),
                tool: "read".into(),
                inline_json: None,
            }
        );
    }

    #[test]
    fn grep_and_description_flag_parse_in_either_order() {
        let expected = CommandSpec::Grep {
            pattern: "**/read?".into(),
            with_descriptions: true,
        };
        assert_eq!(
            parse(&["-d", "grep", "**/read?"]).unwrap().command,
            expected
        );
        assert_eq!(
            parse(&["grep", "**/read?", "--with-descriptions"])
                .unwrap()
                .command,
            expected
        );
    }

    #[test]
    fn help_and_version_short_and_long_forms_are_parser_outcomes() {
        for args in [["-h"], ["--help"]] {
            assert_eq!(parse(&args).unwrap().command, CommandSpec::Help);
        }
        for args in [["-v"], ["--version"]] {
            assert_eq!(parse(&args).unwrap().command, CommandSpec::Version);
        }
    }

    #[test]
    fn options_are_recognized_after_commands_and_double_dash_ends_options() {
        assert_eq!(
            parse(&["info", "alpha", "-c", "cfg.json"])
                .unwrap()
                .config_path,
            Some(PathBuf::from("cfg.json"))
        );
        assert_eq!(
            parse(&["grep", "--", "-literal"]).unwrap().command,
            CommandSpec::Grep {
                pattern: "-literal".into(),
                with_descriptions: false,
            }
        );
    }

    #[test]
    fn unknown_options_and_missing_config_values_are_recoverable() {
        for args in [
            vec!["--server", "alpha"],
            vec!["call", "alpha", "read", "--args", "{}"],
            vec!["--call"],
            vec!["-c"],
            vec!["--config", ""],
        ] {
            assert_recoverable(&parse(&args).unwrap_err());
        }
    }

    #[test]
    fn common_aliases_are_rejected_with_specific_public_replacements() {
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
            let error = parse(&[alias]).unwrap_err();
            assert_eq!(error.kind, ErrorKind::UnknownCommand);
            assert!(error.suggestion.as_deref().unwrap().contains(replacement));
            assert_recoverable(&error);
        }
    }

    #[test]
    fn empty_targets_missing_arguments_and_extra_arguments_are_rejected() {
        for args in [
            vec![""],
            vec!["info"],
            vec!["info", ""],
            vec!["info", "/tool"],
            vec!["info", "server/"],
            vec!["info", "server", ""],
            vec!["info", "server/tool", "extra"],
            vec!["info", "server", "tool", "extra"],
            vec!["grep"],
            vec!["grep", ""],
            vec!["grep", "one", "two"],
            vec!["call"],
            vec!["call", "server"],
            vec!["call", "/tool"],
            vec!["call", "server/"],
            vec!["call", "server", ""],
            vec!["call", "server/tool", "{}", "extra"],
            vec!["call", "server", "tool", "{}", "extra"],
        ] {
            assert_recoverable(&parse(&args).unwrap_err());
        }
    }

    #[test]
    fn commandless_multi_positionals_and_slash_targets_are_ambiguous() {
        for args in [
            vec!["server", "tool"],
            vec!["server", "tool", "{}"],
            vec!["server/tool"],
            vec!["server/tool", "{}"],
        ] {
            let error = parse(&args).unwrap_err();
            assert_eq!(error.kind, ErrorKind::InvalidArguments);
            assert!(error.message.contains("Ambiguous"));
            assert_recoverable(&error);
        }
    }

    #[test]
    fn internal_daemon_is_not_accepted_as_a_public_command() {
        let error = parse(&["__daemon"]).unwrap_err();
        assert_eq!(error.kind, ErrorKind::UnknownCommand);
        assert_recoverable(&error);
    }

    #[test]
    fn clap_metadata_renders_public_syntax_but_hides_internal_daemon() {
        let mut command = cli_command();
        command.clone().debug_assert();
        let help = command.render_long_help().to_string();
        assert!(help.contains("info"));
        assert!(help.contains("grep"));
        assert!(help.contains("call"));
        assert!(help.contains("-c"));
        assert!(help.contains("--config"));
        assert!(help.contains("-d"));
        assert!(help.contains("--with-descriptions"));
        assert!(help.contains("-h"));
        assert!(help.contains("--help"));
        assert!(help.contains("-v"));
        assert!(help.contains("--version"));
        assert!(!help.contains("__daemon"));
        assert_eq!(command.get_version(), Some(env!("CARGO_PKG_VERSION")));
        assert!(command.find_subcommand("__daemon").unwrap().is_hide_set());
    }

    #[test]
    fn invocation_keeps_utf8_config_paths_as_path_bufs() {
        let invocation = parse(&["--config", "configs/mcp.json"]).unwrap();
        assert_eq!(
            invocation.config_path.as_deref(),
            Some(std::path::Path::new("configs/mcp.json"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_path_preserves_non_utf8_bytes_but_other_arguments_reject_them() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let raw_path = b"config-\xFF.json".to_vec();
        let invocation =
            parse_cli([OsString::from("-c"), OsString::from_vec(raw_path.clone())]).unwrap();
        assert_eq!(
            invocation.config_path.unwrap().as_os_str().as_bytes(),
            raw_path
        );

        let error = parse_cli([OsString::from_vec(b"server-\xFF".to_vec())]).unwrap_err();
        assert_recoverable(&error);
        assert!(error.message.contains("UTF-8"));
    }

    #[test]
    fn parser_errors_only_reference_public_commands() {
        let errors = [
            parse(&["server", "tool"]).unwrap_err(),
            parse(&["server/tool"]).unwrap_err(),
            parse(&["--unknown"]).unwrap_err(),
            parse(&["call"]).unwrap_err(),
            parse(&["__daemon"]).unwrap_err(),
        ];
        for error in errors {
            let all_text = format!(
                "{} {} {}",
                error.message,
                error.details.unwrap_or_default(),
                error.suggestion.unwrap_or_default()
            );
            assert!(!all_text.contains("__daemon"));
        }
        assert_eq!(PUBLIC_COMMANDS, "info, grep, call");
    }
}
