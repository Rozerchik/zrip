# Development

## Build

```bash
cargo build
cargo build --no-default-features --features alloc   # no_std + alloc
```

## Test

```bash
cargo test
cargo test --no-default-features --features alloc     # no_std tests
```

## JSR / WebAssembly

`jsr/build.sh` generates bundler bindings with native WASM imports. The npm
build requests web bindings separately and writes them into `npm/src/pkg`.

```bash
cd jsr
bash build.sh
deno task test
```

Use Deno 2.8.3 or newer for `deno bundle` and install `zstd` for cross-checks.
They cover the default SIMD path, forced scalar loading, synchronous bytes,
and initialization races. Bundled default initialization runs without file or
network permissions.

## Releasing

`release-plz` runs on every push to `main`
(`.github/workflows/release-plz.yml`). It opens or updates a release PR,
creates annotated tags after merge, publishes to crates.io, and creates
GitHub releases. Configuration lives in `release-plz.toml`.

Publishing uses crates.io trusted publishing through GitHub Actions OIDC.
Do not add a crates.io token secret.

### Steps

1. **Review the release-plz PR.** Verify semver bumps and dependency versions
   for every affected crate.

2. **Curate changelogs.** Insert a new `## [x.y.z]` section immediately below
   `## [Unreleased]` for each bumped crate and place its release notes there.
   Never rename or modify an existing versioned section.

3. **Bump the WASM package.** Update `jsr/deno.json` and
   `jsr/wasm/Cargo.toml`.
   Keep `npm/package.json` at the same WASM version.

4. **Build and verify.** Run the JSR commands below and any additional release
   audit warranted by the changes. Run `npm test` in `npm/` as well.

5. **Merge the release PR after CI passes.** Monitor `release-plz` for crate
   publication and `.github/workflows/jsr.yml` for JSR publication.
   JSR rejects duplicate versions. Then publish npm using the tag or dispatch
   described below.

6. **Confirm publication.** Verify every intended crate version on crates.io
   and the new package version on each JavaScript registry. Run roundtrips
   against the published packages, including a bundled JSR roundtrip.

```bash
cd jsr
bash build.sh
deno task test
```

Use Deno 2.8.3 or newer for the bundled tests. They cover standalone bundles,
synchronous bytes, and initialization races. Bundled default initialization
runs without file or network permissions.

### npm

`@paddor/zrip` publishes to npm from `.github/workflows/release-npm.yml`.
The workflow version comes from a `zrip-npm-v*` tag or the manual
`workflow_dispatch` input. It builds the WASM package, runs Node tests, packs
the tarball, smoke-tests an installed tarball, then publishes from the `npm`
environment.

Publishing uses npm trusted publishing with GitHub Actions OIDC. In npm,
configure a trusted publisher for `@paddor/zrip` before tagging:

```text
GitHub owner: paddor
GitHub repository: zrip
Workflow filename: release-npm.yml
Environment name: npm
Allowed action: npm publish
```

If the package does not exist yet, trusted publishing may need a bootstrap
publish first. Add a publish-capable GitHub Actions secret named `NPM_TOKEN`,
run `.github/workflows/release-npm.yml`, then remove or rotate that token and
configure the trusted publisher before the next CI release.

```sh
git tag -a zrip-npm-v0.5.8 -m "zrip npm 0.5.8"
git push origin zrip-npm-v0.5.8
gh run watch --repo paddor/zrip --exit-status
```

## Fuzz

Fuzz targets live in `fuzz/fuzz_targets/`. Round-trip targets cross-validate
against C zstd. Corruption targets feed mutated compressed data to the decoder.

```bash
cargo +nightly fuzz run zrip_fuzz_roundtrip_frame -- -max_len=65536
cargo +nightly fuzz run zrip_fuzz_c_compress_zrip_decompress
```

## Pre-release fuzz run

Before tagging a release, run every fuzz target, corrupt targets first. The
code has no `unsafe`, so targets build without a sanitizer. They catch
panics, hangs, out-of-memory, and round-trip or C zstd mismatches.

For the full release duration, run each fuzz target for 3 hours:

```bash
FUZZ_SECONDS_PER_TARGET=10800 scripts/overnight_fuzz.sh
```

For an overnight pass, the script defaults to 30 minutes per fuzz target.
Logs, artifacts, and TSV summaries are written under
`tmp/overnight-fuzz/$RUN_ID/`.

Useful controls:

```bash
SMOKE=1 scripts/overnight_fuzz.sh      # quick harness check
CPU_COUNT=8 scripts/overnight_fuzz.sh  # build/fuzz parallelism
```

The dict decode test uses a pre-built fixture
(`tests/fixtures/corpus_dict_roundtrip.bin`). Regenerate after corpus changes:
`cargo test --features dict_builder -- fuzz_corpus_dict_generate`

### Adversarial corpus

If you have an adversarial corpus of small/malformed zstd files (e.g. from
prior fuzzing campaigns), seed them into the corrupt_decompress target:

```bash
cargo +nightly fuzz run zrip_fuzz_corrupt_decompress /path/to/adversarial/corpus \
  -- -max_total_time=10800 -jobs=2
```

## Benchmarks

### Corpus benchmarks

All benchmark and chart commands below run from the repository root. The
`bench/` crate is excluded from the workspace, so use
`--manifest-path bench/Cargo.toml`.

