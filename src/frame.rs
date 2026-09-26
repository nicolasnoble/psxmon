//! Frames on a byte stream link (SIO1): console text and frames share one
//! stream. A 0x00 byte enters a frame: SYNC, TYPE, LEN, LEN payload words,
//! then a Fletcher-32 checksum as two words, every word low byte first.
//! Every other byte is console text.

use crate::proto::{STREAM_MAX_LEN, SYNC};

/// Bytes before the payload: frame start, SYNC, TYPE, LEN.
const HEADER_BYTES: usize = 7;
/// Bytes after the payload: CKSUM.
const TRAILER_BYTES: usize = 4;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("frame payload of {0} words exceeds the 65535-word LEN field")]
    TooLong(usize),
}

/// The two Fletcher sums over a word stream, with 32-bit wrapping
/// accumulators and no reduction, as the C sides keep them.
pub fn fletcher_sums(words: impl IntoIterator<Item = u16>) -> (u32, u32) {
    let mut s1: u32 = 0;
    let mut s2: u32 = 0;
    for w in words {
        // Wrapping is the specification: both C sides use uint32_t sums.
        s1 = s1.wrapping_add(u32::from(w));
        s2 = s2.wrapping_add(s1);
    }
    (s1, s2)
}

/// Fletcher-32 without the frame checksum's 0 substitution: the BIOS
/// checksum the monitor reports in HELLO and PONG.
pub fn fletcher_raw(words: impl IntoIterator<Item = u16>) -> u32 {
    let (s1, s2) = fletcher_sums(words);
    ((s2 % 65535) << 16) | (s1 % 65535)
}

/// The frame checksum over TYPE, LEN and payload. 0 on the wire means "not
/// computed", so a computed 0 is sent as 0xFFFFFFFF.
pub fn fletcher(words: impl IntoIterator<Item = u16>) -> u32 {
    match fletcher_raw(words) {
        0 => 0xffff_ffff,
        ck => ck,
    }
}

/// A byte buffer as 16-bit little-endian words, an odd last byte padded
/// with 0.
fn le_words(bytes: &[u8]) -> impl Iterator<Item = u16> + '_ {
    bytes.chunks(2).map(|c| match *c {
        [lo, hi] => u16::from_le_bytes([lo, hi]),
        [lo] => u16::from(lo),
        _ => 0,
    })
}

/// Fletcher-32 of a byte buffer read as 16-bit little-endian words, the way
/// the monitor sums the BIOS region.
pub fn bios_fletcher(bytes: &[u8]) -> u32 {
    fletcher_raw(le_words(bytes))
}

/// The low and high halves of a u32.
pub fn split_u32(v: u32) -> [u16; 2] {
    let [a, b, c, d] = v.to_le_bytes();
    [u16::from_le_bytes([a, b]), u16::from_le_bytes([c, d])]
}

/// One frame on the wire: the 0x00 frame start, SYNC, TYPE, LEN, payload,
/// CKSUM.
pub fn encode_frame(ty: u16, payload: &[u16]) -> Result<Vec<u8>, FrameError> {
    let len = u16::try_from(payload.len()).map_err(|_| FrameError::TooLong(payload.len()))?;
    let ck = fletcher([ty, len].into_iter().chain(payload.iter().copied()));
    let mut out = Vec::with_capacity(
        payload
            .len()
            .saturating_mul(2)
            .saturating_add(HEADER_BYTES + TRAILER_BYTES),
    );
    out.push(0);
    for w in [SYNC, ty, len]
        .into_iter()
        .chain(payload.iter().copied())
        .chain(split_u32(ck))
    {
        out.extend_from_slice(&w.to_le_bytes());
    }
    Ok(out)
}

/// u32 values to payload words, low half first.
pub fn u32_words(values: &[u32]) -> Vec<u16> {
    values.iter().flat_map(|&v| split_u32(v)).collect()
}

/// Bytes to payload words, low byte first, an odd trailing byte padded with 0.
pub fn bytes_to_words(bytes: &[u8]) -> Vec<u16> {
    le_words(bytes).collect()
}

/// `nbytes` bytes unpacked from `words[start..]`, zero past the end.
pub fn words_to_bytes(words: &[u16], start: usize, nbytes: usize) -> Vec<u8> {
    words
        .iter()
        .skip(start)
        .flat_map(|w| w.to_le_bytes())
        .chain(std::iter::repeat(0))
        .take(nbytes)
        .collect()
}

