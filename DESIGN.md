# Design

Architecture, performance techniques, and divergences from C zstd. Each
section explains what zrip does, how it differs, and why.


## Encode pipeline

### Fast strategy (`crates/encode/src/fast.rs`)

Port of C zstd's `ZSTD_compressBlock_fast_noDict_generic` 4-cursor match finder.
The pipeline probes positions ip0, ip1=ip0+1, ip2=ip0+step, ip3=ip2+1 per
iteration, reusing hash computations across shifts. Amortizes loop overhead
and branch mispredictions vs a single-position-per-iteration approach. Rep
offset is checked at the step-ahead position (ip2) only.

Rep2 post-match chaining (`rep2_match_loop`): after any match, immediately
checks `rep_offset2` for consecutive rep matches (zero-literal-length
sequences). Chains matches without re-entering the main loop. 1-byte
backward extension on rep matches (C zstd style), multi-byte backward
extension on regular matches. Backward extension may reach into earlier
blocks of the window, as in C zstd.

Const-generic dispatch on minimum match length via
`compress_fast_block_impl::<HASH_LOG, MLS>`:

| MLS | Hash function | Used at levels |
|-----|---------------|----------------|
| 4   | `hash4` (PRIME32_1) | custom params only |
| 5   | `hash5` (PRIME64_1) | custom params only |
| 6   | `hash6` (PRIME64_1) | -8..2 |
| 7   | `hash7` (custom PRIME7) | custom params only |

MLS 6 also fixes `HASH_LOG` for hash logs 13 to 16, so the hash shift is a
constant.

The Fast match finder does not prefetch. Its hash tables are 32-256 KiB,
and prefetching measured 2-4% slower.

Incompressibility bail-out: at probe intervals, if
`total_match_bytes * 6 < scanned`, the compressor clears sequences and returns
early (the block is emitted raw).


### DFast strategy (`crates/encode/src/dfast.rs`)

Dual hash tables: short (4-byte matches, `chain_log`) + long (8-byte matches,
`hash_log`). Same 4-cursor pipeline adapted for two tables with prefetch for
both. `search_log` controls when to try improving a short match with a long
match. Same incompressibility bail-out as Fast.


### Level parameters (`crates/encode/src/strategy.rs`)

Based on C zstd's `ZSTD_defaultCParameters` table, with L-8 added by zrip.
Level 0 is not a separate parameter set; it maps to the library default,
currently level 1.

| Level | Strategy | Hash table | Window | Min match | Target | Search log | Search strength | Notes |
|------:|:---------|:-----------|:-------|:---------:|-------:|-----------:|----------------:|:------|
| -8 | Fast | 32 KiB | 512 KiB | 6 | 21 | 0 | 5 | zrip-only |
| -7 | Fast | 32 KiB | 512 KiB | 6 | 17 | 0 | 7 | |
| -6 | Fast | 32 KiB | 512 KiB | 6 | 13 | 0 | 7 | |
| -5 | Fast | 32 KiB | 512 KiB | 6 | 9 | 0 | 7 | |
| -4 | Fast | 32 KiB | 512 KiB | 6 | 5 | 0 | 7 | |
| -3 | Fast | 64 KiB | 512 KiB | 6 | 4 | 0 | 7 | |
| -2 | Fast | 64 KiB | 512 KiB | 6 | 3 | 0 | 7 | |
| -1 | Fast | 64 KiB | 512 KiB | 6 | 2 | 0 | 7 | |
| 1 | Fast | 128 KiB | 512 KiB | 6 | 1 | 0 | 7 | default |
| 2 | Fast | 256 KiB | 1 MiB | 6 | 1 | 0 | 7 | |
| 3 | DFast | 2x 1 MiB | 2 MiB | 4 | 1 | 1 | 5 | Dual hash; 32-128 KiB uses strength 7 |
| 4 | DFast | 2x 4 MiB | 16 MiB | 4 | 1 | 0 | 8 | Dual hash |

Negative levels trade ratio for throughput by increasing `target_length`
(skip acceleration). Higher `target_length` means more positions are skipped
on consecutive match-finding misses.

