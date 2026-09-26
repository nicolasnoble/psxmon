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
const HASH_BITS: u32 = 16;
const WINDOW: usize = 1 << 16;
const SEARCH_DEPTH: usize = 64;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("block truncated")]
    Truncated,
    #[error("match offset {0} out of range")]
    BadOffset(usize),
    #[error("output exceeds {0} bytes")]
    TooLong(usize),
}

/// A read position in a block.
struct Cursor<'a> {
    buf: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn byte(&mut self) -> Option<u8> {
        let (&b, rest) = self.buf.split_first()?;
        self.buf = rest;
        Some(b)
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.buf.split_at_checked(n)?;
        self.buf = rest;
        Some(head)
    }

    fn u16le(&mut self) -> Option<u16> {
        let &[lo, hi] = self.take(2)?.first_chunk::<2>()?;
        Some(u16::from_le_bytes([lo, hi]))
    }

    /// A length field: the token nibble, then 255-continued extra bytes
    /// when the nibble is 15.
    fn length(&mut self, nibble: u8) -> Result<usize, DecodeError> {
        let mut n = usize::from(nibble);
        if nibble == 15 {
            loop {
                let b = self.byte().ok_or(DecodeError::Truncated)?;
                n = n
                    .checked_add(usize::from(b))
                    .ok_or(DecodeError::TooLong(usize::MAX))?;
                if b != 255 {
                    break;
                }
            }
        }
        Ok(n)
    }
}

/// One sequence as read from a block.
struct RawSequence<'a> {
    literals: &'a [u8],
    /// (offset, match length); None for the last sequence.
    matched: Option<(u16, usize)>,
}

fn next_sequence<'a>(c: &mut Cursor<'a>) -> Result<Option<RawSequence<'a>>, DecodeError> {
    let Some(token) = c.byte() else {
        return Ok(None);
    };
    let lit_len = c.length(token >> 4)?;
    let literals = c.take(lit_len).ok_or(DecodeError::Truncated)?;
    if c.buf.is_empty() {
        return Ok(Some(RawSequence {
            literals,
            matched: None,
        }));
    }
    let offset = c.u16le().ok_or(DecodeError::Truncated)?;
    let match_len = c
        .length(token & 15)?
        .checked_add(MIN_MATCH)
        .ok_or(DecodeError::TooLong(usize::MAX))?;
    Ok(Some(RawSequence {
        literals,
        matched: Some((offset, match_len)),
    }))
}

/// The low byte of a value known to be under 256.
fn low_byte(n: usize) -> u8 {
    let [lo, ..] = n.to_le_bytes();
    lo
}

fn put_len(out: &mut Vec<u8>, n: usize) {
    out.extend(std::iter::repeat_n(255u8, n / 255));
    out.push(low_byte(n % 255));
}

/// One sequence: literals, then (unless it is the last) a match of `mlen`
/// bytes at `off` back. `mlen` is at least [`MIN_MATCH`].
fn emit(out: &mut Vec<u8>, lits: &[u8], matched: Option<(u16, usize)>) {
    let m = matched.map_or(0, |(_, mlen)| mlen.saturating_sub(MIN_MATCH));
    out.push((low_byte(lits.len().min(15)) << 4) | low_byte(m.min(15)));
    if lits.len() >= 15 {
        put_len(out, lits.len().saturating_sub(15));
    }
    out.extend_from_slice(lits);
    if let Some((off, _)) = matched {
        out.extend_from_slice(&off.to_le_bytes());
        if m >= 15 {
            put_len(out, m.saturating_sub(15));
        }
    }
}

fn hash4(src: &[u8], i: usize) -> Option<usize> {
    let v = u32::from_le_bytes(*src.get(i..)?.first_chunk::<4>()?);
    // Multiplicative hashing: the wrap is the point.
    usize::try_from(v.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)).ok()
}

struct MatchFinder {
    head: Vec<usize>,
    chain: Vec<usize>,
}

impl MatchFinder {
    fn new() -> Self {
        MatchFinder {
            head: vec![usize::MAX; 1 << HASH_BITS],
            chain: vec![usize::MAX; WINDOW],
        }
    }

