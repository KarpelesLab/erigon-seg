//! Open a real Erigon `.kv` (with sibling `.bt`/`.kvei`) and exercise the reader:
//! print metadata, iterate the first few pairs, verify point lookups round-trip,
//! resolve the bloom salt, and check a negative lookup.
//!
//! Usage: `cargo run --release --example inspect -- <path-to.kv> [salt-state.txt]`

use std::time::Instant;

use erigon_seg::{KvReader, Salt, murmur3_x64_128_h1, salt_from_file};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let kv_path = args
        .next()
        .expect("usage: inspect <path-to.kv> [salt-state.txt]");
    let salt_path = args.next();

    let t = Instant::now();
    let mut r = KvReader::open(&kv_path)?;
    println!("opened {kv_path} in {:?}", t.elapsed());
    println!("  seg version      : v{}", r.seg().version());
    println!("  words_count      : {}", r.seg().words_count());
    println!("  empty_words      : {}", r.seg().empty_words_count());
    println!("  key_count        : {}", r.key_count());
    println!("  has .bt index    : {}", r.index().is_some());
    if let Some(idx) = r.index() {
        println!("    bt key_count   : {}", idx.key_count());
        println!("    bt M           : {:?}", idx.m());
    }
    match r.existence_filter() {
        Some(f) => println!(
            "  .kvei kind       : {:?} (accelerating={})",
            f.kind(),
            f.is_accelerating()
        ),
        None => println!("  .kvei            : (none)"),
    }

    // First few (key, value) pairs.
    println!("\nfirst pairs:");
    let mut sample: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (i, kv) in r.iter().enumerate().take(5) {
        let (k, v) = kv?;
        println!(
            "  [{i}] key={} ({} B)  value={} B",
            hex(&k),
            k.len(),
            v.len()
        );
        sample.push((k, v));
    }

    // Round-trip a spread of real keys through get().
    println!("\nround-trip lookups (spread across the file):");
    let n = r.key_count();
    let mut checked = 0u64;
    let mut probe_keys: Vec<Vec<u8>> = Vec::new();
    if let Some(idx) = r.index() {
        let g_count = 12u64.min(n.max(1));
        let mut g = r.seg().getter();
        for s in 0..g_count {
            let di = s * n / g_count;
            if let Some(off) = idx.key_offset(di) {
                g.reset(off);
                probe_keys.push(g.next());
            }
        }
    } else {
        probe_keys = sample.iter().map(|(k, _)| k.clone()).collect();
    }
    let t = Instant::now();
    for k in &probe_keys {
        let got = r.get(k)?;
        assert!(got.is_some(), "real key {} not found by get()", hex(k));
        checked += 1;
    }
    let dt = t.elapsed();
    println!(
        "  {checked} keys all found; avg {:?}/lookup",
        dt.checked_div(checked.max(1) as u32).unwrap_or_default()
    );

    // Cross-check: get() value equals the value that follows the key in iteration order.
    for (k, v) in &sample {
        assert_eq!(
            r.get(k)?.as_deref(),
            Some(v.as_slice()),
            "value mismatch for {}",
            hex(k)
        );
    }
    println!("  values match sequential iteration ✓");

    // Negative lookup: a key we are confident is absent.
    let absent = b"\xff_erigon_seg_definitely_absent_key_\xff";
    println!(
        "\nnegative lookup for a synthetic key: {:?}",
        r.get(absent)?.map(|v| v.len())
    );

    // Salt resolution + bloom acceleration.
    if r.existence_filter()
        .map(|f| f.is_accelerating())
        .unwrap_or(false)
    {
        // (a) brute-force find.
        let t = Instant::now();
        let found = r.find_salt(num_cpus());
        println!(
            "\nfind_salt -> {:?}  (in {:?})",
            found.map(|s| format!("{s:#010x}")),
            t.elapsed()
        );

        // (b) known salt from the salt file, if provided.
        let salt = match &salt_path {
            Some(p) => salt_from_file(p),
            None => None,
        };
        if let Some(s) = salt {
            println!("salt-state.txt   -> {s:#010x}");
            if let Some(f) = found {
                assert_eq!(f, s, "brute-forced salt disagrees with salt file");
            }
        }
        let chosen = salt.map(Salt::Known).unwrap_or(Salt::Find(num_cpus()));
        let enabled = r.enable_bloom(chosen);
        println!(
            "enable_bloom     -> {enabled} (active salt = {:?})",
            r.salt().map(|s| format!("{s:#010x}"))
        );

        if let (Some(s), Some(f)) = (r.salt(), r.existence_filter()) {
            // Every real probe key must be reported present by the bloom.
            let all_present = probe_keys
                .iter()
                .all(|k| f.contains_hash(murmur3_x64_128_h1(k, s)));
            println!(
                "bloom: all {} real probe keys reported present = {all_present}",
                probe_keys.len()
            );
            assert!(
                all_present,
                "bloom false-negative on a real key (wrong salt?)"
            );

            // Timed lookups with bloom enabled (negatives short-circuit).
            let t = Instant::now();
            for k in &probe_keys {
                let _ = r.get(k)?;
            }
            println!(
                "  {} bloom-gated lookups in {:?}",
                probe_keys.len(),
                t.elapsed()
            );
        }
    }

    println!("\nOK");
    Ok(())
}

/// Best-effort parallelism for the salt search without pulling in a dependency.
fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}
