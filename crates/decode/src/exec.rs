#[cfg(feature = "alloc")]
use alloc::vec::Vec;

use crate::fast_vec::{BlockOutput, Literals, SeqOut};
use crate::sequences::{SequenceDecodeTables, compute_offset};
use zrip_core::bitstream::reader_reverse::ReverseBitReader;
use zrip_core::error::DecompressError;
use zrip_core::fse::FSE_SEQ_TABLE_MASK;
use zrip_core::hint::{likely, unlikely};

#[derive(Clone, Copy)]
pub(crate) struct SequenceOutputScope<'a> {
    pub(crate) output_base: usize,
    pub(crate) max_block_output: usize,
    pub(crate) history: &'a [u8],
}

/// Width of [`SeqBits`]' container in bits.
const CONTAINER_BITS: u32 = 64;

/// Most extra bits a sequence reads before its literal length without a
/// second refill, given the state bits that follow: C zstd's
/// `64 - 7 - (LLFSELog + MLFSELog + OffFSELog)`.
const MID_RELOAD_THRESHOLD: u32 = 64 - 7 - (9 + 9 + 8);

/// Backward reader for the sequences bitstream, C zstd's `BIT_DStream_t`.
///
/// The container holds the stream bytes `pos..pos + 8`, its low `avail` bits
/// unread. Counting unread bits makes a read one shift and one mask. It is a
/// small `Copy` value and takes the stream per refill, so the loop keeps it
/// in registers. Reads past the stream's start make `avail` wrap below zero,
/// where it stays, so [`SeqBits::is_exhausted`] rejects the stream.
#[derive(Clone, Copy)]
struct SeqBits {
    container: u64,
    avail: u32,
    pos: usize,
}

/// The eight bytes at `src[pos..]` as a little-endian word, or zero when they
/// are not all there, which the refills never ask for. Masking `pos` to 32
/// bits lets the compiler see that `pos + 8` cannot overflow.
#[inline(always)]
fn read64(src: &[u8], pos: usize) -> u64 {
    let pos = pos & u32::MAX as usize;
    match src.get(pos..pos + 8) {
        Some(word) => u64::from_le_bytes(word.try_into().unwrap_or([0; 8])),
        None => 0,
    }
}

impl SeqBits {
    /// Opens `src`, or `None` when it is empty or ends without a marker bit.
    #[inline]
    fn new(src: &[u8]) -> Option<Self> {
        let &last = src.last()?;
        if last == 0 {
            return None;
        }
        let marker = last.leading_zeros() + 1;
        if src.len() >= 8 {
            let pos = src.len() - 8;
            return Some(Self {
                container: read64(src, pos),
                avail: CONTAINER_BITS - marker,
                pos,
            });
        }
        let container = src
            .iter()
            .rev()
            .fold(0u64, |word, &byte| (word << 8) | u64::from(byte));
        Some(Self {
            container,
            avail: CONTAINER_BITS - marker - (8 - src.len() as u32) * 8,
            pos: 0,
        })
    }

    /// Reads `count <= 31` bits.
    #[inline(always)]
    fn read(&mut self, count: u32) -> u32 {
        let start = self.avail.wrapping_sub(count);
        self.avail = start;
        ((self.container >> (start & 63)) & ((1u64 << count) - 1)) as u32
    }

    /// Refills a whole word while one is left before the start, else as far
    /// as the stream allows. Callers read at most 57 bits between refills, so
    /// while `pos >= 8` nothing was read past the start yet.
    #[inline(always)]
    fn reload(&mut self, src: &[u8]) {
        if self.pos >= 8 {
            let bytes = (CONTAINER_BITS - self.avail) >> 3;
            self.pos -= bytes as usize;
            self.avail += bytes * 8;
            self.container = read64(src, self.pos);
        } else {
            *self = self.reload_tail(src);
        }
    }

    /// The end of [`Self::reload`] near the stream's start; by value, so the
    /// hot loop keeps the container in registers.
    #[cold]
    #[inline(never)]
    fn reload_tail(mut self, src: &[u8]) -> Self {
        if self.pos == 0 || self.avail > CONTAINER_BITS {
            return self;
        }
        let wanted = ((CONTAINER_BITS - self.avail) >> 3) as usize;
        let step = wanted.min(self.pos);
        self.pos -= step;
        self.avail += (step as u32) * 8;
        self.container = read64(src, self.pos);
        self
    }

