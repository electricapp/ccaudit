// Single source of truth contract.
//
// Every place ccaudit prints a "total cost" or "total tokens" must agree
// — CLI `daily`, CLI `session`, web's per-session sum, web's daily heatmap.
// This test exercises a synthetic corpus with the cases that have caused
// drift in the past (cross-session message-id duplicates, `<synthetic>`
// compaction, multi-model sessions) and asserts every code path lands on
// the exact same number.
//
// If a future change breaks one path, this test fires before the bug
// reaches CLI vs web visibility.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreadable_literal,
    unused_results,
    unused_qualifications
)]

mod common;

use common::*;
use serde_json::Value;
use std::path::PathBuf;
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

// Build a corpus that exercises every case that has ever caused drift:
//   - Two projects, three sessions (one is a "resume" carrying a duplicated msg_id)
//   - Multiple models in one session (mid-session model switch)
//   - A `<synthetic>` line (Claude Code's compaction pseudo-model — must be filtered)
//   - A line with no message_id (no dedup possible — must still count)
fn build_diverse_corpus(h: &Harness) {
    // Project alpha, session 1 — opus + sonnet + a no-id message.
    h.write_jsonl(
        "-Users-test-code-alpha",
        "sess_alpha_a",
        &[
            &summary_line("Alpha A"),
            &user_line("hello", "2026-04-01T10:00:00.000Z"),
            &assistant_line(&AssistantLine {
                msg_id: "msg_alpha_1",
                model: "claude-opus-4-7",
                iso_ts: "2026-04-01T10:00:01.000Z",
                input: 1000,
                output: 2000,
                cache_read: 5000,
                cache_create: 100,
                text: "first reply (opus)",
            }),
            &assistant_line(&AssistantLine {
                msg_id: "msg_alpha_2",
                model: "claude-sonnet-4-6",
                iso_ts: "2026-04-01T10:01:00.000Z",
                input: 800,
                output: 1500,
                cache_read: 3000,
                cache_create: 50,
                text: "second reply (sonnet)",
            }),
            // No message_id — never deduped, always counted.
            &assistant_line_no_id(
                "claude-opus-4-7",
                "2026-04-01T10:02:00.000Z",
                500,
                300,
                0,
                0,
                "no-id reply",
            ),
            // Synthetic compaction line — MUST be filtered everywhere.
            &assistant_line(&AssistantLine {
                msg_id: "msg_synth_1",
                model: "<synthetic>",
                iso_ts: "2026-04-01T10:03:00.000Z",
                input: 50000,
                output: 200,
                cache_read: 500000,
                cache_create: 0,
                text: "compaction",
            }),
        ],
    );

    // Project alpha, session 2 — RESUMES session 1 (carries forward msg_alpha_1).
    // The duplicated msg should count toward session 1 only (chronological dedup).
    h.write_jsonl(
        "-Users-test-code-alpha",
        "sess_alpha_b",
        &[
            &summary_line("Alpha B (resume)"),
            // Carried-forward duplicate: same msg_id as session 1.
            &assistant_line(&AssistantLine {
                msg_id: "msg_alpha_1",
                model: "claude-opus-4-7",
                iso_ts: "2026-04-02T09:00:00.000Z",
                input: 1000,
                output: 2000,
                cache_read: 5000,
                cache_create: 100,
                text: "duplicate (resumed)",
            }),
            // New message in this session.
            &assistant_line(&AssistantLine {
                msg_id: "msg_alpha_3",
                model: "claude-haiku-4-5",
                iso_ts: "2026-04-02T09:01:00.000Z",
                input: 200,
                output: 400,
                cache_read: 1000,
                cache_create: 0,
                text: "haiku reply",
            }),
        ],
    );

    // Project beta, single session.
    h.write_jsonl(
        "-Users-test-code-beta",
        "sess_beta_a",
        &[
            &summary_line("Beta A"),
            &assistant_line(&AssistantLine {
                msg_id: "msg_beta_1",
                model: "claude-opus-4-7",
                iso_ts: "2026-04-03T15:00:00.000Z",
                input: 5000,
                output: 1000,
                cache_read: 50000,
                cache_create: 1000,
                text: "beta reply",
            }),
        ],
    );
}

