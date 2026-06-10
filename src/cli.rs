// CLI parser — hand-rolled to avoid pulling in clap (~300KB of code).
// Shape: one positional subcommand + global flags + mode-scoped flags.

use chrono::NaiveDate;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cmd {
    #[default]
    Daily,
    Weekly,
    Monthly,
    Session,
    Blocks,
    Statusline,
    Tui,
    Web,
    RefreshPrices,
    Completion,
    Version,
    Help,
}

/// Row sort direction for report tables. `None` in `Options` means
/// "use the per-bucket default" (time buckets ascending, sessions
/// most-recent-first). An explicit `--order` overrides that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    Asc,
    Desc,
}

/// How costs are sourced, mirroring ccusage's `--mode`.
///
/// ccaudit always computes costs from its cached price table (it stores
/// token counts, not a per-line logged cost), so `Auto`/`Calculate` are
/// identical and `Display` falls back to `Calculate` with a one-line
/// note — there is no logged-cost field to display. Accepted so ccusage
/// muscle memory (and scripts) don't error.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CostMode {
    #[default]
    Auto,
    Calculate,
    Display,
}

impl Cmd {
    /// Single source of truth for the `enum Cmd` ⇄ subcommand-string
    /// mapping. `from_positional` parses, `as_str` renders, and any other
    /// site that needs either direction goes through these so a new
    /// subcommand can be added in exactly one place.
    pub const fn as_str(self) -> &'static str {
        match self {
            Cmd::Daily => "daily",
            Cmd::Weekly => "weekly",
            Cmd::Monthly => "monthly",
            Cmd::Session => "session",
            Cmd::Blocks => "blocks",
            Cmd::Statusline => "statusline",
            Cmd::Tui => "tui",
            Cmd::Web => "web",
            Cmd::RefreshPrices => "refresh-prices",
            Cmd::Completion => "completion",
            Cmd::Version => "version",
            Cmd::Help => "help",
        }
    }

    /// Every subcommand, in help-display order. Single source of truth
    /// for `from_positional` (parse), the `did you mean` matcher, and
    /// help rendering.
    pub const ALL: [Cmd; 12] = [
        Cmd::Daily,
        Cmd::Weekly,
        Cmd::Monthly,
        Cmd::Session,
        Cmd::Blocks,
        Cmd::Statusline,
        Cmd::Tui,
        Cmd::Web,
        Cmd::RefreshPrices,
        Cmd::Completion,
        Cmd::Version,
        Cmd::Help,
    ];

    fn from_positional(s: &str) -> Option<Cmd> {
        // Build the table from `as_str` so the two directions can't drift.
        Cmd::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

// Flags are naturally grouped here as independent booleans; clippy
// recommends splitting into a config struct once >3, but the fields
// here are unrelated and never travel together.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub cmd: Cmd,
    /// When `cmd == Help`, which subcommand's help to show (None = global).
    pub help_target: Option<Cmd>,
    pub since: Option<i32>, // days since epoch
    pub until: Option<i32>, // days since epoch (inclusive)
    pub json: bool,
    pub breakdown: bool,
    pub compact: bool,
    pub tz_offset_secs: i32, // 0 = UTC
    pub tz_label: String,    // for display
    pub locale: Option<String>,
    pub instances: bool,
    pub project: Option<String>,
    /// Web server port. `None` means use the default (3131); explicit
    /// `Some(_)` is rejected on non-web subcommands by `validate_flag_scopes`.
    pub port: Option<u16>,
    pub out_dir: Option<String>,
    /// `web` only: skip the local HTTP server and the `open` browser
    /// launch — just emit the static site to `--out` and exit. Used by
    /// CI / scripts / the integration test that asserts CLI ⟷ web
    /// uniformity without spawning a browser.
    pub no_serve: bool,
    pub source: crate::source::SourceKind,
    /// Limit the displayed rows to the most recent N buckets. `None`
    /// shows all rows. Applied after sorting; totals reflect visible
    /// rows only.
    pub tail: Option<u32>,
    /// Visualize each block's cost against this dollar limit (progress
    /// bar + color threshold). Only used by `blocks`.
    pub cost_limit: Option<f64>,
    /// Append a carbon-footprint footer (energy kWh, CO₂ kg, tree-
    /// years) to the report. Off by default to keep output lean.
    pub carbon: bool,
    /// Force-disable ANSI color regardless of TTY detection. Mirrors the
    /// `NO_COLOR` / `CCAUDIT_NO_COLOR` env vars and the universal
    /// `--no-color` convention. The final decision (TTY + env + flag)
    /// is computed once in `main` and pushed to `report::fmt::set_color`.
    pub no_color: bool,
    /// Suppress non-essential output (progress hints on stderr). The
    /// report itself still prints; only the chatter is silenced.
    pub quiet: bool,
    /// Plain, machine-friendly tabular output: tab-separated columns,
    /// no box-drawing, no color, one record per line — for `grep`/`awk`.
    pub plain: bool,
    /// Row sort direction. `None` = per-bucket default (time ascending,
    /// sessions most-recent-first). `Some(_)` overrides for display.
    pub order: Option<Order>,
    /// ccusage parity flag. ccusage re-fetches `LiteLLM` prices on every
    /// run and `--offline` makes it use a bundled snapshot instead;
    /// ccaudit *always* prices from its local cache (refreshed on demand
    /// by `refresh-prices`), so reports are already offline-fast and this
    /// flag is a documented no-op accepted so ccusage scripts/muscle
    /// memory keep working.
    pub offline: bool,
    /// Cost-sourcing mode (ccusage parity). See [`CostMode`].
    pub mode: CostMode,
    /// `blocks` only: show just the currently-active 5-hour block.
    pub blocks_active: bool,
    /// `blocks` only: show only blocks started within the last 3 days.
    pub blocks_recent: bool,
    /// `blocks` only: re-render the active block on an interval until
    /// interrupted (Ctrl-C).
    pub blocks_live: bool,
    /// `completion` only: the target shell to emit a script for.
    pub completion_shell: Option<String>,
}

