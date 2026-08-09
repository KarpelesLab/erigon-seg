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
}

/// Apply [`Advice`] to a mapping. A no-op (returning `Ok`) on non-Unix platforms, where
/// `madvise` is unavailable — advice is a hint, so ignoring it only costs performance.
#[cfg(unix)]
pub(crate) fn advise_mmap(mmap: &Mmap, advice: Advice) -> std::io::Result<()> {
    mmap.advise(match advice {
        Advice::Random => memmap2::Advice::Random,
        Advice::Sequential => memmap2::Advice::Sequential,
    })
}

/// See the Unix implementation above.
#[cfg(not(unix))]
pub(crate) fn advise_mmap(_mmap: &Mmap, _advice: Advice) -> std::io::Result<()> {
    Ok(())
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
