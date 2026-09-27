//! Bootable PS1 disc images: the same Mode 2 `.bin` as PCSX-Redux's
//! `exe2iso`, byte for byte.
//!
//! The image is raw 2352-byte sectors. Sectors 0..16 are the license (system)
//! area, 16 is the ISO9660 primary volume descriptor with system identifier
//! `PLAYSTATION`, 17 the terminator, 18..22 the four path tables, 22 the root
//! directory and 23 onwards `PSX.EXE;1`. Every sector but a raw license is
//! Mode 2 Form 1 with submode 0x08 and a valid EDC and ECC. With padding on,
//! 150 blank Form 1 sectors follow the end of the volume.

use thiserror::Error;

/// Bytes in a raw sector.
pub const SECTOR_RAW: usize = 2352;
/// User data bytes in a Mode 2 Form 1 sector.
pub const SECTOR_DATA: usize = 2048;
/// Sectors in the license area.
pub const LICENSE_SECTORS: u32 = 16;
/// Blank Form 1 sectors appended past the volume end when padding.
pub const PAD_SECTORS: u32 = 150;
/// The two-second lead-in: image sector 0 sits at MSF 00:02:00.
pub const PREGAP: u32 = 150;

/// Bytes of an SDK license file (2336-byte sectors) or a raw image's first
/// 16 sectors that the builder looks at.
const LICENSE_BYTES: usize = SECTOR_RAW * 16;
/// Where an SDK license file (2336-byte sectors) has the `L` of "Licensed".
const LICENSE_2336_MARK: usize = 0x2492;
/// Where a raw 2352-byte image has the `L` of "Licensed".
const LICENSE_2352_MARK: usize = 0x24e2;
/// Offset of the 2048 user bytes inside a 2336-byte sector.
const LICENSE_2336_SKIP: usize = 8;
const LICENSE_2336_SECTOR: usize = 2336;

const PVD_LBA: u32 = 16;
const TERMINATOR_LBA: u32 = 17;
const PATH_TABLE_LBA: u32 = 18;
const ROOT_LBA: u32 = 22;
const FILE_LBA: u32 = 23;
/// Path table: one record for the root, 8 bytes + name `\x01` + pad.
const PATH_TABLE_SIZE: u32 = 10;
/// MSF minutes are BCD, so the last addressable frame is 99:59:74.
const MAX_FRAMES: u32 = 100 * 60 * 75;

const SYNC: [u8; 12] = [
    0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00,
];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IsoError {
    #[error("PS-EXE of {0} bytes does not fit on a disc")]
    TooLarge(usize),
}

/// A disc position in minutes, seconds and frames (75 per second).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Msf {
    pub m: u8,
    pub s: u8,
    pub f: u8,
}

impl Msf {
    /// The position of image sector `lba` (sector 0 is 00:02:00).
    pub fn from_lba(lba: u32) -> Result<Self, IsoError> {
        let frames = lba
            .checked_add(PREGAP)
            .filter(|&f| f < MAX_FRAMES)
            .ok_or(IsoError::TooLarge(usize::MAX))?;
        let digit = |v: u32| u8::try_from(v).map_err(|_| IsoError::TooLarge(usize::MAX));
        Ok(Self {
            m: digit(frames / (60 * 75))?,
            s: digit(frames / 75 % 60)?,
            f: digit(frames % 75)?,
        })
    }

    /// The BCD bytes of a sector header.
    pub fn to_bcd(self) -> [u8; 3] {
        [bcd(self.m), bcd(self.s), bcd(self.f)]
    }
}

fn bcd(v: u8) -> u8 {
    (v / 10).wrapping_mul(16).wrapping_add(v % 10)
}

const fn edc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        #[allow(clippy::cast_possible_truncation)]
        let mut v = i as u32;
        let mut k = 0u32;
        while k < 8 {
            v = (v >> 1) ^ if v & 1 != 0 { 0xd801_8001 } else { 0 };
            k = k.wrapping_add(1);
        }
        t[i] = v;
        i = i.wrapping_add(1);
    }
    t
}

