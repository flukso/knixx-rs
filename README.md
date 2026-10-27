# knixx-rs

Rust firmware and host tools for the knixx Thunder KNX board (Adafruit
Feather nRF52840 with an NCN5121 KNX transceiver), on
[embassy](https://embassy.dev). It ports the Go/TinyGo implementation in
[knixx-go](https://github.com/flukso/knixx-go), which stays the reference,
and aims to be a drop-in replacement for it: same USB IDs, console, config
file and KNX behaviour, so a board can move between the two freely.

Work in progress; see the plan's phases below.

## Flash layout

The firmware keeps the Feather's stock layout: the MBR and the S140 6.1.1
SoftDevice below `0x26000` stay untouched and the SoftDevice is never
enabled, so the Adafruit UF2 bootloader keeps flashing over USB and BLE.

| Region | Range |
|---|---|
| MBR + S140 6.1.1 | `0x00000`–`0x26000` |
| Application | `0x26000`–`0xED000` |
| User data | `0xED000`–`0xF4000` |
| UF2 bootloader | `0xF4000`– |

The application links at `0x26000` (`firmware/knixx-fw/memory.x`) with
`cortex-m-rt`'s `set-vtor`, so its vector table does not depend on the MBR's
forwarding, and RAM from `0x20000008`.

## Requirements

The firmware and the host tools build on macOS and Linux.

- Rust through rustup; `rust-toolchain.toml` pins the version and adds the
  `thumbv7em-none-eabihf` target.
- [flip-link](https://github.com/knurling-rs/flip-link):
  `cargo install flip-link`. It is the firmware's linker; it puts the stack
  below the statics, so a stack overflow faults instead of corrupting them.
- clang and libclang. The littlefs C library is compiled with clang for the
  nRF52840, with its freestanding headers and the two in
  `firmware/c-include/`, so no C library or ARM GCC is needed; the C
  libraries' Rust bindings (littlefs, Nordic's MPSL and SoftDevice
  Controller) are generated with libclang. macOS has both with the Xcode
  command line tools; on Debian or Ubuntu: `apt install clang libclang-dev`.
- For `bolt` only, on Linux: `apt install libdbus-1-dev pkg-config`, for
  BLE through BlueZ. The firmware build, `cargo xtask`, does without.
- Optional: [cargo-deny](https://github.com/EmbarkStudios/cargo-deny), for
  `cargo deny check` (licenses: no copyleft dependencies).

## Building

```
cargo xtask build
```

builds the firmware, stamped with the git commit and the build time, and
writes `build/knixx.elf`, `build/knixx.uf2` and `build/knixx.hex`. It fails
when the image does not fit `0x26000`–`0xED000`.

## Flashing

```
cargo xtask flash [--port <serial port>]
```

builds the firmware and flashes it over USB: it resets the running board
into its UF2 bootloader with the 1200-baud touch (or finds the bootloader's
`FTHR840BOOT` drive after a double tap on reset), copies the UF2 file to the
drive and waits until the board reports the version it just built. It
finds the board by its USB ID, 239A:802A, the same as the Go firmware's.
`bolt uf2 [file]` does the same for a given UF2 file, with `--expect` for
the version line.

macOS must allow the terminal to write to removable volumes (System
Settings, Privacy & Security, Files & Folders).

```
cargo xtask cycle <n>
```

builds once, then flashes `n` times; after each flash the board must report
its version over USB and advertise over BLE. Every flash ends with the
bootloader jumping to the application without a reset, the path on which
USB and the BLE controller can hang.

## Bluetooth

The firmware runs Nordic's SoftDevice Controller
([nrf-sdc](https://github.com/alexmoon/nrf-sdc)), linked into the
application, under the [trouble](https://github.com/embassy-rs/trouble) BLE
host. The S140 SoftDevice in flash is never enabled; the bootloader still
uses it for BLE firmware updates. The multiprotocol service layer (MPSL)
runs from boot and owns the clocks, so USB gets its crystal clock through
it. It keeps interrupt priorities P0 and P1 to itself: the application's
interrupts run at P2 and P3, which the firmware checks at boot. Before
anything else, `main` clears the POWER and CLOCK state that the bootloader
leaves when it jumps to the application (nrf-sdc issue #103).

The board advertises as `knixx`; `bolt scan` looks for it.

## Console

The USB serial port carries the console, as with the Go firmware: the
banner on the first key, then commands after the `>>> ` prompt.

Lines are edited as with the Go firmware: backspace, and a history of 8
lines on the up and down arrows or Ctrl-P and Ctrl-N. Output that does not
fit the 2 KiB queue of the port is dropped whole and noted as
`[N bytes of output dropped]` once the host catches up.

| Command | |
|---|---|
| `switch <A\|B> <on\|off>` | switch the lights of a wall switch |
| `sniff [on\|off]` | show or set the trace of the bus |
| `stat` | report the transceiver's state and system status |
| `config [dump\|init\|get <key>\|set <key> <value>...]` | the configuration |
| `version` | the commit and build time: `version <commit> built <date>` |
| `sys` | uptime, the USB output queue, USB DTR |
| `ota` | reboot into the bootloader for a BLE firmware update |
| `help` | the commands |

## KNX bus

The NCN5121 transceiver is on UARTE0 (TX D0/P0.25, RX D1/P0.24) at 38400
baud, 8N1, as with the Go firmware. The bus task runs `crates/knixx-bus`:
the console, the wall switches and, later, the sensors queue commands for
it; each group write waits for its L_Data.con, up to 500 ms; `sniff on`
traces the bus on the console, with the values of the configured groups
decoded. Errors show on the console and blink red. Received bytes land in
a 256-byte ring by EasyDMA, so none are lost while the flash erases.

`knixx-shell` runs the same bus engine and console commands on the host,
over an NCN5121 attached through an FTDI USB serial adapter (0403:6001),
as the Go firmware's ftdi tool did, without libftdi:

```
cargo run -p knixx-shell -- [--port <serial port>] [--config knixx.ini]
```

## Configuration

The configuration is `/knixx.ini` on a littlefs on the Feather's 2 MiB QSPI
flash (GD25Q16C), the same file in the same littlefs as the Go firmware's,
so a board keeps its configuration when it moves between the two:
`crates/knixx-storage` drives the littlefs C library with the Go firmware's
geometry (256-byte pages, 4 KiB blocks, 512-byte caches) and on-disk version
2.1. The littlefs2 crate is not used: it pins on-disk version 2.0, and
littlefs does not mount a filesystem of a later minor version.
`crates/knixx-storage/testdata/golfs` makes and reads test images with the
Go firmware's littlefs (`go run . write ../go.img`, `go run . read
../rust.img`); the tests read the Go image, and with `UPDATE_TESTDATA=1`
write the Rust one.

At boot the firmware mounts the littlefs, writes the default configuration
when there is no file, and shows what it did under the banner. A blank flash
is formatted, as the Go firmware does, but a flash that holds a littlefs
that does not mount is left alone: the firmware falls back to the built-in
default and blinks red. `config set` and `config init` save the file.

The flash is driven in single-line SPI mode, as the Go firmware did, through
an aligned RAM buffer, since QSPI's EasyDMA only reaches RAM.

## LEDs and switches

The two WS2812B status LEDs on A3 (P0.28) run from PWM0 and EasyDMA, not
bit-banging, which the BLE controller's timing does not allow. LED1 blinks
yellow while the board starts, red on an error and green on motion; LED2
blinks blue while the BLE console is on.

The wall switch contacts, A on/off (D9, D10) and B on/off (A0, A1), are
active low with the internal pull-ups and debounced in software: a press
counts once the contact stayed closed for 30 ms, and again only after it
stayed open for 30 ms. Each press switches the lights as the `switch`
command does.

A panic resets the board; its message survives the reset in RAM and the
next banner shows it as `last panic: ...`.

## Layout

| Path | Contents |
|---|---|
| `firmware/` | the firmware workspace, built for the nRF52840 |
| `crates/knixx-knx` | TP1 frames, individual and group addresses, DPT 1.001/1.018 |
| `crates/knixx-ncn` | the NCN5121 transceiver's host protocol: requests, the indication parser, their descriptions |
| `crates/knixx-device` | the device model (switches, sensor and occupancy groups, ToF settings) and its INI configuration |
| `crates/knixx-bus` | the bus engine: sending with confirmations, stat, occupancy, the sniffer; sans-IO, with an async runner |
| `crates/knixx-cli` | the shared console commands: switch, sniff, stat, config |
| `crates/knixx-console` | the line editor, the console output queues and the BLE NUS framing |
| `crates/knixx-ui` | the status LED player and the wall switch logic |
| `crates/knixx-sensors` | async SHT2x and VCNL4040 drivers on embedded-hal |
| `crates/knixx-storage` | the configuration file on littlefs, compatible with the Go firmware's |
| `host/knixx-host` | host library: firmware images (ELF to UF2, Intel HEX, binary), UF2 files, the bootloader's drive, the USB serial port, flashing, BLE scanning |
| `host/bolt` | host tool: `bolt convert`, `bolt uf2`, `bolt scan` |
| `host/knixx-shell` | KNX shell on the host over an FTDI-attached NCN5121 |
| `xtask/` | `cargo xtask` tasks |

## Tests

```
cargo test
```

runs the host tests, among them the tests of the Go packages the crates
port. The crates in `crates/` are `no_std` without `alloc`: the firmware
links no allocator, so nothing in it can use the heap. `cargo clippy --all-targets` and, in `firmware/`,
`cargo clippy --release` lint the two workspaces.

## License

MIT, see [LICENSE](LICENSE).
