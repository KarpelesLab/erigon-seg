//! Integration tests against real Erigon `.kv`/`.bt`/`.kvei` files.
//!
//! These are skipped unless `ERIGON_SEG_TEST_DIR` points at a directory containing
//! Erigon domain files (e.g. `v1.1-accounts.*`) and a `salt-state.txt`. Run with:
//!
//! ```text
//! ERIGON_SEG_TEST_DIR=/path/to/kv cargo test --test real_files -- --nocapture
//! ```

use std::path::{Path, PathBuf};

use erigon_seg::{
    BtLayout, BtOptions, BtreeIndex, KvReader, MergeOptions, Salt, Seg, SegWriter, build_bt, merge,
    murmur3_x64_128_h1, salt_from_file,
};

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

/// The `n` smallest `.kv` files in the dir, smallest first.
fn smallest_kvs(dir: &Path, n: usize) -> Vec<PathBuf> {
    let mut v: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "kv"))
        .filter_map(|p| std::fs::metadata(&p).ok().map(|m| (m.len(), p)))
        .collect();
    v.sort_by_key(|(len, _)| *len);
    v.into_iter().take(n).map(|(_, p)| p).collect()
}

/// Re-encode a real `.kv` through SegWriter and confirm every word survives byte-exact.
#[test]
fn rewrites_real_kv_roundtrip() {
    let Some(dir) = test_dir() else {
        eprintln!("ERIGON_SEG_TEST_DIR not set; skipping");
        return;
    };
    let Some(kv) = smallest_kv(&dir) else { return };
    eprintln!("re-encoding {}", kv.display());

    let seg = Seg::open(&kv).expect("open real .kv");
    let out = std::env::temp_dir().join(format!("erigon_seg_recode_{}.kv", std::process::id()));
    let mut w = SegWriter::create(&out).unwrap();
    let mut g = seg.getter();
    while g.has_next() {
        let word = g.next();
        w.add_word(&word).unwrap();
    }
    w.finish().unwrap();

    let seg2 = Seg::open(&out).expect("open rewritten .kv");
    assert_eq!(seg2.words_count(), seg.words_count());
    let (mut a, mut b) = (seg.getter(), seg2.getter());
    let mut n = 0u64;
    while a.has_next() {
        assert!(b.has_next(), "rewritten ran short at word {n}");
        assert_eq!(a.next(), b.next(), "word {n} differs after re-encode");
        n += 1;
    }
    assert!(!b.has_next(), "rewritten has extra words");
    let _ = std::fs::remove_file(&out);
}

/// Re-encode a real `.kv` *with pattern compression* and confirm byte-exact round-trip;
/// report size vs erigon's own compressed file.
#[test]
fn recompresses_real_kv_roundtrip() {
    let Some(dir) = test_dir() else {
        eprintln!("ERIGON_SEG_TEST_DIR not set; skipping");
        return;
    };
    let Some(kv) = smallest_kv(&dir) else { return };
    eprintln!("recompressing {}", kv.display());

    let seg = Seg::open(&kv).expect("open real .kv");
    let out = std::env::temp_dir().join(format!("erigon_seg_recomp_{}.kv", std::process::id()));
    let mut w = SegWriter::create_with(&out, true).unwrap();
    let mut g = seg.getter();
    while g.has_next() {
        let word = g.next();
        w.add_word(&word).unwrap();
    }
    w.finish().unwrap();

    let seg2 = Seg::open(&out).expect("open recompressed .kv");
    assert_eq!(seg2.words_count(), seg.words_count());
    let (mut a, mut b) = (seg.getter(), seg2.getter());
    let mut n = 0u64;
    while a.has_next() {
        assert!(b.has_next(), "short at {n}");
        assert_eq!(a.next(), b.next(), "word {n} differs after recompress");
        n += 1;
    }
    assert!(!b.has_next());

    let real_sz = std::fs::metadata(&kv).unwrap().len();
    let ours_sz = std::fs::metadata(&out).unwrap().len();
    eprintln!(
        "recompressed OK: {n} words; ours {ours_sz} vs erigon {real_sz} ({:.1}% of erigon)",
        100.0 * ours_sz as f64 / real_sz as f64
    );
    let _ = std::fs::remove_file(&out);
}