**L-8** is L-7 with a larger step and a lower search strength, so the skip
distance grows faster through regions without matches. It keeps the full
encode pipeline and trades a little ratio for speed.

**L-8 through L2** use the full encode pipeline: Huffman-compressed literals
(with treeless reuse across blocks) and predefined or custom FSE tables for
sequences, whichever produces smaller output.

**L3 and L4** use the DFast strategy with two hash tables (short 4-byte and
long 8-byte matches) for better match quality at lower throughput.

**Raw literals decision.** The negative levels keep dense literals raw:
Huffman coding costs more time than it saves on them. `block_policy` in
`strategy.rs` sets an order-0 entropy limit of 6.25 bits per byte at L-8 to
L-1. `encode_literals_section` measures the full
histogram for up to 4 KiB of literals and a 4 KiB sample for longer runs.

At every level, literals with an order-0 entropy above 7.97 bits per byte
stay raw: Huffman would save under 0.4%, less than its table costs. When
literals are long or sequences few (at least 20 literals per sequence), a
4 KiB sample from each end is checked first, as in C zstd, so incompressible
literals skip the full histogram. Literals whose best Huffman estimate is not
smaller than the raw bytes also stay raw.

Small inputs on the negative levels keep all literals raw and use predefined
sequence tables (`raw_literals_limit`): up to 2 KiB at L-1, growing 1.5x or
1.33x per level to 24 KiB at L-8. Building a Huffman table costs a few
microseconds regardless of input size, which dominates messages of a few
KiB. Without Huffman, matches are the only compression, so the negative
levels use a denser match search on these inputs (`small_input_search`:
target length 8 at L-8 down to 1 at L-1, 5-byte minimum match). These inputs
also skip the incompressibility sampling (`skip_match_search`), which costs
about as much as their match search.


## Block encoder (`crates/encode/src/block_encoder.rs`)

Per-block decisions:

**Literals**: Blocks try Huffman-compressed literals, subject to the
entropy limit above, with treeless reuse across blocks via `prev_huffman`.
For literal blocks >= 1024 bytes, encodes into 4 independent streams to
enable decoder-side parallelism.

**3-way FSE table mode selection.** Compares predefined, custom
(data-derived), and repeat (carry-over from previous block or dict) FSE
encodings. Picks the smallest. Estimates predefined cost without full
encoding to skip the custom path when it cannot win. Blocks with < 64
sequences always use predefined tables (custom table header overhead exceeds
savings).

**Pre-packed sequence batch** (`pack_sequences_and_literals`). Pre-computes
all LL/ML/OF codes, extra bits, and rep-offset resolution into `PackedSeq`
structs in a single pass, counting code frequencies on the way. FSE encoding
then iterates the packed array without recomputing. It flushes once per
sequence: at most 30 extra bits fit after the three state transitions, as in
C zstd, and longer extras flush first.

**SymbolTT encoding scheme.** Two-field per-symbol struct (`delta_nb_bits`,
`delta_find_state`) encodes variable-width emission and state transition in
one lookup. ~1.3 KiB total footprint vs ~20 KiB for full tables.

**Incompressibility fast path**: `block_looks_incompressible()` samples
eight 512-byte windows spread over the block. Skips compression if no byte
value occurs more than 1/128 of the time plus 4, C zstd's Huffman bound.
Applied before entering the match finder, except on inputs that keep raw
literals.

**Fallback**: If the compressed block >= source size, rep offsets are reverted
and a raw block is emitted.

**Predefined encode table caching.** `LazyLock` builds LL/ML/OF predefined
encode tables once per process.


## Dictionary compression

`train_dict_fastcover()` in `crates/core/src/dict/mod.rs`: FastCOVER segment
selection + `finalize_dictionary()` produces standard zstd dictionary format
(magic + dict_id + Huffman table + FSE tables + rep offsets + content).
`FastCoverParams` defaults: k=2048, d=8, accel=1. Full COVER also exists in
`core/src/dict/cover.rs`.

### 3-tier dict setup (`context.rs:compress_with_prepared`)

Adapts setup cost to input size:

