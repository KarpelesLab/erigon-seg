//! K-way merge of several domain `.kv` files into one.
//!
//! Inputs are sorted, unique-keyed domain files. The merge emits each distinct key once,
//! in order, taking the value from the **newest** input that contains it (newest wins,
//! matching erigon's step-range override semantics and this crate's query-time
//! newest-wins in a multi-file reader).
//!
//! **Deleted entries are dropped** following erigon exactly: a key is omitted iff the
//! merged range starts at step 0 *and* the winning value is empty (`r.values.from == 0
//! && len(val) == 0`). Outside a from-zero merge an empty value is a legitimate stored
//! value (e.g. an empty account) and is preserved. The range start is taken from
//! [`MergeOptions::range_from`], or parsed from the output filename's `<from>-<to>`.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::Path;

use super::domain::{DomainOptions, DomainPaths, DomainWriter};
use crate::error::{Error, Result};
use crate::seg::Seg;

/// Options for [`merge`].
#[derive(Debug, Clone, Copy)]
pub struct MergeOptions {
    /// `.bt`/`.kvei` options for the merged output.
    pub domain: DomainOptions,
    /// Honor erigon's "empty value means deletion" rule (only triggers when the merged
    /// range starts at step 0). Default `true`.
    pub drop_deleted: bool,
    /// The merged range's start step. If `None`, parsed from the output `.kv` filename's
    /// `<from>-<to>` segment; if that fails too, deletion-dropping is disabled (no key is
    /// ever dropped), which is the safe choice.
    pub range_from: Option<u64>,
}

impl Default for MergeOptions {
    fn default() -> MergeOptions {
        MergeOptions {
            domain: DomainOptions::default(),
            drop_deleted: true,
            range_from: None,
        }
    }
}

/// Merge `inputs` (domain `.kv` paths) into `out_kv`, also building the sibling `.bt`
/// and—if `opts.domain.salt` is set—`.kvei`.
///
/// Inputs are reordered oldest→newest by the `<from>` step parsed from their filenames
/// (stable, so files without a parseable step keep their given order); the last is
/// treated as newest. Returns the paths written.
pub fn merge(
    inputs: &[impl AsRef<Path>],
    out_kv: impl AsRef<Path>,
    opts: MergeOptions,
) -> Result<DomainPaths> {
    let out_kv = out_kv.as_ref();
    if inputs.is_empty() {
        return Err(Error::format("merge: no input files"));
    }

    // Order oldest -> newest by parsed `from` (stable for unparseable names).
    let mut order: Vec<usize> = (0..inputs.len()).collect();
    order.sort_by_key(|&i| (parse_from(inputs[i].as_ref()).unwrap_or(0), i));

    let segs: Vec<Seg> = order
        .iter()
        .map(|&i| Seg::open(inputs[i].as_ref()))
        .collect::<Result<Vec<_>>>()?;
    let mut getters: Vec<_> = segs.iter().map(|s| s.getter()).collect();

    // Per-input current head (key, value); the heap orders inputs by head key.
    let mut heads: Vec<Option<(Vec<u8>, Vec<u8>)>> = vec![None; segs.len()];
    let mut heap: BinaryHeap<Reverse<(Vec<u8>, usize)>> = BinaryHeap::new();
    for (i, g) in getters.iter_mut().enumerate() {
        if g.has_next() {
            let k = g.next();
            let v = if g.has_next() { g.next() } else { Vec::new() };
            heap.push(Reverse((k.clone(), i)));
            heads[i] = Some((k, v));
        }
    }

    let range_from = opts.range_from.or_else(|| parse_from(out_kv));
    let drop_at_zero = opts.drop_deleted && range_from == Some(0);

    let mut writer = DomainWriter::create(out_kv, opts.domain)?;
    while let Some(Reverse((min_key, idx0))) = heap.pop() {
        // Winner so far is the entry just popped; scan all other inputs with this key and
        // keep the one from the newest input (highest index).
        let mut best_idx = idx0;
        let mut best_val = heads[idx0].take().expect("head present for heap entry").1;
        let mut advance: Vec<usize> = vec![idx0];
        while let Some(Reverse((k, _))) = heap.peek() {
            if *k != min_key {
                break;
            }
            let Reverse((_, idx)) = heap.pop().unwrap();
            let val = heads[idx].take().expect("head present for heap entry").1;
            if idx > best_idx {
                best_idx = idx;
                best_val = val;
            }
            advance.push(idx);
        }

        // Refill the advanced inputs.
        for idx in advance {
            if getters[idx].has_next() {
                let k = getters[idx].next();
                let v = if getters[idx].has_next() {
                    getters[idx].next()
                } else {
                    Vec::new()
                };
                heap.push(Reverse((k.clone(), idx)));
                heads[idx] = Some((k, v));
            }
        }

        if drop_at_zero && best_val.is_empty() {
            continue; // deletion in a from-zero merge
        }
        writer.add(&min_key, &best_val)?;
    }

    writer.finish()
}

/// Parse the `<from>` step from a `…<from>-<to>.<ext>` filename, as a number of steps.
fn parse_from(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_string_lossy();
    for seg in name.split('.') {
        if let Some((a, b)) = seg.split_once('-')
            && let (Ok(a), Ok(_)) = (a.parse::<u64>(), b.parse::<u64>())
        {
            return Some(a);
        }
    }
    None
}
