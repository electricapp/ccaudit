#!/usr/bin/env bash
# bench-vs.sh — reproduce the README "Benchmarks" cross-tool table.
#
# Times the *same logical workload* ("daily token report") for ccaudit and the
# three tools it's compared against, on the SAME real ~/.claude/projects
# dataset, and prints a Markdown table in the exact shape used in README.md so
# the result can be pasted straight back in.
#
# This is the counterpart to `benches/bench.rs`:
#   • bench.rs     — ccaudit-only microbenchmarks on a *synthetic* corpus
#                    (parse/cache/aggregate/web internals). Reproducible,
#                    hermetic, but does NOT touch the competitors.
#   • bench-vs.sh  — this file. Cross-tool, on your *real* logs, the numbers
#                    that actually appear on the front page.
#
# States measured (the README's "cold cache" row is intentionally omitted —
# it needs `sudo purge` to evict the OS page cache and can't run unattended):
#   uncached   — each tool's own app-level cache wiped → full re-parse
#   warm cache — cache present, re-run immediately (mmap + page cache hot)
#
# Tools:
#   ccaudit                    built from this checkout (cargo build --release)
#   ccusage                    bunx ccusage daily
#   claude-code-log            uvx claude-code-log (writes HTML, like README)
#   claude-session-dashboard   npx claude-session-dashboard — a SERVER; we time
#                              cold-start-to-first-serve (banner / port ready),
#                              not a CLI report. Labelled as such in the output.
#
# Usage:
#   benches/bench-vs.sh                       # all tools, real ~/.claude, 5 runs
#   RUNS=8 benches/bench-vs.sh                 # more samples
#   TOOLS="ccaudit ccusage" benches/bench-vs.sh
#   PROJECTS=/path/to/.claude/projects benches/bench-vs.sh   # custom dataset
#   JSON_OUT=bench-vs.json benches/bench-vs.sh # also dump raw numbers
#
# Deps: hyperfine, bunx (bun), uvx (uv), npx (node), curl, jq, perl.
set -euo pipefail

# ── Config ─────────────────────────────────────────────────────────────────
PROJECTS="${PROJECTS:-$HOME/.claude/projects}"
RUNS="${RUNS:-5}"
WARMUP="${WARMUP:-1}"
TOOLS="${TOOLS:-ccaudit ccusage claude-code-log claude-session-dashboard}"
JSON_OUT="${JSON_OUT:-}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# The dir that contains `projects/` — i.e. the effective `~/.claude`. ccaudit,
# claude-code-log, ccusage and the dashboard all key off it (some via $HOME).
# Running with a custom PROJECTS works because we point $HOME at its grandparent.
CLAUDE_ROOT="$(cd "$(dirname "$PROJECTS")" && pwd)"
RUN_HOME="$(dirname "$CLAUDE_ROOT")"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/bench-vs.XXXXXX")"
OUT="$WORK/out"; mkdir -p "$OUT"
# The dashboard ships TWO bin names — `claude-session-dashboard` (the npx
# entry) and `claude-dashboard` (what the long-lived node server shows as in
# its argv). Killing only the former leaves the server orphaned, squatting its
# port and hanging every later launch. Match both. (Deliberately NOT killing by
# port — port 3000 may belong to an unrelated dev server.)
kill_dashboard() {
  pkill -9 -f 'claude-session-dashboard' 2>/dev/null || true
  pkill -9 -f 'claude-dashboard' 2>/dev/null || true
}
cleanup() { kill_dashboard; rm -rf "$WORK"; }
trap cleanup EXIT INT TERM

# ── Preflight ──────────────────────────────────────────────────────────────
need() { command -v "$1" >/dev/null 2>&1 || { echo "missing dependency: $1" >&2; exit 1; }; }
for t in $TOOLS; do
  case "$t" in
    ccaudit) ;; # built below
    ccusage) need bunx ;;
    claude-code-log) need uvx ;;
    claude-session-dashboard) need npx; need curl ;;
    *) echo "unknown tool: $t" >&2; exit 1 ;;
  esac
