#![forbid(unsafe_code)]

use std::{fs, path::Path};

use mcp_cli::{CliError, ErrorKind, ExitCode};

struct ReferenceArea {
    source: &'static str,
    targets: &'static [&'static str],
}

const REFERENCE_AREAS: &[ReferenceArea] = &[
    ReferenceArea {
        source: "config.test.ts",
        targets: &["config"],
    },
    ReferenceArea {
        source: "filter.test.ts",
        targets: &[
            "property_09_tool_filter",
            "property_10_filter_authorization",
        ],
    },
    ReferenceArea {
        source: "output.test.ts",
        targets: &["output", "property_26_result_roundtrip"],
    },
    ReferenceArea {
        source: "errors.test.ts",
        targets: &[
            "reference_compatibility",
            "property_29_exit_codes",
            "property_30_suggestions",
        ],
    },
    ReferenceArea {
        source: "grep.test.ts",
        targets: &["property_11_search_pattern", "commands"],
    },
    ReferenceArea {
        source: "client.test.ts",
        targets: &["runtime_retry", "transport_contract"],
    },
    ReferenceArea {
        source: "cli-errors.test.ts",
        targets: &["cli_syntax", "direct_cli"],
    },
    ReferenceArea {
        source: "integration/cli.test.ts",
        targets: &["cli_end_to_end", "daemon_spawn"],
    },
];

// These are deliberate requirements-first corrections to reference behavior,
// not accidental compatibility gaps. Each behavior names an explicit target
// that executes the regression without network or user configuration.
const REQUIRED_RUST_CORRECTIONS: &[(&str, &str)] = &[
    (
        "complete ToolResult JSON instead of text extraction",
        "output",
    ),
    ("tool schema is one parseable JSON value", "direct_cli"),
    (
        "diagnostics and structured errors stay off stdout",
        "cli_syntax",
    ),
    (
        "canonical client/tool/network/auth exit codes",
        "direct_cli",
    ),
    (
        "invalid runtime values are rejected instead of ignored",
        "runtime_retry",
    ),
    (
        "daemon bootstrap configuration is transferred via stdin",
        "daemon_spawn",
    ),
    (
        "configured secrets are absent from visible errors",
        "http_transport",
    ),
];

fn explicit_test_targets(manifest: &str) -> Vec<(&str, &str)> {
    let mut targets = Vec::new();
    for table in manifest.split("[[test]]").skip(1) {
        let mut name = None;
        let mut path = None;
        for line in table.lines() {
            let line = line.trim();
            if let Some(value) = line.strip_prefix("name = \"") {
                name = value.strip_suffix('"');
            } else if let Some(value) = line.strip_prefix("path = \"") {
                path = value.strip_suffix('"');
            }
            if name.is_some() && path.is_some() {
                break;
            }
        }
        if let (Some(name), Some(path)) = (name, path) {
            targets.push((name, path));
        }
    }
    targets
}

#[test]
fn every_reference_area_and_required_correction_has_explicit_rust_targets() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = fs::read_to_string(root.join("Cargo.toml")).expect("read Cargo.toml");
    let targets = explicit_test_targets(&manifest);

    for area in REFERENCE_AREAS {
        assert!(
            !area.targets.is_empty(),
            "{} has no mapped Rust target",
            area.source
        );
        for target in area.targets {
            let (_, path) = targets
                .iter()
                .find(|(name, _)| name == target)
                .unwrap_or_else(|| {
                    panic!(
                        "{} maps to undeclared cargo test target {target}",
                        area.source
                    )
                });
            assert!(
                root.join(path).is_file(),
                "{} target {target} points to missing {path}",
                area.source
            );
        }
    }

    for (behavior, target) in REQUIRED_RUST_CORRECTIONS {
        assert!(
            targets.iter().any(|(name, _)| name == target),
            "required Rust correction {behavior:?} has no explicit target {target}"
        );
    }
}

#[test]
fn typed_candidate_errors_are_bounded_deterministic_and_actionable() {
    let no_servers = CliError::server_not_found("missing", Vec::<String>::new());
    assert_eq!(no_servers.kind, ErrorKind::ServerNotFound);
    assert_eq!(no_servers.exit_code, ExitCode::Client);
    assert_eq!(
        no_servers.details.as_deref(),
        Some("Available servers: (none)")
    );
    assert!(
        no_servers
            .suggestion
            .as_deref()
            .is_some_and(|suggestion| suggestion.contains("Add server"))
    );

    let missing_tool = CliError::tool_not_found(
        "github",
        "unknown",
        ["t8", "t4", "t2", "t7", "t1", "t6", "t3", "t5"],
    );
    assert_eq!(missing_tool.kind, ErrorKind::ToolNotFound);
    assert_eq!(missing_tool.exit_code, ExitCode::Client);
    assert_eq!(
        missing_tool.details.as_deref(),
        Some("Available tools: t1, t2, t3, t4, t5 (+3 more)")
    );
    assert!(
        missing_tool
            .suggestion
            .as_deref()
            .is_some_and(|suggestion| suggestion.contains("mcp-cli info github"))
    );
}