| Tier | Input size | Hash setup | Match finder | Literals |
|------|-----------|-----------|-------------|----------|
| A (micro) | <= per-level threshold | zero small input table | `compress_fast_attached` (linear scan, dual-lookup) | raw |
| B (small) | threshold-16 KiB | zero moderate input table | `compress_fast_attached` (linear scan, dual-lookup) | Huffman from dict |
| C (large) | > 16 KiB | copy full snapshot | `compress_fast_block` (4-cursor pipeline) | Huffman from dict |

Thresholds: `ATTACH_THRESHOLD` (16 KiB), per-level raw literals ramp (see
level parameters above). DFast strategy (levels 3-4) always uses the copy
path.

### Dict attachment (`fast.rs:compress_fast_attached`)

For tiers A and B, the PreparedDict's hash snapshot is referenced as a frozen
read-only table. A separate zeroed input hash table accumulates entries from
the current input. Two hash lookups per position (input table first, dict
table fallback). Zero-copy: no `memcpy` of the dict hash per frame.

Why a separate loop: the 4-cursor pipeline has ~15 live values for x86_64's
15 GPRs, with 5 stack spills per iteration. Adding dict attachment state
(dict hash pointer, second hash value, dict match candidate) would push to
~17+ live values and worsen spills. The simpler linear-scan loop has fewer
live variables and tolerates the extra state. For small inputs, the pipeline
startup cost dominates anyway.

### Hash log clamping (`strategy.rs:level_params_for_size`)

Clamps `hash_log`, `chain_log`, and `window_log` to `src_log` for small
inputs. The input-side hash table in the attached path uses this clamped size
while the dict-side table keeps the level's full `hash_log`. Reduces table
init cost without losing dict coverage.

### Hash table prefill (`fast.rs:prefill_hash_table`)

Fills the hash table from the dict prefix with strided sampling (step =
prefix_len / hash_size), then densely hashes the last 64 bytes. Balances
coverage against initialization cost.

### Dict entropy table carryover (`context.rs:PreparedDict::new`)

Converts the dictionary's decode FSE/Huffman tables to encode tables at
construction time. The first block uses dict tables as FSE "repeat" mode.

### Encoder prefix mode

Concatenates dict content + source, pre-fills hash table from dict content,
then runs the normal match finder over the combined buffer.


## Streaming

`crates/encode/src/streaming.rs`: `FrameEncoder<W: Write>` buffers input up to
128 KiB (`MAX_BLOCK_SIZE`), then flushes as a compressed block. Frame header
includes magic, descriptor (with content checksum flag), window descriptor, and
optional dict ID. `finish()` flushes the final block with `last=true` and writes
the XXH64 content checksum (lower 32 bits).

Cross-block matching: the encoder maintains a persistent window buffer
(`window_buf`) containing previous blocks' output. Hash table positions reference
into this combined view, allowing matches across block boundaries.

**Hash table position reduction** (`streaming.rs:reduce_hash_table`). When the
window buffer exceeds 2x window size, shifts all hash entries by `shift` and
compacts the window. Preserves hash state across blocks.

`Options` API controls window size and LDM: `Options::default().window_log(24).ldm(true)`.
`FrameEncoder::with_options(writer, level, &opts)` applies these to the frame.

`reset()` finishes the current frame, swaps the writer, and reuses all internal
buffers (hash tables, workspace, block encoder).

First block with a dictionary uses `compress_fast_with_prefix_reuse` /
`compress_dfast_with_prefix_reuse`. Subsequent blocks clear hash tables and run
standard block compression.


## Long distance matching (LDM)

`crates/encode/src/ldm.rs`. Two-pass integration with Fast/DFast strategies.

### Gear hash sampling

The gear hash is a 1-byte-at-a-time rolling hash:
`rolling = (rolling << 1) + GEAR_TABLE[byte]`, where `GEAR_TABLE` is 256
random `u64` values. After each byte, `rolling & stop_mask` is checked. When
it equals zero, the position is a "split point" where LDM samples a candidate.
`stop_mask` is derived from `hash_rate_log`: it has `hash_rate_log` bits set in
the upper part of the accumulator, so roughly 1 in `2^hash_rate_log` positions
trigger a check. At the default `hash_rate_log=6`, about 1 in 64 positions are
sampled.

