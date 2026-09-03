//! Small shared helpers.

use std::path::Path;

use memmap2::Mmap;

use crate::error::{Error, Result};

/// How a mapping is expected to be accessed, for [`advise_mmap`].
///
/// Mirrors the subset of `madvise` advices we use, so the rest of the crate can pass
/// one around on every platform even though `madvise` itself is Unix-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Advice {
    /// Point lookups: suppress read-ahead so a fault reads one page, not a 64 KiB window.
    Random,
    /// Full scans: read ahead aggressively.
    Sequential,
    /// These pages will be needed soon; start pulling them in.
    WillNeed,
}

/// Apply [`Advice`] to a mapping. A no-op (returning `Ok`) on non-Unix platforms, where
/// `madvise` is unavailable — advice is a hint, so ignoring it only costs performance.
#[cfg(unix)]
pub(crate) fn advise_mmap(mmap: &Mmap, advice: Advice) -> std::io::Result<()> {
    mmap.advise(match advice {
        Advice::Random => memmap2::Advice::Random,
        Advice::Sequential => memmap2::Advice::Sequential,
        Advice::WillNeed => memmap2::Advice::WillNeed,
    })
}

/// See the Unix implementation above.
#[cfg(not(unix))]
pub(crate) fn advise_mmap(_mmap: &Mmap, _advice: Advice) -> std::io::Result<()> {
    Ok(())
}

/// Read every page of `mmap` into the page cache, returning once they are resident.
///
/// `MADV_WILLNEED` only *schedules* read-ahead, so we also touch one byte per page to
/// wait for it.
///
/// The advice around that walk is not incidental. An index worth preloading is one we
/// are about to point-query, so it is normally already marked [`Advice::Random`] — and
/// under that advice the kernel suppresses read-ahead, turning this walk into one
/// synchronous fault per page: measured 109 MiB/s against 2.7 GiB/s, a 25× difference on
/// a 1.4 GiB index. So we mark it sequential for the walk and restore random access
/// afterwards, which is the right steady state for the lookups that follow.
///
/// Steps by 4 KiB, the smallest page size on any platform we run on — a larger page just
/// gets touched more than once, which is harmless.
pub(crate) fn preload_mmap(mmap: &Mmap) -> usize {
    let _ = advise_mmap(mmap, Advice::Sequential);
    let _ = advise_mmap(mmap, Advice::WillNeed);

    let len = mmap.len();
    let mut acc = 0u64;
    let mut i = 0usize;
    while i < len {
        acc = acc.wrapping_add(mmap[i] as u64);
        i += 4096;
    }
    std::hint::black_box(acc);

    let _ = advise_mmap(mmap, Advice::Random);
    len
}

