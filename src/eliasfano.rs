//! Reader for a serialized `eliasfano32.EliasFano` — the monotone array of `.kv`
//! byte offsets stored inside a `.bt` index.
//!
//! On disk the section is: `count: u64` (big-endian, stored as `real_count - 1`),
//! `u: u64` (big-endian, `max_offset + 1`), then the bit-packed `u64` words in native
//! little-endian order. We only ever read, so this mirrors erigon-lib's `ReadEliasFano`
//! / `Get`, including the `select`-table ("jump") fast path.

use std::sync::Arc;

use memmap2::Mmap;

use crate::error::{Error, Result};

const EF_LOG2Q: u64 = 8;
const EF_Q: u64 = 1 << EF_LOG2Q; // 256
const EF_QMASK: u64 = EF_Q - 1;
const EF_SUPERQ: u64 = 1 << 14; // 16384
const EF_SUPERQ_SIZE: u64 = 1 + (EF_SUPERQ / EF_Q) / 2; // 33

/// Portable `select64`: index (0-based) of the `k`-th set bit in `x`, by clearing the
/// lowest set bit `k` times. Up to 63 iterations.
#[inline]
fn select64_fallback(mut x: u64, k: u32) -> u32 {
    for _ in 0..k {
        x &= x - 1; // clear lowest set bit
    }
    x.trailing_zeros()
}

/// `select64` as a single `pdep` + `tzcnt`.
///
/// # Safety
/// The caller must have established that the CPU supports BMI2.
#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
#[target_feature(enable = "bmi2")]
#[inline]
unsafe fn select64_bmi2(x: u64, k: u32) -> u32 {
    // The intrinsic is safe to call here: its BMI2 requirement is carried by this
    // function's `target_feature` and discharged by the caller.
    std::arch::x86_64::_pdep_u64(1u64 << k, x).trailing_zeros()
}

/// A read-only view of an Elias-Fano monotone sequence.
///
/// Owns the memory map of the file it lives in and indexes into it at a fixed `base`
/// (0 for the legacy `.bt` layout, or the footer's `ef_offset` for the newer one).
pub struct EliasFano {
    mmap: Arc<Mmap>,
    base: usize,
    count: u64, // stored count == real_count - 1
    l: u64,
    lower_mask: u64,
    words_lower: usize,
    words_upper: usize,
    /// Whether [`select64_bmi2`] may be called — resolved once here rather than on every
    /// `get`, so the hot path carries no feature test. Only meaningful on x86-64; other
    /// architectures always take the portable `select64`, so the field does not exist.
    #[cfg(target_arch = "x86_64")]
    bmi2: bool,
}

impl EliasFano {
    /// Parse the Elias-Fano header located at `base` within `mmap`.
    pub(crate) fn open(mmap: Arc<Mmap>, base: usize) -> Result<EliasFano> {
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
            #[cfg(target_arch = "x86_64")]
            bmi2: std::is_x86_feature_detected!("bmi2"),
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
    ///
    /// A point lookup calls this once per search comparison, so the BMI2 decision is
    /// made here — one perfectly-predicted branch on a stored flag — rather than inside
    /// `select64`. Testing the feature per call cost about 2.8x on `select64` itself and
    /// blocked the surrounding function from being compiled as BMI2 code at all.
    #[inline]
    pub fn get(&self, i: u64) -> u64 {
        #[cfg(target_arch = "x86_64")]
        if self.bmi2 {
            // SAFETY: `self.bmi2` was set from a runtime BMI2 check at open time.
            #[allow(unsafe_code)]
            return unsafe { self.get_bmi2(i) };
        }
        self.get_impl(i, select64_fallback)
    }

    /// `get` compiled with BMI2 enabled throughout, so `pdep` inlines and the rest of
    /// the body gets BMI2 codegen too.
    ///
    /// # Safety
    /// The caller must have established that the CPU supports BMI2.
    #[cfg(target_arch = "x86_64")]
    #[allow(unsafe_code)]
    #[target_feature(enable = "bmi2")]
    unsafe fn get_bmi2(&self, i: u64) -> u64 {
        // SAFETY: this function's `target_feature` guarantees BMI2 for `select64_bmi2`.
        self.get_impl(i, |x, k| unsafe { select64_bmi2(x, k) })
    }

    /// The body of `get`, generic over how the final `select64` is performed.
    #[inline(always)]
    fn get_impl(&self, i: u64, select64: impl Fn(u64, u32) -> u32) -> u64 {
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
    use super::select64_fallback;

    #[test]
    fn select64_matches_definition() {
        assert_eq!(select64_fallback(0b1011, 0), 0);
        assert_eq!(select64_fallback(0b1011, 1), 1);
        assert_eq!(select64_fallback(0b1011, 2), 3);
        assert_eq!(select64_fallback(1u64 << 63, 0), 63);
    }

    /// The BMI2 path is only reachable on hardware that has it, so pin it against the
    /// portable one over a wide spread of inputs rather than trusting them to match.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn select64_bmi2_matches_fallback() {
        if !std::is_x86_feature_detected!("bmi2") {
            eprintln!("no BMI2 here; skipping");
            return;
        }
        let mut st = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..200_000 {
            st ^= st >> 12;
            st ^= st << 25;
            st ^= st >> 27;
            let x = st.wrapping_mul(0x2545_F491_4F6C_DD1D);
            let pc = x.count_ones();
            if pc == 0 {
                continue;
            }
            for k in [0, pc / 2, pc - 1] {
                // SAFETY: guarded by the BMI2 check above.
                #[allow(unsafe_code)]
                let got = unsafe { super::select64_bmi2(x, k) };
                assert_eq!(got, select64_fallback(x, k), "x={x:#x} k={k}");
            }
        }
        // Edge cases: single bit at each end, and all bits set.
        for (x, k, want) in [(1u64, 0, 0), (1u64 << 63, 0, 63), (u64::MAX, 63, 63)] {
            #[allow(unsafe_code)]
            let got = unsafe { super::select64_bmi2(x, k) };
            assert_eq!(got, want);
        }
    }
}
