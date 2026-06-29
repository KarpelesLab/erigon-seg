//! Reader for a `.kvei` existence filter — a *negative* lookup accelerator.
//!
//! `contains_hash(h)` returning `false` means the key is **definitely absent**; `true`
//! means "probably present" (a small false-positive rate). It never reports a real key
//! as absent, so a `false` lets a point lookup skip the `.bt` search entirely.
//!
//! Two `.kvei` encodings exist in the wild:
//!
//! * the `holiman/bloomfilter/v2` layout (magic = 8 zero bytes then `v02\n`), a `k=3`
//!   filter with a rotate-17 + per-key-XOR hash schedule — fully supported here;
//! * a newer "fuse filter" layout (no bloom magic; a small version byte) — detected but
//!   not yet decoded. We treat it as "matches everything", which keeps lookups correct
//!   (just unaccelerated).

use std::path::Path;

use memmap2::Mmap;

use crate::error::Result;
use crate::util::mmap_file;

/// holiman/bloomfilter/v2 header magic: 8 zero bytes followed by `v02\n`.
const BLOOM_MAGIC: [u8; 12] = [0, 0, 0, 0, 0, 0, 0, 0, b'v', b'0', b'2', b'\n'];
/// Byte offset of the bit array in the bloom layout: magic(12) + k(8) + n(8) + m(8) + keys(24).
const BLOOM_BITS_OFFSET: usize = 60;

/// What kind of filter a `.kvei` turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterKind {
    /// An empty (0-byte) filter: matches every key.
    Empty,
    /// A supported `holiman/bloomfilter/v2` bloom filter.
    Bloom,
    /// A recognized-but-unsupported encoding (e.g. fuse filter): treated as match-all.
    Unsupported,
}

enum Inner {
    Empty,
    Bloom { keys: [u64; 3], m: u64, bits_off: usize },
    Unsupported,
}

/// A `.kvei` existence filter.
pub struct ExistenceFilter {
    // Held to keep the bit array mapped for the lifetime of `Inner::Bloom`.
    #[allow(dead_code)]
    mmap: Mmap,
    inner: Inner,
}

impl ExistenceFilter {
    /// Open and parse a `.kvei` file.
    ///
    /// Unknown-but-structurally-valid encodings open successfully as
    /// [`FilterKind::Unsupported`] (match-all) rather than erroring, so a reader can
    /// fall back to the exact `.bt` search without special-casing the filter format.
    pub fn open(path: impl AsRef<Path>) -> Result<ExistenceFilter> {
        let mmap = mmap_file(path.as_ref())?;
        let inner = Self::parse(&mmap);
        Ok(ExistenceFilter { mmap, inner })
    }

    fn parse(d: &[u8]) -> Inner {
        if d.is_empty() {
            return Inner::Empty;
        }
        if d.len() >= BLOOM_BITS_OFFSET && d[0..12] == BLOOM_MAGIC {
            let k = u64::from_le_bytes(d[12..20].try_into().unwrap());
            let m = u64::from_le_bytes(d[28..36].try_into().unwrap());
            let mut keys = [0u64; 3];
            for (i, key) in keys.iter_mut().enumerate() {
                *key = u64::from_le_bytes(d[36 + i * 8..44 + i * 8].try_into().unwrap());
            }
            let nwords = m.div_ceil(64) as usize;
            if k == 3 && m >= 2 && BLOOM_BITS_OFFSET + nwords * 8 <= d.len() {
                return Inner::Bloom { keys, m, bits_off: BLOOM_BITS_OFFSET };
            }
        }
        // Not a (valid) bloom: a fuse filter or something we don't decode. Safe to
        // treat as match-all — it only ever disables the negative speedup.
        Inner::Unsupported
    }

    /// Which encoding this filter turned out to be.
    pub fn kind(&self) -> FilterKind {
        match self.inner {
            Inner::Empty => FilterKind::Empty,
            Inner::Bloom { .. } => FilterKind::Bloom,
            Inner::Unsupported => FilterKind::Unsupported,
        }
    }

    /// Whether this filter can actually exclude keys (i.e. is a supported bloom). When
    /// `false`, [`contains_hash`](Self::contains_hash) always returns `true`.
    pub fn is_accelerating(&self) -> bool {
        matches!(self.inner, Inner::Bloom { .. })
    }

    #[inline]
    fn bit_word(&self, bits_off: usize, idx: usize) -> u64 {
        let off = bits_off + idx * 8;
        u64::from_le_bytes(self.mmap[off..off + 8].try_into().unwrap())
    }

