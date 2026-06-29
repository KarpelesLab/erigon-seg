//! Re-encode a `.kv` through the writer and verify it round-trips, rebuilding the `.bt`
//! (and `.kvei`, if a salt is given). Demonstrates the full write pipeline on real data.
//!
//! Usage: `cargo run --release --example recode -- <in.kv> <out.kv> [salt-state.txt]`

use std::time::Instant;

use erigon_seg::{DomainOptions, DomainWriter, KvReader, Salt, Seg, salt_from_file};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let in_kv = args
        .next()
        .expect("usage: recode <in.kv> <out.kv> [salt-state.txt]");
    let out_kv = args
        .next()
        .expect("usage: recode <in.kv> <out.kv> [salt-state.txt]");
    let salt = args.next().and_then(salt_from_file);

    // Read the source words (key/value pairs) and stream them into a DomainWriter.
    let src = Seg::open(&in_kv)?;
    let n_words = src.words_count();
    println!("source: {} words ({} keys)", n_words, n_words / 2);

    let t = Instant::now();
    let mut w = DomainWriter::create(
        &out_kv,
        DomainOptions {
            salt,
            ..Default::default()
        },
    )?;
    let mut g = src.getter();
    while g.has_next() {
        let key = g.next();
        let value = if g.has_next() { g.next() } else { Vec::new() };
        w.add(&key, &value)?;
    }
    let paths = w.finish()?;
    println!("wrote {:?} in {:?}", paths, t.elapsed());

    // Verify: every word matches the source byte-for-byte.
    let dst = Seg::open(&out_kv)?;
    assert_eq!(dst.words_count(), n_words);
    let (mut a, mut b) = (src.getter(), dst.getter());
    while a.has_next() {
        assert_eq!(a.next(), b.next(), "word mismatch after re-encode");
    }
    assert!(!b.has_next());
    println!("round-trip OK: all {n_words} words identical");

    // If we built a bloom, confirm it accelerates lookups without false negatives.
    if let Some(s) = salt {
        let mut r = KvReader::open(&out_kv)?;
        assert!(
            r.enable_bloom(Salt::Known(s)),
            "rebuilt .kvei failed to validate"
        );
        let mut checked = 0;
        for kv in r.iter().step_by(997).take(500) {
            let (k, v) = kv?;
            assert_eq!(r.get(&k)?.as_deref(), Some(v.as_slice()));
            checked += 1;
        }
        println!("bloom enabled; {checked} sampled lookups OK");
    }

    println!("\nOK");
    Ok(())
}
