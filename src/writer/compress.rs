//! Optional `seg` pattern compression (Phase 7).
//!
//! Produces a valid compressed seg file with a non-empty pattern dictionary. Because the
//! seg format is self-describing (it stores its own pattern + position dictionaries), any
//! *correct* set of patterns yields a file the reader and erigon both decompress — so we
//! choose patterns with a self-contained heuristic (frequent substrings) and cover each
//! word greedily, rather than reproducing erigon's suffix-array pipeline. Correctness is
//! guaranteed by round-trip; output is typically larger than erigon's optimal cover.
//!
//! Encoding per word matches `Getter::next`: a position code for `len+1`, then for each
//! covering pattern a `(distance+1)` position code and the pattern's Huffman code, a `0`
//! terminator, and finally the uncovered literal bytes (byte-aligned).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use super::bitwriter::BitWriter;
use super::huffman::{build_pattern_dict, build_position_dict};
use crate::error::{Error, Result};
use crate::varint::{put_uvarint, read_uvarint};

/// Tuning for the pattern dictionary.
const MIN_PATTERN_LEN: usize = 5; // must exceed the ~4-byte per-pattern encoding overhead
const MAX_PATTERN_LEN: usize = 128;
const MAX_PATTERNS: usize = 8192;
const SAMPLE_WORDS: u64 = 100_000;
const CANDIDATE_CAP: usize = 3_000_000;
const NGRAM_LENS: [usize; 4] = [64, 32, 16, 8];

/// A pattern dictionary with a trie for longest-match-at-position queries.
pub(crate) struct Dictionary {
    patterns: Vec<Vec<u8>>,
    nodes: Vec<TrieNode>,
}

#[derive(Default)]
struct TrieNode {
    children: HashMap<u8, u32>,
    pattern: Option<usize>, // index into `patterns` for a pattern ending here
}

impl Dictionary {
    fn new(patterns: Vec<Vec<u8>>) -> Dictionary {
        let mut d = Dictionary {
            patterns,
            nodes: vec![TrieNode::default()],
        };
        for pid in 0..d.patterns.len() {
            let mut node = 0u32;
            // borrow patterns bytes via index to satisfy the borrow checker
            let plen = d.patterns[pid].len();
            for bi in 0..plen {
                let b = d.patterns[pid][bi];
                let next = match d.nodes[node as usize].children.get(&b) {
                    Some(&n) => n,
                    None => {
                        let n = d.nodes.len() as u32;
                        d.nodes.push(TrieNode::default());
                        d.nodes[node as usize].children.insert(b, n);
                        n
                    }
                };
                node = next;
            }
            d.nodes[node as usize].pattern = Some(pid);
        }
        d
    }

    /// Longest pattern matching `word[i..]`, as `(pattern_id, len)`.
    fn longest_match_at(&self, word: &[u8], i: usize) -> Option<(usize, usize)> {
        let mut node = 0u32;
        let mut best: Option<(usize, usize)> = None;
        let mut j = i;
        while j < word.len() {
            match self.nodes[node as usize].children.get(&word[j]) {
                Some(&n) => node = n,
                None => break,
            }
            j += 1;
            if let Some(pid) = self.nodes[node as usize].pattern {
                best = Some((pid, j - i));
            }
        }
        best
    }

    /// Greedy non-overlapping cover: `(start, pattern_id)` in increasing start order.
    pub(crate) fn cover(&self, word: &[u8]) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < word.len() {
            if let Some((pid, plen)) = self.longest_match_at(word, i) {
                out.push((i, pid));
                i += plen;
            } else {
                i += 1;
            }
        }
        out
    }

    fn pattern_len(&self, pid: usize) -> usize {
        self.patterns[pid].len()
    }
}

