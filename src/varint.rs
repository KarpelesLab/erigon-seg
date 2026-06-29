//! Go-compatible `binary.Uvarint` decoding (LEB128 unsigned), used by the seg
//! pattern/position dictionaries.

/// Decode a Go `binary.Uvarint`: LEB128 unsigned. Returns `(value, bytes_read)`,
/// or `(0, 0)` on overflow/truncation.
pub(crate) fn uvarint(b: &[u8]) -> (u64, usize) {
    let mut x = 0u64;
    let mut s = 0u32;
    for (i, &c) in b.iter().enumerate() {
        if i == 10 {
            return (0, 0); // overflow: more than 10 bytes
        }
        if c < 0x80 {
            if i == 9 && c > 1 {
                return (0, 0); // overflow: 64th bit set with extra data
            }
            return (x | (c as u64) << s, i + 1);
        }
        x |= ((c & 0x7f) as u64) << s;
        s += 7;
    }
    (0, 0) // truncated: ran out of bytes mid-value
}

#[cfg(test)]
mod tests {
    use super::uvarint;

    #[test]
    fn uvarint_basics() {
        assert_eq!(uvarint(&[0x00]), (0, 1));
        assert_eq!(uvarint(&[0x01]), (1, 1));
        assert_eq!(uvarint(&[0x7f]), (127, 1));
        assert_eq!(uvarint(&[0x80, 0x01]), (128, 2));
        assert_eq!(uvarint(&[0xff, 0x01]), (255, 2));
        assert_eq!(uvarint(&[0xac, 0x02]), (300, 2));
    }

    #[test]
    fn uvarint_truncation_and_overflow() {
        // truncated (high bit set, no continuation)
        assert_eq!(uvarint(&[0x80]), (0, 0));
        // 11 continuation bytes -> overflow
        assert_eq!(uvarint(&[0x80; 11]), (0, 0));
    }
}
