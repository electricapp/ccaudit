// Aggregation: cache + filter + bucket → FxHashMap<BreakdownKey, BucketUsage>.
//
// Two paths:
//   • UTC day/month → summed directly from pre-aggregated rows. No
//     per-line walk, no dedup hashset, zero provider calls.
//   • Everything else (session, block, non-UTC) → walks per-line data
//     with cross-session dedup, calling `source.price` per row.

use super::load::LoadedCache;
use crate::source::{Source, day_to_date};
use rustc_hash::FxHashMap;

#[derive(Clone, Copy, Debug)]
pub enum Bucket {
    Day,
    Month,
    Session,
    Block, // 5-hour billing windows aligned to unix epoch
}

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
    pub models: U64Bitset,
    pub projects: U64Bitset,
    pub line_count: u32,
    pub last_ts: i64,
}

/// Length of a billing block in seconds. Public so the report layer
/// (table/json/statusline) can compute "is this block currently active?"
/// against the same constant the aggregator buckets by.
pub const BLOCK_SECS: i64 = 5 * 3600;

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
    if opts.tz_offset_secs == 0 && matches!(bucket, Bucket::Day | Bucket::Month) {
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

    let mut seen: rustc_hash::FxHashSet<u64> = rustc_hash::FxHashSet::with_capacity_and_hasher(
        lines_s.len().saturating_mul(2).max(1024),
        rustc_hash::FxBuildHasher,
    );
    // Pre-size to a realistic ceiling rather than the line count — live
    // aggregate folds 50k lines into ~30–500 bucket rows. Starting at 64
    // covers the common case (daily/monthly) without rehashing; larger
    // result sets (blocks over months) still grow but only once.
    let mut out: FxHashMap<BreakdownKey, BucketUsage> =
        FxHashMap::with_capacity_and_hasher(64, rustc_hash::FxBuildHasher);

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
            // Cross-session dedup
            if (line.flags & 1) != 0 && !seen.insert(line.msg_id_hash) {
                continue;
            }
            let mid = if line.model_id != u16::MAX {
                line.model_id
            } else {
                fallback_model_id
            };
            let model_name: Option<&str> = if mid == u16::MAX {
                None
            } else {
                cache.models.get(mid as usize).map(String::as_str)
            };
            if model_name.is_some_and(|m| source.skip_model(m)) {
                continue;
            }
            let cost = source.compute_cost(
                model_name,
                u64::from(line.input),
                u64::from(line.output),
                u64::from(line.cache_create),
                u64::from(line.cache_read),
            );

            let key = match bucket {
                Bucket::Day => BucketKey(i64::from(effective_day)),
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
            entry.cost += cost;
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
    let mut out: FxHashMap<BreakdownKey, BucketUsage> = FxHashMap::with_capacity_and_hasher(
        cache.preaggs().len().clamp(32, 128),
        rustc_hash::FxBuildHasher,
    );

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
            Bucket::Month => {
                let d = day_to_date(p.day);
                use chrono::Datelike;
                BucketKey(i64::from(d.year()) * 12 + i64::from(d.month() - 1))
            }
            Bucket::Session => BucketKey(i64::from(p.project_id)),
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