/// Build a dictionary from a sample of words by counting frequent fixed-length n-grams.
fn build_dictionary(tmp_path: &Path) -> Result<Dictionary> {
    let mut counts: HashMap<Box<[u8]>, u64> = HashMap::new();
    let mut seen = 0u64;
    for_each_word(tmp_path, |word| {
        if seen >= SAMPLE_WORDS {
            return;
        }
        seen += 1;
        for &l in &NGRAM_LENS {
            if word.len() < l {
                continue;
            }
            let mut pos = 0;
            while pos + l <= word.len() {
                let key = &word[pos..pos + l];
                if counts.len() < CANDIDATE_CAP {
                    *counts.entry(key.into()).or_insert(0) += 1;
                } else if let Some(c) = counts.get_mut(key) {
                    *c += 1;
                }
                pos += 1;
            }
        }
    })?;

    // Score by (len - overhead) * (uses - 1); keep the top MAX_PATTERNS repeated ones.
    let mut scored: Vec<(u64, Box<[u8]>)> = counts
        .into_iter()
        .filter(|(k, c)| *c >= 2 && k.len() >= MIN_PATTERN_LEN && k.len() <= MAX_PATTERN_LEN)
        .map(|(k, c)| ((k.len() as u64 - 4) * (c - 1), k))
        .collect();
    scored.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.truncate(MAX_PATTERNS);
    let patterns: Vec<Vec<u8>> = scored.into_iter().map(|(_, k)| k.into_vec()).collect();
    Ok(Dictionary::new(patterns))
}

/// Compress the words buffered in `tmp_path` into the seg file `kv_path`.
pub(crate) fn compress_finish(
    tmp_path: &Path,
    kv_path: &Path,
    words_count: u64,
    empty_words_count: u64,
) -> Result<()> {
    let dict = build_dictionary(tmp_path)?;

    // Pass A: cover every word, accumulating position-symbol and pattern-use frequencies.
    let mut pos_uses: HashMap<u64, u64> = HashMap::new();
    let mut pattern_uses = vec![0u64; dict.patterns.len()];
    for_each_word(tmp_path, |word| {
        *pos_uses.entry(word.len() as u64 + 1).or_insert(0) += 1;
        *pos_uses.entry(0).or_insert(0) += 1;
        if word.is_empty() {
            return;
        }
        let mut last_start = 0usize;
        for &(start, pid) in &dict.cover(word) {
            *pos_uses.entry((start - last_start) as u64 + 1).or_insert(0) += 1;
            last_start = start;
            pattern_uses[pid] += 1;
        }
    })?;

    // Reindex used patterns (only used ones go in the dictionary) and build the codes.
    let used: Vec<usize> = (0..dict.patterns.len())
        .filter(|&p| pattern_uses[p] > 0)
        .collect();
    let used_uses: Vec<u64> = used.iter().map(|&p| pattern_uses[p]).collect();
    let pat_codes = build_pattern_dict(&used_uses);
    // orig pattern id -> (code, code_bits)
    let mut pat2code: HashMap<usize, (u64, u32)> = HashMap::with_capacity(used.len());
    let mut pattern_dict_bytes = Vec::new();
    for pc in &pat_codes {
        let orig = used[pc.index];
        pat2code.insert(orig, (pc.code, pc.code_bits));
        put_uvarint(&mut pattern_dict_bytes, pc.depth as u64);
        put_uvarint(&mut pattern_dict_bytes, dict.patterns[orig].len() as u64);
        pattern_dict_bytes.extend_from_slice(&dict.patterns[orig]);
    }

    // Position dictionary.
    let mut pos_pairs: Vec<(u64, u64)> = pos_uses.iter().map(|(&p, &u)| (p, u)).collect();
    pos_pairs.sort_unstable();
    let pos_entries = build_position_dict(&pos_pairs);
    let mut pos2code: HashMap<u64, (u64, u32)> = HashMap::with_capacity(pos_entries.len());
    let mut pos_dict_bytes = Vec::new();
    for e in &pos_entries {
        pos2code.insert(e.pos, (e.code, e.code_bits));
        put_uvarint(&mut pos_dict_bytes, e.depth as u64);
        put_uvarint(&mut pos_dict_bytes, e.pos);
    }

    // Pass B: write header + dictionaries, then re-cover each word and emit the stream.
    emit_pass(
        tmp_path,
        kv_path,
        &dict,
        &pat2code,
        &pos2code,
        words_count,
        empty_words_count,
        &pattern_dict_bytes,
        &pos_dict_bytes,
    )
}

