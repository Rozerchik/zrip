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
| 6   | `hash6` (PRIME64_1) | -6..2 |
| 7   | `hash7` (custom PRIME7) | custom params only |
| 8   | `hash8` (PRIME64_1) | -7 |

MLS 5 to 7 set the hash width only: as in C zstd, a match needs four equal
bytes. MLS 8 is zrip's own and also needs eight equal bytes, so it finds
fewer, longer matches.

MLS 6 also fixes `HASH_LOG` for hash logs 13 to 16, and MLS 8 for hash log
13, so the hash shift is a constant.

The Fast match finder does not prefetch. Its hash tables are 32-256 KiB,
and prefetching measured 2-4% slower.

Incompressibility bail-out: at probe intervals, if
`total_match_bytes * 6 < scanned`, the compressor clears sequences and returns
early (the block is emitted raw).


### DFast strategy (`crates/encode/src/dfast.rs`)

Dual hash tables: short (`min_match` bytes, 4 or 5, `chain_log`) and long
(8 bytes, `hash_log`). The loop follows C zstd's
`ZSTD_compressBlock_doubleFast`: one position at a time, rep 0 checked one
byte ahead, then the long table, then the short table. A short match also
tries the long table at the next position and takes it when longer. The step
grows by one every `1 << search_strength` bytes without a match. After a
match, two positions per table go back into the tables and rep 1 matches
are taken at once. On the Silesia corpus it emits within 1% of C zstd's
sequence count at the same parameters.

Candidates count when they lie at or after the window start for the block
end, as in C zstd: table entries always lie behind the search, so one
compare checks them. `search_log` is unused.

An earlier 2-cursor finder probed two positions per step. It ran 36% more
instructions than C zstd's L3, and on data with few matches it hardly
searched: on 1 MiB x-ray slices it found 11 sequences per KiB, against 71
for C zstd.


### Level parameters (`crates/encode/src/strategy.rs`)

Based on C zstd's `ZSTD_defaultCParameters` table, except L-7 to L-4. Level
0 is not a separate parameter set; it maps to the library default, currently
level 1. Level -8, a separate zrip level in earlier versions, runs as L-7.

| Level | Strategy | Hash table | Window | Min match | Target | Search log | Search strength | Notes |
|------:|:---------|:-----------|:-------|:---------:|-------:|-----------:|----------------:|:------|
| -7 | Fast | 32 KiB | 512 KiB | 8 | 2 | 0 | 4 | raw literals; zrip's own search |
| -6 | Fast | 32 KiB | 512 KiB | 8 | 9 | 0 | 5 | |
| -5 | Fast | 32 KiB | 512 KiB | 8 | 5 | 0 | 6 | |
| -4 | Fast | 32 KiB | 512 KiB | 7 | 4 | 0 | 5 | |
| -3 | Fast | 64 KiB | 512 KiB | 6 | 4 | 0 | 7 | |
| -2 | Fast | 64 KiB | 512 KiB | 6 | 3 | 0 | 7 | |
| -1 | Fast | 64 KiB | 512 KiB | 6 | 2 | 0 | 7 | |
| 1 | Fast | 128 KiB | 512 KiB | 6 | 1 | 0 | 7 | default |
| 2 | Fast | 256 KiB | 1 MiB | 6 | 1 | 0 | 7 | |
| 3 | DFast | 512 + 256 KiB | 2 MiB | 5 | 1 | 1 | 4 | C's L3 tables |
| 4 | DFast | 2x 1 MiB | 16 MiB | 5 | 1 | 0 | 6 | C's L4 tables |

Negative levels trade ratio for throughput by increasing `target_length`
(skip acceleration). Higher `target_length` means more positions are skipped
on consecutive match-finding misses.

**L-7** aims at LZ4's niche. It keeps literals raw at every input size, as C
zstd does at its negative levels, with predefined sequence tables, and takes
only matches of eight bytes or more. On the Silesia corpus that makes 29
sequences per KiB, against 40 for C zstd's L-7 frames and 53 for LZ4, so its
frames decode fast. Its search steps 3 bytes, denser than C's L-7 (8) and
the old zrip L-7 (18), and the step grows every 16 bytes without a match,
for a ratio of 1.90 against 1.94 for C's L-7 and for LZ4. Growing it every
32 bytes reached 1.96, but encoded medium files only 3% faster than L-6. It
replaced an L-7 with Huffman literals that was only 4% faster than L-6.
Raw-literal variants with 6-byte matches either encoded slower than L-6 or
compressed no better than 8-byte matches.

**L-6 to L-4** hash 8 bytes (L-4: 7) and step 10, 6 and 5 bytes, with the step
growing every 32 or 64 bytes without a match. They used 6-byte matches at
steps of 14, 10 and 6, growing every 128 bytes, which left many bytes to
Huffman coding, the costlier part. On the Silesia corpus L-6 now encodes at
476 MB/s (was 410) at the same ratio, 2.18; L-5 at 437 MB/s (390) to 2.30
(2.27); L-4 at 386 MB/s (356) to 2.42 (2.43). The gap to L-7 comes from its
raw literals, not its search: with Huffman literals, medium-compressibility
files encode at about 470 MB/s at most whatever the step, and raw-literal
searches denser than L-7's encode slower than L-6 before they reach its ratio.
On osdb and sao, whose literals L-6 keeps raw, L-6 encodes faster than L-7,
and on osdb it compresses worse.

