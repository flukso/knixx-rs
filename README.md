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

To flash, copy `build/knixx.uf2` to the `FTHR840BOOT` drive that the board
shows after a double tap on its reset button.

## Layout

| Path | Contents |
|---|---|
| `firmware/` | the firmware workspace, built for the nRF52840 |
| `host/knixx-host` | host library: firmware images (ELF to UF2, Intel HEX, binary), UF2 files |
| `host/bolt` | host tool: `bolt convert` |
| `xtask/` | `cargo xtask` tasks |

## Tests

```
cargo test
```

runs the host tests. `cargo clippy --all-targets` and, in `firmware/`,
`cargo clippy --release` lint the two workspaces.

## License

MIT, see [LICENSE](LICENSE).
