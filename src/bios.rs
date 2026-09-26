//! Retail BIOS images by the Fletcher-32 the monitor reports in HELLO and
//! PONG (the frame checksum's sums over the 512 KiB at 0xBFC00000). Names
//! follow PCSX-Redux's CRC-32 table (src/core/psxmem.cc), matched by CRC-32
//! over the same dumps; the version string is the ROM's own, read at 0x7FF32.
//! Ported from runner-agent `monitor/bios.ts`.

/// (fletcher32, name), sorted by checksum.
pub const KNOWN_BIOSES: &[(u32, &str)] = &[
    (0x0b469cd2, "SCPH-5000 (2), 2.2 12/04/95 J"),  // crc32 8c93a399
    (0x0be48e6a, "SCPH-101, 4.5 05/25/00 A"),       // crc32 171bdcec
    (0x0d408e6e, "unknown model, 4.5 05/25/00 E"),  // crc32 76b880e5
    (0x18d171d9, "SCPH-7003 (US), 3.0 11/18/96 A"), // crc32 8d8cb7e4
    (0x192a65d9, "unknown model, 2.2 03/06/96 D"),  // crc32 decb22f5
    (0x32d3a79d, "unknown model, 2.1 07/17/95 A"),  // crc32 aff00f2f
    (0x42361d05, "SCPH-1002 (EU), 2.0 05/10/95 E"), // crc32 9bb87c4b
    (0x50436e01, "unknown model, 4.3 03/11/00 J"),  // crc32 f2af798b
    (0x516833dc, "SCPH-1002 - DTLH-3002 (EU), 2.2 12/04/95 E"), // crc32 1e26792f
    (0x51e7fe37, "SCPH-7000 (JP), 4.0 08/18/97 J"), // crc32 ec541cd0
    (0x5f8577fb, "unknown model, 2.0 05/07/95 A"),  // crc32 55847d8c
    (0x68c682e5, "SCPH-1000 (JP)"),                 // crc32 3b601fc8
    (0x823e633d, "SCPH-5000 (JP), 2.2 12/04/95 J"), // crc32 24fc7e17
    (0x890c8a12, "unknown model, 2.1 07/17/95 E"),  // crc32 86c30531
    (0xb9e1b973, "SCPH-3500 (JP), 2.1 07/17/95 J"), // crc32 bc190209
    (0xbee4357a, "SCPH-3000 (JP), 1.1 01/22/1995"), // crc32 3539def6
    (0xbf38df5e, "SCPH-7001 (US), 4.1 12/16/97 A"), // crc32 502224b6
    (0xc094df62, "SCPH-7502 (EU), 4.1 12/16/97 E"), // crc32 318178bf
    (0xc9c4b72e, "SCPH-5502 - SCPH-5552 (EU), 3.0 01/06/97 E"), // crc32 d786f0b9
    (0xee4c998f, "unknown model, 4.4 03/24/00 E"),  // crc32 0bad7ea9
    (0xf238f88d, "SCPH-5502 - SCPH-5552 (2) (EU), 3.0 01/06/97 E"), // crc32 4d9e7c86
    (0xf9b07bcf, "SCPH-5500 (JP), 3.0 09/09/96 J"), // crc32 ff3eeb8c
    (0xfb2e5167, "SCPH-1001 - DTLH-3000 (US), 2.2 12/04/95 A"), // crc32 37157331
];

/// The known name of a BIOS checksum.
pub fn lookup(fletcher: u32) -> Option<&'static str> {
    KNOWN_BIOSES.iter().find(|(f, _)| *f == fletcher).map(|(_, n)| *n)
}

/// The name of a BIOS checksum, or `unknown BIOS 0x........`.
pub fn bios_name(fletcher: u32) -> String {
    lookup(fletcher).map_or_else(|| format!("unknown BIOS 0x{fletcher:08x}"), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table() {
        assert_eq!(KNOWN_BIOSES.len(), 23);
        assert!(KNOWN_BIOSES.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(bios_name(0xbf38df5e), "SCPH-7001 (US), 4.1 12/16/97 A");
        assert_eq!(bios_name(0x12345678), "unknown BIOS 0x12345678");
    }
}
