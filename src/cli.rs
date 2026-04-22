// CLI parser — hand-rolled to avoid pulling in clap (~300KB of code).
// Shape: one positional subcommand + global flags + mode-scoped flags.

use chrono::NaiveDate;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cmd {
    #[default]
    Daily,
    Monthly,
    Session,
    Blocks,
    Statusline,
    Tui,
    Web,
    RefreshPrices,
    Help,
}

impl Cmd {
    fn from_positional(s: &str) -> Option<Cmd> {
        match s {
            "daily" => Some(Cmd::Daily),
            "monthly" => Some(Cmd::Monthly),
            "session" => Some(Cmd::Session),
            "blocks" => Some(Cmd::Blocks),
            "statusline" => Some(Cmd::Statusline),
            "tui" => Some(Cmd::Tui),
            "web" => Some(Cmd::Web),
            "refresh-prices" => Some(Cmd::RefreshPrices),
            "help" => Some(Cmd::Help),
            _ => None,
        }
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
                        if target != Cmd::Help {
                            o.help_target = Some(target);
                            i = 2;
                        }
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
                return Err(format!("unknown flag: {a}"));
            }
            _ => {
                return Err(format!("unexpected argument: {a}"));
            }
        }
    }

    // Help bypasses flag scoping — the user asked for help, just give it.
    if o.cmd != Cmd::Help {
        validate_flag_scopes(&o)?;
    }

    Ok(o)
}

// Subcommand groupings used by the scope checks below.
const REPORT_CMDS: &[Cmd] = &[Cmd::Daily, Cmd::Monthly, Cmd::Session, Cmd::Blocks];
const FILTER_CMDS: &[Cmd] = &[
    Cmd::Daily,
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

    // Blocks-only.
    if o.cost_limit.is_some() && o.cmd != Cmd::Blocks {
        return Err(
            "--cost-limit only applies to `blocks` (e.g. `ccaudit blocks --cost-limit 100`)"
                .to_string(),
        );
    }

    // Web-only.
    if o.out_dir.is_some() && o.cmd != Cmd::Web {
        return Err("--out only applies to `web` (e.g. `ccaudit web --out ./site`)".to_string());
    }
    if o.port.is_some() && o.cmd != Cmd::Web {
        return Err("--port only applies to `web` (e.g. `ccaudit web --port 8080`)".to_string());
    }

    // Global-filter flags: in Phase A, `tui` and `web` do not honor these yet.
    // Rejecting with a clear message beats silently dropping them.
    let not_honored_by_ui = |name: &str| -> Result<(), String> {
        match o.cmd {
            Cmd::Tui => Err(format!(
                "{name} is not yet honored by `tui` — it launches the browser unfiltered"
            )),
            Cmd::Web => Err(format!(
                "{name} is not yet honored by `web` — it generates the site unfiltered"
            )),
            _ => Ok(()),
        }
    };
    if o.since.is_some() {
        not_honored_by_ui("--since")?;
    }
    if o.until.is_some() {
        not_honored_by_ui("--until")?;
    }
    if o.project.is_some() {
        not_honored_by_ui("--project")?;
    }
    if o.locale.is_some() && !FILTER_CMDS.contains(&o.cmd) {
        // `refresh-prices` has no use for --locale; still silent there today.
        not_honored_by_ui("--locale")?;
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
        return Ok((off, format!("Local ({})", format_offset(off))));
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
    let off = sign * (hours * 3600 + mins * 60);
    Ok((off, format_offset(off)))
}

fn format_offset(secs: i32) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.abs();
    format!("{sign}{:02}:{:02}", a / 3600, (a % 3600) / 60)
}

pub fn print_help() {
    eprintln!("ccaudit - fast Claude Code token usage analyzer");
    eprintln!();
    eprintln!("USAGE");
    eprintln!("  ccaudit [SUBCOMMAND] [FLAGS]");
    eprintln!();
    eprintln!("SUBCOMMANDS");
    eprintln!("  daily           (default) daily token usage + cost");
    eprintln!("  monthly         aggregate by month");
    eprintln!("  session         aggregate by conversation session");
    eprintln!("  blocks          5-hour billing windows, with active detection");
    eprintln!("  statusline      compact one-line summary (for terminal status bars)");
    #[cfg(feature = "tui")]
    eprintln!("  tui             interactive TUI browser");
    #[cfg(feature = "web")]
    eprintln!("  web             generate static site + serve");
    eprintln!("  refresh-prices  fetch latest model prices from LiteLLM");
    eprintln!("  help [SUB]      show help for ccaudit or for a subcommand");
    eprintln!();
    eprintln!("GLOBAL FLAGS");
    eprintln!("  --since YYYYMMDD    filter by start date (inclusive)");
    eprintln!("  --until YYYYMMDD    filter by end date (inclusive)");
    eprintln!("  --project NAME      filter to a single project");
    eprintln!("  --timezone TZ       UTC, Local, or ±HH:MM (default UTC)");
    eprintln!("  --locale LOC        date locale (e.g. en_US, ja_JP)");
    eprintln!("  --source NAME       log provider: claude-code (default)");
    eprintln!("  --help, -h          show help");
    eprintln!();
    eprintln!("Run `ccaudit <SUBCOMMAND> --help` for mode-specific flags.");
}

