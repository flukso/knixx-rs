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

- Rust through rustup; `rust-toolchain.toml` pins the version and adds the
  `thumbv7em-none-eabihf` target.
- [flip-link](https://github.com/knurling-rs/flip-link):
  `cargo install flip-link`. It is the firmware's linker; it puts the stack
  below the statics, so a stack overflow faults instead of corrupting them.
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

| Command | |
|---|---|
| `version` | the commit and build time: `version <commit> built <date>` |
| `sys` | uptime, the USB output queue, USB DTR |
| `ota` | reboot into the bootloader for a BLE firmware update |
| `help` | the commands |

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
| `host/knixx-host` | host library: firmware images (ELF to UF2, Intel HEX, binary), UF2 files, the bootloader's drive, the USB serial port, flashing, BLE scanning |
| `host/bolt` | host tool: `bolt convert`, `bolt uf2`, `bolt scan` |
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
