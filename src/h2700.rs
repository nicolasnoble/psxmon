//! The H2700 flash image, as `openbios/h2700/flash/mkimage.py` in nugget
//! builds it: the stock 512 KiB flash with the OpenBIOS monitor dropped into
//! the 128 KiB code cave at 0xbfc40000, and the stock entry jump at
//! 0xbfc00414 redirected to the monitor's hook. The stock image is the
//! user's; only the monitor ships.
//!
//! Sony's FLASH27 kit (MS-DOS, part of the SDK's `pssn/bin/FLASH27`) writes
//! the flash from `H2700.IMG`, a PS-X EXE flash programmer with the 512 KiB
//! flash image as its payload at file offset [`IMG_PAYLOAD_OFFSET`]. That
//! payload is byte-identical to a raw flash dump, so `patch` accepts either
//! form and patches only the flash bytes; every other byte of an `H2700.IMG`
//! (the programmer itself) passes through unchanged.

use crate::exe::Image;

pub const FLASH_BASE: u32 = 0xbfc0_0000;
pub const FLASH_SIZE: usize = 0x8_0000;
pub const CAVE_BASE: u32 = 0xbfc4_0000;
pub const CAVE_SIZE: usize = 0x2_0000;
pub const ENTRY_JUMP: u32 = 0xbfc0_0414;
/// `j 0xbfc07180`, the word a stock H2700 flash has at `ENTRY_JUMP`.
pub const STOCK_JUMP: u32 = 0x0bf0_1c60;

/// The magic a PS-X EXE (and so `H2700.IMG`) starts with.
const IMG_MAGIC: &[u8; 8] = b"PS-X EXE";
/// Offset of the embedded flash image within `H2700.IMG`.
pub const IMG_PAYLOAD_OFFSET: usize = 0x8800;
const IMG_MIN_LEN: usize = IMG_PAYLOAD_OFFSET + FLASH_SIZE;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PatchError {
    #[error(
        "expected a 512 KiB flash dump or the FLASH27 kit's H2700.IMG \
         (a PS-X EXE at least {IMG_MIN_LEN} bytes), got {0} bytes"
    )]
    StockSize(usize),
    #[error(
        "no stock entry jump at 0x{ENTRY_JUMP:08x} (found 0x{0:08x}), not an H2700 flash image"
    )]
    NotStock(u32),
    #[error("the monitor has nothing to load")]
    Empty,
    #[error("monitor loads at 0x{lo:08x}..0x{hi:08x}, outside the cave at 0x{CAVE_BASE:08x}")]
    OutsideCave { lo: u32, hi: u64 },
    #[error("entry 0x{0:08x} is outside the monitor; was it built with BOOT=cart MONITOR=1?")]
    Entry(u32),
}

const CAVE_OFFSET: usize = 0x4_0000;
const JUMP_OFFSET: usize = 0x414;

/// Where the 512 KiB flash image lives within `stock`: at 0 for a raw dump,
/// at [`IMG_PAYLOAD_OFFSET`] for the FLASH27 kit's `H2700.IMG`. `None` if
/// `stock` is neither shape.
fn flash_offset(stock: &[u8]) -> Option<usize> {
    if stock.len() == FLASH_SIZE {
        return Some(0);
    }
    if stock.len() >= IMG_MIN_LEN && stock.starts_with(IMG_MAGIC) {
        return Some(IMG_PAYLOAD_OFFSET);
    }
    None
}

/// Patch `stock` with the monitor `mon`, returning the flash image. `stock`
/// is either a raw 512 KiB flash dump or the FLASH27 kit's `H2700.IMG`; see
/// [`flash_offset`]. Either way the result is the same length as `stock`,
/// with only the code cave and the entry jump changed.
pub fn patch(stock: &[u8], mon: &Image) -> Result<Vec<u8>, PatchError> {
    let Some(offset) = flash_offset(stock) else {
        return Err(PatchError::StockSize(stock.len()));
    };
    let mut img = stock.to_vec();
    let end = offset.saturating_add(FLASH_SIZE);
    patch_flash(&mut img[offset..end], mon)?;
    Ok(img)
}

