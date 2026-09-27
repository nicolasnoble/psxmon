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
    psxmon gdb [<file>] --port DEV [--baud 115200] [--fast-reload RELOAD]
                        [--listen 127.0.0.1:3333] [--no-lz4] [--pcdrv DIR] [-v]
    psxmon ping --port DEV [--baud 115200]
    psxmon dump <addr> <len> -o FILE --port DEV
    psxmon write <addr> <file> --port DEV
    psxmon patch-h2700 <stock> <monitor> -o FILE
    psxmon mkdisc <exe> -o FILE.bin [--license FILE] [--no-pad]

`--port` may also come from `PSXMON_PORT`. Addresses and lengths take decimal
or `0x` hex.

- `run` loads a program, starts it, and copies its console text to stdout.
  It serves `break 0, 0x101..0x107` PCDRV calls from `--pcdrv DIR`, jailed
  to that directory. Without `--pcdrv`, every PCDRV call fails with -1. The
  run ends when the program executes `break 4, 0`, with its exit code in
  `a0`.
- Programs can be PS-EXE, ELF, CPE or PSF, told apart by their magic. An ELF
  loads its PT_LOAD segments at their physical addresses, minus the header
  sections, and starts at `e_entry` with gp from `_gp`. A CPE loads its load
  chunks and starts at register 0x90. ELF and CPE get sp 0x801FFF00.
- PSF (version 0x01) and MiniPSF load as PCSX-Redux loads them: `_lib`
  first, then the file's own PS-EXE, then `_lib2`, `_lib3`, ..., with
  library paths relative to the file naming them. pc and sp (`s_addr`,
  0x801FFFF0 if zero) come from the first PS-EXE loaded, so a MiniPSF
  starts at its library's entry point. Missing libraries are skipped with a
  warning.
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
- `patch-h2700` builds a flash image for the H2700 from a dump of the
  cart's own 512 KiB flash and the OpenBIOS monitor ELF: the monitor goes
  into the code cave at 0xbfc40000 and the stock entry jump is pointed at
  its hook. It refuses anything that is not an unpatched H2700 flash. With
  the reset-mode switch at 7 the cart boots the monitor; any other mode
  boots the stock BIOS.
- `mkdisc` builds a bootable disc image with the PS-EXE as `PSX.EXE`: a
  Mode 2 `.bin` identical to PCSX-Redux's `exe2iso`, and a `.cue` beside it.
  Sectors 0-15 hold `--license` (an SDK file in 2336-byte sectors or a raw
  image), or zeros without it. `--no-pad` leaves out the 150 blank sectors
  after the volume.

## Debugging with gdb

`psxmon gdb` is a GDB remote server (RSP over TCP) on top of the monitor.
Use it with `gdb-multiarch` or any `mips` gdb:

    psxmon gdb farmjob.ps-exe --port /dev/ttyUSB0 --pcdrv ./pc
    gdb-multiarch farmjob.elf -ex 'set architecture mips:3000' \
        -ex 'target remote 127.0.0.1:3333'

- With a program, psxmon loads it (LZ4 as with `run`) and leaves it halted
  on its first instruction, with the registers RUN gives it. A fresh monitor
  has no halted context, so this is done by planting `break 0x3ff, 0` at the
  entry, RUNning, and putting the original word back once it has stopped.
  Without a program, gdb attaches to whatever the monitor has halted.
- One gdb connection is served. On `detach` or `kill` the target is left
  halted where it is (it cannot be stopped again once running, so this
  keeps it attachable: run `psxmon gdb` without a program to attach again).
  When the program exits (`break 4, 0`) gdb sees the process exit, and
  psxmon exits with the code as `run` does. gdb's `W` packet carries only
  the low 8 bits of the code; psxmon prints the full code on stderr.
- Console text goes to psxmon's stdout. PCDRV calls are served from
  `--pcdrv` while the target runs, exactly as with `run`; gdb never sees
  them.
- Software breakpoints are gdb's own. psxmon does not offer `Z0`, so gdb
  writes its `break` instructions into RAM itself, and the monitor stops on
  them with the PC on the break, where gdb expects it. psxmon sends a
  memory map with the BIOS (`0x1fc00000`) and EXP1 (`0x1f000000`) regions
  marked read-only, in kuseg, kseg0 and kseg1, so gdb uses a hardware
  breakpoint for `break` there. A memory write that does not take in ROM
  returns an error to gdb.
- Hardware breakpoints (`hbreak`, or `break` in ROM): the monitor's one
  cop0 exec breakpoint, kept for ROM. One at a time; `hbreak` in RAM is
  refused (use `break`).
- Watchpoints (`watch`, `rwatch`, `awatch`): the one cop0 data breakpoint.
  The length must be a power of two and the address aligned to it. The
  unit compares the address the CPU issues, so a word store that covers a
  watched byte at another address does not trigger it.
- Breakpoints and the watch match an address in all three segments (the
  compare mask leaves out bits 29-31). A hardware stop disarms the whole
  debug unit, so psxmon re-arms it with SET_BP before every CONT, and turns
  it off after every other stop so the monitor's own memory accesses cannot
  trip it.
- Single step: the target description says `<osabi>none</osabi>`, so gdb
  sends `vCont;s` rather than stepping with breakpoints of its own. psxmon
  steps on the host: it decodes the instruction at PC (branches and jumps with their
  delay slot), plants `break 0x3ff, 0` at the successor in RAM, or lends
  the exec breakpoint to a successor in ROM, continues, and restores
  everything at the stop.
- Ctrl-C stops a running target when the monitor reports the `stop`
  capability (`psxmon ping` lists it): psxmon sends STOP, the monitor
  halts the target at its next interrupt, and gdb sees SIGINT with the PC
  where it was. STOP is resent every second until a stop comes. A target
  that has interrupts off, or never unmasks one, does not stop; nor does
  any target under a monitor without the capability (older monitors,
  ATCONS on the DTL-H2700), where psxmon ignores the interrupt and keeps
  waiting for a breakpoint, watch, fault or exit. `psxmon run` is
  unchanged.
- Faults are reported as signals: address errors and bus errors as
  SIGBUS, reserved instruction and coprocessor unusable as SIGILL,
  overflow as SIGFPE. The monitor cannot deliver a signal, so continuing
  re-executes the faulting instruction unless the PC is changed.
- Limits: a `break` in a branch delay slot (a gdb breakpoint there, or a
  program's own) is reported by the monitor as a hardware stop with the PC
  on the branch (see PROTOCOL.md, section 13). A host step of a branch to
  itself is reported done without running it. The R3000A has no FPU; gdb's
  FP registers read as 0 and writes to them are dropped.

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

The tests run the session against a simulated monitor
(`tests/session/sim.rs`). The simulator speaks the byte-stream protocol with
2 MiB of RAM, registers, SET_BAUD, and either a scripted target that makes
PCDRV and exit breaks or a small R3000 interpreter with a ROM and the cop0
debug unit. `tests/gdb` drives `psxmon gdb` against it in raw RSP; with
`PSXMON_GDB_E2E=1` it also runs `gdb-multiarch --batch` against it.

## License

MIT, see `LICENSE`.