The `feed()` method processes input in 4-byte unrolled batches (`LDM_BATCH_SIZE`
= 64 split points per call) for cache efficiency.

### Hash table

The table stores `LdmEntry` values: `offset: u32` (position in the input) +
`checksum: u32` (upper 32 bits of xxh64 over `min_match_length` bytes at that
position). At `hash_log=20`, this is 1M entries = 8 MiB. The table does not
store input data, so a fixed-size table covers arbitrarily large inputs.

Entries are organized into buckets of `1 << bucket_size_log` slots (default 8).
The lower bits of the xxh64 select a bucket, the upper 32 bits become the
checksum. Each bucket has a `u8` write counter in `bucket_offsets[]` that wraps
with `(counter + 1) & bucket_mask`. This is a per-bucket ring buffer: new
entries overwrite whichever slot the counter points at, round-robin. No age
tracking, no LRU.

When a split fires, if an existing entry in the selected bucket has a matching
checksum, the actual bytes at both positions are compared to confirm a match.
Backward and forward extension finds the full match length.

### Two-pass compression

First pass: `generate_sequences()` scans the input with the gear hash, probes
the hash table at each split point, and collects `RawLdmSeq` entries (literal
length, match length, offset). Second pass: `compress_block()` iterates over
the LDM sequences. For each gap between LDM matches, the regular Fast or DFast
compressor runs on that segment and emits short-range sequences. LDM sequences
and gap-fill sequences are interleaved into the final sequence list.

On the first block, `fill_only()` populates the hash table without searching
(there is no prior context to match against).

### Parameters

`LdmParams` controls: `hash_log` (table size), `bucket_size_log` (slots per
bucket), `min_match_length` (minimum bytes for a valid LDM match), and
`hash_rate_log` (sampling density). `LdmParams::default_for_window_log(wl)`
computes defaults for a given window size.

### Streaming and cross-block

In streaming mode, `LdmState` persists across blocks: the hash table retains
entries from previous blocks. `reduce_positions(shift)` subtracts `shift` from
all stored offsets when the window buffer compacts, discarding entries that
would point before the new buffer start.

### Memory and decoder impact

The encoder allocates the 8 MiB LDM hash table plus a window buffer of
`1 << window_log` bytes. The decoder does not know LDM was used. LDM sequences
are standard zstd sequences with larger offsets. The decoder allocates a window
buffer matching the `window_size` declared in the frame header, which is set by
`window_log`. At `window_log=27` this is ~128 MiB on each side.

Enable via `Options::ldm(true)` (requires the `ldm` feature, on by default).


## Decode pipeline

### Sequence execution (`crates/decode/src/exec.rs`)

`#![forbid(unsafe_code)]` under `paranoid`; the copies it calls live in
`fast_vec.rs`. One safe Rust loop handles all platforms, shaped after C
zstd's `ZSTD_decompressSequences_body`:

- **Bit reader.** `SeqBits` counts unread bits in a 64-bit container, so a
  read is one shift and one `bzhi`. It is a small `Copy` value that takes
  the stream on each refill, so it stays in registers. The loop refills once
  per sequence, after the state update, and once more only when a
  sequence's extra bits exceed 31 (`MID_RELOAD_THRESHOLD`). Reads past the
  stream's start make the unread count wrap, and the end check rejects the
  stream, as C's `BIT_endOfDStream` does.
- **Rows.** `SeqEntry` holds the next state's base and bit count and the
  value's baseline and extra-bit count. Both counts are a 32-variant
  `#[repr(u8)]` enum (`BitCount`): its range reaches LLVM with every load,
  so the `bzhi` needs no `& 63`. The loop copies each row out. Holding
  references instead made LLVM move the state updates across the loop's back
  edge and spill them.
- **Output.** `SeqOut` moves the caller's `Vec` into a local for the block,
  so its pointer and length stay in registers. Behind a `&mut Vec`, every
  byte store may alias the vector's header, and each copy reloaded and stored
  the length. The cold path takes the buffer and positions as values, so no
  `&mut SeqOut` leaves the hot loop. `SeqOut::new` reserves the block's
  maximum plus `WILDCOPY_OVERLENGTH` bytes; the length is set once, at the
  end.
