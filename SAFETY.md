# Safety

## No unsafe

All four crates (`zrip`, `zrip-core`, `zrip-encode`, `zrip-decode`) are
`#![forbid(unsafe_code)]`. SIMD dispatch goes through `fearless_simd`
(`dispatch!` in the decoder, `simd_body!` in the encoder), which keeps the
`#[target_feature]` calls inside that crate. No `unsafe` is exposed in the
public API.

The hot loops keep most bounds checks out by construction. Hash tables are
cut to their exact size, bytes read for hashing are reused for match checks,
and DFast clamps reads to a limit the compiler can prove in bounds. A clamped
read never leaves the input, but if a finder invariant broke, it would read
the wrong bytes instead of panicking; debug builds assert the invariants. The
decoder zeroes each block's output room, so its chunked copies index an
initialized slice.

The `paranoid` feature has no effect today. It stays so existing feature
lists keep building, and as a guarantee: if a later release adds `unsafe`
code for speed, builds with `paranoid` leave it out.

## Testing

- **Fuzz** (17 targets): round-trip and corruption targets, 3+ hours each.
- **Adversarial corpus**: 6500+ small/malformed zstd files from prior fuzzing.
- **Proptest**: property-based round-trips with random data sizes and levels.
- **C zstd cross-validation**: compress with C, decompress with zrip and vice versa.

See [DEVELOPMENT.md](DEVELOPMENT.md) for commands.

## Why Rust matters here

C zstd's decompression path has had memory safety bugs that Rust's type system
and bounds checking prevent by construction:

| CVE / Fix | Bug | Rust prevents because |
|-----------|-----|----------------------|
| [#50](https://github.com/facebook/zstd/issues/50) | OOB heap read in `ZSTD_copy8` (via `ZSTD_wildcopy` during `ZSTD_execSequence`): 8-byte read past buffer on malformed input (AFL) | Wildcopy uses slice operations bounded by destination length |
| [#49](https://github.com/facebook/zstd/issues/49) | OOB stack read in `HUF_readStats`: `rankVal` array accessed past bounds on malformed Huffman table (AFL) | Stack arrays are slices; indexing panics on OOB |
| [#22](https://github.com/facebook/zstd/issues/22) | Destination buffer bounds not checked during compress/decompress, heap overflow on undersized output | Output written via `&mut [u8]` slice; writes past end panic |
| [#4499](https://github.com/facebook/zstd/issues/4499) | Stack buffer overflow read in `FASTCOVER_hashPtrToIndex` training with many 1-byte samples | Slice indexing panics on OOB |
| [OSV-2020-405](https://osv.dev/vulnerability/OSV-2020-405) (HIGH) | Stack buffer overflow write in `ZSTD_decodeLiteralsBlock` (OSS-Fuzz #16445) | Slice write panics on OOB |
| [OSV-2020-654](https://osv.dev/vulnerability/OSV-2020-654) (MEDIUM) | Heap buffer overflow read 16 in `ZSTD_copy16` during `ZSTD_decompressSequences_bmi2` (OSS-Fuzz #17451) | Wildcopy bounded by slice length |
| [OSV-2020-429](https://osv.dev/vulnerability/OSV-2020-429) (MEDIUM) | Heap buffer overflow read 4 in `MEM_read32` during legacy v05 sequence decoding (OSS-Fuzz #14368) | `u32::from_le_bytes` requires a bounds-checked `[u8; 4]` |
| [OSV-2021-859](https://osv.dev/vulnerability/OSV-2021-859) (HIGH) | Heap buffer overflow write 1 in `FSE_writeNCount_generic` during Huffman weight compression (OSS-Fuzz #35209) | Slice write panics on OOB |
| [OSV-2022-110](https://osv.dev/vulnerability/OSV-2022-110) (HIGH) | Heap buffer overflow write 1 in `ZSTD_compressLiterals` (OSS-Fuzz #44239) | Slice write panics on OOB |
| [OSV-2022-96](https://osv.dev/vulnerability/OSV-2022-96) (HIGH) | Heap buffer overflow write 16 in sequence compression API (OSS-Fuzz #44122) | Slice write panics on OOB |
| [CVE-2024-11477](https://www.crowdfense.com/cve-2024-11477-7zip-zstd-buffer-overflow/) (CVSS 7.8, 7-Zip's zstd) | Integer underflow in decompression wraps array index, heap buffer overflow write | `usize` underflow panics in debug; slice indexing catches OOB in release |
| [PR #1803](https://github.com/facebook/zstd/pull/1803) | Memory over-read from wildcopy changes (PR #1756), required increasing `WILDCOPY_OVERLENGTH` to 16 (OSS-Fuzz) | Wildcopy in Rust uses slice operations bounded by destination length |
| [PR #2784](https://github.com/facebook/zstd/pull/2784) | Multiple Huffman decompression bugs: pointer underflow and NULL pointer issues (fuzzer) | No null references in Rust; pointer arithmetic replaced by slice indexing |
| [PR #1722](https://github.com/facebook/zstd/pull/1722) | Buffer overflow in legacy v0.3 decompression | Slice indexing panics on OOB |
| [PR #1590](https://github.com/facebook/zstd/pull/1590) | OOB read in `ZSTD_decompressBound` on malformed frame (fuzzer) | Slice indexing panics on OOB |
