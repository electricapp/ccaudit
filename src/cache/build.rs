// ParsedSession → on-disk cache buffers.
//
// Runs only on cold-rebuild. The hot path doesn't touch this file.

use super::schema::{Header, LineEntry, MAGIC, PreAgg, SessionEntry, SessionExt, VERSION};
use crate::source::{ParsedSession, Source};
use rustc_hash::FxHashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

pub struct BuiltCache {
    pub sessions: Vec<SessionEntry>,
    pub sessions_ext: Vec<SessionExt>,
    pub lines: Vec<LineEntry>,
    pub models: Vec<String>,
    pub projects: Vec<String>,
    pub strings: Vec<u8>,
    pub ts_unix: Vec<i64>,
    pub preaggs: Vec<PreAgg>,
}

pub fn build<S: Source + ?Sized>(mut parsed: Vec<ParsedSession>, source: &S) -> BuiltCache {
    let mut model_table: Vec<String> = Vec::new();
    let mut model_index: FxHashMap<String, u16> = FxHashMap::default();
    let mut intern_model = |s: &str| -> u16 {
        if let Some(&id) = model_index.get(s) {
            return id;
        }
        let id = model_table.len() as u16;
        model_table.push(s.to_string());
        let _ = model_index.insert(s.to_string(), id);
        id
    };

    let mut project_table: Vec<String> = Vec::new();
    let mut project_index: FxHashMap<String, u16> = FxHashMap::default();
    let mut intern_project = |s: &str| -> u16 {
        if let Some(&id) = project_index.get(s) {
            return id;
        }
        let id = project_table.len() as u16;
        project_table.push(s.to_string());
        let _ = project_index.insert(s.to_string(), id);
        id
    };

    // Sort by started_at so downstream cross-session dedup (here and
    // in live aggregation) can walk sessions chronologically without
    // re-sorting. `i64::MIN` pushes timestamp-less sessions to the front
    // where they incur minimal dedup cost.
    parsed.sort_by_key(|p| p.started_at.map_or(i64::MIN, |t| t.timestamp()));
    let total_lines: usize = parsed.iter().map(|p| p.lines.len()).sum();
    let mut lines: Vec<LineEntry> = Vec::with_capacity(total_lines);
    let mut sessions: Vec<SessionEntry> = Vec::with_capacity(parsed.len());
    let mut sessions_ext: Vec<SessionExt> = Vec::with_capacity(parsed.len());
    let mut strings: Vec<u8> = Vec::new();
    let mut ts_unix: Vec<i64> = Vec::with_capacity(total_lines);

    for p in &parsed {
        let line_start = lines.len() as u32;
        for l in &p.lines {
            let model_id = l.model.as_deref().map_or(u16::MAX, &mut intern_model);
            let (msg_id_hash, flags) = match l.msg_id_hash {
                Some(h) => (h, 1u16),
                None => (0, 0u16),
            };
            lines.push(LineEntry {
                msg_id_hash,
                day: l.day,
                model_id,
                flags,
                input: l.input,
                output: l.output,
                cache_read: l.cache_read,
                cache_create: l.cache_create,
            });
        }
        ts_unix.extend_from_slice(&p.ts_unix);

        let session_model_id = p
            .session_model
            .as_deref()
            .map_or(u16::MAX, &mut intern_model);
        // u16::MAX marks "no project" (provider doesn't group sessions);
        // reads in agg/report filter these out via cache.projects.get.
        let project_id = p
            .project_name
            .as_deref()
            .map_or(u16::MAX, &mut intern_project);

        let dn_bytes = p.display_name.as_bytes();
        let dn_off = strings.len() as u32;
        let dn_len = dn_bytes.len().min(u16::MAX as usize) as u16;
        if let Some(slice) = dn_bytes.get(..dn_len as usize) {
            strings.extend_from_slice(slice);
        }

        let sid_bytes = p.session_id.as_bytes();
        let sid_off = strings.len() as u32;
        let sid_len = sid_bytes.len().min(u16::MAX as usize) as u16;
        if let Some(slice) = sid_bytes.get(..sid_len as usize) {
            strings.extend_from_slice(slice);
        }

        sessions.push(SessionEntry {
            path_hash: p.path_hash,
            mtime: p.mtime,
            size: p.size,
            started_ts: p.started_at.map_or(i64::MIN, |t| t.timestamp()),
            line_start,
            line_count: (lines.len() as u32) - line_start,
            session_model_id,
            project_id,
            _pad: 0,
        });
        sessions_ext.push(SessionExt {
            display_name_off: dn_off,
            session_id_off: sid_off,
            display_name_len: dn_len,
            session_id_len: sid_len,
            _pad: 0,
        });
    }

    // Build the pre-aggregated summary. Cross-session dedup is the same
    // logic as live aggregate() — we do it once here during build so the
    // hot path just sums pre-reduced rows.
    let preaggs = build_preaggs(&sessions, &lines, &model_table, source);

    BuiltCache {
        sessions,
        sessions_ext,
        lines,
        models: model_table,
        projects: project_table,
        strings,
        ts_unix,
        preaggs,
    }
}

