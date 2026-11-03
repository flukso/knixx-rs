# knixx-rs: port knixx (Go/TinyGo) to Rust on embassy-rs

> **Status: completed.** The implementation starts with commit `263fc45`
> (2026-10-27, "scaffold knixx-rs: blinky at 0x26000, bolt convert, cargo
> xtask build", phase 0); phases 1 to 10 run through `4e59f30` (phase 10:
> hardening and acceptance). Released in v0.1.0. Recovered from the planning
> session's transcript: its plan file was later reused for the Rhai Grain
> scripting plan.

## Context
knixx is the TinyGo firmware for the knixx Thunder KNX wall-switch/sensor board (Feather nRF52840 + NCN5121 transceiver, SHT20, VCNL4040, VL53L7CX ToF, 2× WS2812, 4 switch contacts) plus Go host tools (`bolt`: genpkg/ota/term/uf2; an FTDI KNX shell). TinyGo's runtime just cost us a multi-day hunt (lost wakeup in `rtc_sleep`, now patched), and the cooperative scheduler, heap-avoidance rules and `make allocs` tooling add ongoing friction. Goal: an equivalent Rust implementation on embassy-rs, in a **new repository `knixx-rs`**, MIT-licensed and alloc-free, that drops into the stock Adafruit Feather flash layout (MBR + S140 6.1.1 untouched, app at 0x26000, UF2 + legacy BLE DFU still work) and is a drop-in replacement for the Go firmware (same USB IDs, console, config file, KNX behaviour), so boards can move Go ↔ Rust freely.

