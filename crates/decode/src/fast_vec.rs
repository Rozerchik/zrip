#[cfg(feature = "alloc")]
use alloc::vec::Vec;

use zrip_core::error::DecompressError;
use zrip_core::hint::unlikely;

pub(crate) const WILDCOPY_OVERLENGTH: usize = 64;

pub(crate) struct BlockOutput<'a> {
    vec: &'a mut Vec<u8>,
    block_limit: usize,
}

pub(crate) struct SequenceOutput<'block, 'vec> {
    output: &'block mut BlockOutput<'vec>,
    literal_len: usize,
    match_len: usize,
}

impl<'a> BlockOutput<'a> {
    #[inline(always)]
    pub(crate) fn new(vec: &'a mut Vec<u8>, max_block_size: usize) -> Self {
        vec.reserve(max_block_size + WILDCOPY_OVERLENGTH);
        let block_limit = vec.len() + max_block_size;
        Self { vec, block_limit }
    }

    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.vec.len()
    }

    #[inline(always)]
    pub(crate) fn begin_sequence(
        &mut self,
        literal_len: usize,
        match_len: usize,
    ) -> Result<SequenceOutput<'_, 'a>, DecompressError> {
        debug_assert!(self.vec.len() <= self.block_limit);
        let remaining = self.block_limit - self.vec.len();
        if unlikely(literal_len > remaining || match_len > remaining - literal_len) {
            return Err(DecompressError::CorruptSequences);
        }
        Ok(SequenceOutput {
            output: self,
            literal_len,
            match_len,
        })
    }

    #[inline(always)]
    fn ensure_block_space(&self, len: usize) -> Result<(), DecompressError> {
        debug_assert!(self.vec.len() <= self.block_limit);
        if unlikely(len > self.block_limit - self.vec.len()) {
            return Err(DecompressError::CorruptSequences);
        }
        Ok(())
    }

    #[inline(always)]
    pub(crate) fn extend_literals_range(
        &mut self,
        src: &[u8],
        start: usize,
        len: usize,
    ) -> Result<(), DecompressError> {
        self.extend_slice_range(src, start, len, DecompressError::CorruptLiterals)
    }

    #[inline(always)]
    fn extend_slice_range(
        &mut self,
        src: &[u8],
        start: usize,
        len: usize,
        bounds_error: DecompressError,
    ) -> Result<(), DecompressError> {
        if unlikely(start > src.len() || len > src.len() - start) {
            return Err(bounds_error);
        }
        self.ensure_block_space(len)?;
        self.extend_slice_range_in_block(src, start, len);
        Ok(())
    }

    #[inline(always)]
    fn extend_slice_range_in_block(&mut self, src: &[u8], start: usize, len: usize) {
        // Short runs append a fixed 16 bytes and truncate: a constant-size
        // copy has no length-dependent branches. `new` reserved the slack,
        // so neither step reallocates.
        if len <= 16 && src.len() - start >= 16 {
            let end = self.vec.len() + len;
            self.vec.extend_from_slice(&src[start..start + 16]);
            self.vec.truncate(end);
        } else {
            self.vec.extend_from_slice(&src[start..start + len]);
        }
    }
}

impl SequenceOutput<'_, '_> {
    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.output.len()
    }

    #[inline(always)]
    pub(crate) fn extend_literals_range(
        &mut self,
        src: &[u8],
        start: usize,
    ) -> Result<(), DecompressError> {
        self.extend_slice_range(
            src,
            start,
            self.literal_len,
            DecompressError::CorruptLiterals,
        )
    }

    #[inline(always)]
    fn extend_slice_range(
        &mut self,
        src: &[u8],
        start: usize,
        len: usize,
        bounds_error: DecompressError,
    ) -> Result<(), DecompressError> {
        if unlikely(start > src.len() || len > src.len() - start) {
            return Err(bounds_error);
        }
        self.output.extend_slice_range_in_block(src, start, len);
        Ok(())
    }

    #[inline(always)]
    pub(crate) fn copy_match_single(&mut self, offset: usize) -> Result<(), DecompressError> {
        if unlikely(offset == 0 || offset > self.output.vec.len()) {
            return Err(DecompressError::InvalidOffset);
        }
        let len = self.match_len;
        if len == 0 {
            return Ok(());
        }

        wild_copy_match(self.output.vec, offset, len);
        Ok(())
    }

    #[inline(always)]
    pub(crate) fn copy_match_from_history(
        &mut self,
        history: &[u8],
        offset: usize,
        out_pos: usize,
    ) -> Result<(), DecompressError> {
        if unlikely(offset <= out_pos || offset > out_pos + history.len()) {
            return Err(DecompressError::InvalidOffset);
        }

        let history_reach = offset - out_pos;
        let history_start = history.len() - history_reach;
        let from_history = history_reach.min(self.match_len);
        self.extend_slice_range(
            history,
            history_start,
            from_history,
            DecompressError::InvalidOffset,
        )?;

        let remaining = self.match_len - from_history;
        if remaining > 0 {
            self.copy_match_len(offset, remaining)?;
        }
        Ok(())
    }

    #[inline(always)]
    fn copy_match_len(&mut self, offset: usize, len: usize) -> Result<(), DecompressError> {
        if unlikely(offset == 0 || offset > self.output.vec.len()) {
            return Err(DecompressError::InvalidOffset);
        }
        if len == 0 {
            return Ok(());
        }

        wild_copy_match(self.output.vec, offset, len);
        Ok(())
    }
}

