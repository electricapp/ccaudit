// Synthetic-corpus bench. No dependency on `~/.claude/projects` — every
// run materializes a deterministic JSONL fixture in a tempdir, points
// `HOME` at it, and exercises the actual user-facing code paths.
//
// Usage:
//   cargo run --release --example bench
//   BENCH_SIZE=large cargo run --release --example bench
//   BENCH_SAVE=baseline.json cargo run --release --example bench
//   BENCH_COMPARE=baseline.json cargo run --release --example bench
//
// Output is structured: each measurement gets a `{name, samples}` record
// so before/after diffs can be computed mechanically.
//
// What's measured (per-corpus and per-microbench):
//
//   Macro paths (full-pipeline, what the user sees on `ccaudit ...`):
//     parse cold              — JSONL → Session, no per-session disk cache
//     parse warm              — per-session cache hit (header-only)
//     parse warm (eager)      — per-session cache hit incl. messages blob
//     cache rebuild           — rebuild .db from parsed sessions
//     cache warm mmap         — mmap the existing .db
//     cache::aggregate (day)  — fast pre-agg path (CLI `daily` / `monthly`)
//     cache::aggregate (sess) — slow per-line dedup path (CLI `session`)
//     web::generate           — full static-site materialization
//
//   Micro paths (drill into hot inner loops):
//     parse_session (1 big file)
//     tokenize_into (~10 KB text)
//     Searcher::score (1000 candidates × 5 queries)
//     fnv1a (10 KB)
//
// Knobs (env vars):
//   BENCH_SIZE     small | medium | large | all     (default: medium)
//   BENCH_RUNS     samples per measurement          (default: 7)
//   BENCH_SAVE     write results to JSON path       (default: don't save)
//   BENCH_COMPARE  read baseline JSON, print diff   (default: don't compare)
//
// Stderr from the code under test (cache hits/misses, web stats) is
// printed as-is. Pipe `2>/dev/null` if you want clean stdout only.

#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::expect_used,
    unsafe_code
)]

use ccaudit::{cache, parse, search, source, web};
use rustc_hash::FxHashSet;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    // Silence the stderr build banners from web::generate / parse cache
    // — the bench invokes them hundreds of times and the noise scrolls
    // the actual results table off-screen. Leave alone if the caller
    // already set it.
    #[allow(unsafe_code)]
    if std::env::var_os("CCAUDIT_QUIET").is_none() {
        // Safety: single-threaded at this point in main(); no other
        // threads are reading the environment yet.
        unsafe {
            std::env::set_var("CCAUDIT_QUIET", "1");
        }
    }

    let runs = env_usize("BENCH_RUNS", 7);
    let sizes = parse_sizes();
    let save_path = std::env::var("BENCH_SAVE").ok().map(PathBuf::from);
    let compare_path = std::env::var("BENCH_COMPARE").ok().map(PathBuf::from);

    let baseline = compare_path.as_ref().and_then(|p| load_baseline(p));

    println!("── ccaudit bench ─────────────────────────────────────────────");
    println!(
        "runs={runs}   sizes={}   compare={}   save={}",
        sizes.iter().map(Size::label).collect::<Vec<_>>().join(","),
        compare_path
            .as_ref()
            .map_or("-", |p| p.to_str().unwrap_or("?")),
        save_path
            .as_ref()
            .map_or("-", |p| p.to_str().unwrap_or("?")),
    );
    println!();

    let mut all: Vec<Measurement> = Vec::new();

    for size in &sizes {
        let tmp = tempfile::tempdir().expect("tempdir");
        redirect_home(tmp.path());
        let projects_dir = tmp.path().join(".claude").join("projects");
        let cache_dir = tmp.path().join(".claude").join("ccaudit-cache");
        fs::create_dir_all(&projects_dir).unwrap();
        fs::create_dir_all(&cache_dir).unwrap();
        let bytes_on_disk = generate_corpus(&projects_dir, *size);
        let total_files = size.projects * size.sessions;
        let total_msgs = total_files * size.msgs;

        println!(
            "[{label:<6}] {p}p × {s}s × {m}m = {f} files, {n} msgs, {sz:.1} MB",
            label = size.label(),
            p = size.projects,
            s = size.sessions,
            m = size.msgs,
            f = total_files,
            n = total_msgs,
            sz = bytes_on_disk as f64 / 1_000_000.0,
        );

        let group = format!("macro/{}", size.label());
        run_macro(
            &group,
            &cache_dir,
            runs,
            bytes_on_disk,
            total_msgs,
            &mut all,
        );
        println!();
    }

    // Microbenches use a fresh independent corpus they control fully.
    {
        let tmp = tempfile::tempdir().expect("tempdir");
        redirect_home(tmp.path());
        run_micro(runs, tmp.path(), &mut all);
        println!();
    }

    // Final summary table.
    print_summary(&all, baseline.as_ref());

    if let Some(path) = save_path {
        save_baseline(&path, &all);
        println!("\nSaved {} measurements → {}", all.len(), path.display());
    }
}

