#[inline(always)]
pub(crate) fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(*data[offset..].first_chunk::<4>().unwrap())
}

#[inline(always)]
pub(crate) fn read_u64_le(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(*data[offset..].first_chunk::<8>().unwrap())
}

#[inline(always)]
pub(super) fn bulk_rounds(data: &[u8], v1: &mut u64, v2: &mut u64, v3: &mut u64, v4: &mut u64) {
    let len = data.len();
    debug_assert!(len >= 32);

    #[inline(always)]
    fn rd64(data: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(*data[offset..].first_chunk::<8>().unwrap())
    }

    let bulk_end = len & !31;
    let unroll_end = len & !127;
    let mut offset = 0;

    while offset < unroll_end {
        *v1 = super::xxh64_round(*v1, rd64(data, offset));
        *v2 = super::xxh64_round(*v2, rd64(data, offset + 8));
        *v3 = super::xxh64_round(*v3, rd64(data, offset + 16));
        *v4 = super::xxh64_round(*v4, rd64(data, offset + 24));

        *v1 = super::xxh64_round(*v1, rd64(data, offset + 32));
        *v2 = super::xxh64_round(*v2, rd64(data, offset + 40));
        *v3 = super::xxh64_round(*v3, rd64(data, offset + 48));
        *v4 = super::xxh64_round(*v4, rd64(data, offset + 56));

        *v1 = super::xxh64_round(*v1, rd64(data, offset + 64));
        *v2 = super::xxh64_round(*v2, rd64(data, offset + 72));
        *v3 = super::xxh64_round(*v3, rd64(data, offset + 80));
        *v4 = super::xxh64_round(*v4, rd64(data, offset + 88));

        *v1 = super::xxh64_round(*v1, rd64(data, offset + 96));
        *v2 = super::xxh64_round(*v2, rd64(data, offset + 104));
        *v3 = super::xxh64_round(*v3, rd64(data, offset + 112));
        *v4 = super::xxh64_round(*v4, rd64(data, offset + 120));

        offset += 128;
    }

    while offset < bulk_end {
        *v1 = super::xxh64_round(*v1, rd64(data, offset));
        *v2 = super::xxh64_round(*v2, rd64(data, offset + 8));
        *v3 = super::xxh64_round(*v3, rd64(data, offset + 16));
        *v4 = super::xxh64_round(*v4, rd64(data, offset + 24));
        offset += 32;
    }
}