/// Bytes the fast sequence path reads past the end of a literal run.
pub(crate) const LITERAL_OVERREAD: usize = 32;

/// A block's literals and the slack after them.
///
/// The fast sequence path copies literals in whole chunks and reads up to
/// [`LITERAL_OVERREAD`] bytes past a run, so it serves runs whose end plus
/// that overread is at most `fast_end`: inside the literals and inside the
/// bytes. The exact path serves the rest.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Literals<'a> {
    bytes: &'a [u8],
    len: usize,
    fast_end: usize,
}

impl<'a> Literals<'a> {
    /// The first `len` bytes of `bytes` are the literals; any bytes after
    /// them are slack the copies may read.
    pub(crate) fn new(bytes: &'a [u8], len: usize) -> Self {
        debug_assert!(len <= bytes.len());
        let len = len.min(bytes.len());
        Self {
            bytes,
            len,
            fast_end: (len + LITERAL_OVERREAD).min(bytes.len()),
        }
    }

    /// Number of literals.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// The literals without the slack.
    pub(crate) fn as_slice(&self) -> &'a [u8] {
        &self.bytes[..self.len]
    }
}

/// Bytes added to an overlapping match's source after its first four bytes,
/// per offset below eight (C zstd's `dec32table`).
const SPREAD_ADD: [usize; 8] = [0, 1, 2, 1, 4, 4, 4, 4];
/// Bytes then subtracted from the source (C zstd's `dec64table`).
const SPREAD_SUB: [usize; 8] = [8, 8, 8, 7, 8, 9, 10, 11];

/// One block's sequence output.
///
/// The caller's vector is moved in for the block, so its pointer and length
/// stay in registers. Behind a `&mut Vec`, every byte store may alias the
/// vector's header, so each copy reloads and stores the length, which
/// serializes the copies. For the same reason no `&mut SeqOut` leaves the
/// inlined hot path: the cold path gets the buffer and positions as values.
///
/// `new` zeroes the block's maximum plus [`WILDCOPY_OVERLENGTH`] bytes, so
/// the copies index an initialized slice.
pub(crate) struct SeqOut {
    vec: Vec<u8>,
    /// Bytes of output; every byte below it is initialized.
    op: usize,
    /// Largest `op` the block may reach.
    limit: usize,
    /// Lowest byte a match may copy from: the frame's first byte.
    frame_start: usize,
}

impl SeqOut {
    /// Takes `vec` for a block of at most `max_block` bytes whose frame starts
    /// at `frame_start`.
    #[inline(always)]
    pub(crate) fn new(mut vec: Vec<u8>, max_block: usize, frame_start: usize) -> Self {
        let op = vec.len();
        vec.reserve(max_block + WILDCOPY_OVERLENGTH);
        vec.resize(op + max_block + WILDCOPY_OVERLENGTH, 0);
        Self {
            vec,
            op,
            limit: op + max_block,
            frame_start: frame_start.min(op),
        }
    }

