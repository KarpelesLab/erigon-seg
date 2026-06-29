//! Reader for the Erigon 3 **seg** state file format.
//!
//! Erigon stores a snapshot of domain state (accounts, storage, code, …) as a triple
//! of sibling files sharing one base name, e.g. `v1.1-accounts.0-1024.{kv,bt,kvei}`:
//!
//! * **`.kv`** — the data: a `seg`-compressed stream of *words*. For a domain file the
//!   words alternate `key`, `value`, `key`, `value`, … and the keys are sorted.
//! * **`.bt`** — a B-tree index whose payload is an Elias-Fano array giving the `.kv`
//!   byte offset of every key, enabling an `O(log n)` point lookup.
//! * **`.kvei`** — an existence (bloom) filter: a *negative* accelerator. If it says a
//!   key is absent, the `.bt` search can be skipped entirely. It never reports a real
//!   key as absent (no false negatives), so it is safe to trust for the negative case.
//!
//! This crate currently implements **reading and querying**. Writing and merging are
//! planned as later additions.
//!
//! # Quick start
//!
//! ```no_run
//! use erigon_seg::{KvReader, Salt};
//!
//! // Open a .kv and (if present) its sibling .bt / .kvei.
//! let mut r = KvReader::open("v1.1-accounts.0-1024.kv")?;
//!
//! // The .kvei bloom needs the index salt to be useful; resolve it once.
//! r.enable_bloom(Salt::Find(8));
//!
//! // Point lookup (bloom-accelerated if enabled, else B-tree binary search).
//! if let Some(value) = r.get(b"\x00\x01\x02")? {
//!     println!("value = {} bytes", value.len());
//! }
//!
//! // Or scan every key/value pair sequentially.
//! for kv in r.iter() {
//!     let (key, value) = kv?;
//!     let _ = (key, value);
//! }
//! # Ok::<(), erigon_seg::Error>(())
//! ```
//!
//! # Format notes
//!
//! The reader handles both released on-disk layouts:
//!
//! * `.kv`: the legacy `v0` header (body at offset 0) and the `v1` header (a leading
//!   `[version, feature-flags]` pair, an optional page-compression byte, and optional
//!   out-of-band metadata).
//! * `.bt`: the legacy layout (Elias-Fano at offset 0) and the newer footer layout
//!   (a trailing `erigon\0\0` magic locating the Elias-Fano section).
//! * `.kvei`: the `holiman/bloomfilter` layout (`v02\n` magic). The newer "fuse filter"
//!   layout is detected and skipped (lookups remain correct, just unaccelerated).

// `unsafe` is `deny`-not-`forbid` so the single mmap call site (util.rs) can opt in.
#![deny(unsafe_code)]
#![warn(missing_docs)]

mod bloom;
mod btree;
mod eliasfano;
mod error;
mod hash;
mod reader;
mod salt;
mod seg;
mod util;
mod varint;
mod writer;

pub use bloom::ExistenceFilter;
pub use btree::BtreeIndex;
pub use eliasfano::EliasFano;
pub use error::{Error, Result};
pub use hash::murmur3_x64_128_h1;
pub use reader::{KvIter, KvReader};
pub use salt::{Salt, salt_from_file};
pub use seg::{Getter, OpenOptions, Seg};
pub use writer::{
    BtLayout, BtOptions, DEFAULT_BTREE_M, DomainOptions, DomainPaths, DomainWriter, KveiBuilder,
    MergeOptions, SegWriter, build_bt, build_bt_from_seg, build_kvei_from_seg, merge,
};