    fn insert(&mut self, src: &[u8], i: usize) {
        if let Some(h) = hash4(src, i)
            && let Some(slot) = self.head.get_mut(h)
            && let Some(link) = self.chain.get_mut(i % WINDOW)
        {
            *link = *slot;
            *slot = i;
        }
    }

    /// The longest match for `src[i..]` ending by `limit`: (length, offset).
    fn find(&self, src: &[u8], i: usize, limit: usize) -> (usize, u16) {
        let mut best = (0, 0);
        let (Some(h), Some(cur)) = (hash4(src, i), src.get(i..limit)) else {
            return best;
        };
        let mut cand = self.head.get(h).copied().unwrap_or(usize::MAX);
        for _ in 0..SEARCH_DEPTH {
            // usize::MAX (empty) and anything over 64 KiB back end the chain.
            let Some(dist) = i.checked_sub(cand) else {
                break;
            };
            let (Ok(off), Some(prev)) = (u16::try_from(dist), src.get(cand..)) else {
                break;
            };
            // Overlapping matches compare exactly as the decoder copies them.
            let len = cur.iter().zip(prev).take_while(|(a, b)| a == b).count();
            if len > best.0 {
                best = (len, off);
            }
            cand = self.chain.get(cand % WINDOW).copied().unwrap_or(usize::MAX);
        }
        best
    }
}

/// An LZ4 block encoding `src` (hash-chain match finder, greedy parse). No
/// match cap: see [`compress`].
pub fn compress_block(src: &[u8]) -> Vec<u8> {
    let n = src.len();
    let mut out = Vec::with_capacity(n / 2);
    let mut anchor = 0;
    if n > MF_LIMIT {
        let match_limit = n.saturating_sub(LAST_LITERALS);
        let last_start = n.saturating_sub(MF_LIMIT);
        let mut finder = MatchFinder::new();
        let mut i = 0;
        while i < last_start {
            let (len, off) = finder.find(src, i, match_limit);
            finder.insert(src, i);
            if len >= MIN_MATCH {
                emit(
                    &mut out,
                    src.get(anchor..i).unwrap_or_default(),
                    Some((off, len)),
                );
                let end = i.saturating_add(len);
                for j in i.saturating_add(1)..end {
                    finder.insert(src, j);
                }
                i = end;
                anchor = end;
            } else {
                i = i.saturating_add(1);
            }
        }
    }
    emit(&mut out, src.get(anchor..).unwrap_or_default(), None);
    out
}

/// Re-encode an LZ4 block so no match copies more than `max_match` bytes: a
/// longer match becomes several sequences with the same offset and no
/// literals. A port of `lz4CapMatches` in the reference host
/// (runner-agent `monitor/lz4.ts`). `max_match` should be at least 7 so a
/// split never leaves a piece under the 4-byte minimum.
pub fn cap_matches(block: &[u8], max_match: usize) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::with_capacity(block.len().saturating_add(block.len() / 32));
    let mut c = Cursor { buf: block };
    while let Some(seq) = next_sequence(&mut c)? {
        let Some((off, mut mlen)) = seq.matched else {
            emit(&mut out, seq.literals, None); // last sequence: literals only
            break;
        };
        let mut lits = seq.literals;
        while mlen > 0 {
            // Never leave a remainder under the 4-byte minimum match.
            let mut n = mlen.min(max_match);
            let rest = mlen.saturating_sub(n);
            if rest > 0 && rest < MIN_MATCH {
                n = mlen.saturating_sub(MIN_MATCH);
            }
            if n < MIN_MATCH {
                n = mlen;
            }
            emit(&mut out, lits, Some((off, n)));
            lits = &[];
            mlen = mlen.saturating_sub(n);
        }
    }
    Ok(out)
}

