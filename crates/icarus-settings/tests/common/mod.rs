#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

pub fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

pub fn fixture(name: &str) -> String {
    std::fs::read_to_string(fixture_path(name)).expect("fixture exists")
}

pub const FIXTURES: [&str; 2] = ["full-export.json", "old-build-export.json"];

// None when node is missing, so the comparison is skipped rather than failed.
pub fn node_stringify(text: &str) -> Option<String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("input.json");
    std::fs::write(&input, text).expect("write input");
    let output = Command::new("node")
        .arg("-e")
        .arg(
            "process.stdout.write(JSON.stringify(JSON.parse(require('fs').readFileSync(process.argv[1], 'utf8')), null, 2) + '\\n')",
        )
        .arg(&input)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).expect("utf8"))
}
