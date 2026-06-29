//! Small shared helpers.

use std::path::Path;

use memmap2::Mmap;

use crate::error::{Error, Result};

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
