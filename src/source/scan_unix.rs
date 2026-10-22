// `default_scan` on Unix: the same walk as the portable `std::fs` one,
// with every lookup made relative to an open directory fd.
//
// On macOS, std's `DirEntry::metadata` is `lstat(full path)` and
// `read_dir(subdir)` opens by full path, so each stat and each descent
// re-resolves `/Users/…/.claude/projects/<project>/…` from the root.
// `fstatat` / `openat` against the parent's fd resolve one component.

use super::{SourceFile, path_hash};
use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Walk `dir` to `depth_left` levels, appending every `*.jsonl` regular
/// file (symlinks not followed below `dir`) with its (mtime, size).
pub fn scan(dir: &Path, depth_left: usize, out: &mut Vec<SourceFile>) {
    if depth_left == 0 {
        return;
    }
    let Ok(c_dir) = CString::new(dir.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: `c_dir` is a valid NUL-terminated path; the returned fd is
    // owned by `walk`, which closes it.
    #[allow(unsafe_code)]
    let fd = unsafe {
        libc::open(
            c_dir.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd >= 0 {
        walk(fd, dir, depth_left, out);
    }
}

// Takes ownership of `fd` (closed via `closedir`, or directly when
// `fdopendir` fails). `depth_left` ≥ 1: this directory is listed.
#[allow(unsafe_code)]
fn walk(fd: libc::c_int, dir: &Path, depth_left: usize, out: &mut Vec<SourceFile>) {
    // SAFETY: `fd` is an open directory fd we own; on success the DIR
    // stream owns it and `closedir` below releases both.
    let dirp = unsafe { libc::fdopendir(fd) };
    if dirp.is_null() {
        // SAFETY: fdopendir failed, so `fd` is still ours to close.
        let _ = unsafe { libc::close(fd) };
        return;
    }
    loop {
        // SAFETY: `dirp` is a live stream; the entry stays valid until the
        // next `readdir` / `closedir`, and is only used within this
        // iteration.
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
            // Filesystems that don't fill d_type: classify by lstat, as
            // std's `DirEntry::file_type` does.
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
                // SAFETY: `name` is a NUL-terminated entry of the open
                // directory `fd`; the new fd is handed to `walk`.
                let sub = unsafe {
                    libc::openat(
                        fd,
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                    )
                };
                if sub >= 0 {
                    walk(
                        sub,
                        &dir.join(OsStr::from_bytes(bytes)),
                        depth_left - 1,
                        out,
                    );
                }
            }
            continue;
        }
        if kind != libc::DT_REG
            || Path::new(OsStr::from_bytes(bytes))
                .extension()
                .and_then(|x| x.to_str())
                != Some("jsonl")
        {
            continue;
        }
        let Some(st) = lstat_at(fd, name) else {
            continue;
        };
        let p = dir.join(OsStr::from_bytes(bytes));
        out.push(SourceFile {
            path_hash: path_hash(&p),
            path: p,
            // Pre-epoch clamps to 0, as `duration_since(UNIX_EPOCH)`
            // failing does in the portable walk.
            mtime: u64::try_from(st.st_mtime).unwrap_or(0),
            size: u64::try_from(st.st_size).unwrap_or(0),
        });
    }
    // SAFETY: `dirp` is live and not used after this; closes `fd` too.
    let _ = unsafe { libc::closedir(dirp) };
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
