//! Reader for a `.bt` B-tree index file.
//!
//! The `.bt` carries two things a point lookup can use:
//!
//! * an Elias-Fano array of the `.kv` byte offset of every key, in key order; and
//! * (footer layout only) the *di-nodes*: the key at every `M`-th position, stored
//!   uncompressed.
//!
//! Without the di-nodes a lookup is a binary search over all `n` keys, where each probe
//! seeks the `.kv` getter to `offset(i)` and decompresses the key to compare — roughly
//! `log2(n)` decompressions, each landing on a different part of the file. The di-nodes
//! narrow that to the one `M`-key block that can contain the key, using only `memcmp`
//! against keys already in memory: `log2(n/M)` comparisons plus `log2(M)` decompressions,
//! and those decompressions all land inside a single contiguous block. See
//! [`Nodes::narrow`].
//!
//! Two on-disk layouts are supported:
//!
//! * **legacy** — `[EliasFano][nodes…]`; the first byte is `0x00` (the high byte of the
//!   EF `count`), and the EF starts at offset 0. The trailing nodes are not located by
//!   any header, so narrowing is unavailable and lookups use the full binary search.
//! * **footer** — `[0x01][nodes…][EliasFano][footer][anchor]`; the fixed 16-byte anchor
//!   ends with the magic `erigon\0\0` and carries `footer_len`; the variable footer
//!   holds `keys_count`, `M`, and `ef_offset` locating the EF section.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use memmap2::Mmap;

use crate::eliasfano::EliasFano;
use crate::error::{Error, Result};
use crate::util::{Advice, advise_mmap, lock_mmap, mmap_file, preload_mmap, unlock_mmap};

/// The fixed footer anchor is 16 bytes: `footer_len:u32 | flags:u16 | version:u16 | magic:u64`.
const ANCHOR_LEN: usize = 16;
/// The variable footer payload is at least `keys_count(8) | M(8) | ef_offset(8)`.
const META_LEN: usize = 24;
/// Trailing magic identifying the footer layout (and proving the file isn't truncated).
const FOOTER_MAGIC: [u8; 8] = *b"erigon\x00\x00";
/// First byte of a footer-layout file (a legacy file has `0x00` here).
const FIRST_BYTE_FOOTER: u8 = 0x01;

/// A `.bt` index: the Elias-Fano offset array plus, when known, the B-tree fanout `M`
/// and the di-node array used to narrow lookups.
pub struct BtreeIndex {
    ef: Option<EliasFano>,
    m: Option<u64>,
    /// Kept so the di-nodes can be parsed on first use rather than at open time.
    mmap: Arc<Mmap>,
    /// Where the di-nodes live, for the footer layout: `(keys_count, m, ef_offset)`.
    node_src: Option<(u64, u64, usize)>,
    /// Parsed lazily by [`BtreeIndex::nodes`]; `None` once parsing has been attempted
    /// and found the section unusable.
    nodes: OnceLock<Option<Nodes>>,
}

/// The `.bt` di-node array: the key at every `M`-th position.
///
/// Copied out of the mapping into one compact arena at first use, so the narrowing
/// search touches only hot, contiguous heap instead of faulting `.bt` pages scattered
/// across a multi-gigabyte file.
pub struct Nodes {
    /// All node keys back to back.
    arena: Vec<u8>,
    /// End offset of each node key within `arena`, or empty when every key has the same
    /// length (`fixed_len`), in which case offsets are computed arithmetically.
    ends: Vec<u32>,
    /// `Some(len)` when every node key is `len` bytes — the common case, which lets us
    /// drop `ends` entirely (4 bytes/node saved).
    fixed_len: Option<u32>,
    count: usize,
    m: u64,
    key_count: u64,
}

impl Nodes {
    /// Number of nodes (`ceil(key_count / M)`).
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether there are no nodes.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The B-tree fanout `M` these nodes were sampled at.
    pub fn m(&self) -> u64 {
        self.m
    }

    /// Heap held by the arena and (when present) the offset table.
    pub fn heap_bytes(&self) -> usize {
        self.arena.len() + self.ends.len() * 4
    }

    /// The `j`-th node key, which is key number `j * M` in the file.
    ///
    /// # Panics
    /// Panics if `j >= self.len()`.
    #[inline]
    pub fn key(&self, j: usize) -> &[u8] {
        match self.fixed_len {
            Some(l) => {
                let l = l as usize;
                &self.arena[j * l..(j + 1) * l]
            }
            None => {
                let start = if j == 0 { 0 } else { self.ends[j - 1] as usize };
                &self.arena[start..self.ends[j] as usize]
            }
        }
    }

    /// Narrow a search for `key` to the half-open key-index range that can contain it.
    ///
    /// Node `j` is key `j * M`, and keys are sorted, so if `node[j] <= key < node[j+1]`
    /// then `key`, if present, lies in `[j*M, (j+1)*M)`. Returns an empty range when
    /// `key` sorts before the very first key, which cannot be in the file at all.
    #[inline]
    pub fn narrow(&self, key: &[u8]) -> (u64, u64) {
        // Index of the first node strictly greater than `key`.
        let mut lo = 0usize;
        let mut hi = self.count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.key(mid) <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return (0, 0); // key < node[0] == first key in the file
        }
        let start = (lo as u64 - 1) * self.m;
        (start, (start + self.m).min(self.key_count))
    }
}

