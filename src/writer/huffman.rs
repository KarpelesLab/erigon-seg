//! Canonical Huffman construction for the seg **position** dictionary.
//!
//! Port of erigon-lib's position-Huffman build (`compress.go` `Position`/`PositionHuff`
//! and `buildAndWritePosDict`). Symbols are *positions* (a word's `len+1`, the covering
//! distances, and the terminator `0`). The dictionary is serialized as `(depth, pos)`
//! varint pairs, ordered so the reader's `build_pos_table` DFS reconstructs identical
//! codes; the codes themselves are not stored.
//!
//! The same two-queue algorithm and tie-breaking are reproduced exactly so output is
//! bit-identical to erigon for a given multiset of positions.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// A position symbol with its assigned Huffman code.
#[derive(Clone)]
pub(crate) struct Position {
    pub pos: u64,
    pub uses: u64,
    pub code: u64,
    pub code_bits: u32,
    pub depth: u32,
}

/// `positionListCmp`: by `uses` ascending, then by bit-reversed `code` ascending.
fn position_cmp(a: &Position, b: &Position) -> Ordering {
    if a.uses == b.uses {
        a.code.reverse_bits().cmp(&b.code.reverse_bits())
    } else {
        a.uses.cmp(&b.uses)
    }
}

/// A child of an internal Huffman node: either a leaf (index into the `Position` list)
/// or a nested internal node.
enum Child {
    Leaf(usize),
    Node(Box<HuffNode>),
}

impl Child {
    fn add_zero(&self, pos: &mut [Position]) {
        match self {
            Child::Leaf(i) => {
                pos[*i].code <<= 1;
                pos[*i].code_bits += 1;
            }
            Child::Node(h) => h.add_zero(pos),
        }
    }
    fn add_one(&self, pos: &mut [Position]) {
        match self {
            Child::Leaf(i) => {
                pos[*i].code = (pos[*i].code << 1) | 1;
                pos[*i].code_bits += 1;
            }
            Child::Node(h) => h.add_one(pos),
        }
    }
    /// Set leaf depths given the depth of this child's parent node.
    fn set_depth(&self, parent_depth: u32, pos: &mut [Position]) {
        match self {
            Child::Leaf(i) => {
                pos[*i].depth = parent_depth + 1;
                pos[*i].uses = 0;
            }
            Child::Node(h) => {
                h.c0.set_depth(parent_depth + 1, pos);
                h.c1.set_depth(parent_depth + 1, pos);
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
    fn add_zero(&self, pos: &mut [Position]) {
        self.c0.add_zero(pos);
        self.c1.add_zero(pos);
    }
    fn add_one(&self, pos: &mut [Position]) {
        self.c0.add_one(pos);
        self.c1.add_one(pos);
    }
}

/// Min-heap wrapper: ordered by `uses` then `tie`, both ascending (so `BinaryHeap`'s
/// max-pop yields the minimum).
struct HeapItem(Box<HuffNode>);

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for HeapItem {}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse so the heap's top is the smallest (uses, tie).
        (other.0.uses, other.0.tie).cmp(&(self.0.uses, self.0.tie))
    }
}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A serialized dictionary entry plus the code needed to encode that position.
pub(crate) struct PosCode {
    pub pos: u64,
    pub depth: u32,
    pub code: u64,
    pub code_bits: u32,
}

/// Build the position dictionary from a `(position -> uses)` multiset.
///
/// Returns the entries in the exact order they must be written to the file — each
/// carrying its `depth` (serialized) and its `(code, code_bits)` (used to encode words).
pub(crate) fn build_position_dict(pos_uses: &[(u64, u64)]) -> Vec<PosCode> {
    let mut list: Vec<Position> = pos_uses
        .iter()
        .map(|&(pos, uses)| Position { pos, uses, code: pos, code_bits: 0, depth: 0 })
        .collect();
    list.sort_by(position_cmp);

    let mut heap: BinaryHeap<HeapItem> = BinaryHeap::new();
    let mut i = 0usize;
    let mut tie: u64 = 0;

    // Two-queue Huffman: repeatedly merge the two least-used items, drawn from either the
    // sorted leaf list or the heap of internal nodes.
    while heap.len() + (list.len() - i) > 1 {
        // Pick the first (zero-branch) child.
        let take_heap0 = !heap.is_empty()
            && (i >= list.len() || heap.peek().unwrap().0.uses < list[i].uses);
        let (c0, uses0) = if take_heap0 {
            let h = heap.pop().unwrap().0;
            h.add_zero(&mut list);
            let u = h.uses;
            (Child::Node(h), u)
        } else {
            list[i].code = 0;
            list[i].code_bits = 1;
            let c = Child::Leaf(i);
            let u = list[i].uses;
            i += 1;
            (c, u)
        };

        // Pick the second (one-branch) child.
        let take_heap1 = !heap.is_empty()
            && (i >= list.len() || heap.peek().unwrap().0.uses < list[i].uses);
        let (c1, uses1) = if take_heap1 {
            let h = heap.pop().unwrap().0;
            h.add_one(&mut list);
            let u = h.uses;
            (Child::Node(h), u)
        } else {
            list[i].code = 1;
            list[i].code_bits = 1;
            let c = Child::Leaf(i);
            let u = list[i].uses;
            i += 1;
            (c, u)
        };

        heap.push(HeapItem(Box::new(HuffNode { c0, c1, uses: uses0 + uses1, tie })));
        tie += 1;
    }

    if let Some(root) = heap.pop() {
        // SetDepth(0): the root's direct children sit at depth 1.
        root.0.c0.set_depth(0, &mut list);
        root.0.c1.set_depth(0, &mut list);
    }

    // Final write order: by bit-reversed code (all `uses` are now 0 after set_depth).
    list.sort_by(position_cmp);
    list.into_iter()
        .map(|p| PosCode { pos: p.pos, depth: p.depth, code: p.code, code_bits: p.code_bits })
        .collect()
}