**L-6 through L2** use the full encode pipeline: Huffman-compressed literals
(with treeless reuse across blocks) and predefined or custom FSE tables for
sequences, whichever produces smaller output.

**L3 and L4** use the DFast strategy with C zstd's L3 and L4 tables. C
grows the step every 256 bytes without a match; L3 does so every 16 bytes,
so data with few matches encodes fast. On the Silesia corpus L3 compresses
to 3.16 at 173 MB/s (C: 3.21 at 190 MB/s); on sao and x-ray, 1.32 at 116 MB/s
(C: 1.35 at 101 MB/s). Growing it every 8 bytes cost small text inputs up to
10% in ratio. L4 grows it every 64 bytes and keeps a 16 MiB window: 3.26 at
161 MB/s (C: 3.26 at 188 MB/s). The remaining gap splits between extra
instructions in the finder and per-sequence work outside it; L2 trails C
zstd by as much. A 4-byte short hash helped only text inputs under 16 KiB,
at up to 30% of their speed; a 6-byte one cut L3's ratio on sao and x-ray
to L2's.

**Raw literals decision.** The negative levels keep dense literals raw:
Huffman coding costs more time than it saves on them. `block_policy` in
`strategy.rs` sets an order-0 entropy limit of 6.25 bits per byte at L-6 to
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
1.33x per level to 12 KiB at L-6 (L-7 keeps literals raw at every size).
Building a Huffman table costs a few microseconds regardless of input size,
which dominates messages of a few KiB. Without Huffman, matches are the only
compression, so L-6 to L-1 use a denser match search on these inputs
(`small_input_search`: target length 4 at L-6 down to 1 at L-1, 5-byte hash).
L-7 keeps its own search. These inputs
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

### Dict setup (`context.rs:compress_with_prepared`)

Adapts setup cost to input size:

| Path | Input | Hash setup | Match finder |
|------|-------|-----------|-------------|
| Attached | <= 8 KiB, or one block outside the snapshot's tier | zero an input table sized to the input | `compress_fast_block_attached` |
| Copy | > 8 KiB in the snapshot's tier | copy the prefilled snapshot | `compress_fast_block` |
| Per call | over one block outside the snapshot's tier | prefill per call | `compress_fast_block` |

`ATTACH_THRESHOLD` is 8 KiB, the attach limit of C zstd's fast strategy.
Literals stay raw below the per-level raw-literals ramp, measured on dict plus
input size, and use Huffman seeded from the dict otherwise. DFast (levels 3-4)
always copies or prefills.

### Dict attachment (`fast.rs:compress_fast_block_attached`)

The 4-cursor match finder with `DICT` set. A zeroed input table collects the
input's positions. A probe that misses it looks up a read-only dict table
hashed with the same width. No `memcpy` of the dict table per frame.
`PreparedDict` keeps one dict table per minimum match the level uses, since
negative levels search small inputs with a shorter match. The tables are sized
one bit above the dict content's log.

### Hash log clamping (`strategy.rs:level_params_for_size`)

Clamps `hash_log`, `chain_log`, and `window_log` to `src_log` for small
inputs. The attached input table uses the clamped size; the dict table is
sized to the dict content. Reduces table init cost without losing dict
coverage.

### Hash table prefill (`fast.rs:prefill_hash_table`)

Hashes dict positions with the match finder's width for the level's minimum
match: strided sampling (step = prefix_len / hash_size), then the last 64
positions densely. Another width would put dict entries in slots the match
finder never reads.

### Dict entropy table carryover (`context.rs:PreparedDict::new`)

Converts the dictionary's decode FSE/Huffman tables to encode tables at
construction time. The first block uses dict tables as FSE "repeat" mode.

### Encoder prefix mode

The one-shot and streaming APIs concatenate dict content and source. A
one-shot input of one block runs the attached match finder with a per-call
dict table. Longer inputs and the streaming encoder prefill the hash table
and run the normal match finder over the combined buffer.


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

The copies it calls live in `fast_vec.rs`. One safe Rust loop handles all platforms, shaped after C
zstd's `ZSTD_decompressSequences_body`:

- **Bit reader.** `SeqBits` is one number, the stream's unread bits. Each
  sequence loads the word that holds its next 57 or more bits (`Word`),
  counts the word's unread bits, so a read is one shift and one `bzhi`, and
  turns the rest back into one number. A second word comes only when a
  sequence's extra bits exceed 31 (`MID_RELOAD_THRESHOLD`). Carrying one
  value instead of C's container, fill and position frees two registers in
  a loop that spills: 14 fewer instructions per sequence and about 7% faster
  decoding. Streams under eight bytes are read from a zero-padded copy.
  Reads past the stream's start make the count wrap to a huge value that
  never returns to zero, and the end check rejects the stream, as C's
  `BIT_endOfDStream` does.
