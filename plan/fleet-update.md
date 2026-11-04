# Fleet firmware updates: own bootloader, laptop to many thunders over BLE, KNX as control plane

> **Status: implemented, the bus side still to test.** The implementation
> starts with commit `eead8c8` (2026-11-04, knixx-image), through `f753c08`
> (bolt fleet update). Tested on an isolated Feather nRF52840 with a Black
> Magic Probe:
> - the bootloader: boot, ACL, recovery, the double tap, an install, and a
>   power cut mid-copy, which the next boot repeats;
> - staging over USB and NUS;
> - the fw service, unpaired, with resume;
> - the control plane and `bolt fleet update` by loopback, the one board
>   being proxy and fleet.
>
> Waiting for a KNX line with more boards: the U_SetAddress.req format
> `[0xF1, hi, lo, 0x00]` and the L2 acks it brings, the broadcast scan
> between boards, and parallel pushes.
>
> Beyond or apart from the plan:
> - **The bootloader takes 64 KiB, not 60** (it came out at 58 KiB): the
>   header is at 0x10000 and the app at 0x10100.
> - **flash-probe erases the pages itself through the NVMC** and reads both
>   images back: the probe does not always erase before it writes.
> - **The console upload is framed** ([offset][CRC][data], resend), since
>   USB through a hub lost or damaged a packet now and then.
> - **The NUS ring is 4 KiB.**
> - **ed25519 is built for size** (opt_size, opt-level z).
> - **The board advertises its BLE address as manufacturer data** (0xFFFF),
>   since macOS shows no addresses.
> - **The individual address's key is `device.individual`.**


## Context

Today a thunder is updated one board at a time:

- `ota` on its console starts the Adafruit bootloader's legacy BLE DFU. It
  takes about 2 min for 744 KB, during which the board does nothing else.
- Or USB with `bolt uf2`.

With an installation of 20–50 thunders, that becomes a chore. The board's
observed BLE range is good, so a laptop in one spot reaches most or all
boards.

