//! Integration tests against real Erigon `.kv`/`.bt`/`.kvei` files.
//!
//! These are skipped unless `ERIGON_SEG_TEST_DIR` points at a directory containing
//! Erigon domain files (e.g. `v1.1-accounts.*`) and a `salt-state.txt`. Run with:
//!
//! ```text
//! ERIGON_SEG_TEST_DIR=/path/to/kv cargo test --test real_files -- --nocapture
//! ```

use std::path::{Path, PathBuf};

use erigon_seg::{KvReader, Salt, murmur3_x64_128_h1, salt_from_file};

/// Return the test dir from the env, or `None` to skip.
fn test_dir() -> Option<PathBuf> {
    let d = std::env::var_os("ERIGON_SEG_TEST_DIR")?;
    let p = PathBuf::from(d);
    p.is_dir().then_some(p)
}

/// Pick the smallest `*.kv` in the dir, so the test stays fast on large snapshots.
fn smallest_kv(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "kv"))
        .filter_map(|p| std::fs::metadata(&p).ok().map(|m| (m.len(), p)))
        .min_by_key(|(len, _)| *len)
        .map(|(_, p)| p)
}

#[test]
fn reads_and_queries_real_file() {
    let Some(dir) = test_dir() else {
        eprintln!("ERIGON_SEG_TEST_DIR not set; skipping real-file test");
        return;
    };
    let Some(kv) = smallest_kv(&dir) else {
        eprintln!("no .kv files in {}; skipping", dir.display());
        return;
    };
    eprintln!("testing {}", kv.display());

    let mut r = KvReader::open(&kv).expect("open .kv");
    let n = r.key_count();
    assert!(n > 0, "expected a non-empty domain file");
    if let Some(idx) = r.index() {
        assert_eq!(idx.key_count(), n, "bt key_count vs reader key_count");
    }

    // Collect a spread of real keys and their values via sequential iteration.
    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for kv in r.iter().take(2000) {
        pairs.push(kv.expect("iter"));
    }
    assert!(!pairs.is_empty());
    // Keys must be strictly increasing (sorted, unique).
    for w in pairs.windows(2) {
        assert!(w[0].0 < w[1].0, "keys not sorted/unique");
    }

    // Every sampled key must be found and its value must round-trip exactly.
    for (k, v) in pairs.iter().step_by(97) {
        let got = r.get(k).expect("get").expect("present key missing");
        assert_eq!(&got, v, "value mismatch for a real key");
    }

    // A synthetic key that cannot exist must be absent.
    assert!(r.get(b"\xff_erigon_seg_absent_\xff").expect("get").is_none());

    // Bloom: resolve the salt and verify it accelerates without false negatives.
    if r.existence_filter().map(|f| f.is_accelerating()).unwrap_or(false) {
        let salt_file = salt_from_file(dir.join("salt-state.txt"));
        let found = r.find_salt(8);
        assert!(found.is_some(), "find_salt should recover a salt for a real bloom");
        if let (Some(f), Some(s)) = (found, salt_file) {
            assert_eq!(f, s, "brute-forced salt disagrees with salt-state.txt");
        }

        let chosen = salt_file.map(Salt::Known).unwrap_or(Salt::Find(8));
        assert!(r.enable_bloom(chosen), "enable_bloom should validate");
        let salt = r.salt().expect("active salt");

        // No false negatives: every real key must be reported present.
        let filter = r.existence_filter().unwrap();
        for (k, _) in pairs.iter().step_by(50) {
            assert!(filter.contains_hash(murmur3_x64_128_h1(k, salt)), "bloom false-negative");
        }

        // Lookups remain correct with the bloom enabled.
        for (k, v) in pairs.iter().step_by(97) {
            assert_eq!(r.get(k).expect("get").as_deref(), Some(v.as_slice()));
        }
    }
}
