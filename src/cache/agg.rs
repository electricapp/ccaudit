// Aggregation: cache + filter + bucket → FxHashMap<BreakdownKey, BucketUsage>.
//
// Two paths:
//   • UTC day/week/month → summed directly from pre-aggregated rows. No
//     per-line walk, no dedup hashset, zero provider calls.
//   • Everything else (session, block, non-UTC) → walks per-line data
//     with cross-session dedup, pricing each row via `source.price_columns`.

use super::load::LoadedCache;
use crate::source::{Source, day_to_date};
use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};

/// How an aggregation groups its rows.
#[derive(Clone, Copy, Debug)]
pub enum Bucket {
    Day,
    Week, // Monday-anchored calendar weeks
    Month,
    Session,
    Block, // 5-hour billing windows aligned to unix epoch
}

/// Monday-anchored week index for a day-since-epoch. 1970-01-01 was a
/// Thursday (day 0); `+3` shifts the week boundary so that each bucket
/// runs Monday→Sunday. The matching label start-day is `index*7 - 3`.
pub const fn week_index(day: i32) -> i64 {
    ((day as i64) + 3).div_euclid(7)
}

/// Inverse of [`week_index`]: the day-since-epoch of that week's Monday.
pub const fn week_start_day(index: i64) -> i32 {
    (index * 7 - 3) as i32
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod week_tests {
    use super::*;
    use crate::source::day_to_date;

    // day-since-epoch for a few known dates (UTC):
    //   1970-01-01 = day 0 (Thursday)
    //   2026-06-01 = Monday; 2026-06-07 = Sunday (same week)
    fn day_of(y: i32, m: u32, d: u32) -> i32 {
        use chrono::NaiveDate;
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .signed_duration_since(epoch)
            .num_days() as i32
    }

    #[test]
    fn week_groups_monday_through_sunday() {
        let mon = day_of(2026, 6, 1);
        let sun = day_of(2026, 6, 7);
        let next_mon = day_of(2026, 6, 8);
        assert_eq!(week_index(mon), week_index(sun), "Mon..Sun share a week");
        assert_ne!(
            week_index(sun),
            week_index(next_mon),
            "next Mon is a new week"
        );
    }

    #[test]
    fn week_start_day_is_the_monday() {
        for &(y, m, d) in &[(2026, 6, 1), (2026, 6, 4), (2026, 6, 7)] {
            let idx = week_index(day_of(y, m, d));
            let start = day_to_date(week_start_day(idx));
            use chrono::Datelike;
            assert_eq!(
                start.weekday(),
                chrono::Weekday::Mon,
                "week start for {y}-{m}-{d} must be a Monday, got {start}"
            );
        }
    }

    #[test]
    fn week_index_round_trips_through_start() {
        let idx = week_index(day_of(2026, 6, 3));
        assert_eq!(week_index(week_start_day(idx)), idx);
    }
}

/// Inclusive date / project / timezone filter applied during aggregation.
#[derive(Clone, Debug, Default)]
pub struct FilterOpts<'a> {
    pub since_day: Option<i32>, // inclusive
    pub until_day: Option<i32>, // inclusive
    pub project: Option<&'a str>,
    pub tz_offset_secs: i32, // used for block/session bucketing by local time
}

// Opaque bucket key; rendering turns it into a label based on the
// Bucket it came from.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct BucketKey(pub i64);

