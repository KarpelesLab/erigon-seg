//! High-level [`DomainWriter`]: consume sorted `(key, value)` pairs and emit the full
//! file triple — `.kv` data, `.bt` index, and (optionally) `.kvei` existence filter.
//!
//! Keys must be supplied strictly increasing (sorted and unique), matching a domain
//! `.kv`'s on-disk invariant. The `.kv` is written first; the `.bt` and `.kvei` are then
//! built from the finished file using the same code paths verified against real erigon
//! data.

use std::path::{Path, PathBuf};

use super::bt_writer::{BtOptions, build_bt_from_seg};
use super::kvei_writer::build_kvei_from_seg;
use super::seg_writer::SegWriter;
use crate::error::{Error, Result};
use crate::seg::Seg;

/// Options for [`DomainWriter`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DomainOptions {
    /// `.bt` index layout/fanout.
    pub bt: BtOptions,
    /// If set, a `.kvei` bloom filter is built using this salt; if `None`, no `.kvei` is
    /// produced.
    pub salt: Option<u32>,
    /// Whether to pattern-compress the `.kv` (smaller output, extra passes). Default
    /// `false` (no-pattern fast path).
    pub compress: bool,
}

/// Paths written by [`DomainWriter::finish`].
#[derive(Debug, Clone)]
pub struct DomainPaths {
    /// The seg data file.
    pub kv: PathBuf,
    /// The B-tree index.
    pub bt: PathBuf,
    /// The existence filter, if one was built.
    pub kvei: Option<PathBuf>,
}

/// Builds a domain file set from sorted `(key, value)` pairs.
pub struct DomainWriter {
    kv_path: PathBuf,
    seg: SegWriter,
    opts: DomainOptions,
    last_key: Option<Vec<u8>>,
    key_count: u64,
}

impl DomainWriter {
    /// Create a writer that will produce `kv_path` plus sibling `.bt`/`.kvei` files.
    pub fn create(kv_path: impl AsRef<Path>, opts: DomainOptions) -> Result<DomainWriter> {
        let kv_path = kv_path.as_ref().to_path_buf();
        let seg = SegWriter::create_with(&kv_path, opts.compress)?;
        Ok(DomainWriter { kv_path, seg, opts, last_key: None, key_count: 0 })
    }

    /// Append one `(key, value)` pair. Keys must be strictly increasing.
    pub fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if let Some(last) = &self.last_key
            && key <= last.as_slice()
        {
            return Err(Error::format(format!(
                "DomainWriter: keys must be strictly increasing (got {:02x?} after {:02x?})",
                key, last
            )));
        }
        self.seg.add_word(key)?;
        self.seg.add_word(value)?;
        self.last_key = Some(key.to_vec());
        self.key_count += 1;
        Ok(())
    }

    /// Number of keys added so far.
    pub fn key_count(&self) -> u64 {
        self.key_count
    }

    /// Finalize: write the `.kv`, then build the `.bt` and (if a salt was given) `.kvei`.
    pub fn finish(self) -> Result<DomainPaths> {
        let kv_path = self.kv_path;
        let bt_path = kv_path.with_extension("bt");
        let opts = self.opts;

        self.seg.finish()?;
        let seg = Seg::open(&kv_path)?;

        build_bt_from_seg(&seg, &bt_path, opts.bt)?;

        let kvei = match opts.salt {
            Some(salt) => {
                let kvei_path = kv_path.with_extension("kvei");
                build_kvei_from_seg(&seg, salt, &kvei_path)?;
                Some(kvei_path)
            }
            None => None,
        };

        Ok(DomainPaths { kv: kv_path, bt: bt_path, kvei })
    }
}