pub fn parse(args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        tz_label: "UTC".to_string(),
        ..Default::default()
    };

    // First positional token, if present, may be a subcommand.
    let mut i = 0usize;
    let mut cmd_explicit = false;
    if let Some(first) = args.first() {
        if let Some(cmd) = Cmd::from_positional(first) {
            o.cmd = cmd;
            cmd_explicit = true;
            i = 1;

            // `help <sub>` — consume the optional target.
            if cmd == Cmd::Help {
                if let Some(second) = args.get(1) {
                    if let Some(target) = Cmd::from_positional(second) {
                        // `help <sub>` → scoped help; `help help` → global
                        // help. Either way consume the token so it isn't
                        // re-parsed below as a stray positional and rejected.
                        o.help_target = (target != Cmd::Help).then_some(target);
                        i = 2;
                    }
                }
            }

            // `completion <shell>` — consume the optional shell name so
            // it isn't rejected as an unexpected argument by the flag
            // loop below. Validated in `main` when the script is emitted.
            if cmd == Cmd::Completion {
                if let Some(second) = args.get(1) {
                    if !second.starts_with('-') {
                        o.completion_shell = Some(second.clone());
                        i = 2;
                    }
                }
            }
        }
    }

    while i < args.len() {
        let a = args.get(i).map(String::as_str).unwrap_or("");
        let next = || -> Result<&str, String> {
            args.get(i + 1)
                .map(String::as_str)
                .ok_or_else(|| format!("missing value for {a}"))
        };
        match a {
            "--help" | "-h" => {
                // `ccaudit <sub> --help`  → scoped help for <sub>
                // `ccaudit --help <sub>`  → scoped help for <sub>
                // `ccaudit --help`        → global help
                if cmd_explicit && o.cmd != Cmd::Help {
                    o.help_target = Some(o.cmd);
                }
                o.cmd = Cmd::Help;
                i += 1;
                // Accept `--help <sub>` form: peek the next token.
                if o.help_target.is_none() {
                    if let Some(next_arg) = args.get(i) {
                        if let Some(target) = Cmd::from_positional(next_arg) {
                            if target != Cmd::Help {
                                o.help_target = Some(target);
                                i += 1;
                            }
                        }
                    }
                }
            }
            "--json" | "-j" => {
                o.json = true;
                i += 1;
            }
            "--breakdown" => {
                o.breakdown = true;
                i += 1;
            }
            "--compact" => {
                o.compact = true;
                i += 1;
            }
            "--instances" => {
                o.instances = true;
                i += 1;
            }
            "--since" => {
                o.since = Some(parse_date(next()?)?);
                i += 2;
            }
            "--until" => {
                o.until = Some(parse_date(next()?)?);
                i += 2;
            }
            "--project" => {
                o.project = Some(next()?.to_string());
                i += 2;
            }
            "--timezone" | "--tz" => {
                let raw = next()?.to_string();
                let (offset, label) = parse_timezone(&raw)?;
                o.tz_offset_secs = offset;
                o.tz_label = label;
                i += 2;
            }
            "--locale" => {
                o.locale = Some(next()?.to_string());
                i += 2;
            }
            "--source" => {
                use std::str::FromStr as _;
                o.source = crate::source::SourceKind::from_str(next()?)?;
                i += 2;
            }
            "--port" => {
                let p: u16 = next()?.parse().map_err(|_| "invalid --port".to_string())?;
                o.port = Some(p);
                i += 2;
            }
            "--out" => {
                o.out_dir = Some(next()?.to_string());
                i += 2;
            }
            "--no-serve" => {
                o.no_serve = true;
                i += 1;
            }
            "--tail" => {
                let n: u32 = next()?
                    .parse()
                    .map_err(|_| "invalid --tail (expected non-negative integer)".to_string())?;
                o.tail = Some(n);
                i += 2;
            }
            "--carbon" => {
                o.carbon = true;
                i += 1;
            }
            "--version" | "-V" => {
                o.cmd = Cmd::Version;
                i += 1;
            }
            "--no-color" => {
                o.no_color = true;
                i += 1;
            }
            "--quiet" | "-q" => {
                o.quiet = true;
                i += 1;
            }
            "--plain" => {
                o.plain = true;
                i += 1;
            }
            "--offline" => {
                o.offline = true;
                i += 1;
            }
            "--active" => {
                o.blocks_active = true;
                i += 1;
            }
            "--recent" => {
                o.blocks_recent = true;
                i += 1;
            }
            "--live" => {
                o.blocks_live = true;
                i += 1;
            }
            "--order" => {
                o.order = Some(match next()? {
                    "asc" | "ascending" => Order::Asc,
                    "desc" | "descending" => Order::Desc,
                    other => {
                        return Err(format!(
                            "invalid --order {other:?} (expected `asc` or `desc`)"
                        ));
                    }
                });
                i += 2;
            }
            "--mode" => {
                o.mode = match next()? {
                    "auto" => CostMode::Auto,
                    "calculate" | "calc" => CostMode::Calculate,
                    "display" => CostMode::Display,
                    other => {
                        return Err(format!(
                            "invalid --mode {other:?} (expected `auto`, `calculate`, or `display`)"
                        ));
                    }
                };
                i += 2;
            }
            "--cost-limit" => {
                // Accept "$10", "10", "10.50"; strip leading $ for
                // people who paste the dollar sign from docs.
                let raw = next()?;
                let stripped = raw.strip_prefix('$').unwrap_or(raw);
                let n: f64 = stripped
                    .parse()
                    .map_err(|_| format!("invalid --cost-limit value {raw:?}"))?;
                if !(n.is_finite() && n > 0.0) {
                    return Err(format!("--cost-limit must be > 0, got {raw:?}"));
                }
                o.cost_limit = Some(n);
                i += 2;
            }
            _ if a.starts_with('-') => {
                return Err(match nearest(a.trim_start_matches('-'), KNOWN_FLAGS) {
                    Some(s) => format!("unknown flag: {a}\n  did you mean `--{s}`?"),
                    None => format!("unknown flag: {a}"),
                });
            }
            _ => {
                // A bare word in the first slot is almost always a
                // misspelled subcommand — suggest the closest one.
                return Err(match nearest(a, &Cmd::ALL.map(Cmd::as_str)) {
                    Some(s) => format!("unexpected argument: {a}\n  did you mean `ccaudit {s}`?"),
                    None => format!("unexpected argument: {a}"),
                });
            }
        }
    }

    // Help and `--version` bypass flag scoping — the user asked for one of
    // those, so honor it instead of erroring on an out-of-scope flag
    // (`ccaudit blocks --active --version` should print the version).
    if o.cmd != Cmd::Help && o.cmd != Cmd::Version {
        validate_flag_scopes(&o)?;
    }

    Ok(o)
}