impl BucketKey {
    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

// Composite key used when --breakdown is on. Model is u16::MAX for
// "no breakdown" rows.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct BreakdownKey(pub BucketKey, pub u16);

// 8-byte bitset over u16 ids — fits 64 distinct values. Models in a
// real Claude Code history land around ~15, projects around ~50; 64 is
// comfortable headroom. IDs >= 64 are dropped from the displayed set
// (totals stay correct since they're tracked elsewhere).
#[derive(Clone, Copy, Default, Debug)]
pub struct U64Bitset(u64);

impl U64Bitset {
    pub const fn insert(&mut self, id: u16) {
        if id < 64 {
            self.0 |= 1u64 << id;
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        let v = self.0;
        (0u16..64).filter(move |i| (v >> i) & 1 != 0)
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct BucketUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_create: u64,
    pub cost: f64,
    // Per-column dollar costs, carried alongside the total so renderers
    // can show the column split ("Total Prices" row) summed over exactly
    // the same rows that produced `cost` — honoring tz/filter/--tail
    // trimming instead of re-deriving from the unfiltered preagg table.
    pub cost_input: f64,
    pub cost_output: f64,
    pub cost_cache_create: f64,
    pub cost_cache_read: f64,
    pub models: U64Bitset,
    pub projects: U64Bitset,
    pub line_count: u32,
    pub last_ts: i64,
}

/// Length of a billing block in seconds. Public so the report layer
/// (table/json/statusline) can compute "is this block currently active?"
/// against the same constant the aggregator buckets by.
pub const BLOCK_SECS: i64 = 5 * 3600;

/// Per-session token + cost totals.
///
/// Single source of truth — produced by the same dedup/skip-model/pricing
/// pipeline that powers the CLI reports, so anything that displays
/// "session X cost $Y" agrees with `ccaudit daily` to the cent.
#[derive(Clone, Copy, Default, Debug)]
pub struct SessionTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_create: u64,
    pub cost: f64,
    pub cost_input: f64,
    pub cost_output: f64,
    pub cost_cache_read: f64,
    pub cost_cache_create: f64,
    /// Unix ts of the last billable line. Canonical "last activity".
    pub last_ts: i64,
}

/// Build per-session totals from the cache.
///
/// Walks lines once with the canonical cross-session msg-id dedup,
/// skip-model filter, and per-model pricing. Returned map is keyed by
/// `SessionEntry::path_hash` so callers can match by source-file path
/// without paying for a string lookup.
pub fn per_session_totals<S: Source + ?Sized>(
    cache: &LoadedCache,
    source: &S,
) -> FxHashMap<u64, SessionTotals> {
    let sessions_s = cache.sessions();
    let lines_s = cache.lines();
    let ts_s = cache.ts_unix();

    // Cross-session dedup hashset shared across the whole walk so a
    // checkpointed message_id appearing in multiple sessions only counts
    // once (toward the earliest session, since sessions are stored
    // chronologically at build time).
    let mut seen: FxHashSet<u64> =
        FxHashSet::with_capacity_and_hasher(lines_s.len(), FxBuildHasher);
    let mut out: FxHashMap<u64, SessionTotals> =
        FxHashMap::with_capacity_and_hasher(sessions_s.len(), FxBuildHasher);

    // One rate lookup per distinct model, reused for every line.
    let rates = crate::source::ModelRates::build(source, &cache.models);

    for sess in sessions_s {
        let lo = sess.line_start as usize;
        let hi = lo + sess.line_count as usize;
        let line_range = lo..hi;
        let fallback_model_id = sess.session_model_id;
        let mut totals = SessionTotals::default();

        for (off, line) in lines_s.get(line_range).unwrap_or(&[]).iter().enumerate() {
            if (line.flags & 1) != 0 && !seen.insert(line.msg_id_hash) {
                continue;
            }
            let mid = if line.model_id == u16::MAX {
                fallback_model_id
            } else {
                line.model_id
            };
            if rates.skip(mid) {
                continue;
            }
            let [ci, co, ccw, ccr] = rates.columns(
                mid,
                u64::from(line.input),
                u64::from(line.output),
                u64::from(line.cache_create),
                u64::from(line.cache_read),
            );

            totals.input += u64::from(line.input);
            totals.output += u64::from(line.output);
            totals.cache_read += u64::from(line.cache_read);
            totals.cache_create += u64::from(line.cache_create);
            totals.cost_input += ci;
            totals.cost_output += co;
            totals.cost_cache_read += ccr;
            totals.cost_cache_create += ccw;
            totals.cost += ci + co + ccw + ccr;
            if let Some(&ts) = ts_s.get(lo + off) {
                if ts > totals.last_ts {
                    totals.last_ts = ts;
                }
            }
        }

        let _ = out.insert(sess.path_hash, totals);
    }

    out
}

/// Bucket + sum the cache into `BreakdownKey → BucketUsage`. Day/month
/// rollups read pre-aggregated rows directly; session/block/non-UTC
/// fall back to a per-line walk with cross-session dedup.
pub fn aggregate<S: Source + ?Sized>(
    cache: &LoadedCache,
    bucket: Bucket,
    opts: &FilterOpts,
    breakdown: bool,
    source: &S,
) -> FxHashMap<BreakdownKey, BucketUsage> {
    // Fast path: Day/Month reads pre-aggregated rows directly.
    // Cross-session dedup + pricing were done at build time. Session
    // bucketing falls back to the live path because it needs sub-day
    // `last_ts` precision — preaggs only store per-day granularity
    // so the "most recent" sort can't distinguish two sessions on the
    // same day. Block bucketing also needs per-line ts.
    if opts.tz_offset_secs == 0 && matches!(bucket, Bucket::Day | Bucket::Week | Bucket::Month) {
        return aggregate_from_preaggs(cache, bucket, opts, breakdown);
    }

    let project_filter_id = opts.project.and_then(|name| {
        cache
            .projects
            .iter()
            .position(|p| p == name)
            .map(|i| i as u16)
    });

    let sessions_s = cache.sessions();
    let lines_s = cache.lines();
    let ts_s = cache.ts_unix();

    // At most one insert per line, and `with_capacity(n)` reserves enough
    // for `n` inserts without rehashing — so size at the line count, not 2×.
    let mut seen: FxHashSet<u64> =
        FxHashSet::with_capacity_and_hasher(lines_s.len().max(1024), FxBuildHasher);
    // Pre-size to a realistic ceiling rather than the line count — live
    // aggregate folds 50k lines into ~30–500 bucket rows. Starting at 64
    // covers the common case (daily/monthly) without rehashing; larger
    // result sets (blocks over months) still grow but only once.
    let mut out: FxHashMap<BreakdownKey, BucketUsage> =
        FxHashMap::with_capacity_and_hasher(64, FxBuildHasher);

    // One rate lookup per distinct model, reused for every line.
    let rates = crate::source::ModelRates::build(source, &cache.models);

    // Sessions are stored chronologically at cache-build time so walking
    // in natural order is the same as sorting by `started_at`. Skipping
    // the per-call sort saves ~1ms on 100+ session corpora.
    for sess in sessions_s {
        if project_filter_id.is_some() && Some(sess.project_id) != project_filter_id {
            continue;
        }
        let line_range =
            sess.line_start as usize..(sess.line_start as usize + sess.line_count as usize);
        let fallback_model_id = sess.session_model_id;

        for (local_i, line) in lines_s
            .get(line_range.clone())
            .unwrap_or(&[])
            .iter()
            .enumerate()
        {
            // Cross-session dedup FIRST, before the date filter — claim
            // each message at its earliest occurrence regardless of the
            // window. This matches the build-time preagg dedup (which runs
            // before any filtering); filtering first would let a message
            // whose earliest copy falls outside --since/--until reappear
            // and be counted on a later in-window duplicate.
            if (line.flags & 1) != 0 && !seen.insert(line.msg_id_hash) {
                continue;
            }
            let line_global_idx = sess.line_start as usize + local_i;
            let ts_unix = ts_s.get(line_global_idx).copied().unwrap_or(0);
            // Effective day respects --timezone so --since/--until and
            // daily/monthly bucketing match local-clock expectations.
            let effective_day = if opts.tz_offset_secs == 0 {
                line.day
            } else {
                let local = ts_unix + i64::from(opts.tz_offset_secs);
                local.div_euclid(86_400) as i32
            };

            if let Some(since) = opts.since_day {
                if effective_day < since {
                    continue;
                }
            }
            if let Some(until) = opts.until_day {
                if effective_day > until {
                    continue;
                }
            }
            let mid = if line.model_id != u16::MAX {
                line.model_id
            } else {
                fallback_model_id
            };
            if rates.skip(mid) {
                continue;
            }
            let [ci, co, ccw, ccr] = rates.columns(
                mid,
                u64::from(line.input),
                u64::from(line.output),
                u64::from(line.cache_create),
                u64::from(line.cache_read),
            );

            let key = match bucket {
                Bucket::Day => BucketKey(i64::from(effective_day)),
                Bucket::Week => BucketKey(week_index(effective_day)),
                Bucket::Month => {
                    let d = day_to_date(effective_day);
                    use chrono::Datelike;
                    BucketKey(i64::from(d.year()) * 12 + i64::from(d.month() - 1))
                }
                // Group by project (matches ccusage `session` semantics:
                // its "Session" column is really the project slug, with
                // every session under that project rolled into one row).
                Bucket::Session => BucketKey(i64::from(sess.project_id)),
                Bucket::Block => {
                    // Align by local time (blocks fall on user-visible
                    // clock), stored as UTC start for consistent label
                    // re-localization later.
                    let local = ts_unix + i64::from(opts.tz_offset_secs);
                    BucketKey(
                        local.div_euclid(BLOCK_SECS) * BLOCK_SECS - i64::from(opts.tz_offset_secs),
                    )
                }
            };

            let bkey = BreakdownKey(key, if breakdown { mid } else { u16::MAX });
            let entry = out.entry(bkey).or_default();
            entry.models.insert(mid);
            entry.projects.insert(sess.project_id);
            entry.input += u64::from(line.input);
            entry.output += u64::from(line.output);
            entry.cache_read += u64::from(line.cache_read);
            entry.cache_create += u64::from(line.cache_create);
            entry.cost_input += ci;
            entry.cost_output += co;
            entry.cost_cache_create += ccw;
            entry.cost_cache_read += ccr;
            entry.cost += ci + co + ccw + ccr;
            entry.line_count += 1;
            entry.last_ts = entry.last_ts.max(ts_unix);
        }
    }
    out
}

// Fast path: Day/Month rollup in UTC. Sums over the pre-aggregated table
// directly. No dedup hashset, no per-line touches, no TZ shifts.
fn aggregate_from_preaggs(
    cache: &LoadedCache,
    bucket: Bucket,
    opts: &FilterOpts,
    breakdown: bool,
) -> FxHashMap<BreakdownKey, BucketUsage> {
    let project_filter_id = opts.project.and_then(|name| {
        cache
            .projects
            .iter()
            .position(|p| p == name)
            .map(|i| i as u16)
    });

    // Pre-size: a typical daily/monthly rollup has ~30–60 entries; cap
    // at a generous 128 to avoid over-allocating when queries return
    // few rows. The map will still grow if --breakdown produces more.
    let mut out: FxHashMap<BreakdownKey, BucketUsage> =
        FxHashMap::with_capacity_and_hasher(cache.preaggs().len().clamp(32, 128), FxBuildHasher);

    for p in cache.preaggs() {
        if let Some(since) = opts.since_day {
            if p.day < since {
                continue;
            }
        }
        if let Some(until) = opts.until_day {
            if p.day > until {
                continue;
            }
        }
        if project_filter_id.is_some() && Some(p.project_id) != project_filter_id {
            continue;
        }

        let key = match bucket {
            Bucket::Day => BucketKey(i64::from(p.day)),
            Bucket::Week => BucketKey(week_index(p.day)),
            Bucket::Month => {
                let d = day_to_date(p.day);
                use chrono::Datelike;
                BucketKey(i64::from(d.year()) * 12 + i64::from(d.month() - 1))
            }
            Bucket::Session => BucketKey(i64::from(p.project_id)),
            // The fast-path guard in `aggregate` only dispatches here for
            // Day/Week/Month (see its `matches!` check). Session and Block
            // bucketing need sub-day `ts_unix` precision the preaggs don't
            // carry, so they live on the slow path.
            #[allow(clippy::unreachable)]
            Bucket::Block => unreachable!("blocks need per-line ts; not on fast path"),
        };
        let bkey = BreakdownKey(key, if breakdown { p.model_id } else { u16::MAX });
        let entry = out.entry(bkey).or_default();
        entry.models.insert(p.model_id);
        entry.projects.insert(p.project_id);
        entry.input += u64::from(p.input);
        entry.output += u64::from(p.output);
        entry.cache_read += u64::from(p.cache_read);
        entry.cache_create += u64::from(p.cache_create);
        entry.cost_input += p.cost_input;
        entry.cost_output += p.cost_output;
        entry.cost_cache_create += p.cost_cache_create;
        entry.cost_cache_read += p.cost_cache_read;
        entry.cost += p.total_cost();
        entry.line_count += p.line_count;
        // Track the latest day this bucket saw activity. Day-granularity
        // is enough for the session report's "Last Activity" column;
        // multiply by SECS_PER_DAY so downstream code (which expects
        // unix seconds in last_ts) works without a special case.
        let day_unix = i64::from(p.day) * 86_400;
        if day_unix > entry.last_ts {
            entry.last_ts = day_unix;
        }
    }
    out
}