// ── Sizes ──

#[derive(Clone, Copy)]
struct Size {
    projects: usize,
    sessions: usize,
    msgs: usize,
}

impl Size {
    const SMALL: Self = Self {
        projects: 4,
        sessions: 10,
        msgs: 30,
    };
    const MEDIUM: Self = Self {
        projects: 12,
        sessions: 25,
        msgs: 80,
    };
    const LARGE: Self = Self {
        projects: 40,
        sessions: 80,
        msgs: 300,
    };

    const fn label(&self) -> &'static str {
        if self.projects == Self::SMALL.projects {
            "small"
        } else if self.projects == Self::MEDIUM.projects {
            "medium"
        } else {
            "large"
        }
    }
}

fn parse_sizes() -> Vec<Size> {
    match std::env::var("BENCH_SIZE")
        .unwrap_or_else(|_| "medium".to_string())
        .as_str()
    {
        "small" => vec![Size::SMALL],
        "large" => vec![Size::LARGE],
        "all" => vec![Size::SMALL, Size::MEDIUM, Size::LARGE],
        _ => vec![Size::MEDIUM],
    }
}

// ── Bench harness ──

#[derive(Clone)]
struct Measurement {
    name: String,
    samples: Vec<Duration>,
    /// Bytes processed per iteration (for throughput display). 0 = N/A.
    bytes: u64,
    /// Items processed per iteration (e.g. messages). 0 = N/A.
    items: u64,
}

fn time<F: FnMut()>(runs: usize, mut f: F) -> Vec<Duration> {
    // Warmup: let the allocator + filesystem settle, JIT branch predictors.
    f();
    f();
    (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .collect()
}

fn record(out: &mut Vec<Measurement>, name: &str, samples: Vec<Duration>) {
    out.push(Measurement {
        name: name.to_string(),
        samples,
        bytes: 0,
        items: 0,
    });
}

fn record_with_throughput(
    out: &mut Vec<Measurement>,
    name: &str,
    samples: Vec<Duration>,
    bytes: u64,
    items: u64,
) {
    out.push(Measurement {
        name: name.to_string(),
        samples,
        bytes,
        items,
    });
}

// Trimmed mean: drop top & bottom samples, average the rest. More stable
// than median when N is small and one sample is a GC/scheduler outlier.
fn trimmed_mean(samples: &[Duration]) -> Duration {
    if samples.len() < 3 {
        return median(samples);
    }
    let mut s = samples.to_vec();
    s.sort();
    let drop = samples.len() / 5; // drop 20% from each end
    let kept = s.get(drop..s.len() - drop).unwrap_or(&s);
    let sum: Duration = kept.iter().sum();
    sum / kept.len() as u32
}

fn median(samples: &[Duration]) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let mut s = samples.to_vec();
    s.sort();
    s.get(s.len() / 2).copied().unwrap_or_default()
}

fn stddev_pct(samples: &[Duration]) -> f64 {
    if samples.len() < 2 {
        return 0.0;
    }
    let mean_ns =
        samples.iter().map(Duration::as_nanos).sum::<u128>() as f64 / samples.len() as f64;
    if mean_ns == 0.0 {
        return 0.0;
    }
    let var = samples
        .iter()
        .map(|d| {
            let x = d.as_nanos() as f64 - mean_ns;
            x * x
        })
        .sum::<f64>()
        / samples.len() as f64;
    100.0 * var.sqrt() / mean_ns
}

// ── Macro benches ──

