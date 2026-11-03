# Our own USB identity: pid.codes 0x1209:0xA7EA

> **Status: completed.** Implemented in commit `275e351` (2026-11-03, "our
> own USB ID: pid.codes 1209:A7EA"). On the board: flashing from the old ID,
> both IDs found by bolt and xtask, and a round trip through v0.4.1. macOS
> names the port by USB location, not by the serial number; Linux uses the
> serial in `/dev/serial/by-id`.

## Context

The app still presents itself over USB as an Adafruit board. This was kept from
the TinyGo firmware so that the Rust firmware could replace it without changes on
the host side:

| field | value now |
|---|---|
| VID:PID | Adafruit's 0x239A:0x802A |
| manufacturer | "Adafruit Industries LLC" |
| product | "Feather nRF52840 Express" |
| serial number | none |

The values are set in `firmware/knixx-fw/src/usbcdc.rs:75-78`.

The user has a reserved pid.codes ID, **0x1209:0xA7EA**, and wants the app to
use it now, independently of the parked bootloader plan. The Adafruit UF2
bootloader keeps its own 0x239A IDs and its `FTHR840BOOT` drive.

## Changes

1. **Firmware, `firmware/knixx-fw/src/usbcdc.rs`:**
   - `Config::new(0x1209, 0xA7EA)`
   - manufacturer "flukso", product "knixx thunder"
   - a serial number: the 64-bit FICR DEVICEID as 16 hex digits, formatted
     once into a `StaticCell<[u8; 16]>`, the same ID `ble.rs` uses for the BLE
     address. With it, macOS names the port after the board
     (`/dev/cu.usbmodem<serial>…`), which stays the same across resets and
     USB ports.
   - update the module comment, which says "with the USB IDs … of the TinyGo
     firmware". The 1200-baud touch and everything else stay as they are.
2. **Host, `host/knixx-host/src/usbserial.rs`:**
   - replace `APP_VID`/`APP_PID` with `APP_IDS: [(u16, u16); 2] =
     [(0x1209, 0xA7EA), (0x239A, 0x802A)]`
   - `ports_of(ids)`, or have `ports` take the list, so that both macOS
     (`serialport`) and Linux (`sysfs_ports`) match any of the IDs
   - the old ID stays, so that boards on v0.4.x or the Go firmware are still
     found, flashed and opened; drop it in a later release
   - the `sysfs_by_usb_id` test covers both IDs and still rejects a foreign one
3. **Callers:** `host/bolt/src/client.rs:68`, `host/bolt/src/term.rs:91,119`
   and `host/knixx-host/src/flash.rs:99,153` (`app_port` and the wait for the
   board after a flash) switch to the ID list. Flashing old → new firmware
   then works, since the board comes back with the new ID.
4. **README:**
   - line 237: "It finds the board by its USB ID, 1209:A7EA (pid.codes), or
     239A:802A from earlier firmware"
   - the Linux requirements note that ModemManager can probe a new CDC device.
     A udev rule keeps it away:
     `ATTRS{idVendor}=="1209", ATTRS{idProduct}=="a7ea",
     ENV{ID_MM_DEVICE_IGNORE}="1"`
   - the release notes of the next tag say the USB ID changed
   - save this plan as `plan/usb-identity.md`, marked completed with the
     commit that implements it, like the other plans in `plan/`. It goes in a
     commit of its own after the implementation commit, so that it can name
     that commit.

## Verification

- `cargo test` (the usbserial test with both IDs) and clippy across all three
  workspaces.
- On the USB-attached board:
  1. Run `cargo xtask flash` from the running 0x239A:0x802A firmware. It must
     find the board by the old ID, then find the new firmware by the new one.
  2. `system_profiler SPUSBDataType` shows 1209:a7ea, "flukso", "knixx
     thunder" and the serial number.
  3. `bolt term --usb`, `bolt script ls` and the 1200-baud touch into the
     bootloader all work.
  4. Flash an older release (`knixx.uf2` of v0.4.1) and back: the tools find
     it under the old ID.