- **Checks.** The fast path makes three tests: the block's limit, whether the
  literal run ends at least `LITERAL_OVERREAD` (32) bytes before the end of
  its readable bytes, and the offset. Everything else (literal overruns,
  matches into a dictionary's history, invalid offsets, runs near the end of
  the literals) goes to the `#[cold]` `execute_exact`, which checks in the
  original error order.
- **Literals.** `decode_literals_ws` leaves 32 zero bytes after the literals
  it decodes into the workspace, and borrows raw literals in place when the
  block holds 32 more bytes after them, so literal copies need no
  length-dependent branches.

The last sequence of a block is peeled off, so the loop never tests for it.

Literal copies and match copies use the chunked copies in `fast_vec.rs`
(see [Wild copies](#wild-copies) below).

`fearless_simd::dispatch!` compiles the function with platform target features,
enabling BMI2 bit extraction (`shlx`, `shrx`, `bzhi`).


### FSE tables

**Fixed-size tables with mask** (`crates/decode/src/seq_table.rs`).
`SeqTable` uses 512-entry storage with `& FSE_SEQ_TABLE_MASK` instead of
variable-size `Vec` indexing. This removes per-block sequence table
allocation and uses bitwise masking instead of modular arithmetic.

**Promoted FSE tables** (`seq_table.rs:SeqTable::promote_ll` etc.).
`FseSeqDecodeEntry` pre-computes `extra_bits` and `baseline_value` from the
LL/ML/OF code tables. Decode reads one struct instead of doing a separate
baseline/bits table lookup per symbol.

**In-place table building** (`sequences.rs:parse_sequence_tables_ws`). Builds
decode tables into workspace buffers instead of returning new Vecs.

**Predefined table caching** (`sequences.rs`). `LazyLock` for LL/ML/OF
predefined tables in `std` builds. Built once, then `SeqTable`-cloned per use.


### Bitstream (`crates/core/src/bitstream/reader_reverse.rs`)

Used by the Huffman decoders, the table readers and blocks with a single
sequence. Blocks with more sequences use `SeqBits` (see above).

**bitsConsumed model.** Tracks consumed bits instead of available bits.
Peek uses `(container << consumed) >> (64 - n)` (2 ops) vs the 3-op
available-bits model.

**Branchless bit reads** (`read_bits_branchless`).
`(container << (consumed & 63)) >> 1 >> (63 - n)` avoids the n==0 branch
in the hot FSE decode loop.

**Fast refill** (`refill_fast`). Byte-aligned reload without bounds checking
when the pointer is far from buffer start.

**Branch hints** (`hint.rs:likely`, `unlikely`). `#[cold]` function call to
hint branch prediction. Used for error paths and loop exits.


### Huffman 4-stream decoder (`crates/core/src/huffman/decode_4stream.rs`)

Interleaved 4-stream parallel decode: the compressed literals are split into 4
segments, each with an independent `ReverseBitReader`. The fast loop decodes 5
symbols per stream per iteration (20 symbols total), checking both output bounds
and bitstream bounds. Index-based output with safe slice indexing and inline
`refill!`/`decode_one!` macros.

4-symbol unrolled 1-stream inner loop: decodes 4 symbols per iteration when
bits remaining >= 4 * table_log.

BMI2 variant via `#[target_feature(enable = "bmi2")]` for faster bit extraction
(PEXT/PDEP). Dispatched via `cpu_tier() >= CpuTier::Bmi2`.
Slow path finishes remaining symbols one stream at a time near exhaustion.

Treeless block reuse: `huf_valid` flag allows treeless literal blocks to reuse
the previous Huffman decode table with no re-parse or rebuild.

**Two-symbol tables** (`build_pair_table`, `decode_4_streams_pairs_into`).
For literal sections of at most five bits per symbol, the fast loop reads an
11-bit table whose rows decode one or two symbols (C zstd's `HUF_DEltX2`).
The table is built from the single-symbol rows and rebuilt only after a new
tree. Each lookup stores both bytes of its row and advances by its symbol
count; one bounds check covers a round's 10-byte output window.
`prefers_pairs` applies C's timing model (`HUF_selectDecoder`) under that
five-bit limit: with longer codes few pairs fit in 11 bits, and the
single-symbol loop is faster. Every stream ends on the single-symbol table.

Literal buffer padding: decoded literals are followed by 32 zero bytes, and
raw literals are read in place when the block holds 32 more bytes after
them, so the sequence loop copies literals in whole chunks.


### Wild copies (`crates/decode/src/fast_vec.rs`)

All unsafe gated by `#[cfg(not(feature = "paranoid"))]`.

- **Sequence copies** (`SeqOut`, `Dst`). Literals copy 16 bytes
  unconditionally, then 32-byte chunks. Matches with offsets of 32 or more
  copy 32-byte chunks, offsets of 16 to 31 copy 16-byte chunks, and shorter
  offsets spread the period to a distance of at least eight the C zstd way
  (`ZSTD_overlapCopy8`, `spread_match`) before copying 8-byte chunks. Each
  chunk is read whole before it is written. A copy may write up to 31 bytes
  past its sequence, inside the reserved overlength.
- **Single-sequence blocks** (`BlockOutput`). Tiered copies:
  `wild_copy_match_single_unchecked` for wide offsets, offset 1 via
  `write_bytes`, 8-15 via 8-byte copies, and 2-7 via pattern stamping
  (`build_pattern_u64`).

Under `paranoid`, `SeqOut` zeroes each block's room and runs the same chunked
copies on an initialized slice; `BlockOutput` falls back to
`extend_from_slice` and `extend_from_within`.


## CPU feature dispatch

Two dispatch mechanisms coexist:

**Decoder:** `fearless_simd::dispatch!` with a cached `OnceLock<Level>`. The
macro compiles `decode_execute_sequences` with platform-specific target features
(AVX2+BMI2 on x86_64, NEON on aarch64, SIMD128 on wasm32) and selects the best
version at runtime when the `simd` feature is enabled. Zero `unsafe` in user
code. Works under `paranoid` since `fearless_simd` requires no unsafe. Without
`std` or `simd`, the scalar path runs directly.

**Huffman BMI2:** `crates/core/src/simd/mod.rs` defines a `CpuTier` enum
(`Bmi2`/`Avx2` on x86_64). Runtime detection cached in `OnceLock<CpuTier>`.
The Huffman 4-stream decoder dispatches to a `#[target_feature(enable = "bmi2")]`
variant for faster bit extraction.

**Compile-time tier for no_std.** Falls back to `cfg!(target_feature = "...")`
checks when std is unavailable.


## Compile-time specialization

Const generics eliminate dead code at compile time:

- `compress_fast_block_impl::<HASH_LOG, MLS>`: LLVM specializes per MLS (4, 5, 7),
  dead-code eliminates unused hash/match paths.
- `compress_dfast_block_impl::<HASH_LOG, SHORT_LOG, MLS>`: same for DFast.
- `hash_pos::<HASH_LOG, MLS>`: selects hash4/hash5/hash7 at compile time.
- Strategy dispatch (`Fast` vs `DFast`) happens at block boundaries, not
  per-position.


## Allocation reuse

**CompressContext** (`crates/encode/src/context.rs`). Hash tables, output
buffer, sequence vec, and workspace are allocated once and reused across calls.

**BlockEncodeWorkspace** (`crates/encode/src/block_encoder.rs`). All scratch
buffers (lit_buf, lit_section, pred_seq, cust_seq, repeat_seq, huf_concat,
huf_stream, writer bufs, packed_seqs) allocated once, reused across blocks.

**Cow output borrowing** (`context.rs:take_or_borrow_output`). Small outputs
return `Cow::Borrowed` (zero allocation), large outputs return `Cow::Owned`
(moves the buffer out).

**DecompressContext** (`crates/decode/src/context.rs`). Output buffer and
`BlockDecodeWorkspace` allocated once, reused across frames.

**BlockDecodeWorkspace** (`crates/decode/src/lib.rs`). All scratch vectors
(literal_buf, huf_table, huf_all_weights, huf_rank_count, huf_rank_start,
fse_dist, fse_symbol_next, fse_build_buf) allocated once.

**Dict FSE/Huffman caching** (`crates/decode/src/lib.rs:cache_dict`).
Pre-promotes dict FSE decode tables (LL/ML/OF) and Huffman table at
construction time. Each frame clones the cached promoted tables instead of
rebuilding from raw dictionary entries. Avoids 3 intermediate Vec allocations
per frame.


## Unsafe boundary

Decode execution (`exec.rs`) and sequence parsing (`sequences.rs`) are
`#![forbid(unsafe_code)]`. Platform dispatch is handled by
`fearless_simd::dispatch!`, which encapsulates `#[target_feature]` calling
internally.

Encoder unsafe is confined to `encode/src/primitives.rs` (16 blocks):

- **Unchecked hash table access** (`hash_load`, `hash_store`).
  `get_unchecked`/`get_unchecked_mut` eliminates bounds checks in the
  hot loop.
- **Unaligned 4/8-byte reads** (`rd32`, `rd64`). `read_unaligned` for
  hash input, match confirmation, and match counting.
- **Pointer-based match counting** (`count_match_raw`). Compares 8 bytes
  at a time via XOR + `trailing_zeros()`. Falls to a byte loop for the
  tail.
- **16-byte literal copy** (`copy_literals_fast`). For len <= 16: two
  unconditional 8-byte copies (overlapping ok). For len > 16:
  `copy_nonoverlapping`.
- **Unchecked bitstream flush** (`bitstream_flush`). Direct
  `write_unaligned` into Vec at known-valid position.
- **Unchecked Vec write-at** (`vec_write_at`). Writes into uninitialized
  Vec capacity without updating len.
- **Cold panic path** (`cold_rep_panic`). `#[cold] #[inline(never)]`
  keeps panic code out of the hot path.

Each function has a `debug_assert!` guard. Pattern:
```rust
debug_assert!(pos + 4 <= src.len());
unsafe { (src.as_ptr().add(pos) as *const u32).read_unaligned() }
```

Core crate unsafe follows the same pattern in `bitstream/`, `huffman/`,
`xxhash/` primitives modules.

Decoder support unsafe lives in `decode/src/fast_vec.rs` (wild-copy and Vec
length primitives) and `decode/src/seq_table.rs` (`MaybeUninit` FSE table
reads).


## Core bitstream primitives

**Unaligned u64 read/write** (`bitstream/primitives.rs`). `read_unaligned`/
`write_unaligned` for 8-byte operations in all bit readers and writers.

**BitWriter batch flush** (`bitstream/writer.rs`). Accumulates bits in a u64
register, flushes when >= 32 bits. Single unaligned write per flush.


## `paranoid` feature

`cargo build --features paranoid` adds `#![forbid(unsafe_code)]` to all four
crate roots. `fearless_simd` dispatch still works (it requires no unsafe), so
the decoder gets full SIMD multiversioning under paranoid. What changes:

- Encoder and core `primitives.rs` switch to safe alternatives via dual
  `#[cfg(feature = "paranoid")]` / `#[cfg(not(feature = "paranoid"))]` bodies:
  direct indexing, `from_le_bytes`, `resize` in place of unchecked operations.
- Decoder sequence copies (`fast_vec.rs`) zero each block's output room and
  copy within an initialized slice; single-sequence blocks fall back to
  `extend_from_slice` and `extend_from_within`.
- Huffman BMI2 `#[target_feature]` dispatch is gated out (uses `unsafe` call).


## Cross-cutting

**Nightly optimize attributes.** `#![cfg_attr(feature = "nightly",
feature(optimize_attribute))]` on all three crates. Enables per-function
`#[optimize]` control on nightly builds.

**no_std + alloc.** All core codec logic works without std. Predefined table
caching degrades gracefully (rebuilds per use in no_std vs cached via
`LazyLock` in std).


## Scope

Greedy, Lazy, BtLazy2, BtOpt, and BtUltra strategies are out of scope.
zrip targets the Fast/DFast region of the speed-ratio curve (levels -7 through 4)
where encode throughput matters more than compression ratio.