The proposal (the user's direction):

- The laptop **pairs with one thunder, the proxy**, which gives it the **KNX
  bus as a control plane**: discovery, opening an update window on every
  board, the install command, and the version reports.
- The laptop then **pushes the image over BLE to many boards in parallel**.

For this, each board must take the image while its app runs, into its 2 MiB
QSPI flash, and install it later. The Adafruit bootloader can't do that, and
the firmware has only about 71 KB of flash left (thunder takes 744 KB of
796 KiB). Hence the first part, our own bootloader.

Decisions (user):

- **Bootloader:** our own Rust bootloader at 0x0. No SoftDevice, no MBR,
  **one** app slot, **no rollback**.
- **USB:** pid.codes 0x1209:0xA7EA for app and bootloader. The Feather's
  239A:802A is dropped.
- **Migration:** only development boards, each over SWD with the probe. No
  UF2 migrator, and no UF2 anywhere after it.
- **Images:** only ed25519-signed images install, and no downgrades.
- **Transport:** host to many over BLE, KNX as control plane, through one
  paired proxy. The laptop reaches the proxy through the **knixx GATT
  service**, not the console.
- **KNX control messages:**
  - They use the KNX application services `A_UserManufacturerInfo_Read`
    (APCI 0x2C5) and `A_UserManufacturerInfo_Response` (0x2C6).
  - A scan goes to the broadcast address 0/0/0.
  - After the scan, bolt talks 1-1 to each thunder by its individual
    address.

## Part 1: our own bootloader

### Why

Most of the 1 MB of flash goes unused:

- **0x0–0x26000 (152 KB):** the MBR and the S140 6.1.1 SoftDevice. The app
  never enables the SoftDevice: it links its own BLE stack (nrf-sdc, MPSL,
  trouble). Only the Adafruit bootloader uses the S140, for its BLE DFU.
- **0xED000–0xF4000 (28 KB):** user data that nothing uses.

The Adafruit bootloader sits at 0xF4000.

Two alternatives were ruled out:

- **Sharing the S140 through nrf-softdevice:** the crate is barely
  maintained, supports S140 7.x only, and lacks LESC. It would mean a BLE
  rewrite to save about 70 KB.
- **Sharing nrf-sdc between two binaries:** impossible, since it is linked
  statically.

With our own bootloader, the app grows from 796 KiB to **964 KiB**, freeing
about 168 KiB. The reference project is dcmini-org/dcmini-fw: an
embassy-boot-nrf bootloader at 0x0, a trouble + nrf-sdc app, and DFU into
external QSPI.

### 1.1 Layout

| region | range | |
|---|---|---|
| bootloader | 0x00000–0x0F000 (60 KiB) | vector table at 0x0; estimated at 40–45 KB (embassy-nrf, embassy-usb CDC, QSPI, littlefs read, NVMC, CRC, ed25519 verify) |
| app header | 0x0F000–0x0F100 | 256 B, written last at install |
| app | 0x0F100–0x100000 | vector table at 0x0F100 (64 vectors, 256 B alignment); 964 KiB |

- **No settings page.** The app header is the commit that marks the app
  valid. The staged file is the record that an install is pending.
- **UICR stays untouched.** BOOTLOADERADDR is read only by the MBR, which is
  gone. Erasing UICR would reset REGOUT0, NFCPINS and APPROTECT.
- **The header** is `#[repr(C)]`, in a new no_std + std crate
  `crates/knixx-image` that the bootloader, the firmware and the host share.
  - Fields:
    - magic `KNXX`, header version, flags;
    - load_addr, len, body CRC-32;
    - the **version counter** (u32 build number);
    - the version string (32 B: commit, `-dirty`);
    - the build time;
    - the **ed25519 signature** (64 B) over the header (with the signature
      zeroed) and the body;
    - a header CRC-32.
  - The CRC-32 IEEE moves from `knixx_console::upload::crc32` into
    knixx-image, with a table version for the bootloader.
  - The boot decision, from the header, staged file and GPREGRET, is
    written as pure functions in knixx-image and unit-tested.
- **RAM:**
  - The app's RAM is unchanged: the panic record at 0x20000008, and RAM
    from 0x20000108. The assert in `host/knixx-host/src/image.rs` stays.
  - 0x20000000–0x20000008 was the MBR's. It becomes the handoff between
    bootloader and app: the double-tap magic and one spare word.
  - The bootloader's RAM is 0x2003C000–0x20040000 (16 KiB, 32 if needed).
    It never touches lower RAM, so the panic record survives. A host-side
    check enforces this.

### 1.2 Signed images and the version counter

- **The image file:** `.kfw` is the header plus the body.
- **Signing:** `xtask build` signs `thunder.kfw` with a development key from
  `$KNIXX_SIGNING_KEY`, or else a key in a gitignored file. The release
  workflow signs with the release key, from a GitHub secret.
- **Verification:** the public key (or keys: development and release) is
  compiled into the bootloader and the app. Use a no_std ed25519 verifier
  such as `ed25519-compact` or `salty`, at about 10–15 KB in each binary.
- **The app checks** the signature and the counter before it renames a
  staged file. **The bootloader checks** them again before it erases
  anything. A counter not above the running one is refused.
- **Development:** `xtask flash` goes through USB recovery, where
  `--force` allows the same or an older counter. Over BLE that is never
  allowed.

### 1.3 Staging: a littlefs file

The staged image is the file `/fw/update.kfw`. A raw partition of the QSPI
flash would mean shrinking littlefs, a risky format-and-restore migration
of the config, scripts and bonds.

- **App side:** chunks stream into `/fw/update.tmp`. At the end, the app
  checks the CRC, signature and counter by reading the file back, then
  renames it.
  - `knixx-storage` gets a streaming API: `File::create/write/close`, and
    `File::open/read` for the bootloader. Both are unit-tested in
    `crates/knixx-storage/src/tests.rs` on the RAM device.
- **Bootloader on GPREGRET `INSTALL` (0xB1), or when the app is invalid:**
  1. Mount littlefs read-only, with the same configuration as the app,
     shared from knixx-storage.
  2. Check the staged header, the signature and the counter, and do a full
     CRC pass over the body from QSPI. No internal page is erased before
     this passes.
  3. If the staged image equals the installed one, skip.
  4. Otherwise:
     - erase the header page, then erase and write the body;
     - check the CRC again in internal flash;
     - write the header last;
     - put the result in GPREGRET2, and reset.
- **Power loss mid-copy:** the header is gone, so the app is invalid. The
  next boot finds the file and copies again, since the copy is idempotent.
- **A bad staged image and an invalid app:** USB recovery. Without
  rollback, a bad image is recovered over USB.
- **Cleanup:** the app removes `/fw/update.kfw` once its own header matches
  the file's. The bootloader never writes littlefs.
- **QSPI driver:** `QspiFlash`, the 8 MHz single-line driver with its bounce
  buffer, moves from `firmware/knixx-fw/src/storage.rs` into a shared
  firmware crate, `firmware/knixx-qspi`.

### 1.4 The bootloader's USB recovery mode

- **Entered on:**
  - GPREGRET 0x57, from the app's 1200-baud touch (`usbcdc.rs`);
  - a double tap of reset: only on RESETREAS.RESETPIN, with the magic at
    0x20000000 and a 500 ms DWT busy-wait;
  - an invalid app with no valid staged file.
- **USB:** embassy-usb CDC-ACM, 0x1209:0xA7EA, product "knixx bootloader".
  The serial number comes from FICR DEVICEID. The app has the same serial
  and the product "knixx thunder", so the host follows one board across
  resets and tells the two apart by product string.
- **Protocol:** the console's upload framing. `Upload` gains a chunk-size
  parameter, and `ready <chunk>` falls back to 128.
  - Commands: `version`, `info`, `fw put <size> <crc32>`, `boot`, `reset`.
    `fw put` answers `ready 4096`, then `ok <offset>` per page, then `done`.
  - The bootloader erases the header page first and writes page by page.
    After `done` it checks the signature and a CRC read back, then writes
    the header.
  - The per-page ack and USB hardware NAKs cover the NVMC stalls. The worst
    case is about 20 s for an image.
- **Write protection:**
  - Before it jumps to the app, the bootloader write- and erase-protects
    0x0–0xF000 with an ACL region: `ACL[0]` with ADDR 0x0, SIZE 0xF000 and
    PERM write and erase disabled.
  - The ACL holds until the next reset. So the app, a stray NVMC write or
    a bad image can never damage the bootloader.
  - Only the probe can then rewrite it (`flash-probe`), which matches how
    boards are migrated.
  - Test: a deliberate NVMC write into 0x1000 from the app faults (BPROT
    and ACL report it).

### 1.5 Updates through the app, over the console

A new `firmware/knixx-fw/src/fw.rs`:

- **`fw put <size> <crc32>`:** streams through a 4 KiB chunk buffer into
  `/fw/update.tmp`, with no RAM image, then checks the file and renames it.
  - Over NUS it needs a paired central (`ble.pair`).
  - The RX ring (`knixx-console/src/nus.rs`, 256 B) grows, or the console
    drains it into the chunk buffer, so that 2 KiB chunks fit. The aim is
    20–40 KB/s.
- **`fw install`:** `reset_into(INSTALL)`.
- **`fw status`:** the running header, the staged file and the last result.
- **`boot.rs`:** GPREGRET becomes `RECOVERY` 0x57 and `INSTALL` 0xB1.
  - `OTA` 0xA8 and the `ota` command go.
  - Keep `sanitize()` for one release, then drop it: our bootloader only
    jumps from a pristine state.
- **`memory.x`:** `FLASH : ORIGIN = 0x0F100, LENGTH = 0x100000 - 0x0F100`.

### 1.6 Migrating the development boards

Every board to migrate is a development board with an SWD probe.

- `cargo xtask flash-probe` erases the chip, then writes the bootloader at
  0x0 and the app with its header.
  - The probe is a GDB-server probe on a serial port (`--probe`, default
    `/dev/cu.usbmodemB5C8A6F11`), driven with `arm-none-eabi-gdb`:
    `target extended-remote <port>`, `monitor swdp_scan`, `attach 1`,
    `monitor erase_mass`, then `load` of `thunder-boot.elf` and of the
    app's ELF with its header.
  - The ELF files and the `.kfw` header come from `xtask build`.
- The erase takes the Adafruit bootloader, the MBR and the S140 with it,
  but leaves UICR's REGOUT0 and NFCPINS as they were.
- The config, scripts and bonds on the QSPI flash are not touched.
- From then on the board is updated over USB recovery, the console or the
  fw service. The probe is needed again only for the bootloader.

### 1.7 Host tooling

- **`knixx-host`:**
  - `image.rs` gains `to_kfw()`, with signing.
  - `flash.rs` becomes `recover()`: the 1200-baud touch, a wait for the
    "knixx bootloader" product, then `fw put` over CDC.
  - New `fw::stage()`: the app path, over USB or BLE, through `client.rs`.
- **Removed, along with the UF2 path:**
  - `knixx-host`: `uf2.rs`, `drive.rs` (the FTHR840BOOT drive, the udisks
    mount) and `legacydfu/`;
  - `bolt`: `uf2`, `ota`, `genpkg`.
- **USB IDs:** `usbserial.rs` matches 0x1209:0xA7EA only, and tells the app
  from the bootloader by product string. The Feather's 239A:802A goes, from
  `APP_IDS` and from the README's udev rule.
- **`bolt`:**
  - `flash <kfw>`;
  - `fw put|install|status [--ble]`;
  - `convert`, which emits `.kfw` from an ELF or hex.
- **`xtask`:**
  - `APP_START` and `APP_END` become 0x0F100 and 0x100000.
  - `build` writes:
    - `thunder.{elf,kfw,hex}`;
    - `thunder-boot.{elf,hex}`, with a size check of at most 0xF000.

    The `ARTIFACT` constant stays.
  - `flash` goes through recovery, so it also works with a broken app.
  - `ota` is replaced by `fw --ble`, and `cycle` uses `flash`.
  - New `flash-probe`: the chip erase, then the bootloader and the app over
    SWD.
- **Release:**
  - attached: `thunder.kfw` (signed), `thunder.elf`, `thunder-boot.hex`,
    `thunder-boot.elf`, and bolt;
  - dropped: `thunder.uf2`, the app's `.hex` and `thunder-dfu.zip`.
- **CI:** builds and lints the bootloader too, and checks its size.
- **README:** rewrite "Flashing" and "Flash layout":
  - migrating a board with the probe;
  - then `bolt flash` over USB and `bolt fw` over the console;
  - and the fleet update (Part 2).

## Part 2: fleet updates

### 2.1 The fw GATT service on every board (firmware/knixx-fw/src/fw_gatt.rs)

The service can only stage a signed image: no console, no KNX.

- **`ctrl` (write, notify):**
  - writes:
    - `begin <size> <header 256 B>`: the header comes first, so the board
      refuses a wrong version before any data;
    - `resume`;
    - `abort`;
  - notifications:
    - acks with the highest contiguous offset written, every 4 KiB;
    - the final `staged ok`, or the reason it failed: `bad signature`,
      `downgrade`, `crc`, `no space`.
- **`data` (write without response):**
  - each chunk is `offset u32` and data, up to MTU−7 bytes, into
    `/fw/update.tmp` through the 4 KiB buffer;
  - chunks that are out of order or duplicate are dropped, and the host
    resends from the last ack.
- **Resume:** the partial file stays, with its header. After a lost link,
  `resume` reports how far it got.
- **Open only in an update window (2.2):**
  - Outside the window, the service refuses everything.
  - During it, the board advertises the service's UUID, connectable.
  - It is **not behind `ble.pair`**: the signature protects the update, not
    the link, and pairing every board to the laptop is the chore this
    avoids.
  - The window does not open NUS.
- **One central at a time, as now.** A board being updated is not available
  to the web app for a minute or two.

### 2.2 The KNX control plane: user manufacturer info, 1-1

The control plane uses the two application services that KNX reserves
for manufacturer-specific use:

- **Requests:** `A_UserManufacturerInfo_Read`, APCI 0x2C5.
- **Answers:** `A_UserManufacturerInfo_Response`, APCI 0x2C6, addressed
  back to the requester's individual address.

**Addressing.**

- **Scan:** a broadcast, to group address 0/0/0, which every line coupler
  passes. Every thunder answers 1-1 to the proxy, from its own individual
  address.
- **Everything after the scan:** 1-1, as `T_Data_Individual`
  (connectionless) to the board's individual address. Couplers route these
  by address, so no filter tables are needed.
- **Unique individual addresses:** each board needs one, set with
  `config set device.individual`.
  - The scan answer carries the board's BLE address, which is unique (from
    FICR).
  - So two boards on one individual address show up, and bolt names them.

