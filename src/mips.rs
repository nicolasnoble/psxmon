//! What the host needs to know about R3000 code and the PS1 address map to
//! debug through the monitor: where an instruction goes next (for host-side
//! single stepping) and which addresses are RAM, ROM, or nothing.

use crate::proto::NUM_REGS;

/// Where execution can go after the instruction at `pc` (and, for a branch
/// or jump, its delay slot) has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Exactly one successor.
    One(u32),
    /// A coprocessor branch (BCzF / BCzT): the condition flag is not
    /// visible to the host, so it is either `taken` or `fallthrough`.
    Either { taken: u32, fallthrough: u32 },
}

impl Next {
    pub fn addrs(self) -> Vec<u32> {
        match self {
            Next::One(a) => vec![a],
            Next::Either { taken, fallthrough } if taken == fallthrough => vec![taken],
            Next::Either { taken, fallthrough } => vec![taken, fallthrough],
        }
    }
}

fn field(insn: u32, shift: u32, bits: u32) -> u32 {
    (insn >> shift) & ((1u32 << bits).wrapping_sub(1))
}

fn reg(regs: &[u32; NUM_REGS], index: u32) -> u32 {
    usize::try_from(index)
        .ok()
        .and_then(|i| regs.get(i))
        .copied()
        .unwrap_or(0)
}

/// PC-relative branch target: the delay slot's address plus the offset.
fn branch_target(pc: u32, insn: u32) -> u32 {
    let imm = u16::try_from(insn & 0xffff).unwrap_or(0);
    let off = i32::from(imm.cast_signed()).wrapping_mul(4);
    pc.wrapping_add(4).wrapping_add_signed(off)
}

/// The next PC after `insn` at `pc` executes, from the registers as they are
/// before it runs. A branch or jump runs its delay slot at `pc + 4`, then
/// goes to the target or, not taken, to `pc + 8`. The branch condition and a
/// jump register are read before the delay slot runs, as the CPU does.
pub fn next_pc(pc: u32, insn: u32, regs: &[u32; NUM_REGS]) -> Next {
    let op = insn >> 26;
    let rs = reg(regs, field(insn, 21, 5));
    let rt_index = field(insn, 16, 5);
    let rt = reg(regs, rt_index);
    let seq = pc.wrapping_add(4);
    let skip = pc.wrapping_add(8);
    let cond = |taken: bool| Next::One(if taken { branch_target(pc, insn) } else { skip });
    match op {
        0 => match insn & 0x3f {
            // jr, jalr
            0x08 | 0x09 => Next::One(rs),
            _ => Next::One(seq),
        },
        // REGIMM: bltz, bgez, bltzal, bgezal. The R3000 decodes only bit 0
        // (greater-or-equal) and bit 4 (link) of rt, and links whether or not
        // the branch is taken.
        1 => {
            let negative = rs.cast_signed() < 0;
            cond(if rt_index & 1 != 0 {
                !negative
            } else {
                negative
            })
        }
        // j, jal
        2 | 3 => Next::One((seq & 0xf000_0000) | ((insn & 0x03ff_ffff) << 2)),
        4 => cond(rs == rt),
        5 => cond(rs != rt),
        6 => cond(rs.cast_signed() <= 0),
        7 => cond(rs.cast_signed() > 0),
        // COPz with rs = BC: bczf / bczt.
        0x10..=0x13 if field(insn, 21, 5) == 8 => Next::Either {
            taken: branch_target(pc, insn),
            fallthrough: skip,
        },
        _ => Next::One(seq),
    }
}

/// Whether `insn` is a branch or jump, i.e. has a delay slot.
pub fn has_delay_slot(insn: u32) -> bool {
    let op = insn >> 26;
    match op {
        0 => matches!(insn & 0x3f, 0x08 | 0x09),
        1..=7 => true,
        0x10..=0x13 => field(insn, 21, 5) == 8,
        _ => false,
    }
}

/// Kind of memory at an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// Main RAM (2 MiB mirrored over 8 MiB), scratchpad, I/O ports.
    Ram,
    /// BIOS ROM or the EXP1 expansion region (where a monitor cart lives).
    Rom,
}

const MIB: u32 = 1 << 20;

/// Physical ranges (start, length, kind, reachable through kseg1).
const MAP: [(u32, u32, Region, bool); 5] = [
    (0x0000_0000, 8 * MIB, Region::Ram, true),
    (0x1f00_0000, 8 * MIB, Region::Rom, true),
    // Scratchpad is not reachable uncached.
    (0x1f80_0000, 0x400, Region::Ram, false),
    (0x1f80_1000, 0x2000, Region::Ram, true),
    (0x1fc0_0000, 512 * 1024, Region::Rom, true),
];

/// Segment bases the map is mirrored at: kuseg, kseg0, kseg1.
const SEGMENTS: [u32; 3] = [0x0000_0000, 0x8000_0000, 0xa000_0000];

/// The region `addr` falls in and the bytes left in it from `addr`, or None
/// if nothing is there (kseg2, unmapped holes).
pub fn region(addr: u32) -> Option<(Region, u32)> {
    if addr >= 0xc000_0000 {
        return None;
    }
    let kseg1 = addr >= 0xa000_0000;
    let phys = addr & 0x1fff_ffff;
    MAP.iter().find_map(|&(start, len, kind, uncached)| {
        let off = phys.checked_sub(start)?;
        (off < len && (uncached || !kseg1)).then(|| (kind, len.saturating_sub(off)))
    })
}

