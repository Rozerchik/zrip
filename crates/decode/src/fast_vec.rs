#![cfg_attr(feature = "paranoid", forbid(unsafe_code))]

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
        #[cfg(not(feature = "paranoid"))]
        unsafe {
            debug_assert!(self.vec.len() + len + WILDCOPY_OVERLENGTH <= self.vec.capacity());
            // SAFETY: The caller proves `src[start..start + len]` is readable
            // and that this write stays in the reserved block output range.
            fast_extend_from_ptr(self.vec, src.as_ptr().add(start), len, src.len() - start);
        }
        #[cfg(feature = "paranoid")]
        {
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

        #[cfg(not(feature = "paranoid"))]
        unsafe {
            debug_assert!(
                self.output.vec.len() + len + WILDCOPY_OVERLENGTH <= self.output.vec.capacity()
            );
            // SAFETY: `begin_sequence` proves capacity for the whole sequence,
            // and the offset check proves an initialized source.
            wild_copy_match_single_unchecked(self.output.vec, offset, len);
        }
        #[cfg(feature = "paranoid")]
        {
            wild_copy_match_paranoid(self.output.vec, offset, len);
        }
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

        #[cfg(not(feature = "paranoid"))]
        unsafe {
            debug_assert!(
                self.output.vec.len() + len + WILDCOPY_OVERLENGTH <= self.output.vec.capacity()
            );
            // SAFETY: `begin_sequence` proves capacity for the whole sequence.
            // The offset check proves the match source starts in initialized
            // output. Each copy arm handles its overlap pattern.
            wild_copy_match_unchecked(self.output.vec, offset, len);
        }
        #[cfg(feature = "paranoid")]
        {
            wild_copy_match_paranoid(self.output.vec, offset, len);
        }
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
/// `new` reserves the block's maximum plus [`WILDCOPY_OVERLENGTH`] bytes; the
/// paranoid build zeroes that room, so its copies index an initialized slice.
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
        #[cfg(feature = "paranoid")]
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
        #[cfg(not(feature = "paranoid"))]
        // SAFETY: `op` never passes `limit`, inside the reserved capacity, and
        // every byte below it is the vector's original content or was written
        // by a copy.
        unsafe {
            self.vec.set_len(self.op);
        }
        #[cfg(feature = "paranoid")]
        self.vec.truncate(self.op);
        self.vec
    }

    /// The buffer, valid for writes up to `limit + WILDCOPY_OVERLENGTH`.
    #[inline(always)]
    fn dst(&mut self) -> Dst<'_> {
        #[cfg(not(feature = "paranoid"))]
        {
            Dst(self.vec.as_mut_ptr(), core::marker::PhantomData)
        }
        #[cfg(feature = "paranoid")]
        {
            Dst(&mut self.vec[..])
        }
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

/// A block's output buffer as the copies see it: the raw pointer, or in the
/// paranoid build the zeroed slice. Valid for writes up to the block's limit
/// plus [`WILDCOPY_OVERLENGTH`]; every byte below the output length is
/// initialized.
#[cfg(not(feature = "paranoid"))]
struct Dst<'a>(*mut u8, core::marker::PhantomData<&'a mut [u8]>);

/// A block's output buffer as the copies see it: the raw pointer, or in the
/// paranoid build the zeroed slice. Valid for writes up to the block's limit
/// plus [`WILDCOPY_OVERLENGTH`]; every byte below the output length is
/// initialized.
#[cfg(feature = "paranoid")]
struct Dst<'a>(&'a mut [u8]);