**Message format.**

- A 10-bit APCI leaves 14 bytes of a standard frame.
- Byte 0: the protocol tag `0x6B` ('k'). Answers from other vendors' devices
  carry their manufacturer code there and are ignored.
- Byte 1: the op.
- Up to 12 bytes of body.
- The codec lives in `crates/knixx-knx/src/fw.rs`, host-tested. knx-rs
  (metaneutrons/knx-rs, `knx-rs-core/src/apdu.rs` and `tpdu.rs`) serves as
  a reference for the APCI and TPDU encodings. It is GPL-3.0, so we take
  ideas from it, not code or a dependency.

| request (0x2C5) | answer (0x2C6) |
|---|---|
| `scan`, to 0/0/0 | `info`: running counter u32, state u8 (idle, receiving, staged, failed), BLE address 6 B. Sent after a random 0–2 s delay, so that 50 answers spread out. |
| `status` | `status`: running and staged counters, state, last error, staged progress in % |
| `open <counter u32> <hash 4 B> <minutes u8>` | `ready <ble addr>`, `current`, or `refused <reason>` (e.g. a downgrade) |
| `install <counter u32>` | `installing`, then a reboot into the bootloader, if that counter is staged and verified; else `refused` |
| `close` | `closed` |

- **No unsolicited messages:** after the install, bolt asks each board for
  `status`.
