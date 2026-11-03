# KNX group control from a browser over Web Bluetooth

> **Status: completed.** The implementation starts with commit `1762296`
> (2026-11-03, "BLE pairing: bonds, ble.pair and Service Changed"), followed
> by `7be762b`, `3307108` and `827abc6`, and the README in `9c7c7f8`.
> Released in v0.4.0. Additions beyond this plan: the RSSI characteristic and
> the signal indicator, the Service Changed indication, and in v0.4.1 an
> installable app (manifest, icon, service worker) with a theme button.

## Context

Today the board's only BLE link is the Nordic UART Service (NUS) text console:
it is off at boot, comes up for `nus on` (USB console or wall-switch
gesture), has **no pairing** (README:352: anyone in range can use the
console while it is on), and offers no generic group write/read. Only
scripts can send arbitrary group telegrams (`Command::Send`), and the
L_Data.con result never reaches the sender.

Goal:
1. **Pairing for BLE, in every build.** A central pairs once, while the user
   has physical access to the board, and that bond opens every service: NUS
   and the new knixx service.
2. **The knixx GATT service** (behind the `web-ble` feature): group
   write/read and the telegrams that come back, for a web app.
3. **A web app** (Leptos, wasm, Chrome/Edge Web Bluetooth) on a phone or
   laptop that sends GroupValueWrite/Read to any group address and shows the
   result and the GroupValueResponse/Write telegrams.

## Design

### 0. What the `web-ble` feature switches

- **Always built:** pairing and bonding (trouble-host `security` and
  `security-p256-cortex-m4` become plain features in
  `firmware/knixx-fw/Cargo.toml:33`), bond storage, the `[ble] pair` check
  on NUS, the `ble bonds|forget` commands. The default firmware grows by the pairing code:
  measure it (Risks).
- **Behind `web-ble`** (Cargo feature in `firmware/knixx-fw/Cargo.toml`,
  modelled on `scripts`, **off by default**): `knixx_gatt.rs`, the
  `KnixxService`, its advertising (the knixx UUID in `adv_data`, NUS moved to
  `scan_data`), and continuous advertising for `[ble] knixx`.
  `#[gatt_server]` may not accept a cfg'd field, so use two `Server`
  definitions, one per cfg, if needed.
- Shared crates carry no cfg. The codec (`knixx-knx::gatt`) and the engine's
  `Tag`/`sent()` (knixx-bus) are always built and tested on the host; without
  the feature they cost no flash, since nothing calls them.
- `[ble] knixx` is always parsed in knixx-device, so one `knixx.ini` works
  with either build. Without the feature, `config set ble knixx on` replies
  `web-ble is not built in`.
- xtask: `cargo xtask build --web-ble` (and on `flash`/`ota`), next to
  `--no-scripts`; the flags combine.
- CI (`ci.yml`): also `cargo xtask build --web-ble` and
  `cargo clippy --release --features web-ble` in firmware/.
- Release (`release.yml`): also attach `knixx-web-ble.uf2` and
  `knixx-web-ble-dfu.zip`, built with `--web-ble`.

### 1. Pairing and bonding for every service (firmware/knixx-fw/src/ble.rs)

- IoCapabilities `NoInputNoOutput` (LESC Just Works); seed from the existing
  `Rng` (`ble.rs:87`).
- **`[ble] pair = on|off`, default `on`** (crates/knixx-device: parser,
  `default.ini`, README table; a missing key means `on`). It decides whether
  NUS and the knixx service require pairing. `config set ble pair off` turns
  it off live and saves it, like every other key. With it off, BLE works as
  today: anyone in range can connect while the board advertises.
- **Checked at runtime, not in the GATT table.** trouble's `encrypted`
  attribute flag is fixed at compile time, so the characteristics are
  declared without it. Instead, in `ble::events` (`ble.rs:239`), while `pair`
  is on, each read, write and CCCD write to a NUS or knixx handle goes through
  `conn.security_level()`. Unless the link is encrypted, it is rejected with
  `AttErrorCode::INSUFFICIENT_ENCRYPTION` (`GattEvent::reject`), before the
  data reaches `nus::RX`. That is the error on which macOS, Android, Windows
  and Chrome start pairing, or encryption with a stored bond. On connect,
  the board also calls `conn.request_security()` while `pair` is on, so the
  OS's prompt appears at once. Notifications are only sent on an encrypted
  link.
