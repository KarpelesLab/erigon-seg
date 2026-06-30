//! Index salt handling for the `.kvei` existence filter.
//!
//! Erigon hashes each key as `murmur3.Sum128WithSeed(key, salt)` before adding it to
//! the bloom filter, where `salt` is a per-snapshot 32-bit value (Erigon stores it in
//! `salt-state.txt` / `salt-blocks.txt`). Without the right salt the filter cannot be
//! used, so a reader must learn it — from the file, from the caller, or by brute force.

use std::path::Path;

/// How to obtain the `.kvei` index salt.
#[derive(Debug, Clone, Copy)]
pub enum Salt {
    /// No salt: do not use the bloom filter. Lookups stay correct (exact `.bt` search),
    /// just without the negative-lookup speedup.
    None,
    /// A known salt (e.g. from `salt-state.txt`, big-endian `u32`).
    Known(u32),
    /// Brute-force the salt by requiring a batch of real keys to all hit the bloom,
    /// using `usize` worker threads. The ~1% per-key false-positive rate makes a wrong
    /// salt passing every sampled key astronomically unlikely, so the first salt that
    /// passes is the real one.
    Find(usize),
}

/// Read an Erigon salt file (`salt-state.txt` / `salt-blocks.txt`): a 4-byte big-endian
/// `u32`. Returns `None` if the file is missing or too short.
pub fn salt_from_file(path: impl AsRef<Path>) -> Option<u32> {
    let b = std::fs::read(path).ok()?;
    (b.len() >= 4).then(|| u32::from_be_bytes(b[0..4].try_into().unwrap()))
}
