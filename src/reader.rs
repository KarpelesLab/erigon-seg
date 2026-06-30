//! High-level reader over a `.kv` + sibling `.bt` + `.kvei` triple.
//!
//! [`KvReader`] is the main entry point: it opens the data file and, when present, its
//! B-tree index and existence filter, then answers point lookups ([`get`](KvReader::get))
//! and sequential scans ([`iter`](KvReader::iter)).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::bloom::ExistenceFilter;
use crate::btree::BtreeIndex;
use crate::error::Result;
use crate::hash::murmur3_x64_128_h1;
use crate::salt::Salt;
use crate::seg::{Getter, OpenOptions, Seg};

/// A reader over one seg file set (`.kv` data, optional `.bt` index, optional `.kvei`
/// existence filter).
pub struct KvReader {
    seg: Seg,
    index: Option<BtreeIndex>,
    bloom: Option<ExistenceFilter>,
    /// Active salt: `Some` only once a bloom has been validated via [`enable_bloom`].
    salt: Option<u32>,
    /// The `.kv` file's base name (e.g. `v1.1-accounts.0-1024.kv`), for display.
    name: String,
}

impl KvReader {
    /// Open a `.kv` file and any sibling `.bt` / `.kvei` files found next to it (same
    /// base name). The existence filter is loaded but not used for lookups until a salt
    /// is supplied via [`enable_bloom`](KvReader::enable_bloom).
    pub fn open(kv_path: impl AsRef<Path>) -> Result<KvReader> {
        KvReader::open_with(kv_path, OpenOptions::default())
    }

    /// Like [`open`](KvReader::open) but with explicit seg [`OpenOptions`] (e.g. for
    /// files carrying out-of-band metadata).
    pub fn open_with(kv_path: impl AsRef<Path>, opts: OpenOptions) -> Result<KvReader> {
        let kv_path = kv_path.as_ref();
        let seg = Seg::open_with(kv_path, opts)?;

        let bt_path = kv_path.with_extension("bt");
        let index = if bt_path.exists() {
            Some(BtreeIndex::open(&bt_path)?)
        } else {
            None
        };

        let kvei_path = kv_path.with_extension("kvei");
        let bloom = if kvei_path.exists() {
            Some(ExistenceFilter::open(&kvei_path)?)
        } else {
            None
        };

        let name = kv_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        Ok(KvReader {
            seg,
            index,
            bloom,
            salt: None,
            name,
        })
    }

    /// The `.kv` file's base name (e.g. `v1.1-accounts.0-1024.kv`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the bloom filter is active for lookups — i.e. a `.kvei` is present and a
    /// salt has been validated against real keys via [`enable_bloom`](KvReader::enable_bloom).
    pub fn bloom_active(&self) -> bool {
        self.salt.is_some()
    }

    /// The underlying seg data file.
    pub fn seg(&self) -> &Seg {
        &self.seg
    }

    /// The B-tree index, if a `.bt` was found.
    pub fn index(&self) -> Option<&BtreeIndex> {
        self.index.as_ref()
    }

    /// The existence filter, if a `.kvei` was found.
    pub fn existence_filter(&self) -> Option<&ExistenceFilter> {
        self.bloom.as_ref()
    }

    /// The active bloom salt, if [`enable_bloom`](KvReader::enable_bloom) has succeeded.
    pub fn salt(&self) -> Option<u32> {
        self.salt
    }

    /// Number of keys: from the `.bt` index if present, otherwise inferred as
    /// `words_count / 2` (domain files store alternating key/value words).
    pub fn key_count(&self) -> u64 {
        match &self.index {
            Some(idx) => idx.key_count(),
            None => self.seg.words_count() / 2,
        }
    }

    /// Enable the `.kvei` bloom as a negative-lookup accelerator, resolving the salt per
    /// [`Salt`]. Returns `true` only if a usable bloom is present and the resolved salt
    /// self-validates against real keys (so a wrong salt can never cause a missed key —
    /// it just leaves lookups unaccelerated).
    pub fn enable_bloom(&mut self, salt: Salt) -> bool {
        let Some(bloom) = &self.bloom else {
            return false;
        };
        if !bloom.is_accelerating() {
            return false;
        }
        let resolved = match salt {
            Salt::None => return false,
            Salt::Known(s) => s,
            Salt::Find(threads) => match self.find_salt(threads) {
                Some(s) => s,
                None => return false,
            },
        };
        let samples = self.sample_keys(64);
        if samples.is_empty() {
            return false;
        }
        let ok = samples
            .iter()
            .all(|k| bloom.contains_hash(murmur3_x64_128_h1(k, resolved)));
        if ok {
            self.salt = Some(resolved);
        }
        ok
    }

