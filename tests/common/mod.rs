// Integration-test harness.
//
// Each test gets a pristine $HOME/.claude/projects/ and runs the
// compiled `ccaudit` binary against it. Fixtures are synthesized inline so
// the numbers the report prints are deterministic.

// `dead_code` is blanket-allowed for the module, not because anything
// here is unused, but because Cargo compiles `mod common` separately
// into every integration binary. `tests/cli.rs` and `tests/uniformity.rs`
// each pull in the whole file and use a different subset, so any helper
// is unreferenced in one of the two copies. Per-item annotations just
// spread that one fact across seven attributes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    dead_code,
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

    /// Write a JSONL file into a log root OUTSIDE the default
    /// `$HOME/.claude/projects` tree, for exercising `--logs-dir`.
    /// `root` is relative to the harness home so it stays inside the
    /// tempdir and is cleaned up with it.
    pub fn write_jsonl_at(
        &self,
        root: &str,
        slug: &str,
        session_id: &str,
        lines: &[&str],
    ) -> PathBuf {
        let dir = self.home.path().join(root).join(slug);
        std::fs::create_dir_all(&dir).expect("mk alt root");
        let path = dir.join(format!("{session_id}.jsonl"));
        std::fs::write(&path, lines.join("\n") + "\n").expect("write alt jsonl");
        path
    }

    /// Absolute path to a log root under the harness home.
    pub fn root_path(&self, root: &str) -> String {
        self.home.path().join(root).to_string_lossy().into_owned()
    }

    /// Write a Codex rollout at `~/.codex/sessions/YYYY/MM/DD/`, the
    /// dated layout Codex's scanner expects. Used to give cross-source
    /// tests a second provider with real numbers.
    pub fn write_codex_rollout(
        &self,
        ymd: (&str, &str, &str),
        name: &str,
        lines: &[&str],
    ) -> PathBuf {
        let dir = self
            .home
            .path()
            .join(".codex")
            .join("sessions")
            .join(ymd.0)
            .join(ymd.1)
            .join(ymd.2);
        std::fs::create_dir_all(&dir).expect("mk codex dir");
        let path = dir.join(format!("rollout-{name}.jsonl"));
        std::fs::write(&path, lines.join("\n") + "\n").expect("write rollout");
        path
    }

    /// Write a config file under the harness home and return its path.
    pub fn write_config(&self, name: &str, body: &str) -> String {
        let path = self.home.path().join(name);
        std::fs::write(&path, body).expect("write config");
        path.to_string_lossy().into_owned()
    }

    /// Write a JSONL file under `slug` with the given content.
    pub fn write_jsonl(&self, slug: &str, session_id: &str, lines: &[&str]) -> PathBuf {
        let dir = self.project_dir(slug);
        let path = dir.join(format!("{session_id}.jsonl"));
        let body = lines.join("\n") + "\n";
        std::fs::write(&path, body).expect("write jsonl");
        path
    }

    /// Write a JSONL file at `<project>/<rel>`, creating intermediate dirs.
    ///
    /// Claude Code nests subagent transcripts below the project dir
    /// (`<uuid>/subagents/agent-*.jsonl`), and deeper still for workflow
    /// agents. Tests use this to pin that those files are scanned.
    pub fn write_jsonl_nested(&self, slug: &str, rel: &str, lines: &[&str]) -> PathBuf {
        let path = self.project_dir(slug).join(rel);
        std::fs::create_dir_all(path.parent().expect("nested path has a parent"))
            .expect("mk nested dir");
        let body = lines.join("\n") + "\n";
        std::fs::write(&path, body).expect("write nested jsonl");
        path
    }

    /// Invoke the binary built by `cargo build --release`.
    pub fn run(&self, args: &[&str]) -> Output {
        self.base_cmd().args(args).output().expect("spawn ccaudit")
    }

    // Used by tests/cli.rs for the CCAUDIT_LAZY path.
    pub fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = self.base_cmd();
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.args(args).output().expect("spawn ccaudit")
    }

    /// Command with a scrubbed environment: pristine `$HOME` plus every
    /// env var the binary reads removed, so test outcomes don't depend on
    /// the developer's shell (e.g. `FORCE_COLOR=1` would leak ANSI escapes
    /// into the "no escapes in a pipe" assertions, and a preset `NO_COLOR`
    /// would mask a TTY-autodetect regression by making them vacuous).
    fn base_cmd(&self) -> Command {
        let mut cmd = Command::new(bin_path());
        cmd.env("HOME", self.home.path());
        // Run from the pristine home, not the repo root. The config
        // search includes `./ccaudit.json`, so a config sitting in the
        // working tree would otherwise silently steer every test — the
        // same failure mode the HOME scrub exists to prevent.
        cmd.current_dir(self.home.path());
        for k in [
            "CCAUDIT_LAZY",
            "CCAUDIT_PROF",
            "CCAUDIT_CONFIG",
            "XDG_CONFIG_HOME",
            "NO_COLOR",
            "FORCE_COLOR",
            "CCAUDIT_NO_COLOR",
            "CCAUDIT_FORCE_COLOR",
            "TERM",
        ] {
            cmd.env_remove(k);
        }
        cmd
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

/// One Codex rollout carrying a single billable turn.
///
/// `input` is the total Codex reports, of which `cached` was a cache
/// read — Codex's `input_tokens` includes cached, so the uncached column
/// ends up `input - cached`.
pub fn codex_session(id: &str, model: &str, input: u64, cached: u64, output: u64) -> Vec<String> {
    vec![
        serde_json::json!({
            "timestamp": "2026-04-01T12:00:00.000Z",
            "type": "session_meta",
            "payload": { "id": id, "cwd": "/Users/test/code/gamma" }
        })
        .to_string(),
        serde_json::json!({
            "timestamp": "2026-04-01T12:00:01.000Z",
            "type": "turn_context",
            "payload": { "model": model }
        })
        .to_string(),
        serde_json::json!({
            "timestamp": "2026-04-01T12:00:02.000Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": { "last_token_usage": {
                    "input_tokens": input,
                    "cached_input_tokens": cached,
                    "output_tokens": output
                }}
            }
        })
        .to_string(),
    ]
}

pub fn read_stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

pub fn read_stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}
