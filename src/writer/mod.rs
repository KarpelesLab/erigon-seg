//! Writing and merging seg file sets.
//!
//! Built from shared primitives ([`bitwriter`], [`huffman`], [`ef_builder`]) up through
//! the seg `.kv` writer, the `.bt` and `.kvei` builders, the combined [`DomainWriter`],
//! and [`merge`]. See `ROADMAP.md` for the staged design.

mod bitwriter;
mod bt_writer;
mod compress;
mod domain;
mod ef_builder;
mod huffman;
mod kvei_writer;
mod merge;
mod seg_writer;

pub use bt_writer::{BtLayout, BtOptions, DEFAULT_BTREE_M, build_bt, build_bt_from_seg};
pub use domain::{DomainOptions, DomainPaths, DomainWriter};
pub use kvei_writer::{KveiBuilder, build_kvei_from_seg};
pub use merge::{MergeOptions, merge};
pub use seg_writer::SegWriter;

#[cfg(test)]
mod tests {
    use super::ef_builder::EfBuilder;
    use super::seg_writer::SegWriter;
    use crate::Seg;
    use crate::eliasfano::EliasFano;
    use crate::util::mmap_file;

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("erigon_seg_{}_{}", std::process::id(), name))
    }

    /// Write `words` via SegWriter, read them back via Seg, and assert equality.
    fn seg_roundtrip(words: &[Vec<u8>]) {
        let path = scratch("segw.kv");
        let mut w = SegWriter::create(&path).unwrap();
        for word in words {
            w.add_word(word).unwrap();
        }
        w.finish().unwrap();

        let seg = Seg::open(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(seg.version(), 0);
        assert_eq!(seg.words_count(), words.len() as u64);
        let mut g = seg.getter();
        for (i, want) in words.iter().enumerate() {
            assert!(g.has_next(), "ran out of words at {i}");
            assert_eq!(&g.next(), want, "word {i} mismatch");
        }
        assert!(!g.has_next(), "extra words after {}", words.len());
    }

    fn write_kv(path: &std::path::Path, pairs: &[(Vec<u8>, Vec<u8>)]) {
        let mut w = SegWriter::create(path).unwrap();
        for (k, v) in pairs {
            w.add_word(k).unwrap();
            w.add_word(v).unwrap();
        }
        w.finish().unwrap();
    }

    fn bt_check(layout: super::BtLayout) {
        use crate::{BtreeIndex, KvReader};
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..1000u32)
            .map(|i| (format!("key{i:08}").into_bytes(), format!("val-{i}").into_bytes()))
            .collect();
        // Keys are already sorted lexicographically by the zero-padded format.
        let base = scratch(&format!("bt_{layout:?}"));
        let kv = base.with_extension("kv");
        let bt = base.with_extension("bt");
        write_kv(&kv, &pairs);
        super::build_bt(&kv, &bt, super::BtOptions { layout, m: 32 }).unwrap();

        // Direct index check: each key_offset(i) decompresses to key[i].
        let seg = Seg::open(&kv).unwrap();
        let idx = BtreeIndex::open(&bt).unwrap();
        assert_eq!(idx.key_count(), pairs.len() as u64);
        let mut g = seg.getter();
        for (i, (k, _)) in pairs.iter().enumerate() {
            g.reset(idx.key_offset(i as u64).unwrap());
            assert_eq!(&g.next(), k, "key_offset({i}) mismatch ({layout:?})");
        }
        if layout == super::BtLayout::Footer {
            assert_eq!(idx.m(), Some(32));
        }

        // End-to-end through KvReader.get on every key + a few misses.
        let r = KvReader::open(&kv).unwrap();
        for (k, v) in &pairs {
            assert_eq!(r.get(k).unwrap().as_deref(), Some(v.as_slice()), "get {k:?} ({layout:?})");
        }
        assert!(r.get(b"key99999999").unwrap().is_none());
        assert!(r.get(b"aaa").unwrap().is_none());
        let _ = std::fs::remove_file(&kv);
        let _ = std::fs::remove_file(&bt);
    }

    #[test]
    fn bt_writer_both_layouts() {
        bt_check(super::BtLayout::Legacy);
        bt_check(super::BtLayout::Footer);
    }

    #[test]
    fn kvei_writer_roundtrips_and_accelerates() {
        use crate::{ExistenceFilter, KvReader, Salt, murmur3_x64_128_h1};
        let salt = 12_345u32; // small so the find_salt brute-force below stays fast
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..5000u32)
            .map(|i| (format!("addr{i:010}").into_bytes(), vec![(i % 255) as u8; 12]))
            .collect();
        let base = scratch("kvei");
        let kv = base.with_extension("kv");
        let bt = base.with_extension("bt");
        let kvei = base.with_extension("kvei");
        write_kv(&kv, &pairs);
        super::build_bt(&kv, &bt, super::BtOptions::default()).unwrap();
        let seg = Seg::open(&kv).unwrap();
        super::build_kvei_from_seg(&seg, salt, &kvei).unwrap();

        // No false negatives: every real key must be reported present.
        let f = ExistenceFilter::open(&kvei).unwrap();
        assert!(f.is_accelerating());
        for (k, _) in &pairs {
            assert!(f.contains_hash(murmur3_x64_128_h1(k, salt)), "false negative for {k:?}");
        }

        // End-to-end: KvReader enables the bloom (self-validates) and lookups stay correct.
        let mut r = KvReader::open(&kv).unwrap();
        assert!(r.enable_bloom(Salt::Known(salt)));
        assert_eq!(r.salt(), Some(salt));
        for (k, v) in pairs.iter().step_by(53) {
            assert_eq!(r.get(k).unwrap().as_deref(), Some(v.as_slice()));
        }
        assert!(r.get(b"addr9999999999").unwrap().is_none());

        // find_salt should also recover our salt from the generated filter.
        assert_eq!(r.find_salt(8), Some(salt));

        for p in [&kv, &bt, &kvei] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn domain_writer_full_triple() {
        use crate::{DomainOptions, DomainWriter, KvReader, Salt};
        let salt = 777u32;
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..3000u32)
            .map(|i| (format!("k{i:09}").into_bytes(), format!("v{i}").into_bytes()))
            .collect();
        let kv = scratch("domain").with_extension("kv");
        let mut w = DomainWriter::create(&kv, DomainOptions { salt: Some(salt), ..Default::default() }).unwrap();
        for (k, v) in &pairs {
            w.add(k, v).unwrap();
        }
        let paths = w.finish().unwrap();
        assert!(paths.bt.exists() && paths.kvei.as_ref().unwrap().exists());

        let mut r = KvReader::open(&kv).unwrap();
        assert_eq!(r.key_count(), pairs.len() as u64);
        assert!(r.enable_bloom(Salt::Known(salt)));
        for (k, v) in &pairs {
            assert_eq!(r.get(k).unwrap().as_deref(), Some(v.as_slice()));
        }
        assert!(r.get(b"k999999999").unwrap().is_none());

        // Out-of-order keys are rejected.
        let kv2 = scratch("domain2").with_extension("kv");
        let mut w2 = DomainWriter::create(&kv2, DomainOptions::default()).unwrap();
        w2.add(b"b", b"1").unwrap();
        assert!(w2.add(b"a", b"2").is_err());
        assert!(w2.add(b"b", b"2").is_err()); // duplicate also rejected
        drop(w2);

        for p in [&paths.kv, &paths.bt, paths.kvei.as_ref().unwrap(), &kv2] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn merge_newest_wins_and_deletes() {
        use crate::{DomainOptions, DomainWriter, KvReader, MergeOptions, merge};

        // Build two inputs. older has keys a,b,c,e; newer overrides b, deletes c (empty),
        // adds d. Expected union with newest-wins: a(old), b(new), c(empty), d(new), e(old).
        let dir = std::env::temp_dir();
        let mk = |name: &str, pairs: &[(&str, &[u8])]| {
            let p = dir.join(format!("erigon_seg_{}_{name}", std::process::id()));
            let mut w = DomainWriter::create(&p, DomainOptions::default()).unwrap();
            for (k, v) in pairs {
                w.add(k.as_bytes(), v).unwrap();
            }
            w.finish().unwrap();
            p
        };
        let older = mk("m.0-1.kv", &[("a", b"a0"), ("b", b"b0"), ("c", b"c0"), ("e", b"e0")]);
        let newer = mk("m.1-2.kv", &[("b", b"b1"), ("c", b""), ("d", b"d1")]);

        // (1) Non-zero range (out 0-... actually parse range_from explicitly = 1): keep empties.
        let out_keep = dir.join(format!("erigon_seg_{}_out.1-2.kv", std::process::id()));
        merge(&[&older, &newer], &out_keep, MergeOptions::default()).unwrap();
        let r = KvReader::open(&out_keep).unwrap();
        assert_eq!(r.key_count(), 5);
        let got: std::collections::BTreeMap<Vec<u8>, Vec<u8>> =
            r.iter().map(|kv| kv.unwrap()).collect();
        assert_eq!(got[b"a".as_slice()], b"a0"); // only in older
        assert_eq!(got[b"b".as_slice()], b"b1"); // newer overrides
        assert_eq!(got[b"c".as_slice()], b""); // empty kept (range_from != 0)
        assert_eq!(got[b"d".as_slice()], b"d1");
        assert_eq!(got[b"e".as_slice()], b"e0");

        // (2) From-zero range: the empty value for c is a deletion and is dropped.
        let out_del = dir.join(format!("erigon_seg_{}_out.0-2.kv", std::process::id()));
        merge(&[&older, &newer], &out_del, MergeOptions::default()).unwrap();
        let r2 = KvReader::open(&out_del).unwrap();
        assert_eq!(r2.key_count(), 4, "c should be dropped at from=0");
        assert!(r2.get(b"c").unwrap().is_none());
        assert_eq!(r2.get(b"b").unwrap().as_deref(), Some(b"b1".as_slice()));

        for stem in ["m.0-1", "m.1-2", "out.1-2", "out.0-2"] {
            for ext in ["kv", "bt"] {
                let _ = std::fs::remove_file(dir.join(format!("erigon_seg_{}_{stem}.{ext}", std::process::id())));
            }
        }
    }

    #[test]
    fn compressed_seg_roundtrips_and_shrinks() {
        use super::seg_writer::SegWriter;
        // Repetitive content so the dictionary finds real patterns.
        let words: Vec<Vec<u8>> = (0..4000u32)
            .map(|i| {
                format!("account:{:04}:balance=0x0000000000000000000000000000000000:nonce={}", i % 64, i % 7)
                    .into_bytes()
            })
            .collect();

        let plain = scratch("plain.kv");
        let comp = scratch("comp.kv");
        let mut wp = SegWriter::create(&plain).unwrap();
        let mut wc = SegWriter::create_with(&comp, true).unwrap();
        for w in &words {
            wp.add_word(w).unwrap();
            wc.add_word(w).unwrap();
        }
        wp.finish().unwrap();
        wc.finish().unwrap();

        // Round-trip equality for the compressed file.
        let seg = Seg::open(&comp).unwrap();
        assert_eq!(seg.words_count(), words.len() as u64);
        let mut g = seg.getter();
        for (i, want) in words.iter().enumerate() {
            assert!(g.has_next(), "short at {i}");
            assert_eq!(&g.next(), want, "compressed word {i} mismatch");
        }
        assert!(!g.has_next());

        // It should be meaningfully smaller than the no-pattern file.
        let plain_sz = std::fs::metadata(&plain).unwrap().len();
        let comp_sz = std::fs::metadata(&comp).unwrap().len();
        assert!(comp_sz < plain_sz, "compressed {comp_sz} not < plain {plain_sz}");
        eprintln!("compressed {comp_sz} vs plain {plain_sz} ({:.1}% )", 100.0 * comp_sz as f64 / plain_sz as f64);

        let _ = std::fs::remove_file(&plain);
        let _ = std::fs::remove_file(&comp);
    }

    #[test]
    fn seg_writer_roundtrips_through_reader() {
        // Empty file.
        seg_roundtrip(&[]);
        // Single word.
        seg_roundtrip(&[b"hello".to_vec()]);
        // Mixed: empty words, varied lengths, repeats.
        seg_roundtrip(&[
            b"".to_vec(),
            b"a".to_vec(),
            b"".to_vec(),
            b"the quick brown fox".to_vec(),
            vec![0u8; 300],
            b"the quick brown fox".to_vec(),
            (0..=255u8).collect(),
        ]);
        // Many words exercising a larger position dictionary.
        let many: Vec<Vec<u8>> = (0..2000u32).map(|i| vec![(i % 251) as u8; (i % 64) as usize]).collect();
        seg_roundtrip(&many);
    }

    fn ef_roundtrip(offsets: &[u64]) {
        let max = *offsets.last().unwrap();
        let mut b = EfBuilder::new(offsets.len() as u64, max);
        for &o in offsets {
            b.add_offset(o);
        }
        b.build();
        let mut bytes = Vec::new();
        b.write_to(&mut bytes);
        assert_eq!(bytes.len(), b.serialized_len());

        let path = std::env::temp_dir()
            .join(format!("erigon_seg_ef_{}_{}.bin", std::process::id(), offsets.len()));
        std::fs::write(&path, &bytes).unwrap();
        let ef = EliasFano::open(mmap_file(&path).unwrap(), 0).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(ef.len(), offsets.len() as u64);
        for (i, &want) in offsets.iter().enumerate() {
            assert_eq!(ef.get(i as u64), want, "EF.get({i})");
        }
    }

    #[test]
    fn ef_builder_roundtrips_through_reader() {
        ef_roundtrip(&[0]);
        ef_roundtrip(&[0, 1, 2, 3, 4]);
        ef_roundtrip(&[5, 5, 5, 9, 100]); // duplicates allowed (non-decreasing)
        ef_roundtrip(&[0, 1_000, 2_000, 3_500, 1_000_000]);
        // A longer, irregular monotone sequence spanning multiple super-Q blocks.
        let mut v = Vec::new();
        let mut acc = 0u64;
        for i in 0..5000u64 {
            acc += 1 + (i * 2654435761) % 37;
            v.push(acc);
        }
        ef_roundtrip(&v);
    }
}
