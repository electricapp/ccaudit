// On-disk cache format.
//
// Timestamp-field naming convention (used here and in `agg.rs` /
// `parse.rs` / `source/mod.rs`):
//   `*_at: DateTime<Utc>`  — typed datetime (e.g. `started_at`, `ended_at`)
//   `*_ts:  i64`           — raw unix seconds (e.g. `started_ts`, `last_ts`)
//   `timestamp: DateTime`  — reserved for fields that mirror an input
//                            JSON key of the same name (`Message.timestamp`)
//
// Suffix signals unit + type, which lets a reader distinguish
// `started_at` (typed) from `started_ts` (integer seconds) at a glance.
//
// A single file (`usage.db`) with fixed-size record sections + string
// pools. We mmap it and cast bytes directly to typed slices, so no
// deserialization happens on the hot path.
//
// File layout (all multi-byte integers little-endian):
//   Header                   (32 B)
//   Sessions (hot)           (num_sessions × 48 B, sorted by path_hash)
//   SessionsExt (cold)       (num_sessions × 16 B, parallel to Sessions)
//   Lines                    (num_lines × 32 B)
//   ts_unix                  (num_lines × 8 B — parallel to Lines)
//   PreAggs                  (num_preaggs × 64 B, already deduped)
//   Models pool              (u32 count + for each: u16 len + utf8)
//   Projects pool            (u32 count + for each: u16 len + utf8)
//   Strings pool             (u32 total_len + bytes — display names + session ids)
//
// Hot/cold split on Sessions: validate + aggregate only ever touch the
// hot section (path_hash, mtime, size, line range, project, model). The
// display name + session id offsets only matter for the `session`
// report, so they live in the cold parallel array — never loaded into
// the L1 line for the common path.
//
// Invalidation: each SessionEntry stores (path_hash, mtime, size); on
// load we stat every current JSONL file and compare. Any mismatch
// triggers a rebuild (incremental — unchanged sessions are reused).

use bytemuck::{Pod, Zeroable};

pub const MAGIC: u32 = 0x4343_5547; // "CCUG"
pub const VERSION: u32 = 0;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Header {
    pub magic: u32,
    pub version: u32,
    pub num_sessions: u32,
    pub num_lines: u32,
    pub num_models: u32,
    pub num_projects: u32,
    // Pre-aggregated summary. Computed once at build and used as the
    // daily/monthly hot-path, skipping the per-line walk.
    pub num_preaggs: u32,
    pub _reserved0: u32,
}

// Hot half of a session record — what validate, sort-by-time, and the
// per-line walk in aggregate need.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct SessionEntry {
    pub path_hash: u64,
    pub mtime: u64,
    pub size: u64,
    pub started_ts: i64, // unix seconds; i64::MIN if unknown. `_ts` suffix signals raw seconds vs typed `started_at: DateTime`.
    pub line_start: u32,
    pub line_count: u32,
    pub session_model_id: u16, // u16::MAX if unknown
    pub project_id: u16,       // u16::MAX if the provider has no project concept
    pub _pad: u32,
}

// Cold half — only touched when rendering the `session` report. Lives
// in a parallel array so the hot section above stays dense in cache.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct SessionExt {
    pub display_name_off: u32,
    pub session_id_off: u32,
    pub display_name_len: u16,
    pub session_id_len: u16,
    pub _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct LineEntry {
    pub msg_id_hash: u64, // 0 if no message_id (flags bit 0 still set)
    pub day: i32,         // days since 1970-01-01 UTC
    pub model_id: u16,    // u16::MAX if unknown
    pub flags: u16,       // bit 0: has_msg_id
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    pub cache_create: u32,
}

// Pre-aggregated bucket: one row per (day, model_id, project_id) triple.
// Cross-session dedup has already been applied, so the aggregator sums
// these directly without any seen-id hashset. Per-column dollar costs
// are precomputed at build time so renders never re-price.
//
// Token counts are u32 — a single (day, model, project) can physically
// hold at most 4.3B tokens of any one type, far above any realistic
// usage. Total cost is derived on demand via `total_cost()` rather
// than stored, since it equals the sum of the four column costs.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct PreAgg {
    pub day: i32,
    pub model_id: u16, // u16::MAX if unknown
    pub project_id: u16,
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    pub cache_create: u32,
    pub cost_input: f64,
    pub cost_output: f64,
    pub cost_cache_read: f64,
    pub cost_cache_create: f64,
    pub line_count: u32,
    pub _pad: u32,
}

impl PreAgg {
    /// Total cost = sum of the four column costs. Free vs storing it.
    pub fn total_cost(&self) -> f64 {
        self.cost_input + self.cost_output + self.cost_cache_read + self.cost_cache_create
    }
}

pub const HDR_SZ: usize = size_of::<Header>();
pub const SESS_SZ: usize = size_of::<SessionEntry>();
pub const SESS_EXT_SZ: usize = size_of::<SessionExt>();
pub const LINE_SZ: usize = size_of::<LineEntry>();
pub const PREAGG_SZ: usize = size_of::<PreAgg>();

const _: () = {
    // If struct sizes drift the on-disk format is broken and VERSION
    // must be bumped.
    assert!(HDR_SZ == 32);
    assert!(SESS_SZ == 48);
    assert!(SESS_EXT_SZ == 16);
    assert!(LINE_SZ == 32);
    assert!(PREAGG_SZ == 64);
};