/// Every long-flag spelling we recognize (sans leading dashes). Used
/// only by the `did you mean` matcher on an unknown flag, so it doesn't
/// need to track which command each applies to — `validate_flag_scopes`
/// handles correctness; this just rescues typos like `--complact`.
const KNOWN_FLAGS: &[&str] = &[
    "help",
    "json",
    "breakdown",
    "compact",
    "instances",
    "since",
    "until",
    "project",
    "timezone",
    "tz",
    "locale",
    "source",
    "port",
    "out",
    "no-serve",
    "tail",
    "carbon",
    "cost-limit",
    "version",
    "no-color",
    "quiet",
    "plain",
    "offline",
    "active",
    "recent",
    "live",
    "order",
    "mode",
];

/// Closest candidate to `input` within edit distance 2, or `None`. The
/// distance must also be strictly less than the candidate length so a
/// 2-char input doesn't "match" every 2-char command. Used for the
/// `did you mean ...?` hints on unknown subcommands and flags.
fn nearest(input: &str, candidates: &[&'static str]) -> Option<&'static str> {
    let mut best: Option<(&'static str, usize)> = None;
    for &c in candidates {
        let d = levenshtein(input, c);
        if best.is_none_or(|(_, bd)| d < bd) {
            best = Some((c, d));
        }
    }
    best.filter(|&(c, d)| d <= 2 && d < c.len()).map(|(c, _)| c)
}

/// Classic Wagner–Fischer edit distance over `chars`. Inputs here are
/// short (a CLI token vs. a flag/command name) so the O(n·m) table and
/// its per-call allocation are negligible, and pulling in a crate for
/// this would contradict the lean-binary goal.
//
// All indices below are bounded by the loop ranges (`0..a.len()`,
// `0..b.len()`) against vectors sized `b.len() + 1`, so every access is
// provably in range — the `indexing_slicing` lint can't see that.
#[allow(clippy::indexing_slicing)]
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

// Subcommand groupings used by the scope checks below.
const REPORT_CMDS: &[Cmd] = &[
    Cmd::Daily,
    Cmd::Weekly,
    Cmd::Monthly,
    Cmd::Session,
    Cmd::Blocks,
];
// Subcommands that honor the global filter flags (--since/--until/--project).
// `tui` and `web` deliberately ignore them today (Phase A); `not_honored_by_ui`
// derives its rejection set from this list as the single source of truth.
const FILTER_CMDS: &[Cmd] = &[
    Cmd::Daily,
    Cmd::Weekly,
    Cmd::Monthly,
    Cmd::Session,
    Cmd::Blocks,
    Cmd::Statusline,
];

fn validate_flag_scopes(o: &Options) -> Result<(), String> {
    // Report-display flags only make sense on daily/monthly/session/blocks.
    let report_flag = |name: &str| -> Result<(), String> {
        if !REPORT_CMDS.contains(&o.cmd) {
            Err(format!(
                "{name} only applies to daily / monthly / session / blocks reports"
            ))
        } else {
            Ok(())
        }
    };
    if o.json {
        report_flag("--json")?;
    }
    if o.breakdown {
        report_flag("--breakdown")?;
    }
    if o.compact {
        report_flag("--compact")?;
    }
    if o.instances {
        report_flag("--instances")?;
    }
    if o.tail.is_some() {
        report_flag("--tail")?;
    }
    if o.carbon {
        report_flag("--carbon")?;
    }
    if o.plain {
        report_flag("--plain")?;
    }
    if o.order.is_some() {
        report_flag("--order")?;
    }
    if o.mode != CostMode::Auto {
        report_flag("--mode")?;
    }

    // Blocks-only.
    if o.cost_limit.is_some() && o.cmd != Cmd::Blocks {
        return Err(
            "--cost-limit only applies to `blocks` (e.g. `ccaudit blocks --cost-limit 100`)"
                .to_string(),
        );
    }
    let blocks_flag = |name: &str| -> Result<(), String> {
        if o.cmd == Cmd::Blocks {
            Ok(())
        } else {
            Err(format!("{name} only applies to `blocks`"))
        }
    };
    if o.blocks_active {
        blocks_flag("--active")?;
    }
    if o.blocks_recent {
        blocks_flag("--recent")?;
    }
    if o.blocks_live {
        blocks_flag("--live")?;
    }
    if o.blocks_active && o.blocks_recent {
        return Err("--active and --recent are mutually exclusive".to_string());
    }

    // Web-only.
    if o.out_dir.is_some() && o.cmd != Cmd::Web {
        return Err("--out only applies to `web` (e.g. `ccaudit web --out ./site`)".to_string());
    }
    if o.no_serve && o.cmd != Cmd::Web {
        return Err("--no-serve only applies to `web`".to_string());
    }
    if o.port.is_some() && o.cmd != Cmd::Web {
        return Err("--port only applies to `web` (e.g. `ccaudit web --port 8080`)".to_string());
    }

    // Global-filter flags: only honored by FILTER_CMDS today (Phase A).
    // Rejecting with a clear message beats silently dropping them.
    let filter_flag = |name: &str| -> Result<(), String> {
        if FILTER_CMDS.contains(&o.cmd) {
            return Ok(());
        }
        let (mode, why) = match o.cmd {
            Cmd::Tui => ("tui", " — it launches the browser unfiltered"),
            Cmd::Web => ("web", " — it generates the site unfiltered"),
            _ => ("this subcommand", ""),
        };
        Err(format!("{name} is not yet honored by `{mode}`{why}"))
    };
    if o.since.is_some() {
        filter_flag("--since")?;
    }
    if o.until.is_some() {
        filter_flag("--until")?;
    }
    if o.project.is_some() {
        filter_flag("--project")?;
    }
    if o.locale.is_some() {
        filter_flag("--locale")?;
    }

    // `statusline` is inherently "today + active block" and honors only
    // --project; --since/--until/--locale have no meaning there, so reject
    // them rather than silently dropping (the codebase's stated policy).
    if o.cmd == Cmd::Statusline {
        let bad = if o.since.is_some() {
            Some("--since")
        } else if o.until.is_some() {
            Some("--until")
        } else if o.locale.is_some() {
            Some("--locale")
        } else {
            None
        };
        if let Some(name) = bad {
            return Err(format!(
                "{name} is not honored by `statusline` (it always reports today; use --project to scope)"
            ));
        }
    }

    // tui/web read Claude Code's project tree directly (the loader hardcodes
    // ~/.claude/projects and Claude's project-dir layout), so a non-default
    // --source would scan Claude's logs while pricing from another
    // provider's cache. Reject rather than silently mismatch.
    if matches!(o.cmd, Cmd::Tui | Cmd::Web) && o.source != crate::source::SourceKind::ClaudeCode {
        return Err(format!(
            "--source is not supported by `{}` (it reads Claude Code logs only)",
            o.cmd.as_str()
        ));
    }

    Ok(())
}

// Accept ccusage-compatible YYYYMMDD plus the more readable YYYY-MM-DD.
fn parse_date(s: &str) -> Result<i32, String> {
    let fmt = if s.len() == 8 && !s.contains('-') {
        "%Y%m%d"
    } else {
        "%Y-%m-%d"
    };
    let d = NaiveDate::parse_from_str(s, fmt).map_err(|e| format!("bad date {s:?}: {e}"))?;
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).ok_or_else(|| "epoch".to_string())?;
    Ok(d.signed_duration_since(epoch).num_days() as i32)
}

// Returns (offset_secs_from_utc, display_label).
//
// Supported forms: "UTC", "Local", "+HH:MM", "-HH:MM", "+HHMM", "-HHMM".
// Named IANA zones (e.g. "America/New_York") would require chrono-tz
// which adds ~1MB of data — out of scope for the lean binary.
fn parse_timezone(s: &str) -> Result<(i32, String), String> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("utc") || t == "Z" {
        return Ok((0, "UTC".to_string()));
    }
    if t.eq_ignore_ascii_case("local") {
        // Use system local offset at "now". Chrono returns this cheaply.
        let now = chrono::Local::now();
        let off = now.offset().local_minus_utc();
        // No inner parens: report titles wrap the label in parens already,
        // so "Local +02:00" reads as "(Local +02:00)" not "(Local (+02:00))".
        return Ok((off, format!("Local {}", format_offset(off))));
    }
    // ±HH:MM or ±HHMM
    let (sign, rest) = match t.as_bytes().first() {
        Some(b'+') => (1, &t[1..]),
        Some(b'-') => (-1, &t[1..]),
        _ => {
            return Err(format!(
                "unsupported timezone {s:?}; use UTC, Local, or ±HH:MM"
            ));
        }
    };
    let (h, m) = if let Some((h, m)) = rest.split_once(':') {
        (h, m)
    } else if rest.len() == 4 {
        rest.split_at(2)
    } else {
        return Err(format!("bad offset {s:?}"));
    };
    let hours: i32 = h.parse().map_err(|_| format!("bad hours in {s:?}"))?;
    let mins: i32 = m.parse().map_err(|_| format!("bad minutes in {s:?}"))?;
    // Bound each component: the real UTC-offset range is ±14:00, and a
    // negative component (e.g. `+1:-30`) means the input was malformed.
    if !(0..=14).contains(&hours) || !(0..=59).contains(&mins) {
        return Err(format!(
            "timezone offset out of range {s:?}; expected hours 0-14, minutes 0-59"
        ));
    }
    let off = sign * (hours * 3600 + mins * 60);
    Ok((off, format_offset(off)))
}

fn format_offset(secs: i32) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.abs();
    format!("{sign}{:02}:{:02}", a / 3600, (a % 3600) / 60)
}

