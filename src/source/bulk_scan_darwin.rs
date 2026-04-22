// macOS `getattrlistbulk(2)` — one syscall per directory instead of
// `readdir + stat` per file. Kept behind `#[cfg(target_os = "macos")]`
// and returns `None` on any FFI error so the portable scan path can
// take over.
//
// Apple reference:
//   https://developer.apple.com/library/archive/documentation/Darwin/Reference/ManPages/man2/getattrlistbulk.2.html

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

// ── attr.h bits we need ──

const ATTR_BIT_MAP_COUNT: u16 = 5;

// commonattr group
const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;

// fileattr group
const ATTR_FILE_DATALENGTH: u32 = 0x0000_0200;

// Vnode object types
const VREG: u32 = 1;

// Flags
const FSOPT_PACK_INVAL_ATTRS: u64 = 0x0000_0008;

// O_RDONLY; libc is already pulled in transitively by chrono/rayon but
// we'd rather not depend on it explicitly. Raw constants keep the FFI
// surface self-contained.
const O_RDONLY: i32 = 0;

#[repr(C)]
struct Attrlist {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AttributeSet {
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Attrreference {
    attr_dataoffset: i32,
    attr_length: u32,
}

#[allow(unsafe_code)]
unsafe extern "C" {
    fn getattrlistbulk(
        dirfd: i32,
        alist: *const Attrlist,
        attr_buf: *mut u8,
        buf_size: usize,
        options: u64,
    ) -> i32;
    fn open(path: *const i8, flags: i32, ...) -> i32;
    fn close(fd: i32) -> i32;
}

pub struct BulkEntry {
    pub name: String,
    pub is_regular_file: bool,
    pub mtime_secs: u64,
    pub size: u64,
}

/// Batch-stat every entry in `dir` with one kernel round-trip per buffer
/// fill (typically the whole directory in one call). Returns `None` on
/// any FFI error so the caller can fall back to the portable path.
#[allow(unsafe_code)]
pub fn scan(dir: &Path) -> Option<Vec<BulkEntry>> {
    let c_path = CString::new(dir.as_os_str().as_bytes()).ok()?;

    // Open the directory fd. getattrlistbulk requires an open fd, not a
    // path, so one open(2) per directory is the minimum cost.
    let fd = unsafe { open(c_path.as_ptr(), O_RDONLY) };
    if fd < 0 {
        return None;
    }

    let alist = Attrlist {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME | ATTR_CMN_OBJTYPE | ATTR_CMN_MODTIME,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_DATALENGTH,
        forkattr: 0,
    };

    // 32KB accommodates ~300 entries; we loop until the kernel returns 0.
    let mut buf = vec![0u8; 32 * 1024];
    let mut out: Vec<BulkEntry> = Vec::with_capacity(16);

    loop {
        let alist_ptr: *const Attrlist = &raw const alist;
        let n = unsafe {
            getattrlistbulk(
                fd,
                alist_ptr,
                buf.as_mut_ptr(),
                buf.len(),
                FSOPT_PACK_INVAL_ATTRS,
            )
        };
        if n == 0 {
            break; // end of directory
        }
        if n < 0 {
            let _ = unsafe { close(fd) };
            return None;
        }

        let mut cursor: usize = 0;
        for _ in 0..n {
            let Some(entry) = parse_entry(&buf, cursor) else {
                // Malformed entry — abort bulk path so the portable
                // readdir + stat path can take over.
                let _ = unsafe { close(fd) };
                return None;
            };
            cursor += entry.record_length;
            out.push(entry.into_public());
        }
    }

    let _ = unsafe { close(fd) };
    Some(out)
}

struct ParsedEntry {
    record_length: usize,
    name: String,
    is_regular_file: bool,
    mtime_secs: u64,
    size: u64,
}

impl ParsedEntry {
    fn into_public(self) -> BulkEntry {
        BulkEntry {
            name: self.name,
            is_regular_file: self.is_regular_file,
            mtime_secs: self.mtime_secs,
            size: self.size,
        }
    }
}

// Parse one record starting at `cursor` in `buf`. Returns None on any
// structural problem — we bail back to the portable path rather than
// speculate about partially-read data.
#[allow(unsafe_code)]
fn parse_entry(buf: &[u8], start: usize) -> Option<ParsedEntry> {
    // Record layout (we requested RETURNED_ATTRS + NAME + OBJTYPE + MODTIME
    // in commonattr, DATALENGTH in fileattr, so attrs appear in bit order):
    //
    //   0..4    u32 length           (total bytes consumed by this record)
    //   4..24   AttributeSet         (which attrs actually got filled in)
    //   24..32  Attrreference (name) (offset + len pointing into this record)
    //   32..36  u32 obj_type
    //   (pad to 8-byte alignment)
    //   ..      struct timespec mod_time (sec, nsec) — 16 bytes on 64-bit
    //   ..      u64 data_length      (only if file was regular — fileattr)
    //   ..      name bytes (NUL-terminated utf8), addressed via name_ref
    let len_bytes = buf.get(start..start + 4)?;
    let record_length = u32::from_ne_bytes(len_bytes.try_into().ok()?) as usize;
    if record_length < 24 || start + record_length > buf.len() {
        return None;
    }

    // AttributeSet tells us which attrs were actually filled. If the kernel
    // dropped one we asked for (e.g. modtime unavailable), the field is
    // absent from the record. Use read_unaligned — we can't assume the
    // record starts on a 4-byte boundary even though it usually does.
    #[allow(clippy::cast_ptr_alignment)] // read_unaligned handles it
    let returned_ptr = unsafe { buf.as_ptr().add(start + 4).cast::<AttributeSet>() };
    let returned: AttributeSet = unsafe { returned_ptr.read_unaligned() };
    let mut cursor = start + 4 + size_of::<AttributeSet>();

    // Name reference (always immediately after RETURNED_ATTRS when requested).
    let name_ref_pos = cursor;
    #[allow(clippy::cast_ptr_alignment)] // read_unaligned handles it
    let nref_ptr = unsafe { buf.as_ptr().add(cursor).cast::<Attrreference>() };
    let nref: Attrreference = unsafe { nref_ptr.read_unaligned() };
    cursor += size_of::<Attrreference>();

    // obj_type: u32, if filled
    let mut obj_type: u32 = 0;
    if returned.commonattr & ATTR_CMN_OBJTYPE != 0 {
        let b = buf.get(cursor..cursor + 4)?;
        obj_type = u32::from_ne_bytes(b.try_into().ok()?);
        cursor += 4;
    }

    // mod_time: struct timespec (16 bytes on 64-bit Darwin). Unlike
    // getattrlist, the bulk form packs these tight — no 8-byte padding
    // between obj_type and tv_sec. Verified empirically on macOS 14+.
    let mut mtime_secs: u64 = 0;
    if returned.commonattr & ATTR_CMN_MODTIME != 0 {
        let b = buf.get(cursor..cursor + 8)?;
        mtime_secs = u64::from_ne_bytes(b.try_into().ok()?);
        cursor += 16; // skip sec + nsec together
    }

    // data_length: u64, only populated for regular files
    let size: u64 = if returned.fileattr & ATTR_FILE_DATALENGTH != 0 {
        let b = buf.get(cursor..cursor + 8)?;
        u64::from_ne_bytes(b.try_into().ok()?)
    } else {
        0
    };

    // Name lives at `name_ref_pos + attr_dataoffset`. `attr_length`
    // includes the trailing NUL. `name_ref_pos` is bounded by `buf.len()`
    // (≤ 32K) so the isize cast is fine on any 32/64-bit target.
    let name_abs =
        (name_ref_pos.cast_signed()).checked_add(nref.attr_dataoffset as isize)? as usize;
    if nref.attr_length == 0 {
        return None;
    }
    let end = name_abs + nref.attr_length as usize - 1; // drop trailing NUL
    let name_bytes = buf.get(name_abs..end)?;
    let name = std::str::from_utf8(name_bytes).ok()?.to_string();

    Some(ParsedEntry {
        record_length,
        name,
        is_regular_file: obj_type == VREG,
        mtime_secs,
        size,
    })
}
