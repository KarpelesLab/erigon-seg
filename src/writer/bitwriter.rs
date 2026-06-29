//! LSB-first bit packing — the exact inverse of the reader's `Getter` bit reads.
//!
//! Port of erigon-lib `seg.BitWriter`. Bits of each code are emitted low bit first into
//! the current byte; a full byte is pushed to the sink. [`flush`](BitWriter::flush)
//! pads the partial byte with zero bits, which is how seg aligns each word boundary to a
//! byte so the `.bt` index can store byte offsets.

/// Accumulates Huffman code bits and appends finished bytes to an output buffer.
pub(crate) struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    output_bits: u32,
    output_byte: u8,
}

impl<'a> BitWriter<'a> {
    pub(crate) fn new(out: &'a mut Vec<u8>) -> BitWriter<'a> {
        BitWriter { out, output_bits: 0, output_byte: 0 }
    }

    /// Emit the low `code_bits` bits of `code`, low bit first.
    pub(crate) fn encode(&mut self, mut code: u64, mut code_bits: u32) {
        while code_bits > 0 {
            let bits_used = if self.output_bits + code_bits > 8 { 8 - self.output_bits } else { code_bits };
            let mask = (1u64 << bits_used) - 1;
            self.output_byte |= ((code & mask) << self.output_bits) as u8;
            code >>= bits_used;
            code_bits -= bits_used;
            self.output_bits += bits_used;
            if self.output_bits == 8 {
                self.out.push(self.output_byte);
                self.output_bits = 0;
                self.output_byte = 0;
            }
        }
    }

    /// Flush a partial byte (zero-padded), aligning the stream to a byte boundary.
    pub(crate) fn flush(&mut self) {
        if self.output_bits > 0 {
            self.out.push(self.output_byte);
            self.output_bits = 0;
            self.output_byte = 0;
        }
    }
}