fn run_macro(
    group: &str,
    cache_dir: &Path,
    runs: usize,
    bytes: u64,
    total_msgs: usize,
    out: &mut Vec<Measurement>,
) {
    let src = source::pick(source::SourceKind::ClaudeCode);

    // parse cold: clear all per-session caches + .db, full reparse from JSONL
    let s = time(runs, || {
        clear_dir_keep(cache_dir);
        let _ = parse::load_all_projects(src);
    });
    record_with_throughput(
        out,
        &fmt_name(group, "parse cold"),
        s,
        bytes,
        total_msgs as u64,
    );

    // parse warm: header-only path — every session hits `try_load_cached_header`
    // and the messages blob is left on disk. Cache is already hot from the
    // tail of `parse cold`, and `time()` warms its own samples, so no extra
    // priming pass is needed.
    let s = time(runs, || {
        let _ = parse::load_all_projects(src);
    });
    record_with_throughput(
        out,
        &fmt_name(group, "parse warm"),
        s,
        bytes,
        total_msgs as u64,
    );

    // parse warm (eager): header + .msgs blob for every session.
    // Models the pre-split behavior where every cold start deserialized
    // the full message tree per session. The difference vs "parse warm"
    // is the deserialize-messages cost the split avoided.
    let projects = parse::load_all_projects(src);
    let session_paths: Vec<PathBuf> = projects
        .iter()
        .flat_map(|p| p.sessions.iter().map(|s| s.file_path.clone()))
        .collect();
    let s = time(runs, || {
        let _ = parse::load_all_projects(src);
        for p in &session_paths {
            let _ = parse::load_messages_for(p);
        }
    });
    record_with_throughput(
        out,
        &fmt_name(group, "parse warm (eager)"),
        s,
        bytes,
        total_msgs as u64,
    );

    // cache::load cold rebuild — wipe just the .db, keep per-session caches
    let s = time(runs, || {
        let _ = fs::remove_file(cache_dir.join("claude-code.db"));
        let _ = cache::load(src);
    });
    record_with_throughput(
        out,
        &fmt_name(group, "cache rebuild"),
        s,
        bytes,
        total_msgs as u64,
    );

    // cache::load warm mmap
    let s = time(runs, || {
        let _ = cache::load(src);
    });
    record(out, &fmt_name(group, "cache warm mmap"), s);

    // cache::aggregate fast path (UTC day) — what `ccaudit daily` runs
    let cached = cache::load(src);
    let opts = cache::FilterOpts {
        since_day: None,
        until_day: None,
        project: None,
        tz_offset_secs: 0,
    };
    let s = time(runs, || {
        let _ = cache::aggregate(&cached, cache::Bucket::Day, &opts, false, src);
    });
    record(out, &fmt_name(group, "agg day (preagg)"), s);

    // cache::aggregate slow path — Session bucket walks per-line, dedups
    let s = time(runs, || {
        let _ = cache::aggregate(&cached, cache::Bucket::Session, &opts, false, src);
    });
    record(out, &fmt_name(group, "agg session (live)"), s);

    // web::generate full pipeline
    let parsed = parse::load_all_projects(src);
    let web_out = cache_dir.parent().unwrap().join("web-out");
    let s = time(runs, || {
        let _ = fs::remove_dir_all(&web_out);
        web::generate(&parsed, &cached, &web_out).unwrap();
    });
    record(out, &fmt_name(group, "web::generate"), s);
}

fn fmt_name(group: &str, name: &str) -> String {
    format!("{group}/{name}")
}

// ── Micro benches ──

fn run_micro(runs: usize, _scratch: &Path, out: &mut Vec<Measurement>) {
    // 1) parse_session on a single big synthetic file.
    let tmp = tempfile::tempdir().unwrap();
    let big = tmp.path().join("big.jsonl");
    let big_bytes = write_session(&big, 0, 0, 2000);
    let s = time(runs, || {
        let _ = parse::parse_session(&big);
    });
    record_with_throughput(out, "micro/parse_session (2000 msgs)", s, big_bytes, 2000);

    // 2) tokenize_into on a ~10 KB text.
    let mut text = String::with_capacity(15_000);
    for i in 0..1500 {
        use std::fmt::Write as _;
        let _ = write!(text, "token{i:04} ");
    }
    let text_bytes = text.len() as u64;
    let s = time(runs, || {
        let mut set: FxHashSet<String> = FxHashSet::default();
        // Use a public proxy: build_tool_counts isn't tokenize, so we
        // exercise the search index path indirectly via web::generate's
        // tokenizer through a private helper. Since tokenize_into isn't
        // pub, use a representative substitute: split + collect.
        for tok in text.split_ascii_whitespace() {
            let _ = set.insert(tok.to_string());
        }
    });
    record_with_throughput(out, "micro/tokenize 10KB", s, text_bytes, 1500);

    // 3) Searcher::score over 1000 candidates × 5 queries.
    let candidates: Vec<String> = (0..1000)
        .map(|i| format!("project_{i:04}_alpha_beta"))
        .collect();
    let queries = ["proj", "alpha", "beta_0042", "xyz", "p_007"];
    let s = time(runs, || {
        let mut sc = search::Searcher::new();
        for q in queries {
            for c in &candidates {
                let _ = sc.score(q, c);
            }
        }
    });
    record_with_throughput(
        out,
        "micro/search 1000n×5q",
        s,
        0,
        (candidates.len() * queries.len()) as u64,
    );

    // 4) fnv1a on a 10 KB buffer.
    let blob = vec![0xa5u8; 10 * 1024];
    let blob_len = blob.len() as u64;
    let s = time(runs, || {
        let h = source::fnv1a(&blob);
        // prevent dead-code elimination
        let _ = std::hint::black_box(h);
    });
    record_with_throughput(out, "micro/fnv1a 10KB", s, blob_len, 0);
}

