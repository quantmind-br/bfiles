//! Parallel directory walker built directly on `getdents64` + `statx`.
//!
//! Each directory is opened once, read with a thread-local `getdents64` buffer,
//! and every entry is sized with a `statx` relative to that directory's file
//! descriptor — the kernel resolves a single path component instead of walking
//! the whole path again per file. A path is only materialized for directories
//! (needed to recurse) and for files that pass the size filter.

use std::cell::RefCell;
use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawDir, StatxFlags, open, statx};

/// getdents64 buffer size. Large enough that most directories need a single
/// syscall; reused across every directory a thread visits.
const DENTS_BUF: usize = 64 * 1024;

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

thread_local! {
    static DENTS: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(DENTS_BUF));
}

struct Ctx {
    min_size: u64,
    errors: AtomicUsize,
    found: Mutex<Vec<(PathBuf, u64)>>,
}

/// Walk `root` and return every regular file of at least `min_size` bytes,
/// along with the number of entries that could not be read.
///
/// Symlinks are never followed. Hidden entries are included.
pub fn scan(root: PathBuf, min_size: u64) -> (Vec<(PathBuf, u64)>, usize) {
    let ctx = Ctx {
        min_size,
        errors: AtomicUsize::new(0),
        found: Mutex::new(Vec::new()),
    };
    rayon::scope(|s| walk(&ctx, root, s));
    (
        ctx.found.into_inner().unwrap_or_else(|e| e.into_inner()),
        ctx.errors.into_inner(),
    )
}

fn walk<'s>(ctx: &'s Ctx, dir: PathBuf, scope: &rayon::Scope<'s>) {
    let fd = match open(&dir, DIR_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(_) => {
            ctx.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut local: Vec<(PathBuf, u64)> = Vec::new();
    // Accumulated locally so a failing directory costs one atomic, not one per entry.
    let mut errors = 0usize;

    let join = |name: &[u8]| -> PathBuf {
        let mut p = PathBuf::with_capacity(dir.as_os_str().len() + 1 + name.len());
        p.push(&dir);
        p.push(OsStr::from_bytes(name));
        p
    };

    DENTS.with_borrow_mut(|buf| {
        let mut iter = RawDir::new(&fd, buf.spare_capacity_mut());
        while let Some(entry) = iter.next() {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    errors += 1;
                    break;
                }
            };
            let name: &CStr = entry.file_name();
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            match entry.file_type() {
                FileType::Directory => subdirs.push(join(bytes)),
                FileType::RegularFile => match size_of(&fd, name, StatxFlags::SIZE) {
                    Some((_, size)) if size >= ctx.min_size => local.push((join(bytes), size)),
                    Some(_) => {}
                    None => errors += 1,
                },
                // Some filesystems leave d_type unset; resolve type and size at once.
                FileType::Unknown => {
                    match size_of(&fd, name, StatxFlags::SIZE | StatxFlags::TYPE) {
                        Some((mode, _)) if mode == S_IFDIR => subdirs.push(join(bytes)),
                        Some((mode, size)) if mode == S_IFREG && size >= ctx.min_size => {
                            local.push((join(bytes), size))
                        }
                        Some(_) => {}
                        None => errors += 1,
                    }
                }
                // Symlinks and other special files are never sized or followed.
                _ => {}
            }
        }
    });
    drop(fd);

    if errors > 0 {
        ctx.errors.fetch_add(errors, Ordering::Relaxed);
    }
    if !local.is_empty() {
        ctx.found.lock().unwrap().extend(local);
    }
    // Recurse into one child on this thread: keeps the work-stealing queue
    // shorter and avoids a task spawn for every leaf directory.
    let last = subdirs.pop();
    for sub in subdirs {
        scope.spawn(move |s| walk(ctx, sub, s));
    }
    if let Some(sub) = last {
        walk(ctx, sub, scope);
    }
}

/// `(file type bits, size)` of `name` relative to `dirfd`, without following symlinks.
fn size_of(dirfd: &rustix::fd::OwnedFd, name: &CStr, mask: StatxFlags) -> Option<(u32, u64)> {
    let st = statx(
        dirfd,
        name,
        AtFlags::SYMLINK_NOFOLLOW | AtFlags::STATX_DONT_SYNC,
        mask,
    )
    .ok()?;
    Some((st.stx_mode as u32 & S_IFMT, st.stx_size))
}