- **Switching it on live:** a connected central whose link isn't encrypted is
  disconnected. Switching it off keeps the bonds, so turning it on again needs
  no new pairing.
- **One bond, both services:** NUS and the knixx service use the same
  check. On one computer, bolt and Chrome share the OS's bond, so pairing once
  covers both.
- **Pairing window = while NUS is on**, which already needs the USB console
  or the wall-switch gesture (and shows the blue LED). The first connection
  in that window pairs. A pairing that completes outside it is dropped
  (bond not stored, link disconnected). With `[ble] knixx` on, the board
  advertises all the time, but only bonded centrals get in.
- **Bonds:** up to 4, saved in `/bonds.bin` on the littlefs
  (crates/knixx-storage), loaded at boot with `stack.add_bond_information`.
  Console: `ble bonds` lists them, `ble forget [all|<addr>]` removes them.
  The USB console always works, so a lost phone is never a lockout.
- **OTA:** `ota` still reboots into the Adafruit bootloader, whose DFU has
  no pairing. Reaching it now needs a bonded console (or USB), so that is
  covered. Document it.

### 2. bolt with an encrypted NUS (host/bolt, host/knixx-host)

- **macOS:** CoreBluetooth pairs on its own on the first encrypted access
  (a system prompt), so bolt needs no change.
- **Linux:** btleplug has no pairing API, and BlueZ needs an agent. Document
  `bluetoothctl pair <addr>` (once, during `nus on`). In `client.rs`, turn
  ATT "insufficient authentication/encryption" into a hint that names that
  command.
- `cargo xtask term|ota|cycle` go through bolt and inherit this.

### 3. The knixx GATT service (behind `web-ble`)

`KnixxService` in `Server`, own 128-bit UUID base (e.g. `6b6e7800-…`).
Its characteristics require pairing while `[ble] pair` is on, through the
same runtime check as NUS (step 1).

| char | props | payload (≤ 20 bytes, so it fits the default ATT MTU 23) |
|---|---|---|
| `send` | write (with response) | `seq u8, op u8 (0 write, 1 read, 2 response), group u16 BE, prio u8, data…` (data as `Telegram::data()`: from the APCI octet on, APCI bits cleared, ≤ 15) |
| `result` | notify | `seq u8, status u8` (0 confirmed, 1 negative con, 2 no con, 3 refused, 4 busy) |
| `group` | notify | `flags u8 (own, op, prio), src u16, dest u16, data…` for each received group value telegram |

Advertising data (31 bytes) can't hold two 128-bit UUIDs plus the name:
flags + knixx UUID + name in `adv_data`, NUS UUID in `scan_data` (bolt
matches on the name only, `host/knixx-host/src/blescan.rs:34`).

### 4. Wire codec, no_std and host-tested

New `crates/knixx-knx/src/gatt.rs`: `decode_send(&[u8]) -> Result<(u8, Telegram)>`
and `encode_group(&Telegram, own, &mut [u8; 20]) -> usize`, built on
`tp1::Telegram` (`crates/knixx-knx/src/tp1.rs:224`). Unit tests beside it.

### 5. Send results from the bus engine (crates/knixx-bus/src/lib.rs)

- `Command::Send` gains a tag: `Send { tg: Telegram, tag: Tag }`, with
  `Tag::Script | Tag::Gatt(u8)`; scripts pass `Tag::Script`
  (`firmware/knixx-fw/src/runner.rs`).
- `Output` gets `fn sent(&mut self, _tag: Tag, _r: SendResult) {}` (default
  no-op), called where the engine already decides: `refuse()` (lib.rs:211),
  positive/negative L_Data.con in `feed`, and the `CON_TIMEOUT` branch in
  `poll`. Track the tag next to `self.script`.
