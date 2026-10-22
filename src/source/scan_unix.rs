// `default_scan` on Unix: the same walk as the portable `std::fs` one,
// with every lookup made relative to an open directory fd, and directory
// listings reused across runs while a directory's mtime holds still.
//
// On macOS, std's `DirEntry::metadata` is `lstat(full path)` and
// `read_dir(subdir)` opens by full path, so each stat and each descent
// re-resolves `/Users/…/.claude/projects/<project>/…` from the root.
// `fstatat` / `openat` against an open fd resolve only what's below it.
//
// Listing a directory (open, fdopendir, getdirentries until empty,
// close) costs ~10 µs; a project tree has hundreds of them, nearly all
// unchanged between two runs, and that listing was ~4 ms of a 7 ms warm
// `statusline`. A directory's mtime changes whenever an entry is added,
// removed or renamed, so a listing taken at a known mtime stays valid
// until it moves. Appends to a log don't touch its directory, which is
// why every `*.jsonl` is still stat'ed on every run.

use super::{SourceFile, path_hash};
use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Bump when the walk changes what it records (or `MAX_SCAN_DEPTH`
/// changes), so a snapshot from another version is ignored.
const SNAPSHOT_VERSION: u32 = 1;

/// A directory modified this recently is listed again next run rather
/// than trusted: on a filesystem with coarse timestamps (1 s on HFS+,
/// 2 s on FAT) an entry added in the same tick as the listing would
/// leave the mtime unchanged.
const TRUST_AFTER_NS: i128 = 2_000_000_000;

#[derive(Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    depth: u32,
    root: Option<DirSnap>,
}

/// One directory's listing — subdirectories walked and `*.jsonl` files,
/// in readdir order — and the mtime it was taken at.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
struct DirSnap {
    /// `(secs, nanos)`, or `None` when it was too recent to trust.
    mtime: Option<(i64, i64)>,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
enum Entry {
    Jsonl(Vec<u8>),
    Dir(Vec<u8>, DirSnap),
}

/// Walk `dir` to `depth_left` levels, appending every `*.jsonl` regular
/// file (symlinks not followed below `dir`) with its (mtime, size).
pub fn scan(dir: &Path, depth_left: usize, out: &mut Vec<SourceFile>) {
    let _ = scan_with_snapshot(dir, depth_left, out, snapshot_path(dir).as_deref());
}

/// `~/.claude/ccaudit-cache/scan-<root hash>.bin`, next to the caches the
/// scan feeds.
fn snapshot_path(root: &Path) -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".claude")
            .join("ccaudit-cache")
            .join(format!("scan-{:016x}.bin", path_hash(root)))
    })
}