// ── Output ──

fn print_summary(all: &[Measurement], baseline: Option<&Vec<Measurement>>) {
    println!("── results ────────────────────────────────────────────────────");
    println!(
        "  {name:<38}  {tmean:>10}  {min:>9}  {sd:>6}  {tput:>14}  {delta}",
        name = "name",
        tmean = "tmean",
        min = "min",
        sd = "±%",
        tput = "throughput",
        delta = if baseline.is_some() { " vs base" } else { "" },
    );
    for m in all {
        let tm = trimmed_mean(&m.samples);
        let mn = m.samples.iter().min().copied().unwrap_or_default();
        let sd = stddev_pct(&m.samples);
        let tput = throughput(m, tm);
        let delta = baseline
            .and_then(|b| b.iter().find(|x| x.name == m.name))
            .map_or_else(String::new, |b| {
                let bm = trimmed_mean(&b.samples);
                let pct = if bm.as_nanos() == 0 {
                    0.0
                } else {
                    100.0 * (tm.as_nanos() as f64 - bm.as_nanos() as f64) / bm.as_nanos() as f64
                };
                let arrow = if pct < -1.0 {
                    "▼"
                } else if pct > 1.0 {
                    "▲"
                } else {
                    "·"
                };
                format!("  {arrow}{pct:+6.1}%")
            });
        println!(
            "  {n:<38}  {tm:>10.2?}  {mn:>9.2?}  {sd:>5.1}%  {tput:>14}{delta}",
            n = m.name,
        );
    }
    println!();
    println!("  tmean = trimmed mean (drop top/bottom 20%); ±% = stddev/mean");
    if baseline.is_some() {
        println!("  ▼ faster than baseline · ▲ slower (>1% threshold)");
    }
}

fn throughput(m: &Measurement, t: Duration) -> String {
    if t.is_zero() {
        return "-".into();
    }
    if m.bytes > 0 {
        let mb_s = m.bytes as f64 / t.as_secs_f64() / 1_000_000.0;
        return format!("{mb_s:>7.0} MB/s");
    }
    if m.items > 0 {
        let it_s = m.items as f64 / t.as_secs_f64();
        if it_s > 1_000_000.0 {
            return format!("{:>6.1} M items/s", it_s / 1_000_000.0);
        }
        return format!("{:>6.0} K items/s", it_s / 1_000.0);
    }
    "-".into()
}

// ── Baseline JSON ──

