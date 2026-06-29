//! Writer for `seg` `.kv` files (the no-pattern fast path).
//!
//! Port of erigon-lib `compressNoWordPatterns` + `buildAndWritePosDict`: an empty
//! pattern dictionary and a position-Huffman dictionary that encodes word lengths, with
//! word bytes stored as literals. Output is a valid V0 seg file that this crate's
//! [`Seg`](crate::Seg) reader and Erigon both decompress.
//!
//! Words are buffered to a temporary file as they are added (so arbitrarily large
//! inputs stream through constant memory); [`finish`](SegWriter::finish) makes one pass
//! over that file to encode the output.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use super::bitwriter::BitWriter;
use super::huffman::build_position_dict;
use crate::error::{Error, Result};
use crate::varint::{put_uvarint, read_uvarint};

/// Builds a `seg` `.kv` file from a sequence of words.
pub struct SegWriter {
    kv_path: PathBuf,
    tmp_path: PathBuf,
    tmp: BufWriter<File>,
    words_count: u64,
    empty_words_count: u64,
    /// `position -> uses` frequency map for the Huffman dictionary, accumulated as words
    /// are added (a word of length `l` contributes one `l+1` and one terminator `0`).
    pos_uses: HashMap<u64, u64>,
    finished: bool,
}

impl SegWriter {
    /// Create a writer that will produce `kv_path`. A sibling temporary file
    /// (`<kv_path>.words.tmp`) holds buffered words until [`finish`](SegWriter::finish).
    pub fn create(kv_path: impl AsRef<Path>) -> Result<SegWriter> {
        let kv_path = kv_path.as_ref().to_path_buf();
        let tmp_path = with_suffix(&kv_path, ".words.tmp");
        let tmp = BufWriter::new(File::create(&tmp_path).map_err(|e| Error::io(&tmp_path, e))?);
        Ok(SegWriter {
            kv_path,
            tmp_path,
            tmp,
            words_count: 0,
            empty_words_count: 0,
            pos_uses: HashMap::new(),
            finished: false,
        })
    }

    /// Append one word. Words are emitted in the order added.
    pub fn add_word(&mut self, word: &[u8]) -> Result<()> {
        let l = word.len() as u64;
        *self.pos_uses.entry(l + 1).or_insert(0) += 1;
        *self.pos_uses.entry(0).or_insert(0) += 1;
        self.words_count += 1;
        if l == 0 {
            self.empty_words_count += 1;
        }
        let mut hdr = Vec::with_capacity(5);
        put_uvarint(&mut hdr, l);
        self.tmp.write_all(&hdr).map_err(|e| Error::io(&self.tmp_path, e))?;
        self.tmp.write_all(word).map_err(|e| Error::io(&self.tmp_path, e))?;
        Ok(())
    }

    /// Number of words added so far.
    pub fn words_count(&self) -> u64 {
        self.words_count
    }

    /// Finalize: write `kv_path` and remove the temporary file.
    pub fn finish(mut self) -> Result<()> {
        self.finished = true;
        self.tmp.flush().map_err(|e| Error::io(&self.tmp_path, e))?;

        // Build the position dictionary and the (depth, pos) serialization.
        let mut pairs: Vec<(u64, u64)> = self.pos_uses.iter().map(|(&p, &u)| (p, u)).collect();
        // Deterministic input order (build_position_dict re-sorts canonically anyway).
        pairs.sort_unstable();
        let entries = build_position_dict(&pairs);
        let mut pos2code: HashMap<u64, (u64, u32)> = HashMap::with_capacity(entries.len());
        let mut pos_dict_bytes = Vec::new();
        for e in &entries {
            pos2code.insert(e.pos, (e.code, e.code_bits));
            put_uvarint(&mut pos_dict_bytes, e.depth as u64);
            put_uvarint(&mut pos_dict_bytes, e.pos);
        }

        let mut out = BufWriter::new(File::create(&self.kv_path).map_err(|e| Error::io(&self.kv_path, e))?);
        let w = |out: &mut BufWriter<File>, b: &[u8]| out.write_all(b).map_err(|e| Error::io(&self.kv_path, e));
        // Header: words_count | empty_words_count | patterns_size(=0) | pos_dict_size | pos_dict
        w(&mut out, &self.words_count.to_be_bytes())?;
        w(&mut out, &self.empty_words_count.to_be_bytes())?;
        w(&mut out, &0u64.to_be_bytes())?;
        w(&mut out, &(pos_dict_bytes.len() as u64).to_be_bytes())?;
        w(&mut out, &pos_dict_bytes)?;

        // Pass over the buffered words: encode the length code, terminator, then literals.
        let code_of = |pos: u64| -> (u64, u32) {
            *pos2code.get(&pos).expect("position missing from dictionary")
        };
        let mut tmp_in = BufReader::new(File::open(&self.tmp_path).map_err(|e| Error::io(&self.tmp_path, e))?);
        let mut word_buf: Vec<u8> = Vec::new();
        let mut code_buf: Vec<u8> = Vec::new();
        while let Some(len) = read_uvarint(&mut tmp_in).map_err(|e| Error::io(&self.tmp_path, e))? {
            let len = len as usize;
            word_buf.resize(len, 0);
            tmp_in.read_exact(&mut word_buf).map_err(|e| Error::io(&self.tmp_path, e))?;

            code_buf.clear();
            let mut bw = BitWriter::new(&mut code_buf);
            let (c, cb) = code_of(len as u64 + 1);
            bw.encode(c, cb);
            if len != 0 {
                let (c0, cb0) = code_of(0);
                bw.encode(c0, cb0);
            }
            bw.flush();
            w(&mut out, &code_buf)?;
            if len != 0 {
                w(&mut out, &word_buf)?;
            }
        }
        out.flush().map_err(|e| Error::io(&self.kv_path, e))?;
        let _ = std::fs::remove_file(&self.tmp_path);
        Ok(())
    }
}

impl Drop for SegWriter {
    fn drop(&mut self) {
        // Clean up the temp file if finish() was never called (e.g. on error/panic).
        if !self.finished {
            let _ = std::fs::remove_file(&self.tmp_path);
        }
    }
}

/// Append a literal suffix to a path's filename (`foo.kv` + `.words.tmp` ->
/// `foo.kv.words.tmp`), used for sidecar temp files.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}