impl Dst<'_> {
    /// Writes `bytes` at `at`; `at + bytes.len()` is within the limit.
    #[inline(always)]
    fn put(&mut self, at: usize, bytes: &[u8]) {
        #[cfg(not(feature = "paranoid"))]
        // SAFETY: callers keep `at + bytes.len()` within the limit, inside the
        // reserved capacity, and `bytes` never lies in this buffer.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.0.add(at), bytes.len());
        }
        #[cfg(feature = "paranoid")]
        self.0[at..at + bytes.len()].copy_from_slice(bytes);
    }

    /// Copies `len` literals from `lits[lp..]` to `at` in whole chunks: 16
    /// bytes, then 32-byte chunks. Requires `lp + len + LITERAL_OVERREAD <=
    /// lits.len()` and `at + len` within the limit.
    #[inline(always)]
    fn wild_literals(&mut self, at: usize, lits: &[u8], lp: usize, len: usize) {
        debug_assert!(lp + len + LITERAL_OVERREAD <= lits.len());
        #[cfg(not(feature = "paranoid"))]
        // SAFETY: the source holds `LITERAL_OVERREAD` readable bytes past the
        // run, and the destination `WILDCOPY_OVERLENGTH` writable bytes past
        // the limit; the last chunk starts before `len`.
        unsafe {
            let src = lits.as_ptr().add(lp);
            let dst = self.0.add(at);
            copy_16(src, dst);
            let mut i = 16;
            while i < len {
                copy_32_wide(src.add(i), dst.add(i));
                i += 32;
            }
        }
        #[cfg(feature = "paranoid")]
        {
            self.0[at..at + 16].copy_from_slice(&lits[lp..lp + 16]);
            let mut i = 16;
            while i < len {
                self.0[at + i..at + i + 32].copy_from_slice(&lits[lp + i..lp + i + 32]);
                i += 32;
            }
        }
    }

    /// Copies a match of `len` bytes from `offset` bytes before `at` in
    /// whole chunks. Requires `1 <= offset <= at - frame_start` and
    /// `at + len` within the limit.
    #[inline(always)]
    fn wild_match(&mut self, at: usize, offset: usize, len: usize, frame_start: usize) {
        debug_assert!(offset >= 1 && offset <= at - frame_start);
        let _ = frame_start;
        #[cfg(not(feature = "paranoid"))]
        // SAFETY: the source starts at or after the frame's first byte, inside
        // the initialized output, and each chunk reads only bytes before the
        // chunk it writes. Writes end at most 31 bytes past `at + len`, inside
        // the `WILDCOPY_OVERLENGTH` room past the limit.
        unsafe {
            let dst = self.0.add(at);
            let src = dst.sub(offset);
            if offset >= 32 {
                let mut i = 0;
                loop {
                    copy_32_wide(src.add(i), dst.add(i));
                    i += 32;
                    if i >= len {
                        break;
                    }
                }
            } else if offset >= 16 {
                let mut i = 0;
                loop {
                    copy_16(src.add(i), dst.add(i));
                    i += 16;
                    if i >= len {
                        break;
                    }
                }
            } else {
                spread_match(src, dst, offset, len);
            }
        }
        #[cfg(feature = "paranoid")]
        {
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
}

/// Copies `N` bytes within `out`, reading them all before writing.
#[cfg(feature = "paranoid")]
#[inline(always)]
fn chunk<const N: usize>(out: &mut [u8], src: usize, dst: usize) {
    let mut value = [0u8; N];
    value.copy_from_slice(&out[src..src + N]);
    out[dst..dst + N].copy_from_slice(&value);
}

/// Copies an overlapping match with `offset < 16` the C zstd way: eight
/// bytes that spread the period to a distance of at least eight
/// (`ZSTD_overlapCopy8`), then eight-byte chunks.
///
/// # Safety
///
/// `src = dst - offset` with `1 <= offset < 16`, `src..dst` initialized,
/// and `dst..dst + len + 8` writable.
#[cfg(not(feature = "paranoid"))]
#[inline(always)]
unsafe fn spread_match(src: *const u8, dst: *mut u8, offset: usize, len: usize) {
    debug_assert!((1..16).contains(&offset));
    // SAFETY: see the function contract. Each byte or chunk is read only
    // after every byte it covers was written: the first four bytes go one at
    // a time, the spread then leaves the source at least eight bytes behind.
    unsafe {
        let mut src = src;
        if offset < 8 {
            *dst = *src;
            *dst.add(1) = *src.add(1);
            *dst.add(2) = *src.add(2);
            *dst.add(3) = *src.add(3);
            src = src.add(SPREAD_ADD[offset]);
            let word = (src as *const u32).read_unaligned();
            (dst.add(4) as *mut u32).write_unaligned(word);
            src = src.add(8).sub(SPREAD_SUB[offset]);
        } else {
            copy_8(src, dst);
            src = src.add(8);
        }
        let mut i = 8;
        while i < len {
            copy_8(src, dst.add(i));
            src = src.add(8);
            i += 8;
        }
    }
}

/// 32-byte copy as one load and one store.
///
/// # Safety
///
/// `src..src+32` must be readable and `dst..dst+32` writable.
#[cfg(not(feature = "paranoid"))]
#[inline(always)]
unsafe fn copy_32_wide(src: *const u8, dst: *mut u8) {
    // SAFETY: the caller provides readable and writable 32-byte ranges;
    // unaligned accesses are intentional.
    unsafe {
        let v = (src as *const [u8; 32]).read_unaligned();
        (dst as *mut [u8; 32]).write_unaligned(v);
    }
}

/// 16-byte copy via two u64 load/stores.
///
/// # Safety
///
/// `src..src+16` must be readable and `dst..dst+16` must be writable. The
/// regions may overlap because both reads complete before either write.
#[cfg(not(feature = "paranoid"))]
#[inline(always)]
unsafe fn copy_16(src: *const u8, dst: *mut u8) {
    debug_assert!(!src.is_null());
    debug_assert!(!dst.is_null());
    // SAFETY: The caller provides readable/writable 16-byte ranges. Unaligned
    // accesses are intentional and do not require pointer alignment.
    unsafe {
        let a = (src as *const u64).read_unaligned();
        let b = (src.add(8) as *const u64).read_unaligned();
        (dst as *mut u64).write_unaligned(a);
        (dst.add(8) as *mut u64).write_unaligned(b);
    }
}

/// # Safety
///
/// `src..src+8` must be readable and `dst..dst+8` must be writable.
#[cfg(not(feature = "paranoid"))]
#[inline(always)]
unsafe fn copy_8(src: *const u8, dst: *mut u8) {
    debug_assert!(!src.is_null());
    debug_assert!(!dst.is_null());
    // SAFETY: The caller provides readable/writable 8-byte ranges. Unaligned
    // accesses are intentional and do not require pointer alignment.
    unsafe {
        let v = (src as *const u64).read_unaligned();
        (dst as *mut u64).write_unaligned(v);
    }
}

/// # Safety
///
/// `src..src+32` must be readable and `dst..dst+32` must be writable.
#[cfg(not(feature = "paranoid"))]
#[inline(always)]
unsafe fn copy_32(src: *const u8, dst: *mut u8) {
    debug_assert!(!src.is_null());
    debug_assert!(!dst.is_null());
    // SAFETY: The caller provides readable/writable 32-byte ranges.
    unsafe {
        copy_16(src, dst);
        copy_16(src.add(16), dst.add(16));
    }
}

/// # Safety
///
/// `src..src+64` must be readable and `dst..dst+64` must be writable.
#[cfg(not(feature = "paranoid"))]
#[inline(always)]
unsafe fn copy_64(src: *const u8, dst: *mut u8) {
    debug_assert!(!src.is_null());
    debug_assert!(!dst.is_null());
    // SAFETY: The caller provides readable/writable 64-byte ranges.
    unsafe {
        copy_16(src, dst);
        copy_16(src.add(16), dst.add(16));
        copy_16(src.add(32), dst.add(32));
        copy_16(src.add(48), dst.add(48));
    }
}

/// Copy `src` into the end of `vec` using 16-byte chunk copies.
///
/// All reads stay within the `avail` readable source bytes. When at least 16
/// are available, the first 16 bytes are copied unconditionally, as C zstd
/// does: the copy length then needs no size-class branches, which mispredict
/// on the varying literal lengths of real data.
///
/// # Safety
///
/// `sp..sp+avail` must be readable with `len <= avail`, and `vec` must have
/// at least `len + 16` bytes of spare capacity from its current end.
#[cfg(not(feature = "paranoid"))]
#[inline(always)]
unsafe fn fast_extend_from_ptr(vec: &mut Vec<u8>, sp: *const u8, len: usize, avail: usize) {
    debug_assert!(len <= avail);
    debug_assert!(vec.len() + len + 16 <= vec.capacity());
    // SAFETY: The caller supplies `avail` readable source bytes and enough
    // spare capacity for the destination, including the 16-byte headroom.
    unsafe {
        let dst = vec.as_mut_ptr().add(vec.len());
        if avail >= 16 {
            copy_16(sp, dst);
            let mut off = 16usize;
            while off < len {
                copy_16(sp.add(off.min(len - 16)), dst.add(off.min(len - 16)));
                off += 16;
            }
            vec.set_len(vec.len() + len);
            return;
        }
        if len == 0 {
            return;
        }
        if len >= 16 {
            let mut off = 0usize;
            while off + 16 <= len {
                copy_16(sp.add(off), dst.add(off));
                off += 16;
            }
            if off < len {
                copy_16(sp.add(len - 16), dst.add(len - 16));
            }
        } else if len >= 8 {
            copy_8(sp, dst);
            copy_8(sp.add(len - 8), dst.add(len - 8));
        } else if len >= 4 {
            let a = (sp as *const u32).read_unaligned();
            (dst as *mut u32).write_unaligned(a);
            let b = (sp.add(len - 4) as *const u32).read_unaligned();
            (dst.add(len - 4) as *mut u32).write_unaligned(b);
        } else if len == 3 {
            let a = (sp as *const u16).read_unaligned();
            (dst as *mut u16).write_unaligned(a);
            *dst.add(2) = *sp.add(2);
        } else if len == 2 {
            let a = (sp as *const u16).read_unaligned();
            (dst as *mut u16).write_unaligned(a);
        } else {
            *dst = *sp;
        }
        vec.set_len(vec.len() + len);
    }
}

/// Build an 8-byte repeating pattern from `offset` bytes at `src`.
/// Only reads the first `offset` bytes (no out-of-bounds access).
///
/// # Safety
///
/// `src..src+offset` must be readable, with offset in 2..=7.
#[cfg(not(feature = "paranoid"))]
#[inline(always)]
unsafe fn build_pattern_u64(src: *const u8, offset: usize) -> u64 {
    debug_assert!((2..=7).contains(&offset));
    let mut buf = [0u8; 8];
    // SAFETY: The caller provides offset readable bytes. The remaining copies
    // operate wholly inside the local 8-byte buffer.
    unsafe {
        let p = buf.as_mut_ptr();
        core::ptr::copy_nonoverlapping(src, p, offset);
        let mut have = offset;
        while have < 8 {
            let n = have.min(8 - have);
            core::ptr::copy_nonoverlapping(p, p.add(have), n);
            have += n;
        }
    }
    u64::from_ne_bytes(buf)
}

/// Copy a match of `len` bytes at `offset` bytes back from the end of `vec`.
///
/// Handles all cases: non-overlapping (offset >= 16), offset 8..15,
/// RLE (offset == 1), and overlapping small-offset copies (2..7).
///
/// # Safety contract (upheld by caller):
/// - `offset > 0 && offset <= vec.len()`
/// - `vec.len() + len + 16 <= vec.capacity()`
#[cfg(not(feature = "paranoid"))]
#[inline(always)]
unsafe fn wild_copy_match_unchecked(vec: &mut Vec<u8>, offset: usize, len: usize) {
    debug_assert!(offset > 0 && offset <= vec.len());
    debug_assert!(vec.len() + len + 16 <= vec.capacity());
    // SAFETY: offset is validated above, and reserve ensures enough output
    // headroom. Each copy arm handles its overlap pattern.
    unsafe {
        let ptr = vec.as_mut_ptr();
        let op = ptr.add(vec.len());
        let src = op.sub(offset);

        if offset >= 16 {
            let mut off = 0usize;
            loop {
                copy_16(src.add(off), op.add(off));
                off += 16;
                if off >= len {
                    break;
                }
            }
        } else if offset == 1 {
            core::ptr::write_bytes(op, *src, len + 16);
        } else if offset >= 8 {
            // 8-byte copies tracking src; overlap at boundary is fine because
            // read_unaligned completes before write_unaligned.
            copy_8(src, op);
            copy_8(src.add(8), op.add(8));
            if len > 16 {
                let mut off = 16usize;
                loop {
                    copy_8(src.add(off), op.add(off));
                    copy_8(src.add(off + 8), op.add(off + 8));
                    off += 16;
                    if off >= len {
                        break;
                    }
                }
            }
        } else {
            // Offset 2..7: build 8-byte repeating pattern from individual source
            // bytes (avoids reading uninitialized destination memory), then stamp it
            // at offset-aligned positions.
            let pat64 = build_pattern_u64(src, offset);
            let mut off = 0usize;
            loop {
                (op.add(off) as *mut u64).write_unaligned(pat64);
                off += offset;
                if off >= len {
                    break;
                }
            }
        }
        vec.set_len(vec.len() + len);
    }
}

#[cfg(feature = "paranoid")]
#[inline(always)]
fn wild_copy_match_paranoid(vec: &mut Vec<u8>, offset: usize, len: usize) {
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

/// Variant for one-sequence tiny frames. The caller reserves 64 bytes of
/// headroom, so non-overlapping copies can use wider chunks without changing
/// the generic multi-sequence path.
#[cfg(not(feature = "paranoid"))]
#[inline(always)]
unsafe fn wild_copy_match_single_unchecked(vec: &mut Vec<u8>, offset: usize, len: usize) {
    debug_assert!(offset > 0 && offset <= vec.len());
    debug_assert!(vec.len() + len + 64 <= vec.capacity());
    // SAFETY: offset is validated above, and reserve ensures the 64 bytes of
    // wildcopy headroom required by this single-sequence variant.
    unsafe {
        let ptr = vec.as_mut_ptr();
        let op = ptr.add(vec.len());
        let src = op.sub(offset);

        if len <= 16 && offset >= 16 {
            copy_16(src, op);
        } else if offset >= 64 {
            let mut off = 0usize;
            loop {
                copy_64(src.add(off), op.add(off));
                off += 64;
                if off >= len {
                    break;
                }
            }
        } else if offset >= 32 {
            let mut off = 0usize;
            loop {
                copy_32(src.add(off), op.add(off));
                off += 32;
                if off >= len {
                    break;
                }
            }
        } else {
            wild_copy_match_unchecked(vec, offset, len);
            return;
        }
        vec.set_len(vec.len() + len);
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

// Run with: cargo kani -p zrip-decode -j4 --output-format terse
#[cfg(all(kani, not(feature = "paranoid")))]
mod kani_proofs {
    use super::*;

    // -- Arithmetic proof: BlockOutput capacity invariant --

    /// BlockOutput::new reserves block_size + WILDCOPY_OVERLENGTH spare
    /// capacity. Prove this is sufficient for any valid sequence's
    /// literal copy (needing 16 bytes headroom) and match copy (needing
    /// up to WILDCOPY_OVERLENGTH = 64 bytes headroom).
    #[kani::proof]
    fn block_output_capacity_sufficient() {
        let initial_len: usize = kani::any();
        let block_size: usize = kani::any();
        kani::assume(initial_len <= 64);
        kani::assume(block_size >= 1 && block_size <= 64);

        let capacity = initial_len + block_size + WILDCOPY_OVERLENGTH;
        let literal_len: usize = kani::any();
        let match_len: usize = kani::any();
        kani::assume(literal_len <= block_size);
        kani::assume(match_len <= block_size - literal_len);

        let vec_len_after_literals = initial_len + literal_len;

        // wild_copy_match_unchecked: 16-byte headroom
        assert!(vec_len_after_literals + match_len + 16 <= capacity);
        // wild_copy_match_{16plus,single}_unchecked: 64-byte headroom
        assert!(vec_len_after_literals + match_len + WILDCOPY_OVERLENGTH <= capacity);
    }

    // -- Primitive: build_pattern_u64 --

    /// The inner doubling loop fills exactly 8 bytes from `offset`
    /// source bytes (offset in 2..=7) without OOB reads or writes.
    #[kani::proof]
    #[kani::unwind(3)] // at most 2 doubling iterations
    fn build_pattern_u64_no_oob() {
        let src = [0u8; 7];
        let offset: usize = kani::any();
        kani::assume(offset >= 2 && offset <= 7);
        unsafe {
            build_pattern_u64(src.as_ptr(), offset);
        }
    }

    // -- Primitive: fast_extend_from_ptr --

    /// All copy tiers (unconditional 16-byte, 1, 2, 3, 4..7, 8..15, 16+)
    /// stay within source and destination bounds given the documented
    /// precondition (len <= avail readable bytes, len + 16 bytes of spare
    /// capacity).
    #[kani::proof]
    #[kani::unwind(6)] // 64/16 = 4 iterations max in the 16-byte loops
    fn fast_extend_from_ptr_no_oob() {
        let src = [0u8; 64];
        let len: usize = kani::any();
        let avail: usize = kani::any();
        kani::assume(len <= avail && avail <= 64);

        let mut vec = Vec::with_capacity(len + 16);
        unsafe {
            fast_extend_from_ptr(&mut vec, src.as_ptr(), len, avail);
        }
        assert_eq!(vec.len(), len);
    }

    // -- wild_copy_match_unchecked --
    //
    // Split by offset tier to control loop-unwind bounds. Each harness
    // establishes the documented precondition (offset > 0, offset <=
    // vec.len(), capacity >= vec.len() + len + 16) and proves the
    // corresponding code path stays in-bounds.
    //
    // Pre-fill uses extend_from_slice (memcpy, no loop) to avoid
    // triggering unwind limits in Vec::resize's internal loop.

    /// offset >= 16: copy_16 loop
    #[kani::proof]
    #[kani::unwind(4)] // 48/16 = 3 iterations max
    fn wild_copy_match_offset_ge16_no_oob() {
        let offset: usize = kani::any();
        let len: usize = kani::any();
        kani::assume(offset >= 16 && offset <= 48);
        kani::assume(len >= 1 && len <= 48);

        let mut vec = Vec::with_capacity(48 + len + 16);
        vec.extend_from_slice(&[0u8; 48]);
        unsafe {
            wild_copy_match_unchecked(&mut vec, offset, len);
        }
        assert_eq!(vec.len(), 48 + len);
    }

    /// offset == 1: RLE via write_bytes (no loop in this path, but
    /// Kani still needs an unwind bound for loops in sibling branches)
    #[kani::proof]
    #[kani::unwind(2)]
    fn wild_copy_match_offset_1_no_oob() {
        let len: usize = kani::any();
        kani::assume(len >= 1 && len <= 64);

        let mut vec = Vec::with_capacity(4 + len + 16);
        vec.extend_from_slice(&[0xAAu8; 4]);
        unsafe {
            wild_copy_match_unchecked(&mut vec, 1, len);
        }
        assert_eq!(vec.len(), 4 + len);
    }

    /// offset 8..15: copy_8 pairs loop
    #[kani::proof]
    #[kani::unwind(4)] // (48-16)/16 = 2 iterations max in inner loop
    fn wild_copy_match_offset_8to15_no_oob() {
        let offset: usize = kani::any();
        let len: usize = kani::any();
        kani::assume(offset >= 8 && offset <= 15);
        kani::assume(len >= 1 && len <= 48);

        let mut vec = Vec::with_capacity(16 + len + 16);
        vec.extend_from_slice(&[0u8; 16]);
        unsafe {
            wild_copy_match_unchecked(&mut vec, offset, len);
        }
        assert_eq!(vec.len(), 16 + len);
    }

    /// offset 2..7: build_pattern_u64 + u64 stamp loop
    #[kani::proof]
    #[kani::unwind(13)] // 24/2 = 12 iterations max (offset=2, len=24)
    fn wild_copy_match_offset_2to7_no_oob() {
        let offset: usize = kani::any();
        let len: usize = kani::any();
        kani::assume(offset >= 2 && offset <= 7);
        kani::assume(len >= 1 && len <= 24);

        let mut vec = Vec::with_capacity(8 + len + 16);
        vec.extend_from_slice(&[0u8; 8]);
        unsafe {
            wild_copy_match_unchecked(&mut vec, offset, len);
        }
        assert_eq!(vec.len(), 8 + len);
    }

    // -- spread_match --

    /// Offsets 1..15: the first eight bytes spread the period, then 8-byte
    /// chunks. Reads stay in `src..dst` and the bytes already written;
    /// writes stay in `dst..dst + len + 8`.
    #[kani::proof]
    #[kani::unwind(5)] // 24/8 = 3 iterations max
    fn spread_match_no_oob() {
        let offset: usize = kani::any();
        let len: usize = kani::any();
        kani::assume(offset >= 1 && offset < 16);
        kani::assume(len >= 1 && len <= 24);

        let mut buf = [0u8; 16 + 24 + 8];
        unsafe {
            let dst = buf.as_mut_ptr().add(16);
            spread_match(dst.sub(offset), dst, offset, len);
        }
    }

    // -- wild_copy_match_single_unchecked --
    //
    // Split into three harnesses matching the dispatch:
    //   len <= 16 && offset >= 16 → single copy_16
    //   offset >= 32             → copy_32 or copy_64 loop
    //   else                     → fallthrough to wild_copy_match_unchecked

    /// Short match with wide offset: single copy_16 (no loop in this
    /// path, but Kani needs an unwind bound for loops in sibling
    /// branches).
    #[kani::proof]
    #[kani::unwind(2)]
    fn wild_copy_match_single_short_no_oob() {
        let offset: usize = kani::any();
        let len: usize = kani::any();
        kani::assume(offset >= 16 && offset <= 48);
        kani::assume(len >= 1 && len <= 16);

        let mut vec = Vec::with_capacity(48 + len + WILDCOPY_OVERLENGTH);
        vec.extend_from_slice(&[0u8; 48]);
        unsafe {
            wild_copy_match_single_unchecked(&mut vec, offset, len);
        }
        assert_eq!(vec.len(), 48 + len);
    }

    /// Wide offset: copy_32 or copy_64 loops.
    #[kani::proof]
    #[kani::unwind(3)] // 64/32 = 2 iterations max
    fn wild_copy_match_single_wide_no_oob() {
        let offset: usize = kani::any();
        let len: usize = kani::any();
        kani::assume(offset >= 32 && offset <= 64);
        kani::assume(len >= 1 && len <= 64);

        let mut vec = Vec::with_capacity(64 + len + WILDCOPY_OVERLENGTH);
        vec.extend_from_slice(&[0u8; 64]);
        unsafe {
            wild_copy_match_single_unchecked(&mut vec, offset, len);
        }
        assert_eq!(vec.len(), 64 + len);
    }

    /// Fallthrough to wild_copy_match_unchecked (offset < 32,
    /// excluding the short + offset >= 16 fast path).
    #[kani::proof]
    #[kani::unwind(13)] // pattern stamp loop: 24/2 = 12 max
    fn wild_copy_match_single_fallthrough_no_oob() {
        let offset: usize = kani::any();
        let len: usize = kani::any();
        kani::assume(offset >= 1 && offset <= 31);
        kani::assume(len >= 1 && len <= 24);
        kani::assume(!(len <= 16 && offset >= 16));

        let mut vec = Vec::with_capacity(32 + len + WILDCOPY_OVERLENGTH);
        vec.extend_from_slice(&[0u8; 32]);
        unsafe {
            wild_copy_match_single_unchecked(&mut vec, offset, len);
        }
        assert_eq!(vec.len(), 32 + len);
    }
}
