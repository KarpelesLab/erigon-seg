# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `memlock_limit` and `raise_memlock_limit`, and the locking calls now raise the soft
  `RLIMIT_MEMLOCK` to the hard limit automatically, once per process. A stock Linux box
  ships a small soft limit under a large hard one, so `mlock` used to fail for a reason
  the administrator never intended: with a 64 MiB soft limit and a 4 GiB hard limit,
  locking a 1.37 GiB mapping directly fails with `ENOMEM` while `lock_all` now succeeds.
  Raising the soft limit needs no privilege and cannot exceed configured policy. Raising
  the *hard* limit needs `CAP_SYS_RESOURCE` and is only ever done through the explicit
  `raise_memlock_limit`, never implicitly — escalating past a configured limit is the
  application's decision, not a file reader's.
- `KvReader::lock_all` / `unlock_all`, `KvStack::lock_all` / `unlock_all`, and
  `Seg::lock` / `unlock`: the locked form of `preload_all`, pinning the `.kv` as well as
  the index so nothing is evicted. Locking a shared file mapping does *not* cost memory
  per process — two processes locking the same 1.37 GiB file were measured to hold 1.39
  GiB of system `Mlocked` between them, not 2.74 GiB — but `RLIMIT_MEMLOCK` is
  per-process accounting and defaults low, so over the limit it fails with `ENOMEM` and
  leaves the data preloaded (measured: 100% resident) rather than half-configured.

## [1.2.0](https://github.com/KarpelesLab/erigon-seg/compare/v1.1.0...v1.2.0) - 2026-09-03

### Added

- add preload_all so the .kv can be brought into memory too

### Fixed

- keep docs and non-x86-64 clippy clean

### Other

- resolve BMI2 once per index, prefetch large node arenas

### Added

- `KvReader::preload_all` / `KvStack::preload_all`, `KvReader::total_bytes` /
  `KvStack::total_bytes`, and `Seg::preload` / `Seg::mapped_bytes`. `preload_index` only
  ever covered the index; there was no way to bring the `.kv` itself into memory. On an
  11.5 GiB file set, `preload_all` loaded 11.82 GiB in 3.8s and took cold lookups from
  306us to 2.5us, against 118us for `preload_index` alone.
- `advise_will_need` on `KvReader` / `KvStack` / `Seg` / `BtreeIndex` / `ExistenceFilter`.
  Note that Linux implements `MADV_WILLNEED` on a file mapping as bounded, best-effort
  read-ahead: against an 11.5 GiB `.kv` it left 0% of pages resident after five seconds,
  so it is documented as a weak hint and `preload_all` is the way to actually load data.

### Changed

- `EliasFano::get` resolves the BMI2 feature check once, when the index is opened, and
  dispatches to a `#[target_feature(enable = "bmi2")]` body rather than testing the
  feature inside `select64` on every call. Measured 1.05-1.42x faster `EliasFano::get`
  on real indexes; the per-call test alone cost ~2.8x on `select64`, and it also stopped
  the surrounding function from being compiled as BMI2 code.
- `Nodes::narrow` prefetches both candidate probes when the node arena is large enough
  to miss cache (16 MiB and up). Measured 1.20-1.23x on 29 MiB and 142 MiB arenas; below
  the threshold the wasted prefetch costs ~8%, hence the gate.

## [1.1.0](https://github.com/KarpelesLab/erigon-seg/compare/v1.0.1...v1.1.0) - 2026-08-09

### Added

- add index preload and mlock for machines with RAM to spare

### Fixed

- gate mlock behind cfg(unix) so Windows builds

### Other

- narrow lookups with .bt di-nodes, single-pass decode, madvise hints

### Added

- `BtreeIndex::nodes` and the `Nodes` type: the `.bt` di-node array (the key at every
  `M`-th position), which the reader previously ignored. Parsed on first use, so opening
  a file only to scan it costs nothing.
- `BtreeIndex::narrow`: the key-index range a lookup can be restricted to.
- `KvReader::advise_random` / `KvReader::advise_sequential`, `KvStack::advise_random`,
  and the same on `Seg` / `BtreeIndex` / `ExistenceFilter`. Opt-in `madvise` hints. On a
  file much larger than RAM, `advise_random` cut measured read amplification for a point
  lookup from ~19 MiB to ~18 KiB; on a file that fits in the page cache it is a
  pessimisation, which is why it is not the default.
- `KvReader::preload_index` / `lock_index` / `unlock_index` / `index_bytes`, and the same
  on `KvStack`; `preload` / `lock` / `unlock` / `mapped_bytes` on `BtreeIndex` and
  `ExistenceFilter`. A point lookup reads the `.bt` at every search comparison but
  decompresses only the final block of keys, and the `.bt` is one to two orders of
  magnitude smaller than the `.kv` — so on a machine with RAM to spare, holding just the
  index resident removes nearly all remaining faults. On a 37 GiB file set with a 1.4 GiB
  `.bt`, cold lookups went from ~400 µs to ~140 µs for a one-off 242 ms load.
  `preload_index` covers the `.kvei` only when the bloom is active, since it is otherwise
  never read; `lock_index` additionally pins the pages with `mlock`, subject to
  `RLIMIT_MEMLOCK`.

### Changed

- Point lookups now use the `.bt` di-nodes to narrow the binary search to one `M`-key
  block before touching the `.kv`, turning `log2(n)` decompressing probes into
  `log2(n/M)` in-memory comparisons plus `log2(M)` probes. Measured 2.2–7.6× faster
  `get()` on real files (8 files, 68 K to 1.5 B keys), and up to ~30× cold when combined
  with `advise_random`. Falls back to the full search for the legacy `.bt` layout.
- `Getter::next` decodes the Huffman position stream once instead of twice, recording
  pattern positions on the first pass rather than rewinding to recover them. Output is
  byte-identical; sequential decoding is ~1.3–1.5× faster.
- `EliasFano::get` uses a BMI2 `pdep` for `select64` on x86-64, falling back to the
  previous loop elsewhere.
- `KvStack::get` hashes the key once for the whole stack instead of once per file (one
  salt covers the stack), and only reuses the hash for files whose bloom was enabled
  with that salt.
- `KvReader::get` on a file with no `.bt` now skips over values it is not returning
  instead of decompressing and allocating them.

## [1.0.1](https://github.com/KarpelesLab/erigon-seg/compare/v1.0.0...v1.0.1) - 2026-06-30

### Added

- add KvStack multi-file stack, Salt::None, KvReader bloom_active/name

### Other

- cargo fmt
- mark Salt #[non_exhaustive]

### Added

- `KvStack`: a newest-wins stack of seg files spanning a step range, with `get`
  (overrides win), `salt`, `bloom_count`, `files`, and `readers`. Opens an explicit set
  of paths (`KvStack::open`) or every matching `.kv` in a directory (`KvStack::open_dir`),
  resolving the bloom salt once and enabling each file's filter against it.
- `Salt::None` variant (skip the bloom entirely; exact `.bt` search only). `Salt` is now
  `#[non_exhaustive]` so future variants are not a breaking change.
- `KvReader::name` and `KvReader::bloom_active` accessors.

## [1.0.0](https://github.com/KarpelesLab/erigon-seg/compare/v0.1.0...v1.0.0) - 2026-06-29

### Other

- non_exhaustive Error/FilterKind; add path-based build_kvei
- Add MIT LICENSE; switch crate license from dual to MIT
- remove completed ROADMAP; drop its references
- add CI, crates.io, docs.rs, license, and MSRV badges to README
