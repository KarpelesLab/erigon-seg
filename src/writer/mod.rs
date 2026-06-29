//! Writing and merging seg file sets.
//!
//! Built from shared primitives ([`bitwriter`], [`huffman`], [`ef_builder`]) up through
//! the seg `.kv` writer, the `.bt` and `.kvei` builders, the combined [`DomainWriter`],
//! and [`merge`]. See `ROADMAP.md` for the staged design.

mod bitwriter;
mod ef_builder;
mod huffman;
mod seg_writer;

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
