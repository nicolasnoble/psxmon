psxmon binaries for Linux (x86_64), Windows (x86_64) and macOS (arm64), and the monitor images built from the nugget revision named below.

Monitor on the retail BIOS kernel:
- `monitor-sio1.ps-exe`, `monitor-sio1.bin`/`.cue`: load or boot it, talk to it over the serial port.
- `monitor-sio1-cart.rom`: flash cartridge image; the console boots straight into the monitor.
- `monitor-ft232h-<board>.ps-exe`, `.bin`/`.cue`: FT232H on the expansion port, one image per board, since the link addresses are fixed at build time. Boards: `psx232h-a20`, `psx232h-a0`, `picodev-usb`, `picodev-uart`, `piodev-lite`. None of these has run on real hardware yet, and the `piodev-lite` addresses are read off its schematic.

OpenBIOS with the monitor:
- `openbios-atcons-h2700.elf`: for the H2700 dev board. `psxmon patch-h2700 stock.bin openbios-atcons-h2700.elf -o flash.bin` puts it into a dump of the board's own flash; reset mode 7 boots the monitor, any other mode the stock BIOS.

The disc images carry no license sectors, so they boot on consoles that boot burned discs.
