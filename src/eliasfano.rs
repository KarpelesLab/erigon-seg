//! Reader for a serialized `eliasfano32.EliasFano` — the monotone array of `.kv`
//! byte offsets stored inside a `.bt` index.
//!
//! On disk the section is: `count: u64` (big-endian, stored as `real_count - 1`),
//! `u: u64` (big-endian, `max_offset + 1`), then the bit-packed `u64` words in native
//! little-endian order. We only ever read, so this mirrors erigon-lib's `ReadEliasFano`
//! / `Get`, including the `select`-table ("jump") fast path.

use memmap2::Mmap;

use crate::error::{Error, Result};

const EF_LOG2Q: u64 = 8;
const EF_Q: u64 = 1 << EF_LOG2Q; // 256
const EF_QMASK: u64 = EF_Q - 1;
const EF_SUPERQ: u64 = 1 << 14; // 16384
const EF_SUPERQ_SIZE: u64 = 1 + (EF_SUPERQ / EF_Q) / 2; // 33

/// `bitutil.Select64`: index (0-based) of the `k`-th set bit in `x`.
#[inline]
fn select64(mut x: u64, k: u32) -> u32 {
    for _ in 0..k {
        x &= x - 1; // clear lowest set bit
    }
    x.trailing_zeros()
}

/// A read-only view of an Elias-Fano monotone sequence.
///
/// Owns the memory map of the file it lives in and indexes into it at a fixed `base`
/// (0 for the legacy `.bt` layout, or the footer's `ef_offset` for the newer one).
pub struct EliasFano {
    mmap: Mmap,
    base: usize,
    count: u64, // stored count == real_count - 1
    l: u64,
    lower_mask: u64,
    words_lower: usize,
    words_upper: usize,
}

impl EliasFano {
    /// Parse the Elias-Fano header located at `base` within `mmap`.
    pub(crate) fn open(mmap: Mmap, base: usize) -> Result<EliasFano> {
        if base + 16 > mmap.len() {
            return Err(Error::format("Elias-Fano: truncated header"));
        }
        let count = u64::from_be_bytes(mmap[base..base + 8].try_into().unwrap());
        let u = u64::from_be_bytes(mmap[base + 8..base + 16].try_into().unwrap());
        // deriveFields(): pick the lower-bits width `l = floor(log2(u / (count+1)))`.
        let l = if u / (count + 1) == 0 {
            0
        } else {
            63 - (u / (count + 1)).leading_zeros() as u64
        };
        let lower_mask = if l == 0 { 0 } else { (1u64 << l) - 1 };
        let words_lower = (((count + 1) * l).div_ceil(64) + 1) as usize;
        let words_upper = ((count + 1 + (u >> l)).div_ceil(64)) as usize;
        let ef = EliasFano {
            mmap,
            base,
            count,
            l,
            lower_mask,
            words_lower,
            words_upper,
        };
        // Bounds-check that at least the lower+upper bit regions fit; the trailing
        // jump (select) table is addressed only at valid indices by `get`.
        let need = ef.word_off(words_lower + words_upper);
        if need > ef.mmap.len() {
            return Err(Error::format(
                "Elias-Fano: data shorter than header implies",
            ));
        }
        Ok(ef)
    }

    #[inline]
    fn word_off(&self, idx: usize) -> usize {
        self.base + 16 + idx * 8
    }
    #[inline]
    fn word(&self, idx: usize) -> u64 {
        let off = self.word_off(idx);
        u64::from_le_bytes(self.mmap[off..off + 8].try_into().unwrap())
    }
    #[inline]
    fn lower(&self, i: usize) -> u64 {
        self.word(i)
    }
    #[inline]
    fn upper(&self, i: usize) -> u64 {
        self.word(self.words_lower + i)
    }
    #[inline]
    fn jump(&self, i: usize) -> u64 {
        self.word(self.words_lower + self.words_upper + i)
    }

    /// Number of values in the sequence.
    pub fn len(&self) -> u64 {
        self.count + 1
    }

    /// Whether the sequence is empty. (It never is in a valid `.bt`, but provided for
    /// API completeness alongside [`len`](Self::len).)
    pub fn is_empty(&self) -> bool {
        false
    }

    /// `ef.Get(i)`: the `i`-th value of the monotone sequence (a `.kv` byte offset).
    ///
    /// Panics if `i >= self.len()`; callers must bound-check (point-lookup does).
    pub fn get(&self, i: u64) -> u64 {
        // lower `l` bits live at bit position `i*l`
        let mut lower = 0u64;
        if self.l != 0 {
            let lower_bit = i * self.l;
            let idx64 = (lower_bit / 64) as usize;
            let shift = lower_bit % 64;
            lower = self.lower(idx64) >> shift;
            if shift > 0 {
                lower |= self.lower(idx64 + 1) << (64 - shift);
            }
        }
        // upper bits via the jump (select) table
        let jump_super_q = (i / EF_SUPERQ) * EF_SUPERQ_SIZE;
        let jump_inside = (i % EF_SUPERQ) / EF_Q;
        let idx64j = (jump_super_q + 1 + (jump_inside >> 1)) as usize;
        let shiftj = (32 * (jump_inside % 2)) as u32;
        let mask = 0xffff_ffffu64 << shiftj;
        let jump = self.jump(jump_super_q as usize) + ((self.jump(idx64j) & mask) >> shiftj);
        let mut curr_word = jump / 64;
        let mut window = self.upper(curr_word as usize) & (0xffff_ffff_ffff_ffffu64 << (jump % 64));
        let mut d = (i & EF_QMASK) as i64;
        loop {
            let bc = window.count_ones() as i64;
            if bc > d {
                break;
            }
            curr_word += 1;
            window = self.upper(curr_word as usize);
            d -= bc;
        }
        let sel = select64(window, d as u32) as u64;
        ((curr_word * 64 + sel - i) << self.l) | (lower & self.lower_mask)
    }
}

#[cfg(test)]
mod tests {
    use super::select64;

    #[test]
    fn select64_matches_definition() {
        assert_eq!(select64(0b1011, 0), 0);
        assert_eq!(select64(0b1011, 1), 1);
        assert_eq!(select64(0b1011, 2), 3);
        assert_eq!(select64(1u64 << 63, 0), 63);
    }
}