/// Version + build provenance without a leading program name:
/// `X.Y.Z (built <iso8601> , <sha>)`. The shared core so the help header
/// (`version <core>`) and `--version` (`ccaudit <core>`) can't drift.
///
/// The timestamp and SHA come from `build.rs` via compile-time env vars.
pub fn version_core() -> String {
    format!(
        "{} (built {}, {})",
        env!("CARGO_PKG_VERSION"),
        env!("CCAUDIT_BUILD_TIME"),
        env!("CCAUDIT_GIT_SHA"),
    )
}

/// Program-prefixed version for `--version` / the `version` subcommand:
/// `ccaudit X.Y.Z (built <iso8601>, <sha>)`.
pub fn version_detail() -> String {
    format!("ccaudit {}", version_core())
}

#[allow(clippy::print_stdout)] // requested help is primary output → stdout
pub fn print_help() {
    // Sectioned layout modeled on cloudflared / phonon: NAME, USAGE,
    // DESCRIPTION, COMMANDS, GLOBAL OPTIONS, EXAMPLES, LEARN MORE,
    // SUPPORT. Section headers give onboarding readers something to
    // skim; the two-space command/flag columns line up for scanning.
    println!("NAME:");
    println!("   ccaudit - fast Claude Code token usage analyzer");
    println!("   version {}", version_core());
    println!();
    println!("USAGE:");
    println!("   ccaudit [COMMAND] [FLAGS]");
    println!();
    println!("DESCRIPTION:");
    println!("   ccaudit reads your local ~/.claude logs and reports token");
    println!("   usage + cost — daily, weekly, monthly, per session, or per");
    println!("   5-hour billing block. A mmap'd binary cache keeps warm runs");
    println!("   under ~5 ms — reports price from a local copy of LiteLLM's");
    println!("   model prices instead of re-fetching every run. Update that");
    println!("   copy online anytime with `ccaudit refresh-prices`.");
    println!();
    println!("COMMANDS:");
    println!("  daily           (default) daily token usage + cost");
    println!("  weekly          aggregate by week (Mon-anchored)");
    println!("  monthly         aggregate by month");
    println!("  session         aggregate by conversation session");
    println!("  blocks          5-hour billing windows, with active detection");
    println!("  statusline      compact one-line summary for terminal status bars");
    #[cfg(feature = "tui")]
    println!("  tui             interactive TUI browser");
    #[cfg(feature = "web")]
    println!("  web             generate static site + serve");
    println!("  refresh-prices  fetch latest model prices from LiteLLM");
    println!("  completion      print a shell completion script (bash/zsh/fish)");
    println!("  version         print the version");
    println!("  help [COMMAND]  show help for ccaudit or for a command");
    println!();
    println!("GLOBAL OPTIONS:");
    println!("      --since YYYYMMDD    filter by start date (inclusive)");
    println!("      --until YYYYMMDD    filter by end date (inclusive)");
    println!("      --project NAME      filter to a single project");
    println!("      --timezone TZ      UTC, Local, or ±HH:MM (default UTC)");
    println!("      --locale LOC       date locale (e.g. en_US, ja_JP)");
    println!("      --source NAME      log provider: claude-code (default), codex");
    println!("      --no-color         disable ANSI color (also: NO_COLOR env)");
    println!("  -q, --quiet            suppress non-essential output");
    println!("  -V, --version          print the version");
    println!("  -h, --help             show help");
    println!();
    println!("EXAMPLES:");
    println!("   $ ccaudit                                  # today + recent daily usage");
    println!("   $ ccaudit monthly --breakdown              # months, split per model");
    println!("   $ ccaudit blocks --active                  # just the live 5-hour window");
    println!("   $ ccaudit daily --since 20260101 --json | jq .totals");
    println!("   $ ccaudit daily --plain | awk '{{print $1, $NF}}'");
    println!();
    println!("LEARN MORE:");
    println!("   Use `ccaudit <command> --help` for command-specific flags.");
    println!("   Read the full docs at https://github.com/electricapp/ccaudit");
    println!();
    println!("SUPPORT:");
    println!("   Report bugs at https://github.com/electricapp/ccaudit/issues");
}

