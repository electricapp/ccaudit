// mimalloc is noticeably faster on macOS than the system allocator for
// allocation-heavy startup (HashMap rebuilds, small String intern).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(any(feature = "tui", feature = "web"))]
use ccaudit::parse;
#[cfg(feature = "tui")]
use ccaudit::ui;
use ccaudit::{cache, cli, report, source};
#[cfg(feature = "web")]
use ccaudit::{serve, web};
use std::process;

// `main` is the one place the binary gets to write to stdio directly:
// errors to stderr, `refresh-prices` summary + profiling to stderr/stdout.
#[allow(clippy::print_stdout, clippy::print_stderr)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let opts = match cli::parse(&args) {
        Ok(o) => o,
        Err(e) => {
            // clig.dev signal-to-noise: a one-line error plus a pointer,
            // not a wall of help text. `e` may already carry a
            // typo-hint line from the parser.
            eprintln!("error: {e}");
            eprintln!("Run `ccaudit --help` for usage.");
            process::exit(2);
        }
    };

    // Publish log-root and price overrides before anything scans or
    // prices. `--logs-dir` scopes to the selected --source and replaces
    // whatever the config set for it; the config can cover several
    // providers at once.
    let mut roots = opts.config_log_roots.clone();
    if !opts.logs_dirs.is_empty() {
        let id = source::pick(opts.source).id().to_string();
        roots.retain(|(k, _)| *k != id);
        roots.push((
            id,
            opts.logs_dirs
                .iter()
                .map(std::path::PathBuf::from)
                .collect(),
        ));
    }
    if !roots.is_empty() {
        source::set_log_roots(roots);
    }
    if !opts.config_prices.is_empty() {
        source::set_price_overrides(opts.config_prices.clone());
    }

    // Resolve the color decision once and publish it to the report
    // layer. Order of precedence (clig.dev + NO_COLOR spec):
    //   1. explicit --no-color / --plain        → off
    //   2. NO_COLOR / CCAUDIT_NO_COLOR (nonempty) → off
    //   3. TERM=dumb                              → off
    //   4. FORCE_COLOR / CCAUDIT_FORCE_COLOR      → on
    //   5. otherwise: on iff stdout is a TTY
    report::fmt::set_color(resolve_color(&opts));

    // --offline is ccaudit's default (reads never touch the network), and
    // --mode display has no logged-cost field to show. Tell the user
    // honestly rather than silently diverging — unless --quiet.
    if opts.mode == cli::CostMode::Display && !opts.quiet {
        eprintln!(
            "note: ccaudit computes costs from its cached price table; `--mode display` falls back to calculated costs."
        );
    }

    match opts.cmd {
        cli::Cmd::Help => match opts.help_target {
            Some(target) => cli::print_subcommand_help(target),
            None => cli::print_help(),
        },
        cli::Cmd::Version => {
            println!("{}", cli::version_detail());
        }
        cli::Cmd::Completion => {
            if let Err(e) = print_completion(opts.completion_shell.as_deref()) {
                eprintln!("error: {e}");
                process::exit(2);
            }
        }
        cli::Cmd::RefreshPrices => match source::prices::refresh() {
            Ok(r) => {
                println!(
                    "refreshed {} models → {} ({} bytes){}",
                    r.model_count,
                    r.cache_path.display(),
                    r.bytes_written,
                    if r.invalidated_usage_db {
                        "  (usage cache invalidated)"
                    } else {
                        ""
                    }
                );
            }
            Err(e) => {
                eprintln!("error: {e}");
                process::exit(1);
            }
        },
        cli::Cmd::Tui => {
            #[cfg(feature = "tui")]
            {
                let source = source::pick(opts.source);
                let projects = parse::load_all_projects(source);
                match run_tui(projects, source) {
                    Ok(Some(ui::PostAction::Resume { id, cwd })) => {
                        resume_session(&id, cwd.as_deref());
                    }
                    Ok(Some(ui::PostAction::OpenWeb)) => {
                        #[cfg(feature = "web")]
                        run_web_cmd(&opts);
                        #[cfg(not(feature = "web"))]
                        {
                            eprintln!(
                                "error: this build does not include the web report. Rebuild with `--features web` to use `o` from the TUI."
                            );
                            process::exit(1);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("error: {e}");
                        process::exit(1);
                    }
                }
            }
            #[cfg(not(feature = "tui"))]
            {
                eprintln!(
                    "error: this build does not include the TUI. Rebuild with `--features tui`."
                );
                process::exit(1);
            }
        }
        cli::Cmd::Web => {
            #[cfg(feature = "web")]
            {
                run_web_cmd(&opts);
            }
            #[cfg(not(feature = "web"))]
            {
                eprintln!(
                    "error: this build does not include the web report. Rebuild with `--features web`."
                );
                process::exit(1);
            }
        }
        cli::Cmd::Daily
        | cli::Cmd::Weekly
        | cli::Cmd::Monthly
        | cli::Cmd::Session
        | cli::Cmd::Blocks
        | cli::Cmd::Statusline => {
            let tprof = std::env::var_os("CCAUDIT_PROF").is_some();
            let t0 = std::time::Instant::now();
            let source = source::pick(opts.source);
            // clig.dev "be responsive": the very first run on a machine
            // pays the full parse (hundreds of ms on a large history). A
            // one-line note on stderr (never stdout — it must not pollute
            // a pipe) reassures the user it isn't hung. Skipped under
            // --quiet and when the cache already exists.
            maybe_announce_cache_build(source, &opts);
            // blocks --live re-renders on an interval until interrupted;
            // everything else renders once.
            if opts.cmd == cli::Cmd::Blocks && opts.blocks_live {
                run_blocks_live(source, &opts);
            }
            let cache = cache::load(source);
            let t_load = t0.elapsed();
            // Reject a --project that matches nothing (typo, or a stem
            // form from another provider) up front: rendering the
            // unfiltered — or silently empty — totals under the user's
            // filter would misrepresent the data.
            if let Some(name) = opts.project.as_deref() {
                if cache::resolve_project_filter(&cache, Some(name))
                    == cache::ProjectFilter::NoMatch
                {
                    eprintln!("error: unknown project {name:?}");
                    eprintln!("Run `ccaudit session` to list project names.");
                    process::exit(2);
                }
            }
            let render_result = report::render(&cache, &opts, source);
            let t_render = t0.elapsed().saturating_sub(t_load);
            if tprof {
                eprintln!("load={t_load:?}  render={t_render:?}");
            }
            if let Err(e) = render_result {
                // `| head` closing the pipe is a normal exit, not an
                // error. Anything else (ENOSPC, closed fd) must not
                // report success while producing no output.
                if e.kind() == std::io::ErrorKind::BrokenPipe {
                    process::exit(0);
                }
                eprintln!("error: failed to write report: {e}");
                process::exit(1);
            }
            // Skip destructors: we only allocated for this run and the OS
            // reclaims everything on exit. Avoids mimalloc's teardown
            // scan and all the drop-glue for the cache Vecs (~0.3ms).
            process::exit(0);
        }
    }
}