/// The u32 at word `index` (low half first), zero past the end.
pub fn word_u32(words: &[u16], index: usize) -> u32 {
    let mut it = words.iter().skip(index).copied();
    let lo = it.next().unwrap_or(0);
    let hi = it.next().unwrap_or(0);
    u32::from(lo) | (u32::from(hi) << 16)
}

/// A frame as received. `ok` is false when the checksum did not match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub ty: u16,
    pub words: Vec<u16>,
    pub ok: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Console text: a run of non-zero bytes outside frames.
    Tty(Vec<u8>),
    Frame(Frame),
}

/// Incremental splitter for the byte stream. `feed` bytes as they arrive,
/// then drain events with `next_event`. A partial frame waits for more bytes.
#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
    pos: usize,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        if self.pos > 0 && self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        } else if self.pos > 65_536 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    fn advance(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n).min(self.buf.len());
    }

    pub fn next_event(&mut self) -> Option<Event> {
        loop {
            let buf = self.buf.get(self.pos..)?;
            let (&first, _) = buf.split_first()?;
            if first != 0 {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                let text = buf.get(..end)?.to_vec();
                self.advance(end);
                return Some(Event::Tty(text));
            }
            let &[_, s0, s1, t0, t1, l0, l1] = buf.first_chunk::<HEADER_BYTES>()?;
            let sync = u16::from_le_bytes([s0, s1]);
            let ty = u16::from_le_bytes([t0, t1]);
            let len = u16::from_le_bytes([l0, l1]);
            if sync != SYNC || len > STREAM_MAX_LEN {
                // A 0 that does not start a frame is noise, as on the monitor side.
                self.advance(1);
                continue;
            }
            // LEN <= STREAM_MAX_LEN, so none of these sizes can overflow.
            let payload_bytes = usize::from(len).checked_mul(2)?;
            let body = buf.get(HEADER_BYTES..)?;
            let (payload, rest) = body.split_at_checked(payload_bytes)?;
            let ck = u32::from_le_bytes(*rest.first_chunk::<TRAILER_BYTES>()?);
            let words: Vec<u16> = le_words(payload).collect();
            let ok = ck == fletcher([ty, len].into_iter().chain(words.iter().copied()));
            self.advance(payload_bytes.checked_add(HEADER_BYTES + TRAILER_BYTES)?);
            return Some(Event::Frame(Frame { ty, words, ok }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::*;

    fn enc(ty: u16, payload: &[u16]) -> Vec<u8> {
        encode_frame(ty, payload).expect("test frames fit in a LEN field")
    }

    fn parse_all(bytes: &[u8]) -> Vec<Event> {
        let mut p = Parser::new();
        p.feed(bytes);
        std::iter::from_fn(|| p.next_event()).collect()
    }

    #[test]
    fn ping_wire_bytes() {
        // PROTOCOL.md section 2.3 and the SET_BAUD window frames in transport.c.
        assert_eq!(
            enc(PING, &[]),
            [
                0x00, 0xaa, 0x55, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00
            ]
        );
        assert_eq!(
            enc(PING, &[1]),
            [
                0x00, 0xaa, 0x55, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x03, 0x00, 0x06, 0x00
            ]
        );
    }

    #[test]
    fn golden_fletcher_matches_ts() {
        // Pinned with fletcher() from runner-agent's monitor/protocol.ts under node.
        // (i * 0x9e37 + 0x1234) & 0xffff, as the node script computed it.
        let words: Vec<u16> = (0..5000u16)
            .map(|i| i.wrapping_mul(0x9e37).wrapping_add(0x1234))
            .collect();
        assert_eq!(fletcher(words.iter().copied()), 0x6d2fab28);
        assert_eq!(fletcher(std::iter::repeat_n(0xffffu16, 4112)), 0xff7e0000);
        assert_eq!(fletcher([]), 0xffffffff);
    }

    #[test]
    fn round_trip_with_tty() {
        let mut wire = b"hello ".to_vec();
        wire.extend(enc(STOPPED, &[1, 2, 3, 4, 5, 6, 7]));
        wire.extend(b"world");
        wire.extend(enc(ACK, &[]));
        let ev = parse_all(&wire);
        assert_eq!(
            ev,
            vec![
                Event::Tty(b"hello ".to_vec()),
                Event::Frame(Frame {
                    ty: STOPPED,
                    words: vec![1, 2, 3, 4, 5, 6, 7],
                    ok: true
                }),
                Event::Tty(b"world".to_vec()),
                Event::Frame(Frame {
                    ty: ACK,
                    words: vec![],
                    ok: true
                }),
            ]
        );
    }

    #[test]
    fn byte_at_a_time_and_zero_runs() {
        let mut wire = vec![0, 0, 0];
        wire.extend(enc(PONG, &[2, 1, 0x1234, 0x5678]));
        let mut p = Parser::new();
        let mut got = vec![];
        for b in wire {
            p.feed(&[b]);
            while let Some(e) = p.next_event() {
                got.push(e);
            }
        }
        assert_eq!(
            got,
            vec![Event::Frame(Frame {
                ty: PONG,
                words: vec![2, 1, 0x1234, 0x5678],
                ok: true
            })]
        );
    }

    #[test]
    fn corrupted_frame_is_flagged() {
        let mut wire = enc(DATA, &[4, 0, 0x4241, 0x4443]);
        wire[9] ^= 0x01; // a payload byte
        match &parse_all(&wire)[..] {
            [Event::Frame(f)] => assert!(!f.ok),
            other => panic!("unexpected {other:?}"),
        }
        // A zero checksum is never valid on a stream link.
        let mut wire = enc(ACK, &[]);
        if let Some(ck) = wire.last_chunk_mut::<4>() {
            *ck = [0; 4];
        }
        match &parse_all(&wire)[..] {
            [Event::Frame(f)] => assert!(!f.ok),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn noise_zero_and_oversized_len() {
        // 0 followed by non-SYNC: the 0 is dropped, the rest is text.
        assert_eq!(
            parse_all(&[0, b'a', b'b', b'c', b'd', b'e', b'f', b'g']),
            vec![Event::Tty(b"abcdefg".to_vec())]
        );
        // LEN above the stream limit is noise.
        let mut wire = vec![0, 0xaa, 0x55, 0x41, 0x00];
        wire.extend(STREAM_MAX_LEN.checked_add(1).expect("4113").to_le_bytes());
        wire.extend(b"xy");
        let ev = parse_all(&wire);
        assert!(ev.iter().all(|e| matches!(e, Event::Tty(_))));
    }

    #[test]
    fn big_frame_wraps_accumulators() {
        // A full 8 KiB DATA frame of 0xFF bytes: s2 passes 2^32 and wraps,
        // so a checksum computed with unbounded integers would differ.
        let mut payload = u32_words(&[8192]);
        payload.extend(bytes_to_words(&[0xff; 8192]));
        assert_eq!(payload.len(), 4098);
        let len = u16::try_from(payload.len()).expect("4098 words");
        let words: Vec<u16> = [DATA, len]
            .into_iter()
            .chain(payload.iter().copied())
            .collect();
        let (_, s2) = fletcher_sums(words.iter().copied());
        let wide: u64 = {
            let (mut a, mut b) = (0u64, 0u64);
            for &w in &words {
                a = a.checked_add(u64::from(w)).expect("no u64 overflow");
                b = b.checked_add(a).expect("no u64 overflow");
            }
            b
        };
        assert!(wide > u64::from(u32::MAX) && wide & 0xffff_ffff == u64::from(s2));
        let wire = enc(DATA, &payload);
        match &parse_all(&wire)[..] {
            [Event::Frame(f)] => {
                assert!(f.ok);
                assert_eq!(word_u32(&f.words, 0), 8192);
                assert_eq!(words_to_bytes(&f.words, 2, 8192), vec![0xff; 8192]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn packing() {
        assert_eq!(bytes_to_words(&[1, 2, 3]), vec![0x0201, 0x0003]);
        assert_eq!(words_to_bytes(&[0x0201, 0x0003], 0, 3), vec![1, 2, 3]);
        assert_eq!(u32_words(&[0x80010000]), vec![0x0000, 0x8001]);
        assert_eq!(bios_fletcher(&[1, 0, 2, 0]), (4 << 16) | 3);
    }
}
