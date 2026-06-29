//! Writer for `.bt` B-tree index files, in both on-disk layouts.
//!
//! The index records the `.kv` byte offset of every key as an Elias-Fano array. Two
//! layouts are produced (selectable via [`BtLayout`]):
//!
//! * [`BtLayout::Legacy`] — just the serialized Elias-Fano (`[EF]`). The reader and
//!   erigon both treat trailing B-tree nodes as optional.
//! * [`BtLayout::Footer`] — `[0x01][nodes][pad→4096][EF][pad→8][footer][anchor]`, where
//!   `nodes` holds the key at every `M`-th position (`keyLen:u16-BE | key`) for
//!   co-located binary search, and the trailing footer/anchor carry `keys_count`, `M`,
//!   `ef_offset`, and the `erigon\0\0` magic. Port of erigon `BtIndexWriter`.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use super::ef_builder::EfBuilder;
use crate::error::{Error, Result};
use crate::seg::Seg;

/// Default B-tree fanout (`DefaultBtreeM`), the number of keys per leaf.
pub const DEFAULT_BTREE_M: u64 = 256;

const BT_EF_ALIGN: usize = 4096;
const BT_FOOTER_ALIGN: usize = 8;
const BT_VERSION: u16 = 1;
const BT_METADATA_LEN: u32 = 24;
const FOOTER_MAGIC: [u8; 8] = *b"erigon\x00\x00";
const FIRST_BYTE_FOOTER: u8 = 0x01;

/// Which `.bt` on-disk layout to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtLayout {
    /// Just the Elias-Fano offset array (smallest; binary search only).
    Legacy,
    /// Footer layout with di-nodes for co-located reads (erigon's current default).
    Footer,
}

/// Options for building a `.bt` index.
#[derive(Debug, Clone, Copy)]
pub struct BtOptions {
    /// Which layout to write.
    pub layout: BtLayout,
    /// B-tree fanout `M` (footer layout only).
    pub m: u64,
}

impl Default for BtOptions {
    fn default() -> BtOptions {
        BtOptions { layout: BtLayout::Footer, m: DEFAULT_BTREE_M }
    }
}

/// Build a `.bt` index for a `.kv` file, writing it to `bt_path`.
pub fn build_bt(kv_path: impl AsRef<Path>, bt_path: impl AsRef<Path>, opts: BtOptions) -> Result<()> {
    let seg = Seg::open(kv_path)?;
    build_bt_from_seg(&seg, bt_path, opts)
}

/// Build a `.bt` index from an already-open [`Seg`].
pub fn build_bt_from_seg(seg: &Seg, bt_path: impl AsRef<Path>, opts: BtOptions) -> Result<()> {
    let bt_path = bt_path.as_ref();
    let key_count = seg.words_count() / 2;

    // An empty domain yields an empty (0-byte) index, which the reader treats as 0 keys.
    if key_count == 0 {
        File::create(bt_path).map_err(|e| Error::io(bt_path, e))?;
        return Ok(());
    }

    let max_offset = seg.len() as u64;
    let m = opts.m.max(1);
    let mut ef = EfBuilder::new(key_count, max_offset);

    // The footer layout streams the di-nodes (keys at every M-th position) ahead of the EF.
    let mut nodes: Vec<u8> = Vec::new();
    let footer = opts.layout == BtLayout::Footer;
    if footer {
        nodes.push(FIRST_BYTE_FOOTER);
    }

    let mut g = seg.getter();
    for di in 0..key_count {
        let off = g.offset();
        if footer && di % m == 0 {
            let key = g.next(); // need the bytes for this node
            let klen = u16::try_from(key.len())
                .map_err(|_| Error::format("key longer than 65535 bytes (unsupported in .bt node)"))?;
            nodes.extend_from_slice(&klen.to_be_bytes());
            nodes.extend_from_slice(&key);
        } else {
            g.skip(); // key
        }
        g.skip(); // value
        ef.add_offset(off);
    }
    ef.build();

    match opts.layout {
        BtLayout::Legacy => {
            let mut out = Vec::with_capacity(ef.serialized_len());
            ef.write_to(&mut out);
            write_all(bt_path, &out)
        }
        BtLayout::Footer => {
            let mut out = nodes;
            pad_to(&mut out, BT_EF_ALIGN);
            let ef_offset = out.len() as u64;
            ef.write_to(&mut out);
            pad_to(&mut out, BT_FOOTER_ALIGN);
            // Footer payload: keys_count | M | ef_offset.
            out.extend_from_slice(&key_count.to_be_bytes());
            out.extend_from_slice(&m.to_be_bytes());
            out.extend_from_slice(&ef_offset.to_be_bytes());
            // Anchor: footer_len(u32) | flags(u16) | format_version(u16) | magic(u64).
            out.extend_from_slice(&BT_METADATA_LEN.to_be_bytes());
            out.extend_from_slice(&0u16.to_be_bytes()); // flags
            out.extend_from_slice(&BT_VERSION.to_be_bytes());
            out.extend_from_slice(&FOOTER_MAGIC);
            write_all(bt_path, &out)
        }
    }
}

fn pad_to(out: &mut Vec<u8>, align: usize) {
    let rem = out.len() % align;
    if rem != 0 {
        out.resize(out.len() + (align - rem), 0);
    }
}

fn write_all(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = File::create(path).map_err(|e| Error::io(path, e))?;
    f.write_all(bytes).map_err(|e| Error::io(path, e))?;
    f.flush().map_err(|e| Error::io(path, e))
}
