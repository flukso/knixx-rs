# BLE TX power, coded PHY, 2M PHY and data length extension

> **Status: completed.** The implementation starts with commit `d08e74e`
> (2026-11-03, "BLE TX power, 2M PHY, larger packets and ble link": steps
> 1, 2 and 2b), followed by `0e13fda` (the coded PHY, xtask, CI and README).
> Beyond the plan: two connectable advertising sets need two peripheral
> links in the controller ("Connection Rejected due to Limited Resources"
> with one), and advertising failures are now said on the console. Tested
> on the board with a Mac: TX power live (-20 to +8 dBm moved the Mac's
> RSSI by 25 dB), 2M and 251 B packets, `ble link`, and 1M through extended
> advertising with the coded set on. Reception on coded is still to be
> checked with an Android phone (nRF Connect).

## Context

Today the BLE radio's settings are fixed:

- TX power is the controller's default, 0 dBm (the nRF52840 can reach +8).
- Advertising is legacy, on 1M.
- Connections stay on 1M with 27-byte link-layer packets.

The SoftDevice Controller is built with `support_adv().support_peripheral()`
only (`firmware/knixx-fw/src/ble.rs:123-134`), so it also refuses a phone's
requests for 2M, for coded or for larger packets.

The user wants:
- a TX power setting
- coded PHY for range (125 kbps, S=8, about 4 times the range of 1M)
- 2M and DLE for faster uploads, which are cheap and work in today's
  controller library

**Decisions (user):**
- **Coded PHY** goes behind a Cargo feature, `coded-phy`, off by default,
  like `web-ble`.
- **Default TX power** stays 0 dBm.
- **2M and DLE** are added in every build.

**Constraints found:**
- **The library:** `support_ext_adv` and `support_le_coded_phy` exist only in
  nrf-sdc's *multirole* library (`sdc.rs:450,503`, gated on central +
  peripheral). Today's `peripheral` library lacks them, and multirole costs
  more flash; the cost is to be measured.
- **The command lock:** the controller locks into legacy or extended
  advertising commands at the first one (`sdc.rs:977-987`). So a `coded-phy`
  build uses only extended commands (`advertise_ext`), also for the 1M legacy
  set.
- **trouble:** its `extended-advertising` feature is off today
  (`default-features = false`), and `HostResources` has `ADV_SETS` = 1.
- **After a connection:** when a connection comes in on one advertising set,
  trouble does not stop the other (`peripheral.rs:381-388`). The firmware has
  to send `LeSetExtAdvEnable(false, &[])` itself.

## Design

### 1. Configuration (crates/knixx-device)

- `Ble` (`thunder.rs`) gains `tx_power: i8` (default 0) and `coded: bool`
  (default off). `TX_POWERS: [i8; 14] = [-40, -20, -16, -12, -8, -4, 0, 2, 3,
  4, 5, 6, 7, 8]` lists the nRF52840's levels.
- `config.rs`:
  - keys `ble.tx_power` (dBm) and `ble.coded` (on|off), with `KEYS`
    becoming 17
  - a new `Slot::Choice { n: &mut i8, allowed: &[i8] }` with signed parsing,
    and `ValueError::Choice` reading `"9" is not one of -40, -20, …, 8`. The
    existing `Number` slot takes unsigned u16 only.
  - `write_ini` writes both keys, and `default.ini` gets `tx_power = 0` and
    `coded = off`
  - tests: parse, set and get, the error text, a missing key keeping its
    default, a round trip, and the existing `BLE` constant and format tests
- `firmware/knixx-fw/src/console.rs` `check()`: `ble.coded on` without the
  feature gives "coded PHY is not built in (cargo xtask build --coded-phy)",
  as `ble.knixx` does for `web-ble`.

### 2. Every build: 2M, DLE and TX power (firmware/knixx-fw/src/ble.rs)

- **Builder:** `.support_le_2m_phy().support_phy_update_peripheral()
  .support_dle_peripheral()`, and `.default_tx_power(dev.ble.tx_power)?` from
  the configuration loaded at boot.
- **Memory:** `SDC_MEM` comes from `Builder::required_memory()`. Find each
  build's value once, from a temporary print or an assert, and set it per
  feature set.
- **On connecting** (next to the connection-parameter update,
  `ble.rs:265-278`):
  - `conn.raw().update_data_length(stack, 251, 2120)`, best effort
  - set the connection's TX power with `nrf_sdc::vendor::ZephyrWriteTxPower`
    (handle type 2, the connection handle), sent through `stack.command`
- The board doesn't ask for a PHY itself. The controller now accepts a
  central's request for 2M or coded.
- **Live changes:** `bonds::CHANGED`, which `set_device` already signals,
  makes the BLE task send `ZephyrWriteTxPower` to the advertiser (handle type
  0) and to the connection, so `config set ble.tx_power 8` applies at once.
- **Visibility:** `ble` (console.rs `ble_cmd`) gains a line `ble: tx power
  0 dBm, coded off|on|not built in`.

### 2b. `ble link`: the parameters of the active connection

A new subcommand of the console's `ble` command. With no central connected it
prints `ble link: no central connected`; otherwise, for example:

```
central    a0:9a:8e:94:d5:3f (bonded, encrypted)
connected  2m14s
interval   15 ms, latency 0, supervision timeout 4 s
phy        tx 2M, rx 2M
data       tx 251 B / 2120 µs, rx 251 B / 2120 µs
att mtu    247
tx power   0 dBm
rssi       -58 dBm
```

