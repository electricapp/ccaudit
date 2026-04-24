// mimalloc is noticeably faster on macOS than the system allocator for
// allocation-heavy startup (HashMap rebuilds, small String intern).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod cache;
mod cli;
mod parse;
mod report;
mod source;
mod style;

#[cfg(feature = "tui")]
mod search;
#[cfg(feature = "tui")]
#[allow(clippy::indexing_slicing)]
mod ui;

#[cfg(feature = "web")]
mod serve;
#[cfg(feature = "web")]
#[allow(clippy::indexing_slicing)]
mod web;

use std::process;

// `main` is the one place the binary gets to write to stdio directly:
// errors to stderr, `refresh-prices` summary + profiling to stderr/stdout.
#[allow(clippy::print_stdout, clippy::print_stderr)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let opts = match cli::parse(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!();
            cli::print_help();
            process::exit(2);
        }
    };

    match opts.cmd {
        cli::Cmd::Help => match opts.help_target {
            Some(target) => cli::print_subcommand_help(target),
            None => cli::print_help(),
        },
        cli::Cmd::RefreshPrices => match source::prices::refresh(source::pick(opts.source)) {
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
                let projects = parse::load_all_projects();
                match run_tui(projects) {
                    Ok(Some(ui::PostAction::Resume(id))) => {
                        resume_session(&id);
                    }
                    Ok(Some(ui::PostAction::OpenWeb)) => {
                        run_web_cmd(&opts);
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
        | cli::Cmd::Monthly
        | cli::Cmd::Session
        | cli::Cmd::Blocks
        | cli::Cmd::Statusline => {
            let tprof = std::env::var_os("CCAUDIT_PROF").is_some();
            let t0 = std::time::Instant::now();
            let source = source::pick(opts.source);
            let cache = cache::load(source);
            let t_load = t0.elapsed();
            report::render(&cache, &opts, source);
            let t_render = t0.elapsed().saturating_sub(t_load);
            if tprof {
                eprintln!("load={t_load:?}  render={t_render:?}");
            }
            // Skip destructors: we only allocated for this run and the OS
            // reclaims everything on exit. Avoids mimalloc's teardown
            // scan and all the drop-glue for the cache Vecs (~0.3ms).
            process::exit(0);
        }
    }
}

#[cfg(feature = "tui")]
fn run_tui(projects: Vec<parse::Project>) -> std::io::Result<Option<ui::PostAction>> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let mut app = ui::App::new(projects);
    let run_result = app.run(&mut terminal);

    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    run_result?;
    Ok(app.post_action.take())
}

#[cfg(feature = "tui")]
// Writes an exec-failed error to stderr before exiting non-zero.
#[allow(clippy::print_stderr)]
fn resume_session(id: &str) {
    use std::process::Command;
    // Replace our process with `claude -r <id>` on Unix so the user's
    // shell ends up talking to Claude directly. Falls back to spawn+wait
    // elsewhere. If `claude` isn't on PATH the exec fails and we print.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = Command::new("claude").args(["-r", id]).exec();
        eprintln!("failed to exec claude: {err}");
        process::exit(1);
    }
    #[cfg(not(unix))]
    {
        let status = Command::new("claude").args(["-r", id]).status();
        match status {
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
    let projects = parse::load_all_projects();
    // `cache` is the same aggregation substrate the CLI usage reports use
    // (daily/monthly/blocks). Web now emits its daily rollup from this
    // shared cache rather than re-deriving it from session totals, so the
    // heatmap bucketing matches the usage table — no cross-midnight drift.
    let source = source::pick(opts.source);
    let cache = cache::load(source);
    let out_dir = opts
        .out_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .map(|h| h.join(".claude").join("ccaudit-web"))
                .unwrap_or_else(|| std::path::PathBuf::from("ccaudit-web"))
        });
    if let Err(e) = web::generate(&projects, &cache, source, &out_dir) {
        eprintln!("error: {e}");
        process::exit(1);
    }
    if let Err(e) = serve::serve(&out_dir, opts.port.unwrap_or(3131)) {
        eprintln!("error: {e}");
        process::exit(1);
    }
}
