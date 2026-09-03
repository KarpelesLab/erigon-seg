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
    /// Whether [`Nodes::narrow`] should issue explicit prefetches; see the constant.
    prefetch: bool,
}

/// Arena size above which [`Nodes::narrow`] prefetches both candidate probes.
///
/// Purely empirical, and about cache residency rather than any property of the format.
/// Measured on one machine: a 142 MiB arena gained 1.21x from prefetching, a 29 MiB one
/// was unchanged, and a 1.2 MiB one lost 8% to the wasted prefetch instructions. Any
/// threshold between those middle two behaves identically on that data; this sits in the
/// gap. If the search ever looks slow on a machine with a very different cache, this is
/// the knob.
const PREFETCH_ARENA_BYTES: usize = 16 << 20;

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
    ///
    /// Prefetches both possible next probes when the arena is large enough to miss cache
    /// (see [`PREFETCH_ARENA_BYTES`]); one of the two is always wasted, which is why it
    /// is not done unconditionally.
    ///
    /// This is deliberately a *branchy* binary search. Rewriting it branchlessly with
    /// `select_unpredictable` (a `cmov`) was measured as a 1.07-1.20x win while the node
    /// arena fits in L2, but a 0.50-0.62x *loss* once it does not — 34 MiB and 142 MiB
    /// arenas both regressed sharply. A mispredicted branch is recovered from in a few
    /// cycles, whereas `cmov` makes each level's load address depend on the previous
    /// comparison, serialising the search at full memory latency; the branchy form lets
    /// the CPU speculate down one side and issue the next load early, which is worth far
    /// more than the mispredictions cost. Large files are the ones that matter here, so
    /// the branchy form stays.
    #[inline]
    pub fn narrow(&self, key: &[u8]) -> (u64, u64) {
        // Decided once, not per level, so the loop body stays tight either way.
        let lo = if self.prefetch {
            self.upper_bound::<true>(key)
        } else {
            self.upper_bound::<false>(key)
        };
        if lo == 0 {
            return (0, 0); // key < node[0] == first key in the file
        }
        let start = (lo as u64 - 1) * self.m;
        (start, (start + self.m).min(self.key_count))
    }

    /// Number of nodes that are `<= key`, i.e. the index of the first node above it.
    #[inline(always)]
    fn upper_bound<const PREFETCH: bool>(&self, key: &[u8]) -> usize {
        let mut lo = 0usize;
        let mut hi = self.count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if PREFETCH {
                // The two nodes the next iteration could probe, whichever way this one goes.
                self.prefetch_key(mid + 1 + (hi - mid - 1) / 2);
                self.prefetch_key(lo + (mid - lo) / 2);
            }
            if self.key(mid) <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Hint the cache to fetch node `j`, if it exists. A no-op off x86-64.
    #[inline(always)]
    fn prefetch_key(&self, j: usize) {
        #[cfg(target_arch = "x86_64")]
        if j < self.count {
            let p = self.key(j).as_ptr();
            // SAFETY: `p` points into `arena`, since `j < self.count` and `key` returns a
            // slice of it. `_mm_prefetch` only reads a cache line and cannot fault.
            #[allow(unsafe_code)]
            unsafe {
                std::arch::x86_64::_mm_prefetch(p as *const i8, std::arch::x86_64::_MM_HINT_T0);
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = j;
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

    /// Ask the kernel to start pulling this `.bt` into the page cache, without waiting.
    /// See [`KvReader::advise_will_need`](crate::KvReader::advise_will_need).
    pub fn advise_will_need(&self) -> std::io::Result<()> {
        advise_mmap(&self.mmap, Advice::WillNeed)
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
    let prefetch = arena.len() >= PREFETCH_ARENA_BYTES;
    Some(Nodes {
        arena,
        ends,
        fixed_len,
        count,
        m,
        key_count,
        prefetch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `Nodes` directly, so both `upper_bound` specialisations can be exercised
    /// without a file large enough to trip [`PREFETCH_ARENA_BYTES`] (which would need
    /// hundreds of millions of keys).
    fn nodes_of(keys: &[&[u8]], prefetch: bool) -> Nodes {
        let mut arena = Vec::new();
        let mut ends = Vec::new();
        let uniform = keys.windows(2).all(|w| w[0].len() == w[1].len());
        for k in keys {
            arena.extend_from_slice(k);
            ends.push(arena.len() as u32);
        }
        Nodes {
            fixed_len: if uniform && !keys.is_empty() {
                Some(keys[0].len() as u32)
            } else {
                None
            },
            ends: if uniform { Vec::new() } else { ends },
            arena,
            count: keys.len(),
            m: 256,
            key_count: keys.len() as u64 * 256,
            prefetch,
        }
    }

    /// The prefetching and non-prefetching searches must be indistinguishable: prefetch
    /// is only a cache hint, never a change of result.
    #[test]
    fn prefetch_variant_matches_plain() {
        // Uniform-length keys, plus a variable-length set, plus a 1-element edge case.
        let uniform: Vec<[u8; 4]> = (0u32..500).map(|i| (i * 3).to_be_bytes()).collect();
        let uniform_refs: Vec<&[u8]> = uniform.iter().map(|k| &k[..]).collect();

        let variable: Vec<Vec<u8>> = (0u32..300)
            .map(|i| {
                let mut v = (i * 7).to_be_bytes().to_vec();
                v.truncate(2 + (i % 3) as usize);
                v.push(0xff);
                v
            })
            .collect();
        let mut variable = variable;
        variable.sort();
        variable.dedup();
        let variable_refs: Vec<&[u8]> = variable.iter().map(|k| &k[..]).collect();

        let single: Vec<&[u8]> = vec![b"mmmm"];

        for set in [&uniform_refs, &variable_refs, &single] {
            let plain = nodes_of(set, false);
            let pref = nodes_of(set, true);
            // Probe every stored key, plus the gaps either side of it, plus the extremes.
            let mut probes: Vec<Vec<u8>> = Vec::new();
            for k in set.iter() {
                probes.push(k.to_vec());
                let mut lo = k.to_vec();
                let last = lo.len() - 1;
                lo[last] = lo[last].wrapping_sub(1);
                probes.push(lo);
                let mut hi = k.to_vec();
                hi.push(0);
                probes.push(hi);
            }
            probes.push(Vec::new());
            probes.push(vec![0u8]);
            probes.push(vec![0xffu8; 8]);

            for p in &probes {
                assert_eq!(
                    plain.upper_bound::<false>(p),
                    pref.upper_bound::<true>(p),
                    "upper_bound disagrees for {p:02x?}"
                );
                assert_eq!(
                    plain.narrow(p),
                    pref.narrow(p),
                    "narrow disagrees for {p:02x?}"
                );
            }
        }
    }

    /// The gate is derived from the arena, so a small index must not be prefetching.
    #[test]
    fn small_arenas_do_not_prefetch() {
        let keys: Vec<[u8; 4]> = (0u32..10).map(|i| i.to_be_bytes()).collect();
        let refs: Vec<&[u8]> = keys.iter().map(|k| &k[..]).collect();
        let n = nodes_of(
            &refs,
            refs.iter().map(|k| k.len()).sum::<usize>() >= PREFETCH_ARENA_BYTES,
        );
        assert!(!n.prefetch, "a 40-byte arena must not opt into prefetching");
    }
}
