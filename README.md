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

- `run` loads a PS-EXE, starts it, and copies its console text to stdout. It
  serves `break 0, 0x101..0x107` PCDRV calls from `--pcdrv DIR`, jailed to
  that directory. Without `--pcdrv`, every PCDRV call fails with -1. The run
  ends when the program executes `break 4, 0`, and psxmon then exits with the
  program's `a0`. The OS keeps only the low 8 bits of that status, so the
  full value is also printed on stderr. Other exit statuses: 124 on
  timeout, 125 on a host or link error, and 126 when the target stops
  without exiting (a fault or another breakpoint).
- LZ4 is used when the monitor advertises it and it shrinks the program. The
  compressor caps every match at `--max-match` bytes, because the monitor
  decodes while it receives and cannot pause the sender inside a frame.
- `--fast-reload 9` switches SIO1 to 230400 baud after attaching (SET_BAUD
  with its two-PING confirmation). If the new rate does not answer, psxmon
  falls back to the old one. The monitor keeps the new rate, so later
  commands need `--baud 230400`.
- `ping` prints the protocol version, the capability bits, and the BIOS
  checksum with its name from a table of retail BIOS images.

## Build

    cargo build --release
    cargo test

The tests run the session against a simulated monitor (`tests/sim`). The
simulator speaks the byte-stream protocol with 2 MiB of RAM, registers,
SET_BAUD and a scripted target that makes PCDRV and exit breaks.

## License

MIT, see `LICENSE`.
