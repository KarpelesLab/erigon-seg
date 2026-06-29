# Roadmap: write + merge

**Status:** the reader (Phase 0) and the full write + merge critical path (Phases 1–6)
and hardening (Phase 8) are **complete and verified against real Erigon v1.1 files**. The
only remaining item is Phase 7 (optional `seg` pattern compression for output size
parity); files written today are valid and interoperable, just larger than erigon's.

| Phase | What | Status |
|------:|------|--------|
| 0 | Reader (`.kv`/`.bt`/`.kvei`, query, bloom) | ✅ done |
| 1 | Writer primitives (BitWriter, Huffman, EF builder) | ✅ done |
| 2 | `.kv` writer (no-pattern path) | ✅ done |
| 3 | `.bt` writer (legacy + footer) | ✅ done |
| 4 | `.kvei` bloom writer | ✅ done |
| 5 | `DomainWriter` | ✅ done |
| 6 | Merge (newest-wins, drop deletions at from=0) | ✅ done |
| 7 | `seg` pattern compression (size parity) | ⬜ optional, not started |
| 8 | Hardening (real-file write/merge tests, docs) | ✅ done |

This document plans the path to a full read/write/merge library.

A key finding from Erigon's source (`db/seg/parallel_compress.go`) shapes the plan:
`compressNoWordPatterns` produces **fully valid, Erigon-readable `.kv` files with an
empty pattern dictionary**. So a correct writer — and therefore merge — does *not*
require the heavy `seg` compressor (suffix array + patricia tree + DP cover). That stays
an optional **size-parity** pass (Phase 7). Everything in Phases 1–6 yields correct,
interoperable files; they are just larger than Erigon's pattern-compressed output until
Phase 7 lands.

Legend — complexity/risk: ⬤ low · ⬤⬤ medium · ⬤⬤⬤ high.

---

## Phase 0 — Reader ✅ (done)

`seg` `.kv` decompression, `.bt` (Elias-Fano, legacy + footer), `.kvei` (holiman bloom),
murmur3, `KvReader` (point lookup + iterate + salt). Verified end-to-end on real files
incl. a 12 GB / 338 M-key snapshot.

---

## Phase 1 — Shared writer primitives ⬤⬤

Foundations reused by every writer below. No file formats yet — pure building blocks
with exhaustive unit tests.

- **`BitWriter`** — LSB-first bit packing; `encode(code, code_bits)` + `flush()`. Must be
  the exact inverse of the reader's `Getter::next_pos` bit reads.
  *Ref:* `compress.go` `BitWriter` (~L784).
- **Canonical Huffman builder** — frequency map → `(depth, symbol)` codewords, assigning
  codes so the reader's `build_pos_table` / `build_pattern_table` DFS reconstructs them.
  *Ref:* `compress.go` `Position`/`PositionHuff`/`SetDepth`/`AddZero`/`positionListCmp`,
  and the build loop in `buildAndWritePosDict` (`parallel_compress.go` ~L868).
- **Elias-Fano builder** — `new(count, max_offset)`, `add_offset(o)`, `build()` (upper
  bits + the super-Q jump/select table), `write()`. Inverse of `eliasfano.rs`.
  *Ref:* `elias_fano.go` `NewEliasFano`/`AddOffset`/`Build`/`Write` (L97–L268, L757).

**Verification:** round-trip each primitive against the existing reader in-memory (build
EF → `EliasFano::get` matches inputs; Huffman encode → decode tables agree).

---

## Phase 2 — `.kv` writer, no-pattern path ⬤⬤  *(depends on Phase 1)*

Port `compressNoWordPatterns` + `buildAndWritePosDict`: an empty pattern dictionary and a
position-Huffman dictionary that encodes word lengths; word bytes stored as literals.

Body layout (V0, no header — simplest and what existing v1.1 files use):
`words_count(8 BE) | empty_words_count(8 BE) | patterns_size=0 (8 BE) | pos_dict | words`.
Per word: Huffman-encode `len+1`; if non-empty, encode terminator `0`, `flush`, append
raw bytes.

- API: `SegWriter::create(path)` → `add_word(&[u8])` → `finish()`.
- Two passes (count/frequencies, then encode) like Erigon, or one pass buffering words.

**Verification:** `add_word` a corpus → reopen with our `Seg` → assert identical words;
round-trip random/edge cases (empty words, 1-byte, large). *Stretch:* have a real Erigon
binary decompress our output.

---

## Phase 3 — `.bt` writer ⬤⬤  *(depends on Phase 1 EF builder)*

Support **both** on-disk layouts, selectable via a writer option (default: footer, to
match current Erigon output):

- **Legacy** = just the serialized Elias-Fano of key offsets (the reader and Erigon both
  treat trailing B-tree nodes as optional). Walk the `.kv` keys, record each key's word
  offset, `add_offset`, `build`, `write`.
- **Footer** = `[0x01][nodes][EF][footer][anchor]`: the di-nodes blob (page-aligned EF),
  the EF section aligned to `btEFAlign` (4096), then the variable footer
  (`keys_count | M | ef_offset`) and the fixed 16-byte anchor (`footer_len | flags |
  version | magic="erigon\0\0"`). Enables co-located binary search on huge cold files.
  *Ref:* `btree_index.go` `BtIndexWriter`/`AddKey`/`Build`, `footer.go` `Footer::Encode`.

