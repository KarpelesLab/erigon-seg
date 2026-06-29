# erigon-seg

A Rust library for the Erigon 3 **seg** state file format. It **reads, queries, writes,
and merges** the file triple Erigon writes for each domain snapshot.

| File    | Contents                                                                 |
|---------|--------------------------------------------------------------------------|
| `.kv`   | `seg`-compressed *words*. For a domain file: alternating sorted `key`, `value`. |
| `.bt`   | B-tree index — an Elias-Fano array of the `.kv` byte offset of every key. |
| `.kvei` | Existence (bloom) filter — a *negative* lookup accelerator.               |

## Features

- Decompress `seg` `.kv` files (both the legacy `v0` and the `v1` versioned header).
- Point lookups via the `.bt` Elias-Fano index (`O(log n)` binary search), or an ordered
  linear scan when no index is present.
- Bloom-accelerated negative lookups via `.kvei`, including resolving the index *salt*
  (known, from `salt-state.txt`, or brute-forced).
- Sequential `(key, value)` iteration.
- **Writing**: produce valid, erigon-readable `.kv` (seg, with optional pattern
  compression), `.bt` (legacy or footer layout), and `.kvei` (holiman bloom) files from
  sorted pairs.
- **Merging**: k-way newest-wins merge of several domain files into one, with erigon's
  deletion semantics (empty value dropped only when the merged range starts at step 0).
- Pure-safe Rust apart from the single `mmap` call; no `unsafe` elsewhere.

## Usage

```rust
use erigon_seg::{KvReader, Salt};

let mut r = KvReader::open("v1.1-accounts.0-1024.kv")?;

// Optional: enable the .kvei bloom for fast definite-absent answers.
r.enable_bloom(Salt::Find(8)); // or Salt::Known(salt)

if let Some(value) = r.get(b"\x00\x01\x02")? {
    println!("{} bytes", value.len());
}

for kv in r.iter() {
    let (key, value) = kv?;
    let _ = (key, value);
}
# Ok::<(), erigon_seg::Error>(())
```

## Writing and merging

```rust
use erigon_seg::{DomainWriter, DomainOptions, MergeOptions, merge};

// Write a domain file set from sorted, unique (key, value) pairs.
let mut w = DomainWriter::create("accounts.0-100.kv", DomainOptions {
    salt: Some(0x34e9_3639), // build a .kvei bloom too (omit for no filter)
    compress: true,          // pattern-compress the .kv (smaller; omit for the fast path)
    ..Default::default()
})?;
w.add(b"\x00..key1..", b"value1")?; // keys must be strictly increasing
w.add(b"\x00..key2..", b"value2")?;
let paths = w.finish()?; // writes .kv, .bt, and .kvei

// Merge several domain files into one (newest input wins per key).
merge(&["accounts.0-50.kv", "accounts.50-100.kv"], "accounts.0-100.kv", MergeOptions::default())?;
# Ok::<(), erigon_seg::Error>(())
```

Lower-level building blocks are also public: `SegWriter` (raw words), `build_bt` /
`BtLayout` / `BtOptions`, and `KveiBuilder` / `build_kvei_from_seg`.

## Supported layouts

- **`.kv`**: `v0` (body at offset 0) and `v1` (`[version, feature-flags]`, optional
  page-compression byte, optional out-of-band metadata).
- **`.bt`**: legacy (Elias-Fano at offset 0) and footer (trailing `erigon\0\0` magic
  locating the Elias-Fano section).
- **`.kvei`**: `holiman/bloomfilter/v2` (`v02\n` magic). The newer "fuse filter" layout is
  detected and skipped — lookups stay correct, just unaccelerated.

## Status

Read, query, write, merge, and pattern compression are all implemented and verified
against real Erigon v1.1 files: re-encoding a real `.kv` round-trips byte-exact (with and
without compression), a rebuilt `.bt` matches the real index offset-for-offset, merges
reproduce newest-wins semantics, and compressed output is competitive with erigon's own
(~81% the size on a sample `v1.1-accounts` file). See [ROADMAP.md](ROADMAP.md).

## License

MIT OR Apache-2.0
