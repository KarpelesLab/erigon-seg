# erigon-seg

A Rust library for the Erigon 3 **seg** state file format. It currently **reads and
queries** the file triple Erigon writes for each domain snapshot; writing and merging are
planned.

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

## Supported layouts

- **`.kv`**: `v0` (body at offset 0) and `v1` (`[version, feature-flags]`, optional
  page-compression byte, optional out-of-band metadata).
- **`.bt`**: legacy (Elias-Fano at offset 0) and footer (trailing `erigon\0\0` magic
  locating the Elias-Fano section).
- **`.kvei`**: `holiman/bloomfilter/v2` (`v02\n` magic). The newer "fuse filter" layout is
  detected and skipped — lookups stay correct, just unaccelerated.

## Status

Reader only, for now. The format details (`seg` Huffman dictionaries, Elias-Fano, the
`.bt` footer, and the bloom hash schedule) are implemented to match Erigon 3 on-disk
files. Writing and merging will be added later.

## License

MIT OR Apache-2.0