// Local helper: assistant line with no message_id (some early Claude Code
// builds emitted these). The common harness only has the `with-id` variant.
fn assistant_line_no_id(
    model: &str,
    iso_ts: &str,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
    text: &str,
) -> String {
    let v = serde_json::json!({
        "type": "assistant",
        "timestamp": iso_ts,
        "message": {
            "role": "assistant",
            "model": model,
            "content": [{ "type": "text", "text": text }],
            "usage": {
                "input_tokens": input,
                "output_tokens": output,
                "cache_read_input_tokens": cache_read,
                "cache_creation_input_tokens": cache_create
            }
        }
    });
    v.to_string()
}

// Pull the canonical total cost out of `<subcommand> --json`'s `totals` block.
fn cli_total_cost(h: &Harness, subcommand: &str) -> f64 {
    let out = h.run(&[subcommand, "--json"]);
    require_success(&out, &format!("ccaudit {subcommand} --json"));
    let v: Value = serde_json::from_str(&read_stdout(&out)).expect("valid JSON");
    v["totals"]["cost_usd"]
        .as_f64()
        .expect("totals.cost_usd present")
}

// Sum cost across every session emitted by web's index.json (and pull the
// daily-heatmap rollup at the same time so we can assert all three views).
fn web_totals(h: &Harness) -> (f64, f64) {
    let out_dir: PathBuf = h.home.path().join("web-out");
    let out = h.run(&["web", "--no-serve", "--out", out_dir.to_str().unwrap()]);
    require_success(&out, "ccaudit web --no-serve");

    let index_path = out_dir.join("index.json");
    let body = std::fs::read(&index_path).expect("read index.json");
    let doc: Value = serde_json::from_slice(&body).expect("valid JSON");

    let projects = doc["projects"].as_array().expect("projects array");
    let sessions_sum: f64 = projects
        .iter()
        .flat_map(|p| p["sessions"].as_array().expect("sessions array"))
        .map(|s| s["cost"].as_f64().unwrap_or(0.0))
        .sum();

    // Daily rollup column index 7 = cost (see web::DailyRow tuple shape).
    let daily_rows = doc["daily"]["rows"].as_array().expect("daily rows");
    let daily_sum: f64 = daily_rows
        .iter()
        .map(|r| r[7].as_f64().unwrap_or(0.0))
        .sum();

    (sessions_sum, daily_sum)
}

#[test]
fn cli_daily_session_and_web_all_agree_on_total_cost() {
    let h = Harness::new();
    build_diverse_corpus(&h);

    // 1. CLI daily
    let cli_daily = cli_total_cost(&h, "daily");
    // 2. CLI session
    let cli_session = cli_total_cost(&h, "session");
    // 3. Web sessions sum + 4. Web daily rollup
    let (web_sessions, web_daily) = web_totals(&h);

    let totals = [
        ("cli daily", cli_daily),
        ("cli session", cli_session),
        ("web sessions", web_sessions),
        ("web daily", web_daily),
    ];

    let baseline = cli_daily;
    for (name, val) in totals {
        let diff = (val - baseline).abs();
        assert!(
            diff < 0.005,
            "DIVERGENCE: {name} = ${val:.4}, baseline (cli daily) = ${baseline:.4}, diff ${diff:.4}\nfull: {totals:?}",
        );
    }

    // Also: total must be non-trivial (otherwise we're asserting 0 == 0).
    assert!(baseline > 0.0, "fixture produced $0 — test is meaningless");
}