done
need hyperfine; need jq; need perl

[ -d "$PROJECTS" ] || { echo "no dataset at $PROJECTS" >&2; exit 1; }

# ── Dataset stats (for the table caption + drift-proofing) ─────────────────
DS_KB="$(du -sk "$PROJECTS" | awk '{print $1}')"
DS_GB="$(awk -v k="$DS_KB" 'BEGIN{printf "%.1f", k/1024/1024}')"
DS_FILES="$(find "$PROJECTS" -name '*.jsonl' | wc -l | tr -d ' ')"

echo "── bench-vs ────────────────────────────────────────────────────────────"
echo "dataset : $PROJECTS"
echo "        : $DS_GB GB / $DS_FILES JSONL files"
echo "runs    : $RUNS (warmup $WARMUP)   tools: $TOOLS"
echo "work    : $WORK"
echo

# ── Build ccaudit (release) from this checkout ─────────────────────────────
CCAUDIT_BIN=""
if echo "$TOOLS" | grep -qw ccaudit; then
  echo "building ccaudit (release, --all-features) …" >&2
  ( cd "$REPO_ROOT" && cargo build --release --all-features >&2 )
  CCAUDIT_BIN="$REPO_ROOT/target/release/ccaudit"
  [ -x "$CCAUDIT_BIN" ] || { echo "ccaudit binary not found at $CCAUDIT_BIN" >&2; exit 1; }
fi

# ── Cache wipes (the "uncached" prepare step), per tool ────────────────────
wipe_ccaudit()        { rm -rf "$CLAUDE_ROOT/ccaudit-cache" "$CLAUDE_ROOT/ccaudit-web"; }
wipe_ccusage()        { rm -rf "$HOME/.cache/ccusage" 2>/dev/null || true; } # price cache only
wipe_claudecodelog()  { rm -f "$CLAUDE_ROOT/claude-code-log-cache.db"; rm -rf "$OUT/ccl"; }
wipe_dashboard()      { :; } # no documented on-disk cache; reads ~/.claude each boot

# ── Commands (the "daily token report" workload) ───────────────────────────
# All run with HOME=$RUN_HOME so a custom PROJECTS resolves correctly; for the
# default ~/.claude/projects this is just your real HOME.
cmd_ccaudit()        { echo "env HOME='$RUN_HOME' '$CCAUDIT_BIN' daily --no-color"; }
cmd_ccusage()        { echo "env HOME='$RUN_HOME' bunx ccusage@latest daily"; }
# uncached: --no-cache forces a full reprocess every sample. warm: omit it.
# No --combined/--detail flags: the README numbers reflect the DEFAULT run,
# which builds the full per-project + combined HTML tree (that's the "render
# thousands of HTML files" cost the README diagram attributes to this tool).
cmd_claudecodelog()  {
  local extra="$1" # "--no-cache" for uncached, "" for warm
  echo "env HOME='$RUN_HOME' uvx claude-code-log --projects-dir '$PROJECTS' -o '$OUT/ccl' $extra"
}

# ── hyperfine runner for exit-on-completion tools → mean,min seconds ────────
# Emits "<mean> <min>" (seconds). Uses --prepare to wipe before each sample.
hf() { # hf <label> <prepare-cmd|""> <command>
  local label="$1" prep="$2" command="$3"
  local jf="$WORK/hf.${label//\//-}.json"   # '/' in label would be a bad path
  local args=(--shell=bash --runs "$RUNS" --warmup "$WARMUP"
              --export-json "$jf" --command-name "$label")
  [ -n "$prep" ] && args+=(--prepare "$prep")
  hyperfine "${args[@]}" "$command" >&2
  jq -r '.results[0] | "\(.mean) \(.min)"' "$jf"
}

