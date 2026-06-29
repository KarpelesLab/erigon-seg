//! Canonical Huffman construction for the seg **position** and **pattern** dictionaries.
//!
//! Port of erigon-lib's two-queue position/pattern Huffman (`compress.go`). Symbols are
//! serialized as `(depth, …)` records ordered so the reader's `build_pos_table` /
//! `build_pattern_table` DFS reconstructs identical codes; the codes themselves are not
//! stored. The same algorithm and tie-breaking are reproduced so a given multiset of
//! symbols yields a deterministic, reader-compatible dictionary.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// One symbol during Huffman construction. `init_code` seeds the pre-build ordering
/// (erigon uses the position value for positions); after the build, `code`/`code_bits`
/// hold the assigned Huffman code and `depth` its length.
#[derive(Clone)]
struct Sym {
    index: usize,
    uses: u64,
    code: u64,
    code_bits: u32,
    depth: u32,
}

fn sym_cmp(a: &Sym, b: &Sym) -> Ordering {
    if a.uses == b.uses {
        a.code.reverse_bits().cmp(&b.code.reverse_bits())
    } else {
        a.uses.cmp(&b.uses)
    }
}

enum Child {
    Leaf(usize), // index into the `syms` vec
    Node(Box<HuffNode>),
}

impl Child {
    fn add_bit(&self, one: bool, syms: &mut [Sym]) {
        match self {
            Child::Leaf(i) => {
                syms[*i].code = (syms[*i].code << 1) | (one as u64);
                syms[*i].code_bits += 1;
            }
            Child::Node(h) => h.add_bit(one, syms),
        }
    }
    fn set_depth(&self, parent_depth: u32, syms: &mut [Sym]) {
        match self {
            Child::Leaf(i) => {
                syms[*i].depth = parent_depth + 1;
                syms[*i].uses = 0;
            }
            Child::Node(h) => {
                h.c0.set_depth(parent_depth + 1, syms);
                h.c1.set_depth(parent_depth + 1, syms);
            }
        }
    }
}

struct HuffNode {
    c0: Child,
    c1: Child,
    uses: u64,
    tie: u64,
}

impl HuffNode {
    fn add_bit(&self, one: bool, syms: &mut [Sym]) {
        self.c0.add_bit(one, syms);
        self.c1.add_bit(one, syms);
    }
}

struct HeapItem(Box<HuffNode>);
impl PartialEq for HeapItem {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for HeapItem {}
impl Ord for HeapItem {
    fn cmp(&self, o: &Self) -> Ordering {
        // Reverse so BinaryHeap's max-pop yields the minimum (uses, tie).
        (o.0.uses, o.0.tie).cmp(&(self.0.uses, self.0.tie))
    }
}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// Result of the build for one symbol: its original index plus the assigned code.
pub(crate) struct HuffCode {
    pub index: usize,
    pub depth: u32,
    pub code: u64,
    pub code_bits: u32,
}

/// Build canonical Huffman codes. Input is one `(uses, init_code)` per symbol; the
/// returned codes are in serialization order (by bit-reversed code). `index` refers back
/// to the input position.
pub(crate) fn build_huffman(symbols: &[(u64, u64)]) -> Vec<HuffCode> {
    let mut syms: Vec<Sym> = symbols
        .iter()
        .enumerate()
        .map(|(index, &(uses, init_code))| Sym {
            index,
            uses,
            code: init_code,
            code_bits: 0,
            depth: 0,
        })
        .collect();
    syms.sort_by(sym_cmp);

    let mut heap: BinaryHeap<HeapItem> = BinaryHeap::new();
    let mut i = 0usize;
    let mut tie: u64 = 0;
    while heap.len() + (syms.len() - i) > 1 {
        let take_heap = |heap: &BinaryHeap<HeapItem>, i: usize, syms: &[Sym]| {
            !heap.is_empty() && (i >= syms.len() || heap.peek().unwrap().0.uses < syms[i].uses)
        };
        let (c0, u0) = if take_heap(&heap, i, &syms) {
            let h = heap.pop().unwrap().0;
            h.add_bit(false, &mut syms);
            let u = h.uses;
            (Child::Node(h), u)
        } else {
            syms[i].code = 0;
            syms[i].code_bits = 1;
            let c = Child::Leaf(i);
            let u = syms[i].uses;
            i += 1;
            (c, u)
        };
        let (c1, u1) = if take_heap(&heap, i, &syms) {
            let h = heap.pop().unwrap().0;
            h.add_bit(true, &mut syms);
            let u = h.uses;
            (Child::Node(h), u)
        } else {
            syms[i].code = 1;
            syms[i].code_bits = 1;
            let c = Child::Leaf(i);
            let u = syms[i].uses;
            i += 1;
            (c, u)
        };
        heap.push(HeapItem(Box::new(HuffNode {
            c0,
            c1,
            uses: u0 + u1,
            tie,
        })));
        tie += 1;
    }

    if let Some(root) = heap.pop() {
        root.0.c0.set_depth(0, &mut syms);
        root.0.c1.set_depth(0, &mut syms);
    }

    syms.sort_by(sym_cmp);
    syms.into_iter()
        .map(|s| HuffCode {
            index: s.index,
            depth: s.depth,
            code: s.code,
            code_bits: s.code_bits,
        })
        .collect()
}

/// A serialized position-dictionary entry plus its encoding code.
pub(crate) struct PosCode {
    pub pos: u64,
    pub depth: u32,
    pub code: u64,
    pub code_bits: u32,
}

/// Build the position dictionary from a `(position -> uses)` multiset. Entries are
/// returned in file order; each carries its `depth` (serialized) and `(code, code_bits)`.
pub(crate) fn build_position_dict(pos_uses: &[(u64, u64)]) -> Vec<PosCode> {
    // erigon seeds the ordering with the position value as the initial code.
    let input: Vec<(u64, u64)> = pos_uses.iter().map(|&(pos, uses)| (uses, pos)).collect();
    build_huffman(&input)
        .into_iter()
        .map(|h| {
            let pos = pos_uses[h.index].0;
            PosCode {
                pos,
                depth: h.depth,
                code: h.code,
                code_bits: h.code_bits,
            }
        })
        .collect()
}

/// A serialized pattern-dictionary entry plus its encoding code.
pub(crate) struct PatCode {
    /// Index into the caller's pattern list.
    pub index: usize,
    pub depth: u32,
    pub code: u64,
    pub code_bits: u32,
}

/// Build the pattern dictionary from per-pattern use counts. `pattern_uses[i]` is the
/// number of times pattern `i` was used. Entries are returned in file order.
pub(crate) fn build_pattern_dict(pattern_uses: &[u64]) -> Vec<PatCode> {
    // Pattern ordering is seeded by index (any deterministic seed yields a valid, reader-
    // reconstructable code; only compression efficiency depends on the choice).
    let input: Vec<(u64, u64)> = pattern_uses
        .iter()
        .enumerate()
        .map(|(i, &u)| (u, i as u64))
        .collect();
    build_huffman(&input)
        .into_iter()
        .map(|h| PatCode {
            index: h.index,
            depth: h.depth,
            code: h.code,
            code_bits: h.code_bits,
        })
        .collect()
}