#[cfg(feature = "tui")]
fn run_tui(
    projects: Vec<parse::Project>,
    source: &'static dyn source::Source,
) -> std::io::Result<Option<ui::PostAction>> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // Bracketed paste: without it a paste arrives as individual
    // keypresses, and stray 'q'/'c'/'o' characters execute as commands.
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste
    )?;

    // Restore the terminal on panic. With `panic = "abort"` there is no
    // unwind, so the straight-line teardown below never runs on a panic;
    // the hook runs before the abort and leaves the user's shell usable
    // (cooked mode, main screen) instead of a wrecked raw-mode terminal.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = std::io::stdout();
        let _ = crossterm::execute!(
            out,
            crossterm::event::DisableBracketedPaste,
            crossterm::terminal::LeaveAlternateScreen
        );
        let _ = crossterm::terminal::disable_raw_mode();
        default_hook(info);
    }));

    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let mut app = ui::App::new(projects, source);
    let run_result = app.run(&mut terminal);

    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    run_result?;
    Ok(app.post_action.take())
}

#[cfg(feature = "tui")]
// Writes an exec-failed error to stderr before exiting non-zero.
#[allow(clippy::print_stderr)]
fn resume_session(id: &str, cwd: Option<&str>) {
    use std::process::Command;
    let mut cmd = Command::new("claude");
    let _ = cmd.args(["-r", id]);
    // `claude -r` resolves the conversation under the project derived from
    // the working directory, so resuming any other project's session needs
    // a chdir first. A recorded cwd that no longer exists is skipped —
    // chdir would fail and blame `claude` rather than the stale path.
    if let Some(dir) = cwd {
        if std::path::Path::new(dir).is_dir() {
            let _ = cmd.current_dir(dir);
        } else {
            eprintln!("note: {dir} no longer exists; resuming from the current directory");
        }
    }
    // exec on Unix so the user's shell talks to Claude directly; spawn+wait
    // elsewhere. If `claude` isn't on PATH the exec fails and we print.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("failed to exec claude: {err}");
        process::exit(1);
    }
    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(s) => process::exit(s.code().unwrap_or(0)),
            Err(e) => {
                eprintln!("failed to run claude: {e}");
                process::exit(1);
            }
        }
    }
}

