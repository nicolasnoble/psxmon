//! Program images: what gets written where, and where execution starts.
//! PS-EXE for now; ELF and CPE fit the same [`Image`].

use std::path::Path;

/// Size of the PS-EXE header; the text follows it.
pub const PS_EXE_HEADER_SIZE: usize = 2048;
/// Stack top for a PS-EXE that sets none (`s_addr + s_size == 0`), as the
/// reference host uses.
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
        self.segments
            .iter()
            .map(|s| s.data.len())
            .fold(0, usize::saturating_add)
    }

    /// Merge each segment into the one before it when it starts exactly
    /// where that one ends, so a CPE's many small load chunks go out as few
    /// transfers. Order is kept, so later data still wins over earlier.
    pub fn coalesce(&mut self) {
        let mut out: Vec<Segment> = Vec::with_capacity(self.segments.len());
        for seg in self.segments.drain(..) {
            if let Some(prev) = out.last_mut()
                && u32::try_from(prev.data.len())
                    .ok()
                    .and_then(|n| prev.addr.checked_add(n))
                    == Some(seg.addr)
            {
                prev.data.extend_from_slice(&seg.data);
            } else {
                out.push(seg);
            }
        }
        self.segments = out;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExeError {
    #[error("file too small to be a PS-EXE: need at least a 0x800-byte header")]
    TooSmall,
    #[error("unrecognised program format (want a PS-EXE)")]
    Unknown,
    #[error("truncated {0}")]
    Truncated(&'static str),
    #[error("ELF: {0}")]
    Elf(String),
    #[error("CPE: {0}")]
    Cpe(String),
    #[error("{0} does not fit in the address space")]
    TooLarge(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn rd32(b: &[u8], off: usize, what: &'static str) -> Result<u32, ExeError> {
    let bytes = b
        .get(off..)
        .and_then(|s| s.first_chunk::<4>())
        .ok_or(ExeError::Truncated(what))?;
    Ok(u32::from_le_bytes(*bytes))
}

fn to_usize(v: u32, what: &'static str) -> Result<usize, ExeError> {
    usize::try_from(v).map_err(|_| ExeError::TooLarge(what))
}

/// A PS-EXE, the way the reference host loads it: the file is padded with
/// zeros to a 2 KiB multiple, the text is `t_size` bytes after the header
/// (all of it when `t_size` is 0), loaded at `t_addr`; entry `pc0`, `gp0`,
/// and `sp = s_addr + s_size` or [`DEFAULT_STACK`].
pub fn parse_ps_exe(file: &[u8]) -> Result<Image, ExeError> {
    let text = file.get(PS_EXE_HEADER_SIZE..).ok_or(ExeError::TooSmall)?;
    let padded = text
        .len()
        .checked_next_multiple_of(PS_EXE_HEADER_SIZE)
        .ok_or(ExeError::TooLarge("PS-EXE"))?;
    let size = match to_usize(rd32(file, 0x1c, "PS-EXE header")?, "t_size")? {
        0 => padded,
        n => n.min(padded),
    };
    let mut data = text
        .get(..size.min(text.len()))
        .unwrap_or_default()
        .to_vec();
    data.resize(size, 0);
    // (s_addr + s_size) >>> 0 in the reference host: a u32 sum that wraps.
    let sp = rd32(file, 0x30, "PS-EXE header")?.wrapping_add(rd32(file, 0x34, "PS-EXE header")?);
    Ok(Image {
        segments: vec![Segment {
            addr: rd32(file, 0x18, "PS-EXE header")?,
            data,
        }],
        pc: rd32(file, 0x10, "PS-EXE header")?,
        gp: rd32(file, 0x14, "PS-EXE header")?,
        sp: if sp == 0 { DEFAULT_STACK } else { sp },
    })
}

/// Parse a program file by its magic.
pub fn parse(file: &[u8]) -> Result<Image, ExeError> {
    if file.starts_with(b"PS-X EXE") {
        parse_ps_exe(file)
    } else {
        Err(ExeError::Unknown)
    }
}

pub fn load(path: &Path) -> Result<Image, ExeError> {
    parse(&std::fs::read(path)?)
}

/// A minimal PS-EXE around `text`, for tests and tools.
pub fn build_ps_exe(
    text: &[u8],
    t_addr: u32,
    pc: u32,
    gp: u32,
    stack: u32,
) -> Result<Vec<u8>, ExeError> {
    let padded = text
        .len()
        .checked_next_multiple_of(PS_EXE_HEADER_SIZE)
        .ok_or(ExeError::TooLarge("PS-EXE"))?;
    let t_size = u32::try_from(padded).map_err(|_| ExeError::TooLarge("PS-EXE"))?;
    let mut h = vec![0u8; PS_EXE_HEADER_SIZE];
    let mut put = |off: usize, bytes: &[u8]| {
        if let Some(dst) = h.get_mut(off..off.saturating_add(bytes.len())) {
            dst.copy_from_slice(bytes);
        }
    };
    put(0, b"PS-X EXE");
    put(0x10, &pc.to_le_bytes());
    put(0x14, &gp.to_le_bytes());
    put(0x18, &t_addr.to_le_bytes());
    put(0x1c, &t_size.to_le_bytes());
    put(0x30, &stack.to_le_bytes());
    h.extend_from_slice(text);
    h.resize(PS_EXE_HEADER_SIZE.saturating_add(padded), 0);
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_exe_fields() {
        let file =
            build_ps_exe(&[1, 2, 3], 0x8001_0000, 0x8001_0000, 0x8002_0000, 0).expect("build");
        let img = parse(&file).expect("parse");
        assert_eq!(img.pc, 0x8001_0000);
        assert_eq!(img.gp, 0x8002_0000);
        assert_eq!(img.sp, DEFAULT_STACK);
        assert_eq!(img.segments.len(), 1);
        let seg = img.segments.first().expect("one segment");
        assert_eq!(seg.addr, 0x8001_0000);
        assert_eq!(seg.data.len(), 2048);
        assert_eq!(seg.data.get(..4), Some(&[1u8, 2, 3, 0][..]));
        assert!(matches!(parse_ps_exe(&[0; 16]), Err(ExeError::TooSmall)));
        assert!(matches!(parse(&[0; 4096]), Err(ExeError::Unknown)));
    }

    #[test]
    fn short_file_is_padded_and_t_size_clamped() {
        let mut file =
            build_ps_exe(&[9; 10], 0x8001_0000, 0x8001_0000, 0, 0x801f_0000).expect("build");
        if let Some(t_size) = file.get_mut(0x1c..0x20) {
            t_size.copy_from_slice(&0x10000u32.to_le_bytes());
        }
        file.truncate(2048 + 100);
        let img = parse(&file).expect("parse");
        assert_eq!(img.segments.first().map(|s| s.data.len()), Some(2048));
        assert_eq!(img.sp, 0x801f_0000);
    }
}
