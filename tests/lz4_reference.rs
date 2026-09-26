//! The LZ4 blocks psxmon sends decode with an independent decoder
//! (lz4_flex), and no sequence copies more than the match cap.

use psxmon::lz4;

fn corpus() -> Vec<Vec<u8>> {
    let mut x: u32 = 99;
    let mut rnd = move || {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
        (x >> 16) as u8
    };
    let noise: Vec<u8> = (0..70_000).map(|_| rnd()).collect();
    let zeros = vec![0u8; 300_000];
    let mut mixed = Vec::new();
    for k in 0..200 {
        mixed.extend(std::iter::repeat_n(k as u8, 700));
        mixed.extend(&noise[k * 100..k * 100 + 300]);
        mixed.extend(b"\x27\xbd\xff\xe8\xaf\xbf\x00\x14\x0c\x00\x40\x00");
    }
    // Repeats more than 64 KiB apart must not be matched.
    let mut far = noise[..40_000].to_vec();
    far.extend(vec![0x11; 40_000]);
    far.extend(&noise[..40_000]);
    let text: Vec<u8> = (0..20000).map(|i| b"abcdefghijklmnopqrstuvwxyz\n"[i % 27]).collect();
    vec![Vec::new(), vec![1], noise, zeros, mixed, far, text]
}

#[test]
fn round_trip_through_reference_decoder_with_cap() {
    for (i, src) in corpus().iter().enumerate() {
        for cap in [lz4::DEFAULT_MAX_MATCH, 255, 16] {
            let block = lz4::compress(src, cap);
            let out =
                lz4_flex::block::decompress(&block, src.len()).unwrap_or_else(|e| panic!("sample {i} cap {cap}: {e}"));
            assert_eq!(&out, src, "sample {i} cap {cap}");
            let seqs = lz4::sequences(&block).unwrap();
            let max = seqs.iter().map(|s| s.match_len).max().unwrap_or(0);
            assert!(max <= cap, "sample {i}: match {max} > cap {cap}");
            assert!(seqs.iter().all(|s| s.offset <= 65535));
        }
    }
}

#[test]
fn cap_matches_is_a_pure_re_encoding() {
    // Re-capping a block made by another encoder (lz4_flex) keeps its content.
    for src in corpus() {
        let foreign = lz4_flex::block::compress(&src);
        let capped = lz4::cap_matches(&foreign, 128);
        assert_eq!(lz4_flex::block::decompress(&capped, src.len()).unwrap(), src);
        assert!(lz4::sequences(&capped).unwrap().iter().all(|s| s.match_len <= 128));
    }
}

#[test]
fn compresses_zeros_well() {
    let block = lz4::compress(&vec![0u8; 300_000], 128);
    // Each capped sequence costs 4 bytes (token, offset, one length byte)
    // for 128 bytes.
    assert!(block.len() < 300_000 / 128 * 4 + 64, "{}", block.len());
}
