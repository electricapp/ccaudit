// mmap + load + incremental rebuild.
//
// Two storage modes: Mmap (zero-copy hot path) and Owned (cold rebuild
// returns owned Vecs). Callers see the same `LoadedCache::sessions()`
// etc. either way.

use super::build::{self, BuiltCache};
use super::schema::{
    HDR_SZ, Header, LINE_SZ, LineEntry, MAGIC, PREAGG_SZ, PreAgg, SESS_EXT_SZ, SESS_SZ,
    SessionEntry, SessionExt, VERSION,
};
use crate::source::{ParsedLine, ParsedSession, Source, SourceFile};
use chrono::{DateTime, Utc};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::fs;

// ── LoadedCache ──

pub enum CacheStorage {
    // Zero-copy hot/lazy path: slices point directly into the mmap.
    Mmap {
        mmap: memmap2::Mmap,
        sess: (usize, usize),
        sess_ext: (usize, usize),
        lines: (usize, usize),
        ts: (usize, usize),
        preaggs: (usize, usize),
        strings: (usize, usize), // raw strings pool, sliced lazily
    },
    // Cold rebuild or `empty()`: owned Vecs.
    Owned {
        sessions: Vec<SessionEntry>,
        sessions_ext: Vec<SessionExt>,
        lines: Vec<LineEntry>,
        ts_unix: Vec<i64>,
        preaggs: Vec<PreAgg>,
        strings: Vec<u8>,
    },
}

pub struct LoadedCache {
    storage: CacheStorage,
    pub models: Vec<String>,
    pub projects: Vec<String>,
}

// All slice ranges below are validated against the mmap length at load
// time (see `cold_load`); `clippy::indexing_slicing` would be noise.
#[allow(clippy::indexing_slicing)]
impl LoadedCache {
    pub fn sessions(&self) -> &[SessionEntry] {
        match &self.storage {
            CacheStorage::Mmap { mmap, sess, .. } => bytemuck::cast_slice(&mmap[sess.0..sess.1]),
            CacheStorage::Owned { sessions, .. } => sessions,
        }
    }
    pub fn sessions_ext(&self) -> &[SessionExt] {
        match &self.storage {
            CacheStorage::Mmap { mmap, sess_ext, .. } => {
                bytemuck::cast_slice(&mmap[sess_ext.0..sess_ext.1])
            }
            CacheStorage::Owned { sessions_ext, .. } => sessions_ext,
        }
    }
    pub fn lines(&self) -> &[LineEntry] {
        match &self.storage {
            CacheStorage::Mmap { mmap, lines, .. } => bytemuck::cast_slice(&mmap[lines.0..lines.1]),
            CacheStorage::Owned { lines, .. } => lines,
        }
    }
    pub fn ts_unix(&self) -> &[i64] {
        match &self.storage {
            CacheStorage::Mmap { mmap, ts, .. } => bytemuck::cast_slice(&mmap[ts.0..ts.1]),
            CacheStorage::Owned { ts_unix, .. } => ts_unix,
        }
    }
    pub fn preaggs(&self) -> &[PreAgg] {
        match &self.storage {
            CacheStorage::Mmap { mmap, preaggs, .. } => {
                bytemuck::cast_slice(&mmap[preaggs.0..preaggs.1])
            }
            CacheStorage::Owned { preaggs, .. } => preaggs,
        }
    }
    fn strings(&self) -> &[u8] {
        match &self.storage {
            CacheStorage::Mmap { mmap, strings, .. } => &mmap[strings.0..strings.1],
            CacheStorage::Owned { strings, .. } => strings,
        }
    }

    /// Display name for session `idx` — UTF-8 bytes from the strings
    /// pool, decoded on demand. Cold path; only the `session` report
    /// hits this.
    pub fn display_name(&self, idx: usize) -> &str {
        let Some(ext) = self.sessions_ext().get(idx) else {
            return "";
        };
        let off = ext.display_name_off as usize;
        let len = ext.display_name_len as usize;
        self.strings()
            .get(off..off + len)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("")
    }