    /// `ContainsHash`: `false` ⇒ the key is definitely absent. `hash` is the murmur3
    /// `h1` of the key (see [`crate::murmur3_x64_128_h1`]). Always `true` for an empty
    /// or unsupported filter.
    #[inline]
    pub fn contains_hash(&self, mut hash: u64) -> bool {
        let (keys, m, bits_off) = match &self.inner {
            Inner::Bloom { keys, m, bits_off } => (keys, *m, *bits_off),
            Inner::Empty | Inner::Unsupported => return true,
        };
        let mut r = 1u64;
        for &key in keys {
            if r == 0 {
                break;
            }
            hash = hash.rotate_left(17) ^ key;
            let i = hash % m;
            r &= (self.bit_word(bits_off, (i >> 6) as usize) >> (i & 0x3f)) & 1;
        }
        r != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::murmur3_x64_128_h1;

    /// Build a tiny holiman/bloomfilter/v2 `.kvei`, read it back, and confirm membership.
    #[test]
    fn bloom_roundtrip_via_file() {
        let m: u64 = 4096;
        let keys: [u64; 3] = [0x1111_2222_3333_4444, 0xaaaa_bbbb_cccc_dddd, 0xdead_beef_0bad_f00d];
        let nwords = (m as usize).div_ceil(64);
        let mut bits = vec![0u64; nwords];
        // AddHash(h): h = rotl(h,17) ^ key[n]; set bit (h % m).
        let add = |bits: &mut [u64], mut h: u64| {
            for &k in &keys {
                h = h.rotate_left(17) ^ k;
                let i = h % m;
                bits[(i >> 6) as usize] |= 1 << (i & 63);
            }
        };
        // Use real murmur3 hashes of a few keys so the test exercises the full path.
        let present_keys: [&[u8]; 3] = [b"alpha", b"bravo-key", b"0123456789abcdef0123"];
        let present: Vec<u64> = present_keys.iter().map(|k| murmur3_x64_128_h1(k, 9)).collect();
        for &h in &present {
            add(&mut bits, h);
        }
        // Serialize: magic(12) + k,n,m (LE) + keys (LE) + bits (LE) + sha384(48, ignored).
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&BLOOM_MAGIC);
        buf.extend_from_slice(&3u64.to_le_bytes());
        buf.extend_from_slice(&(present.len() as u64).to_le_bytes());
        buf.extend_from_slice(&m.to_le_bytes());
        for k in keys {
            buf.extend_from_slice(&k.to_le_bytes());
        }
        for w in &bits {
            buf.extend_from_slice(&w.to_le_bytes());
        }
        buf.extend_from_slice(&[0u8; 48]);

        let path = std::env::temp_dir().join(format!("erigon_seg_bloom_{}.kvei", std::process::id()));
        std::fs::write(&path, &buf).unwrap();
        let f = ExistenceFilter::open(&path).expect("open bloom");
        let _ = std::fs::remove_file(&path);

        assert_eq!(f.kind(), FilterKind::Bloom);
        assert!(f.is_accelerating());
        for (k, &h) in present_keys.iter().zip(&present) {
            assert!(f.contains_hash(h), "added key {k:?} must be present");
            assert!(f.contains_hash(murmur3_x64_128_h1(k, 9)));
        }
        // A key we didn't add should (almost certainly, with this m) be reported absent.
        assert!(!f.contains_hash(murmur3_x64_128_h1(b"definitely-not-added", 9)));
    }

    #[test]
    fn empty_and_unsupported_match_all() {
        let dir = std::env::temp_dir();
        let empty = dir.join(format!("erigon_seg_empty_{}.kvei", std::process::id()));
        std::fs::write(&empty, []).unwrap();
        let f = ExistenceFilter::open(&empty).unwrap();
        let _ = std::fs::remove_file(&empty);
        assert_eq!(f.kind(), FilterKind::Empty);
        assert!(!f.is_accelerating());
        assert!(f.contains_hash(0xdead_beef)); // match-all

        // A non-bloom blob (looks like a fuse filter) -> Unsupported, still match-all.
        let fuse = dir.join(format!("erigon_seg_fuse_{}.kvei", std::process::id()));
        std::fs::write(&fuse, [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]).unwrap();
        let f = ExistenceFilter::open(&fuse).unwrap();
        let _ = std::fs::remove_file(&fuse);
        assert_eq!(f.kind(), FilterKind::Unsupported);
        assert!(f.contains_hash(123));
    }
}