    /// Returns the vector, holding exactly the output written so far.
    #[inline(always)]
    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.vec.truncate(self.op);
        self.vec
    }

    /// The buffer, valid for writes up to `limit + WILDCOPY_OVERLENGTH`.
    #[inline(always)]
    fn dst(&mut self) -> Dst<'_> {
        Dst(&mut self.vec[..])
    }

    /// Executes one sequence: `ll` literals from `lits[lp..]`, then `ml`
    /// bytes copied from `offset` bytes back. A match may reach before the
    /// frame into `history`.
    ///
    /// The fast path makes three tests: the block's limit, the literals'
    /// end against their slack, and the offset. Everything else, including the error checks
    /// in their original order, is in the cold [`execute_exact`].
    #[inline(always)]
    pub(crate) fn execute(
        &mut self,
        lits: Literals<'_>,
        lp: usize,
        ll: usize,
        ml: usize,
        offset: usize,
        history: &[u8],
    ) -> Result<(), DecompressError> {
        let op = self.op;
        let frame_start = self.frame_start;
        let lit_end = op + ll;
        let match_end = lit_end + ml;
        if unlikely(match_end > self.limit) {
            return Err(DecompressError::CorruptSequences);
        }
        if unlikely(
            lp + ll + LITERAL_OVERREAD > lits.fast_end
                || offset.wrapping_sub(1) >= lit_end - frame_start,
        ) {
            let span = Span {
                op,
                ll,
                ml,
                frame_start,
            };
            self.op = execute_exact(self.dst(), span, lits.as_slice(), lp, offset, history)?;
            return Ok(());
        }
        let mut dst = self.dst();
        dst.wild_literals(op, lits.bytes, lp, ll);
        dst.wild_match(lit_end, offset, ml, frame_start);
        self.op = match_end;
        Ok(())
    }

    /// Appends `lits[lp..lp + len]` exactly.
    #[inline(always)]
    pub(crate) fn extend_literals(
        &mut self,
        lits: Literals<'_>,
        lp: usize,
        len: usize,
    ) -> Result<(), DecompressError> {
        let lits = lits.as_slice();
        if unlikely(lp > lits.len() || len > lits.len() - lp) {
            return Err(DecompressError::CorruptLiterals);
        }
        if unlikely(len > self.limit - self.op) {
            return Err(DecompressError::CorruptSequences);
        }
        let op = self.op;
        self.dst().put(op, &lits[lp..lp + len]);
        self.op += len;
        Ok(())
    }
}

/// A sequence's place in the output: where it starts, its literal and match
/// lengths, and the frame's first byte.
#[derive(Clone, Copy)]
struct Span {
    op: usize,
    ll: usize,
    ml: usize,
    frame_start: usize,
}

/// A sequence near the end of the literals, or whose offset reaches before
/// the frame or is invalid; returns the new output length. The caller
/// checked that `span` ends within the block's limit.
#[cold]
#[inline(never)]
fn execute_exact(
    mut dst: Dst<'_>,
    span: Span,
    lits: &[u8],
    lp: usize,
    offset: usize,
    history: &[u8],
) -> Result<usize, DecompressError> {
    let Span {
        op,
        ll,
        ml,
        frame_start,
    } = span;
    if lp > lits.len() || ll > lits.len() - lp {
        return Err(DecompressError::CorruptLiterals);
    }
    dst.put(op, &lits[lp..lp + ll]);
    let lit_end = op + ll;
    let reach = lit_end - frame_start;
    if offset == 0 {
        return Err(DecompressError::InvalidOffset);
    }
    let mut done = 0;
    if offset > reach {
        // The match starts in the history before the frame and continues
        // from the frame's first byte.
        let before = offset - reach;
        if before > history.len() {
            return Err(DecompressError::InvalidOffset);
        }
        let start = history.len() - before;
        done = before.min(ml);
        dst.put(lit_end, &history[start..start + done]);
    }
    if ml > done {
        dst.wild_match(lit_end + done, offset, ml - done, frame_start);
    }
    Ok(lit_end + ml)
}

/// A block's output buffer as the copies see it, zeroed past the output.
/// Valid for writes up to the block's limit plus [`WILDCOPY_OVERLENGTH`];
/// every byte below the output length is initialized.
struct Dst<'a>(&'a mut [u8]);

