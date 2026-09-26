//! Frames on a byte stream link (SIO1): console text and frames share one
//! stream. A 0x00 byte enters a frame: SYNC, TYPE, LEN, LEN payload words,
//! then a Fletcher-32 checksum as two words, every word low byte first.
//! Every other byte is console text.

use crate::proto::{STREAM_MAX_LEN, SYNC};

/// The two Fletcher sums over a word stream, with 32-bit wrapping
/// accumulators and no reduction, as the C sides keep them.
pub fn fletcher_sums(words: impl IntoIterator<Item = u16>) -> (u32, u32) {
    let mut s1: u32 = 0;
    let mut s2: u32 = 0;
    for w in words {
        s1 = s1.wrapping_add(w as u32);
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

/// Fletcher-32 of a byte buffer read as 16-bit little-endian words, the way
/// the monitor sums the BIOS region.
pub fn bios_fletcher(bytes: &[u8]) -> u32 {
    fletcher_raw(
        bytes
            .chunks(2)
            .map(|c| c[0] as u16 | (*c.get(1).unwrap_or(&0) as u16) << 8),
    )
}

/// One frame on the wire: the 0x00 frame start, SYNC, TYPE, LEN, payload, CKSUM.
pub fn encode_frame(ty: u16, payload: &[u16]) -> Vec<u8> {
    assert!(payload.len() <= u16::MAX as usize, "frame payload too long");
    let len = payload.len() as u16;
    let ck = fletcher([ty, len].into_iter().chain(payload.iter().copied()));
    let mut out = Vec::with_capacity(1 + 2 * (payload.len() + 5));
    out.push(0);
    for w in [SYNC, ty, len].into_iter().chain(payload.iter().copied()) {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out.extend_from_slice(&(ck as u16).to_le_bytes());
    out.extend_from_slice(&((ck >> 16) as u16).to_le_bytes());
    out
}

/// u32 values to payload words, low half first.
pub fn u32_words(values: &[u32]) -> Vec<u16> {
    values.iter().flat_map(|&v| [v as u16, (v >> 16) as u16]).collect()
}

/// Bytes to payload words, low byte first, an odd trailing byte padded with 0.
pub fn bytes_to_words(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks(2)
        .map(|c| c[0] as u16 | (*c.get(1).unwrap_or(&0) as u16) << 8)
        .collect()
}

/// `nbytes` bytes unpacked from `words[start..]`, zero past the end.
pub fn words_to_bytes(words: &[u16], start: usize, nbytes: usize) -> Vec<u8> {
    (0..nbytes)
        .map(|i| {
            let w = words.get(start + (i >> 1)).copied().unwrap_or(0);
            if i & 1 == 1 { (w >> 8) as u8 } else { w as u8 }
        })
        .collect()
}

/// The u32 at word `index` (low half first), zero past the end.
pub fn word_u32(words: &[u16], index: usize) -> u32 {
    let lo = words.get(index).copied().unwrap_or(0) as u32;
    let hi = words.get(index + 1).copied().unwrap_or(0) as u32;
    lo | hi << 16
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
        } else if self.pos > 64 * 1024 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    pub fn next_event(&mut self) -> Option<Event> {
        loop {
            let buf = &self.buf[self.pos..];
            if buf.is_empty() {
                return None;
            }
            if buf[0] != 0 {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                self.pos += end;
                return Some(Event::Tty(buf[..end].to_vec()));
            }
            if buf.len() < 7 {
                return None;
            }
            let rd = |i: usize| u16::from_le_bytes([buf[i], buf[i + 1]]);
            let sync = rd(1);
            let len = rd(5);
            if sync != SYNC || len > STREAM_MAX_LEN {
                // A 0 that does not start a frame is noise, as on the monitor side.
                self.pos += 1;
                continue;
            }
            let total = 7 + 2 * len as usize + 4;
            if buf.len() < total {
                return None;
            }
            let ty = rd(3);
            let words: Vec<u16> = (0..len as usize).map(|i| rd(7 + 2 * i)).collect();
            let off = 7 + 2 * len as usize;
            let ck = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
            let ok = ck == fletcher([ty, len].into_iter().chain(words.iter().copied()));
            self.pos += total;
            return Some(Event::Frame(Frame { ty, words, ok }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::*;

    fn parse_all(bytes: &[u8]) -> Vec<Event> {
        let mut p = Parser::new();
        p.feed(bytes);
        std::iter::from_fn(|| p.next_event()).collect()
    }

    #[test]
    fn ping_wire_bytes() {
        // PROTOCOL.md section 2.3 and the SET_BAUD window frames in transport.c.
        assert_eq!(
            encode_frame(PING, &[]),
            [0x00, 0xaa, 0x55, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00]
        );
        assert_eq!(
            encode_frame(PING, &[1]),
            [
                0x00, 0xaa, 0x55, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x03, 0x00, 0x06, 0x00
            ]
        );
    }

    #[test]
    fn golden_fletcher_matches_ts() {
        // Pinned with fletcher() from runner-agent's monitor/protocol.ts under node.
        let words: Vec<u16> = (0..5000u32)
            .map(|i| (i.wrapping_mul(0x9e37).wrapping_add(0x1234)) as u16)
            .collect();
        assert_eq!(fletcher(words.iter().copied()), 0x6d2fab28);
        assert_eq!(fletcher(std::iter::repeat_n(0xffffu16, 4112)), 0xff7e0000);
        assert_eq!(fletcher([]), 0xffffffff);
    }

    #[test]
    fn round_trip_with_tty() {
        let mut wire = b"hello ".to_vec();
        wire.extend(encode_frame(STOPPED, &[1, 2, 3, 4, 5, 6, 7]));
        wire.extend(b"world");
        wire.extend(encode_frame(ACK, &[]));
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
        wire.extend(encode_frame(PONG, &[2, 1, 0x1234, 0x5678]));
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
        let mut wire = encode_frame(DATA, &[4, 0, 0x4241, 0x4443]);
        wire[9] ^= 0x01; // a payload byte
        match &parse_all(&wire)[..] {
            [Event::Frame(f)] => assert!(!f.ok),
            other => panic!("unexpected {other:?}"),
        }
        // A zero checksum is never valid on a stream link.
        let mut wire = encode_frame(ACK, &[]);
        let n = wire.len();
        wire[n - 4..].fill(0);
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
        wire.extend((STREAM_MAX_LEN + 1).to_le_bytes());
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
        let words: Vec<u16> = [DATA, payload.len() as u16]
            .into_iter()
            .chain(payload.iter().copied())
            .collect();
        let (_, s2) = fletcher_sums(words.iter().copied());
        let wide: u64 = {
            let (mut a, mut b) = (0u64, 0u64);
            for &w in &words {
                a += w as u64;
                b += a;
            }
            b
        };
        assert!(wide > u32::MAX as u64 && wide as u32 == s2);
        let wire = encode_frame(DATA, &payload);
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