#[allow(clippy::print_stdout)] // requested help is primary output → stdout
pub fn print_subcommand_help(cmd: Cmd) {
    match cmd {
        Cmd::Daily => print_report_help("daily", "daily token usage + cost", None),
        Cmd::Weekly => print_report_help("weekly", "aggregate by week (Mon-anchored)", None),
        Cmd::Monthly => print_report_help("monthly", "aggregate by month", None),
        Cmd::Session => print_report_help("session", "aggregate by conversation session", None),
        Cmd::Blocks => print_report_help(
            "blocks",
            "5-hour billing windows, with active detection",
            Some(&[
                ("--cost-limit $N", "show a progress bar vs $N limit"),
                ("--active", "show only the currently-active block"),
                ("--recent", "show only blocks from the last 3 days"),
                ("--live", "refresh the active block until Ctrl-C"),
            ]),
        ),
        Cmd::Statusline => {
            println!("ccaudit statusline - compact one-line summary (for terminal status bars)");
            println!();
            println!("USAGE");
            println!("  ccaudit statusline [GLOBAL FLAGS]");
            println!();
            println!("FLAGS");
            println!("  (global flags only — see `ccaudit --help`)");
            println!();
            println!("EXAMPLES");
            println!("  ccaudit statusline");
            println!("  ccaudit statusline --timezone Local --project alpha");
        }
        Cmd::Tui => {
            println!("ccaudit tui - interactive TUI browser");
            println!();
            println!("USAGE");
            println!("  ccaudit tui");
            println!();
            println!("FLAGS");
            println!("  (none currently)");
            println!();
            println!("EXAMPLES");
            println!("  ccaudit tui");
            println!();
            println!("Note: global filter flags (--since/--until/--project) are not yet honored");
            println!("by `tui`. It launches the browser with all data loaded.");
        }
        Cmd::Web => {
            println!("ccaudit web - generate static site + serve");
            println!();
            println!("USAGE");
            println!("  ccaudit web [FLAGS]");
            println!();
            println!("FLAGS");
            println!("  --port N            HTTP server port (default 3131)");
            println!("  --out DIR           output directory (default: ~/.claude/ccaudit-web)");
            println!("  --no-serve          generate the static site, then exit (no browser)");
            println!();
            println!("EXAMPLES");
            println!("  ccaudit web");
            println!("  ccaudit web --port 8080 --out ./site");
            println!();
            println!("Note: global filter flags (--since/--until/--project) are not yet honored");
            println!("by `web`. It generates the site with all data loaded.");
        }
        Cmd::RefreshPrices => {
            println!("ccaudit refresh-prices - fetch latest model prices from LiteLLM");
            println!();
            println!("USAGE");
            println!("  ccaudit refresh-prices [FLAGS]");
            println!();
            println!("FLAGS");
            println!("  --source NAME       log provider: claude-code (default), codex");
            println!();
            println!("EXAMPLES");
            println!("  ccaudit refresh-prices");
        }
        Cmd::Completion => {
            println!("ccaudit completion - print a shell completion script");
            println!();
            println!("USAGE");
            println!("  ccaudit completion <SHELL>");
            println!();
            println!("SHELLS");
            println!("  bash    zsh    fish");
            println!();
            println!("EXAMPLES");
            println!("  ccaudit completion zsh  > ~/.zfunc/_ccaudit");
            println!("  ccaudit completion bash > /etc/bash_completion.d/ccaudit");
            println!("  ccaudit completion fish > ~/.config/fish/completions/ccaudit.fish");
        }
        Cmd::Version => {
            println!("ccaudit version - print the version");
            println!();
            println!("USAGE");
            println!("  ccaudit version      (also: ccaudit --version, ccaudit -V)");
        }
        Cmd::Help => print_help(),
    }
}