/// The block the monitor gets for `src`: compressed, then match-capped.
pub fn compress(src: &[u8], max_match: usize) -> Result<Vec<u8>, DecodeError> {
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

/// Walk a block's sequences.
pub fn sequences(block: &[u8]) -> Result<Vec<Sequence>, DecodeError> {
    let mut seqs = Vec::new();
    let mut c = Cursor { buf: block };
    while let Some(seq) = next_sequence(&mut c)? {
        let (offset, match_len) = seq.matched.map_or((0, 0), |(o, m)| (usize::from(o), m));
        seqs.push(Sequence {
            literals: seq.literals.len(),
            offset,
            match_len,
        });
    }
    Ok(seqs)
}

/// Decode a block of at most `max_out` bytes.
pub fn decompress(block: &[u8], max_out: usize) -> Result<Vec<u8>, DecodeError> {
    let mut out: Vec<u8> = Vec::new();
    let mut c = Cursor { buf: block };
    while let Some(seq) = next_sequence(&mut c)? {
        if out.len().saturating_add(seq.literals.len()) > max_out {
            return Err(DecodeError::TooLong(max_out));
        }
        out.extend_from_slice(seq.literals);
        let Some((off, mlen)) = seq.matched else {
            break;
        };
        let off = usize::from(off);
        let start = out
            .len()
            .checked_sub(off)
            .filter(|_| off != 0)
            .ok_or(DecodeError::BadOffset(off))?;
        if out.len().saturating_add(mlen) > max_out {
            return Err(DecodeError::TooLong(max_out));
        }
        for k in start..start.saturating_add(mlen) {
            let b = out.get(k).copied().ok_or(DecodeError::BadOffset(off))?;
            out.push(b);
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
            let [_, _, _, noise] = x.to_le_bytes();
            let [_, _, _, phase] = (k % 4).to_be_bytes();
            let op = [0x27u8, 0xbd, 0xff, 0xe0];
            match k % 1000 {
                0..=299 => v.push(0),
                300..=599 => v.extend(op.get(usize::from(phase))),
                _ => v.push(noise),
            }
        }
        v
    }

    #[test]
    fn round_trip_small_and_edge_sizes() {
        for n in [0u8, 1, 5, 12, 13, 17, 100] {
            let src: Vec<u8> = (0..n).map(|i| i % 7).collect();
            let c = compress(&src, DEFAULT_MAX_MATCH).expect("compress");
            assert_eq!(decompress(&c, src.len()).expect("decode"), src, "n={n}");
        }
    }

    #[test]
    fn cap_splits_long_matches() {
        let src = sample();
        let raw = compress_block(&src);
        let seqs = sequences(&raw).expect("walk raw block");
        assert!(seqs.iter().any(|s| s.match_len > 128));
        for cap in [7usize, 16, 128, 255] {
            let c = cap_matches(&raw, cap).expect("cap");
            let seqs = sequences(&c).expect("walk capped block");
            assert!(seqs.iter().all(|s| s.match_len <= cap), "cap {cap}");
            assert_eq!(decompress(&c, src.len()).expect("decode"), src);
        }
    }

    #[test]
    fn cap_remainder_never_under_min_match() {
        // A 130-byte match at cap 128 splits 126 + 4, not 128 + 2.
        let mut src = vec![b'x'; 132];
        src.extend_from_slice(b"ABCDEFGHIJKLMNOP");
        let c = compress(&src, 128).expect("compress");
        let seqs = sequences(&c).expect("walk");
        assert!(
            seqs.iter()
                .all(|s| s.match_len == 0 || (4..=128).contains(&s.match_len))
        );
        assert_eq!(decompress(&c, src.len()).expect("decode"), src);
    }

    #[test]
    fn malformed_blocks_are_errors() {
        assert_eq!(decompress(&[0xf0], 100), Err(DecodeError::Truncated));
        assert_eq!(
            decompress(&[0x10, b'a', 0x05, 0x00], 100),
            Err(DecodeError::BadOffset(5))
        );
        assert_eq!(
            decompress(&[0x10, b'a', 0x00, 0x00], 100),
            Err(DecodeError::BadOffset(0))
        );
        assert_eq!(
            decompress(&[0x20, b'a', b'b'], 1),
            Err(DecodeError::TooLong(1))
        );
        assert_eq!(
            cap_matches(&[0x10, b'a', 0x01], 128),
            Err(DecodeError::Truncated)
        );
    }
}
