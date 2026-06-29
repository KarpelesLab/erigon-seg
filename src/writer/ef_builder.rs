//! Builder for a serialized `eliasfano32.EliasFano` (the inverse of `eliasfano.rs`).
//!
//! Port of erigon-lib `NewEliasFano`/`AddOffset`/`Build`/`Write`. Offsets must be added
//! in non-decreasing order and there must be exactly `count` of them; the maximum offset
//! is bounded by `max_offset` (an over-estimate such as the data file size is fine).

const Q: u64 = 1 << 8; // 256
const Q_MASK: u64 = Q - 1;
const SUPERQ: u64 = 1 << 14; // 16384
const SUPERQ_MASK: u64 = SUPERQ - 1;
const SUPERQ_SIZE: u64 = 1 + (SUPERQ / Q) / 2; // 33

/// Accumulates a monotone offset sequence and serializes it as Elias-Fano.
pub(crate) struct EfBuilder {
    data: Vec<u64>,
    count: u64, // real_count - 1, matching the on-disk field
    u: u64,
    l: u64,
    lower_mask: u64,
    i: u64,
    words_lower: usize,
    words_upper: usize,
}

#[inline]
fn jump_size_words(count_plus_1: u64) -> u64 {
    let mut size = (count_plus_1 / SUPERQ) * SUPERQ_SIZE;
    if count_plus_1 % SUPERQ != 0 {
        size += 1 + (((count_plus_1 % SUPERQ) + Q - 1) / Q + 3) / 2;
    }
    size
}

impl EfBuilder {
    /// Create a builder for `count` offsets (must be > 0) in `[0, max_offset]`.
    pub(crate) fn new(count: u64, max_offset: u64) -> EfBuilder {
        assert!(count > 0, "EfBuilder requires count > 0");
        let stored = count - 1;
        let u = max_offset + 1;
        let count_plus_1 = stored + 1; // == count
        let l = if u / count_plus_1 == 0 { 0 } else { 63 - (u / count_plus_1).leading_zeros() as u64 };
        let lower_mask = if l == 0 { 0 } else { (1u64 << l) - 1 };
        let words_lower = (((count_plus_1) * l).div_ceil(64) + 1) as usize;
        let words_upper = (count_plus_1 + (u >> l)).div_ceil(64) as usize;
        let total = words_lower + words_upper + jump_size_words(count_plus_1) as usize;
        EfBuilder {
            data: vec![0u64; total],
            count: stored,
            u,
            l,
            lower_mask,
            i: 0,
            words_lower,
            words_upper,
        }
    }

    /// Add the next offset (must be >= all previous and <= `max_offset`).
    pub(crate) fn add_offset(&mut self, offset: u64) {
        if self.l != 0 {
            self.set_lower_bits(self.i * self.l, offset & self.lower_mask);
        }
        let pos = (offset >> self.l) + self.i;
        // upper region starts at word index `words_lower`
        let word = self.words_lower + (pos / 64) as usize;
        self.data[word] |= 1u64 << (pos % 64);
        self.i += 1;
    }

    /// `setBits(lowerBits, start, value)`, with a guard for the Go `>>64 == 0` quirk.
    #[inline]
    fn set_lower_bits(&mut self, start: u64, value: u64) {
        let idx = (start >> 6) as usize;
        let shift = (start & 63) as u32;
        self.data[idx] |= value << shift;
        if shift != 0 {
            self.data[idx + 1] |= value >> (64 - shift);
        }
    }

    /// Construct the super-Q jump (select) table. Panics if a jump offset exceeds 32
    /// bits (unreachable for any realistic universe).
    pub(crate) fn build(&mut self) {
        let upper_base = self.words_lower;
        let jump_base = self.words_lower + self.words_upper;
        let (mut c, mut last_super_q) = (0u64, 0u64);
        for i in 0..self.words_upper as u64 {
            let mut word = self.data[upper_base + i as usize];
            while word != 0 {
                let b = word.trailing_zeros() as u64;
                if (c & SUPERQ_MASK) == 0 {
                    last_super_q = i * 64 + b;
                    self.data[jump_base + ((c / SUPERQ) * SUPERQ_SIZE) as usize] = last_super_q;
                }
                if (c & Q_MASK) != 0 {
                    c += 1;
                    word &= word - 1;
                    continue;
                }
                let offset = i * 64 + b - last_super_q;
                assert!(offset < (1 << 32), "eliasfano: jump offset exceeds 32 bits");
                let jump_super_q = (c / SUPERQ) * SUPERQ_SIZE;
                let jump_inside = (c % SUPERQ) / Q;
                let idx64 = (jump_super_q + 1 + (jump_inside >> 1)) as usize;
                let shift = 32 * (jump_inside % 2);
                let mask = 0xffff_ffffu64 << shift;
                self.data[jump_base + idx64] = (self.data[jump_base + idx64] & !mask) | (offset << shift);
                c += 1;
                word &= word - 1;
            }
        }
    }

    /// Serialize: `count(8 BE) | u(8 BE) | data words (native LE)`.
    pub(crate) fn write_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.count.to_be_bytes());
        out.extend_from_slice(&self.u.to_be_bytes());
        for w in &self.data {
            out.extend_from_slice(&w.to_le_bytes());
        }
    }

    /// Total serialized length in bytes (`16 + 8 * data_words`).
    pub(crate) fn serialized_len(&self) -> usize {
        16 + self.data.len() * 8
    }
}
