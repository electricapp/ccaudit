// Cross-provider reporting.
//
// `--source` reports one provider. `--all` reports every provider that
// has logs, as one set of rows; `--by-agent` splits those rows by
// provider instead of by model.
//
// The merge happens at the *rollup* level, not the cache level. Each
// provider is aggregated by its own `Source` — its own pricing, its own
// model-name normalization, its own skip rules — and only the resulting
// per-bucket totals are summed. Splicing the caches together first would
// mean re-pricing rows from one provider against another's rate table,
// which is exactly the class of bug this tool exists to catch.

use super::agg::{BreakdownKey, Bucket, BucketKey, BucketUsage, FilterOpts, aggregate};
use super::load::{LoadedCache, load, names_only};
use crate::source::{SourceKind, pick};
use rustc_hash::FxHashMap;

/// A merged rollup plus the name tables its ids point into.
pub struct Merged {
    /// Names-only cache: what renderers resolve `models` / `projects`
    /// ids and session labels against.
    pub cache: LoadedCache,
    pub rollup: FxHashMap<BreakdownKey, BucketUsage>,
    /// Providers that actually contributed rows, in report order. Empty
    /// when no provider has logs on this machine.
    pub contributors: Vec<&'static str>,
}

/// Intern `name`, returning its id in `table`.
///
/// Ids are what the display bitsets hold, and that bitset tops out at 64
/// entries — see `U64Bitset`. Interning in first-seen order keeps the
/// providers reported first (Claude Code by default) inside that window.
fn intern(table: &mut Vec<String>, index: &mut FxHashMap<String, u16>, name: &str) -> u16 {
    if let Some(&id) = index.get(name) {
        return id;
    }
    let id = u16::try_from(table.len()).unwrap_or(u16::MAX);
    table.push(name.to_owned());
    let _ = index.insert(name.to_owned(), id);
    id
}

/// Aggregate every provider with logs and sum the results per bucket.
///
/// `by_agent` replaces the model dimension with the provider: rows split
/// one per (bucket, provider) and the second column names the provider.
/// It is mutually exclusive with `breakdown`, which splits by model —
/// the renderer has one slot for that dimension, and the CLI rejects the
/// combination rather than silently picking a winner.
pub fn aggregate_all(bucket: Bucket, opts: &FilterOpts, breakdown: bool, by_agent: bool) -> Merged {
    let mut models: Vec<String> = Vec::new();
    let mut model_index: FxHashMap<String, u16> = FxHashMap::default();
    let mut projects: Vec<String> = Vec::new();
    let mut project_index: FxHashMap<String, u16> = FxHashMap::default();
    let mut out: FxHashMap<BreakdownKey, BucketUsage> = FxHashMap::default();
    let mut contributors: Vec<&'static str> = Vec::new();

    for kind in SourceKind::ALL {
        let source = pick(kind);
        let cache = load(source);
        // Model breakdown and agent breakdown compete for the same key
        // slot, so a by-agent run aggregates unsplit and re-keys below.
        let rollup = aggregate(&cache, bucket, opts, breakdown && !by_agent, source);
        if rollup.is_empty() {
            continue;
        }
        contributors.push(source.id());

        // One id per provider, reserved before any model name so the
        // by-agent column is never the thing pushed past the bitset cap.
        let agent_id = if by_agent {
            intern(&mut models, &mut model_index, source.display_name())
        } else {
            u16::MAX
        };

        for (key, usage) in rollup {
            let mut merged = usage;

            if by_agent {
                // The display set becomes the provider itself.
                merged.models = super::agg::U64Bitset::default();
                merged.models.insert(agent_id);
            } else {
                // Normalize through the provider that produced the name:
                // "claude-opus-4-6-20251205" and "gpt-5.4" shorten by
                // different rules, and the merged table has no provider
                // to ask afterwards.
                let mut remapped = super::agg::U64Bitset::default();
                for id in usage.models.iter() {
                    if let Some(raw) = cache.models.get(id as usize) {
                        let short = source.normalize_model(raw);
                        remapped.insert(intern(&mut models, &mut model_index, &short));
                    }
                }
                merged.models = remapped;
            }

            let mut remapped_projects = super::agg::U64Bitset::default();
            for id in usage.projects.iter() {
                if let Some(name) = cache.projects.get(id as usize) {
                    remapped_projects.insert(intern(&mut projects, &mut project_index, name));
                }
            }
            merged.projects = remapped_projects;

            // Session buckets key on a project id, which is per-cache.
            // Every other bucket keys on time, which is universal.
            let bucket_key = if matches!(bucket, Bucket::Session) {
                let name = cache
                    .projects
                    .get(key.0.as_i64() as usize)
                    .cloned()
                    .unwrap_or_default();
                BucketKey(i64::from(intern(&mut projects, &mut project_index, &name)))
            } else {
                key.0
            };
            let new_key = if by_agent {
                BreakdownKey(bucket_key, agent_id)
            } else {
                BreakdownKey(bucket_key, key.1)
            };

            let slot = out.entry(new_key).or_default();
            slot.input += merged.input;
            slot.output += merged.output;
            slot.cache_read += merged.cache_read;
            slot.cache_create += merged.cache_create;
            slot.cost += merged.cost;
            slot.cost_input += merged.cost_input;
            slot.cost_output += merged.cost_output;
            slot.cost_cache_create += merged.cost_cache_create;
            slot.cost_cache_read += merged.cost_cache_read;
            slot.line_count = slot.line_count.saturating_add(merged.line_count);
            slot.last_ts = slot.last_ts.max(merged.last_ts);
            for id in merged.models.iter() {
                slot.models.insert(id);
            }
            for id in merged.projects.iter() {
                slot.projects.insert(id);
            }
        }
    }

    Merged {
        cache: names_only(models, projects),
        rollup: out,
        contributors,
    }
}
