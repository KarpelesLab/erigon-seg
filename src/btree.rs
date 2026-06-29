//! Reader for a `.bt` B-tree index file.
//!
//! For point lookups we only need one thing the `.bt` carries: an Elias-Fano array of
//! the `.kv` byte offset of every key, in key order. A lookup is then a binary search
//! that, at each probe `i`, seeks the `.kv` getter to `offset(i)` and decompresses the
//! key to compare. The in-file B-tree nodes are an optimization we don't require.
//!
//! Two on-disk layouts are supported:
//!
//! * **legacy** — `[EliasFano][nodes…]`; the first byte is `0x00` (the high byte of the
//!   EF `count`), and the EF starts at offset 0.
//! * **footer** — `[0x01][nodes…][EliasFano][footer][anchor]`; the fixed 16-byte anchor
//!   ends with the magic `erigon\0\0` and carries `footer_len`; the variable footer
//!   holds `keys_count`, `M`, and `ef_offset` locating the EF section.

use std::path::Path;

use crate::eliasfano::EliasFano;
use crate::error::{Error, Result};
use crate::util::mmap_file;

/// The fixed footer anchor is 16 bytes: `footer_len:u32 | flags:u16 | version:u16 | magic:u64`.
const ANCHOR_LEN: usize = 16;
/// The variable footer payload is at least `keys_count(8) | M(8) | ef_offset(8)`.
const META_LEN: usize = 24;
/// Trailing magic identifying the footer layout (and proving the file isn't truncated).
const FOOTER_MAGIC: [u8; 8] = *b"erigon\x00\x00";
/// First byte of a footer-layout file (a legacy file has `0x00` here).
const FIRST_BYTE_FOOTER: u8 = 0x01;

/// A `.bt` index: the Elias-Fano offset array plus, when known, the B-tree fanout `M`.
pub struct BtreeIndex {
    ef: Option<EliasFano>,
    m: Option<u64>,
}

impl BtreeIndex {
    /// Open and parse a `.bt` file, auto-detecting the legacy vs footer layout.
    pub fn open(path: impl AsRef<Path>) -> Result<BtreeIndex> {
        let path = path.as_ref();
        let mmap = mmap_file(path)?;
        let len = mmap.len();

        // A zero-length .bt is a valid empty index (0 keys).
        if len == 0 {
            return Ok(BtreeIndex { ef: None, m: None });
        }

        // Footer layout iff the trailing anchor carries the magic.
        if len >= ANCHOR_LEN && mmap[len - 8..len] == FOOTER_MAGIC {
            let anchor = &mmap[len - ANCHOR_LEN..];
            let footer_len = u32::from_be_bytes(anchor[0..4].try_into().unwrap()) as usize;
            if footer_len < META_LEN || ANCHOR_LEN + footer_len > len {
                return Err(Error::format(format!(
                    "{}: corrupt .bt footer (footer_len={footer_len}, file={len})",
                    path.display()
                )));
            }
            let footer_start = len - ANCHOR_LEN - footer_len;
            let payload = &mmap[footer_start..len - ANCHOR_LEN];
            let keys_count = u64::from_be_bytes(payload[0..8].try_into().unwrap());
            let m = u64::from_be_bytes(payload[8..16].try_into().unwrap());
            let ef_offset = u64::from_be_bytes(payload[16..24].try_into().unwrap()) as usize;
            if ef_offset >= footer_start {
                return Err(Error::format(format!(
                    "{}: corrupt .bt footer (ef_offset={ef_offset} >= body={footer_start})",
                    path.display()
                )));
            }
            let ef = EliasFano::open(mmap, ef_offset)?;
            if ef.len() != keys_count {
                return Err(Error::format(format!(
                    "{}: .bt EF has {} keys, footer says {keys_count}",
                    path.display(),
                    ef.len()
                )));
            }
            return Ok(BtreeIndex {
                ef: Some(ef),
                m: Some(m),
            });
        }

        // No magic: must be the legacy layout, whose first byte is 0x00.
        if mmap[0] == FIRST_BYTE_FOOTER {
            return Err(Error::format(format!(
                "{}: .bt looks like footer layout but the trailing magic is missing (truncated?)",
                path.display()
            )));
        }
        let ef = EliasFano::open(mmap, 0)?;
        Ok(BtreeIndex {
            ef: Some(ef),
            m: None,
        })
    }

    /// Number of indexed keys.
    pub fn key_count(&self) -> u64 {
        self.ef.as_ref().map_or(0, EliasFano::len)
    }

    /// The `.kv` byte offset of the `i`-th key (0-based). Returns `None` if out of range.
    pub fn key_offset(&self, i: u64) -> Option<u64> {
        let ef = self.ef.as_ref()?;
        (i < ef.len()).then(|| ef.get(i))
    }

    /// The B-tree fanout `M`, if the layout records it (footer layout only).
    pub fn m(&self) -> Option<u64> {
        self.m
    }

    /// Borrow the underlying Elias-Fano offset array, if the index is non-empty.
    pub fn elias_fano(&self) -> Option<&EliasFano> {
        self.ef.as_ref()
    }
}