    /// Short session id derived from the JSONL filename.
    pub fn session_id(&self, idx: usize) -> &str {
        let Some(ext) = self.sessions_ext().get(idx) else {
            return "";
        };
        let off = ext.session_id_off as usize;
        let len = ext.session_id_len as usize;
        self.strings()
            .get(off..off + len)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("")
    }
}

// ── Public entry point ──

pub fn load<S: Source + ?Sized>(source: &S) -> LoadedCache {
    // Lazy mode: skip the filesystem scan and use the cache as-is. Opt-in
    // via CCAUDIT_LAZY=1 for status-bar integrations where one-run
    // staleness is OK in exchange for ~0ms startup.
    if std::env::var_os("CCAUDIT_LAZY").is_some() {
        if let Some(mmap) = try_load_mmap(source) {
            if let Some(c) = cache_from_mmap(mmap) {
                return c;
            }
        }
    }

    let sources = source.scan_sources();
    if sources.is_empty() {
        return empty();
    }
    if let Some(c) = try_hot_path(source, &sources) {
        return c;
    }
    cold_rebuild(source, sources)
}

// ── mmap ──

fn try_load_mmap<S: Source + ?Sized>(source: &S) -> Option<memmap2::Mmap> {
    let path = source.cache_path()?;
    let file = fs::File::open(&path).ok()?;
    #[allow(unsafe_code)]
    let mmap = unsafe { memmap2::Mmap::map(&file).ok()? };
    Some(mmap)
}

fn try_hot_path<S: Source + ?Sized>(source: &S, sources: &[SourceFile]) -> Option<LoadedCache> {
    let mmap = try_load_mmap(source)?;
    let cache = cache_from_mmap(mmap)?;
    if !validate(cache.sessions(), sources) {
        return None;
    }
    Some(cache)
}

fn validate(sessions: &[SessionEntry], sources: &[SourceFile]) -> bool {
    if sessions.len() != sources.len() {
        return false;
    }
    // Sessions on disk are now stored in `started_at` order rather than
    // path_hash order, so we can't zip-compare. Build a hash-map of
    // sources keyed by path_hash and look up each session.
    let src_by_hash: FxHashMap<u64, &SourceFile> =
        sources.iter().map(|s| (s.path_hash, s)).collect();
    for entry in sessions {
        let Some(src) = src_by_hash.get(&entry.path_hash) else {
            return false;
        };
        if entry.mtime != src.mtime || entry.size != src.size {
            return false;
        }
    }
    true
}

const fn empty() -> LoadedCache {
    LoadedCache {
        storage: CacheStorage::Owned {
            sessions: Vec::new(),
            sessions_ext: Vec::new(),
            lines: Vec::new(),
            ts_unix: Vec::new(),
            preaggs: Vec::new(),
            strings: Vec::new(),
        },
        models: Vec::new(),
        projects: Vec::new(),
    }
}

// ── Parse + construct ──

struct CacheLayout {
    sess_range: (usize, usize),
    sess_ext_range: (usize, usize),
    lines_range: (usize, usize),
    ts_range: (usize, usize),
    preagg_range: (usize, usize),
    strings_range: (usize, usize),
    models: Vec<String>,
    projects: Vec<String>,
}