/// GF(2^8) multiply-by-2 (`f`) and its companion used to solve the two
/// parity symbols (`b`), polynomial 0x11d.
const fn ecc_tables() -> ([u8; 256], [u8; 256]) {
    let mut f = [0u8; 256];
    let mut b = [0u8; 256];
    let mut i = 0usize;
    while i < 256 {
        #[allow(clippy::cast_possible_truncation)]
        let x = i as u32;
        let j = (x << 1) ^ if x & 0x80 != 0 { 0x11d } else { 0 };
        #[allow(clippy::cast_possible_truncation)]
        {
            f[i] = j as u8;
            b[(x ^ j) as usize] = i as u8;
        }
        i = i.wrapping_add(1);
    }
    (f, b)
}

static EDC_TABLE: [u32; 256] = edc_table();
static ECC_TABLES: ([u8; 256], [u8; 256]) = ecc_tables();

/// The CD-ROM EDC (a reflected CRC-32, polynomial 0x8001801b) of `data`.
pub fn edc(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |e, &b| {
        EDC_TABLE[usize::from(e.to_le_bytes()[0] ^ b)] ^ (e >> 8)
    })
}

/// One ECC pass (P or Q) over the 2340 bytes from the header on, writing
/// `2 * major_count` parity bytes to `dest`.
fn ecc_block(
    src: &[u8; 2340],
    major_count: usize,
    minor_count: usize,
    major_mult: usize,
    minor_inc: usize,
    dest: &mut [u8],
) {
    let (f_lut, b_lut) = &ECC_TABLES;
    let size = major_count.wrapping_mul(minor_count);
    for major in 0..major_count {
        let mut index = (major >> 1)
            .wrapping_mul(major_mult)
            .wrapping_add(major & 1);
        let mut a = 0u8;
        let mut b = 0u8;
        for _ in 0..minor_count {
            let t = src[index];
            index = index.wrapping_add(minor_inc);
            if index >= size {
                index = index.wrapping_sub(size);
            }
            a ^= t;
            b ^= t;
            a = f_lut[usize::from(a)];
        }
        let a = b_lut[usize::from(f_lut[usize::from(a)] ^ b)];
        dest[major] = a;
        dest[major.wrapping_add(major_count)] = a ^ b;
    }
}

/// Fill in the EDC, and for Form 1 the P and Q ECC, of a Mode 2 sector
/// whose sync, header, subheader and user data are already in place. The
/// form comes from the subheader's submode bit 5, as on a real drive. Other
/// modes are left alone.
pub fn fill_edc_ecc(sector: &mut [u8; SECTOR_RAW]) {
    if sector[15] != 2 {
        return;
    }
    let form2 = sector[18] & 0x20 != 0;
    let len = if form2 { 2324 + 8 } else { SECTOR_DATA + 8 };
    let end = 16usize.wrapping_add(len);
    let e = edc(&sector[16..end]);
    sector[end..end.wrapping_add(4)].copy_from_slice(&e.to_le_bytes());
    if form2 {
        return;
    }
    // Form 1 ECC is computed as if the four header bytes were zero.
    let mut src = [0u8; 2340];
    src.copy_from_slice(&sector[12..12 + 2340]);
    src[..4].fill(0);
    let mut p = [0u8; 172];
    ecc_block(&src, 86, 24, 2, 86, &mut p);
    src[2064..2064 + 172].copy_from_slice(&p);
    let mut q = [0u8; 104];
    ecc_block(&src, 52, 43, 86, 88, &mut q);
    sector[2076..2076 + 172].copy_from_slice(&p);
    sector[2248..2248 + 104].copy_from_slice(&q);
}

/// A Mode 2 Form 1 sector at image sector `lba` holding `data` (zero padded
/// to 2048 bytes), subheader file 0, channel 0, submode 0x08, coding 0.
pub fn form1_sector(lba: u32, data: &[u8]) -> Result<[u8; SECTOR_RAW], IsoError> {
    let mut s = [0u8; SECTOR_RAW];
    s[..12].copy_from_slice(&SYNC);
    s[12..15].copy_from_slice(&Msf::from_lba(lba)?.to_bcd());
    s[15] = 2;
    s[18] = 0x08;
    s[22] = 0x08;
    let n = data.len().min(SECTOR_DATA);
    s[24..24usize.wrapping_add(n)].copy_from_slice(&data[..n]);
    fill_edc_ecc(&mut s);
    Ok(s)
}