/// Emit pass that actually writes the file (separated so the word callback can borrow the
/// output writer mutably).
#[allow(clippy::too_many_arguments)]
fn emit_pass(
    tmp_path: &Path,
    kv_path: &Path,
    dict: &Dictionary,
    pat2code: &HashMap<usize, (u64, u32)>,
    pos2code: &HashMap<u64, (u64, u32)>,
    words_count: u64,
    empty_words_count: u64,
    pattern_dict_bytes: &[u8],
    pos_dict_bytes: &[u8],
) -> Result<()> {
    let mut out = BufWriter::new(File::create(kv_path).map_err(|e| Error::io(kv_path, e))?);
    out.write_all(&words_count.to_be_bytes())
        .map_err(|e| Error::io(kv_path, e))?;
    out.write_all(&empty_words_count.to_be_bytes())
        .map_err(|e| Error::io(kv_path, e))?;
    out.write_all(&(pattern_dict_bytes.len() as u64).to_be_bytes())
        .map_err(|e| Error::io(kv_path, e))?;
    out.write_all(pattern_dict_bytes)
        .map_err(|e| Error::io(kv_path, e))?;
    out.write_all(&(pos_dict_bytes.len() as u64).to_be_bytes())
        .map_err(|e| Error::io(kv_path, e))?;
    out.write_all(pos_dict_bytes)
        .map_err(|e| Error::io(kv_path, e))?;

    let pos_code =
        |pos: u64| -> (u64, u32) { *pos2code.get(&pos).expect("position missing from dict") };
    let mut code_buf: Vec<u8> = Vec::new();
    let mut literals: Vec<u8> = Vec::new();
    let mut err: Option<Error> = None;
    for_each_word(tmp_path, |word| {
        if err.is_some() {
            return;
        }
        code_buf.clear();
        literals.clear();
        let mut bw = BitWriter::new(&mut code_buf);
        let (c, cb) = pos_code(word.len() as u64 + 1);
        bw.encode(c, cb);
        if !word.is_empty() {
            let mut last_start = 0usize;
            let mut last_uncovered = 0usize;
            for &(start, pid) in &dict.cover(word) {
                let (pc, pcb) = pat2code[&pid];
                let (dc, dcb) = pos_code((start - last_start) as u64 + 1);
                bw.encode(dc, dcb);
                bw.encode(pc, pcb);
                last_start = start;
                if start > last_uncovered {
                    literals.extend_from_slice(&word[last_uncovered..start]);
                }
                last_uncovered = start + dict.pattern_len(pid);
            }
            let (tc, tcb) = pos_code(0);
            bw.encode(tc, tcb);
            if word.len() > last_uncovered {
                literals.extend_from_slice(&word[last_uncovered..]);
            }
        }
        bw.flush();
        if let Err(e) = out
            .write_all(&code_buf)
            .and_then(|_| out.write_all(&literals))
        {
            err = Some(Error::io(kv_path, e));
        }
    })?;
    if let Some(e) = err {
        return Err(e);
    }
    out.flush().map_err(|e| Error::io(kv_path, e))
}

/// Invoke `f` for each word stored in the temp words file.
fn for_each_word(tmp_path: &Path, mut f: impl FnMut(&[u8])) -> Result<()> {
    let mut r = BufReader::new(File::open(tmp_path).map_err(|e| Error::io(tmp_path, e))?);
    let mut buf: Vec<u8> = Vec::new();
    while let Some(len) = read_uvarint(&mut r).map_err(|e| Error::io(tmp_path, e))? {
        let len = len as usize;
        buf.resize(len, 0);
        r.read_exact(&mut buf).map_err(|e| Error::io(tmp_path, e))?;
        f(&buf);
    }
    Ok(())
}