    /// Whether every bit was read, and no more.
    #[inline]
    fn is_exhausted(&self) -> bool {
        self.pos == 0 && self.avail == 0
    }

    /// Bits left in the stream, when nothing was read past its start.
    #[inline]
    fn remaining(&self) -> usize {
        self.avail as usize + self.pos * 8
    }
}

#[inline(always)]
pub(crate) fn decode_execute_sequences<const HAS_HISTORY: bool>(
    data: &[u8],
    num_sequences: u32,
    tables: &SequenceDecodeTables,
    offsets: &mut [u32; 3],
    literals: Literals<'_>,
    output: &mut Vec<u8>,
    scope: SequenceOutputScope<'_>,
) -> Result<(), DecompressError> {
    if num_sequences == 0 {
        return Ok(());
    }
    let bits = SeqBits::new(data).ok_or(DecompressError::CorruptSequences)?;
    let state_bits = u32::from(tables.ll_accuracy)
        + u32::from(tables.of_accuracy)
        + u32::from(tables.ml_accuracy);
    if bits.remaining() < state_bits as usize {
        return Err(DecompressError::InputExhausted);
    }
    let history = if HAS_HISTORY { scope.history } else { &[] };
    let mut out = SeqOut::new(
        core::mem::take(output),
        scope.max_block_output,
        scope.output_base,
    );
    let result = run_sequences(
        bits,
        data,
        num_sequences,
        tables,
        offsets,
        literals,
        &mut out,
        history,
    );
    *output = out.finish();
    result
}

/// The decoding half of the sequence loop: the three FSE states and the
/// repeat offsets.
struct SeqDecoder {
    ll_state: u32,
    of_state: u32,
    ml_state: u32,
    rep: [u32; 3],
}

impl SeqDecoder {
    /// Decodes one sequence as `(literal length, match length, offset)`,
    /// C zstd's `ZSTD_decodeSequence`. With `UPDATE` the states advance and
    /// the container is refilled; the block's last sequence does neither.
    #[inline(always)]
    fn decode<const UPDATE: bool>(
        &mut self,
        b: &mut SeqBits,
        data: &[u8],
        tables: &SequenceDecodeTables,
    ) -> (usize, usize, usize) {
        macro_rules! table_entry {
            ($table:expr, $state:expr) => {{
                let idx = ($state & FSE_SEQ_TABLE_MASK) as usize;
                *$table.get(idx)
            }};
        }
        let of_e = table_entry!(tables.of_table, self.of_state);
        let ml_e = table_entry!(tables.ml_table, self.ml_state);
        let ll_e = table_entry!(tables.ll_table, self.ll_state);
        let of_bits = of_e.extra_bits.get();
        let offset_value = of_e.baseline_value + b.read(of_bits);
        let ml_bits = ml_e.extra_bits.get();
        let match_length = ml_e.baseline_value + b.read(ml_bits);
        let ll_bits = ll_e.extra_bits.get();
        if ll_bits + ml_bits + of_bits > MID_RELOAD_THRESHOLD {
            b.reload(data);
        }
        let literal_length = ll_e.baseline_value + b.read(ll_bits);

        let [rep0, rep1, rep2] = self.rep;
        let offset = if likely(offset_value > 3) {
            let offset = offset_value - 3;
            self.rep = [offset, rep0, rep1];
            offset
        } else {
            let ll0 = u32::from(literal_length == 0);
            let rep_idx = offset_value - 1 + ll0;
            let offset = match rep_idx {
                0 => rep0,
                1 => rep1,
                2 => rep2,
                _ => rep0.wrapping_sub(1),
            };
            self.rep = [
                offset,
                if rep_idx >= 1 { rep0 } else { rep1 },
                if rep_idx >= 2 { rep1 } else { rep2 },
            ];
            offset
        };

        if UPDATE {
            // The format's order: literal length, match length, offset.
            self.ll_state = u32::from(ll_e.base_line) + b.read(ll_e.num_bits.get());
            self.ml_state = u32::from(ml_e.base_line) + b.read(ml_e.num_bits.get());
            self.of_state = u32::from(of_e.base_line) + b.read(of_e.num_bits.get());
            b.reload(data);
        }
        (
            literal_length as usize,
            match_length as usize,
            offset as usize,
        )
    }
}

