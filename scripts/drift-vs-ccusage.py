# /// script
# requires-python = ">=3.11"
# dependencies = ["typer>=0.15", "rich>=13.9"]
# ///
"""Diff ccaudit's report against ccusage's over the same log corpus.

ccusage is a differential oracle, not ground truth. It has been right
where ccaudit was wrong — it split cache writes by TTL first — and the
two also diverge on purpose: ccaudit reads subagent transcripts, dedups
across sessions, and drops `<synthetic>` compaction lines. So drift is a
signal to investigate, never a defect on its own, and a breach exits
non-zero purely as an advisory the workflow is expected to swallow.

Token columns are held to a much tighter bound than cost: identical
inputs should produce identical counts, whereas costs legitimately move
whenever the two tools disagree about a model's rate.

    uv run scripts/drift-vs-ccusage.py ccusage.json ccaudit.json --seed 42
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Annotated, Final

import typer
from rich.console import Console
from rich.table import Table

# Counting the same logs should agree almost exactly; anything above this
# means one tool is reading lines the other isn't.
TOKEN_TOLERANCE_PCT: Final[float] = 1.0
# Costs ride on two independently maintained rate tables, so they drift
# whenever a new model lands in one before the other.
COST_TOLERANCE_PCT: Final[float] = 10.0

console: Final[Console] = Console()
# Markdown for the job summary: GitHub renders markdown, not the ANSI
# that rich emits, so the summary is composed separately rather than
# exported from the console.
err: Final[Console] = Console(stderr=True)


@dataclass(frozen=True)
class Field:
    label: str
    ccusage_key: str
    ccaudit_key: str
    tolerance: float


FIELDS: Final[tuple[Field, ...]] = (
    Field("input", "inputTokens", "input", TOKEN_TOLERANCE_PCT),
    Field("output", "outputTokens", "output", TOKEN_TOLERANCE_PCT),
    Field("cache_create", "cacheCreationTokens", "cache_create", TOKEN_TOLERANCE_PCT),
    Field("cache_read", "cacheReadTokens", "cache_read", TOKEN_TOLERANCE_PCT),
    Field("cost_usd", "totalCost", "cost_usd", COST_TOLERANCE_PCT),
)


def drift_pct(theirs: float, ours: float) -> float:
    if theirs == 0:
        return 0.0 if ours == 0 else 100.0
    return 100.0 * (ours - theirs) / theirs


def main(
    ccusage: Annotated[Path, typer.Argument(help="ccusage `daily --json` output")],
    ccaudit: Annotated[Path, typer.Argument(help="ccaudit `daily --json` output")],
    seed: Annotated[str, typer.Option(help="Corpus seed, echoed so a run reproduces")] = "?",
) -> None:
    theirs = json.loads(ccusage.read_text(encoding="utf-8"))
    ours = json.loads(ccaudit.read_text(encoding="utf-8"))

    breaches: list[str] = []
    md: list[str] = [f"Corpus seed: `{seed}`", "", "| field | ccusage | ccaudit | drift | tolerance |", "|---|---:|---:|---:|---:|"]

    table = Table(title=f"ccusage drift (seed {seed})", header_style="bold")
    table.add_column("field")
    table.add_column("ccusage", justify="right")
    table.add_column("ccaudit", justify="right")
    table.add_column("drift", justify="right")
    table.add_column("tolerance", justify="right")

    for f in FIELDS:
        t = theirs["totals"].get(f.ccusage_key, 0)
        o = ours["totals"].get(f.ccaudit_key, 0)
        d = drift_pct(t, o)
        over = abs(d) > f.tolerance
        style = "red" if over else "green"
        table.add_row(
            f.label,
            f"{t:,.2f}",
            f"{o:,.2f}",
            f"[{style}]{d:+.2f}%[/{style}]",
            f"±{f.tolerance:.0f}%",
        )
        md.append(
            f"| {f.label} | {t:,.2f} | {o:,.2f} | {d:+.2f}%{' ⚠' if over else ''} | ±{f.tolerance:.0f}% |"
        )
        if over:
            breaches.append(f"{f.label} drifted {d:+.2f}% (tolerance ±{f.tolerance:.0f}%)")

    console.print(table)

    their_days = {r["date"]: r for r in theirs.get("daily", [])}
    our_days = {r["key"]: r for r in ours.get("rows", [])}
    console.print(f"Days: ccusage [bold]{len(their_days)}[/bold], ccaudit [bold]{len(our_days)}[/bold]")
    md += ["", f"Days: ccusage {len(their_days)}, ccaudit {len(our_days)}"]

    for name, missing in (
        ("ccaudit", sorted(set(their_days) - set(our_days))),
        ("ccusage", sorted(set(our_days) - set(their_days))),
    ):
        if missing:
            shown = ", ".join(missing[:8])
            console.print(f"[red]{len(missing)} day(s) missing from {name}:[/red] {shown}")
            md.append(f"- {len(missing)} day(s) missing from {name}: {shown}")
            breaches.append(f"{len(missing)} day(s) missing from {name}")

    worst = sorted(
        (
            (abs(drift_pct(their_days[d]["totalCost"], our_days[d]["cost_usd"])), d)
            for d in set(their_days) & set(our_days)
        ),
        reverse=True,
    )
    if worst:
        per_day = Table(title="Widest per-day cost drift", header_style="bold")
        per_day.add_column("day")
        per_day.add_column("ccusage", justify="right")
        per_day.add_column("ccaudit", justify="right")
        per_day.add_column("drift", justify="right")
        md += ["", "Widest per-day cost drift:", "", "| day | ccusage | ccaudit | drift |", "|---|---:|---:|---:|"]
        for _, day in worst[:5]:
            t = their_days[day]["totalCost"]
            o = our_days[day]["cost_usd"]
            per_day.add_row(day, f"${t:,.2f}", f"${o:,.2f}", f"{drift_pct(t, o):+.1f}%")
            md.append(f"| {day} | ${t:,.2f} | ${o:,.2f} | {drift_pct(t, o):+.1f}% |")
        console.print(per_day)

    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as fh:
            fh.write("## ccusage drift\n\n" + "\n".join(md) + "\n")

    if breaches:
        for b in breaches:
            # A GitHub annotation, so the run surfaces the reason without
            # anyone opening the log.
            console.print(f"::warning title=ccusage drift::{b}")
        err.print(f"[bold red]ADVISORY[/bold red]: {len(breaches)} tolerance breach(es).")
        raise typer.Exit(code=1)

    console.print("[green]Within tolerance on every field.[/green]")


if __name__ == "__main__":
    typer.run(main)