pub fn print_subcommand_help(cmd: Cmd) {
    match cmd {
        Cmd::Daily => print_report_help("daily", "daily token usage + cost", None),
        Cmd::Monthly => print_report_help("monthly", "aggregate by month", None),
        Cmd::Session => print_report_help("session", "aggregate by conversation session", None),
        Cmd::Blocks => print_report_help(
            "blocks",
            "5-hour billing windows, with active detection",
            Some(("--cost-limit $N", "show a progress bar vs $N limit")),
        ),
        Cmd::Statusline => {
            eprintln!("ccaudit statusline - compact one-line summary (for terminal status bars)");
            eprintln!();
            eprintln!("USAGE");
            eprintln!("  ccaudit statusline [GLOBAL FLAGS]");
            eprintln!();
            eprintln!("FLAGS");
            eprintln!("  (global flags only — see `ccaudit --help`)");
            eprintln!();
            eprintln!("EXAMPLES");
            eprintln!("  ccaudit statusline");
            eprintln!("  ccaudit statusline --timezone Local --project alpha");
        }
        Cmd::Tui => {
            eprintln!("ccaudit tui - interactive TUI browser");
            eprintln!();
            eprintln!("USAGE");
            eprintln!("  ccaudit tui");
            eprintln!();
            eprintln!("FLAGS");
            eprintln!("  (none currently)");
            eprintln!();
            eprintln!("EXAMPLES");
            eprintln!("  ccaudit tui");
            eprintln!();
            eprintln!("Note: global filter flags (--since/--until/--project) are not yet honored");
            eprintln!("by `tui`. It launches the browser with all data loaded.");
        }
        Cmd::Web => {
            eprintln!("ccaudit web - generate static site + serve");
            eprintln!();
            eprintln!("USAGE");
            eprintln!("  ccaudit web [FLAGS]");
            eprintln!();
            eprintln!("FLAGS");
            eprintln!("  --port N            HTTP server port (default 3131)");
            eprintln!("  --out DIR           output directory (default: ~/.claude/ccaudit-web)");
            eprintln!();
            eprintln!("EXAMPLES");
            eprintln!("  ccaudit web");
            eprintln!("  ccaudit web --port 8080 --out ./site");
            eprintln!();
            eprintln!("Note: global filter flags (--since/--until/--project) are not yet honored");
            eprintln!("by `web`. It generates the site with all data loaded.");
        }
        Cmd::RefreshPrices => {
            eprintln!("ccaudit refresh-prices - fetch latest model prices from LiteLLM");
            eprintln!();
            eprintln!("USAGE");
            eprintln!("  ccaudit refresh-prices [FLAGS]");
            eprintln!();
            eprintln!("FLAGS");
            eprintln!("  --source NAME       log provider: claude-code (default)");
            eprintln!();
            eprintln!("EXAMPLES");
            eprintln!("  ccaudit refresh-prices");
        }
        Cmd::Help => print_help(),
    }
}

// Shared template for the four reporting subcommands. `extra` threads a
// single mode-specific flag (currently only --cost-limit for blocks).
fn print_report_help(name: &str, tagline: &str, extra: Option<(&str, &str)>) {
    eprintln!("ccaudit {name} - {tagline}");
    eprintln!();
    eprintln!("USAGE");
    eprintln!("  ccaudit {name} [FLAGS]");
    eprintln!();
    eprintln!("FLAGS");
    eprintln!("  --json              JSON output");
    eprintln!("  --breakdown         split rows by model");
    eprintln!("  --compact           narrower table layout");
    eprintln!("  --instances         group by project");
    eprintln!("  --tail N            keep only the N most-recent rows");
    eprintln!("  --carbon            append energy / CO₂ / tree-year footer");
    if let Some((flag, desc)) = extra {
        eprintln!("  {flag:<19} {desc}");
    }
    eprintln!("  (plus global flags — see `ccaudit --help`)");
    eprintln!();
    eprintln!("EXAMPLES");
    eprintln!("  ccaudit {name}");
    match name {
        "blocks" => eprintln!("  ccaudit blocks --cost-limit 100 --tail 5"),
        _ => eprintln!("  ccaudit {name} --since 20260101 --project alpha --breakdown"),
    }
}
