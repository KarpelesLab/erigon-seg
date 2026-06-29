//! Reader for a `seg`-compressed `.kv` file: a stream of *words* encoded with a
//! Huffman-coded pattern dictionary plus literal runs.
//!
//! [`Seg`] owns the memory map and the pattern/position dictionaries; a [`Getter`] is a
//! cheap cursor that decompresses words on demand. Getters are not thread-safe, so each
//! thread makes its own from a shared `&Seg`.
//!
//! ## Header
//!
//! The file may begin with a small header before the seg body:
//! * **v0** — no header; the body (the big-endian `words_count`) starts at offset 0.
//! * **v1** — a `[version=1, feature_flags]` pair; if the `PAGE_COMPRESSION` flag is set,
//!   one more byte (values-per-page) follows.
//!
//! An optional, out-of-band *metadata* blob (length-prefixed `u32`) may follow the
//! header for some file kinds; pass `has_metadata = true` to [`Seg::open_with`] if so.
//! Domain `.kv` files do not use it.

use std::path::Path;
use std::sync::Arc;

use memmap2::Mmap;

use crate::error::{Error, Result};
use crate::util::mmap_file;
use crate::varint::uvarint;

const FORMAT_V1: u8 = 1;
const FLAG_PAGE_COMPRESSION: u8 = 0b001;
/// Minimum size of the seg *body* (words_count + empty_words_count + dict_size headers).
const BODY_MIN: usize = 32;

/// Options controlling how a `.kv` is opened.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenOptions {
    /// Whether the file carries an out-of-band metadata blob after the header. This is
    /// not auto-detectable from the file; it is a property of the file kind.
    pub has_metadata: bool,
}

// ---------------------------------------------------------------- pattern dictionary

/// One Huffman codeword in the pattern dictionary. `len == 0` marks an inner node whose
/// `ptr` is the deeper table.
struct Codeword {
    pattern: Vec<u8>,
    ptr: Option<Box<PatternTable>>,
    len: u8,
}

/// A condensed pattern table indexed directly by `code` (erigon always condenses, since
/// every table's `bit_len` is ≤ 9).
struct PatternTable {
    patterns: Vec<Option<Arc<Codeword>>>,
    bit_len: i32,
}