- **Rows.** `SeqEntry` holds the next state's base and bit count and the
  value's baseline and extra-bit count. Both counts are a 32-variant
  `#[repr(u8)]` enum (`BitCount`): its range reaches LLVM with every load,
  so the `bzhi` needs no `& 63`. The loop copies each row out. Holding
  references instead made LLVM move the state updates across the loop's back
  edge and spill them. Packing each row into a `u64` cost 11 more
  instructions per sequence, and decoding 16 to 64 sequences ahead of their
  copies cost 15 more. Packing the older repeat offsets into one word saved
  5 instructions but measured 1-2% slower.
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
  original error order. For outputs below 4 GiB, the positions are masked to
  32 bits and the limit test reads `match_end + WILDCOPY_OVERLENGTH <= len`.
  The sums then cannot overflow, so the three tests imply the bounds of each
  copy's first chunk, and LLVM drops those checks. Later chunks keep theirs.
  The offset test is a `checked_sub`, so LLVM also knows that the match
  source lies below `lit_end`; a plain subtraction left a bounds check on
  the source's first chunk.
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

- **Sequence copies** (`SeqOut`, `Dst`). Literals copy 32 bytes
  unconditionally, then more 32-byte chunks. Matches with offsets of 32 or more
  copy 32-byte chunks, offsets of 16 to 31 copy 16-byte chunks, and shorter
  offsets spread the period to a distance of at least eight the C zstd way
  (`ZSTD_overlapCopy8`, `spread_match`) before copying 8-byte chunks. Each
  chunk is read whole before it is written. A copy may write up to 31 bytes
  past its sequence, inside the overlength. `SeqOut` zeroes each block's room,
  so the copies index an initialized slice.
- **Single-sequence blocks** (`BlockOutput`). Appends with
  `extend_from_slice` and `extend_from_within`.


## CPU feature dispatch

Two dispatch mechanisms coexist:

**Decoder:** `fearless_simd::dispatch!` with a cached `OnceLock<Level>`. The
macro compiles `decode_execute_sequences` with platform-specific target features
(AVX2+BMI2 on x86_64, NEON on aarch64, SIMD128 on wasm32) and selects the best
version at runtime when the `simd` feature is enabled, with no `unsafe` in
zrip. Without `std` or `simd`, the scalar path runs directly.

**Huffman:** the 4-stream decoder has one build. It gets BMI2's `shrx` and
`bzhi` only when the build targets a CPU with BMI2.

**Compile-time tier for no_std.** Falls back to `cfg!(target_feature = "...")`
checks when std is unavailable.


## Compile-time specialization

Const generics eliminate dead code at compile time:

- `compress_fast_block_impl::<HASH_LOG, MLS>`: LLVM specializes per MLS (4, 5, 7),
  dead-code eliminates unused hash/match paths.
- `compress_dfast_block_impl::<HASH_LOG, SHORT_LOG, MLS>`: same for DFast,
  with fixed table logs for L3 (17/16), L4 (18/18) and the equal logs 15-17
  that smaller inputs clamp them to.
- `hash_val::<HASH_LOG, MLS>`: selects hash4/hash5/hash6/hash7 at compile time.

**Bounds checks in the match finders.** A fixed `HASH_LOG` also lets the
finders index hash tables without checks: `exact_table` cuts each table to
exactly `1 << HASH_LOG` entries, so the compiler sees every hash in bounds.
The Fast finder passes the bytes read for hashing to `rd64_again`, which
answers from those bytes instead of reading again.
DFast also clamps its reads to `ilimit` (`rd64_upto`), turning two bounds
checks into a conditional move. The Fast finder keeps checked reads: clamps
measured 3% slower there, because a conditional move delays the load while a
predicted check does not.
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


## No unsafe

All four crates are `#![forbid(unsafe_code)]`. Platform dispatch goes through
`fearless_simd`, which keeps the `#[target_feature]` calls inside that crate.
Hot loops avoid bounds checks by construction: tables cut to their exact size
(`exact_table`), reads clamped to a limit the compiler can prove
(`rd64_upto`), bytes reused across checks (`rd64_again`), and output rooms
zeroed ahead so copies index initialized slices.


## Core bitstream primitives

**Little-endian u64 read/write** (`bitstream/primitives.rs`). `from_le_bytes`
reads and `copy_from_slice` writes for 8-byte operations in all bit readers
and writers.

**BitWriter batch flush** (`bitstream/writer.rs`). Accumulates bits in a u64
register, flushes when >= 32 bits. Single unaligned write per flush.


## `paranoid` feature

No effect today. Every build is safe Rust; the feature stays so existing
feature lists keep building, and so any future `unsafe` code can be left out
of builds that enable it.


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
