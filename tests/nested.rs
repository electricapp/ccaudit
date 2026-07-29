// Nested session discovery + project filter resolution.
//
// Claude Code nests subagent transcripts at
// `<project>/<uuid>/subagents/agent-*.jsonl` and workflow agents deeper.
// Cost math: opus $5/M in, $25/M out → 1000 in + 2000 out = $0.0550.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    unused_results,
    unused_qualifications
)]

mod common;

use common::*;
use serde_json::Value;
use std::process::Output;

fn require_success(out: &Output, ctx: &str) {
    assert!(
        out.status.success(),
        "{ctx} failed: exit={:?}\nSTDOUT:\n{}\nSTDERR:\n{}",
        out.status.code(),
        read_stdout(out),
        read_stderr(out)
    );
}

fn require_usage_error(out: &Output, ctx: &str) -> String {
    assert_eq!(
        out.status.code(),
        Some(2),
        "{ctx}: expected exit 2, got {:?}\nSTDERR:\n{}",
        out.status.code(),
        read_stderr(out)
    );
    read_stderr(out)
}

fn reply(msg_id: &str, ts: &str, input: u64, output: u64) -> String {
    assistant_line(&AssistantLine {
        msg_id,
        model: "claude-opus-4-7",
        iso_ts: ts,
        input,
        output,
        cache_read: 0,
        cache_create: 0,
        text: "reply",
    })
}

/// One project, same content at all three real layouts: flat, subagent
/// (depth 4), workflow agent (depth 6).
fn setup_nested_project(h: &Harness) {
    h.write_jsonl(
        "-Users-test-code-alpha",
        "sess_flat",
        &[
            &user_line("hello", "2026-04-01T12:00:00.000Z"),
            &reply("msg_flat", "2026-04-01T12:00:01.000Z", 1000, 2000),
        ],
    );
    h.write_jsonl_nested(
        "-Users-test-code-alpha",
        "sess_flat/subagents/agent-deadbeef.jsonl",
        &[
            &user_line("subtask", "2026-04-01T12:05:00.000Z"),
            &reply("msg_sub", "2026-04-01T12:05:01.000Z", 1000, 2000),
        ],
    );
    h.write_jsonl_nested(
        "-Users-test-code-alpha",
        "sess_flat/subagents/workflows/wf_1/agent-cafe.jsonl",
        &[
            &user_line("wf task", "2026-04-01T12:06:00.000Z"),
            &reply("msg_wf", "2026-04-01T12:06:01.000Z", 1000, 2000),
        ],
    );
}

#[test]
fn nested_subagent_transcripts_are_counted() {
    let h = Harness::new();
    setup_nested_project(&h);
    let out = h.run(&["daily", "--json"]);
    require_success(&out, "daily --json");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();

    let cost = v["totals"]["cost_usd"].as_f64().unwrap();
    assert!(
        (cost - 0.1650).abs() < 1e-9,
        "3 transcripts x $0.0550; a flat-only scan yields $0.0550. got {cost}"
    );
    assert_eq!(v["totals"]["total_tokens"].as_u64().unwrap(), 9000);
}

#[test]
fn nested_sessions_join_their_real_project() {
    let h = Harness::new();
    setup_nested_project(&h);
    let out = h.run(&["session", "--json"]);
    require_success(&out, "session --json");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = v["rows"].as_array().unwrap();

    assert_eq!(rows.len(), 1, "one project expected, got {rows:?}");
    let projects: Vec<&str> = rows[0]["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert_eq!(projects, vec!["code/alpha"]);
    // Grouping by the file's parent would invent `subagents` / `wf_1`.
    let key = rows[0]["key"].as_str().unwrap();
    for leaked in ["subagents", "wf_1", "sess_flat"] {
        assert!(!key.contains(leaked), "{leaked} leaked into key {key:?}");
    }
}

#[test]
fn nested_discovery_survives_a_warm_cache() {
    let h = Harness::new();
    setup_nested_project(&h);
    let cold = h.run(&["daily", "--json"]);
    require_success(&cold, "cold run");
    let warm = h.run(&["daily", "--json"]);
    require_success(&warm, "warm run");

    let a: Value = serde_json::from_slice(&cold.stdout).unwrap();
    let b: Value = serde_json::from_slice(&warm.stdout).unwrap();
    assert_eq!(a["totals"]["cost_usd"], b["totals"]["cost_usd"]);
}

#[test]
fn files_past_the_depth_cap_are_not_scanned() {
    // The cap terminates the walk — read_dir follows symlinks, so a cycle
    // would otherwise recurse forever.
    let h = Harness::new();
    setup_nested_project(&h);
    h.write_jsonl_nested(
        "-Users-test-code-alpha",
        "a/b/c/d/e/f/g/h/i/too-deep.jsonl",
        &[
            &user_line("deep", "2026-04-01T12:07:00.000Z"),
            &reply("msg_deep", "2026-04-01T12:07:01.000Z", 1000, 2000),
        ],
    );
    let out = h.run(&["daily", "--json"]);
    require_success(&out, "daily --json");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let cost = v["totals"]["cost_usd"].as_f64().unwrap();
    assert!((cost - 0.1650).abs() < 1e-9, "got {cost}");
}

#[test]
fn unknown_project_filter_is_rejected_not_silently_widened() {
    // u16::MAX is a real stored project_id ("no project"), so reusing it
    // as "matched nothing" made an unknown --project match exactly those.
    let h = Harness::new();
    setup_nested_project(&h);
    let out = h.run(&["daily", "--project", "definitely-not-a-project"]);
    let stderr = require_usage_error(&out, "unknown --project");
    assert!(stderr.contains("unknown project"), "got: {stderr}");
}

#[test]
fn project_filter_accepts_stored_and_stem_forms() {
    let h = Harness::new();
    setup_nested_project(&h);
    for name in ["code/alpha", "code-alpha"] {
        let out = h.run(&["daily", "--json", "--project", name]);
        require_success(&out, name);
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        let cost = v["totals"]["cost_usd"].as_f64().unwrap();
        assert!((cost - 0.1650).abs() < 1e-9, "--project {name} got {cost}");
    }
}

#[test]
fn statusline_scopes_to_a_stem_form_project() {
    // statusline only ever reports today + the active block, so the
    // fixture has to be stamped now rather than at a fixed date.
    let now = chrono::Utc::now() - chrono::Duration::minutes(1);
    let ts = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();

    let h = Harness::new();
    h.write_jsonl(
        "-Users-test-code-alpha",
        "sess_alpha",
        &[
            &user_line("hi", &ts),
            &reply("msg_alpha", &ts, 1_000_000, 0),
        ],
    );
    h.write_jsonl(
        "-Users-test-code-beta",
        "sess_beta",
        &[&user_line("hi", &ts), &reply("msg_beta", &ts, 9_000_000, 0)],
    );

    let all = h.run(&["statusline"]);
    require_success(&all, "statusline");
    let scoped = h.run(&["statusline", "--project", "code-alpha"]);
    require_success(&scoped, "statusline --project code-alpha");

    let all_s = read_stdout(&all);
    let scoped_s = read_stdout(&scoped);
    assert!(all_s.contains("10_000_000"), "unscoped total: {all_s}");
    assert!(scoped_s.contains("1_000_000"), "scoped total: {scoped_s}");
    assert_ne!(
        all_s, scoped_s,
        "a stem-form --project must scope statusline"
    );
}