    /// Brute-force the bloom salt by requiring a batch of real keys to all hit the
    /// filter, using `threads` workers. Returns `None` if no `.kvei` bloom is usable or
    /// no salt validates (e.g. a fuse-filter or format mismatch).
    pub fn find_salt(&self, threads: usize) -> Option<u32> {
        let bloom = self.bloom.as_ref()?;
        if !bloom.is_accelerating() {
            return None;
        }
        let samples = self.sample_keys(16);
        if samples.is_empty() {
            return None;
        }
        let threads = threads.clamp(1, 256) as u32;
        let found = AtomicU64::new(u64::MAX);
        std::thread::scope(|sc| {
            for t in 0..threads {
                let (found, bloom, samples) = (&found, bloom, &samples);
                sc.spawn(move || {
                    let mut salt = t;
                    loop {
                        if found.load(Ordering::Relaxed) != u64::MAX {
                            return;
                        }
                        if samples
                            .iter()
                            .all(|k| bloom.contains_hash(murmur3_x64_128_h1(k, salt)))
                        {
                            found.fetch_min(salt as u64, Ordering::Relaxed);
                            return;
                        }
                        match salt.checked_add(threads) {
                            Some(s) => salt = s,
                            None => return,
                        }
                    }
                });
            }
        });
        let f = found.load(Ordering::Relaxed);
        (f != u64::MAX).then_some(f as u32)
    }

    /// Look up `key`, returning its value if present.
    ///
    /// Uses, in order: the bloom filter for a fast definite-absent answer (if enabled),
    /// then the `.bt` index for an `O(log n)` binary search, or — if there is no index —
    /// an ordered linear scan.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        // Fast negative.
        if let Some(salt) = self.salt
            && let Some(bloom) = &self.bloom
            && !bloom.contains_hash(murmur3_x64_128_h1(key, salt))
        {
            return Ok(None);
        }
        match &self.index {
            Some(idx) => Ok(self.get_indexed(idx, key)),
            None => Ok(self.get_scan(key)),
        }
    }

    fn get_indexed(&self, idx: &BtreeIndex, key: &[u8]) -> Option<Vec<u8>> {
        let n = idx.key_count();
        if n == 0 {
            return None;
        }
        let mut g = self.seg.getter();
        let (mut lo, mut hi) = (0u64, n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let off = idx.key_offset(mid)?;
            g.reset(off);
            if !g.has_next() {
                return None;
            }
            let probe = g.next();
            match probe.as_slice().cmp(key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    return Some(if g.has_next() { g.next() } else { Vec::new() });
                }
            }
        }
        None
    }

    fn get_scan(&self, key: &[u8]) -> Option<Vec<u8>> {
        let mut g = self.seg.getter();
        while g.has_next() {
            let k = g.next();
            let v = if g.has_next() { g.next() } else { Vec::new() };
            match k.as_slice().cmp(key) {
                std::cmp::Ordering::Less => continue,
                std::cmp::Ordering::Greater => return None, // keys are sorted
                std::cmp::Ordering::Equal => return Some(v),
            }
        }
        None
    }

    /// Sample up to `n` real keys spread across the file, for salt validation / search.
    /// Each returned key is genuinely present, so a correct bloom must contain it.
    fn sample_keys(&self, n: usize) -> Vec<Vec<u8>> {
        match &self.index {
            Some(idx) => {
                let count = idx.key_count();
                if count == 0 {
                    return Vec::new();
                }
                let n = (n as u64).min(count);
                let mut g = self.seg.getter();
                (0..n)
                    .filter_map(|s| {
                        let di = s * count / n;
                        idx.key_offset(di).map(|off| {
                            g.reset(off);
                            g.next()
                        })
                    })
                    .collect()
            }
            None => {
                // No index: take the first `n` keys by scanning.
                let mut g = self.seg.getter();
                let mut out = Vec::new();
                while out.len() < n && g.has_next() {
                    out.push(g.next());
                    if g.has_next() {
                        g.next(); // skip value
                    }
                }
                out
            }
        }
    }

    /// Iterate every `(key, value)` pair sequentially, in stored (key) order.
    pub fn iter(&self) -> KvIter<'_> {
        KvIter {
            getter: self.seg.getter(),
        }
    }
}

/// Iterator over the `(key, value)` pairs of a [`KvReader`], in stored order.
pub struct KvIter<'a> {
    getter: Getter<'a>,
}

impl Iterator for KvIter<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.getter.has_next() {
            return None;
        }
        let key = self.getter.next();
        let value = if self.getter.has_next() {
            self.getter.next()
        } else {
            Vec::new()
        };
        Some(Ok((key, value)))
    }
}
