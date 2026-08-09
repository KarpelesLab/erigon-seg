//! Tests for `.bt` di-node narrowing: the node array must describe the file exactly,
//! and narrowing a lookup must never change what a lookup returns.
//!
//! These build their own domain files, so unlike `real_files.rs` they always run.

use std::path::{Path, PathBuf};

use erigon_seg::{
    BtLayout, BtOptions, DEFAULT_BTREE_M, DomainOptions, DomainWriter, KvReader, Seg,
};

/// A scratch path unique to this process and `tag`.
fn scratch(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("erigon_seg_narrow_{}_{tag}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p.join("v1.1-test.0-1.kv")
}

fn cleanup(kv: &Path) {
    for ext in ["kv", "bt", "kvei"] {
        let _ = std::fs::remove_file(kv.with_extension(ext));
    }
    if let Some(d) = kv.parent() {
        let _ = std::fs::remove_dir(d);
    }
}

/// A domain's `(key, value)` pairs, in the order they were written.
type Pairs = Vec<(Vec<u8>, Vec<u8>)>;

/// Build a domain of `n` keys. `variable` makes key length vary, exercising the
/// non-uniform node path; otherwise every key is 20 bytes (the uniform fast path).
fn build(tag: &str, n: u32, variable: bool, layout: BtLayout) -> (PathBuf, Pairs) {
    let kv = scratch(tag);
    let mut pairs: Pairs = Vec::with_capacity(n as usize);
    for i in 0..n {
        // Big-endian counter in a fixed prefix keeps the keys strictly increasing even
        // when the length varies.
        let mut k = vec![0u8; 20];
        k[..4].copy_from_slice(&i.to_be_bytes());
        if variable {
            k.truncate(8 + (i % 12) as usize);
        }
        let v = format!("value-{i}").into_bytes();
        pairs.push((k, v));
    }
    let mut w = DomainWriter::create(
        &kv,
        DomainOptions {
            bt: BtOptions {
                layout,
                m: DEFAULT_BTREE_M,
            },
            salt: None,
            compress: true,
        },
    )
    .unwrap();
    for (k, v) in &pairs {
        w.add(k, v).unwrap();
    }
    w.finish().unwrap();
    (kv, pairs)
}

#[test]
fn nodes_describe_the_file() {
    for (tag, variable) in [("uniform", false), ("variable", true)] {
        let n = 3000u32;
        let (kv, pairs) = build(tag, n, variable, BtLayout::Footer);
        let r = KvReader::open(&kv).unwrap();
        let seg = Seg::open(&kv).unwrap();
        let idx = r.index().unwrap();
        let nodes = idx.nodes().expect("footer layout must expose nodes");
        let m = DEFAULT_BTREE_M;

        assert_eq!(
            nodes.len() as u64,
            (n as u64).div_ceil(m),
            "{tag}: node count"
        );
        assert_eq!(nodes.m(), m, "{tag}: node M");

        // node[j] must be exactly key j*M, both against the index and the raw pairs.
        let mut g = seg.getter();
        for j in 0..nodes.len() {
            let di = j as u64 * m;
            g.reset(idx.key_offset(di).unwrap());
            assert_eq!(nodes.key(j), g.next().as_slice(), "{tag}: node[{j}] vs .kv");
            assert_eq!(
                nodes.key(j),
                pairs[di as usize].0.as_slice(),
                "{tag}: node[{j}] vs source"
            );
        }
        cleanup(&kv);
    }
}

#[test]
fn narrow_brackets_every_key() {
    let n = 3000u32;
    let (kv, pairs) = build("bracket", n, false, BtLayout::Footer);
    let r = KvReader::open(&kv).unwrap();
    let idx = r.index().unwrap();

    for (i, (k, _)) in pairs.iter().enumerate() {
        let (lo, hi) = idx.narrow(k);
        let i = i as u64;
        assert!(
            lo <= i && i < hi,
            "narrow({i}) = ({lo},{hi}) does not contain the key's own index"
        );
        assert!(hi - lo <= DEFAULT_BTREE_M, "narrow({i}) window too wide");
    }
    cleanup(&kv);
}

#[test]
fn lookups_match_across_block_boundaries() {
    // Cover both the uniform and variable-length node paths, and a key count that is not
    // a multiple of M so the final block is short.
    for (tag, variable, n) in [("bnd-u", false, 2001u32), ("bnd-v", true, 2001)] {
        let (kv, pairs) = build(tag, n, variable, BtLayout::Footer);
        let r = KvReader::open(&kv).unwrap();

        // Every key resolves to its own value.
        for (k, v) in &pairs {
            assert_eq!(
                r.get(k).unwrap().as_deref(),
                Some(v.as_slice()),
                "{tag}: key {k:02x?}"
            );
        }

        // Keys immediately around each M-boundary are where an off-by-one in narrowing
        // would show up first.
        let m = DEFAULT_BTREE_M as usize;
        for j in 0..pairs.len().div_ceil(m) {
            for idx in [
                j * m,
                (j * m).saturating_sub(1),
                (j * m + 1).min(pairs.len() - 1),
            ] {
                let (k, v) = &pairs[idx];
                assert_eq!(
                    r.get(k).unwrap().as_deref(),
                    Some(v.as_slice()),
                    "{tag}: boundary key at {idx}"
                );
            }
        }

        // Misses: perturbed keys, a key below the first, and one above the last.
        for (k, _) in pairs.iter().take(500) {
            let mut miss = k.clone();
            let l = miss.len();
            miss[l - 1] ^= 0xff;
            // Only assert when the perturbation really is absent from the set.
            if !pairs.iter().any(|(kk, _)| *kk == miss) {
                assert_eq!(r.get(&miss).unwrap(), None, "{tag}: miss {miss:02x?}");
            }
        }
        assert_eq!(r.get(&[0u8; 4]).unwrap(), None, "{tag}: below first key");
        assert_eq!(r.get(&[0xffu8; 32]).unwrap(), None, "{tag}: above last key");

        cleanup(&kv);
    }
}

#[test]
fn legacy_layout_has_no_nodes_but_still_looks_up() {
    let n = 1500u32;
    let (kv, pairs) = build("legacy", n, false, BtLayout::Legacy);
    let r = KvReader::open(&kv).unwrap();
    let idx = r.index().unwrap();

    assert!(
        idx.nodes().is_none(),
        "legacy layout must not expose a node array"
    );
    // narrow() falls back to the full key range.
    assert_eq!(idx.narrow(&pairs[0].0), (0, n as u64));

    for (k, v) in pairs.iter().step_by(7) {
        assert_eq!(r.get(k).unwrap().as_deref(), Some(v.as_slice()));
    }
    assert_eq!(r.get(&[0xffu8; 32]).unwrap(), None);
    cleanup(&kv);
}

#[test]
fn single_key_and_single_block_files() {
    for n in [1u32, 2, DEFAULT_BTREE_M as u32, DEFAULT_BTREE_M as u32 + 1] {
        let (kv, pairs) = build(&format!("small{n}"), n, false, BtLayout::Footer);
        let r = KvReader::open(&kv).unwrap();
        let nodes = r.index().unwrap().nodes().unwrap();
        assert_eq!(nodes.len() as u64, (n as u64).div_ceil(DEFAULT_BTREE_M));

        for (k, v) in &pairs {
            assert_eq!(r.get(k).unwrap().as_deref(), Some(v.as_slice()), "n={n}");
        }
        assert_eq!(r.get(&[0u8; 4]).unwrap(), None, "n={n}: below first");
        assert_eq!(r.get(&[0xffu8; 32]).unwrap(), None, "n={n}: above last");
        cleanup(&kv);
    }
}