fn both32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off.wrapping_add(4)].copy_from_slice(&v.to_le_bytes());
    buf[off.wrapping_add(4)..off.wrapping_add(8)].copy_from_slice(&v.to_be_bytes());
}

fn both16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off.wrapping_add(2)].copy_from_slice(&v.to_le_bytes());
    buf[off.wrapping_add(2)..off.wrapping_add(4)].copy_from_slice(&v.to_be_bytes());
}

/// A directory record at `buf[0..]`: `name` as given, dates all zero, and
/// with `xa` the 14-byte XA system-use field (attributes 0, file number 0).
/// Returns its length.
fn dir_record(buf: &mut [u8], lba: u32, size: u32, dir: bool, name: &[u8], xa: bool) -> usize {
    let n = name.len();
    let mut len = 33usize.wrapping_add(n);
    if n.is_multiple_of(2) {
        len = len.wrapping_add(1);
    }
    let xa_off = len;
    if xa {
        len = len.wrapping_add(14);
    }
    let rec = &mut buf[..len];
    rec.fill(0);
    rec[0] = u8::try_from(len).unwrap_or(0);
    both32(rec, 2, lba);
    both32(rec, 10, size);
    rec[25] = if dir { 0x02 } else { 0 };
    both16(rec, 28, 1);
    rec[32] = u8::try_from(n).unwrap_or(0);
    rec[33..33usize.wrapping_add(n)].copy_from_slice(name);
    if xa {
        rec[xa_off.wrapping_add(6)] = b'X';
        rec[xa_off.wrapping_add(7)] = b'A';
    }
    len
}

fn pvd(volume_sectors: u32) -> [u8; SECTOR_DATA] {
    let mut b = [0u8; SECTOR_DATA];
    b[0] = 1;
    b[1..6].copy_from_slice(b"CD001");
    b[6] = 1;
    // System, volume, volume set, publisher, preparer and application
    // identifiers, then copyright, abstract and bibliographic file ids:
    // space padded, only the system identifier set.
    for (off, len) in [
        (8usize, 32usize),
        (40, 32),
        (190, 128),
        (318, 128),
        (446, 128),
        (574, 128),
        (702, 37),
        (739, 37),
        (776, 37),
    ] {
        b[off..off.wrapping_add(len)].fill(b' ');
    }
    b[8..8 + 11].copy_from_slice(b"PLAYSTATION");
    both32(&mut b, 80, volume_sectors);
    both16(&mut b, 120, 1);
    both16(&mut b, 124, 1);
    both16(&mut b, 128, 2048);
    both32(&mut b, 132, PATH_TABLE_SIZE);
    b[140..144].copy_from_slice(&PATH_TABLE_LBA.to_le_bytes());
    b[144..148].copy_from_slice(&(PATH_TABLE_LBA + 1).to_le_bytes());
    b[148..152].copy_from_slice(&(PATH_TABLE_LBA + 2).to_be_bytes());
    b[152..156].copy_from_slice(&(PATH_TABLE_LBA + 3).to_be_bytes());
    dir_record(&mut b[156..], ROOT_LBA, 2048, true, &[0], false);
    // The four dates stay binary zero; file structure version 1.
    b[881] = 1;
    b
}

fn terminator() -> [u8; SECTOR_DATA] {
    let mut b = [0u8; SECTOR_DATA];
    b[0] = 255;
    b[1..6].copy_from_slice(b"CD001");
    b[6] = 1;
    b
}

fn path_table(big_endian: bool) -> [u8; SECTOR_DATA] {
    let mut b = [0u8; SECTOR_DATA];
    b[0] = 1;
    let lba = if big_endian {
        ROOT_LBA.to_be_bytes()
    } else {
        ROOT_LBA.to_le_bytes()
    };
    b[2..6].copy_from_slice(&lba);
    let parent: u16 = 1;
    let parent = if big_endian {
        parent.to_be_bytes()
    } else {
        parent.to_le_bytes()
    };
    b[6..8].copy_from_slice(&parent);
    b[8] = 1;
    b
}

