#![no_main]
//! Every dictionary decode path must return the input, for frames from zrip
//! and from C zstd. The dictionary comes from C zstd: zrip's trainer stores
//! the predefined sequence tables, which hides a decoder that keeps a
//! dictionary table after a block selects Predefined_Mode.
use libfuzzer_sys::fuzz_target;
use std::io::Read;
use std::sync::LazyLock;

const LIMIT: usize = 1 << 20;

static DICT_BYTES: LazyLock<Vec<u8>> = LazyLock::new(|| {
    let samples: Vec<Vec<u8>> = (0..5_000u32)
        .map(|i| {
            let id = i.wrapping_mul(0x9E37_79B1);
            let mut sample = vec![0x0a, 4];
            sample
                .extend_from_slice(["XNAS", "XNYS", "ARCX", "BATS"][id as usize >> 30].as_bytes());
            sample.push(0x19);
            sample.extend_from_slice(&f64::from(id >> 16).to_le_bytes());
            sample.push(0x28);
            sample.extend_from_slice(&id.to_le_bytes());
            sample.extend_from_slice(format!("user_{}@example.com", id >> 22).as_bytes());
            sample
        })
        .collect();
    zstd::dict::from_samples(&samples, 8192).expect("dictionary training failed")
});

static DICT: LazyLock<zrip::Dictionary> =
    LazyLock::new(|| zrip::Dictionary::from_bytes(&DICT_BYTES).expect("invalid dictionary"));

fn check_paths(frame: &[u8], payload: &[u8]) {
    let dict = &*DICT;
    let oneshot = zrip::decompress_with_dict_and_limit(frame, dict, LIMIT)
        .expect("one-shot dict decode failed");
    assert_eq!(payload, &oneshot[..], "one-shot");

    // Twice, so the second decode sees the context's cached tables.
    let mut ctx = zrip::DecompressContext::with_dict(dict.clone());
    for _ in 0..2 {
        let out = ctx
            .decompress_with_limit(frame, LIMIT)
            .expect("context dict decode failed");
        assert_eq!(payload, out.as_ref(), "context");
    }

    let mut out = Vec::new();
    zrip::FrameDecoder::with_dict(frame, dict.clone())
        .read_to_end(&mut out)
        .expect("streaming dict decode failed");
    assert_eq!(payload, &out[..], "streaming");

    let c = zstd::bulk::Decompressor::with_dictionary(&DICT_BYTES)
        .expect("C decompressor failed")
        .decompress(frame, LIMIT)
        .expect("C zstd dict decode failed");
    assert_eq!(payload, &c[..], "C zstd");
}

fuzz_target!(|payload: &[u8]| {
    if payload.len() > 64 * 1024 {
        return;
    }
    for level in [1, 3] {
        let frame = zrip::compress_with_dict(payload, level, &DICT).expect("zrip encode failed");
        check_paths(&frame, payload);

        let frame = zstd::bulk::Compressor::with_dictionary(level, &DICT_BYTES)
            .expect("C compressor failed")
            .compress(payload)
            .expect("C encode failed");
        check_paths(&frame, payload);
    }
});
