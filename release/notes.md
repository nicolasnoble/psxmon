psxmon binaries for Linux (x86_64), Windows (x86_64) and macOS (arm64), and the monitor images built from the nugget revision named below.

Monitor on the retail BIOS kernel:
- `monitor-sio1.ps-exe`, `monitor-sio1.zip`: load or boot it, talk to it over the serial port. The zip is the disc image, a `.bin` and its `.cue`.
- `monitor-sio1-cart.rom`: flash cartridge image; the console boots straight into the monitor.
- `monitor-ft232h-<board>.ps-exe`, `.zip` (disc image): FT232H on the expansion port, one image per board, since the link addresses are fixed at build time. Boards: `psx232h-a20`, `psx232h-a0`, `picodev-usb`, `picodev-uart`, `piodev-lite`. The `piodev-lite` addresses are read off its schematic.

OpenBIOS with the monitor:
- `openbios-sio1-cart.rom`: flash cartridge image; OpenBIOS takes over from the retail BIOS at boot and runs the monitor on the serial port.
- `openbios-sio1.rom`, `openbios-ft232h-<board>.rom`: 512 KiB BIOS ROM images, for a console with a replaced BIOS chip.
- `openbios-atcons-h2700.elf`: for the H2700 dev board. `psxmon patch-h2700 stock.bin openbios-atcons-h2700.elf -o flash.bin` puts it into a dump of the board's own flash; reset mode 7 boots the monitor, any other mode the stock BIOS.

Run on real hardware: `monitor-sio1-cart.rom` and the H2700 image. Run in PCSX-Redux only: the OpenBIOS SIO1 cart and ROM. Reported working on hardware by a user: `monitor-ft232h-picodev-usb`. Not run anywhere: every other FT232H image.

The disc images carry no license sectors, so they boot on consoles that boot burned discs.
