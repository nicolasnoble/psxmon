# psxmon

Host tool for the PS1 debug monitor (`monitor/` in PCSX-Redux, wire protocol
version 2, described in `monitor/PROTOCOL.md`). It talks to the monitor over
SIO1 through a serial port. With it you can upload and run a program, stream
the program's console text, serve its PCDRV file I/O from a host directory,
and read or write target memory.

A library (`psxmon`) holds the protocol and the session logic. The `psxmon`
binary is a thin command line on top of it.

## Commands

    psxmon run <file> --port DEV [--baud 115200] [--fast-reload RELOAD]
                      [--lz4 | --no-lz4] [--max-match 128] [--pcdrv DIR]
                      [--timeout SECS] [-v]
    psxmon ping --port DEV [--baud 115200]
    psxmon dump <addr> <len> -o FILE --port DEV
    psxmon write <addr> <file> --port DEV

`--port` may also come from `PSXMON_PORT`. Addresses and lengths take decimal
or `0x` hex.

- `run` loads a program, starts it, and copies its console text to stdout.
  It serves `break 0, 0x101..0x107` PCDRV calls from `--pcdrv DIR`, jailed
  to that directory. Without `--pcdrv`, every PCDRV call fails with -1. The
  run ends when the program executes `break 4, 0`, with its exit code in
  `a0`.
- Programs can be PS-EXE, ELF or CPE, told apart by their magic. An ELF
  loads its PT_LOAD segments at their physical addresses, minus the header
  sections, and starts at `e_entry` with gp from `_gp`. A CPE loads its load
  chunks and starts at register 0x90. ELF and CPE get sp 0x801FFF00.
- LZ4 is used when the monitor advertises it and it shrinks the program. The
  compressor caps every match at `--max-match` bytes, because the monitor
  decodes while it receives and cannot pause the sender inside a frame.
- `--fast-reload 9` switches SIO1 to 230400 baud after attaching (SET_BAUD
  with its two-PING confirmation). If the new rate does not answer, psxmon
  falls back to the old one. The monitor keeps the new rate, so later
  commands need `--baud 230400`.
- If the monitor does not answer at `--baud`, psxmon tries 115200, 230400
  and the `--fast-reload` rate, in that order. It says on stderr which rate
  answered.
- `ping` prints the protocol version, the capability bits, and the BIOS
  checksum with its name from a table of retail BIOS images.

## Exit status

`psxmon run` prints the target's full exit code on stderr. The process exit
status is that code when it is between 0 and 123, and 123 for any larger
code. Other statuses: 124 means the target did not stop before `--timeout`,
125 means a host, link or protocol error, and 126 means the target stopped
without exiting (a fault or another breakpoint).

## Build

    cargo build --release
    cargo test

The toolchain is stable Rust (`rust-toolchain.toml`). CI runs `cargo fmt`,
`cargo clippy` with the lint set in `Cargo.toml`, the tests, and release
builds for Linux, Windows and macOS.

The tests run the session against a simulated monitor (`tests/sim`). The
simulator speaks the byte-stream protocol with 2 MiB of RAM, registers,
SET_BAUD and a scripted target that makes PCDRV and exit breaks.

## License

MIT, see `LICENSE`.
