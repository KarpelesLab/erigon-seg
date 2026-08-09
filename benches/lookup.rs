//! Point-lookup benchmark over a real `.kv` + `.bt` (+ `.kvei`) file set.
//!
//! Usage:
//!
//! ```text
//! cargo bench --bench lookup -- <path-to.kv> [probes]
//! ERIGON_SEG_BENCH_KV=/path/to/file.kv cargo bench --bench lookup
//! ```
//!
//! Without a file it builds a synthetic domain in the temp dir, so `cargo bench` always
//! does something useful. Each timing is the best of several rounds — the interesting
//! costs here are memory- and I/O-bound, so the minimum is far more stable than the mean.
//!
//! This measures the *warm* path (page cache primed). Cold-cache behaviour depends on
//! whether the file exceeds RAM and on [`KvReader::advise_random`]; measuring it needs a
//! fresh process per variant plus `posix_fadvise(DONTNEED)`, which is out of scope here.

use std::path::PathBuf;
use std::time::Instant;

use erigon_seg::{BtOptions, DomainOptions, DomainWriter, KvReader, Seg};

const ROUNDS: usize = 5;

/// xorshift64*, so probe selection is deterministic without a dev-dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// Best-of-`ROUNDS` nanoseconds per operation.
fn best_ns<T>(ops: usize, mut f: impl FnMut() -> T) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..ROUNDS {
        let t = Instant::now();
        let out = f();
        let ns = t.elapsed().as_nanos() as f64;
        std::hint::black_box(out);
        best = best.min(ns);
    }
    best / ops as f64
}

/// Build a synthetic domain so the benchmark runs with no arguments.
fn synthetic(n: usize) -> PathBuf {
    let dir = std::env::temp_dir().join("erigon_seg_bench");
    std::fs::create_dir_all(&dir).unwrap();
    let kv = dir.join(format!("v1.1-bench.0-{n}.kv"));
    if kv.exists() {
        return kv;
    }
    let mut rng = Rng(0x1234_5678_9abc_def0);
    let mut keys: Vec<[u8; 32]> = (0..n)
        .map(|_| {
            let mut k = [0u8; 32];
            for c in k.chunks_mut(8) {
                c.copy_from_slice(&rng.next().to_be_bytes());
            }
            k
        })
        .collect();
    keys.sort_unstable();
    keys.dedup();

    let mut w = DomainWriter::create(
        &kv,
        DomainOptions {
            bt: BtOptions::default(),
            salt: Some(9),
            compress: true,
        },
    )
    .unwrap();
    for (i, k) in keys.iter().enumerate() {
        let mut v = [0u8; 40];
        v[..8].copy_from_slice(&(i as u64).to_be_bytes());
        v[8..].copy_from_slice(&k[..32]);
        w.add(k, &v).unwrap();
    }
    w.finish().unwrap();
    kv
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let kv = args
        .first()
        .cloned()
        .or_else(|| std::env::var("ERIGON_SEG_BENCH_KV").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| synthetic(1_000_000));
    let probe_count: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2000);

    let t = Instant::now();
    let reader = KvReader::open(&kv).unwrap();
    let open = t.elapsed();
    let seg = Seg::open(&kv).unwrap();
    let Some(idx) = reader.index() else {
        eprintln!("{}: no .bt index; nothing to benchmark", kv.display());
        return;
    };
    let count = idx.key_count();
    assert!(count > 0, "empty index");

    println!("file      : {}", kv.display());
    println!(
        "keys      : {count}  ({:.2} GiB .kv)",
        std::fs::metadata(&kv).map(|m| m.len()).unwrap_or(0) as f64 / (1u64 << 30) as f64
    );
    println!("open      : {open:?} (di-nodes are parsed on first lookup, not here)");

    // Touching `nodes()` forces the lazy parse, so it is not charged to the first probe.
    let t = Instant::now();
    match idx.nodes() {
        Some(n) => println!(
            "nodes     : {} entries, {:.1} MiB, parsed in {:?}  (narrowing active)",
            n.len(),
            n.heap_bytes() as f64 / (1u64 << 20) as f64,
            t.elapsed()
        ),
        None => println!("nodes     : none (legacy .bt layout; full binary search)"),
    }
    println!(
        "probes    : {probe_count} random keys, ~{:.0} comparisons unnarrowed vs ~{:.0} narrowed",
        (count as f64).log2(),
        (idx.m().unwrap_or(0) as f64).max(2.0).log2()
    );

    // Probe set: real keys spread at random across the file, plus matching misses.
    let mut rng = Rng(0xDEAD_BEEF_1234_5678);
    let mut g = seg.getter();
    let hits: Vec<Vec<u8>> = (0..probe_count)
        .map(|_| {
            g.reset(idx.key_offset(rng.next() % count).unwrap());
            g.next()
        })
        .collect();
    let misses: Vec<Vec<u8>> = hits
        .iter()
        .map(|k| {
            let mut k = k.clone();
            let l = k.len();
            k[l - 1] ^= 0x5a;
            k[l / 2] ^= 0xa5;
            k
        })
        .collect();

    println!("\n-- point lookup (best of {ROUNDS}) --");
    let hit_ns = best_ns(hits.len(), || {
        hits.iter()
            .filter(|k| reader.get(k).unwrap().is_some())
            .count()
    });
    println!("  get() hit        {hit_ns:>9.0} ns/op");
    let miss_ns = best_ns(misses.len(), || {
        misses
            .iter()
            .filter(|k| reader.get(k).unwrap().is_some())
            .count()
    });
    println!("  get() miss       {miss_ns:>9.0} ns/op");

    println!("\n-- components --");
    let ef = idx.elias_fano().unwrap();
    let ef_ns = best_ns(200_000, || {
        let mut acc = 0u64;
        for i in 0..200_000u64 {
            acc ^= ef.get(i.wrapping_mul(2_654_435_761) % count);
        }
        acc
    });
    println!("  EliasFano::get   {ef_ns:>9.1} ns/op   (one per search comparison)");

    let offsets: Vec<u64> = (0..100_000u64)
        .map(|i| {
            idx.key_offset(i.wrapping_mul(2_654_435_761) % count)
                .unwrap()
        })
        .collect();
    let dec_ns = best_ns(offsets.len(), || {
        let mut g = seg.getter();
        let mut n = 0usize;
        for &o in &offsets {
            g.reset(o);
            n += g.next().len();
        }
        n
    });
    println!("  Getter::next     {dec_ns:>9.1} ns/op   (random offsets, one key)");

    let skip_ns = best_ns(offsets.len(), || {
        let mut g = seg.getter();
        let mut n = 0u64;
        for &o in &offsets {
            g.reset(o);
            n += g.skip();
        }
        n
    });
    println!("  Getter::skip     {skip_ns:>9.1} ns/op");

    let words = (seg.words_count()).min(400_000);
    let scan_ns = best_ns(words as usize, || {
        let mut g = seg.getter();
        let mut n = 0usize;
        let mut i = 0u64;
        while g.has_next() && i < words {
            n += g.next().len();
            i += 1;
        }
        n
    });
    println!("  sequential next  {scan_ns:>9.1} ns/word ({words} words)");
}