fn root_dir(exe_size: u32) -> [u8; SECTOR_DATA] {
    let mut b = [0u8; SECTOR_DATA];
    let mut off = dir_record(&mut b, ROOT_LBA, 2048, true, &[0], true);
    off = off.wrapping_add(dir_record(&mut b[off..], ROOT_LBA, 2048, true, &[1], true));
    dir_record(&mut b[off..], FILE_LBA, exe_size, false, b"PSX.EXE;1", true);
    b
}

/// The 16 license sectors from a license file, as `exe2iso -license` reads
/// it: the first 16 * 2352 bytes (zero filled if short). An SDK license in
/// 2336-byte sectors (an `L` at 0x2492) has each sector's 2048 user bytes
/// rewritten as Form 1; a raw 2352-byte image (an `L` at 0x24e2) is copied
/// as is; anything else, like no file at all, gives zeroed Form 1 sectors.
fn license_area(out: &mut Vec<u8>, license: Option<&[u8]>) -> Result<(), IsoError> {
    let mut buf = vec![0u8; LICENSE_BYTES];
    if let Some(l) = license {
        let n = l.len().min(LICENSE_BYTES);
        buf[..n].copy_from_slice(&l[..n]);
        if buf[LICENSE_2336_MARK] == b'L' {
            for (i, chunk) in (0..LICENSE_SECTORS).zip(buf.as_chunks::<LICENSE_2336_SECTOR>().0) {
                let data = &chunk[LICENSE_2336_SKIP..LICENSE_2336_SKIP + SECTOR_DATA];
                out.extend_from_slice(&form1_sector(i, data)?);
            }
            return Ok(());
        }
        if buf[LICENSE_2352_MARK] == b'L' {
            out.extend_from_slice(&buf);
            return Ok(());
        }
    }
    for i in 0..LICENSE_SECTORS {
        out.extend_from_slice(&form1_sector(i, &[])?);
    }
    Ok(())
}