/// [`scan`], reading and refreshing the listing snapshot at `snap_path`
/// (none: list everything, save nothing). Returns whether any directory
/// had to be listed.
pub(super) fn scan_with_snapshot(
    dir: &Path,
    depth_left: usize,
    out: &mut Vec<SourceFile>,
    snap_path: Option<&Path>,
) -> bool {
    if depth_left == 0 {
        return false;
    }
    let Ok(c_dir) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c_dir` is a valid NUL-terminated path; the fd is closed
    // below.
    #[allow(unsafe_code)]
    let rootfd = unsafe {
        libc::open(
            c_dir.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if rootfd < 0 {
        return false;
    }
    let depth = u32::try_from(depth_left).unwrap_or(u32::MAX);
    let old_bytes = snap_path.and_then(|p| std::fs::read(p).ok());
    let cached = old_bytes
        .as_deref()
        .and_then(|b| postcard::from_bytes::<Snapshot>(b).ok())
        .filter(|s| s.version == SNAPSHOT_VERSION && s.depth == depth)
        .and_then(|s| s.root);
    let mut walk = Walk {
        rootfd,
        root: dir,
        out,
        now_ns: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i128::try_from(d.as_nanos()).unwrap_or(i128::MAX)),
        listed: false,
    };
    let snap = walk.dir(&[], depth_left, cached);
    let listed = walk.listed;
    // SAFETY: `rootfd` is ours and no longer used.
    #[allow(unsafe_code)]
    let _ = unsafe { libc::close(rootfd) };
    if let Some(p) = snap_path {
        let snap = Snapshot {
            version: SNAPSHOT_VERSION,
            depth,
            root: snap,
        };
        // A directory too recent to trust is listed on every run but
        // usually lists the same; skip rewriting an identical snapshot.
        if let Ok(bytes) = postcard::to_allocvec(&snap) {
            if old_bytes.as_deref() != Some(&bytes[..]) {
                save(p, &bytes);
            }
        }
    }
    listed
}

fn save(path: &Path, bytes: &[u8]) {
    // Per-process tmp + rename: a concurrent run never reads a torn file.
    let tmp = path.with_extension(format!("bin.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, bytes).is_err() || std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

struct Walk<'a> {
    rootfd: libc::c_int,
    root: &'a Path,
    out: &'a mut Vec<SourceFile>,
    now_ns: i128,
    /// Some directory was listed afresh, so the snapshot needs saving.
    listed: bool,
}

impl Walk<'_> {
    /// Walk the directory at `rel` (relative to the root; empty for the
    /// root), reusing `cached` if its mtime still matches. `None` if it
    /// can't be stat'ed as a directory.
    fn dir(&mut self, rel: &[u8], depth_left: usize, cached: Option<DirSnap>) -> Option<DirSnap> {
        if depth_left == 0 {
            return None;
        }
        // Stat before listing: an entry added in between leaves the
        // recorded mtime older than the directory's, so the next run
        // lists it again rather than missing the entry.
        let st = self.stat_rel(rel)?;
        if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return None;
        }
        // `c_long`: i64 on the 64-bit targets ccaudit ships, i32 elsewhere.
        #[allow(clippy::useless_conversion)]
        let mtime = (st.st_mtime, i64::from(st.st_mtime_nsec));
        match cached {
            Some(c) if c.mtime == Some(mtime) => Some(self.replay(rel, depth_left, c)),
            c => self.list(rel, depth_left, mtime, c),
        }
    }

    /// An unchanged directory: its recorded entries, re-stat'ed.
    fn replay(&mut self, rel: &[u8], depth_left: usize, c: DirSnap) -> DirSnap {
        let mut entries = Vec::with_capacity(c.entries.len());
        for e in c.entries {
            match e {
                Entry::Jsonl(name) => {
                    let st = self.stat_rel(&join(rel, &name));
                    self.push(rel, &name, st);
                    entries.push(Entry::Jsonl(name));
                }
                Entry::Dir(name, sub) => {
                    if let Some(s) = self.dir(&join(rel, &name), depth_left - 1, Some(sub)) {
                        entries.push(Entry::Dir(name, s));
                    }
                }
            }
        }
        DirSnap {
            mtime: c.mtime,
            entries,
        }
    }

    /// List the directory afresh. Subdirectories keep their own cached
    /// listings — a new file in a project doesn't change its sessions'
    /// subagent directories.
    #[allow(unsafe_code)]
    fn list(
        &mut self,
        rel: &[u8],
        depth_left: usize,
        mtime: (i64, i64),
        cached: Option<DirSnap>,
    ) -> Option<DirSnap> {
        self.listed = true;
        let mut sub_cache: rustc_hash::FxHashMap<Vec<u8>, DirSnap> = cached
            .into_iter()
            .flat_map(|c| c.entries)
            .filter_map(|e| match e {
                Entry::Dir(name, s) => Some((name, s)),
                Entry::Jsonl(_) => None,
            })
            .collect();
        let c_rel = rel_cstring(rel)?;
        // SAFETY: `c_rel` is NUL-terminated and relative to the open
        // `rootfd`; the new fd is owned by the DIR stream below.
        let fd = unsafe {
            libc::openat(
                self.rootfd,
                c_rel.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return None;
        }
        // SAFETY: `fd` is an open directory fd we own; on success the
        // stream owns it and `closedir` below releases both.
        let dirp = unsafe { libc::fdopendir(fd) };
        if dirp.is_null() {
            // SAFETY: fdopendir failed, so `fd` is still ours to close.
            let _ = unsafe { libc::close(fd) };
            return None;
        }
        let mut entries = Vec::new();
        loop {
            // SAFETY: `dirp` is a live stream; the entry stays valid until
            // the next `readdir` / `closedir`, and is copied out before.
            let ent = unsafe { libc::readdir(dirp) };
            if ent.is_null() {
                break;
            }
            // SAFETY: non-null `readdir` result; `d_name` is NUL-terminated.
            let (d_type, name) = unsafe { ((*ent).d_type, CStr::from_ptr((*ent).d_name.as_ptr())) };
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let kind = if d_type == libc::DT_UNKNOWN {
                // Filesystems that don't fill d_type: classify by lstat,
                // as std's `DirEntry::file_type` does.
                match lstat_at(fd, name) {
                    Some(st) => match st.st_mode & libc::S_IFMT {
                        libc::S_IFDIR => libc::DT_DIR,
                        libc::S_IFREG => libc::DT_REG,
                        _ => continue,
                    },
                    None => continue,
                }
            } else {
                d_type
            };
            if kind == libc::DT_DIR {
                if depth_left > 1 {
                    let name = bytes.to_vec();
                    let sub = sub_cache.remove(&name);
                    if let Some(s) = self.dir(&join(rel, &name), depth_left - 1, sub) {
                        entries.push(Entry::Dir(name, s));
                    }
                }
                continue;
            }
            if kind != libc::DT_REG || !is_jsonl(bytes) {
                continue;
            }
            let st = lstat_at(fd, name);
            self.push(rel, bytes, st);
            entries.push(Entry::Jsonl(bytes.to_vec()));
        }
        // SAFETY: `dirp` is live and not used after this; closes `fd` too.
        let _ = unsafe { libc::closedir(dirp) };
        let age = self.now_ns - (i128::from(mtime.0) * 1_000_000_000 + i128::from(mtime.1));
        Some(DirSnap {
            mtime: (age >= TRUST_AFTER_NS).then_some(mtime),
            entries,
        })
    }

    fn push(&mut self, rel: &[u8], name: &[u8], st: Option<libc::stat>) {
        let Some(st) = st else {
            return;
        };
        if st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return;
        }
        let p = self.root.join(OsStr::from_bytes(&join(rel, name)));
        self.out.push(SourceFile {
            path_hash: path_hash(&p),
            path: p,
            // Pre-epoch clamps to 0, as `duration_since(UNIX_EPOCH)`
            // failing does in the portable walk.
            mtime: u64::try_from(st.st_mtime).unwrap_or(0),
            size: u64::try_from(st.st_size).unwrap_or(0),
        });
    }

    /// lstat of `rel` below the root (the root itself when empty).
    fn stat_rel(&self, rel: &[u8]) -> Option<libc::stat> {
        lstat_at(self.rootfd, &rel_cstring(rel)?)
    }
}

/// Exact, case-sensitive `.jsonl` extension, as `Path::extension` sees it
/// (so a file named just `.jsonl` doesn't count).
fn is_jsonl(name: &[u8]) -> bool {
    Path::new(OsStr::from_bytes(name))
        .extension()
        .and_then(|x| x.to_str())
        == Some("jsonl")
}

fn join(rel: &[u8], name: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(rel.len() + 1 + name.len());
    if !rel.is_empty() {
        v.extend_from_slice(rel);
        v.push(b'/');
    }
    v.extend_from_slice(name);
    v
}

fn rel_cstring(rel: &[u8]) -> Option<CString> {
    CString::new(if rel.is_empty() { &b"."[..] } else { rel }).ok()
}

#[allow(unsafe_code)]
fn lstat_at(dirfd: libc::c_int, name: &CStr) -> Option<libc::stat> {
    // SAFETY: `st` is written by `fstatat` before it's read, and only
    // read on success.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        (libc::fstatat(dirfd, name.as_ptr(), &raw mut st, libc::AT_SYMLINK_NOFOLLOW) == 0)
            .then_some(st)
    }
}
