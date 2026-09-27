#!/bin/sh
# Builds the monitor images a psxmon release carries, from the nugget
# submodule, into $1 (default: dist). Needs mipsel-none-elf-gcc on PATH, as in
# the ghcr.io/grumpycoders/pcsx-redux-build container.
#
# Names are monitor-<link>[-<board>].<ext> for the monitor running on the
# retail BIOS kernel, and openbios-<link>-<target>.<ext> for OpenBIOS with the
# monitor built in:
#   monitor-sio1.ps-exe             PS-EXE, SIO1 (serial port)
#   monitor-sio1-cart.rom           flash cartridge image, SIO1
#   monitor-ft232h-<board>.ps-exe   PS-EXE, FT232H on the expansion port;
#                                   boards in nugget monitor/hosts/ft232h-boards.mk
#   openbios-atcons-h2700.elf       OpenBIOS for the H2700 code cave; turn it
#                                   into a flash image with psxmon patch-h2700
# Disc images of the PS-EXEs come from psxmon mkdisc, in the release workflow.
set -eu

out=$(realpath -m "${1:-dist}")
nugget=$(realpath "$(dirname "$0")/../nugget")
jobs=$(nproc)
mkdir -p "$out"

retail="$nugget/monitor/hosts/retail"
cart="$nugget/monitor/hosts/cart"

clean() {
    make -C "$retail" clean >/dev/null
    make -C "$cart" clean >/dev/null
}

clean
make -C "$retail" -j"$jobs"
cp "$retail/monitor-retail.ps-exe" "$out/monitor-sio1.ps-exe"

clean
make -C "$cart" -j"$jobs"
cp "$cart/monitor-cart.rom" "$out/monitor-sio1-cart.rom"

for board in psx232h-a20 psx232h-a0 picodev-usb picodev-uart piodev-lite; do
    clean
    make -C "$retail" -j"$jobs" MONITOR_LINK=FT232H MONITOR_FT232H_BOARD="$board"
    cp "$retail/monitor-retail.ps-exe" "$out/monitor-ft232h-$board.ps-exe"
done
clean

make -C "$nugget/openbios" clean >/dev/null
make -C "$nugget/openbios" -j"$jobs" BOOT=cart MONITOR=1
cp "$nugget/openbios/openbios.elf" "$out/openbios-atcons-h2700.elf"
make -C "$nugget/openbios" clean >/dev/null

ls -l "$out"