pub fn is_rom(addr: u32) -> bool {
    matches!(region(addr), Some((Region::Rom, _)))
}

/// A gdb memory map (qXfer:memory-map:read) of the regions above. Marking
/// the ROMs read-only makes gdb use a hardware breakpoint (Z1) for `break`
/// there, and makes it refuse accesses outside the map.
pub fn memory_map_xml() -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\"?>\n<!DOCTYPE memory-map PUBLIC \"+//IDN gnu.org//DTD GDB Memory Map V1.0//EN\" \"http://sourceware.org/gdb/gdb-memory-map.dtd\">\n<memory-map>\n",
    );
    for seg in SEGMENTS {
        for (start, len, kind, uncached) in MAP {
            if seg == 0xa000_0000 && !uncached {
                continue;
            }
            let ty = match kind {
                Region::Ram => "ram",
                Region::Rom => "rom",
            };
            let addr = seg | start;
            xml.push_str(&format!(
                "  <memory type=\"{ty}\" start=\"0x{addr:08x}\" length=\"0x{len:x}\"/>\n"
            ));
            // gdb holds 32-bit MIPS addresses sign-extended to 64 bits, so
            // kseg0/kseg1 are also given as 0xffffffff8.../0xffffffffa....
            if addr >= 0x8000_0000 {
                xml.push_str(&format!(
                    "  <memory type=\"{ty}\" start=\"0xffffffff{addr:08x}\" length=\"0x{len:x}\"/>\n"
                ));
            }
        }
    }
    xml.push_str("</memory-map>\n");
    xml
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regs(set: &[(usize, u32)]) -> [u32; NUM_REGS] {
        let mut r = [0u32; NUM_REGS];
        for &(i, v) in set {
            r[i] = v;
        }
        r
    }

    const PC: u32 = 0x8001_0010;

    #[test]
    fn sequential_and_jumps() {
        let r = regs(&[(8, 0x8002_0000), (31, 0x8001_0040)]);
        assert_eq!(next_pc(PC, 0x2409_0005, &r), Next::One(PC + 4)); // addiu
        assert_eq!(next_pc(PC, 0x0100_0008, &r), Next::One(0x8002_0000)); // jr t0
        assert_eq!(next_pc(PC, 0x03e0_0008, &r), Next::One(0x8001_0040)); // jr ra
        assert_eq!(next_pc(PC, 0x0100_f809, &r), Next::One(0x8002_0000)); // jalr t0
        // j 0x80010100 / jal 0x80010100
        assert_eq!(next_pc(PC, 0x0800_4040, &r), Next::One(0x8001_0100));
        assert_eq!(next_pc(PC, 0x0c00_4040, &r), Next::One(0x8001_0100));
        assert!(has_delay_slot(0x0c00_4040) && !has_delay_slot(0x2409_0005));
    }

    #[test]
    fn conditional_branches() {
        let r = regs(&[(9, 5), (10, 5), (11, 0xffff_fffe)]);
        let back = PC.wrapping_add(4).wrapping_sub(8);
        let fwd = PC + 4 + 0x10;
        // beq t1, t2, +4 words: taken
        assert_eq!(next_pc(PC, 0x112a_0004, &r), Next::One(fwd));
        // bne t1, t2, +4: not taken
        assert_eq!(next_pc(PC, 0x152a_0004, &r), Next::One(PC + 8));
        // beq t1, zero, -2 words: not taken; bne t1, zero, -2: taken
        assert_eq!(next_pc(PC, 0x1120_fffe, &r), Next::One(PC + 8));
        assert_eq!(next_pc(PC, 0x1520_fffe, &r), Next::One(back));
        // blez / bgtz on t3 = -2 and t1 = 5
        assert_eq!(next_pc(PC, 0x1960_0004, &r), Next::One(fwd));
        assert_eq!(next_pc(PC, 0x1d60_0004, &r), Next::One(PC + 8));
        assert_eq!(next_pc(PC, 0x1d20_0004, &r), Next::One(fwd));
        // bltz / bgez / bltzal / bgezal on t3 = -2
        assert_eq!(next_pc(PC, 0x0560_0004, &r), Next::One(fwd));
        assert_eq!(next_pc(PC, 0x0561_0004, &r), Next::One(PC + 8));
        assert_eq!(next_pc(PC, 0x0570_0004, &r), Next::One(fwd));
        assert_eq!(next_pc(PC, 0x0571_0004, &r), Next::One(PC + 8));
        // bc2t: either way
        assert_eq!(next_pc(PC, 0x4901_0004, &r).addrs(), vec![fwd, PC + 8]);
    }

    #[test]
    fn address_map() {
        assert_eq!(region(0x8001_0000), Some((Region::Ram, 8 * MIB - 0x1_0000)));
        assert_eq!(region(0xbfc0_0180).map(|r| r.0), Some(Region::Rom));
        assert_eq!(region(0x1f00_0000).map(|r| r.0), Some(Region::Rom));
        assert_eq!(region(0x9f80_0000).map(|r| r.0), Some(Region::Ram));
        assert_eq!(region(0xbf80_0000), None);
        assert_eq!(region(0x1fa0_0000), None);
        assert_eq!(region(0xfffe_0130), None);
        assert!(is_rom(0x9fc0_0000) && !is_rom(0x8000_0000));
        assert!(memory_map_xml().contains("start=\"0xbfc00000\" length=\"0x80000\""));
    }
}
