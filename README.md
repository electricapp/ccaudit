# ccaudit

Fast, local Claude Code log viewer. CLI, TUI, and static web — Rust binary, ~1.7 MB.

<p align="center">
  <img src="docs/dashboard.png" alt="ccaudit web dashboard" width="900">
</p>

<p align="center">
  <img src="docs/cli-daily.png" alt="ccaudit daily CLI output" width="900">
</p>

```
ccaudit daily                         # token usage + cost, today and lately
ccaudit tui                           # interactive terminal browser
ccaudit web                           # generate + serve the web dashboard
ccaudit statusline                    # one-line summary for your shell prompt
```

## What it is

One Rust binary. CLI, TUI, and web for your `~/.claude` logs. Mmap'd cache, ~5 ms warm start.

Inspired by:

- [ccusage](https://github.com/ryoppippi/ccusage)
- [claude-code-log](https://github.com/daaain/claude-code-log)
- [claude-session-dashboard](https://github.com/dlupiak/claude-session-dashboard)

|                       | ccaudit               | ccusage        | claude-code-log    | claude-session-dashboard |
| --------------------- | --------------------- | -------------- | ------------------ | ------------------------ |
| Runtime               | Rust binary (~1.7 MB) | Node.js        | Python             | Node.js + browser        |
| CLI reports           | yes (daily/monthly/…) | reference impl | —                  | —                        |
| TUI browser           | yes (`ccaudit tui`)   | —              | —                  | —                        |
| Web dashboard         | yes (`ccaudit web`)   | —              | HTML export        | yes (local server)       |
| Session detail viewer | yes                   | —              | yes                | yes                      |
| Install               | binary / npm / cargo  | npm            | `pip install` + py | `npx` / `npm install -g` |

`claude-session-dashboard` has one thing ccaudit doesn't: an **agent-delegation Gantt chart** for the sub-agent dispatch tree per session. Worth it if you run agentic workflows and want to see the delegation order.

### Benchmarks

Same dataset, same workload (`daily` token report). Single-shot wall time on Apple Silicon, against `~/.claude/projects/` = **2.5 GB / 1161 JSONL files**. Reproduce with [`benches/bench-vs.sh`](benches/bench-vs.sh) — the table below is its output.

|                                    |   uncached |  warm cache |
| ---------------------------------- | ---------: | ----------: |
| **ccaudit**                        | **0.14 s** | **0.005 s** |
| ccusage (`bunx`)                   |      7.1 s |       7.4 s |
| claude-code-log (`uvx`)            |       99 s |       168 s |
| claude-session-dashboard (`npx`) † |      3.6 s |       3.6 s |

- **uncached** — app-level cache wiped, forcing a full re-parse from JSONL
- **warm cache** — re-run immediately afterwards; mmap + page cache hot

† `claude-session-dashboard` is a local server, not a CLI report — its figure is cold-start to first-serve (ready banner). Two quirks the reproducible run surfaces: `ccusage` keeps no persistent parse cache (re-parses every run, so warm ≈ uncached), and `claude-code-log`'s Python pickle cache is _counterproductive_ at this corpus size — a warm re-run is slower than a no-cache one. Competitor tools move fast; re-run the script for current numbers.

### Where the time goes

Same input (`~/.claude/projects/`), same logical question ("daily token totals"). Here's what each tool does for each invocation:

```
ccaudit daily — warm cache (~5 ms)
─────────────────────────────────────────────
  mmap claude-code.db / codex.db  (2 MB binary, already bucketed by day × model)
       │
       ▼
  sum PreAgg cells
       │
       ▼
  print table
```

```
ccaudit daily — uncached (~140 ms, pays the cost ONCE per file lifetime)
─────────────────────────────────────────────
  walk ~/.claude/projects → 1161 files
       │
       ▼
  rayon-parallel parse JSONL  (318 K lines, typed serde)
       │
       ▼
  bucket by (day × model)
       │
       ▼
  ┌────────────────────────────┐
  │ persist binary cache.db    │ ◄── pay once,
  └────────┬───────────────────┘     all future runs are ~5 ms
           ▼
  mmap + sum + print
```

```
ccusage daily (~7 s, EVERY SINGLE RUN)
─────────────────────────────────────────────
  ┌────────────────────────────────────────────┐
  │ ⏬ HTTP GET litellm/prices.json   (~1 MB)   │ ◄── network call,
  └────────┬───────────────────────────────────┘     every run
           ▼
  parse pricing JSON (~2900 models)
           │
           ▼
  walk ~/.claude/projects → 1161 files
           │
           ▼
  ┌────────────────────────────────────────┐
  │ for each of 318 K lines:               │
  │   ├── read line from disk              │
  │   ├── JSON.parse(...)                  │ ◄── re-parse every run
  │   ├── lookup model in pricing map      │
  │   ├── tokens × cents                   │
  │   └── push into per-day bucket         │
  └────────┬───────────────────────────────┘
           ▼
       print table
```

```
claude-code-log (~99 s uncached / ~168 s warm)
─────────────────────────────────────────────
  load Python pickle cache (if present)
           │
           ▼
  walk ~/.claude/projects → 1161 files
           │
           ▼
  ┌────────────────────────────────────────────┐
  │ for each .jsonl:                           │
  │   ├── parse JSONL                          │
  │   ├── build parentUuid → child DAG         │
  │   ├── ⚠ orphan-node "promote to root"      │ ◄── hundreds
  │   ├── ⚠ multi-root fallback                │     of these
  │   └── ⚠ DAG incomplete → timestamp resort  │
  └────────┬───────────────────────────────────┘
           ▼
  ┌────────────────────────────────────────────┐
  │ for each session:                          │
  │   └── jinja2 template → write .html file   │ ◄── thousands of
  └────────┬───────────────────────────────────┘     output files
           ▼
  write combined_transcripts.html
           │
           ▼
  update pickle cache
```

The redundancy in one row:

| once per run                             | ccaudit (warm) | ccusage | claude-code-log |
| ---------------------------------------- | :------------: | :-----: | :-------------: |
| stat 1161 JSONL files                    |       —        |    ✓    |        ✓        |
| parse 318 K JSONL lines                  |       —        |    ✓    |        ✓        |
| build per-message parent→child DAG       |       —        |    —    |        ✓        |
| render thousands of HTML files           |       —        |    —    |        ✓        |
| HTTP-fetch LiteLLM prices                |       —        |    ✓    |        —        |
| look up model prices, multiply by tokens |       —        |    ✓    |        —        |
| sum already-bucketed numbers + print     |       ✓        |    ✓    |        —        |

ccaudit does the top six rows **once**, when it first sees a file, persists the result to a ~2 MB binary cache, and every subsequent run is just the bottom row.

### Cache layout

ccaudit keeps two purpose-built caches under `~/.claude/ccaudit-cache/`. CLI reports, the TUI, and the web view each consume the one shaped for what they actually need:

```
~/.claude/ccaudit-cache/
├── claude-code.db           ◄── aggregation cache (CLI reports)
│                                bucketed by (day × model × project),
│                                mmap'd, zero deserialization on read
│
├── codex.db                 ◄── same shape, per --source provider
│
├── <hash>.meta              ◄── per-session cache (TUI + web)
└── <hash>.bin                   one pair per JSONL file —
                                 full parsed Session struct, every
                                 message, every tool call, postcard-encoded
                                 (validated by mtime + size + version)
```

**CLI** (`daily` / `monthly` / `session` / `blocks` / `statusline`) reads only `<source>.db` — no per-message data needed for token totals.

**TUI** (`ccaudit tui`) reads only the per-session `.bin` files. On startup, `load_all_projects()` walks every JSONL; for each file it stat's `mtime + size`, hits the matching `.bin` if those match the meta, and postcard-deserializes straight into a `Session`. A miss triggers a reparse + cache rewrite for next time. Once everything is in memory, navigation, search, dashboard, scope filters — all in-memory, zero IO. Pressing `c` exec's `claude -r <id>` against the current session.

**Web** (`ccaudit web`) reuses the same per-session cache for the parse step, then materializes the browser-side cache as static files under `~/.claude/ccaudit-web/`:

```
~/.claude/ccaudit-web/
├── index.html
├── app.js  /  style.css  /  util.js     ◄── single-file bundle (no build step)
├── index.json                            ◄── one fetch:
│                                              project + session metadata,
│                                              hourly histograms, tool counts,
│                                              + a daily rollup pulled straight
│                                                from <source>.db so the heatmap
│                                                can't drift from the CLI
├── search.json                           ◄── word → session posting list
└── s/
    └── <pi>_<si>.json                    ◄── per-session message tree,
                                              lazy-loaded by the browser
                                              only when you open that session
```

The browser fetches `index.json` once and renders the dashboard / table from it. Opening a session is one HTTP GET for `<pi>_<si>.json`; repeat opens hit the browser's HTTP cache. The bundled HTTP server is a 10-line static file handler, no API.

End result: writing a daily report, scrolling the TUI, and clicking through 200 sessions in the web view all share the same parsed-once-on-disk substrate. The only thing that ever re-parses a JSONL file is a real change to that file (mtime or size moves).

## Install

Ephemeral (no install — runs the latest release once):

```bash
npx ccaudit              # Node / npm
bunx ccaudit             # Bun
uvx ccaudit              # uv / Astral
```

Permanent:

```bash
npm install -g ccaudit           # npm / yarn / pnpm
cargo install ccaudit            # cargo (from crates.io)
pipx install ccaudit             # pipx (Python)
uv tool install ccaudit          # uv tool

# from source
git clone https://github.com/electricapp/ccaudit && cd ccaudit
cargo install --path . --locked
```

All five package managers resolve to the same pre-built platform
binary — there's no Node or Python runtime involvement at execution
time, the Python/JS shims just locate and `exec` the binary.

## Quickstart

```bash
ccaudit                                        # daily report (default)
ccaudit weekly --breakdown                     # weekly totals, split per model
ccaudit blocks --cost-limit 100                # 5-hour billing windows w/ progress bar
ccaudit session --breakdown                    # per-session per-model cost
ccaudit daily --plain | awk -F'\t' '{print $1, $NF}'   # scriptable, tab-separated
ccaudit web --port 8080                        # static site + local server
ccaudit web --no-serve --out ./site            # generate the static site, exit (CI / scripts)
ccaudit tui                                    # interactive browser
ccaudit daily --source codex                   # OpenAI Codex CLI logs (~/.codex/sessions)
```

Report subcommands accept `--json` (structured) or `--plain` (tab-separated) for machine-readable output. See [Scripting & ccusage parity](#scripting--ccusage-parity).

## Features

- **Daily / weekly / monthly / session / blocks** reports, plus a compact `statusline` for shell prompts
- **TUI browser** — keyboard-driven navigation, fuzzy search, message viewer, dashboard (`d`), resume (`c`)
- **Web dashboard** — tables with sortable columns, pie/histogram/heatmap charts, full message viewer, URL routing (`/p/{slug}/s/{uuid}`)
- **Scope-aware**: press `d` from any view and the dashboard reflects just that project or session
- **Per-token-type cost breakdown** on hover
- **Accurate pricing** — fetches latest rates from LiteLLM on demand (`ccaudit refresh-prices`)
- **Carbon footer** (`--carbon`) — energy / CO₂ / tree-year estimate for the reported window
- **Deterministic filters** — `--since YYYYMMDD`, `--until`, `--project`, `--timezone`, `--locale`, `--source`
- **mmap'd cache** — repeated runs on the same `~/.claude/projects/` read from a memory-mapped schema, zero deserialization
- **Pluggable sources** — a `Source` trait (see `src/source/mod.rs`) abstracts log discovery, parsing, model pricing, and model normalization. Today ships **Claude Code** (`--source claude-code`, default) and **OpenAI Codex CLI** (`--source codex`); the trait accepts adapters for OpenCode, π / Pi, MCP servers, or any other agent that writes JSONL-shaped session logs.
- **Single source of truth** — CLI `daily`, web sessions table, and web heatmap all read from one `cache::per_session_totals` pipeline. A test (`tests/uniformity.rs`) asserts they agree to the cent on every run.

## Optional features

```bash
cargo install ccaudit --features simd-json     # ~30% faster JSONL parse, +300 KB binary
cargo install ccaudit --features locale        # locale-aware date formatting (chrono unstable-locales)
cargo install ccaudit --features full          # tui + web + locale (default omits locale)
```

## TUI keybindings

| key                | action                                    |
| ------------------ | ----------------------------------------- |
| `↑`/`↓` or `k`/`j` | move selection                            |
| `Enter` or `→`     | open                                      |
| `←` or `Esc`       | back                                      |
| `/`                | search                                    |
| `d`                | toggle dashboard (scoped to current view) |
| `c`                | `claude -r <id>` — resume this session    |
| `o`                | open the web view                         |
| `r`                | reset all filters / scope                 |
| `q`                | quit                                      |

## Web dashboard keys

| key             | action                           |
| --------------- | -------------------------------- |
| `d`             | toggle scoped dashboard          |
| `m` / `t`       | pie: by model / by tool          |
| `h` / `p` / `y` | histogram: hour / project / day  |
| `l`             | histogram log/linear scale       |
| `r`             | reset all filters / sort / scope |
| `j`/`k`         | row navigation                   |

## Subcommands

```
ccaudit daily           daily token usage + cost          (default)
ccaudit weekly          aggregate by week (Mon-anchored)
ccaudit monthly         aggregate by month
ccaudit session         aggregate by conversation session
ccaudit blocks          5-hour billing windows, with active detection
ccaudit statusline      compact one-line summary (for terminal status bars)
ccaudit tui             interactive TUI browser
ccaudit web             generate static site + serve
ccaudit refresh-prices  fetch latest model prices from LiteLLM
ccaudit completion SH   print a shell completion script (bash/zsh/fish)
ccaudit version         print the version (also --version / -V)
ccaudit help [SUB]      show help for ccaudit or for a subcommand
```

Run `ccaudit <COMMAND> --help` for command-specific flags.

## Scripting & ccusage parity

ccaudit aims for rough usability parity with [ccusage](https://github.com/ryoppippi/ccusage), so the muscle memory carries over:

```bash
ccaudit daily --json | jq .totals.cost_usd     # structured output for jq
ccaudit daily --plain | awk -F'\t' '{print $1, $NF}'   # tab-separated for awk/cut/grep
ccaudit monthly --order desc                    # newest first (default: oldest first)
ccaudit blocks --active                         # only the live 5-hour window
ccaudit blocks --recent                         # only blocks from the last 3 days
ccaudit blocks --live                           # refresh the active block until Ctrl-C
ccaudit completion zsh > ~/.zfunc/_ccaudit      # shell completions
```

- `--json` / `--plain` make output machine-readable; `--plain` is tab-separated with raw integers and no box-drawing or color.
- `--order asc|desc`, `--offline`, and `--mode auto|calculate|display` are accepted for ccusage compatibility. ccaudit prices from its local cache on every run (update it online with `refresh-prices`), so `--offline` is already the default and `--mode display` falls back to calculated costs.
- **Color** follows the [`NO_COLOR`](https://no-color.org) convention: ANSI color is emitted only when stdout is a terminal, and is disabled by `--no-color`, `NO_COLOR`, `CCAUDIT_NO_COLOR`, or `TERM=dumb` (force it back on with `FORCE_COLOR`). Piped output is always clean.
- Mistyped a command or flag? ccaudit suggests the closest match (`ccaudit dialy` → _did you mean `ccaudit daily`?_).

## Development

```bash
cargo test --release --all-features              # 74 tests across 5 suites
cargo bench                                      # synthetic-corpus bench (ccaudit internals)
BENCH_SIZE=large BENCH_RUNS=10 \
  cargo bench                                    # scaling stress
BENCH_SAVE=baseline.json    cargo bench
BENCH_COMPARE=baseline.json cargo bench          # diff vs baseline
benches/bench-vs.sh                              # cross-tool wall-time table (vs ccusage / claude-code-log / dashboard)
```

The bench builds a deterministic JSONL fixture in a tempdir (no
dependency on `~/.claude/projects/`) and times the macro paths
(`parse cold`, `parse warm`, `cache rebuild`, `cache warm mmap`,
`cache::aggregate day` & `session`, `web::generate`) plus a few inner
loops (`parse_session`, `split_whitespace`, `Searcher::score`, `fnv1a`).

## License

MIT. See [LICENSE](LICENSE).