/// Patch a 512 KiB flash image in place with the monitor `mon`.
fn patch_flash(flash: &mut [u8], mon: &Image) -> Result<(), PatchError> {
    debug_assert_eq!(flash.len(), FLASH_SIZE);
    let jump = flash
        .get(JUMP_OFFSET..)
        .and_then(|s| s.first_chunk::<4>())
        .map_or(0, |b| u32::from_le_bytes(*b));
    if jump != STOCK_JUMP {
        return Err(PatchError::NotStock(jump));
    }

    let cave_end = u64::from(CAVE_BASE).saturating_add(CAVE_SIZE as u64);
    let mut lo = u32::MAX;
    let mut hi = 0_u64;
    for seg in mon.segments.iter().filter(|s| !s.data.is_empty()) {
        lo = lo.min(seg.addr);
        hi = hi.max(u64::from(seg.addr).saturating_add(seg.data.len() as u64));
    }
    if hi == 0 {
        return Err(PatchError::Empty);
    }
    // objcopy -O binary starts at the lowest load address, so the image
    // has to start at the cave for the hook to land where it is linked.
    if lo != CAVE_BASE || hi > cave_end {
        return Err(PatchError::OutsideCave { lo, hi });
    }
    if !(u64::from(CAVE_BASE)..hi).contains(&u64::from(mon.pc)) {
        return Err(PatchError::Entry(mon.pc));
    }

    for seg in mon.segments.iter().filter(|s| !s.data.is_empty()) {
        let off = seg.addr.wrapping_sub(CAVE_BASE) as usize;
        let start = CAVE_OFFSET.saturating_add(off);
        if let Some(dst) = flash.get_mut(start..start.saturating_add(seg.data.len())) {
            dst.copy_from_slice(&seg.data);
        }
    }
    let j = (2_u32 << 26) | ((mon.pc >> 2) & 0x03ff_ffff);
    if let Some(dst) = flash.get_mut(JUMP_OFFSET..JUMP_OFFSET.saturating_add(4)) {
        dst.copy_from_slice(&j.to_le_bytes());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exe::Segment;

    fn stock() -> Vec<u8> {
        let mut s = vec![0xa5_u8; FLASH_SIZE];
        s[JUMP_OFFSET..JUMP_OFFSET + 4].copy_from_slice(&STOCK_JUMP.to_le_bytes());
        s
    }

    /// A synthetic `H2700.IMG`: some programmer bytes before and after the
    /// embedded flash image, which is `flash`.
    fn img(flash: &[u8]) -> Vec<u8> {
        let mut v = vec![0x5a_u8; IMG_PAYLOAD_OFFSET];
        v[..IMG_MAGIC.len()].copy_from_slice(IMG_MAGIC);
        v.extend_from_slice(flash);
        v.extend_from_slice(&[0x5a_u8; 0x100]); // trailer past the payload
        v
    }

    fn mon(addr: u32, len: usize, pc: u32) -> Image {
        Image {
            segments: vec![Segment {
                addr,
                data: (0..=255_u8).cycle().take(len).collect(),
            }],
            pc,
            gp: 0,
            sp: 0,
        }
    }

    #[test]
    fn patches_the_cave_and_the_entry_jump() {
        let s = stock();
        let img = patch(&s, &mon(CAVE_BASE, 0x100, CAVE_BASE + 0x40)).expect("patch");
        assert_eq!(img.len(), FLASH_SIZE);
        assert_eq!(&img[CAVE_OFFSET..CAVE_OFFSET + 4], &[0, 1, 2, 3]);
        assert_eq!(img[CAVE_OFFSET + 0x100], 0xa5);
        // j 0xbfc40040
        assert_eq!(
            &img[JUMP_OFFSET..JUMP_OFFSET + 4],
            &0x0bf1_0010_u32.to_le_bytes()
        );
        // Nothing else moved.
        assert_eq!(img[..JUMP_OFFSET], s[..JUMP_OFFSET]);
        assert_eq!(
            img[JUMP_OFFSET + 4..CAVE_OFFSET],
            s[JUMP_OFFSET + 4..CAVE_OFFSET]
        );
    }

    #[test]
    fn refuses_anything_but_a_stock_h2700_flash() {
        let m = mon(CAVE_BASE, 0x100, CAVE_BASE);
        assert_eq!(patch(&[0; 1024], &m), Err(PatchError::StockSize(1024)));
        let mut s = stock();
        s[JUMP_OFFSET] ^= 1;
        assert_eq!(patch(&s, &m), Err(PatchError::NotStock(STOCK_JUMP ^ 1)));
        // An already-patched image is not stock either.
        let once = patch(&stock(), &m).expect("patch");
        assert!(matches!(patch(&once, &m), Err(PatchError::NotStock(_))));
    }

    #[test]
    fn refuses_a_monitor_that_is_not_a_cave_build() {
        let s = stock();
        assert!(matches!(
            patch(&s, &mon(0x8001_0000, 0x100, 0x8001_0000)),
            Err(PatchError::OutsideCave { .. })
        ));
        assert!(matches!(
            patch(&s, &mon(CAVE_BASE, CAVE_SIZE + 4, CAVE_BASE)),
            Err(PatchError::OutsideCave { .. })
        ));
        assert_eq!(
            patch(&s, &mon(CAVE_BASE, 0x100, 0x8001_0000)),
            Err(PatchError::Entry(0x8001_0000))
        );
    }

    #[test]
    fn patches_the_flash_embedded_in_an_h2700_img() {
        let s = stock();
        let whole = img(&s);
        let m = mon(CAVE_BASE, 0x100, CAVE_BASE + 0x40);
        let patched = patch(&whole, &m).expect("patch");
        assert_eq!(patched.len(), whole.len());

        // The embedded flash image is patched the same way the raw dump is.
        let want_flash = patch(&s, &m).expect("patch raw");
        assert_eq!(
            &patched[IMG_PAYLOAD_OFFSET..IMG_PAYLOAD_OFFSET + FLASH_SIZE],
            &want_flash[..]
        );
        // Everything outside the payload -- the programmer -- is untouched.
        assert_eq!(patched[..IMG_PAYLOAD_OFFSET], whole[..IMG_PAYLOAD_OFFSET]);
        assert_eq!(
            patched[IMG_PAYLOAD_OFFSET + FLASH_SIZE..],
            whole[IMG_PAYLOAD_OFFSET + FLASH_SIZE..]
        );
    }

    #[test]
    fn refuses_an_img_without_the_stock_jump_or_magic() {
        let m = mon(CAVE_BASE, 0x100, CAVE_BASE);
        let mut whole = img(&stock());
        whole[IMG_PAYLOAD_OFFSET + JUMP_OFFSET] ^= 1;
        assert!(matches!(patch(&whole, &m), Err(PatchError::NotStock(_))));

        // Right length, no PS-X EXE magic: not a recognized stock shape.
        let mut not_magic = img(&stock());
        not_magic[0] = 0;
        let len = not_magic.len();
        assert_eq!(patch(&not_magic, &m), Err(PatchError::StockSize(len)));

        // Magic present but shorter than a real payload fits: also refused.
        let short = img(&stock())[..IMG_MIN_LEN - 1].to_vec();
        let len = short.len();
        assert_eq!(patch(&short, &m), Err(PatchError::StockSize(len)));
    }
}
