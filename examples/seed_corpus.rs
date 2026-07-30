//! Generate a synthetic Claude Code log corpus from a seed.
//!
//! Feeds the nightly differential run against ccusage: both tools read
//! the same generated tree, and their reports are diffed. A fresh seed
//! each night explores shapes a fixed fixture never would, and printing
//! the seed makes any run reproducible.
//!
//! ```text
//! cargo run --release --example seed_corpus -- --seed 42 --out /tmp/projects
//! ```
//!
//! The generator deliberately emits the shapes the two tools have
//! historically disagreed on:
//!
//! - streamed assistant messages: several lines sharing one
//!   `message.id`, with `output_tokens` growing and everything else
//!   repeating unchanged
//! - `cache_creation` split across the 5-minute and 1-hour TTLs
//! - assistant lines carrying `tool_use` blocks and no text
//!
//! Two shapes are deliberately left out, because both are *known*
//! divergences that would fire on every run and bury the unknown ones:
//!
//! - subagent transcripts, which ccaudit reads and ccusage does not
//! - `<synthetic>` compaction lines, whose tokens ccaudit drops entirely
//!   while ccusage counts them and prices them at zero (measured:
//!   exactly `n * 900` input and `n * 40` output apart)
//!
//! Both are covered by unit tests instead, where the expected behavior
//! can be asserted directly rather than inferred from a diff.

// A corpus generator's whole job is writing files and reporting what it
// wrote, so stdout printing is the point rather than a leak. Indexing is
// bounded by the fixed tables below.
#![allow(
    clippy::print_stdout,
    clippy::indexing_slicing,
    reason = "dev-only generator: printing is its output, and every index is into a const table"
)]

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// splitmix64 — small, fast, and identical across platforms, so a seed
/// reproduces the same corpus on a laptop and on CI.
struct Rng(u64);

