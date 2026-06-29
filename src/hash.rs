//! MurmurHash3 x64 128-bit, as Erigon hashes keys for the existence filter.
//!
//! Erigon computes `murmur3.Sum128WithSeed(key, salt)` and feeds the first 64-bit
//! half (`h1`) to the `.kvei` filter. This is a port of `spaolacci/murmur3`, where the
//! 32-bit seed initialises both 64-bit lanes.

const C1: u64 = 0x87c3_7b91_1142_53d5;
const C2: u64 = 0x4cf5_ad43_2745_937f;

/// MurmurHash3 x64 128-bit, returning only the first 64-bit half (`h1`) — the value
/// Erigon feeds to the existence filter as the hashed key.
pub fn murmur3_x64_128_h1(data: &[u8], seed: u32) -> u64 {
    let mut h1 = seed as u64;
    let mut h2 = seed as u64;

    let nblocks = data.len() / 16;
    for i in 0..nblocks {
        let b = &data[i * 16..i * 16 + 16];
        let mut k1 = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let mut k2 = u64::from_le_bytes(b[8..16].try_into().unwrap());
        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(31);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;
        h1 = h1.rotate_left(27);
        h1 = h1.wrapping_add(h2);
        h1 = h1.wrapping_mul(5).wrapping_add(0x52dc_e729);
        k2 = k2.wrapping_mul(C2);
        k2 = k2.rotate_left(33);
        k2 = k2.wrapping_mul(C1);
        h2 ^= k2;
        h2 = h2.rotate_left(31);
        h2 = h2.wrapping_add(h1);
        h2 = h2.wrapping_mul(5).wrapping_add(0x3849_5ab5);
    }

    let tail = &data[nblocks * 16..];
    let mut k1 = 0u64;
    let mut k2 = 0u64;
    let tl = tail.len();
    if tl >= 15 {
        k2 ^= (tail[14] as u64) << 48;
    }
    if tl >= 14 {
        k2 ^= (tail[13] as u64) << 40;
    }
    if tl >= 13 {
        k2 ^= (tail[12] as u64) << 32;
    }
    if tl >= 12 {
        k2 ^= (tail[11] as u64) << 24;
    }
    if tl >= 11 {
        k2 ^= (tail[10] as u64) << 16;
    }
    if tl >= 10 {
        k2 ^= (tail[9] as u64) << 8;
    }
    if tl >= 9 {
        k2 ^= tail[8] as u64;
        k2 = k2.wrapping_mul(C2);
        k2 = k2.rotate_left(33);
        k2 = k2.wrapping_mul(C1);
        h2 ^= k2;
    }
    if tl >= 8 {
        k1 ^= (tail[7] as u64) << 56;
    }
    if tl >= 7 {
        k1 ^= (tail[6] as u64) << 48;
    }
    if tl >= 6 {
        k1 ^= (tail[5] as u64) << 40;
    }
    if tl >= 5 {
        k1 ^= (tail[4] as u64) << 32;
    }
    if tl >= 4 {
        k1 ^= (tail[3] as u64) << 24;
    }
    if tl >= 3 {
        k1 ^= (tail[2] as u64) << 16;
    }
    if tl >= 2 {
        k1 ^= (tail[1] as u64) << 8;
    }
    if tl >= 1 {
        k1 ^= tail[0] as u64;
        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(31);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;
    }

    h1 ^= data.len() as u64;
    h2 ^= data.len() as u64;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    h1 = fmix64(h1);
    h2 = fmix64(h2);
    h1 = h1.wrapping_add(h2);
    // h2 = h2.wrapping_add(h1); // not needed: we only return h1
    h1
}

#[inline]
fn fmix64(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^= k >> 33;
    k
}

#[cfg(test)]
mod tests {
    use super::murmur3_x64_128_h1;

    #[test]
    fn reference_vectors() {
        // Authoritative spaolacci/murmur3 vectors (h1 = first 64-bit half).
        assert_eq!(murmur3_x64_128_h1(b"hello", 0), 0xcbd8_a7b3_41bd_9b02);
        assert_eq!(
            murmur3_x64_128_h1(b"hello, world", 0),
            0x342f_ac62_3a5e_bc8e
        );
        assert_eq!(
            murmur3_x64_128_h1(b"19 Jan 2038 at 3:14:07 AM", 0),
            0xb89e_5988_b737_affc
        );
        assert_eq!(murmur3_x64_128_h1(b"hello", 1), 0xa78d_dff5_adae_8d10);
        assert_eq!(murmur3_x64_128_h1(b"hello", 0x2a), 0xc4b8_b3c9_60af_6f08);
        assert_eq!(
            murmur3_x64_128_h1(b"The quick brown fox jumps over the lazy dog.", 0),
            0xcd99_481f_9ee9_02c9
        );
    }

    #[test]
    fn empty_is_zero_and_deterministic() {
        // MurmurHash3_x64_128("", seed=0) = (0, 0), so h1 == 0.
        assert_eq!(murmur3_x64_128_h1(b"", 0), 0);
        let a = murmur3_x64_128_h1(b"0123456789abcdef0123", 7);
        assert_eq!(a, murmur3_x64_128_h1(b"0123456789abcdef0123", 7));
        assert_ne!(a, murmur3_x64_128_h1(b"0123456789abcdef0123", 8));
    }
}
