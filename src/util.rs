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
