//! The LZ4 blocks psxmon sends decode with an independent decoder
//! (lz4_flex), and no sequence copies more than the match cap.
#![cfg(test)]

use psxmon::lz4;

fn corpus() -> Vec<Vec<u8>> {
    let mut x: u32 = 99;
    let mut rnd = move || {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let [_, _, b, _] = x.to_le_bytes();
        b
    };
    let noise: Vec<u8> = (0..70_000).map(|_| rnd()).collect();
    let zeros = vec![0u8; 300_000];
    let mut mixed = Vec::new();
    for (k, piece) in noise.chunks(100).take(200).enumerate() {
        let fill = u8::try_from(k).expect("k < 200");
        mixed.extend(std::iter::repeat_n(fill, 700));
        mixed.extend(piece);
        mixed.extend(b"\x27\xbd\xff\xe8\xaf\xbf\x00\x14\x0c\x00\x40\x00");
    }
    // Repeats more than 64 KiB apart must not be matched.
    let head = noise.get(..40_000).expect("70000 bytes of noise");
    let mut far = head.to_vec();
    far.extend(vec![0x11; 40_000]);
    far.extend(head);
    let text: Vec<u8> = b"abcdefghijklmnopqrstuvwxyz\n"
        .iter()
        .copied()
        .cycle()
        .take(20000)
        .collect();
    vec![Vec::new(), vec![1], noise, zeros, mixed, far, text]
}

#[test]
fn round_trip_through_reference_decoder_with_cap() {
    for (i, src) in corpus().iter().enumerate() {
        for cap in [lz4::DEFAULT_MAX_MATCH, 255, 16] {
            let block = lz4::compress(src, cap).expect("compress");
            let out = lz4_flex::block::decompress(&block, src.len())
                .unwrap_or_else(|e| panic!("sample {i} cap {cap}: {e}"));
            assert_eq!(&out, src, "sample {i} cap {cap}");
            let seqs = lz4::sequences(&block).expect("walk block");
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
        let capped = lz4::cap_matches(&foreign, 128).expect("well-formed block");
        let out = lz4_flex::block::decompress(&capped, src.len()).expect("reference decode");
        assert_eq!(out, src);
        let seqs = lz4::sequences(&capped).expect("walk block");
        assert!(seqs.iter().all(|s| s.match_len <= 128));
    }
}

#[test]
fn compresses_zeros_well() {
    let block = lz4::compress(&vec![0u8; 300_000], 128).expect("compress");
    // Each capped sequence costs 4 bytes (token, offset, one length byte)
    // for 128 bytes.
    assert!(block.len() < 300_000 / 128 * 4 + 64, "{}", block.len());
}
