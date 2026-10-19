#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 256 * 1024 {
        return;
    }
    for level in [-7, -6, -5, -4, -3, -2, -1, 1, 2, 3, 4] {
        if let Ok(compressed) = zrip::compress(data, level) {
            let decompressed = zstd::bulk::decompress(&compressed, data.len() + 1024)
                .expect("C zstd failed to decompress zrip output");
            assert_eq!(data, &decompressed[..]);
        }
    }
});