impl BtreeIndex {
    /// Open and parse a `.bt` file, auto-detecting the legacy vs footer layout.
    pub fn open(path: impl AsRef<Path>) -> Result<BtreeIndex> {
        let path = path.as_ref();
        let mmap = mmap_file(path)?;
        let len = mmap.len();

        let mmap = Arc::new(mmap);

        // A zero-length .bt is a valid empty index (0 keys).
        if len == 0 {
            return Ok(BtreeIndex {
                ef: None,
                m: None,
                mmap,
                node_src: None,
                nodes: OnceLock::from(None),
            });
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
            let ef = EliasFano::open(Arc::clone(&mmap), ef_offset)?;
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
                mmap,
                node_src: Some((keys_count, m, ef_offset)),
                nodes: OnceLock::new(),
            });
        }

        // No magic: must be the legacy layout, whose first byte is 0x00.
        if mmap[0] == FIRST_BYTE_FOOTER {
            return Err(Error::format(format!(
                "{}: .bt looks like footer layout but the trailing magic is missing (truncated?)",
                path.display()
            )));
        }
        let ef = EliasFano::open(Arc::clone(&mmap), 0)?;
        Ok(BtreeIndex {
            ef: Some(ef),
            m: None,
            mmap,
            // Legacy layout: nothing locates the trailing nodes, so there is no narrowing.
            node_src: None,
            nodes: OnceLock::from(None),
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

    /// The di-node array, parsed on first call and cached thereafter.
    ///
    /// Returns `None` for the legacy layout, for an empty index, or if the section does
    /// not parse — in each case lookups simply fall back to the full binary search.
    ///
    /// Parsing walks the whole node section once (`ceil(key_count / M)` entries) and
    /// copies the keys into an arena, so the first call costs one pass over that
    /// section and holds it in memory. It is deliberately *not* done at open time, so
    /// opening a file only to scan it — merging, re-encoding — pays nothing.
    pub fn nodes(&self) -> Option<&Nodes> {
        self.nodes
            .get_or_init(|| {
                let (key_count, m, ef_offset) = self.node_src?;
                parse_nodes(&self.mmap, key_count, m, ef_offset)
            })
            .as_ref()
    }

    /// Narrow a lookup for `key` to the half-open key-index range that can contain it,
    /// using the di-nodes. Falls back to the full range when narrowing is unavailable.
    #[inline]
    pub fn narrow(&self, key: &[u8]) -> (u64, u64) {
        match self.nodes() {
            Some(n) => n.narrow(key),
            None => (0, self.key_count()),
        }
    }

    /// Advise the kernel that this `.bt` is read in random order (point lookups). See
    /// [`KvReader::advise_random`](crate::KvReader::advise_random).
    pub fn advise_random(&self) -> std::io::Result<()> {
        advise_mmap(&self.mmap, Advice::Random)
    }

    /// Bytes this `.bt` occupies when fully resident — what
    /// [`preload`](BtreeIndex::preload) or [`lock`](BtreeIndex::lock) would cost.
    pub fn mapped_bytes(&self) -> u64 {
        self.mmap.len() as u64
    }

    /// Read the whole `.bt` into the page cache, returning once it is resident. See
    /// [`KvReader::preload_index`](crate::KvReader::preload_index).
    pub fn preload(&self) -> u64 {
        preload_mmap(&self.mmap) as u64
    }

    /// Pin the whole `.bt` in RAM with `mlock`. See
    /// [`KvReader::lock_index`](crate::KvReader::lock_index) for the caveats.
    pub fn lock(&self) -> std::io::Result<()> {
        lock_mmap(&self.mmap)
    }

    /// Release an [`mlock`](BtreeIndex::lock).
    pub fn unlock(&self) -> std::io::Result<()> {
        unlock_mmap(&self.mmap)
    }

    /// Borrow the underlying Elias-Fano offset array, if the index is non-empty.
    pub fn elias_fano(&self) -> Option<&EliasFano> {
        self.ef.as_ref()
    }
}

/// Parse the di-node array out of a footer-layout `.bt`.
///
/// Layout from byte 1 (after the `0x01` marker): `keys_count / M` entries of
/// `klen:u16-BE | key`, then zero padding up to `ef_offset`. Returns `None` if anything
/// fails to line up, which only costs the narrowing optimization.
fn parse_nodes(data: &[u8], key_count: u64, m: u64, ef_offset: usize) -> Option<Nodes> {
    if key_count == 0 || m == 0 || ef_offset <= 1 || ef_offset > data.len() {
        return None;
    }
    let count = usize::try_from(key_count.div_ceil(m)).ok()?;
    let mut arena: Vec<u8> = Vec::new();
    let mut ends: Vec<u32> = Vec::with_capacity(count);
    let mut fixed_len: Option<u32> = None;
    let mut uniform = true;
    let mut p = 1usize;
    for j in 0..count {
        let lb = data.get(p..p + 2)?;
        let klen = u16::from_be_bytes(lb.try_into().ok()?) as usize;
        p += 2;
        if p + klen > ef_offset {
            return None;
        }
        match fixed_len {
            None if j == 0 => fixed_len = Some(klen as u32),
            Some(l) if l as usize != klen => uniform = false,
            _ => {}
        }
        arena.extend_from_slice(&data[p..p + klen]);
        p += klen;
        ends.push(u32::try_from(arena.len()).ok()?);
    }
    if uniform {
        // Every key is the same length, so offsets are `j * len` — drop the table.
        ends = Vec::new();
    } else {
        fixed_len = None;
    }
    Some(Nodes {
        arena,
        ends,
        fixed_len,
        count,
        m,
        key_count,
    })
}