# ── server timer for the dashboard (time-to-first-serve) ───────────────────
# The dashboard is a long-running server, not a CLI that exits — so we time
# cold-start → "Running at http://…" banner, then HARD-kill the whole tree.
# The result is returned via the global DASH_T (never via stdout): a detached
# node server can keep a captured `$(...)` pipe open and hang the parent, and
# a SIGTERM-trapping server can make `wait` block — `kill -9` + a global var
# sidestep both.
DASH_T=""
time_dashboard_once() { # sets DASH_T to elapsed seconds, or "NaN"
  local log="$WORK/dash.log"; : > "$log"
  kill_dashboard
  local start end pid ready=0 i
  start="$(perl -MTime::HiRes=time -e 'printf "%.4f", time')"
  ( env HOME="$RUN_HOME" npx -y claude-session-dashboard </dev/null ) >"$log" 2>&1 &
  pid=$!
  for i in $(seq 1 120); do # up to ~60s to print its ready banner
    if grep -qiE 'Running at http://|listening on|ready' "$log" 2>/dev/null; then ready=1; break; fi
    kill -0 "$pid" 2>/dev/null || break   # server died before becoming ready
    perl -e 'select(undef,undef,undef,0.5)'
  done
  end="$(perl -MTime::HiRes=time -e 'printf "%.4f", time')"
  kill -9 "$pid" 2>/dev/null || true
  kill_dashboard
  wait "$pid" 2>/dev/null || true
  if [ "$ready" = 1 ]; then DASH_T="$(awk -v s="$start" -v e="$end" 'BEGIN{printf "%.3f", e-s}')"
  else DASH_T="NaN"; fi
}

stats_dashboard() { # <warmup-discard> → echoes "<mean> <min>" exactly once
  local warm="$1" i sum=0 n=0 mn=""
  for ((i=0;i<warm;i++)); do time_dashboard_once; done   # discard
  for ((i=0;i<RUNS;i++)); do
    time_dashboard_once
    if [ "$DASH_T" = NaN ]; then echo "NaN NaN"; return 0; fi
    sum="$(awk -v a="$sum" -v b="$DASH_T" 'BEGIN{printf "%.6f", a+b}')"
    if [ -z "$mn" ] || awk -v a="$DASH_T" -v b="$mn" 'BEGIN{exit !(a<b)}'; then mn="$DASH_T"; fi
    n=$((n+1))
  done
  awk -v s="$sum" -v n="$n" -v m="$mn" 'BEGIN{printf "%.3f %s", s/n, m}'
}

# ── Collect (bash 3.2-safe: results in a TSV, no associative arrays) ────────
RESULTS="$WORK/results.tsv"; : > "$RESULTS"
collect() { # <tool> <uncached_mean> <uncached_min> <warm_mean> <warm_min>
  printf '%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" >> "$RESULTS"
}
field() { # <tool> <col 2..5> → value, or "NaN" if absent
  awk -F'\t' -v t="$1" -v c="$2" '$1==t{print $c; f=1} END{if(!f)print "NaN"}' "$RESULTS"
}

