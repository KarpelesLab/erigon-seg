# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