Decisions taken with the user:
- **metaneutrons/knx-rs: reference only.** It is GPL-3.0-only and needs `alloc`; we port our own tp1/ncn/bus code and mirror knx-rs names (`IndividualAddress`, `GroupAddress`, `TpFrame`, `TpIndication`, `TransmitConfirm`…) so adopting it later is cheap. `cargo-deny` bans GPL to keep it out.
- **Separate repository**; the Go repo stays the reference (golden tests, ULD C harness, genfw, pcb/, cad/).
- **Keep littlefs + `/knixx.ini` on the QSPI flash** (littlefs2 crate) so config survives Go ↔ Rust swaps.
- **SoftDevice: not used.** nrf-softdevice supports S140 7.x only (would mean replacing the bootloader's SD). BLE = trouble-host + nrf-sdc/nrf-mpsl (MIT/Apache) linked into app flash; S140 6.1.1 stays in flash, never enabled (the bootloader disables it before jumping; `cortex-m-rt` `set-vtor` bypasses MBR forwarding). Prior art: trouble PR #694 (app at 0x26000, S140 6.1.1 left in place).

## Repository layout (knixx-rs)
```
Cargo.toml            host workspace: crates/*, host/*, xtask   (edition 2024)
rust-toolchain.toml   pinned stable + thumbv7em-none-eabihf
deny.toml             license allowlist (MIT/Apache/BSD/ISC/Zlib), GPL banned
crates/               #![no_std], no alloc, host-tested (cfg(test) extern crate std)
  knixx-knx           tp1 Telegram pack/unpack, addresses, DPT 1.001/1.018/9.xxx f16 enc/dec   ← src/tp1, thunder/knx-go bits
  knixx-ncn           NCN5121 services, TxData index interleave, Parser::feed, describe          ← src/ncn
  knixx-bus           sans-IO Engine (sender/con wait, occupancy, sniffer lines[256], format)
                      + async runner over embedded_io_async Read+Write                          ← src/bus
  knixx-app           thunder Device+validation, config INI parse/format/get/set (default.ini via include_bytes!), cli  ← src/thunder, config, cli
  knixx-console       lineedit, outq, nus Ring/UTF-8-safe split/Timer                           ← src/lineedit, outq, nus
  knixx-ui            led::Player, switches Edges/Hold                                          ← src/led, switches
  knixx-sensors       sht2x, vcnl4040 on embedded-hal-async I2c                                 ← src/internal/sht2x, vcnl4040
  knixx-vl53l7cx      ULD port + motion/threshold plugins, blobs via include_bytes!, golden test ← src/internal/vl53l7cx
host/
  knixx-host          std lib: uf2 writer/check, elf→bin/hex/uf2 (object), ihex, legacydfu (genpkg + DFU state machine over Link trait),
                      usbserial (sysfs scan, macOS enumerate by VID:PID, 1200 touch, query), blescan, mount (/Volumes; /proc/mounts + UDisks2 via zbus)
  bolt                bin: convert, genpkg, uf2, ota, term                                      ← src/host/bolt + internal/*
  knixx-shell         bin: KNX shell over the FTDI VCP (serialport), same bus runner on tokio  ← src/ftdi
xtask/                cargo xtask build|flash|dfu|ota|term|test|size|soak  (replaces the Makefile)
firmware/             separate workspace, .cargo/config.toml target thumbv7em-none-eabihf, linker flip-link
  knixx-fw/           memory.x, build.rs (version), src/{main,usb,console,nus,ble,ota,storage,bus,sensor,tof,leds,switches,sys,panic}.rs
tools/soak.py         copied from the Go repo
```
Firmware has **no `#[global_allocator]`**: any dependency needing `alloc` fails to link, which enforces the no-heap rule at compile time and replaces `tools/allocs` + `allocs.txt`.

## Crates
Firmware: embassy-executor / embassy-nrf (nrf52840, time-driver-rtc1, gpiote) / embassy-time / embassy-sync / embassy-usb / embassy-embedded-hal / embassy-futures — **versions pinned to the set nrf-sdc's release was tested with** (one `[patch]` git rev for the whole family if needed); cortex-m-rt (`set-vtor`), flip-link; nrf-sdc 0.4 + nrf-mpsl, trouble-host 0.8 (peripheral, gatt); littlefs2 (latest, default features off, `c-stubs`); heapless, static_cell, embedded-io-async, embedded-hal-async 1.0; defmt + defmt-rtt behind an optional `probe` feature only.
Host: clap, anyhow, tokio, btleplug, serialport, crossterm, zip, serde_json (BTreeMap + 4-space PrettyFormatter), object, zbus (Linux), uuid. ihex reader/writer ported by hand from `src/host/internal/legacydfu/ihex.go` to keep genpkg output byte-identical.

## Key technical design
- **Memory/boot**: `memory.x` FLASH 0x26000 len 0xC7000 (ends at user data 0xED000), RAM 0x20000008 len 0x3FFF8. Release profile: opt-level "s", fat LTO, codegen-units 1, panic abort. Panic handler stores file/line/msg in an `.uninit` record and resets; next boot prints `last panic: …`.
- **Bootloader-jump sanitising** (nrf-sdc issue #103: USB + MPSL hang after a UF2/DFU jump): first thing in `main`, clear POWER USB events, CLOCK HFCLKSTARTED/LFCLKSTARTED, POWER/CLOCK INTEN and pending CLOCK_POWER; bind both `nrf_mpsl::ClockInterruptHandler` and `usb::vbus_detect::InterruptHandler` on CLOCK_POWER. Fallback: one-time `sys_reset` via a GPREGRET2 marker.
- **MPSL always on from boot** (owns LFCLK/HFCLK; USB's HFXO requested through MPSL); SDC/trouble built at boot with static buffers; `nus on/off` only start/stop advertising. Reserved: RADIO/TIMER0/RTC0 at P0, TEMP, PPI 19/30/31, EGU0. App IRQs at P2–P3 (RTC1 time driver P2; GPIOTE, UARTE0+TIMER1, USBD, TWIM1, QSPI, PWM0 at P3); debug-build assert that no app IRQ sits at P0/P1.
- **Tasks** (one thread-mode executor; large buffers in `StaticCell`): usb_device, console, usb_tx, mpsl/sdc/nus_gatt/nus_tx, bus, sensor, tof, switch_edges + switches, leds. Go channels → `embassy_sync::Channel` (CMDS 16, LED triggers 8 with `try_send`); `Sniff`/`Occupied` stay atomics; `Device: Copy` in a blocking `Mutex<CS, Cell<Device>>` with a `Signal` telling the ToF task to stop/apply/start; outq per port behind a CS mutex + `Signal` to wake its sender (replaces 2 ms polling).
- **CLI**: `Cmd { name, help, run: fn(&mut Ctx, &[&str], &mut dyn fmt::Write) -> Action }`, `Action = None | Reboot(magic) | Nus(on/off) | Exit`; side effects via channels; hot paths (sniffer) use the ported `append*` formatters, no float `core::fmt`.
- **USB CDC**: embassy-usb `CdcAcmClass` with VID:PID **239A:802A** and TinyGo's strings (Go `bolt`, `soak.py`, macOS `usbmodem` glob keep working). 1200-baud touch = line coding 1200 + DTR falling → GPREGRET 0x57 + reset (TinyGo did this implicitly). `ota` → GPREGRET 0xA8 + reset (direct register write; SD never enabled). `version` prints `version <commit> built <date>` from `build.rs` env (`KNIXX_COMMIT`, `KNIXX_BUILD_DATE`, set by xtask from git).
- **KNX**: BufferedUarte UARTE0, TX P0.25 / RX P0.24, 38400 8N1 (as today). `knixx-bus` is sans-IO (`Engine::feed/command/poll`); the runner `select`s on UART read, CMDS and the engine deadline (no 10 ms poll), one group write at a time awaiting L_Data.con ≤ 500 ms while still feeding the sniffer. The same runner runs on the host in knixx-shell (tokio + embedded-io adapters over serialport; FTDI VCP instead of libftdi, no C dependency).
- **Storage**: embassy-nrf QSPI in single-line mode (SCK P0.19, CSN P0.20, IO0 P0.17, IO1 P0.22, IO2 P0.23, IO3 P0.21; never set the non-volatile QE bit), littlefs2 `Storage` impl on the blocking QSPI calls. **Exactly the Go geometry**: read = prog = 256, block 4096, count 512 (2 MiB), cache 512, lookahead 512, block_cycles 100, on-disk v2.1. **Never auto-format a flash that holds a littlefs superblock** (Go formats on any mount failure; in Rust a geometry mistake must not wipe a Go config): format only when blocks 0/1 lack the "littlefs" magic, else defaults + error LED. Blocking only at boot and on `config set/init`; worst-case stall measured. littlefs2-sys needs `arm-none-eabi-gcc` headers + libclang (documented, CI image).
- **WS2812**: own ~60-line driver on `SequencePwm` (16 MHz, 1.25 µs bits, static RAM sequence, P0.28) — DMA, no bit-banging (incompatible with MPSL).
- **I2C**: TWIM1 (SDA P0.12, SCL P0.11, 400 kHz) in an embassy-sync `Mutex`, one `shared_bus::asynch::i2c::I2cDevice` per driver (lock per transaction, as `i2cbus.Shared` does now). VL53L7CX keeps Go's upload chunk size through a static RAM tx buffer (EasyDMA can't read flash); pins LPn P0.13, I2C_RST P0.15, INT P0.14.
- **Image tooling**: `bolt convert` reads ELF PT_LOAD segments (`object`) → bin/hex/UF2 (family 0xADA52840); genpkg/ota/uf2/term ported from `src/host/bolt` + `src/host/internal/{legacydfu,usbserial,uf2,blescan}` with their fake-based tests.

## Phases (each verifiable on its own; Go `bolt` remains the fallback tool until Phase 9)
0. **Scaffold & boot** — repo, workspaces, deny, CI (host tests, firmware build, size gate), memory.x/set-vtor/flip-link, `bolt convert`, blinky on P1.15. *Exit:* UF2 drag-and-drop runs at 0x26000; double-tap still enters the bootloader; Go firmware reflashes fine.
1. **USB console core & flashing** — CDC 239A:802A, echo console, `version`, `sys` (uptime), `ota` (0xA8), 1200-baud touch (0x57), panic record; host `uf2`/`usbserial`/`mount`, `bolt uf2 -expect`, `cargo xtask flash`. *Exit:* Go→Rust→Rust flash with version match on macOS + Linux; `ota` enters AdaDFU and Go `bolt ota` reinstalls Go.
2. **MPSL/USB coexistence spike** (biggest risk first) — sanitising, MPSL+SDC+trouble advertising "knixx" next to USB CDC, priority table + assert. *Exit:* no hang and USB enumerates after UF2 flash, after OTA and from cold power-on; 100 automated 1200-touch reset cycles, zero failures.
3. **Pure-logic crates on the host** (can start alongside 0) — knixx-knx/ncn/bus/app/console/ui/sensors with every Go test table ported. *Exit:* `cargo test` green; `format(parse(default.ini))` byte-identical; sniffer lines equal Go's strings; every crate builds for thumbv7em without alloc.
4. **Console & UI on the board** — lineedit/history, full CLI on a RAM default config, outq drop notice, banner on first key, WS2812 Player, switches (GPIOTE + Hold), `sys` with all fields, `bolt term` over USB. *Exit:* manual script (both arrow encodings, Ctrl-P/N, redraw, LED patterns, switch edges); cable pull mid-output doesn't wedge the console.
5. **Storage** — QSPI single-line backend, littlefs2 with Go geometry, format guard, load/save `/knixx.ini`, fallback. *Exit:* host test mounts a Go-made 2 MiB image in Rust littlefs2 (RAM-backed) and vice versa (image generated once in the Go repo); on hardware `config set` survives Go→Rust and Rust→Go; worst-case `config set` stall measured.
6. **KNX bus** — BufferedUarte bus task, switch/stat/sniff/config via CMDS, occupancy state machine, contacts → group writes, knixx-shell over FTDI. *Exit (real TP line + second sniffer):* `switch A on` with positive L_Data.con; `stat` and sniffer output identical to Go for the same traffic; con timeout → error LED.
7. **Sensors** — shared TWIM1, SHT20 (CRC 0x31), VCNL4040 (ID 0x0186), first at 5 s then every 60 s, `sensor` command. *Exit:* DPT 9.001/9.007/9.004 telegrams with plausible values, decoded by the sniffer.
8. **ToF & occupancy** — VL53L7CX port with golden test (`testdata/configure.golden` + blobs copied from the Go repo; genfw/ULD harness stay there), INT-driven 50 ms watch, `tof` command, live settings, 60 s retry. *Exit:* golden test passes; motion → DPT 1.018 true + green LED, false after timeout; sensor unplugged → retry without blocking other tasks.
9. **BLE NUS console & host BLE tools** — NUS GATT (20 B UTF-8-safe notifications, 5 ms/1 s retry), `nus on/off`, 60 s idle return, 5 s switch gestures, blue LED, `ota` while connected; `bolt term` auto/NUS, `bolt genpkg` (golden), `bolt ota` (btleplug, PRN 10, 20 B fallback chunk since btleplug can't query MTU). *Exit:* full CLI over BLE from macOS + Linux incl. multibyte UTF-8; Rust `bolt ota` installs a Rust then a Go zip; genpkg byte-identical to adafruit-nrfutil and the Go golden.
10. **Hardening & acceptance** — stack high-water (painted RAM) in `sys`, size/RAM report + cargo-bloat in CI, `soak.py` copied (`sys` keeps `heap`/`goroutines` keys: `heap 0 of 0 bytes in use (no allocator)`, `goroutines <tasks>`), docs, release tag.

## Risks
| Risk | Mitigation |
|---|---|
| MPSL+USB hang after bootloader jump | Phase 2 spike first; sanitising + both CLOCK_POWER handlers; GPREGRET2 one-time reset fallback |
| embassy / nrf-sdc / trouble version skew | pin the set from nrf-sdc's manifest; upgrade as a set |
| littlefs mismatch wiping Go config | exact geometry, format guard, Go↔Rust image tests on the host before any hardware write |
| littlefs2 C/bindgen toolchain | documented arm-none-eabi-gcc + libclang, CI image, vendor bindings if needed |
| blocking QSPI stalls tasks | writes only on config changes; measured; biased select; InterruptExecutor for the bus only if needed |
| reserved MPSL resources / IRQ priorities | allocation table in `main.rs` + debug assert; BufferedUarte on TIMER1/PPI 0–1 |
| EasyDMA can't read flash | static RAM buffers for VL53L7CX chunks, WS2812 sequence, UART tx |
| btleplug (no MTU query, macOS write-without-response) | 20 B fallback + PRN 10 like Go; `--chunk`; Go `bolt` as fallback |
| strings parsed by tools drift (`version`, `sys`, sniffer) | golden string tests in Phase 3; Go `bolt` + `soak.py` run against Rust |
| licensing | knx-rs names only, no code; cargo-deny; ST blobs/ULD under the same terms as in the Go repo |
| image size | CI gate < 0xC7000 (est. 86 KiB ToF blob + ~150 KiB SDC/MPSL + ~40 KiB trouble + app ≪ 780 KiB) |

## Reference sources in the Go repo
`src/bus/bus.go`, `decode.go`, `format.go` · `src/ncn/service.go` · `src/tp1/tp1.go` · `src/config/config.go` + `default.ini` · `src/cli/cli.go` · `src/nrf/{console,nus,storage,tof,sensor,switches,ota,sys}.go` · `src/internal/vl53l7cx/` (+ `testdata/configure.golden`, `firmware/*.bin`) · `src/host/bolt/*.go`, `src/host/internal/{legacydfu,usbserial,uf2,blescan}` · `tools/soak.py`.

## Verification (end-to-end acceptance)
CI on every push: host `cargo test`, firmware release build, `cargo deny check`, size gate, thumbv7em no_std build of each `crates/*`.
1. `cargo xtask flash` (1200 touch → FTHR840BOOT → version match) from Go and from Rust, macOS + Linux.
2. `ota` + `cargo xtask ota` installs Rust zips and then a Go zip; MBR+S140 region 0x0–0x26000 unchanged (SWD read vs factory dump on one board).
3. `/knixx.ini` survives Go→Rust→Go with `config set` on each side; blank flash formats; corrupt file → defaults + red LED.
4. KNX: switch A/B from CLI and contacts (positive L_Data.con), sensor telegrams every 60 s, occupancy true/false, sniffer output line-identical to Go — checked with a second sniffer (knixx-shell over FTDI).
5. BLE console: `nus on`/gesture, `bolt term` over NUS, 60 s idle return, `nus off`/gesture, LEDs.
6. 24 h `soak.py` past 2h27m and past RTC1 wraps: monotonic uptime, no unexplained lost bytes, empty panic record, stable stack high-water, bus traffic flowing.
7. Image ≤ 0xC7000 and RAM reported; no `__rust_alloc` symbol in the ELF.
8. Every Go test suite ported and passing (default.ini round-trip, VL53L7CX golden, genpkg golden).