impl Dst<'_> {
    /// Writes `bytes` at `at`; `at + bytes.len()` is within the limit.
    #[inline(always)]
    fn put(&mut self, at: usize, bytes: &[u8]) {
        self.0[at..at + bytes.len()].copy_from_slice(bytes);
    }

    /// Copies `len` literals from `lits[lp..]` to `at` in whole chunks: 16
    /// bytes, then 32-byte chunks. Requires `lp + len + LITERAL_OVERREAD <=
    /// lits.len()` and `at + len` within the limit.
    #[inline(always)]
    fn wild_literals(&mut self, at: usize, lits: &[u8], lp: usize, len: usize) {
        debug_assert!(lp + len + LITERAL_OVERREAD <= lits.len());
        self.0[at..at + 16].copy_from_slice(&lits[lp..lp + 16]);
        let mut i = 16;
        while i < len {
            self.0[at + i..at + i + 32].copy_from_slice(&lits[lp + i..lp + i + 32]);
            i += 32;
        }
    }

    /// Copies a match of `len` bytes from `offset` bytes before `at` in
    /// whole chunks. Requires `1 <= offset <= at - frame_start` and
    /// `at + len` within the limit.
    #[inline(always)]
    fn wild_match(&mut self, at: usize, offset: usize, len: usize, frame_start: usize) {
        debug_assert!(offset >= 1 && offset <= at - frame_start);
        let _ = frame_start;
        let out = &mut *self.0;
        let src = at - offset;
        if offset >= 32 {
            let mut i = 0;
            loop {
                chunk::<32>(out, src + i, at + i);
                i += 32;
                if i >= len {
                    break;
                }
            }
        } else if offset >= 16 {
            let mut i = 0;
            loop {
                chunk::<16>(out, src + i, at + i);
                i += 16;
                if i >= len {
                    break;
                }
            }
        } else {
            let mut src = src;
            if offset < 8 {
                for k in 0..4 {
                    out[at + k] = out[src + k];
                }
                src += SPREAD_ADD[offset];
                chunk::<4>(out, src, at + 4);
                src = src + 8 - SPREAD_SUB[offset];
            } else {
                chunk::<8>(out, src, at);
                src += 8;
            }
            let mut i = 8;
            while i < len {
                chunk::<8>(out, src, at + i);
                src += 8;
                i += 8;
            }
        }
    }
}

/// Copies `N` bytes within `out`, reading them all before writing.
#[inline(always)]
fn chunk<const N: usize>(out: &mut [u8], src: usize, dst: usize) {
    let mut value = [0u8; N];
    value.copy_from_slice(&out[src..src + N]);
    out[dst..dst + N].copy_from_slice(&value);
}