- Requests from the knixx service go into the existing `console::CMDS` with
  `try_send`; full ⇒ `busy` right away. Same `refuse()` rules and own source
  address as scripts.

### 6. Firmware glue (new firmware/knixx-fw/src/knixx_gatt.rs, `web-ble`)

- `GROUP: Channel<(Telegram, bool), 8>` and `RESULTS: Channel<(u8, SendResult), 4>`,
  fed from `bus::Console::telegram` / `sent` (`firmware/knixx-fw/src/bus.rs:44`);
  the `telegram` hook calls `runner::telegram` under `scripts` and
  `knixx_gatt::telegram` under `web-ble`. Only group value telegrams are
  queued, and only while a bonded central is subscribed.
- In `ble::serve`, a `knixx` future joins `events`/`send`: decodes `send`
  writes, notifies `result` and `group`.

### 7. When the board advertises

- New config key `[ble] knixx = on|off` (default off) in
  `crates/knixx-device` (parser, `default.ini`, README table). With it on
  (and `web-ble` built in), the board advertises continuously; otherwise, as
  today, only during `nus on`.
- The serve loop's "on" becomes `nus_on || knixx_enabled`; the 60 s idle
  timeout keeps applying to NUS only. NUS writes reach the console only
  while it is on NUS. One central at a time (`peripheral_count(1)`) stays.

### 8. The web app: Leptos (CSR) to wasm, in web/

**Why Leptos:** the page is Rust, so it uses `knixx-knx` directly: the
`gatt` codec, group address parsing and the DPT encode/decode. Board and
browser can't drift apart. Later, a script editor could compile Rhai in the
browser with `knixx-script`.

- **Layout:** `web/` is a workspace of its own, excluded from the root
  workspace like `firmware/`. `web/rust-toolchain.toml` pins the root's
  channel and adds `wasm32-unknown-unknown`. It depends on
  `knixx-knx = { path = "../crates/knixx-knx" }` (no_std, no deps).
- **Crates:** `leptos` (csr, exact version pinned), `wasm-bindgen`,
  `wasm-bindgen-futures`, `web-sys` with the Bluetooth features.
- **Web Bluetooth bindings:** unstable in web-sys, need
  `--cfg=web_sys_unstable_apis` in `web/.cargo/config.toml`. One module,
  `web/src/ble.rs`: `connect()`, `send(req)`, and streams of `result` and
  `group` notifications. The rest of the app sees only Rust types.
- **UI:** connect (`requestDevice({filters:[{services:[KNIXX_UUID]}]})`; the
  OS pairs on first use), a group address field (`x/y/z`), Write/Read, a DPT
  picker (1.001, 5.001, 9.001 to start, from `knixx-knx`), and a log of
  results and incoming telegrams with decoded values, matched by `seq`.
- **Lights at startup:** the main view is a grid of 24 lights on
  `0/1/1`…`0/1/24`. Right after connecting, the app polls their state:
  - It sends a GroupValueRead to each address **one at a time**: the next
    only after the previous one's `result`, so the board's command queue
    (`console::CMDS`, 16) never fills and the TP1 bus (9600 baud) isn't
    flooded. 24 reads take about 1–2 s. A `busy` result is retried after
    100 ms.
  - The GroupValueResponse telegrams on `group` set each light on or off
    (DPT 1.001). A light stays *unknown* (grey) until it gets an answer;
    without one 2 s after its read, it shows *no answer*.
  - Afterwards, every GroupValueWrite or Response to these addresses on
    `group` (from wall switches, other devices, or the app's own writes,
    `own`) keeps the grid live. Tapping a light sends a GroupValueWrite of
    the opposite value.
  - The range is a setting in the app (first address, count), kept in the
    browser's `localStorage`, with `0/1/1` and 24 as defaults.
  - Caveat for the README: a read gets an answer only from an actuator
    whose object has the read flag on that address. Installations with
    separate status addresses need those as the range instead.
- **Build:** `trunk build --release` → `web/dist/`; locally `trunk serve`
  (localhost is a secure context).