// Shared template for the reporting subcommands. `extra` threads any
// mode-specific flags (e.g. --cost-limit / --active for blocks).
#[allow(clippy::print_stdout)] // requested help is primary output → stdout
fn print_report_help(name: &str, tagline: &str, extra: Option<&[(&str, &str)]>) {
    println!("ccaudit {name} - {tagline}");
    println!();
    println!("USAGE");
    println!("  ccaudit {name} [FLAGS]");
    println!();
    println!("FLAGS");
    println!("  --json              JSON output");
    println!("  --plain             tab-separated, no box / color (for grep, awk)");
    println!("  --breakdown         split rows by model");
    println!("  --compact           narrower table layout");
    println!("  --instances         group by project");
    println!("  --order asc|desc    row sort direction");
    println!("  --tail N            keep only the N most-recent rows");
    println!("  --carbon            append energy / CO₂ / tree-year footer");
    println!("  --mode MODE         cost source: auto|calculate|display (ccusage parity)");
    println!("  --offline           price from the local cache (always on; ccusage parity)");
    for (flag, desc) in extra.unwrap_or(&[]) {
        println!("  {flag:<19} {desc}");
    }
    println!("  (plus global flags — see `ccaudit --help`)");
    println!();
    println!("EXAMPLES");
    println!("  ccaudit {name}");
    match name {
        "blocks" => println!("  ccaudit blocks --cost-limit 100 --tail 5"),
        _ => println!("  ccaudit {name} --since 20260101 --project alpha --breakdown"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> Options {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        parse(&owned).unwrap()
    }
    fn parse_err(args: &[&str]) -> String {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        parse(&owned).unwrap_err()
    }

    #[test]
    fn version_flag_and_subcommand() {
        assert_eq!(parse_ok(&["--version"]).cmd, Cmd::Version);
        assert_eq!(parse_ok(&["-V"]).cmd, Cmd::Version);
        assert_eq!(parse_ok(&["version"]).cmd, Cmd::Version);
    }

    #[test]
    fn weekly_is_a_subcommand() {
        assert_eq!(parse_ok(&["weekly"]).cmd, Cmd::Weekly);
    }

    #[test]
    fn order_parses_and_validates() {
        assert_eq!(
            parse_ok(&["daily", "--order", "asc"]).order,
            Some(Order::Asc)
        );
        assert_eq!(
            parse_ok(&["daily", "--order", "desc"]).order,
            Some(Order::Desc)
        );
        assert!(parse_err(&["daily", "--order", "sideways"]).contains("invalid --order"));
    }

    #[test]
    fn mode_parses_and_validates() {
        assert_eq!(
            parse_ok(&["daily", "--mode", "calculate"]).mode,
            CostMode::Calculate
        );
        assert_eq!(
            parse_ok(&["daily", "--mode", "display"]).mode,
            CostMode::Display
        );
        assert!(parse_err(&["daily", "--mode", "guess"]).contains("invalid --mode"));
    }

    #[test]
    fn blocks_flags_are_scoped() {
        assert!(parse_ok(&["blocks", "--active"]).blocks_active);
        assert!(parse_err(&["daily", "--active"]).contains("only applies to `blocks`"));
        assert!(parse_err(&["blocks", "--active", "--recent"]).contains("mutually exclusive"));
    }

    #[test]
    fn plain_and_order_are_report_scoped() {
        assert!(parse_ok(&["daily", "--plain"]).plain);
        assert!(parse_err(&["tui", "--plain"]).contains("daily / monthly"));
    }

    #[test]
    fn unknown_flag_suggests_nearest() {
        let e = parse_err(&["daily", "--complact"]);
        assert!(e.contains("unknown flag"), "{e}");
        assert!(e.contains("did you mean `--compact`"), "{e}");
    }

    #[test]
    fn unknown_subcommand_suggests_nearest() {
        let e = parse_err(&["dialy"]);
        assert!(e.contains("did you mean `ccaudit daily`"), "{e}");
    }

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("daily", "daily"), 0);
        assert_eq!(levenshtein("dialy", "daily"), 2);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    #[test]
    fn nearest_only_matches_within_threshold() {
        assert_eq!(nearest("compact", KNOWN_FLAGS), Some("compact"));
        assert_eq!(nearest("zzzzzzz", KNOWN_FLAGS), None);
    }

    #[test]
    fn offline_is_accepted() {
        assert!(parse_ok(&["daily", "--offline"]).offline);
    }
}
