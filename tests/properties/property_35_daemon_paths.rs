#![cfg(unix)]
#![forbid(unsafe_code)]

use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path},
};

use mcp_cli::{
    ServerId,
    config::{SHA256_HEX_LENGTH, server_id},
    daemon::{DaemonPathError, DaemonPaths},
};
use proptest::prelude::*;
use tempfile::TempDir;

const CASES: u32 = 128;

fn raw_server_name() -> impl Strategy<Value = String> {
    let control = prop_oneof![Just('\u{1}'), Just('\u{7}'), Just('\u{1f}'), Just('\u{7f}'),];
    let emoji = prop_oneof![
        Just("🚀🧪🔒".to_owned()),
        Just("👩🏽‍💻🌍".to_owned()),
        Just("🦀✨🧵".to_owned()),
    ];
    let multilingual = prop_oneof![
        Just("服务器-название-خادم".to_owned()),
        Just("日本語-한국어-हिन्दी".to_owned()),
        Just("Ελληνικά-עברית-中文".to_owned()),
    ];
    let long_fragment = proptest::collection::vec(
        prop_oneof![
            Just('a'),
            Just('界'),
            Just('Ж'),
            Just('م'),
            Just('🦀'),
            Just('.'),
            Just('/'),
            Just('\\'),
        ],
        300..=512,
    )
    .prop_map(|characters| characters.into_iter().collect::<String>());
    let noise = proptest::collection::vec(any::<char>(), 0..=24)
        .prop_map(|characters| characters.into_iter().collect::<String>());
    let joiner = prop_oneof![
        Just("|".to_owned()),
        Just("/\\".to_owned()),
        Just("::".to_owned()),
        Just("组合🚀".to_owned()),
        Just("\0".to_owned()),
    ];

    (
        control,
        emoji,
        multilingual,
        long_fragment,
        noise,
        joiner,
        0_usize..10,
    )
        .prop_map(
            |(control, emoji, multilingual, long_fragment, noise, joiner, rotation)| {
                let mut mandatory_parts = vec![
                    "/".to_owned(),
                    "\\".to_owned(),
                    "..".to_owned(),
                    ".".to_owned(),
                    "\0".to_owned(),
                    control.to_string(),
                    " \t\r\n\u{a0}\u{2003}".to_owned(),
                    emoji,
                    multilingual,
                    long_fragment,
                ];
                let part_count = mandatory_parts.len();
                mandatory_parts.rotate_left(rotation % part_count);
                format!(
                    "raw-server{joiner}{}{joiner}{noise}",
                    mandatory_parts.join(&joiner)
                )
            },
        )
}

