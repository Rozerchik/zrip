#[cfg(feature = "alloc")]
use alloc::vec::Vec;

#[inline(always)]
pub(crate) fn read_u64_le_unaligned(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(*data[offset..].first_chunk::<8>().unwrap())
}

#[cfg(feature = "alloc")]
#[inline(always)]
pub(crate) fn write_u64_le_unaligned(buf: &mut Vec<u8>, pos: usize, val: u64) {
    let needed = pos + 8;
    if buf.len() < needed {
        buf.resize(needed, 0);
    }
    buf[pos..pos + 8].copy_from_slice(&val.to_le_bytes());
}

#[cfg(feature = "alloc")]
#[inline(always)]
pub(crate) fn set_vec_len(vec: &mut Vec<u8>, len: usize) {
    vec.resize(len, 0);
}