// Length is checked up-front against every section offset, so the
// subsequent slices are proven safe.
#[allow(clippy::indexing_slicing)]
fn parse_cache_layout(bytes: &[u8]) -> Option<CacheLayout> {
    if bytes.len() < HDR_SZ {
        return None;
    }
    let header: Header = *bytemuck::from_bytes(&bytes[..HDR_SZ]);
    if header.magic != MAGIC || header.version != VERSION {
        return None;
    }

    let n_sess = header.num_sessions as usize;
    let n_lines = header.num_lines as usize;
    let n_preaggs = header.num_preaggs as usize;

    let sess_end = HDR_SZ + n_sess * SESS_SZ;
    let sess_ext_end = sess_end + n_sess * SESS_EXT_SZ;
    let lines_end = sess_ext_end + n_lines * LINE_SZ;
    let ts_end = lines_end + n_lines * 8;
    let preagg_end = ts_end + n_preaggs * PREAGG_SZ;
    if bytes.len() < preagg_end {
        return None;
    }

    let (models, after_models) = read_string_vec(bytes, preagg_end)?;
    let (projects, after_projects) = read_string_vec(bytes, after_models)?;
    if after_projects + 4 > bytes.len() {
        return None;
    }
    let slen = u32::from_le_bytes(
        bytes
            .get(after_projects..after_projects + 4)?
            .try_into()
            .ok()?,
    ) as usize;
    let strings_start = after_projects + 4;
    let strings_end = strings_start + slen;
    if bytes.len() < strings_end {
        return None;
    }

    Some(CacheLayout {
        sess_range: (HDR_SZ, sess_end),
        sess_ext_range: (sess_end, sess_ext_end),
        lines_range: (sess_ext_end, lines_end),
        ts_range: (lines_end, ts_end),
        preagg_range: (ts_end, preagg_end),
        strings_range: (strings_start, strings_end),
        models,
        projects,
    })
}

fn cache_from_mmap(mmap: memmap2::Mmap) -> Option<LoadedCache> {
    let p = parse_cache_layout(&mmap[..])?;
    Some(LoadedCache {
        storage: CacheStorage::Mmap {
            mmap,
            sess: p.sess_range,
            sess_ext: p.sess_ext_range,
            lines: p.lines_range,
            ts: p.ts_range,
            preaggs: p.preagg_range,
            strings: p.strings_range,
        },
        models: p.models,
        projects: p.projects,
    })
}

fn cache_from_mmap_owned(mmap: &memmap2::Mmap) -> Option<LoadedCache> {
    // Owned variant for the cold-rebuild path: copy out the slices and
    // drop the mmap so we don't keep it alive longer than needed.
    let bytes = &mmap[..];
    let p = parse_cache_layout(bytes)?;
    Some(LoadedCache {
        storage: CacheStorage::Owned {
            sessions: bytemuck::cast_slice::<u8, SessionEntry>(
                bytes.get(p.sess_range.0..p.sess_range.1)?,
            )
            .to_vec(),
            sessions_ext: bytemuck::cast_slice::<u8, SessionExt>(
                bytes.get(p.sess_ext_range.0..p.sess_ext_range.1)?,
            )
            .to_vec(),
            lines: bytemuck::cast_slice::<u8, LineEntry>(
                bytes.get(p.lines_range.0..p.lines_range.1)?,
            )
            .to_vec(),
            ts_unix: bytemuck::cast_slice::<u8, i64>(bytes.get(p.ts_range.0..p.ts_range.1)?)
                .to_vec(),
            preaggs: bytemuck::cast_slice::<u8, PreAgg>(
                bytes.get(p.preagg_range.0..p.preagg_range.1)?,
            )
            .to_vec(),
            strings: bytes.get(p.strings_range.0..p.strings_range.1)?.to_vec(),
        },
        models: p.models,
        projects: p.projects,
    })
}

// ── Cold rebuild with incremental reparse ──