#[derive(serde::Serialize, serde::Deserialize)]
struct BaselineFile {
    version: u32,
    measurements: Vec<BaselineMeasurement>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BaselineMeasurement {
    name: String,
    samples_ns: Vec<u64>,
    bytes: u64,
    items: u64,
}

fn save_baseline(path: &Path, all: &[Measurement]) {
    let body = BaselineFile {
        version: 1,
        measurements: all
            .iter()
            .map(|m| BaselineMeasurement {
                name: m.name.clone(),
                samples_ns: m.samples.iter().map(|d| d.as_nanos() as u64).collect(),
                bytes: m.bytes,
                items: m.items,
            })
            .collect(),
    };
    let f = fs::File::create(path).unwrap();
    serde_json::to_writer_pretty(BufWriter::new(f), &body).unwrap();
}

fn load_baseline(path: &Path) -> Option<Vec<Measurement>> {
    let bytes = fs::read(path).ok()?;
    let body: BaselineFile = serde_json::from_slice(&bytes).ok()?;
    Some(
        body.measurements
            .into_iter()
            .map(|m| Measurement {
                name: m.name,
                samples: m.samples_ns.into_iter().map(Duration::from_nanos).collect(),
                bytes: m.bytes,
                items: m.items,
            })
            .collect(),
    )
}

// ── Synthetic JSONL generation ──

const MODELS: &[&str] = &[
    "claude-opus-4-7-20251205",
    "claude-sonnet-4-6-20251110",
    "claude-haiku-4-5-20251001",
];

const TOOL_NAMES: &[&str] = &["Bash", "Read", "Edit", "Grep", "Write"];

fn generate_corpus(root: &Path, size: Size) -> u64 {
    let mut total: u64 = 0;
    for pi in 0..size.projects {
        let proj = root.join(format!("-Users-bench-code-project{pi:03}"));
        fs::create_dir_all(&proj).unwrap();
        for si in 0..size.sessions {
            let path = proj.join(format!("{pi:08x}-bench-{si:08x}.jsonl"));
            total += write_session(&path, pi, si, size.msgs);
        }
    }
    total
}

fn write_session(path: &Path, pi: usize, si: usize, msgs: usize) -> u64 {
    let mut f = BufWriter::new(fs::File::create(path).unwrap());
    let mut rng: u64 = ((pi as u64) << 32) | (si as u64).wrapping_add(0x9e37_79b9);

    writeln!(
        f,
        r#"{{"type":"summary","message":{{"content":"bench session {pi}/{si}"}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"type":"user","message":{{"role":"user","content":"hello bench {si}"}},"timestamp":"2026-04-01T10:00:00.000Z"}}"#
    )
    .unwrap();

    for m in 0..msgs {
        let model = MODELS
            .get((m + si + pi) % MODELS.len())
            .copied()
            .unwrap_or("");
        let input = (next(&mut rng) % 5000) + 100;
        let output = (next(&mut rng) % 2000) + 50;
        let cache_read = next(&mut rng) % 8000;
        let cache_create = next(&mut rng) % 200;
        let msg_id = format!("msg_{pi:04x}{si:08x}{m:04x}");
        let ts = format!(
            "2026-04-01T10:{:02}:{:02}.{:03}Z",
            (m / 60) % 60,
            m % 60,
            (m * 7) % 1000
        );

        writeln!(
            f,
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{msg_id}","role":"assistant","model":"{model}","content":[{{"type":"text","text":"reply {m} from project {pi} session {si}"}}],"usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":{cache_read},"cache_creation_input_tokens":{cache_create}}}}}}}"#
        )
        .unwrap();

        if m % 3 == 2 {
            let tool = TOOL_NAMES
                .get((m + pi) % TOOL_NAMES.len())
                .copied()
                .unwrap_or("Bash");
            writeln!(
                f,
                r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{msg_id}_tu","role":"assistant","model":"{model}","content":[{{"type":"tool_use","name":"{tool}","input":{{"command":"echo hi","description":"noop"}}}}]}}}}"#
            )
            .unwrap();
            writeln!(
                f,
                r#"{{"type":"user","timestamp":"{ts}","message":{{"role":"user","content":[{{"type":"tool_result","content":"ok"}}]}}}}"#
            )
            .unwrap();
        }

        if m % 7 == 6 {
            writeln!(
                f,
                r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{msg_id}_th","role":"assistant","model":"{model}","content":[{{"type":"thinking","thinking":"considering options for step {m}"}}]}}}}"#
            )
            .unwrap();
        }
    }

    f.flush().unwrap();
    fs::metadata(path).unwrap().len()
}

const fn next(state: &mut u64) -> u64 {
    let mut x = *state;
    if x == 0 {
        x = 0xdead_beef_cafe_babe;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn clear_dir_keep(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            let _ = fs::remove_dir_all(&p);
        } else {
            let _ = fs::remove_file(&p);
        }
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// `parse::load_all_projects` and `cache::load` resolve their data dirs via
// `dirs::home_dir()`, which reads `$HOME` on Unix. Setting it before any
// thread is spawned (we're single-threaded at this point) is sound.
fn redirect_home(p: &Path) {
    // SAFETY: called once per corpus at startup; no other threads exist yet.
    unsafe {
        std::env::set_var("HOME", p);
    }
    #[cfg(windows)]
    unsafe {
        std::env::set_var("USERPROFILE", p);
    }
}