impl Rng {
    const fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`. `n` is always a small literal here, so the
    /// modulo bias is far below anything the diff could notice.
    const fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    const fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

const MODELS: &[&str] = &[
    "claude-opus-4-6-20251205",
    "claude-sonnet-4-6-20251110",
    "claude-haiku-4-5-20251001",
];

const TOOLS: &[&str] = &["Bash", "Read", "Edit", "Grep", "Write"];

struct Args {
    seed: u64,
    out: PathBuf,
    projects: usize,
    sessions: usize,
    /// How many distinct calendar days sessions spread across. `u64` so
    /// it feeds the RNG's range without a signedness cast.
    days: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        seed: 1,
        out: PathBuf::from("corpus"),
        projects: 4,
        sessions: 6,
        days: 14,
    };
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        let flag = raw.get(i).map(String::as_str).unwrap_or_default();
        let val = || {
            raw.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        let num = |s: String| s.parse::<u64>().map_err(|e| format!("{flag}: {e}"));
        match flag {
            "--seed" => args.seed = num(val()?)?,
            "--out" => args.out = PathBuf::from(val()?),
            "--projects" => args.projects = num(val()?)? as usize,
            "--sessions" => args.sessions = num(val()?)? as usize,
            "--days" => args.days = num(val()?)?,
            other => return Err(format!("unknown flag {other}")),
        }
        i += 2;
    }
    Ok(args)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            println!("error: {e}");
            std::process::exit(2);
        }
    };
    match generate(&args) {
        Ok(stats) => {
            println!("seed        {}", args.seed);
            println!("out         {}", args.out.display());
            println!("projects    {}", args.projects);
            println!("sessions    {}", stats.sessions);
            println!("lines       {}", stats.lines);
            println!("api calls   {}", stats.calls);
        }
        Err(e) => {
            println!("error: {e}");
            std::process::exit(1);
        }
    }
}

#[derive(Default)]
struct Stats {
    sessions: usize,
    lines: usize,
    /// Distinct billable API calls — what a correct reader should count,
    /// as opposed to the larger number of JSONL lines they span.
    calls: usize,
}

fn generate(args: &Args) -> std::io::Result<Stats> {
    let mut rng = Rng(args.seed);
    let mut stats = Stats::default();
    fs::create_dir_all(&args.out)?;

    for p in 0..args.projects {
        // Dash-encoded project directory, the shape Claude Code writes.
        let dir = args.out.join(format!("-Users-seed-code-project{p:02}"));
        fs::create_dir_all(&dir)?;
        for s in 0..args.sessions {
            let path = dir.join(format!("{:08x}-{p:02}{s:02}.jsonl", rng.next() as u32));
            let (lines, calls) = write_session(&path, &mut rng, args.days, p, s)?;
            stats.sessions += 1;
            stats.lines += lines;
            stats.calls += calls;
        }
    }
    Ok(stats)
}

fn write_session(
    path: &Path,
    rng: &mut Rng,
    days: u64,
    project: usize,
    session: usize,
) -> std::io::Result<(usize, usize)> {
    let mut buf = String::with_capacity(16 * 1024);
    let mut lines = 0usize;
    let mut calls = 0usize;

    // Anchor every corpus at a fixed date so a seed reproduces byte for
    // byte regardless of when it runs.
    let day = 1 + rng.below(days.max(1));
    let stamp = |h: u64, m: u64| format!("2026-03-{day:02}T{h:02}:{m:02}:00.000Z");

    let _ = writeln!(
        buf,
        r#"{{"type":"summary","message":{{"content":"seeded session {project}/{session}"}}}}"#
    );
    lines += 1;

    let turns = 2 + rng.below(6);
    for t in 0..turns {
        let ts = stamp(9 + (t % 12), (t * 7) % 60);
        let _ = writeln!(
            buf,
            r#"{{"type":"user","timestamp":"{ts}","message":{{"role":"user","content":"turn {t} of session {session}"}}}}"#
        );
        lines += 1;

        let model = MODELS[rng.below(MODELS.len() as u64) as usize];
        let msg_id = format!(
            "msg_{project:02}{session:02}{t:04}_{:08x}",
            rng.next() as u32
        );

        let input = 200 + rng.below(4000);
        let cache_read = rng.below(60_000);
        let cache_create = rng.below(20_000);
        // Mix the two cache TTLs. An all-5m or all-1h corpus would never
        // exercise the tier split that reporting gets wrong.
        let one_hour = if rng.chance(60) {
            rng.below(cache_create + 1)
        } else {
            0
        };
        let five_min = cache_create - one_hour;
        let final_output = 50 + rng.below(3000);

        // A streamed message is rewritten as it arrives: same id on every
        // line, `output_tokens` climbing to its final value while input
        // and cache counts repeat unchanged. One API call, several lines.
        let chunks = if rng.chance(65) { 1 + rng.below(3) } else { 0 };
        for c in 0..chunks {
            let partial = final_output * (c + 1) / (chunks + 1);
            let _ = writeln!(
                buf,
                r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{msg_id}","role":"assistant","model":"{model}","content":[{{"type":"text","text":"partial {c}"}}],"usage":{{"input_tokens":{input},"output_tokens":{partial},"cache_read_input_tokens":{cache_read},"cache_creation_input_tokens":{cache_create},"cache_creation":{{"ephemeral_5m_input_tokens":{five_min},"ephemeral_1h_input_tokens":{one_hour}}}}}}}}}"#
            );
            lines += 1;
        }
        let _ = writeln!(
            buf,
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{msg_id}","role":"assistant","model":"{model}","content":[{{"type":"text","text":"reply {t}"}}],"usage":{{"input_tokens":{input},"output_tokens":{final_output},"cache_read_input_tokens":{cache_read},"cache_creation_input_tokens":{cache_create},"cache_creation":{{"ephemeral_5m_input_tokens":{five_min},"ephemeral_1h_input_tokens":{one_hour}}}}}}}}}"#
        );
        lines += 1;
        calls += 1;

        // Tool round trip: an assistant line whose only block is a
        // tool_use, followed by the user-shaped tool_result.
        if rng.chance(45) {
            let tool = TOOLS[rng.below(TOOLS.len() as u64) as usize];
            let _ = writeln!(
                buf,
                r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{msg_id}_tu","role":"assistant","model":"{model}","content":[{{"type":"tool_use","name":"{tool}","input":{{"command":"echo seed","description":"noop"}}}}]}}}}"#
            );
            let _ = writeln!(
                buf,
                r#"{{"type":"user","timestamp":"{ts}","message":{{"role":"user","content":[{{"type":"tool_result","content":"ok"}}]}}}}"#
            );
            lines += 2;
        }
    }

    fs::write(path, buf)?;
    Ok((lines, calls))
}