#[inline(always)]
fn wild_copy_match(vec: &mut Vec<u8>, offset: usize, len: usize) {
    let start = vec.len() - offset;
    if offset >= len {
        vec.extend_from_within(start..start + len);
    } else if offset == 1 {
        vec.resize(vec.len() + len, vec[start]);
    } else {
        vec.extend_from_within(start..start + offset);
        let mut copied = offset;
        while copied < len {
            let n = (len - copied).min(copied);
            let src = vec.len() - copied;
            vec.extend_from_within(src..src + n);
            copied += n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_lengths(max_len: usize) -> Vec<usize> {
        (1..=max_len).collect()
    }

    fn expected_match(seed: &[u8], offset: usize, len: usize) -> Vec<u8> {
        let mut expected = seed.to_vec();
        let start = expected.len() - offset;
        for i in 0..len {
            expected.push(expected[start + i % offset]);
        }
        expected
    }

    /// Runs one sequence through `SeqOut::execute` after `prefix`, with
    /// `slack` readable bytes after the literals, and returns the output.
    fn seq_out_run(
        prefix: &[u8],
        lits: &[u8],
        slack: usize,
        ml: usize,
        offset: usize,
        history: &[u8],
    ) -> Result<Vec<u8>, DecompressError> {
        let mut buf = lits.to_vec();
        buf.resize(lits.len() + slack, 0xAA);
        let mut out = SeqOut::new(prefix.to_vec(), lits.len() + ml, 0);
        let result = out.execute(
            Literals::new(&buf, lits.len()),
            0,
            lits.len(),
            ml,
            offset,
            history,
        );
        let v = out.finish();
        result.map(|()| v)
    }

    fn check_wild_copy_match_offsets(start_offset: usize, end_offset: usize) {
        for offset in start_offset..=end_offset {
            let seed: Vec<u8> = (0..offset).map(|i| (i as u8).wrapping_mul(37)).collect();
            for len in test_lengths(128) {
                let expected = expected_match(&seed, offset, len);
                // With slack the fast path copies whole chunks; without it
                // the exact path runs.
                for slack in [LITERAL_OVERREAD, 0] {
                    let v = seq_out_run(&seed, &[], slack, len, offset, &[]).unwrap();
                    assert_eq!(v, expected, "offset={offset} len={len} slack={slack}");
                }
            }
        }
    }

    fn check_wild_copy_match_single_offsets(start_offset: usize, end_offset: usize) {
        for offset in start_offset..=end_offset {
            for len in test_lengths(256) {
                let mut v = Vec::new();
                let seed: Vec<u8> = (0..offset).map(|i| (i as u8).wrapping_mul(37)).collect();
                v.extend_from_slice(&seed);
                let expected = expected_match(&v, offset, len);
                let mut output = BlockOutput::new(&mut v, len);
                let mut sequence = output.begin_sequence(0, len).unwrap();
                sequence.copy_match_single(offset).unwrap();
                assert_eq!(
                    &v[..offset + len],
                    &expected[..offset + len],
                    "offset={offset} len={len}"
                );
            }
        }
    }

    #[test]
    fn fast_extend_from_slice_all_sizes() {
        // `tail` bytes of source follow the copied run: with 16 or more the
        // copy reads a fixed 16 bytes, with fewer it copies exactly.
        for len in 0..=64 {
            for tail in [0, 1, 15, 16, 17, 40] {
                for start in [0, 3] {
                    let src: Vec<u8> = (0..(start + len + tail) as u8).collect();
                    let mut dst = vec![0xEEu8; 5];
                    {
                        let mut output = BlockOutput::new(&mut dst, len);
                        output.extend_literals_range(&src, start, len).unwrap();
                    }
                    assert_eq!(dst[..5], [0xEE; 5]);
                    assert_eq!(dst[5..], src[start..start + len], "len={len} tail={tail}");
                }
            }
        }
    }

    #[test]
    fn block_output_extends_literal_range() {
        let mut dst = Vec::new();
        {
            let mut output = BlockOutput::new(&mut dst, 16);
            output
                .extend_literals_range(b"abcdefgh", 2, 4)
                .expect("literal range should fit");
        }
        assert_eq!(dst, b"cdef");
    }

    #[test]
    fn block_output_rejects_literal_range_oob() {
        let mut dst = Vec::new();
        let mut output = BlockOutput::new(&mut dst, 16);
        let err = output
            .extend_literals_range(b"abc", 2, 4)
            .expect_err("literal range should be out of bounds");
        assert_eq!(err, DecompressError::CorruptLiterals);
    }

    #[test]
    fn block_output_rejects_block_overflow() {
        let mut dst = Vec::new();
        let mut output = BlockOutput::new(&mut dst, 4);
        let err = output
            .extend_literals_range(b"abcde", 0, 5)
            .expect_err("literal range should exceed the block limit");
        assert_eq!(err, DecompressError::CorruptSequences);
    }

    #[test]
    fn seq_out_copies_literals_then_matches() {
        let prefix: Vec<u8> = (0..40u8).map(|i| i.wrapping_mul(29) ^ 0x5A).collect();
        let lits_all: Vec<u8> = (0..100u8).map(|i| i ^ 0xC3).collect();
        for ll in 0..=70 {
            for ml in [3, 17, 40, 100] {
                for offset in [1, 2, 3, 7, 8, 15, 16, 31, 32, 40 + ll] {
                    let lits = &lits_all[..ll];
                    let mut expected = prefix.clone();
                    expected.extend_from_slice(lits);
                    let expected = expected_match(&expected, offset, ml);
                    for slack in [LITERAL_OVERREAD, 5, 0] {
                        let v = seq_out_run(&prefix, lits, slack, ml, offset, &[]).unwrap();
                        assert_eq!(v, expected, "ll={ll} ml={ml} offset={offset} slack={slack}");
                    }
                }
            }
        }
    }

    #[test]
    fn seq_out_rejects_bad_offsets() {
        for slack in [LITERAL_OVERREAD, 0] {
            for offset in [0, 5, 100] {
                let err = seq_out_run(b"abcd", &[], slack, 4, offset, &[]).unwrap_err();
                assert_eq!(err, DecompressError::InvalidOffset, "offset={offset}");
            }
        }
        // A match may not reach before the frame's first byte.
        let mut out = SeqOut::new(b"abcdef".to_vec(), 8, 4);
        let lits = [0u8; LITERAL_OVERREAD];
        assert_eq!(
            out.execute(Literals::new(&lits, 0), 0, 0, 4, 3, &[]),
            Err(DecompressError::InvalidOffset)
        );
        assert_eq!(
            out.execute(Literals::new(&lits, 0), 0, 0, 4, 2, &[]),
            Ok(())
        );
        assert_eq!(out.finish(), b"abcdefefef");
    }

    #[test]
    fn seq_out_rejects_overruns() {
        let lits = [7u8; 10];
        // More literals than the section holds.
        let mut out = SeqOut::new(b"ab".to_vec(), 32, 0);
        assert_eq!(
            out.execute(Literals::new(&lits, 10), 4, 8, 3, 1, &[]),
            Err(DecompressError::CorruptLiterals)
        );
        // A sequence past the block's limit.
        let mut out = SeqOut::new(b"ab".to_vec(), 8, 0);
        assert_eq!(
            out.execute(Literals::new(&lits, 10), 0, 4, 5, 1, &[]),
            Err(DecompressError::CorruptSequences)
        );
        assert_eq!(
            out.extend_literals(Literals::new(&lits, 10), 0, 9),
            Err(DecompressError::CorruptSequences)
        );
        assert_eq!(
            out.extend_literals(Literals::new(&lits, 10), 4, 7),
            Err(DecompressError::CorruptLiterals)
        );
        assert_eq!(out.extend_literals(Literals::new(&lits, 10), 4, 6), Ok(()));
        assert_eq!(out.finish(), b"ab\x07\x07\x07\x07\x07\x07");
    }

    #[test]
    fn seq_out_copies_matches_from_history() {
        let history = b"abcdefghijklmnop";
        let prefix = b"qrst";
        let lits = b"uv";
        for offset in [7, 8, 20, 22] {
            for ml in [1, 2, 10, 30] {
                let mut combined = history.to_vec();
                combined.extend_from_slice(prefix);
                combined.extend_from_slice(lits);
                let expected = expected_match(&combined, offset, ml);
                for slack in [LITERAL_OVERREAD, 0] {
                    let v = seq_out_run(prefix, lits, slack, ml, offset, history).unwrap();
                    assert_eq!(v, expected[history.len()..], "offset={offset} ml={ml}");
                }
            }
        }
        let err = seq_out_run(prefix, lits, 0, 3, 23, history).unwrap_err();
        assert_eq!(err, DecompressError::InvalidOffset);
    }

    #[test]
    fn block_output_copies_match_from_history() {
        let history = b"abcdefghijklmnop";
        let mut dst = b"qrst".to_vec();
        let out_pos = dst.len();
        let offset = 8;
        let match_len = 20;

        let mut expected = dst.clone();
        for i in 0..match_len {
            let src = history.len() + out_pos - offset + i;
            let byte = if src < history.len() {
                history[src]
            } else {
                expected[src - history.len()]
            };
            expected.push(byte);
        }

        {
            let mut output = BlockOutput::new(&mut dst, 32);
            let mut sequence = output
                .begin_sequence(0, match_len)
                .expect("sequence should fit block");
            sequence
                .copy_match_from_history(history, offset, out_pos)
                .expect("history match should fit");
        }

        assert_eq!(dst, expected);
    }

    #[test]
    fn wild_copy_match_offsets_1_8() {
        check_wild_copy_match_offsets(1, 8);
    }

    #[test]
    fn wild_copy_match_offsets_9_16() {
        check_wild_copy_match_offsets(9, 16);
    }

    #[test]
    fn wild_copy_match_offsets_17_32() {
        check_wild_copy_match_offsets(17, 32);
    }

    #[test]
    fn wild_copy_match_offsets_33_64() {
        check_wild_copy_match_offsets(33, 64);
    }

    #[test]
    fn wild_copy_match_single_offsets_1_8() {
        check_wild_copy_match_single_offsets(1, 8);
    }

    #[test]
    fn wild_copy_match_single_offsets_9_16() {
        check_wild_copy_match_single_offsets(9, 16);
    }

    #[test]
    fn wild_copy_match_single_offsets_17_32() {
        check_wild_copy_match_single_offsets(17, 32);
    }

    #[test]
    fn wild_copy_match_single_offsets_33_64() {
        check_wild_copy_match_single_offsets(33, 64);
    }

    #[test]
    fn wild_copy_match_single_offsets_65_96() {
        check_wild_copy_match_single_offsets(65, 96);
    }

    #[test]
    fn wild_copy_match_single_offsets_97_128() {
        check_wild_copy_match_single_offsets(97, 128);
    }
}
