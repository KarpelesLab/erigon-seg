//! Writer for `.kvei` existence filters (the `holiman/bloomfilter/v2` layout).
//!
//! Reproduces the bloom files erigon historically wrote (and still reads): a `k=3`
//! filter sized `m = ceil(-n·ln(0.01)/ln(2)²)` bits, with the rotate-17 + per-key-XOR
//! hash schedule that [`crate::ExistenceFilter`] decodes. Keys are hashed with
//! [`murmur3_x64_128_h1`](crate::murmur3_x64_128_h1) under a caller-supplied salt.
//!
//! For `< 2` keys erigon writes a zero-byte (empty, match-all) filter; we do the same.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};
use crate::hash::murmur3_x64_128_h1;

/// holiman/bloomfilter/v2 header magic: 8 zero bytes followed by `v02\n`.
const BLOOM_MAGIC: [u8; 12] = [0, 0, 0, 0, 0, 0, 0, 0, b'v', b'0', b'2', b'\n'];
/// Fixed number of hash keys (matches the filters erigon produced).
const K: u64 = 3;
/// Target false-positive rate used to size the bit array (`OptimalM(n, 0.01)`).
const P: f64 = 0.01;

/// `bloomfilter.OptimalM(n, p)`: bit count `ceil(-n·ln(p)/ln(2)²)`.
fn optimal_m(n: u64) -> u64 {
    (-(n as f64) * P.ln() / (std::f64::consts::LN_2 * std::f64::consts::LN_2)).ceil() as u64
}

/// splitmix64, to derive the filter's per-key XOR salts deterministically.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Accumulates set bits for a `.kvei` bloom filter.
pub struct KveiBuilder {
    keys: [u64; 3],
    m: u64,
    n: u64,
    bits: Vec<u64>,
    empty: bool,
}

impl KveiBuilder {
    /// Create a builder sized for `key_count` keys. With `< 2` keys the filter is empty
    /// (match-all) and serializes to a zero-byte file.
    pub fn new(key_count: u64) -> KveiBuilder {
        if key_count < 2 {
            return KveiBuilder {
                keys: [0; 3],
                m: 0,
                n: key_count,
                bits: Vec::new(),
                empty: true,
            };
        }
        let m = optimal_m(key_count).max(2);
        // Distinct, well-mixed XOR salts; their values only need to be reproducible, not
        // secret — every added key is recorded, so there are never false negatives.
        let mut seed = 0x1234_5678_9ABC_DEF0u64 ^ key_count;
        let keys = [
            splitmix64(&mut seed),
            splitmix64(&mut seed),
            splitmix64(&mut seed),
        ];
        let nwords = m.div_ceil(64) as usize;
        KveiBuilder {
            keys,
            m,
            n: key_count,
            bits: vec![0u64; nwords],
            empty: false,
        }
    }

    /// Add a pre-hashed key (murmur3 `h1`).
    pub fn add_hash(&mut self, mut hash: u64) {
        if self.empty {
            return;
        }
        for &key in &self.keys {
            hash = hash.rotate_left(17) ^ key;
            let i = hash % self.m;
            self.bits[(i >> 6) as usize] |= 1u64 << (i & 63);
        }
    }

    /// Add a key, hashing it with the given salt.
    pub fn add_key(&mut self, key: &[u8], salt: u32) {
        self.add_hash(murmur3_x64_128_h1(key, salt));
    }

    /// Serialize the filter to `path`.
    pub fn finish(self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let mut f = File::create(path).map_err(|e| Error::io(path, e))?;
        if self.empty {
            return f.flush().map_err(|e| Error::io(path, e)); // zero-byte file
        }
        let mut out = Vec::with_capacity(60 + self.bits.len() * 8 + 48);
        out.extend_from_slice(&BLOOM_MAGIC);
        out.extend_from_slice(&K.to_le_bytes());
        out.extend_from_slice(&self.n.to_le_bytes());
        out.extend_from_slice(&self.m.to_le_bytes());
        for k in self.keys {
            out.extend_from_slice(&k.to_le_bytes());
        }
        for w in &self.bits {
            out.extend_from_slice(&w.to_le_bytes());
        }
        // 48-byte trailer (sha384 slot). erigon reads with verification disabled and this
        // crate's reader ignores it, so zeros are sufficient and interoperable.
        out.extend_from_slice(&[0u8; 48]);
        f.write_all(&out).map_err(|e| Error::io(path, e))?;
        f.flush().map_err(|e| Error::io(path, e))
    }
}

/// Build a `.kvei` for every key in a `.kv`, hashing with `salt`.
pub fn build_kvei_from_seg(
    seg: &crate::seg::Seg,
    salt: u32,
    out_path: impl AsRef<Path>,
) -> Result<()> {
    let key_count = seg.words_count() / 2;
    let mut b = KveiBuilder::new(key_count);
    let mut g = seg.getter();
    for _ in 0..key_count {
        let key = g.next();
        g.skip(); // value
        b.add_key(&key, salt);
    }
    b.finish(out_path)
}

#[cfg(test)]
mod tests {
    use super::optimal_m;

    #[test]
    fn optimal_m_matches_real_files() {
        // (key_count, m) pairs read from real erigon .kvei headers.
        assert_eq!(optimal_m(1_079_554), 10_347_589);
        assert_eq!(optimal_m(338_868_457), 3_248_073_943);
    }
}