// When a live session appends a few lines, only one JSONL's (mtime,
// size) changes. We load the old cache (structurally, without full
// validation), keep every session whose fingerprint still matches, and
// reparse only the ones that actually differ.
fn cold_rebuild<S: Source + ?Sized>(source: &S, sources: Vec<SourceFile>) -> LoadedCache {
    let existing: Option<LoadedCache> =
        try_load_mmap(source).and_then(|m| cache_from_mmap_owned(&m));

    let old_by_hash: FxHashMap<u64, usize> = existing
        .as_ref()
        .map(|c| {
            c.sessions()
                .iter()
                .enumerate()
                .map(|(i, s)| (s.path_hash, i))
                .collect()
        })
        .unwrap_or_default();

    let mut to_parse: Vec<SourceFile> = Vec::new();
    let mut reusable: Vec<(SourceFile, usize)> = Vec::new();
    for src in sources {
        let reuse_idx = old_by_hash.get(&src.path_hash).copied().and_then(|i| {
            let s = existing.as_ref()?.sessions().get(i)?;
            (s.mtime == src.mtime && s.size == src.size).then_some(i)
        });
        match reuse_idx {
            Some(i) => reusable.push((src, i)),
            None => to_parse.push(src),
        }
    }

    let freshly_parsed: Vec<ParsedSession> = to_parse
        .par_iter()
        .filter_map(|s| source.parse_session(s))
        .collect();

    let mut all_parsed: Vec<ParsedSession> = freshly_parsed;
    if let Some(old) = existing {
        for (src, old_idx) in reusable {
            if let Some(p) = reconstruct_from_cache(&old, old_idx, &src) {
                all_parsed.push(p);
            }
        }
    }

    let built: BuiltCache = build::build(all_parsed, source);
    if let Some(path) = source.cache_path() {
        let _ = build::write_cache(&built, &path);
    }
    LoadedCache {
        storage: CacheStorage::Owned {
            sessions: built.sessions,
            sessions_ext: built.sessions_ext,
            lines: built.lines,
            ts_unix: built.ts_unix,
            preaggs: built.preaggs,
            strings: built.strings,
        },
        models: built.models,
        projects: built.projects,
    }
}

// Rehydrate a ParsedSession straight from the existing cache so we can
// feed it back into build() next to freshly-parsed ones. build() rebuilds
// intern tables from scratch, so handing it plain Strings is fine.
fn reconstruct_from_cache(
    cache: &LoadedCache,
    old_idx: usize,
    src: &SourceFile,
) -> Option<ParsedSession> {
    let s = cache.sessions().get(old_idx)?;
    let start = s.line_start as usize;
    let end = start + s.line_count as usize;
    let lines_slice = cache.lines().get(start..end)?;
    let ts_slice = cache.ts_unix().get(start..end)?;
    let mut lines = Vec::with_capacity(lines_slice.len());
    for l in lines_slice {
        let model = if l.model_id == u16::MAX {
            None
        } else {
            cache.models.get(l.model_id as usize).cloned()
        };
        let msg_id_hash = if (l.flags & 1) != 0 {
            Some(l.msg_id_hash)
        } else {
            None
        };
        lines.push(ParsedLine {
            day: l.day,
            msg_id_hash,
            model,
            input: l.input,
            output: l.output,
            cache_read: l.cache_read,
            cache_create: l.cache_create,
        });
    }
    let started_at = if s.started_ts == i64::MIN {
        None
    } else {
        DateTime::<Utc>::from_timestamp(s.started_ts, 0)
    };
    let session_model = if s.session_model_id == u16::MAX {
        None
    } else {
        cache.models.get(s.session_model_id as usize).cloned()
    };
    Some(ParsedSession {
        path_hash: src.path_hash,
        mtime: src.mtime,
        size: src.size,
        started_at,
        session_model,
        display_name: cache.display_name(old_idx).to_string(),
        session_id: cache.session_id(old_idx).to_string(),
        project_name: if s.project_id == u16::MAX {
            None
        } else {
            cache.projects.get(s.project_id as usize).cloned()
        },
        lines,
        ts_unix: ts_slice.to_vec(),
    })
}

fn read_string_vec(bytes: &[u8], mut pos: usize) -> Option<(Vec<String>, usize)> {
    if pos + 4 > bytes.len() {
        return None;
    }
    let count = u32::from_le_bytes(bytes.get(pos..pos + 4)?.try_into().ok()?) as usize;
    pos += 4;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if pos + 2 > bytes.len() {
            return None;
        }
        let len = u16::from_le_bytes(bytes.get(pos..pos + 2)?.try_into().ok()?) as usize;
        pos += 2;
        if pos + len > bytes.len() {
            return None;
        }
        let s = std::str::from_utf8(bytes.get(pos..pos + len)?)
            .ok()?
            .to_string();
        pos += len;
        out.push(s);
    }
    Some((out, pos))
}