fn is_lowercase_sha256_hex(value: &str) -> bool {
    value.len() == SHA256_HEX_LENGTH
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn byte_hex(value: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn percent_encode_all(value: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len() * 3);
    for byte in value.as_bytes() {
        encoded.push('%');
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn assert_single_normal_basename(path: &Path, runtime_dir: &Path) -> Result<(), TestCaseError> {
    let relative = path
        .strip_prefix(runtime_dir)
        .map_err(|error| TestCaseError::fail(format!("artifact escaped runtime dir: {error}")))?;
    let components = relative.components().collect::<Vec<_>>();
    prop_assert_eq!(components.len(), 1);
    prop_assert!(matches!(components[0], Component::Normal(_)));
    Ok(())
}

fn forged_server_ids(valid_id: &str) -> Vec<ServerId> {
    vec![
        ServerId(format!("../property35-{}", &valid_id[..16])),
        ServerId(format!("{valid_id}/child")),
        ServerId(format!("{valid_id}\\child")),
        ServerId("A".repeat(SHA256_HEX_LENGTH)),
        ServerId("a".repeat(SHA256_HEX_LENGTH - 1)),
        ServerId("a".repeat(SHA256_HEX_LENGTH + 1)),
        ServerId(format!("{}\n", "a".repeat(SHA256_HEX_LENGTH - 1))),
        ServerId(format!("{}\0", "a".repeat(SHA256_HEX_LENGTH - 1))),
        ServerId(format!("{}..", "a".repeat(SHA256_HEX_LENGTH - 2))),
        ServerId(format!(
            "{}/{}",
            "a".repeat((SHA256_HEX_LENGTH - 1) / 2),
            "b".repeat((SHA256_HEX_LENGTH - 1) / 2)
        )),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: CASES,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 35: daemon 路径恒定受限
    // **Validates: Requirements 16.1, 16.2**
    #[test]
    fn property_35_daemon_paths_remain_strictly_confined(raw_name in raw_server_name()) {
        // The production hash is the input transformation under test. All path
        // and grammar assertions below are independent filesystem/string oracles.
        let id = server_id(&raw_name);
        prop_assert!(is_lowercase_sha256_hex(&id.0));

        let isolated_root = TempDir::new().expect("isolated property runtime root");
        let canonical_root = fs::canonicalize(isolated_root.path())
            .expect("canonical isolated property runtime root");
        let paths = DaemonPaths::from_runtime_parent(isolated_root.path(), &id)
            .map_err(|error| TestCaseError::fail(format!("valid hashed ID rejected: {error}")))?;
        let expected_runtime = canonical_root.join(format!(
            "mcp-cli-{}",
            rustix::process::getuid().as_raw()
        ));

        prop_assert_eq!(&paths.runtime_dir, &expected_runtime);
        prop_assert_eq!(
            fs::canonicalize(&paths.runtime_dir).expect("canonical runtime directory"),
            expected_runtime.clone()
        );
        prop_assert_eq!(
            fs::canonicalize(paths.runtime_dir.parent().expect("runtime parent"))
                .expect("canonical runtime parent"),
            canonical_root.clone()
        );

        let root_entries = fs::read_dir(&canonical_root)
            .expect("read isolated root")
            .map(|entry| entry.expect("root entry").path())
            .collect::<Vec<_>>();
        prop_assert_eq!(root_entries, vec![expected_runtime.clone()]);

        let raw_hex = byte_hex(&raw_name);
        let raw_percent = percent_encode_all(&raw_name);
        let mut basenames = BTreeSet::new();
        let mut stems = BTreeSet::new();

        for (path, suffix) in [
            (&paths.socket, ".sock"),
            (&paths.pid, ".pid"),
            (&paths.lock, ".lock"),
        ] {
            prop_assert_eq!(path.parent(), Some(expected_runtime.as_path()));
            prop_assert_eq!(
                fs::canonicalize(path.parent().expect("artifact parent"))
                    .expect("canonical artifact parent"),
                expected_runtime.clone()
            );
            assert_single_normal_basename(path, &expected_runtime)?;

            let basename = path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("ASCII artifact basename");
            let stem = basename
                .strip_suffix(suffix)
                .expect("artifact has independently expected suffix");
            prop_assert!(is_lowercase_sha256_hex(stem));
            prop_assert_eq!(basename.len(), SHA256_HEX_LENGTH + suffix.len());
            prop_assert_eq!(stem, id.0.as_str());
            prop_assert!(!basename.contains('/'));
            prop_assert!(!basename.contains('\\'));
            prop_assert!(!basename.contains(".."));
            prop_assert!(!basename.contains(&raw_name));
            prop_assert!(!basename.contains(&raw_hex));
            prop_assert!(!basename.contains(&raw_percent));
            prop_assert!(!path.to_string_lossy().contains(&raw_name));
            prop_assert!(!path.to_string_lossy().contains(&raw_hex));
            prop_assert!(!path.to_string_lossy().contains(&raw_percent));
            prop_assert!(!path.exists(), "constructing paths must not create artifacts");

            basenames.insert(basename.to_owned());
            stems.insert(stem.to_owned());
        }

        prop_assert_eq!(basenames.len(), 3, "socket/PID/lock paths must differ");
        prop_assert_eq!(stems, BTreeSet::from([id.0.clone()]));

        // Forged identifiers are checked in a second pristine root so rejection
        // must happen before even a runtime directory or escape artifact exists.
        let forged_root = TempDir::new().expect("isolated forged-ID root");
        for forged in forged_server_ids(&id.0) {
            prop_assert!(!is_lowercase_sha256_hex(&forged.0));
            let result = DaemonPaths::from_runtime_parent(forged_root.path(), &forged);
            prop_assert!(
                matches!(result, Err(DaemonPathError::Unsafe { .. })),
                "forged server identifier was not rejected"
            );
        }
        prop_assert!(
            fs::read_dir(forged_root.path())
                .expect("read forged-ID root")
                .next()
                .is_none(),
            "rejected IDs must not create runtime or escape files"
        );
    }
}