- **Where the values come from:** the BLE task keeps a `LINK` static
  (`Mutex<RefCell<Option<Link>>>` in ble.rs).
  - On connecting: the peer identity, `security_level()`,
    `is_bonded_peer()`, the time, `conn.raw().params()` (interval, latency,
    supervision timeout), and `read_phy`.
  - Then from `events()`, which ignores these events today:
    `ConnectionParamsUpdated`, `PhyUpdated`, `DataLengthUpdated`,
    `Encrypted` and `PairingComplete`.
  - The ATT MTU from the connection, after the MTU exchange.
- **RSSI and TX power** need HCI commands, which the synchronous console
  command can't wait for. While a central is connected, the BLE task reads
  them every 2 s, `ReadRssi` and `ZephyrReadTxPower` (handle type 2), into
  `LINK`. The web-ble RSSI loop (`ble.rs:505-516`) uses the same reading.
- **Cleanup:** `LINK` is cleared on disconnect.
- **Help text:** `ble [link | forget all|<address>]`, and the README's
  `ble` row describes `ble link`.

### 3. The `coded-phy` feature

`firmware/knixx-fw/Cargo.toml`:

- `coded-phy = ["nrf-sdc/multirole", "trouble-host/extended-advertising"]`.
  Check how nrf-sdc-sys picks the library when `peripheral` and `multirole`
  are both on; move `peripheral` behind `not(coded-phy)` if needed.

The controller and host:

- **Builder:** `support_ext_adv()` instead of `support_adv()`, plus
  `support_le_coded_phy()`, `adv_count(2)` and `adv_buffer_cfg(64)`.
- **Host resources:** `HostResources<…, 1, 2, 2, …>`, two advertising sets.

Advertising, in `serve`, with `advertise_ext` in this build:

- **Set 0:** today's `ConnectableScannableUndirected` on 1M, with the
  advertising data and scan response of `advertising()`, so phones and
  Chrome keep finding the board.
- **Set 1, while `ble.coded` is on:** `ExtConnectableNonscannableUndirected`,
  with `primary_phy` and `secondary_phy` on `LeCoded`. Its data, up to 64 B,
  holds the flags, the service UUIDs and the name, since there is no scan
  response.
- **Both sets** advertise at `tx_power` from the configuration
  (`TxPower::try_from` on the dBm value).
- **After `accept`:** send `LeSetExtAdvEnable(false, &[])`, so that the
  other set stops. A change of `ble.coded` (`bonds::CHANGED`) restarts
  advertising with the new set list.
- **A central connecting on coded** gets a link on coded; `ble` shows it.

### 4. Build, CI, release, docs

- xtask: `--coded-phy`, which combines with `--web-ble` and `--no-scripts`.
- CI: also build and lint `--web-ble --coded-phy`.
- Release: unchanged for now, with the default and `web-ble` builds. Note the
  `coded-phy` build in the README.
- README:
  - the Configuration table rows and the `default.ini` block
  - the `ble` command row
  - Bluetooth in "How it works": the PHYs, DLE, TX power, the two advertising
    sets and the command lock
  - the build sizes, measured
- Save this plan as `plan/ble-radio.md`, marked completed with its first
  implementation commit, like the others, in a commit of its own after the
  implementation.

## Order of work

1. Configuration keys and tests (knixx-device), and `check()`.
2. Every build: 2M, DLE, TX power (default, per connection, live) and the
   `ble` output. Flash, then measure with bolt: upload speed and TX power
   changes.
3. `coded-phy`: the multirole library, extended advertising with two sets,
   coded. Measure flash and RAM.
4. xtask, CI, README, `plan/`.

## Risks

- **Flash:** the multirole library may not fit next to scripts and `web-ble`
  (87% today). Measure it right after step 3's first build. If too big,
  `coded-phy` stays for builds without scripts or without `web-ble`.
- **Who speaks coded:** iPhones, Macs and Chrome's Web Bluetooth can't
  initiate coded. Testing needs Android with nRF Connect, which scans and
  connects on coded, or a second nRF52840 as central.
- **Legacy PDUs through `advertise_ext`:** trouble's code path for this is
  untested here. Check that the Mac still connects on set 0 and that bonding
  still works.
- **MPSL timing with two advertising sets and a connection:** fine for one
  connection, but check that USB and the bus keep working.

## Verification

- `cargo test`: the knixx-device configuration tests.
- Clippy and fmt in all workspaces, for the builds default, `--web-ble`,
  `--coded-phy` and `--web-ble --coded-phy`.
- On the board, default build:
  - `config set ble.tx_power 8`, then `-20`: nRF Connect's RSSI moves by
    about 28 dB, without a reconnect.
  - A Mac or phone connects. `ble link` (on the USB console) shows its
    address and security, 15 ms interval (or what the central chose), the
    PHY (2M when the central asks), 251 B packets, the MTU, the TX power and
    an RSSI that changes as the phone moves. Without a central it says so.
  - `bolt script upload` over BLE is faster than before (time a 16 KiB
    script).
- On the board, `--coded-phy`:
  - `config set ble.coded on`: nRF Connect on Android shows two advertisers,
    one 1M and one coded.
  - Connecting on coded gives a link on LE Coded in `ble`, and the web app
    still works on 1M from Chrome.
  - `ble.coded off` removes the coded set live.
- Sizes: `cargo xtask build` for all four combinations, with flash and RAM in
  the README.