fn build_preaggs<S: Source + ?Sized>(
    sessions: &[SessionEntry],
    lines: &[LineEntry],
    models: &[String],
    source: &S,
) -> Vec<PreAgg> {
    // Sessions are already stored in chronological order (see `build`),
    // so walking them in natural order is enough — no sort needed. The
    // cross-session dedup below still picks the earliest occurrence of
    // each `msg_id` because we visit sessions oldest-first.
    let mut seen: rustc_hash::FxHashSet<u64> = rustc_hash::FxHashSet::with_capacity_and_hasher(
        lines.len().saturating_mul(2).max(1024),
        rustc_hash::FxBuildHasher,
    );

    // Key = (day, model_id, project_id) packed into u64 for FxHashMap.
    let mut bucket: FxHashMap<u64, PreAgg> = FxHashMap::default();

    for sess in sessions {
        let line_range =
            sess.line_start as usize..(sess.line_start as usize + sess.line_count as usize);
        let fallback_model_id = sess.session_model_id;
        let project_id = sess.project_id;

        for line in lines.get(line_range).unwrap_or(&[]) {
            // Cross-session dedup (canonical, UTC-day based).
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
                models.get(mid as usize).map(String::as_str)
            };
            if model_name.is_some_and(|m| source.skip_model(m)) {
                continue;
            }
            // Split cost by column so renders can show which token type
            // is actually driving spend without re-pricing at query time.
            let rates = source.price(model_name);
            let ci = f64::from(line.input) * rates.input / 1_000_000.0;
            let co = f64::from(line.output) * rates.output / 1_000_000.0;
            let ccw = f64::from(line.cache_create) * rates.cache_write / 1_000_000.0;
            let ccr = f64::from(line.cache_read) * rates.cache_read / 1_000_000.0;
            let cost = ci + co + ccw + ccr;

            let key: u64 =
                (u64::from(line.day as u32) << 32) | (u64::from(mid) << 16) | u64::from(project_id);
            let entry = bucket.entry(key).or_insert(PreAgg {
                day: line.day,
                model_id: mid,
                project_id,
                input: 0,
                output: 0,
                cache_read: 0,
                cache_create: 0,
                cost_input: 0.0,
                cost_output: 0.0,
                cost_cache_read: 0.0,
                cost_cache_create: 0.0,
                line_count: 0,
                _pad: 0,
            });
            entry.input = entry.input.saturating_add(line.input);
            entry.output = entry.output.saturating_add(line.output);
            entry.cache_read = entry.cache_read.saturating_add(line.cache_read);
            entry.cache_create = entry.cache_create.saturating_add(line.cache_create);
            entry.cost_input += ci;
            entry.cost_output += co;
            entry.cost_cache_read += ccr;
            entry.cost_cache_create += ccw;
            entry.line_count += 1;
            // Silence unused warning if we ever drop column-cost reuse.
            let _ = cost;
        }
    }

    let mut out: Vec<PreAgg> = bucket.into_values().collect();
    out.sort_by_key(|p| (p.day, p.model_id, p.project_id));
    out
}

// ── I/O ──

pub fn write_cache(b: &BuiltCache, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("db.tmp");
    let mut f = fs::File::create(&tmp)?;

    let header = Header {
        magic: MAGIC,
        version: VERSION,
        num_sessions: b.sessions.len() as u32,
        num_lines: b.lines.len() as u32,
        num_models: b.models.len() as u32,
        num_projects: b.projects.len() as u32,
        num_preaggs: b.preaggs.len() as u32,
        _reserved0: 0,
    };
    f.write_all(bytemuck::bytes_of(&header))?;
    f.write_all(bytemuck::cast_slice(&b.sessions))?;
    f.write_all(bytemuck::cast_slice(&b.sessions_ext))?;
    f.write_all(bytemuck::cast_slice(&b.lines))?;
    f.write_all(bytemuck::cast_slice(&b.ts_unix))?;
    f.write_all(bytemuck::cast_slice(&b.preaggs))?;
    write_string_vec(&mut f, &b.models)?;
    write_string_vec(&mut f, &b.projects)?;
    let slen: u32 = b.strings.len() as u32;
    f.write_all(&slen.to_le_bytes())?;
    f.write_all(&b.strings)?;

    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path)?;
    Ok(())
}

fn write_string_vec(f: &mut fs::File, v: &[String]) -> std::io::Result<()> {
    let count: u32 = v.len() as u32;
    f.write_all(&count.to_le_bytes())?;
    for s in v {
        let bytes = s.as_bytes();
        let len: u16 = bytes.len().min(u16::MAX as usize) as u16;
        f.write_all(&len.to_le_bytes())?;
        if let Some(slice) = bytes.get(..len as usize) {
            f.write_all(slice)?;
        }
    }
    Ok(())
}
