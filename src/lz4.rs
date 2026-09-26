//! LZ4 block compression for LOAD|LZ4 and WRITE_MEM|LZ4 frames: raw block
//! format, no frame header.
//!
//! The monitor decodes a block while it receives it and cannot pause the host
//! inside a frame (SIO1 RTS stays up for the whole frame, the RX FIFO is 8
//! bytes). A long match copies many bytes for the few wire bytes that encode
//! it, so the host caps the match length of every sequence
//! ([`cap_matches`]); 128 is what the reference host uses.

/// Default longest match copy per sequence. Uncapped matches overran the SIO1
/// FIFO at 230400 on an SCPH-7001; 255 and 128 both loaded cleanly there.
pub const DEFAULT_MAX_MATCH: usize = 128;

const MIN_MATCH: usize = 4;
/// The last 5 bytes of a block are always literals.
const LAST_LITERALS: usize = 5;
/// The last match starts at least 12 bytes before the end of the block.
const MF_LIMIT: usize = 12;
const MAX_DISTANCE: usize = 65535;
const HASH_BITS: u32 = 16;
const WINDOW: usize = 1 << 16;
const SEARCH_DEPTH: usize = 64;

fn hash4(src: &[u8], i: usize) -> usize {
    let v = u32::from_le_bytes([src[i], src[i + 1], src[i + 2], src[i + 3]]);
    (v.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
}

fn put_len(out: &mut Vec<u8>, mut n: usize) {
    while n >= 255 {
        out.push(255);
        n -= 255;
    }
    out.push(n as u8);
}

/// One sequence: literals, then (unless it is the last) a match of `mlen`
/// bytes at `off` back.
fn emit(out: &mut Vec<u8>, lits: &[u8], matched: Option<(usize, usize)>) {
    let m = matched.map_or(0, |(_, mlen)| mlen - MIN_MATCH);
    out.push(((lits.len().min(15) as u8) << 4) | m.min(15) as u8);
    if lits.len() >= 15 {
        put_len(out, lits.len() - 15);
    }
    out.extend_from_slice(lits);
    if let Some((off, _)) = matched {
        out.extend_from_slice(&(off as u16).to_le_bytes());
        if m >= 15 {
            put_len(out, m - 15);
        }
    }
}

/// An LZ4 block encoding `src` (hash-chain match finder, greedy parse). No
/// match cap: see [`compress`].
pub fn compress_block(src: &[u8]) -> Vec<u8> {
    let n = src.len();
    let mut out = Vec::with_capacity(n / 2 + 16);
    let mut anchor = 0;
    if n > MF_LIMIT {
        let match_limit = n - LAST_LITERALS;
        let mut head = vec![usize::MAX; 1 << HASH_BITS];
        let mut chain = vec![usize::MAX; WINDOW];
        let insert = |head: &mut [usize], chain: &mut [usize], i: usize| {
            let h = hash4(src, i);
            chain[i % WINDOW] = head[h];
            head[h] = i;
        };
        let mut i = 0;
        while i + MF_LIMIT < n {
            let mut best = (0usize, 0usize); // (len, offset)
            let mut cand = head[hash4(src, i)];
            let mut depth = SEARCH_DEPTH;
            while cand != usize::MAX && i - cand <= MAX_DISTANCE && depth > 0 {
                if src[cand..cand + MIN_MATCH] == src[i..i + MIN_MATCH] {
                    let mut len = MIN_MATCH;
                    while i + len < match_limit && src[cand + len] == src[i + len] {
                        len += 1;
                    }
                    if len > best.0 {
                        best = (len, i - cand);
                    }
                }
                cand = chain[cand % WINDOW];
                depth -= 1;
            }
            insert(&mut head, &mut chain, i);
            if best.0 >= MIN_MATCH {
                emit(&mut out, &src[anchor..i], Some((best.1, best.0)));
                for j in i + 1..i + best.0 {
                    if j + MIN_MATCH <= n {
                        insert(&mut head, &mut chain, j);
                    }
                }
                i += best.0;
                anchor = i;
            } else {
                i += 1;
            }
        }
    }
    emit(&mut out, &src[anchor..], None);
    out
}

/// Re-encode an LZ4 block so no match copies more than `max_match` bytes: a
/// longer match becomes several sequences with the same offset and no
/// literals. A port of `lz4CapMatches` in the reference host
/// (runner-agent `monitor/lz4.ts`). `max_match` should be at least 7 so a
/// split never leaves a piece under the 4-byte minimum.
pub fn cap_matches(block: &[u8], max_match: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(block.len() + block.len() / 32 + 16);
    let mut i = 0;
    while i < block.len() {
        let token = block[i];
        i += 1;
        let mut lit_len = (token >> 4) as usize;
        if lit_len == 15 {
            loop {
                let b = block[i];
                i += 1;
                lit_len += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        let lits = &block[i..(i + lit_len).min(block.len())];
        i += lit_len;
        if i >= block.len() {
            emit(&mut out, lits, None); // last sequence: literals only
            break;
        }
        let off = block[i] as usize | (block[i + 1] as usize) << 8;
        i += 2;
        let mut mlen = (token & 15) as usize + MIN_MATCH;
        if token & 15 == 15 {
            loop {
                let b = block[i];
                i += 1;
                mlen += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        let mut first = true;
        while mlen > 0 {
            // Never leave a remainder under the 4-byte minimum match.
            let mut n = mlen.min(max_match);
            if mlen - n > 0 && mlen - n < MIN_MATCH {
                n = mlen - MIN_MATCH;
            }
            if n < MIN_MATCH {
                n = mlen;
            }
            emit(&mut out, if first { lits } else { &[] }, Some((off, n)));
            first = false;
            mlen -= n;
        }
    }
    out
}

/// The block the monitor gets for `src`: compressed, then match-capped.
pub fn compress(src: &[u8], max_match: usize) -> Vec<u8> {
    cap_matches(&compress_block(src), max_match)
}

/// One decoded sequence: literal count, match offset and length (0, 0 for
/// the last sequence).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sequence {
    pub literals: usize,
    pub offset: usize,
    pub match_len: usize,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("block truncated")]
    Truncated,
    #[error("match offset {0} out of range")]
    BadOffset(usize),
    #[error("output exceeds {0} bytes")]
    TooLong(usize),
}

/// Walk a block's sequences.
pub fn sequences(block: &[u8]) -> Result<Vec<Sequence>, DecodeError> {
    let mut seqs = Vec::new();
    decode_inner(block, usize::MAX, Some(&mut seqs), false)?;
    Ok(seqs)
}

/// Decode a block of at most `max_out` bytes.
pub fn decompress(block: &[u8], max_out: usize) -> Result<Vec<u8>, DecodeError> {
    decode_inner(block, max_out, None, true)
}

fn decode_inner(
    block: &[u8],
    max_out: usize,
    mut seqs: Option<&mut Vec<Sequence>>,
    produce: bool,
) -> Result<Vec<u8>, DecodeError> {
    let mut out: Vec<u8> = Vec::new();
    let mut produced = 0usize;
    let mut i = 0;
    let byte = |i: usize| block.get(i).copied().ok_or(DecodeError::Truncated);
    while i < block.len() {
        let token = byte(i)?;
        i += 1;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            loop {
                let b = byte(i)?;
                i += 1;
                lit += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        if i + lit > block.len() {
            return Err(DecodeError::Truncated);
        }
        if produced + lit > max_out {
            return Err(DecodeError::TooLong(max_out));
        }
        if produce {
            out.extend_from_slice(&block[i..i + lit]);
        }
        produced += lit;
        i += lit;
        if i == block.len() {
            if let Some(s) = seqs.as_deref_mut() {
                s.push(Sequence {
                    literals: lit,
                    offset: 0,
                    match_len: 0,
                });
            }
            break;
        }
        let off = byte(i)? as usize | (byte(i + 1)? as usize) << 8;
        i += 2;
        let mut mlen = (token & 15) as usize + MIN_MATCH;
        if token & 15 == 15 {
            loop {
                let b = byte(i)?;
                i += 1;
                mlen += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        if off == 0 || off > produced {
            return Err(DecodeError::BadOffset(off));
        }
        if produced + mlen > max_out {
            return Err(DecodeError::TooLong(max_out));
        }
        if produce {
            let start = out.len() - off;
            for k in 0..mlen {
                out.push(out[start + k]);
            }
        }
        produced += mlen;
        if let Some(s) = seqs.as_deref_mut() {
            s.push(Sequence {
                literals: lit,
                offset: off,
                match_len: mlen,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        // Code-like data: repeats, long zero runs, some noise.
        let mut v = Vec::new();
        let mut x: u32 = 1;
        for k in 0..40_000u32 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            match k % 1000 {
                0..=299 => v.push(0),
                300..=599 => v.extend_from_slice(&[0x27, 0xbd, 0xff, 0xe0][(k % 4) as usize..(k % 4) as usize + 1]),
                _ => v.push((x >> 24) as u8),
            }
        }
        v
    }

    #[test]
    fn round_trip_small_and_edge_sizes() {
        for n in [0usize, 1, 5, 12, 13, 17, 100] {
            let src: Vec<u8> = (0..n).map(|i| (i % 7) as u8).collect();
            let c = compress(&src, DEFAULT_MAX_MATCH);
            assert_eq!(decompress(&c, n).unwrap(), src, "n={n}");
        }
    }

    #[test]
    fn cap_splits_long_matches() {
        let src = sample();
        let raw = compress_block(&src);
        assert!(sequences(&raw).unwrap().iter().any(|s| s.match_len > 128));
        for cap in [7usize, 16, 128, 255] {
            let c = cap_matches(&raw, cap);
            let seqs = sequences(&c).unwrap();
            assert!(seqs.iter().all(|s| s.match_len <= cap), "cap {cap}");
            assert_eq!(decompress(&c, src.len()).unwrap(), src);
        }
    }

    #[test]
    fn cap_remainder_never_under_min_match() {
        // A 130-byte match at cap 128 splits 126 + 4, not 128 + 2.
        let mut src = vec![b'x'];
        src.extend(std::iter::repeat_n(b'x', 131));
        src.extend_from_slice(b"ABCDEFGHIJKLMNOP");
        let c = compress(&src, 128);
        let seqs = sequences(&c).unwrap();
        assert!(
            seqs.iter()
                .all(|s| s.match_len == 0 || (4..=128).contains(&s.match_len))
        );
        assert_eq!(decompress(&c, src.len()).unwrap(), src);
    }
}