impl PatternTable {
    fn new(bit_len: i32) -> PatternTable {
        PatternTable { patterns: vec![None; 1usize << bit_len.max(0)], bit_len }
    }
    fn insert(&mut self, cw: Arc<Codeword>, code: u16) {
        let code_step: u16 = 1 << cw.len;
        let code_from = code;
        let mut code_to = code.wrapping_add(code_step);
        if self.bit_len != cw.len as i32 && cw.len > 0 {
            code_to = code_from | (1u16 << self.bit_len);
        }
        let mut c = code_from;
        while c < code_to {
            self.patterns[c as usize] = Some(cw.clone());
            c = c.wrapping_add(code_step);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_pattern_table(
    table: &mut PatternTable,
    depths: &[u64],
    patterns: &[&[u8]],
    code: u16,
    bits: i32,
    depth: u64,
    max_depth: u64,
) -> usize {
    if depths.is_empty() {
        return 0;
    }
    if depth == depths[0] {
        let cw = Arc::new(Codeword { pattern: patterns[0].to_vec(), ptr: None, len: bits as u8 });
        table.insert(cw, code);
        return 1;
    }
    if bits == 9 {
        let bl = if max_depth > 9 { 9 } else { max_depth as i32 };
        let mut nested = PatternTable::new(bl);
        let consumed = build_pattern_table(&mut nested, depths, patterns, 0, 0, depth, max_depth);
        let cw = Arc::new(Codeword { pattern: Vec::new(), ptr: Some(Box::new(nested)), len: 0 });
        table.insert(cw, code);
        return consumed;
    }
    if max_depth == 0 {
        return 0;
    }
    let b0 = build_pattern_table(table, depths, patterns, code, bits + 1, depth + 1, max_depth - 1);
    let b1 = build_pattern_table(
        table,
        &depths[b0..],
        &patterns[b0..],
        (1u16 << bits) | code,
        bits + 1,
        depth + 1,
        max_depth - 1,
    );
    b0 + b1
}

// ---------------------------------------------------------------- position dictionary

struct PosTable {
    pos: Vec<u64>,
    lens: Vec<u8>,
    ptrs: Vec<Option<Box<PosTable>>>,
    bit_len: i32,
}

impl PosTable {
    fn new(bit_len: i32) -> PosTable {
        let n = 1usize << bit_len.max(0);
        PosTable { pos: vec![0; n], lens: vec![0; n], ptrs: (0..n).map(|_| None).collect(), bit_len }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_pos_table(
    depths: &[u64],
    poss: &[u64],
    table: &mut PosTable,
    code: u16,
    bits: i32,
    depth: u64,
    max_depth: u64,
) -> usize {
    if depths.is_empty() {
        return 0;
    }
    if depth == depths[0] {
        let p = poss[0];
        if table.bit_len == bits {
            table.pos[code as usize] = p;
            table.lens[code as usize] = bits as u8;
            table.ptrs[code as usize] = None;
        } else {
            let code_step = 1u16 << bits;
            let code_to = code | (1u16 << table.bit_len);
            let mut c = code;
            while c < code_to {
                table.pos[c as usize] = p;
                table.lens[c as usize] = bits as u8;
                table.ptrs[c as usize] = None;
                c += code_step;
            }
        }
        return 1;
    }
    if bits == 9 {
        let bl = if max_depth > 9 { 9 } else { max_depth as i32 };
        let mut nested = PosTable::new(bl);
        let consumed = build_pos_table(depths, poss, &mut nested, 0, 0, depth, max_depth);
        table.pos[code as usize] = 0;
        table.lens[code as usize] = 0;
        table.ptrs[code as usize] = Some(Box::new(nested));
        return consumed;
    }
    if max_depth == 0 {
        return 0;
    }
    let b0 = build_pos_table(depths, poss, table, code, bits + 1, depth + 1, max_depth - 1);
    let b1 = build_pos_table(
        &depths[b0..],
        &poss[b0..],
        table,
        (1u16 << bits) | code,
        bits + 1,
        depth + 1,
        max_depth - 1,
    );
    b0 + b1
}

// ---------------------------------------------------------------- Seg

/// A `seg`-compressed file (`.kv`). Owns its mmap and dictionaries; create a [`Getter`]
/// to read words.
pub struct Seg {
    mmap: Mmap,
    dict: Option<PatternTable>,
    pos_dict: Option<PosTable>,
    words_start: usize,
    words_count: u64,
    empty_words_count: u64,
    version: u8,
    page_values_count: u8,
}

impl Seg {
    /// Open a `.kv` with default options (no out-of-band metadata).
    pub fn open(path: impl AsRef<Path>) -> Result<Seg> {
        Seg::open_with(path, OpenOptions::default())
    }

    /// Open a `.kv` with explicit [`OpenOptions`].
    pub fn open_with(path: impl AsRef<Path>, opts: OpenOptions) -> Result<Seg> {
        let mmap = mmap_file(path.as_ref())?;
        Seg::from_mmap(mmap, opts)
            .ok_or_else(|| Error::format(format!("{}: invalid .kv file", path.as_ref().display())))
    }

    fn from_mmap(mmap: Mmap, opts: OpenOptions) -> Option<Seg> {
        let data: &[u8] = &mmap;

        // ---- header: detect v0 vs v1, optional page byte, optional metadata ----
        let mut off = 0usize;
        let version = *data.first()?;
        let mut page_values_count = 0u8;
        if version == FORMAT_V1 {
            // [version, feature_flags]
            let flags = *data.get(1)?;
            off = 2;
            if flags & FLAG_PAGE_COMPRESSION != 0 {
                page_values_count = *data.get(off)?;
                off += 1;
            }
        }
        if opts.has_metadata {
            let lb = data.get(off..off + 4)?;
            let metadata_len = u32::from_be_bytes(lb.try_into().unwrap()) as usize;
            off += 4 + metadata_len;
        }

        let body = data.get(off..)?;
        if body.len() < BODY_MIN {
            return None;
        }

        let words_count = u64::from_be_bytes(body[0..8].try_into().unwrap());
        let empty_words_count = u64::from_be_bytes(body[8..16].try_into().unwrap());
        let dict_size = u64::from_be_bytes(body[16..24].try_into().unwrap()) as usize;
        let mut pos = 24usize;
        if pos + dict_size > body.len() {
            return None;
        }

        // ---- pattern dictionary: (depth, pattern-bytes) pairs ----
        let dd = &body[pos..pos + dict_size];
        let mut depths: Vec<u64> = Vec::new();
        let mut patterns: Vec<&[u8]> = Vec::new();
        let mut max_depth = 0u64;
        let mut dp = 0usize;
        while dp < dict_size {
            let (depth, ns) = uvarint(&dd[dp..]);
            if ns == 0 || depth > 50 {
                return None;
            }
            depths.push(depth);
            max_depth = max_depth.max(depth);
            dp += ns;
            let (l, n) = uvarint(&dd[dp..]);
            if n == 0 {
                return None;
            }
            dp += n;
            let l = l as usize;
            if dp + l > dict_size {
                return None;
            }
            patterns.push(&dd[dp..dp + l]);
            dp += l;
        }
        let dict = if dict_size > 0 {
            let bit_len = if max_depth > 9 { 9 } else { max_depth as i32 };
            let mut t = PatternTable::new(bit_len);
            build_pattern_table(&mut t, &depths, &patterns, 0, 0, 0, max_depth);
            Some(t)
        } else {
            None
        };

        pos += dict_size;
        if pos + 8 > body.len() {
            return None;
        }
        let pos_dict_size = u64::from_be_bytes(body[pos..pos + 8].try_into().unwrap()) as usize;
        pos += 8;
        if pos + pos_dict_size > body.len() {
            return None;
        }

        // ---- position dictionary: (depth, position) pairs ----
        let pd = &body[pos..pos + pos_dict_size];
        let mut pdepths: Vec<u64> = Vec::new();
        let mut poss: Vec<u64> = Vec::new();
        let mut pmax_depth = 0u64;
        let mut dp = 0usize;
        while dp < pos_dict_size {
            let (depth, ns) = uvarint(&pd[dp..]);
            if ns == 0 || depth > 50 {
                return None;
            }
            pdepths.push(depth);
            pmax_depth = pmax_depth.max(depth);
            dp += ns;
            let (p, n) = uvarint(&pd[dp..]);
            if n == 0 {
                return None;
            }
            dp += n;
            poss.push(p);
        }
        let pos_dict = if pos_dict_size > 0 {
            let bit_len = if pmax_depth > 9 { 9 } else { pmax_depth as i32 };
            let mut t = PosTable::new(bit_len);
            build_pos_table(&pdepths, &poss, &mut t, 0, 0, 0, pmax_depth);
            Some(t)
        } else {
            None
        };

        // `words_start` is absolute (from file start), so getter offsets — which are
        // relative to the words region — index `&mmap[words_start..]`.
        let words_start = off + pos + pos_dict_size;
        Some(Seg {
            mmap,
            dict,
            pos_dict,
            words_start,
            words_count,
            empty_words_count,
            version,
            page_values_count,
        })
    }

    /// Number of words in the file (for a domain `.kv`, `2 × key_count`).
    pub fn words_count(&self) -> u64 {
        self.words_count
    }

    /// Number of empty (zero-length) words.
    pub fn empty_words_count(&self) -> u64 {
        self.empty_words_count
    }

    /// On-disk header version (`0` = legacy, `1` = versioned header).
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Values-per-page if page-level compression is enabled, else `0`. Page reassembly
    /// is not performed by this low-level reader; words are returned as encoded.
    pub fn page_values_count(&self) -> u8 {
        self.page_values_count
    }

    /// Total mapped file length in bytes — a safe over-estimate of the maximum word
    /// offset, suitable as the `max_offset` bound when building a `.bt` index.
    pub fn len(&self) -> usize {
        self.mmap.len()
    }

    /// Whether the file maps to zero bytes.
    pub fn is_empty(&self) -> bool {
        self.mmap.is_empty()
    }

    /// Create a cursor positioned at the start of the words region.
    pub fn getter(&self) -> Getter<'_> {
        Getter {
            pattern_dict: self.dict.as_ref(),
            pos_dict: self.pos_dict.as_ref(),
            data: &self.mmap[self.words_start..],
            data_p: 0,
            data_bit: 0,
        }
    }
}

// ---------------------------------------------------------------- Getter

/// A cursor over a [`Seg`]'s words. Cheap to create; not thread-safe, so each thread
/// makes its own from a shared `&Seg`.
pub struct Getter<'a> {
    pattern_dict: Option<&'a PatternTable>,
    pos_dict: Option<&'a PosTable>,
    data: &'a [u8],
    data_p: u64,
    data_bit: i32,
}

impl<'a> Getter<'a> {
    /// Position the cursor at `offset` (a value from the `.bt` index, or 0 for the
    /// first word).
    #[inline]
    pub fn reset(&mut self, offset: u64) {
        self.data_p = offset;
        self.data_bit = 0;
    }

    /// Whether another word is available at the current position.
    #[inline]
    pub fn has_next(&self) -> bool {
        (self.data_p as usize) < self.data.len()
    }

    /// Byte offset of the cursor within the words region. Valid (byte-aligned) at word
    /// boundaries — i.e. immediately after [`reset`](Self::reset), [`next`](Self::next),
    /// or [`skip`](Self::skip). This is the value the `.bt` index stores per key.
    #[inline]
    pub fn offset(&self) -> u64 {
        self.data_p
    }

    /// Advance past the word at the current offset without materializing it, returning
    /// its length. Port of erigon `Getter.Skip`; far cheaper than [`next`](Self::next)
    /// when only positions/offsets are needed (e.g. building an index).
    pub fn skip(&mut self) -> u64 {
        let word_len = self.next_pos(true).wrapping_sub(1); // -1: 0 is the terminator
        if word_len == 0 {
            if self.data_bit > 0 {
                self.data_p += 1;
                self.data_bit = 0;
            }
            return 0;
        }
        let mut add = 0u64;
        let mut buf_pos: usize = 0;
        let mut last_uncovered: usize = 0;
        loop {
            let pos = self.next_pos(false);
            if pos == 0 {
                break;
            }
            buf_pos += pos as usize - 1;
            if buf_pos > last_uncovered {
                add += (buf_pos - last_uncovered) as u64;
            }
            last_uncovered = buf_pos + self.next_pattern().len();
        }
        if self.data_bit > 0 {
            self.data_p += 1;
            self.data_bit = 0;
        }
        if word_len as usize > last_uncovered {
            add += word_len - last_uncovered as u64;
        }
        self.data_p += add;
        word_len
    }

    fn next_pos(&mut self, clean: bool) -> u64 {
        if clean && self.data_bit > 0 {
            self.data_p += 1;
            self.data_bit = 0;
        }
        let mut table = self.pos_dict.expect("position dict missing");
        if table.bit_len == 0 {
            return table.pos[0];
        }
        let data = self.data;
        let data_len = data.len();
        let mut pos = 0u64;
        loop {
            let mut code = (data[self.data_p as usize] as u16) >> self.data_bit;
            if 8 - self.data_bit < table.bit_len && (self.data_p as usize) + 1 < data_len {
                code |= (data[self.data_p as usize + 1] as u16) << (8 - self.data_bit);
            }
            code &= (1u16 << table.bit_len) - 1;
            let l = table.lens[code as usize];
            if l == 0 {
                table = table.ptrs[code as usize].as_deref().expect("pos inner node missing");
                self.data_bit += 9;
            } else {
                self.data_bit += l as i32;
                pos = table.pos[code as usize];
            }
            self.data_p += (self.data_bit / 8) as u64;
            self.data_bit %= 8;
            if l != 0 {
                break;
            }
        }
        pos
    }

    fn next_pattern(&mut self) -> &'a [u8] {
        let mut table = self.pattern_dict.expect("pattern dict missing");
        if table.bit_len == 0 {
            return &table.patterns[0].as_ref().expect("empty pattern table").pattern;
        }
        let data = self.data;
        let data_len = data.len();
        loop {
            let mut code = (data[self.data_p as usize] as u16) >> self.data_bit;
            if 8 - self.data_bit < table.bit_len && (self.data_p as usize) + 1 < data_len {
                code |= (data[self.data_p as usize + 1] as u16) << (8 - self.data_bit);
            }
            code &= (1u16 << table.bit_len) - 1;
            let cw = table.patterns[code as usize].as_ref().expect("missing codeword");
            let l = cw.len;
            if l == 0 {
                table = cw.ptr.as_deref().expect("pattern inner node missing");
                self.data_bit += 9;
                self.data_p += (self.data_bit / 8) as u64;
                self.data_bit %= 8;
            } else {
                self.data_bit += l as i32;
                self.data_p += (self.data_bit / 8) as u64;
                self.data_bit %= 8;
                return &cw.pattern;
            }
        }
    }

    /// Decompress the word at the current offset, advancing past it. Port of erigon
    /// `Getter.Next(nil)`.
    ///
    /// Named `next` to mirror erigon; the cursor is deliberately not an `Iterator`,
    /// since a domain `.kv` interleaves key and value words that callers consume in
    /// pairs.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Vec<u8> {
        let save_pos = self.data_p;
        let word_len = self.next_pos(true).wrapping_sub(1); // -1: 0 is the terminator
        if word_len == 0 {
            if self.data_bit > 0 {
                self.data_p += 1;
                self.data_bit = 0;
            }
            return Vec::new();
        }
        let word_len = word_len as usize;
        let mut buf = vec![0u8; word_len];

        // Pass 1: lay down the patterns.
        let mut buf_pos: usize = 0;
        loop {
            let pos = self.next_pos(false);
            if pos == 0 {
                break;
            }
            buf_pos += pos as usize - 1;
            let pt = self.next_pattern();
            if buf_pos < buf.len() {
                let n = pt.len().min(buf.len() - buf_pos);
                buf[buf_pos..buf_pos + n].copy_from_slice(&pt[..n]);
            }
        }
        if self.data_bit > 0 {
            self.data_p += 1;
            self.data_bit = 0;
        }
        let mut post_loop_pos = self.data_p;
        self.data_p = save_pos;
        self.data_bit = 0;
        self.next_pos(true); // reset huffman reader to re-walk positions

        // Pass 2: fill the gaps between patterns with literal bytes.
        let data = self.data;
        buf_pos = 0;
        let mut last_uncovered: usize = 0;
        loop {
            let pos = self.next_pos(false);
            if pos == 0 {
                break;
            }
            buf_pos += pos as usize - 1;
            if buf_pos > last_uncovered {
                let dif = buf_pos - last_uncovered;
                buf[last_uncovered..buf_pos]
                    .copy_from_slice(&data[post_loop_pos as usize..post_loop_pos as usize + dif]);
                post_loop_pos += dif as u64;
            }
            last_uncovered = buf_pos + self.next_pattern().len();
        }
        if word_len > last_uncovered {
            let dif = word_len - last_uncovered;
            buf[last_uncovered..last_uncovered + dif]
                .copy_from_slice(&data[post_loop_pos as usize..post_loop_pos as usize + dif]);
            post_loop_pos += dif as u64;
        }
        self.data_p = post_loop_pos;
        self.data_bit = 0;
        buf
    }
}