/// Build the `.bin` of a disc holding `exe` as `PSX.EXE;1`, identical to
/// `exe2iso exe [-license file] [-nopad] -o out.bin`. The PS-EXE is not
/// checked; the BIOS will do that.
pub fn build(exe: &[u8], license: Option<&[u8]>, pad: bool) -> Result<Vec<u8>, IsoError> {
    let too_large = || IsoError::TooLarge(exe.len());
    let exe_size = u32::try_from(exe.len()).map_err(|_| too_large())?;
    let file_sectors = u32::try_from(exe.len().div_ceil(SECTOR_DATA)).map_err(|_| too_large())?;
    let volume = FILE_LBA.checked_add(file_sectors).ok_or_else(too_large)?;
    let total = if pad {
        volume.checked_add(PAD_SECTORS).ok_or_else(too_large)?
    } else {
        volume
    };
    // Fail before allocating if the last sector has no MSF.
    if let Some(last) = total.checked_sub(1) {
        Msf::from_lba(last).map_err(|_| too_large())?;
    }
    let bytes = usize::try_from(total)
        .ok()
        .and_then(|t| t.checked_mul(SECTOR_RAW))
        .ok_or_else(too_large)?;
    let mut out = Vec::with_capacity(bytes);

    license_area(&mut out, license)?;
    out.extend_from_slice(&form1_sector(PVD_LBA, &pvd(volume))?);
    out.extend_from_slice(&form1_sector(TERMINATOR_LBA, &terminator())?);
    for (i, big_endian) in [false, false, true, true].into_iter().enumerate() {
        let lba = PATH_TABLE_LBA.wrapping_add(u32::try_from(i).unwrap_or(0));
        out.extend_from_slice(&form1_sector(lba, &path_table(big_endian))?);
    }
    out.extend_from_slice(&form1_sector(ROOT_LBA, &root_dir(exe_size))?);
    for (lba, chunk) in (FILE_LBA..).zip(exe.chunks(SECTOR_DATA)) {
        out.extend_from_slice(&form1_sector(lba, chunk)?);
    }
    for lba in volume..total {
        out.extend_from_slice(&form1_sector(lba, &[])?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msf_and_bcd() {
        assert_eq!(Msf::from_lba(0).expect("msf").to_bcd(), [0x00, 0x02, 0x00]);
        assert_eq!(Msf::from_lba(16).expect("msf").to_bcd(), [0x00, 0x02, 0x16]);
        assert_eq!(
            Msf::from_lba(75 * 58).expect("msf").to_bcd(),
            [0x01, 0x00, 0x00]
        );
        assert_eq!(
            Msf::from_lba(MAX_FRAMES - PREGAP - 1)
                .expect("msf")
                .to_bcd(),
            [0x99, 0x59, 0x74]
        );
        assert!(Msf::from_lba(MAX_FRAMES - PREGAP).is_err());
    }

    #[test]
    fn edc_is_the_cdrom_crc() {
        // The EDC of a zero Form 1 subheader + data is 0, and a reflected CRC
        // with no final xor leaves 0 over zero input.
        assert_eq!(edc(&[0u8; 2056]), 0);
        // Check value of the CD-ROM EDC (CRC-32/CD-ROM-EDC) over "123456789".
        assert_eq!(edc(b"123456789"), 0x6ec2_edc4);
    }

    #[test]
    fn form1_sector_checks_out() {
        let s = form1_sector(7, b"hello").expect("sector");
        assert_eq!(&s[..12], &SYNC);
        assert_eq!(&s[12..16], &[0x00, 0x02, 0x07, 2]);
        assert_eq!(&s[16..24], &[0, 0, 8, 0, 0, 0, 8, 0]);
        assert_eq!(&s[24..29], b"hello");
        let e = edc(&s[16..2072]);
        assert_eq!(&s[2072..2076], &e.to_le_bytes());
        // Recomputing over the finished sector is a no-op.
        let mut again = s;
        fill_edc_ecc(&mut again);
        assert_eq!(again, s);
        // The ECC ignores the header: another address, same parity.
        let t = form1_sector(8, b"hello").expect("sector");
        assert_eq!(&s[2072..], &t[2072..]);
    }

    #[test]
    fn layout() {
        let exe = vec![0x5au8; 5000];
        let img = build(&exe, None, false).expect("build");
        assert_eq!(img.len(), 26 * SECTOR_RAW);
        let data = |lba: usize| &img[lba * SECTOR_RAW + 24..lba * SECTOR_RAW + 24 + 2048];
        assert_eq!(&data(16)[..8], b"\x01CD001\x01\x00");
        assert_eq!(&data(16)[8..19], b"PLAYSTATION");
        assert_eq!(&data(16)[80..88], &[26, 0, 0, 0, 0, 0, 0, 26]);
        assert_eq!(&data(17)[..7], b"\xffCD001\x01");
        assert_eq!(&data(22)[96 + 33..96 + 42], b"PSX.EXE;1");
        assert_eq!(&data(25)[..5000 - 4096], &exe[4096..]);
        assert!(data(25)[5000 - 4096..].iter().all(|&b| b == 0));
        let padded = build(&exe, None, true).expect("build");
        assert_eq!(padded.len(), (26 + 150) * SECTOR_RAW);
        assert_eq!(&padded[..img.len()], &img[..]);
    }

    #[test]
    fn license_formats() {
        let mut sdk = vec![0x11u8; 16 * 2336];
        sdk[LICENSE_2336_MARK] = b'L';
        let img = build(&[1], Some(&sdk), false).expect("build");
        assert_eq!(img[4 * SECTOR_RAW + 24 + 10], b'L');
        assert_eq!(
            &img[4 * SECTOR_RAW + 16..4 * SECTOR_RAW + 24],
            &[0, 0, 8, 0, 0, 0, 8, 0]
        );

        let mut raw = vec![0x22u8; 16 * SECTOR_RAW];
        raw[LICENSE_2352_MARK] = b'L';
        let img = build(&[1], Some(&raw), false).expect("build");
        assert_eq!(&img[..raw.len()], &raw[..]);

        let none = build(&[1], None, false).expect("build");
        let junk = build(&[1], Some(&[0x33u8; 100]), false).expect("build");
        assert_eq!(none, junk);
    }
}