for t in $TOOLS; do
  echo ">>> $t" >&2
  case "$t" in
    ccaudit)
      read -r um umin < <(hf "ccaudit/uncached" "rm -rf '$CLAUDE_ROOT/ccaudit-cache' '$CLAUDE_ROOT/ccaudit-web'" "$(cmd_ccaudit)")
      read -r wm wmin < <(hf "ccaudit/warm" "" "$(cmd_ccaudit)")
      collect ccaudit "$um" "$umin" "$wm" "$wmin" ;;
    ccusage)
      read -r um umin < <(hf "ccusage/uncached" "rm -rf '$HOME/.cache/ccusage' 2>/dev/null || true" "$(cmd_ccusage)")
      read -r wm wmin < <(hf "ccusage/warm" "" "$(cmd_ccusage)")
      collect ccusage "$um" "$umin" "$wm" "$wmin" ;;
    claude-code-log)
      read -r um umin < <(hf "ccl/uncached" "rm -f '$CLAUDE_ROOT/claude-code-log-cache.db'; rm -rf '$OUT/ccl'" "$(cmd_claudecodelog --no-cache)")
      # warm: prime the pickle once (warmup does it), then time cache-hit runs.
      # Clear only the HTML output between samples (keep the pickle) so each run
      # is a clean generation — otherwise re-rendering into a populated dir adds
      # confounding overhead and inverts the warm/uncached gap.
      read -r wm wmin < <(hf "ccl/warm" "rm -rf '$OUT/ccl'" "$(cmd_claudecodelog '')")
      collect claude-code-log "$um" "$umin" "$wm" "$wmin" ;;
    claude-session-dashboard)
      # The server path is the flaky one; never let it abort the whole run.
      um=NaN; umin=NaN; wm=NaN; wmin=NaN
      read -r um umin < <(stats_dashboard 1) || true
      read -r wm wmin < <(stats_dashboard 1) || true
      collect claude-session-dashboard "$um" "$umin" "$wm" "$wmin" ;;
  esac
done

# ── Render the README-shaped Markdown table ────────────────────────────────
fsec() { # format seconds like the README (≥10 → 1dp, else 2dp); "—" for NaN
  awk -v v="$1" 'BEGIN{ if(v=="NaN"||v==""){print "—"} else if(v>=10){printf "%.1f s", v} else {printf "%.2f s", v} }'
}
label_for() { # <tool>
  case "$1" in
    ccaudit)                  echo '**ccaudit**' ;;
    ccusage)                  echo 'ccusage (`bunx`)' ;;
    claude-code-log)          echo 'claude-code-log (`uvx`)' ;;
    claude-session-dashboard) echo 'claude-session-dashboard (`npx`)*' ;;
  esac
}
row() { # <tool>
  local t="$1" name u w
  name="$(label_for "$t")"
  u="$(fsec "$(field "$t" 2)")"; w="$(fsec "$(field "$t" 4)")"
  # bold ccaudit's winning (warm) cell, matching README emphasis
  [ "$t" = ccaudit ] && w="**$w**"
  printf '| %-33s | %8s | %10s |\n' "$name" "$u" "$w"
}

echo
echo "════════════════════════════════════════════════════════════════════════"
echo "Paste-ready table (mean wall time, $RUNS runs) — dataset $DS_GB GB / $DS_FILES files:"
echo
echo "|                                   | uncached | warm cache |"
echo "| --------------------------------- | -------: | ---------: |"
for t in ccaudit ccusage claude-code-log claude-session-dashboard; do
  echo "$TOOLS" | grep -qw "$t" && row "$t"
done
echo
echo "*\`claude-session-dashboard\` is a server; its column is cold-start→first-serve (banner ready), not a CLI report."
echo

# ── Raw JSON sidecar (mean + min per cell) ─────────────────────────────────
if [ -n "$JSON_OUT" ]; then
  jnum() { local v; v="$(field "$1" "$2")"; [ "$v" = NaN ] && echo null || echo "$v"; }
  {
    echo "{"
    echo "  \"dataset\": {\"path\": \"$PROJECTS\", \"gb\": $DS_GB, \"files\": $DS_FILES},"
    echo "  \"runs\": $RUNS, \"results\": {"
    first=1
    for t in $TOOLS; do
      [ $first = 1 ] || echo ","; first=0
      printf '    "%s": {"uncached_mean": %s, "uncached_min": %s, "warm_mean": %s, "warm_min": %s}' \
        "$t" "$(jnum "$t" 2)" "$(jnum "$t" 3)" "$(jnum "$t" 4)" "$(jnum "$t" 5)"
    done
    echo; echo "  }"
    echo "}"
  } > "$JSON_OUT"
  echo "raw numbers → $JSON_OUT" >&2
fi
