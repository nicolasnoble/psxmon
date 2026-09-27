//! Program images: what gets written where, and where execution starts.
//! PS-EXE, ELF and CPE, told apart by their magic.

use std::path::Path;

/// Size of the PS-EXE header; the text follows it.
pub const PS_EXE_HEADER_SIZE: usize = 2048;
/// Stack top for a PS-EXE that sets none (`s_addr + s_size == 0`), as the
/// reference host uses.
pub const DEFAULT_STACK: u32 = 0x801f_fff0;
/// Stack top for ELF and CPE programs, which carry none.
pub const DEFAULT_STACK_ELF_CPE: u32 = 0x801f_ff00;

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
    #[error("unrecognised program format (want PS-EXE, ELF or CPE)")]
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

fn rd16(b: &[u8], off: usize, what: &'static str) -> Result<u16, ExeError> {
    let bytes = b
        .get(off..)
        .and_then(|s| s.first_chunk::<2>())
        .ok_or(ExeError::Truncated(what))?;
    Ok(u16::from_le_bytes(*bytes))
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

/// `base + index * size`, for walking header tables.
fn table_entry(base: usize, index: usize, size: usize) -> Result<usize, ExeError> {
    index
        .checked_mul(size)
        .and_then(|o| base.checked_add(o))
        .ok_or(ExeError::TooLarge("header table"))
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

const PT_LOAD: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_NOBITS: u32 = 8;
const SHF_ALLOC: u32 = 2;
const EM_MIPS: u16 = 8;

struct ElfSection {
    name: String,
    ty: u32,
    flags: u32,
    offset: usize,
    size: usize,
    link: u32,
}

/// The NUL-terminated string at `off` in `table`.
fn c_string(table: &[u8], off: usize) -> String {
    let s = table.get(off..).unwrap_or_default();
    let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    String::from_utf8_lossy(s.get(..end).unwrap_or_default()).into_owned()
}

fn elf_sections(file: &[u8]) -> Result<Vec<ElfSection>, ExeError> {
    let shoff = to_usize(rd32(file, 0x20, "ELF header")?, "e_shoff")?;
    let shentsize = usize::from(rd16(file, 0x2e, "ELF header")?);
    let shnum = usize::from(rd16(file, 0x30, "ELF header")?);
    let shstrndx = usize::from(rd16(file, 0x32, "ELF header")?);
    if shoff == 0 || shnum == 0 {
        return Ok(Vec::new());
    }
    let mut raw = Vec::with_capacity(shnum);
    for k in 0..shnum {
        let at = table_entry(shoff, k, shentsize)?;
        let field = |o: usize| rd32(file, at.saturating_add(o), "ELF section header");
        raw.push((
            field(0)?,
            ElfSection {
                name: String::new(),
                ty: field(0x04)?,
                flags: field(0x08)?,
                offset: to_usize(field(0x10)?, "sh_offset")?,
                size: to_usize(field(0x14)?, "sh_size")?,
                link: field(0x18)?,
            },
        ));
    }
    let names = raw
        .get(shstrndx)
        .and_then(|(_, s)| file.get(s.offset..s.offset.saturating_add(s.size)))
        .unwrap_or_default();
    Ok(raw
        .into_iter()
        .map(|(name_off, mut s)| {
            s.name = to_usize(name_off, "sh_name")
                .map(|o| c_string(names, o))
                .unwrap_or_default();
            s
        })
        .collect())
}

/// The value of symbol `wanted` in the ELF symbol tables, if any.
fn elf_symbol(file: &[u8], sections: &[ElfSection], wanted: &str) -> Option<u32> {
    for symtab in sections.iter().filter(|s| s.ty == SHT_SYMTAB) {
        let strtab = sections.get(usize::try_from(symtab.link).ok()?)?;
        let strings = file.get(strtab.offset..strtab.offset.saturating_add(strtab.size))?;
        let table = file.get(symtab.offset..symtab.offset.saturating_add(symtab.size))?;
        for sym in table.as_chunks::<16>().0 {
            let name = usize::try_from(rd32(sym, 0, "symbol").ok()?).ok()?;
            if c_string(strings, name) == wanted {
                return rd32(sym, 4, "symbol").ok();
            }
        }
    }
    None
}

/// A 32-bit little-endian MIPS ELF. Each PT_LOAD segment is written at its
/// `p_paddr`, trimmed to the allocated, file-backed sections it holds other
/// than `*_Header` ones (the PS-EXE header section the nugget linker script
/// emits, and the ELF headers a linker may place in the first segment, are
/// not program data). An ELF without section headers loads whole segments.
/// Entry from `e_entry`; gp from `_gp` if present, else 0; sp
/// [`DEFAULT_STACK_ELF_CPE`]. BSS is not zeroed: PS1 programs clear their
/// own, as they must when run from a PS-EXE.
pub fn parse_elf(file: &[u8]) -> Result<Image, ExeError> {
    let ident = file.get(..16).ok_or(ExeError::Truncated("ELF header"))?;
    if ident.get(4) != Some(&1) || ident.get(5) != Some(&1) {
        return Err(ExeError::Elf("not a 32-bit little-endian ELF".into()));
    }
    let machine = rd16(file, 0x12, "ELF header")?;
    if machine != EM_MIPS {
        return Err(ExeError::Elf(format!("machine {machine} is not MIPS")));
    }
    let entry = rd32(file, 0x18, "ELF header")?;
    let phoff = to_usize(rd32(file, 0x1c, "ELF header")?, "e_phoff")?;
    let phentsize = usize::from(rd16(file, 0x2a, "ELF header")?);
    let phnum = usize::from(rd16(file, 0x2c, "ELF header")?);
    let sections = elf_sections(file)?;
    let program: Vec<(usize, usize)> = sections
        .iter()
        .filter(|s| {
            s.flags & SHF_ALLOC != 0
                && s.ty != SHT_NOBITS
                && s.size > 0
                && !s.name.ends_with("_Header")
        })
        .map(|s| (s.offset, s.offset.saturating_add(s.size)))
        .collect();

    let mut segments = Vec::new();
    for k in 0..phnum {
        let at = table_entry(phoff, k, phentsize)?;
        let field = |o: usize| rd32(file, at.saturating_add(o), "ELF program header");
        if field(0)? != PT_LOAD {
            continue;
        }
        let offset = to_usize(field(0x04)?, "p_offset")?;
        let paddr = field(0x0c)?;
        let end = offset.saturating_add(to_usize(field(0x10)?, "p_filesz")?);
        let (lo, hi) = if sections.is_empty() {
            (offset, end)
        } else {
            let inside = program.iter().filter(|&&(s, e)| s >= offset && e <= end);
            let lo = inside.clone().map(|&(s, _)| s).min();
            let hi = inside.map(|&(_, e)| e).max();
            match (lo, hi) {
                (Some(lo), Some(hi)) => (lo, hi),
                _ => continue,
            }
        };
        if lo >= hi {
            continue;
        }
        let data = file
            .get(lo..hi)
            .ok_or(ExeError::Truncated("ELF segment"))?
            .to_vec();
        let delta = u32::try_from(lo.saturating_sub(offset))
            .map_err(|_| ExeError::TooLarge("ELF segment"))?;
        let addr = paddr
            .checked_add(delta)
            .ok_or(ExeError::TooLarge("ELF segment"))?;
        segments.push(Segment { addr, data });
    }
    if segments.is_empty() {
        return Err(ExeError::Elf("no loadable segments".into()));
    }
    Ok(Image {
        segments,
        pc: entry,
        gp: elf_symbol(file, &sections, "_gp").unwrap_or(0),
        sp: DEFAULT_STACK_ELF_CPE,
    })
}

/// The CPE register id that holds the initial PC. psx-spx, PCSX-Redux's
/// loader and ps1-packer all use it; none documents an id for SP or GP.
pub const CPE_REG_PC: u16 = 0x90;

/// A PsyQ CPE: the 4-byte `CPE\x01` id, then chunks. 0x00 end; 0x01 load
/// `[addr:u32][size:u32][data]`; 0x03..0x06 set register `[reg:u16]` to a
/// 4/2/1/3-byte value; 0x02 run address and 0x07 workspace `[u32]` and 0x08
/// select unit `[u8]` are skipped. Register 0x90 is the entry PC; values
/// for other registers are ignored. gp is 0, sp [`DEFAULT_STACK_ELF_CPE`].
pub fn parse_cpe(file: &[u8]) -> Result<Image, ExeError> {
    let mut rest = file.get(4..).ok_or(ExeError::Truncated("CPE header"))?;
    let mut take = |n: usize| -> Result<&[u8], ExeError> {
        let (head, tail) = rest
            .split_at_checked(n)
            .ok_or(ExeError::Truncated("CPE chunk"))?;
        rest = tail;
        Ok(head)
    };
    let mut segments = Vec::new();
    let mut pc = None;
    loop {
        let &[op] = take(1)?
            .first_chunk::<1>()
            .ok_or(ExeError::Truncated("CPE chunk"))?;
        match op {
            0x00 => break,
            0x01 => {
                let addr = rd32(take(4)?, 0, "CPE load chunk")?;
                let size = to_usize(rd32(take(4)?, 0, "CPE load chunk")?, "CPE load size")?;
                segments.push(Segment {
                    addr,
                    data: take(size)?.to_vec(),
                });
            }
            0x03..=0x06 => {
                let reg = rd16(take(2)?, 0, "CPE register chunk")?;
                let width = match op {
                    0x03 => 4,
                    0x04 => 2,
                    0x05 => 1,
                    _ => 3,
                };
                let mut value = [0u8; 4];
                for (dst, src) in value.iter_mut().zip(take(width)?) {
                    *dst = *src;
                }
                if reg == CPE_REG_PC {
                    pc = Some(u32::from_le_bytes(value));
                }
            }
            0x02 | 0x07 => {
                take(4)?;
            }
            0x08 => {
                take(1)?;
            }
            other => return Err(ExeError::Cpe(format!("unsupported chunk 0x{other:02x}"))),
        }
    }
    let pc = pc.ok_or_else(|| ExeError::Cpe("no entry point (register 0x90)".into()))?;
    let mut image = Image {
        segments,
        pc,
        gp: 0,
        sp: DEFAULT_STACK_ELF_CPE,
    };
    image.coalesce();
    Ok(image)
}

/// Parse a program file by its magic.
pub fn parse(file: &[u8]) -> Result<Image, ExeError> {
    if file.starts_with(b"\x7fELF") {
        parse_elf(file)
    } else if file.starts_with(b"CPE\x01") {
        parse_cpe(file)
    } else if file.starts_with(b"PS-X EXE") {
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

    /// A little-endian MIPS ELF32 with the layout a nugget link produces:
    /// a first R segment holding the ELF headers and `.PSX_EXE_Header`, a
    /// second holding `.text` and `.data` with a gap between them, a
    /// `.bss`, and a symbol table with `_gp`.
    fn build_elf(with_gp: bool) -> Vec<u8> {
        fn put(buf: &mut Vec<u8>, off: usize, bytes: &[u8]) {
            let end = off.checked_add(bytes.len()).expect("small offsets");
            if buf.len() < end {
                buf.resize(end, 0);
            }
            buf.get_mut(off..end)
                .expect("resized")
                .copy_from_slice(bytes);
        }
        fn table<const N: usize>(rows: &[[u32; N]]) -> Vec<u8> {
            rows.iter()
                .flatten()
                .flat_map(|v| v.to_le_bytes())
                .collect()
        }
        let w32 = |v: u32| v.to_le_bytes();
        let w16 = |v: u16| v.to_le_bytes();
        let mut f = vec![0u8; 0x1000];
        put(&mut f, 0, b"\x7fELF\x01\x01\x01");
        put(&mut f, 0x10, &w16(2)); // ET_EXEC
        put(&mut f, 0x12, &w16(EM_MIPS));
        put(&mut f, 0x18, &w32(0x8001_0000)); // e_entry
        put(&mut f, 0x1c, &w32(0x34)); // e_phoff
        put(&mut f, 0x2a, &w16(32)); // e_phentsize
        put(&mut f, 0x2c, &w16(2)); // e_phnum
        put(&mut f, 0x2e, &w16(40)); // e_shentsize
        // Program headers: [type, offset, vaddr, paddr, filesz, memsz, flags, align]
        let phdrs: [[u32; 8]; 2] = [
            [
                PT_LOAD,
                0,
                0x8000_f000,
                0x8000_f000,
                0x1000,
                0x1000,
                4,
                0x1000,
            ],
            [
                PT_LOAD,
                0x1000,
                0x8001_0000,
                0x8001_0000,
                0x40,
                0x100,
                7,
                0x1000,
            ],
        ];
        put(&mut f, 0x34, &table(&phdrs));
        put(&mut f, 0x800, b"PS-X EXE header bytes");
        put(&mut f, 0x1000, &[0xaa; 0x10]); // .text
        put(&mut f, 0x1020, &[0xbb; 0x20]); // .data
        // Strings, symbols.
        let shstr = b"\0.PSX_EXE_Header\0.text\0.data\0.bss\0.symtab\0.strtab\0.shstrtab\0";
        put(&mut f, 0x1100, shstr);
        let strtab = b"\0_start\0_gp\0";
        put(&mut f, 0x1180, strtab);
        let gp_name: u32 = if with_gp { 8 } else { 1 };
        let syms: [[u32; 4]; 2] = [[0, 0, 0, 0], [gp_name, 0x8001_8000, 0, 0]];
        put(&mut f, 0x1200, &table(&syms));
        // Section headers: [name, type, flags, addr, offset, size, link, info, align, entsize]
        let shdrs: [[u32; 10]; 8] = [
            [0; 10],
            [1, 1, SHF_ALLOC, 0x8000_f800, 0x800, 0x800, 0, 0, 1, 0],
            [17, 1, SHF_ALLOC | 4, 0x8001_0000, 0x1000, 0x10, 0, 0, 4, 0],
            [23, 1, SHF_ALLOC | 1, 0x8001_0020, 0x1020, 0x20, 0, 0, 4, 0],
            [
                29,
                SHT_NOBITS,
                SHF_ALLOC | 1,
                0x8001_0040,
                0x1040,
                0xc0,
                0,
                0,
                4,
                0,
            ],
            [34, SHT_SYMTAB, 0, 0, 0x1200, 32, 6, 1, 4, 16],
            [
                42,
                3,
                0,
                0,
                0x1180,
                u32::try_from(strtab.len()).expect("short"),
                0,
                0,
                1,
                0,
            ],
            [
                50,
                3,
                0,
                0,
                0x1100,
                u32::try_from(shstr.len()).expect("short"),
                0,
                0,
                1,
                0,
            ],
        ];
        let shoff = 0x1300usize;
        put(&mut f, shoff, &table(&shdrs));
        put(&mut f, 0x20, &w32(u32::try_from(shoff).expect("small")));
        put(&mut f, 0x30, &w16(8)); // e_shnum
        put(&mut f, 0x32, &w16(7)); // e_shstrndx
        f
    }

    #[test]
    fn elf_loads_program_sections_by_paddr() {
        let img = parse(&build_elf(true)).expect("parse ELF");
        assert_eq!(img.pc, 0x8001_0000);
        assert_eq!(img.gp, 0x8001_8000);
        assert_eq!(img.sp, DEFAULT_STACK_ELF_CPE);
        // The header segment is skipped; the program segment spans .text
        // through .data, gap included, and no BSS.
        assert_eq!(img.segments.len(), 1);
        let seg = img.segments.first().expect("segment");
        assert_eq!(seg.addr, 0x8001_0000);
        assert_eq!(seg.data.len(), 0x40);
        assert_eq!(seg.data.get(..0x10), Some(&[0xaa; 0x10][..]));
        assert_eq!(seg.data.get(0x10..0x20), Some(&[0; 0x10][..]));
        assert_eq!(seg.data.get(0x20..), Some(&[0xbb; 0x20][..]));
        assert_eq!(parse(&build_elf(false)).expect("parse ELF").gp, 0);
    }

    #[test]
    fn elf_rejects_wrong_class_and_machine() {
        let mut f = build_elf(true);
        if let Some(class) = f.get_mut(4) {
            *class = 2;
        }
        assert!(matches!(parse(&f), Err(ExeError::Elf(_))));
        let mut f = build_elf(true);
        f.splice(0x12..0x14, 3u16.to_le_bytes());
        assert!(matches!(parse(&f), Err(ExeError::Elf(_))));
    }

    fn cpe_load(addr: u32, data: &[u8]) -> Vec<u8> {
        let mut c = vec![0x01];
        c.extend(addr.to_le_bytes());
        c.extend(u32::try_from(data.len()).expect("short").to_le_bytes());
        c.extend(data);
        c
    }

    #[test]
    fn cpe_chunks() {
        let mut f = b"CPE\x01".to_vec();
        f.extend([0x08, 0x00]); // select unit 0
        f.extend([0x03, 0x90, 0x00]); // PC
        f.extend(0x8001_0010u32.to_le_bytes());
        f.extend([0x05, 0x74, 0x00, 0x7f]); // another register, ignored
        f.extend([0x07, 0, 0, 0, 0]); // workspace
        f.extend(cpe_load(0x8001_0000, &[1, 2, 3, 4]));
        f.extend(cpe_load(0x8001_0004, &[5, 6])); // contiguous: merged
        f.extend(cpe_load(0x8002_0000, &[7]));
        f.extend([0x02, 0, 0, 1, 0x80]); // run address, ignored
        f.push(0x00);
        let img = parse(&f).expect("parse CPE");
        assert_eq!(img.pc, 0x8001_0010);
        assert_eq!((img.gp, img.sp), (0, DEFAULT_STACK_ELF_CPE));
        assert_eq!(
            img.segments,
            vec![
                Segment {
                    addr: 0x8001_0000,
                    data: vec![1, 2, 3, 4, 5, 6]
                },
                Segment {
                    addr: 0x8002_0000,
                    data: vec![7]
                },
            ]
        );
    }

    #[test]
    fn cpe_value_widths_and_errors() {
        // A 24-bit PC value.
        let mut f = b"CPE\x01".to_vec();
        f.extend([0x06, 0x90, 0x00, 0x33, 0x22, 0x11, 0x00]);
        assert_eq!(parse(&f).expect("parse").pc, 0x0011_2233);
        // No end chunk.
        let mut g = b"CPE\x01".to_vec();
        g.extend([0x03, 0x90, 0x00, 0, 0, 1, 0x80]);
        assert!(matches!(parse(&g), Err(ExeError::Truncated(_))));
        // Load chunk longer than the file.
        let mut h = b"CPE\x01".to_vec();
        h.extend(cpe_load(0x8001_0000, &[1, 2, 3]));
        h.truncate(h.len().saturating_sub(1));
        assert!(matches!(parse(&h), Err(ExeError::Truncated(_))));
        // Unknown chunk, and no entry point.
        assert!(matches!(parse(b"CPE\x01\x09"), Err(ExeError::Cpe(_))));
        assert!(matches!(parse(b"CPE\x01\x00"), Err(ExeError::Cpe(_))));
    }
}