- API: `BtWriter` with a `BtLayout::{Legacy, Footer}` option; `build_bt(kv, out, layout)`
  and/or folded into the high-level writer.

**Verification:** for both layouts, build a `.bt` from a real `.kv` and compare
`EliasFano::get(i)` for all `i` against the real `.bt`'s offsets (must match exactly); the
footer build should additionally byte-match the real footer/anchor and re-open through our
own footer reader.

---

## Phase 4 — `.kvei` writer ⬤⬤  *(depends on murmur3; independent of 2–3)*

Write the holiman `bloomfilter/v2` layout our reader and Erigon both read: magic +
`k,n,m` + keys + bit array + trailing hash. Replicate `NewOptimal(n, p)`'s `m`/`k`
formulas (`k=3` on observed files), generate the per-filter key salts, then `AddHash`
(`murmur3 h1(key, salt)`) for every key.

- API: `build_kvei(keys_iter, salt, out_path)`.
- Salt comes from the caller (`Salt::Known` / `salt_from_file`).

**Verification:** build from a real `.kv`'s keys with the real salt; assert *no false
negatives* on all keys and that membership results match the real `.kvei` on a large
sample.

---

## Phase 5 — High-level `DomainWriter` ⬤  *(depends on 2–4)*

One call that consumes sorted `(key, value)` pairs and emits the triple: writes `.kv`
(alternating key/value words), then builds `.bt` and (if a salt is given) `.kvei` in the
same pass over the keys.

- API: `DomainWriter::create(base_path, opts)` → `add(key, value)` → `finish()`.
- Enforce/verify strictly increasing keys; surface duplicates as an error.
- Filenames follow `…<from>-<to>.{kv,bt,kvei}`.

**Verification:** generate a synthetic domain → open with `KvReader` → `get`/`iter`
round-trip; salt validates.

---

## Phase 6 — Merge ⬤⬤  *(depends on 5; reuses the reader)*

K-way merge of N input domain files into one wider-range file. Inputs are sorted; on
duplicate keys the **newest** file wins (matches `KvReader`'s query-time newest-wins and
Erigon's step-range semantics). Stream merged pairs straight into `DomainWriter`.

- API: `merge(&[input_kv_paths], out_base, opts)`; derive `<from>-<to>` from inputs.
- Streaming (heap of getters), constant memory regardless of file size.
- **Tombstones — drop deleted entries:** a key whose *newest* occurrence is a deletion is
  omitted from the output entirely (not carried forward). ⚠️ Confirm the exact deletion
  encoding against Erigon before finalizing: an empty (0-byte) value is **not** by itself
  a tombstone — real latest-state `.kv` files legitimately store empty values (e.g. an
  account with zero nonce/balance and no code). Identify the true deletion marker (or
  pass a caller-supplied `is_deleted(value)` predicate) so we never drop live keys.

**Verification:** merge real adjacent snapshots (e.g. `2256-2257` + `2256-2258`); for a
key sample, `merged.get(k)` must equal the newest input's `get(k)`; merged key set =
union; output re-reads cleanly.

---

## Phase 7 — `seg` pattern compression (size parity) ⬤⬤⬤  *(optional; depends on 2)*

Bring output size in line with Erigon by porting the real compressor. Large and
self-contained; can land long after merge works.

- Superstring sampling (`AddWord`/`sampledSuperstring`/`advanceScan`).
- **SAIS suffix array** over superstrings → repeated-substring pattern candidates with
  scores (`extractPatternsInSuperstrings`, `sais/sais.go`).
- Dictionary reduction/scoring (`DictionaryBuilder`).
- **Patricia tree + Aho-Corasick** matcher (`patricia/`).
- Per-word **min-cost cover** DP (`coverWordByPatterns`, `DynamicCell`).
- Pattern + position Huffman; re-encode words (covering positions, patterns, uncovered
  literals) — `compressWithPatternCandidates`.

**Verification:** round-trip equality with the reader; compression ratio within a few
percent of Erigon on the same corpus; differential test vs. real files.

---

## Phase 8 — Hardening & parity ⬤⬤  *(continuous)*

- Differential round-trip fuzzing (write → read → compare) across word-size/shape edge
  cases; `cargo fuzz` targets for the writer + merge.
- Optional `.kv` V1 header + page-level compression *writing* (reader already parses V1).
- Cross-tool validation: a small harness that shells out to an Erigon build to read files
  we wrote (and vice-versa), gated like `tests/real_files.rs`.
- Docs/examples for the write + merge APIs.

---

## Dependency graph

```
Phase 1 (primitives)
 ├─ Phase 2 (.kv writer) ─┐
 ├─ Phase 3 (.bt writer) ─┼─ Phase 5 (DomainWriter) ─ Phase 6 (merge)
 └─ Phase 4 (.kvei) ──────┘
Phase 7 (pattern compression)  ← optional, plugs into Phase 2/5
Phase 8 (hardening)            ← continuous
```

**Critical path to "write + merge" (correct, interoperable):** 1 → 2 → 3 → 5 → 6, with 4
sliced in before 5. Phase 7 is a later size-parity upgrade, not a blocker.
