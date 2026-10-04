//! Packaging `.desktop` entries for launcher exposure (NUKE-78).
//!
//! Hyprburst — and any XDG-compliant launcher — discovers capture actions
//! only through `.desktop` files. These tests pin the shipped entries:
//! each must parse as a desktop entry, stay discoverable, label its capture
//! mode and output destination in its name, invoke only flags the real CLI
//! accepts, and never imply upload or sharing.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// (file, expected Name, expected Exec) — names label mode + destination.
const EXPECTED: &[(&str, &str, &str)] = &[
    ("crabture.desktop", "Crabture", "crabture"),
    (
        "crabture-area-copy.desktop",
        "Crabture — Area (copy)",
        "crabture --select --copy",
    ),
    (
        "crabture-full-save.desktop",
        "Crabture — Full screen (save)",
        "crabture --instant",
    ),
];

fn packaging_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("packaging")
}

/// Flag tokens advertised by the real binary's `--help`.
fn help_flags() -> BTreeSet<String> {
    let help = Command::new(env!("CARGO_BIN_EXE_crabture"))
        .arg("--help")
        .output()
        .expect("run crabture --help");
    assert!(
        help.status.success(),
        "crabture --help exited with {}",
        help.status
    );
    let text = String::from_utf8(help.stdout).expect("help output is utf-8");
    text.split_whitespace()
        .filter(|token| token.starts_with("--"))
        .map(|token| token.trim_matches(',').to_string())
        .collect()
}

/// Minimal desktop-entry parse: `[Desktop Entry]` group of `key=value` lines.
fn parse_desktop_entry(text: &str) -> Vec<(String, String)> {
    let mut in_group = false;
    let mut pairs = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("malformed desktop-entry line: {line:?}"));
        pairs.push((key.trim().to_string(), value.trim().to_string()));
    }
    pairs
}

fn field<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

#[test]
fn shipped_desktop_entries_parse_and_label_capture_actions() {
    let flags = help_flags();
    assert!(!flags.is_empty(), "no flags parsed from --help");

    for (file, want_name, want_exec) in EXPECTED {
        let path = packaging_dir().join(file);
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing desktop entry: {}", path.display()));
        let pairs = parse_desktop_entry(&text);

        assert_eq!(
            field(&pairs, "Type"),
            Some("Application"),
            "{file}: Type must be Application"
        );
        assert_eq!(
            field(&pairs, "Name"),
            Some(*want_name),
            "{file}: name must label the capture action"
        );
        assert_eq!(
            field(&pairs, "Exec"),
            Some(*want_exec),
            "{file}: Exec must match the pinned action command"
        );

        // Stay discoverable: Hyprburst skips Hidden/NoDisplay entries.
        assert_ne!(field(&pairs, "Hidden"), Some("true"), "{file}: Hidden");
        assert_ne!(
            field(&pairs, "NoDisplay"),
            Some("true"),
            "{file}: NoDisplay"
        );

        // Exec must invoke crabture with only real CLI flags.
        let exec = field(&pairs, "Exec").expect("Exec checked above");
        let mut tokens = exec.split_whitespace();
        assert_eq!(tokens.next(), Some("crabture"), "{file}: Exec program");
        for token in tokens {
            assert!(
                token.starts_with("--"),
                "{file}: unexpected positional arg {token:?}"
            );
            assert!(
                flags.contains(token),
                "{file}: Exec flag {token} is not a real CLI flag"
            );
        }

        // No implicit upload or sharing is introduced anywhere.
        let lower = exec.to_ascii_lowercase();
        for banned in ["upload", "share", "http://", "https://"] {
            assert!(
                !lower.contains(banned),
                "{file}: Exec must not imply {banned:?}"
            );
        }
    }
}