#[cfg(feature = "web")]
// Writes error messages to stderr before exiting non-zero.
#[allow(clippy::print_stderr)]
fn run_web_cmd(opts: &cli::Options) {
    let out_dir = opts
        .out_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .map(|h| h.join(".claude").join("ccaudit-web"))
                .unwrap_or_else(|| std::path::PathBuf::from("ccaudit-web"))
        });

    // Every provider with logs on this machine gets its own payload under
    // `out_dir/{id}/`, so the browser's source dropdown can switch
    // without calling back into ccaudit. `--source` picks which one the
    // bundle opens on (and which one `reset` returns to), not which one
    // gets generated — a bundle with a single provider baked in couldn't
    // offer the switch at all, and `--no-serve` exports have no server to
    // ask for the rest later.
    let mut manifest: Vec<web::SourceEntry> = Vec::new();
    for kind in source::SourceKind::ALL {
        let src = source::pick(kind);
        // One load: `cache` is the same aggregation substrate the CLI
        // usage reports use (daily/monthly/blocks), and
        // `load_all_projects` already builds it internally — returning it
        // avoids a second full scan+validate pass. Web emits its daily
        // rollup from this shared cache rather than re-deriving it from
        // session totals, so the heatmap bucketing matches the usage
        // table — no cross-midnight drift.
        let (projects, cache) = parse::load_all_projects_with_cache(src);
        // A provider with nothing to show is left out of the dropdown
        // rather than offered as a dead end — except the one the user
        // asked for, which must stay selectable so an empty result reads
        // as "no sessions" instead of a silent switch to another
        // provider's data.
        if projects.is_empty() && kind != opts.source {
            continue;
        }
        if let Err(e) = web::generate(&projects, &cache, &out_dir.join(src.id())) {
            eprintln!("error: {e}");
            process::exit(1);
        }
        manifest.push(web::SourceEntry {
            id: src.id(),
            label: src.display_name(),
            sessions: projects.iter().map(|p| p.sessions.len()).sum(),
            cost: projects.iter().map(|p| p.total_cost).sum(),
        });
    }
    if let Err(e) = web::write_shell(&out_dir, source::pick(opts.source).id(), &manifest) {
        eprintln!("error: {e}");
        process::exit(1);
    }
    if opts.no_serve {
        return;
    }
    if let Err(e) = serve::serve(&out_dir, opts.port.unwrap_or(3131)) {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

/// Resolve whether ANSI color should be emitted, once, at startup.
/// Precedence (clig.dev + the `NO_COLOR` spec): explicit off → env off →
/// dumb terminal → env force-on → TTY auto-detect.
fn resolve_color(opts: &cli::Options) -> bool {
    use std::io::IsTerminal as _;
    if opts.no_color || opts.plain {
        return false;
    }
    let env_nonempty = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    if env_nonempty("NO_COLOR") || env_nonempty("CCAUDIT_NO_COLOR") {
        return false;
    }
    if std::env::var_os("TERM").is_some_and(|v| v == "dumb") {
        return false;
    }
    // FORCE_COLOR (node / clig.dev convention): "0" or "false" explicitly
    // disables; any other non-empty value forces color on. `FORCE_COLOR=0`
    // must turn color OFF, not on.
    for k in ["FORCE_COLOR", "CCAUDIT_FORCE_COLOR"] {
        if let Some(v) = std::env::var_os(k) {
            if v.is_empty() {
                continue;
            }
            let s = v.to_string_lossy();
            return !(s == "0" || s.eq_ignore_ascii_case("false"));
        }
    }
    std::io::stdout().is_terminal()
}

/// `clig.dev` "be responsive": on the very first run the cache file
/// doesn't exist yet and the full parse can take a beat. Print a single
/// reassuring line to stderr (never stdout — it must not enter a pipe),
/// only when interactive and not `--quiet`.
#[allow(clippy::print_stderr)]
fn maybe_announce_cache_build(source: &dyn source::Source, opts: &cli::Options) {
    use std::io::IsTerminal as _;
    if opts.quiet || !std::io::stderr().is_terminal() {
        return;
    }
    let missing = source.cache_path().map(|p| !p.exists()).unwrap_or(false);
    if missing {
        eprintln!("building cache (first run; subsequent runs are ~10 ms)…");
    }
}

/// `blocks --live`: re-render the currently-active 5-hour block on a
/// fixed interval until the user hits Ctrl-C (which the default SIGINT
/// handler turns into an immediate exit — clig.dev "let the user
/// escape"). Diverges; callers gate it behind the `--live` check so the
/// normal one-shot path stays reachable.
fn run_blocks_live(source: &'static dyn source::Source, opts: &cli::Options) -> ! {
    use std::io::{IsTerminal as _, Write as _};
    let mut o = opts.clone();
    o.blocks_active = true; // live view always scopes to the active block
    o.blocks_live = false;
    // Only emit the clear-screen escape on a TTY (consistent with the
    // color decision) — piping `blocks --live` into a file or pager
    // shouldn't get raw cursor controls.
    let interactive = std::io::stdout().is_terminal();
    loop {
        let cache = cache::load(source);
        if interactive {
            // Cursor home + clear screen so the table refreshes in place
            // instead of scrolling.
            let stdout = std::io::stdout();
            let _ = stdout.lock().write_all(b"\x1b[H\x1b[2J");
        }
        if report::render(&cache, &o, source).is_err() {
            // Broken pipe or dead terminal — stop refreshing.
            process::exit(0);
        }
        // Use write_all (not println!) so a broken pipe (`| head`) exits
        // cleanly instead of panicking on the next write.
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        if lock
            .write_all(b"\n(refreshing every 2s - Ctrl-C to stop)\n")
            .and_then(|()| lock.flush())
            .is_err()
        {
            process::exit(0);
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// Emit a shell completion script to stdout. Hand-written (the parser is
/// hand-rolled, so there's no clap tree to derive from) but kept in sync
/// with [`cli::Cmd::ALL`] for the subcommand list.
#[allow(clippy::print_stdout)]
fn print_completion(shell: Option<&str>) -> Result<(), String> {
    let shell = shell.ok_or_else(|| {
        "completion needs a shell: `ccaudit completion <bash|zsh|fish>`".to_string()
    })?;
    let subcommands: Vec<&str> = cli::Cmd::ALL.iter().map(|c| c.as_str()).collect();
    let subs = subcommands.join(" ");
    let flags = "--since --until --project --timezone --locale --source --json --plain \
--breakdown --compact --instances --order --tail --carbon --cost-limit --no-cost --logs-dir --config \
--active --recent \
--live --offline --mode --no-color --quiet --version --help --port --out --no-serve";
    let script = match shell {
        "bash" => format!(
            "# ccaudit bash completion. Install: ccaudit completion bash > \
/usr/local/etc/bash_completion.d/ccaudit\n\
_ccaudit() {{\n\
  local cur=\"${{COMP_WORDS[COMP_CWORD]}}\"\n\
  if [[ $COMP_CWORD -eq 1 ]]; then\n\
    COMPREPLY=( $(compgen -W \"{subs}\" -- \"$cur\") )\n\
  else\n\
    COMPREPLY=( $(compgen -W \"{flags}\" -- \"$cur\") )\n\
  fi\n\
}}\n\
complete -F _ccaudit ccaudit\n"
        ),
        "zsh" => format!(
            "#compdef ccaudit\n\
# ccaudit zsh completion. Install: ccaudit completion zsh > ~/.zfunc/_ccaudit\n\
_ccaudit() {{\n\
  local -a subs flags\n\
  subs=({subs})\n\
  flags=({flags})\n\
  if (( CURRENT == 2 )); then\n\
    compadd -- $subs\n\
  else\n\
    compadd -- $flags\n\
  fi\n\
}}\n\
_ccaudit \"$@\"\n"
        ),
        "fish" => {
            use std::fmt::Write as _;
            let mut s = String::from(
                "# ccaudit fish completion. Install: ccaudit completion fish > \
~/.config/fish/completions/ccaudit.fish\n",
            );
            for sub in &subcommands {
                let _ = writeln!(s, "complete -c ccaudit -n __fish_use_subcommand -a {sub}");
            }
            for flag in flags.split_whitespace() {
                let _ = writeln!(s, "complete -c ccaudit -l {}", flag.trim_start_matches('-'));
            }
            s
        }
        other => {
            return Err(format!(
                "unknown shell {other:?} (expected bash, zsh, or fish)"
            ));
        }
    };
    print!("{script}");
    Ok(())
}