```bash
cargo run --manifest-path bench/Cargo.toml --example zrip_bench --release
cargo run --manifest-path bench/Cargo.toml --example zrip_bench --release -- \
  --impl all
```

Options: `--impl zrip|"C zstd"|all`, `--levels -7,1,3`,
`--files dickens,mozilla`, `--extra /path/to/file`.

The benchmark harness prepares its own ignored corpus under `bench/corpus/`.
It downloads the 12 pinned Silesia inputs. `--small-only` slices the first bytes
from the base corpus in memory; no small corpus files need to be generated.

Results cache under `~/.cache/zrip/` is append-only benchmark history. Do not
delete, truncate, or rewrite cache files when re-benchmarking after code
changes. Run the benchmark again and append fresh rows.

C zstd results are stable across zrip code changes and rarely need rerunning.
Routine refreshes after zrip code changes benchmark only `zrip` for the
relevant chart inputs. Do not run `--impl all` unless the user asks for all
implementations, external baselines must be refreshed, or the benchmark
harness or corpus changed in a way that affects all implementations.

### Benchmarking all levels

zrip supports levels -7 through 4. The default bench runs all zrip levels.
To bench a subset of levels:

```bash
cargo run --manifest-path bench/Cargo.toml --example zrip_bench --release
cargo run --manifest-path bench/Cargo.toml --example zrip_bench --release -- \
  --levels -1,1,3
```

## Charts

Rust chart generation lives in the `bench` crate and reads
`~/.cache/zrip/*.jsonl`:

| Command | Output | Description |
|---------|--------|-------------|
| `cargo run --manifest-path bench/Cargo.toml --bin zrip_charts -- scatter` | `scatter.svg` | Encode speed vs ratio geomean |
| `cargo run --manifest-path bench/Cargo.toml --bin zrip_charts -- summary` | `summary.svg` | Summary comparison table |
| `cargo run --manifest-path bench/Cargo.toml --bin zrip_charts -- matrix` | `matrix.svg` | Per-file/level heatmap matrix |
| `cargo run --manifest-path bench/Cargo.toml --bin zrip_charts -- pipeline` | `pipeline.svg` | Encode+decode pipeline throughput |
| `cargo run --manifest-path bench/Cargo.toml --bin zrip_charts -- small-encode` | `small_encode.svg` | Encode speed vs input size |
| `cargo run --manifest-path bench/Cargo.toml --bin zrip_charts -- small-decode` | `small_decode.svg` | Small decode comparison |

Chart refresh means benchmark first, appending new rows to the cache, then run
the chart tool. Re-running the chart tool against old cache data only replots;
it does not refresh the charts.

For normal zrip code changes, refresh only the changed implementations:

```bash
cargo run --manifest-path bench/Cargo.toml --example zrip_bench --release
cargo run --manifest-path bench/Cargo.toml --example zrip_bench --release -- \
  --small-only
cargo run --manifest-path bench/Cargo.toml --example zrip_bench --release -- \
  --small-only --decode-only --levels 3 --impl zrip
```

That updates the `zrip` rows used by the charts without rerunning stable
external implementations.

### Regenerating charts

After a full-corpus benchmark run, regenerate only the full-corpus charts:

```bash
cargo run --manifest-path bench/Cargo.toml --bin zrip_charts -- scatter
cargo run --manifest-path bench/Cargo.toml --bin zrip_charts -- summary
cargo run --manifest-path bench/Cargo.toml --bin zrip_charts -- matrix
cargo run --manifest-path bench/Cargo.toml --bin zrip_charts -- pipeline
```

The `all` chart command also renders small-input charts, so it needs
small-input encode and decode caches too. With no output dir, charts go under
`doc/charts/<arch>/`.

Optional local hardware labels can live in ignored `.chart_hw`:

```text
prefix=Linux VM on a 2018 Mac Mini
postfix=performance governor, turbo off
```

The chart tool reads `.chart_hw` from the current dir or parent dir. Env vars
`ZRIP_HW_PREFIX`, `ZRIP_HW_POSTFIX`, and `ZRIP_HW_EXTRAS` override or extend
local detection.

### Small-input benchmark + chart workflow

Each small input is up to 64 distinct consecutive slices of one size, and
each timed pass walks all of them. Repeating one slice lets the branch
predictor learn it and distorts results. `small_decode.svg` draws each
implementation decoding C zstd's L3 frames (thick) and its own L3 output
(thin, from the small encode rows).

`small_encode.svg` includes zrip, C zstd, structured-zstd, and lz4rip.
Default `--small-only` benchmarks zrip only:

```bash
cargo run --manifest-path bench/Cargo.toml --example zrip_bench --release -- \
  --small-only
cargo run --manifest-path bench/Cargo.toml --bin zrip_charts -- small-encode \
  doc/charts/x86_64/
```

### All-implementation benchmark + chart workflow

Use this only when refreshing all baselines, validating a benchmark harness
change, or when explicitly asked to re-run every implementation.

```bash
cargo run --manifest-path bench/Cargo.toml --example zrip_bench --release -- \
  --impl all
cargo run --manifest-path bench/Cargo.toml --example zrip_bench --release -- \
  --small-only --impl all
cargo run --manifest-path bench/Cargo.toml --example zrip_bench --release -- \
  --small-only --decode-only --levels 3
export ZRIP_HW_EXTRAS="performance governor,turbo off"
cargo run --manifest-path bench/Cargo.toml --bin zrip_charts -- all
```