/// The process's `RLIMIT_MEMLOCK` as `(soft, hard)` in bytes, or `None` off Unix.
/// `u64::MAX` means unlimited.
#[cfg(unix)]
pub(crate) fn memlock_limit_raw() -> Option<(u64, u64)> {
    let mut r = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` only writes through the provided pointer, which is a valid
    // local. It cannot fail for a valid resource id.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut r) };
    if rc != 0 {
        return None;
    }
    // `rlim_t` happens to be `u64` on the platforms we build for, which makes these
    // casts look redundant, but it is platform-defined — so convert explicitly rather
    // than assume.
    #[allow(clippy::unnecessary_cast)]
    Some((r.rlim_cur as u64, r.rlim_max as u64))
}

/// See the Unix implementation above.
#[cfg(not(unix))]
pub(crate) fn memlock_limit_raw() -> Option<(u64, u64)> {
    None
}

/// Set `RLIMIT_MEMLOCK` to `(soft, hard)`.
#[cfg(unix)]
pub(crate) fn set_memlock_limit_raw(soft: u64, hard: u64) -> std::io::Result<()> {
    let r = libc::rlimit {
        rlim_cur: soft as libc::rlim_t,
        rlim_max: hard as libc::rlim_t,
    };
    // SAFETY: `setrlimit` reads through the provided pointer, which is a valid local.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &r) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// See the Unix implementation above.
#[cfg(not(unix))]
pub(crate) fn set_memlock_limit_raw(_soft: u64, _hard: u64) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "RLIMIT_MEMLOCK is not available on this platform",
    ))
}

/// Raise the soft `RLIMIT_MEMLOCK` to the hard limit, once per process.
///
/// This is what makes `mlock` usable on a stock Linux box, where the soft limit is
/// commonly a few megabytes while the hard limit is unlimited: without it, locking fails
/// for a reason the administrator never intended. Raising the soft limit toward the hard
/// one needs no privilege and cannot exceed the configured policy — the hard limit *is*
/// the policy — so it is safe to do implicitly. Raising the *hard* limit is a different
/// matter and is never done automatically; see
/// [`raise_memlock_limit`](crate::raise_memlock_limit).
///
/// Unix only: it is reached from `lock_mmap`, which does nothing elsewhere.
#[cfg(unix)]
pub(crate) fn raise_memlock_soft_once() {
    static DONE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    DONE.get_or_init(|| {
        if let Some((soft, hard)) = memlock_limit_raw()
            && soft < hard
        {
            let _ = set_memlock_limit_raw(hard, hard);
        }
    });
}

/// Pin a mapping in RAM with `mlock`.
///
/// Unlike [`advise_mmap`], the non-Unix path reports `Unsupported` rather than pretending
/// to succeed: advice is a hint whose loss only costs performance, but locking is a
/// guarantee the caller may be relying on, so silently not doing it would be a lie.
#[cfg(unix)]
pub(crate) fn lock_mmap(mmap: &Mmap) -> std::io::Result<()> {
    raise_memlock_soft_once();
    mmap.lock()
}

/// See the Unix implementation above.
#[cfg(not(unix))]
pub(crate) fn lock_mmap(_mmap: &Mmap) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "mlock is not available on this platform",
    ))
}

/// Release a [`lock_mmap`].
#[cfg(unix)]
pub(crate) fn unlock_mmap(mmap: &Mmap) -> std::io::Result<()> {
    mmap.unlock()
}

/// See the Unix implementation above.
#[cfg(not(unix))]
pub(crate) fn unlock_mmap(_mmap: &Mmap) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "mlock is not available on this platform",
    ))
}

/// Read-only memory-map of a file, treated as immutable for the map's lifetime.
pub(crate) fn mmap_file(path: &Path) -> Result<Mmap> {
    let f = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    // SAFETY: we only ever read through the map and document that callers must not
    // mutate the underlying file while a reader is open. memmap2 requires `unsafe`
    // here purely because that invariant cannot be expressed in the type system.
    #[allow(unsafe_code)]
    let mmap = unsafe { Mmap::map(&f) }.map_err(|e| Error::io(path, e))?;
    Ok(mmap)
}

/// The process's `RLIMIT_MEMLOCK` as `(soft, hard)` in bytes, or `None` on a platform
/// without it. [`u64::MAX`] means unlimited.
///
/// The soft limit is what `mlock` is actually checked against; the hard limit is the
/// ceiling the soft one can be raised to without privilege. Compare against
/// [`KvReader::total_bytes`](crate::KvReader::total_bytes) before
/// [`lock_all`](crate::KvReader::lock_all).
pub fn memlock_limit() -> Option<(u64, u64)> {
    memlock_limit_raw()
}

/// Raise `RLIMIT_MEMLOCK` as far as this process is permitted to, returning the
/// resulting `(soft, hard)`.
///
/// Two things can happen, and they need different privileges:
///
/// * The soft limit is raised to the hard limit. This needs no privilege and never
///   exceeds the administrator's policy, so the locking calls already do it for you —
///   calling this explicitly is only needed if you want the numbers back.
/// * The hard limit is raised to `want` (use [`u64::MAX`] for unlimited). This requires
///   `CAP_SYS_RESOURCE`, i.e. root, and is **not** attempted by anything else in this
///   crate: escalating past a configured limit is a decision for the application, not a
///   library reading files.
///
/// A failure to raise the hard limit is not reported as an error — the soft-limit raise
/// is what usually matters, and you can see what you ended up with in the return value.
/// Errors are reserved for not being able to read or set the limit at all.
///
/// This mutates process-wide state and affects every other user of the process.
pub fn raise_memlock_limit(want: u64) -> std::io::Result<(u64, u64)> {
    let (_, hard) = memlock_limit_raw().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "RLIMIT_MEMLOCK is not available on this platform",
        )
    })?;
    // Always take the free part: soft up to hard.
    set_memlock_limit_raw(hard, hard)?;
    // Then try to lift the ceiling itself, which only root can do. Ignore the refusal.
    if want > hard {
        let _ = set_memlock_limit_raw(want, want);
    }
    memlock_limit_raw()
        .ok_or_else(|| std::io::Error::other("RLIMIT_MEMLOCK became unreadable after being set"))
}