- **Hosting:** GitHub Pages via a new `.github/workflows/pages.yml` on pushes
  to `main` touching `web/` or `crates/knixx-knx/`: wasm target + trunk
  (taiki-e/install-action), `trunk build --release`, `actions/deploy-pages`.
- **CI:** `cd web && cargo clippy --target wasm32-unknown-unknown -- -D warnings`
  and `cargo fmt --check`; `cargo deny --manifest-path web/Cargo.toml check licenses`.
- **xtask:** `cargo xtask web` (trunk serve) and `cargo xtask web --build`.

### 9. Docs

README: a "Pairing" section (the window, `ble bonds|forget`, macOS vs
`bluetoothctl pair` on Linux), a "Web Bluetooth" section (`--web-ble`,
`[ble] knixx`, the app's URL, browser support: Chrome/Edge on desktop and
Android, not Safari/iOS or Firefox), and replace the BLE security note
(README:352).

## Order of work

1. Pairing + encrypted NUS + bond storage + `ble` commands; bolt hint.
   Ship on its own: it closes the open console.
2. Engine `Tag`/`sent()` + `knixx-knx::gatt` codec, host tests.
3. `web-ble`: knixx service, advertising, `[ble] knixx`, xtask/CI/release.
4. `web/` Leptos app + Pages workflow.
5. Docs.

## Risks to check early

- trouble-host 0.8 SMP: confirm Just Works bonding with macOS (bolt and
  Chrome), Android and Windows Chrome, and Linux BlueZ; how to refuse
  pairing outside the window (reject in the pairing event vs. disconnect
  after `PairingComplete`).
- The runtime `[ble] pair` check: confirm that each OS starts pairing on
  `INSUFFICIENT_ENCRYPTION` from a characteristic that has no static
  `encrypted` flag, and on the board's `request_security()`. Check also that
  trouble hands writes to `events` before storing them, so a rejected write
  never lands. If it doesn't, use `accept_unprocessed` or two GATT tables.
- Flash/RAM in **every** build now: measure SMP + P-256 with
  `cargo xtask build` and `--no-scripts` right after enabling it (~150 KB
  headroom with scripts, README:140).
- Linux: bolt can't pair by itself; if `bluetoothctl pair` turns out to be
  too clumsy, pairing over BlueZ's D-Bus `Device1.Pair` in bolt is the
  follow-up.
- Chrome on Linux may still need a flag for Web Bluetooth.
- web-sys's Web Bluetooth bindings: pin the version; check that notifications
  and `writeValueWithResponse` are bound, else add `#[wasm_bindgen]` glue.
- wasm size: a few hundred KB expected (trunk's wasm-opt, `opt-level = "z"`).

## Verification

- `cargo test`: codec tests in knixx-knx; knixx-bus tests that `sent()`
  reports confirmed / negative / no con / refused with the right tag.
- `cargo xtask build`, `--no-scripts`, `--web-ble`, `--web-ble --no-scripts`
  all fit and keep the heap rule; note the flash added by pairing.
- Pairing: `nus on`, `bolt term` on macOS pairs and works; on Linux after
  `bluetoothctl pair`. After `nus off`, an unbonded central gets no console
  (insufficient encryption) and can't pair; the bonded one still works after
  a reboot; `ble forget all` locks it out; USB console always works.
- `config set ble pair off`: an unbonded central gets the console (and the
  knixx service) without a prompt, as today; `config get ble pair` shows off
  after a reboot. `config set ble pair on` drops an unencrypted central at
  once, and the bonded one still gets in without pairing again.
- Host tests in knixx-device: `pair` parses on/off, a missing key means on,
  `default.ini` has `pair = on`.
- Web: `cd web && trunk build --release`; `cargo xtask web` on localhost.
- On a board on a bus (`--web-ble`, `config set ble knixx on`): pair in the
  window. On connecting, the grid fills in with the states of 0/1/1…0/1/24
  within a few seconds, with no `busy` in the log; a light switched by a
  wall switch updates its tile; tapping a tile switches the light. Write
  0/0/1 on and see the actuator switch and `confirmed`; read a
  group and see the response; unplug the bus and see `no con`. The same
  Chrome then also reaches NUS without pairing again.
