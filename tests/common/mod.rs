// Integration-test harness.
//
// Each test gets a pristine $HOME/.claude/projects/ and runs the
// compiled `ccaudit` binary against it. Fixtures are synthesized inline so
// the numbers the report prints are deterministic.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    unused_results
)]

use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

pub struct Harness {
    pub home: TempDir,
}

impl Harness {
    pub fn new() -> Self {
        let home = TempDir::new().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".claude").join("projects"))
            .expect("mk projects dir");
        std::fs::create_dir_all(home.path().join(".claude").join("ccaudit-cache"))
            .expect("mk cache dir");
        Self { home }
    }

    pub fn project_dir(&self, slug: &str) -> PathBuf {
        let p = self.home.path().join(".claude").join("projects").join(slug);
        std::fs::create_dir_all(&p).expect("mk project dir");
        p
    }

    /// Write a JSONL file under `slug` with the given content.
    pub fn write_jsonl(&self, slug: &str, session_id: &str, lines: &[&str]) -> PathBuf {
        let dir = self.project_dir(slug);
        let path = dir.join(format!("{session_id}.jsonl"));
        let body = lines.join("\n") + "\n";
        std::fs::write(&path, body).expect("write jsonl");
        path
    }

    /// Invoke the binary built by `cargo build --release`.
    pub fn run(&self, args: &[&str]) -> Output {
        let bin = bin_path();
        Command::new(bin)
            .env("HOME", self.home.path())
            .env_remove("CCAUDIT_LAZY")
            .env_remove("CCAUDIT_PROF")
            .args(args)
            .output()
            .expect("spawn ccaudit")
    }

    pub fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let bin = bin_path();
        let mut cmd = Command::new(bin);
        cmd.env("HOME", self.home.path())
            .env_remove("CCAUDIT_LAZY")
            .env_remove("CCAUDIT_PROF");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.args(args).output().expect("spawn ccaudit")
    }
}

fn bin_path() -> PathBuf {
    // Cargo exposes the test binary's sibling via CARGO_BIN_EXE_ccaudit.
    let p = std::env::var("CARGO_BIN_EXE_ccaudit")
        .unwrap_or_else(|_| "target/release/ccaudit".to_string());
    let pb = PathBuf::from(&p);
    if pb.exists() {
        return pb;
    }
    // Fall back to looking up from the workspace root.
    let root = cargo_root();
    let rel = root.join("target").join("release").join("ccaudit");
    if rel.exists() {
        return rel;
    }
    panic!(
        "ccaudit binary not found — run `cargo build --release` first. Tried: {} and {}",
        p,
        rel.display()
    )
}

fn cargo_root() -> PathBuf {
    // Integration tests run from the crate root. Manifest dir gives us
    // that even if `cargo test` is invoked elsewhere.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// ── JSONL line builders ──
//
// Small helpers so individual tests stay readable instead of drowning in
// raw escape sequences.

pub fn summary_line(text: &str) -> String {
    let v = serde_json::json!({
        "type": "summary",
        "message": { "content": text }
    });
    v.to_string()
}

pub fn user_line(content: &str, iso_ts: &str) -> String {
    let v = serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": content },
        "timestamp": iso_ts
    });
    v.to_string()
}

pub struct AssistantLine<'a> {
    pub msg_id: &'a str,
    pub model: &'a str,
    pub iso_ts: &'a str,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_create: u64,
    pub text: &'a str,
}

pub fn assistant_line(a: &AssistantLine<'_>) -> String {
    let v = serde_json::json!({
        "type": "assistant",
        "timestamp": a.iso_ts,
        "message": {
            "id": a.msg_id,
            "role": "assistant",
            "model": a.model,
            "content": [{ "type": "text", "text": a.text }],
            "usage": {
                "input_tokens": a.input,
                "output_tokens": a.output,
                "cache_read_input_tokens": a.cache_read,
                "cache_creation_input_tokens": a.cache_create
            }
        }
    });
    v.to_string()
}

#[allow(dead_code)]
pub fn read_stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[allow(dead_code)]
pub fn read_stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}
