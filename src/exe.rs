//! Program images: what gets written where, and where execution starts.
//! Only PS-EXE is read for now; ELF and CPE fit the same [`Image`].

use std::path::Path;

/// Size of the PS-EXE header; the text follows it.
pub const PS_EXE_HEADER_SIZE: usize = 2048;
/// Stack top for a PS-EXE that sets none (`s_addr + s_size == 0`).
pub const DEFAULT_STACK: u32 = 0x801f_fff0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub addr: u32,
    pub data: Vec<u8>,
}

/// A loadable program: segments to write, then RUN with `pc`, `gp`, `sp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub segments: Vec<Segment>,
    pub pc: u32,
    pub gp: u32,
    pub sp: u32,
}

impl Image {
    pub fn total_bytes(&self) -> usize {
        self.segments.iter().map(|s| s.data.len()).sum()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExeError {
    #[error("file too small to be a PS-EXE: need at least a 0x800-byte header")]
    TooSmall,
    #[error("unrecognised program format (only PS-EXE is supported)")]
    Unknown,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// A PS-EXE, the way the reference host loads it: the file is padded with
/// zeros to a 2 KiB multiple, the text is `t_size` bytes after the header
/// (all of it when `t_size` is 0), loaded at `t_addr`; entry `pc0`, `gp0`,
/// and `sp = s_addr + s_size` or [`DEFAULT_STACK`]. The `PS-X EXE` magic is
/// not required.
pub fn parse_ps_exe(file: &[u8]) -> Result<Image, ExeError> {
    if file.len() < PS_EXE_HEADER_SIZE {
        return Err(ExeError::TooSmall);
    }
    let mut data = file.to_vec();
    let rem = data.len() % PS_EXE_HEADER_SIZE;
    if rem != 0 {
        data.resize(data.len() + PS_EXE_HEADER_SIZE - rem, 0);
    }
    let payload = data.len() - PS_EXE_HEADER_SIZE;
    let size = match rd32(&data, 0x1c) as usize {
        0 => payload,
        n => n.min(payload),
    };
    let sp = rd32(&data, 0x30).wrapping_add(rd32(&data, 0x34));
    Ok(Image {
        segments: vec![Segment {
            addr: rd32(&data, 0x18),
            data: data[PS_EXE_HEADER_SIZE..PS_EXE_HEADER_SIZE + size].to_vec(),
        }],
        pc: rd32(&data, 0x10),
        gp: rd32(&data, 0x14),
        sp: if sp == 0 { DEFAULT_STACK } else { sp },
    })
}

/// Parse a program file by content.
pub fn parse(file: &[u8]) -> Result<Image, ExeError> {
    if file.starts_with(b"\x7fELF") || file.starts_with(b"CPE") {
        return Err(ExeError::Unknown);
    }
    parse_ps_exe(file)
}

pub fn load(path: &Path) -> Result<Image, ExeError> {
    parse(&std::fs::read(path)?)
}

/// A minimal PS-EXE around `text`, for tests and tools.
pub fn build_ps_exe(text: &[u8], t_addr: u32, pc: u32, gp: u32, stack: u32) -> Vec<u8> {
    let mut h = vec![0u8; PS_EXE_HEADER_SIZE];
    h[..8].copy_from_slice(b"PS-X EXE");
    let mut put = |off: usize, v: u32| h[off..off + 4].copy_from_slice(&v.to_le_bytes());
    put(0x10, pc);
    put(0x14, gp);
    put(0x18, t_addr);
    put(
        0x1c,
        text.len().div_ceil(PS_EXE_HEADER_SIZE) as u32 * PS_EXE_HEADER_SIZE as u32,
    );
    put(0x30, stack);
    h.extend_from_slice(text);
    let rem = h.len() % PS_EXE_HEADER_SIZE;
    if rem != 0 {
        h.resize(h.len() + PS_EXE_HEADER_SIZE - rem, 0);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_exe_fields() {
        let file = build_ps_exe(&[1, 2, 3], 0x8001_0000, 0x8001_0000, 0x8002_0000, 0);
        let img = parse(&file).unwrap();
        assert_eq!(img.pc, 0x8001_0000);
        assert_eq!(img.gp, 0x8002_0000);
        assert_eq!(img.sp, DEFAULT_STACK);
        assert_eq!(img.segments[0].addr, 0x8001_0000);
        assert_eq!(img.segments[0].data.len(), 2048);
        assert_eq!(&img.segments[0].data[..4], &[1, 2, 3, 0]);
        assert!(matches!(parse(&[0; 16]), Err(ExeError::TooSmall)));
    }

    #[test]
    fn short_file_is_padded_and_t_size_clamped() {
        let mut file = build_ps_exe(&[9; 10], 0x8001_0000, 0x8001_0000, 0, 0x801f_0000);
        file[0x1c..0x20].copy_from_slice(&0x10000u32.to_le_bytes());
        file.truncate(2048 + 100);
        let img = parse(&file).unwrap();
        assert_eq!(img.segments[0].data.len(), 2048);
        assert_eq!(img.sp, 0x801f_0000);
    }
}