/// The sequence loop of [`decode_execute_sequences`], C zstd's
/// `ZSTD_decompressSequences_body`: one refill per sequence, a second one
/// only when its extra bits could exhaust the container, and the last
/// sequence peeled off so the loop never tests for it.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn run_sequences(
    bits: SeqBits,
    data: &[u8],
    num_sequences: u32,
    tables: &SequenceDecodeTables,
    offsets: &mut [u32; 3],
    literals: Literals<'_>,
    out: &mut SeqOut,
    history: &[u8],
) -> Result<(), DecompressError> {
    let mut b = bits;
    let mut dec = SeqDecoder {
        ll_state: b.read(u32::from(tables.ll_accuracy)),
        of_state: b.read(u32::from(tables.of_accuracy)),
        ml_state: b.read(u32::from(tables.ml_accuracy)),
        rep: *offsets,
    };
    b.reload(data);

    let mut lp: usize = 0;
    let mut left = num_sequences - 1;
    while left != 0 {
        let (ll, ml, offset) = dec.decode::<true>(&mut b, data, tables);
        out.execute(literals, lp, ll, ml, offset, history)?;
        lp += ll;
        left -= 1;
    }
    let (ll, ml, offset) = dec.decode::<false>(&mut b, data, tables);
    out.execute(literals, lp, ll, ml, offset, history)?;
    lp += ll;

    if !b.is_exhausted() {
        return Err(DecompressError::CorruptSequences);
    }
    if lp < literals.len() {
        out.extend_literals(literals, lp, literals.len() - lp)?;
    }
    *offsets = dec.rep;
    Ok(())
}

#[inline(always)]
pub(crate) fn decode_execute_single_sequence<const HAS_HISTORY: bool>(
    data: &[u8],
    tables: &SequenceDecodeTables,
    offsets: &mut [u32; 3],
    literals: &[u8],
    output: &mut Vec<u8>,
    scope: SequenceOutputScope<'_>,
) -> Result<(), DecompressError> {
    if data.is_empty() {
        return Err(DecompressError::CorruptSequences);
    }

    let mut rev_reader =
        ReverseBitReader::new(data).map_err(|_| DecompressError::CorruptSequences)?;
    let ll_state = rev_reader.read_bits(tables.ll_accuracy)?;
    let of_state = rev_reader.read_bits(tables.of_accuracy)?;
    let ml_state = rev_reader.read_bits(tables.ml_accuracy)?;

    let mut output = BlockOutput::new(output, scope.max_block_output);
    let output_base = scope.output_base;
    let history = scope.history;

    macro_rules! table_entry {
        ($table:expr, $state:expr) => {{
            let idx = ($state & FSE_SEQ_TABLE_MASK) as usize;
            $table.get(idx)
        }};
    }

    rev_reader.refill();
    let of_e = table_entry!(tables.of_table, of_state);
    let ml_e = table_entry!(tables.ml_table, ml_state);
    let ll_e = table_entry!(tables.ll_table, ll_state);

    let of_extra = rev_reader.read_bits_branchless(of_e.extra_bits.get() as u8);
    let offset_value = of_e.baseline_value + of_extra;
    let ml_extra = rev_reader.read_bits_branchless(ml_e.extra_bits.get() as u8);
    let match_length = ml_e.baseline_value + ml_extra;
    let ll_extra = rev_reader.read_bits_branchless(ll_e.extra_bits.get() as u8);
    let literal_length = ll_e.baseline_value + ll_extra;
    let offset = compute_offset(offset_value, literal_length, offsets);

    let ll = literal_length as usize;
    let ml = match_length as usize;
    {
        let mut seq_output = output.begin_sequence(ll, ml)?;
        seq_output.extend_literals_range(literals, 0)?;

        let off = offset as usize;
        let out_pos = seq_output.len() - output_base;
        if HAS_HISTORY {
            if likely(off <= out_pos) {
                seq_output.copy_match_single(off)?;
            } else {
                seq_output.copy_match_from_history(history, off, out_pos)?;
            }
        } else {
            if unlikely(off == 0 || off > out_pos) {
                return Err(DecompressError::InvalidOffset);
            }
            seq_output.copy_match_single(off)?;
        }
    }

    if !rev_reader.is_exhausted() {
        return Err(DecompressError::CorruptSequences);
    }

    if ll < literals.len() {
        let remaining = literals.len() - ll;
        output.extend_literals_range(literals, ll, remaining)?;
    }

    Ok(())
}