#[test]
fn cli_monthly_agrees_with_daily() {
    // Same dataset, different bucketing — should sum to identical totals.
    let h = Harness::new();
    build_diverse_corpus(&h);

    let daily = cli_total_cost(&h, "daily");
    let monthly = cli_total_cost(&h, "monthly");

    assert!(
        (daily - monthly).abs() < 0.005,
        "DIVERGENCE: daily ${daily:.4} ≠ monthly ${monthly:.4}",
    );
}

// ── Edge cases ──

#[test]
fn empty_corpus_yields_zero_totals_in_every_view() {
    let h = Harness::new();
    assert!(cli_total_cost(&h, "daily").abs() < f64::EPSILON);
    assert!(cli_total_cost(&h, "session").abs() < f64::EPSILON);
    let (web_sessions, web_daily) = web_totals(&h);
    assert!(web_sessions.abs() < f64::EPSILON);
    assert!(web_daily.abs() < f64::EPSILON);
}

#[test]
fn single_message_session_agrees_across_views() {
    let h = Harness::new();
    h.write_jsonl(
        "-Users-test-code-tiny",
        "sess_tiny",
        &[&assistant_line(&AssistantLine {
            msg_id: "msg_tiny",
            model: "claude-opus-4-7",
            iso_ts: "2026-04-01T10:00:01.000Z",
            input: 100,
            output: 200,
            cache_read: 0,
            cache_create: 0,
            text: "hi",
        })],
    );

    let cli_daily = cli_total_cost(&h, "daily");
    let cli_session = cli_total_cost(&h, "session");
    let (web_sessions, web_daily) = web_totals(&h);

    let baseline = cli_daily;
    assert!(baseline > 0.0, "tiny corpus produced $0");
    for (name, val) in [
        ("cli session", cli_session),
        ("web sessions", web_sessions),
        ("web daily", web_daily),
    ] {
        assert!(
            (val - baseline).abs() < 0.005,
            "{name} ${val:.4} ≠ baseline ${baseline:.4}",
        );
    }
}

#[test]
fn all_synthetic_session_yields_zero() {
    // Guards against regressions in the skip_model filter.
    let h = Harness::new();
    h.write_jsonl(
        "-Users-test-code-synthonly",
        "sess_synthonly",
        &[
            &assistant_line(&AssistantLine {
                msg_id: "msg_s1",
                model: "<synthetic>",
                iso_ts: "2026-04-01T10:00:01.000Z",
                input: 100000,
                output: 500,
                cache_read: 1000000,
                cache_create: 0,
                text: "compaction 1",
            }),
            &assistant_line(&AssistantLine {
                msg_id: "msg_s2",
                model: "<synthetic>",
                iso_ts: "2026-04-01T10:01:01.000Z",
                input: 50000,
                output: 200,
                cache_read: 500000,
                cache_create: 0,
                text: "compaction 2",
            }),
        ],
    );

    assert!(cli_total_cost(&h, "daily").abs() < f64::EPSILON);
    assert!(cli_total_cost(&h, "session").abs() < f64::EPSILON);
    let (web_sessions, web_daily) = web_totals(&h);
    assert!(web_sessions.abs() < f64::EPSILON);
    assert!(web_daily.abs() < f64::EPSILON);
}

#[test]
fn very_long_content_doesnt_break_parser() {
    let h = Harness::new();
    let big = "x".repeat(50_000);
    h.write_jsonl(
        "-Users-test-code-long",
        "sess_long",
        &[&assistant_line(&AssistantLine {
            msg_id: "msg_long",
            model: "claude-opus-4-7",
            iso_ts: "2026-04-01T10:00:01.000Z",
            input: 1000,
            output: 2000,
            cache_read: 0,
            cache_create: 0,
            text: &big,
        })],
    );

    let cli = cli_total_cost(&h, "daily");
    let (web_sessions, web_daily) = web_totals(&h);
    assert!(cli > 0.0);
    assert!((cli - web_sessions).abs() < 0.005);
    assert!((cli - web_daily).abs() < 0.005);
}