- **Bus load:** a handful of telegrams per board for the whole update.
- **Security:** anyone on the bus can open a window. That is harmless,
  since only signed, newer images install. Two limits keep it from being a
  nuisance:
  - **A size cap:** an image is at most 1 MiB (`MAX_IMAGE`, in
    knixx-image), which the 964 KiB app region fits. The fw service
    refuses a `begin` with a larger size, or a header whose `len` is
    larger. It also stops writing at the size it was given, so a
    misbehaving sender can't fill the littlefs and swamp the config,
    scripts and bonds. `fw put` and USB recovery apply the same cap.
  - **A rate limit** on `open`.

**Gaps in our KNX stack, found while checking:**

- **APCI decoding:** `knixx-knx` decodes only the 4-bit APCIs
  (`tp1::Apci`, where 0x2C0–0x2FF is just `UserMessage`). It gets the 10-bit
  APCIs 0x2C5 and 0x2C6, and the individual/broadcast distinction for
  received telegrams.
- **Sending:** `knixx-bus` lets only group value services out (`refuse()`).
  A `Command::Send` with `Tag::Fw` may send 0x2C5 and 0x2C6 telegrams 1-1
  or to 0/0/0. Scripts stay limited as now.
- **L2 acks:** the transceiver is never given the board's individual
  address. Without NCN5121 `U_SetAddress.req` (0xF1, address high, address
  low, 0x00; knx-rs-tp's `commands.rs` names it too), it doesn't L2-ack
  telegrams addressed to the board. The sender then repeats them 3 times
  and gets a negative confirmation.
  - Send it at bus start and on `config set device.individual`.
  - Test that a 1-1 telegram to the board gets a positive confirmation at
    the sender.

### 2.3 The proxy: the knixx GATT service

The laptop talks to the proxy through the knixx GATT service, already
paired and encrypted (`ble.pair`), not through the console. Console
commands for this would be of no use on their own.

- **A new characteristic `knx` (write, notify):**
  - **write:** `seq u8, dest u16, flags u8 (individual/broadcast),
    payload ≤ 14 B`. It sends one 0x2C5 telegram with that payload. The
    `result` notification (seq, status) reports its confirmation, as for
    `send`.
  - **notify:** `src u16, payload ≤ 14 B`, for each 0x2C6 telegram
    addressed to the proxy.
  - Both fit the service's 20-byte values.
- **The proxy as a board too.** It answers requests addressed to itself
  locally, as if they had come over the bus, so bolt treats it like any
  other board. Its own image goes in over its fw service. It installs last.
- **Host:** knixx-host gets a client for the knixx service, next to its NUS
  client, so that bolt can use it.

### 2.4 `bolt fleet` (host/bolt)

`bolt fleet update thunder.kfw [--via <proxy>] [--parallel 4] [--stagger 2s]`
runs these steps:

1. **Scan:** connect to the proxy's knixx service and send `scan` to 0/0/0.
   The `info` answers that arrive within 3 s give the boards, each with its
   individual address, BLE address and running counter.
2. **Open:** `open` 1-1 to each board that needs the image. Those that
   answer `ready` go into the queue.
3. **Push:** up to `--parallel` boards at a time, over btleplug connections
   by BLE address.
   - It uses write without response, with the largest MTU.
   - After a drop, it resumes from the last ack.
   - After two retries, the board is `unreachable`.
4. **Show:** a live table of board, address, state and progress, then a
   summary.
5. **Install:** `install` 1-1 to each staged board, `--stagger` apart, so
   that lights and switches don't all reboot at once. The proxy goes last.
6. **Check:** after its reboot, reconnect and send `status` to every board,
   then list those still behind.
   - Running it again skips the boards that are current or already staged.
   - `bolt fleet status` does the scan alone.

**Throughput (to be measured).** One link from macOS or BlueZ, with write
without response on 2M, is about 30–60 KB/s. Parallel links share the
laptop's radio, so the total stays around 50–100 KB/s. The board's QSPI
writes (about 70 KB/s) are not the limit.

| boards | time for the whole update |
|---|---|
| 20 | about 3–5 min |
| 50 | about 7–12 min |

The run is unattended either way.

**Out of the laptop's range:** a board shows as `unreachable`. Move the
laptop and run the update again; it does only what is left.

### 2.5 Flash and RAM

| part | flash |
|---|---|
| fw service, KNX fw codec and state machine, the `knx` characteristic | about 10–15 KB |
| ed25519 | about 10–15 KB |

There is no central role in the app. The total fits easily in the 964 KiB
app region. RAM grows by the 4 KiB stage buffer.

## Phases (each a commit series, tested on the board)

1. **Bootloader and USB recovery:**
   - new: `crates/knixx-image` (header with `KNXX`, CRC, signature, boot
     decision), `firmware/knixx-boot` (with the ACL write protection),
     `firmware/knixx-qspi`;
   - changed: the app at 0x0F100, `boot.rs`, `Upload` with a chunk size,
     the host's `flash`, and `xtask flash-probe`;
   - removed: the UF2 and legacy DFU paths, and 239A:802A;
   - signing in `xtask build` and the release;
   - bring-up over SWD with the probe and `arm-none-eabi-gdb`
     (breakpoints, memory reads of GPREGRET, ACL and the header), then over
     the bootloader's USB console;
   - then the development boards are migrated with `flash-probe`.
2. **Staging in the app:**
   - the streaming littlefs API;
   - `fw put|install|status` and the NUS chunking;
   - installing from littlefs in the bootloader;
   - `bolt fw`.
3. **The fw GATT service and `bolt fw push --addr`**: one board, unpaired,
   in a window opened from its console (`fw window 10`). Measure the
   throughput from macOS and Linux.
4. **The KNX control plane.** There is one board for now, so phase 4 is
   tested by proxy loopback only: requests addressed to the proxy itself,
   and a broadcast `scan` that the proxy answers locally. The bus side
   (acks, other boards) waits for a second board.
   - `U_SetAddress.req` at bus start, and a check that 1-1 telegrams are
     acked;
   - the 10-bit APCIs 0x2C5 and 0x2C6, and individual/broadcast in
     knixx-knx;
   - `Tag::Fw` sends in knixx-bus;
   - the fw codec, with host tests;
   - the scan, status, open, install and close handlers;
   - the knixx service's `knx` characteristic, and the knixx-service client
     in knixx-host.
5. **`bolt fleet update|status`**, and the README section "Updating an
   installation".

## Risks

- **The bootloader can't update itself,** and the ACL keeps it that way. A
  fix needs the probe. Keep it small, and test its decisions as pure
  functions.
- **Broadcast 0x2C5:** `A_UserManufacturerInfo_Read` is normally sent 1-1.
  Other vendors' devices may ignore it on 0/0/0, or answer with their
  manufacturer data. The protocol tag filters those out. Check that a mixed
  installation is not disturbed.
- **littlefs read-only in the bootloader:** its configuration must equal
  the app's (block count, sizes, multiversion).
- **NVMC stalls during USB:** test on macOS too.
- **Many links from one laptop:** CoreBluetooth and BlueZ limit
  simultaneous connections, and the total throughput may be lower than
  estimated. Phase 3 measures it before the fleet work.
- **Losing the signing key** means boards accept no new release except
  through USB recovery with a new key. Keep a backup, and compile a second
  (offline) key into the bootloader.

## Verification

- **Host tests:**
  - knixx-image: header, CRC, signature, counter and the boot decision;
  - the knixx-storage streaming API;
  - `Upload` with larger chunks;
  - the KNX fw codec;
  - the fw service's chunk state machine (gaps, duplicates, resume);
  - bolt's scheduler, with simulated boards that drop, fail or are out of
    range.
- **On the USB board, through the probe first:**
  - The bootloader boots the app at 0x0F100. BLE, USB, the scripts and the
    panic record still work: trigger a panic and see it after the reset.
  - An NVMC write into the bootloader from the app faults (ACL).
  - The config, scripts and bonds on QSPI survive `flash-probe`.
  - A double tap or the 1200-baud touch enters recovery, and `bolt flash`
    restores the app.
  - A corrupted header lands in recovery.
- **Staging:**
  - Stage over USB, over NUS and over the fw service, timing each, then
    `fw install`.
  - Cut the power during the install: the copy restarts and the app runs.
  - A corrupted or unsigned staged file, or a downgrade: nothing is erased.
  - A `begin` above 1 MiB is refused, and a sender that writes past its
    declared size is cut off. littlefs keeps its free space.
  - The config, scripts and bonds survive.
- **KNX and fleet, with the one board we have (proxy loopback):**
  - `bolt fleet status` through the knixx service: the broadcast `scan` is
    answered locally by the proxy, with its `info`.
  - `bolt fleet update` on that one board:
    - `open`, a push over its fw service, `install`, and `status` with the
      new counter afterwards;
    - a link cut during the push shows `unreachable`, and a second run
      finishes the update.
  - `bus sniff` shows the broadcast 0x2C5 going out on the bus, well
    formed.
  - The scheduler's parallel pushes and stagger are covered by the host
    tests with simulated boards.
- **Waiting for a second board:**
  - a 1-1 0x2C5 between two boards gets a positive confirmation, with no
    repeats;
  - one `info` per board;
  - parallel pushes, and the staggered install across boards;
  - the bus load stays at a handful of telegrams per board.
- **Size:** `cargo xtask build` reports the app against 964 KiB and the
  bootloader against 60 KiB.