/// Merge two real `.kv` files and verify newest-wins + union against the source readers.
#[test]
fn merges_real_files() {
    let Some(dir) = test_dir() else {
        eprintln!("ERIGON_SEG_TEST_DIR not set; skipping");
        return;
    };
    let kvs = smallest_kvs(&dir, 2);
    if kvs.len() < 2 {
        eprintln!("need >=2 .kv files; skipping");
        return;
    }
    let (older, newer) = (&kvs[0], &kvs[1]);
    eprintln!(
        "merging {} (old) + {} (new)",
        older.display(),
        newer.display()
    );

    // Output range start != 0 so legitimate empty values are preserved.
    let out = std::env::temp_dir().join(format!(
        "erigon_seg_merge_{}.100-200.kv",
        std::process::id()
    ));
    merge(&[older, newer], &out, MergeOptions::default()).expect("merge");

    let r_old = KvReader::open(older).unwrap();
    let r_new = KvReader::open(newer).unwrap();
    let r_out = KvReader::open(&out).unwrap();

    // Sample keys from both inputs; merged value must be newest-wins.
    let mut checked = 0u64;
    for src in [&r_new, &r_old] {
        for kv in src.iter().step_by(617).take(400) {
            let (k, _) = kv.unwrap();
            let expect = r_new.get(&k).unwrap().or(r_old.get(&k).unwrap());
            assert_eq!(
                r_out.get(&k).unwrap(),
                expect,
                "newest-wins mismatch for a key"
            );
            checked += 1;
        }
    }
    // Merged key set is the union (these inputs have no deletions dropped at from!=0).
    let mut union: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
    for src in [&r_old, &r_new] {
        for kv in src.iter() {
            union.insert(kv.unwrap().0);
        }
    }
    assert_eq!(
        r_out.key_count(),
        union.len() as u64,
        "merged key_count != union size"
    );
    eprintln!(
        "merged {} keys, checked {checked} lookups",
        r_out.key_count()
    );

    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(out.with_extension("bt"));
}

/// Rebuild a `.bt` from a real `.kv` and confirm every offset matches the real `.bt`.
#[test]
fn rebuilt_bt_matches_real() {
    let Some(dir) = test_dir() else {
        eprintln!("ERIGON_SEG_TEST_DIR not set; skipping");
        return;
    };
    let Some(kv) = smallest_kv(&dir) else { return };
    let real_bt = kv.with_extension("bt");
    if !real_bt.exists() {
        eprintln!("no sibling .bt for {}; skipping", kv.display());
        return;
    }
    eprintln!("rebuilding .bt for {}", kv.display());

    let real = BtreeIndex::open(&real_bt).expect("open real .bt");
    let out = std::env::temp_dir().join(format!("erigon_seg_rebuilt_{}.bt", std::process::id()));

    // Real files use the legacy layout; rebuild legacy and compare offsets 1:1.
    build_bt(
        &kv,
        &out,
        BtOptions {
            layout: BtLayout::Legacy,
            ..Default::default()
        },
    )
    .unwrap();
    let rebuilt = BtreeIndex::open(&out).expect("open rebuilt .bt");
    let _ = std::fs::remove_file(&out);

    assert_eq!(rebuilt.key_count(), real.key_count(), "key_count");
    let n = real.key_count();
    // Compare a dense prefix and a strided sweep across the whole file.
    for i in (0..n.min(5000)).chain((0..n).step_by((n / 1000).max(1) as usize)) {
        assert_eq!(
            rebuilt.key_offset(i),
            real.key_offset(i),
            "offset[{i}] differs"
        );
    }

    // The rebuilt index must also resolve real keys through a fresh reader.
    let seg = Seg::open(&kv).unwrap();
    let mut g = seg.getter();
    for i in (0..n).step_by((n / 200).max(1) as usize) {
        let real_key = {
            g.reset(real.key_offset(i).unwrap());
            g.next()
        };
        g.reset(rebuilt.key_offset(i).unwrap());
        assert_eq!(g.next(), real_key, "key at di={i}");
    }
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
    assert!(
        r.get(b"\xff_erigon_seg_absent_\xff")
            .expect("get")
            .is_none()
    );

    // Bloom: resolve the salt and verify it accelerates without false negatives.
    if r.existence_filter()
        .map(|f| f.is_accelerating())
        .unwrap_or(false)
    {
        let salt_file = salt_from_file(dir.join("salt-state.txt"));
        let found = r.find_salt(8);
        assert!(
            found.is_some(),
            "find_salt should recover a salt for a real bloom"
        );
        if let (Some(f), Some(s)) = (found, salt_file) {
            assert_eq!(f, s, "brute-forced salt disagrees with salt-state.txt");
        }

        let chosen = salt_file.map(Salt::Known).unwrap_or(Salt::Find(8));
        assert!(r.enable_bloom(chosen), "enable_bloom should validate");
        let salt = r.salt().expect("active salt");

        // No false negatives: every real key must be reported present.
        let filter = r.existence_filter().unwrap();
        for (k, _) in pairs.iter().step_by(50) {
            assert!(
                filter.contains_hash(murmur3_x64_128_h1(k, salt)),
                "bloom false-negative"
            );
        }

        // Lookups remain correct with the bloom enabled.
        for (k, v) in pairs.iter().step_by(97) {
            assert_eq!(r.get(k).expect("get").as_deref(), Some(v.as_slice()));
        }
    }
}
