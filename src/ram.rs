//! Installed RAM and its mirrors.
//!
//! The BIOS sets the first DRAM bank to 8 MiB (DRAM_CTRL, 0x1f801060,
//! = 0xb88, or 0xb80 on early boards), whatever is fitted. A retail
//! console has 2 MiB, which then repeats four times over the 8 MiB; a
//! 4 MiB console repeats twice, a development unit fills it. The cop0 debug
//! unit compares addresses, not RAM cells, so a breakpoint on 0x80000100
//! misses an access through 0x80200100 unless its mask also leaves out
//! the address bits between the installed size and the window.
//!
//! The installed size is found by writing, not by trusting the register:
//! read a sentinel word, compare the words 2, 4 and 6 MiB above it, and
//! for each that matches, flip the sentinel and see whether that word
//! flips too. The sentinel is put back afterwards (see
//! [`Session::probe_ram`](crate::session::Session::probe_ram)).

/// One mebibyte.
pub const MIB: u32 = 1 << 20;

/// The physical RAM window.
pub const RAM_WINDOW: u32 = 8 * MIB;

/// Memory control DRAM_CTRL (RAM_SIZE), read through kseg1.
pub const RAM_SIZE_REG: u32 = 0xbf80_1060;

/// The sentinel word: physical 0, through kseg1. The first 64 bytes of
/// RAM are the kernel's garbage area, which nothing reads for a value (a
/// null pointer store lands there), and the target is halted while it is
/// flipped.
pub const SENTINEL: u32 = 0xa000_0000;

/// Where the sentinel's possible mirrors are, above it.
pub const PROBE_OFFSETS: [u32; 3] = [2 * MIB, 4 * MIB, 6 * MIB];

/// Whether a DRAM_CTRL value makes the first bank 8 MiB (bank size bits
/// 9 and 11 both set, `1 MiB << 3`), so that installed RAM repeats over
/// the whole window. A smaller bank (a program's SetMem(2), 0x888) leaves
/// the rest of the window to the second bank or to bus errors: nothing to
/// mirror there, and nothing safe to probe.
pub fn window_mirrors(ram_size_reg: u32) -> bool {
    ram_size_reg & 0xa00 == 0xa00
}

/// Installed RAM from which of the words at [`PROBE_OFFSETS`] are the
/// sentinel's mirrors.
pub fn size_from_mirrors(mirror: [bool; 3]) -> u32 {
    match mirror {
        [true, _, _] => 2 * MIB,
        [false, true, _] => 4 * MIB,
        _ => RAM_WINDOW,
    }
}

/// Whether `addr` (any segment) is in the physical RAM window.
pub fn in_ram_window(addr: u32) -> bool {
    addr & 0x1fff_ffff < RAM_WINDOW
}

/// A debug-unit compare `mask` for `addr` that also matches every mirror
/// of it when `installed` bytes of RAM repeat over the window: the bits
/// from the installed size up to the window's are left out of the
/// compare. Addresses outside the window keep their mask.
pub fn mirror_mask(addr: u32, mask: u32, installed: u32) -> u32 {
    if !in_ram_window(addr) || !installed.is_power_of_two() || installed >= RAM_WINDOW {
        return mask;
    }
    let mirror_bits = RAM_WINDOW.wrapping_sub(1) & !installed.wrapping_sub(1);
    mask & !mirror_bits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_leave_out_mirror_bits_in_ram_only() {
        // A word watch in RAM, 2 MiB: bits 21 and 22 go.
        assert_eq!(mirror_mask(0x8000_0100, 0x1fff_fffc, 2 * MIB), 0x1f9f_fffc);
        assert_eq!(mirror_mask(0x0000_0100, 0x1fff_fffc, 4 * MIB), 0x1fbf_fffc);
        assert_eq!(mirror_mask(0xa07f_fff0, 0x1fff_fff0, 8 * MIB), 0x1fff_fff0);
        // ROM, scratchpad and I/O keep theirs.
        assert_eq!(mirror_mask(0xbfc0_0100, 0x1fff_ffff, 2 * MIB), 0x1fff_ffff);
        assert_eq!(mirror_mask(0x1f80_0000, 0x1fff_fffc, 2 * MIB), 0x1fff_fffc);
        // A bad size changes nothing.
        assert_eq!(mirror_mask(0x8000_0100, 0x1fff_fffc, 0), 0x1fff_fffc);
        assert_eq!(mirror_mask(0x8000_0100, 0x1fff_fffc, 3 * MIB), 0x1fff_fffc);
    }

    #[test]
    fn sizes_and_windows() {
        assert_eq!(size_from_mirrors([true, true, true]), 2 * MIB);
        assert_eq!(size_from_mirrors([false, true, false]), 4 * MIB);
        assert_eq!(size_from_mirrors([false, false, false]), 8 * MIB);
        assert!(window_mirrors(0xb88));
        assert!(window_mirrors(0xb80));
        // A 2 MiB first bank, the rest a bus error.
        assert!(!window_mirrors(0x888));
        assert!(!window_mirrors(0));
    }
}
