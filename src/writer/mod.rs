//! Writing and merging seg file sets.
//!
//! Built from shared primitives ([`bitwriter`], [`huffman`], [`ef_builder`]) up through
//! the seg `.kv` writer, the `.bt` and `.kvei` builders, the combined [`DomainWriter`],
//! and [`merge`]. See `ROADMAP.md` for the staged design.

mod bitwriter;
mod bt_writer;
mod ef_builder;
mod huffman;
mod kvei_writer;
mod seg_writer;

pub use bt_writer::{BtLayout, BtOptions, DEFAULT_BTREE_M, build_bt, build_bt_from_seg};
pub use kvei_writer::{KveiBuilder, build_kvei_from_seg};
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
