# One upload protocol: knixx-xfer

> **Status: implemented, the bus side still to test.** From commit
> `1a7767b` (2026-11-11, knixx-xfer) through `7afda86` (fleet push and
> pull). Tested on the isolated Feather nRF52840:
> - `bolt flash` through the new recovery, and a cut-off flash resumed;
> - `bolt fw push` at 17 KiB/s, a killed push resumed, then the install;
> - `bolt script push` and `bolt config push`: applied live, kept over a
>   restart, saved byte for byte; refusals for another individual address
>   and outside the window;
> - pulls: the config byte for byte, the scripts with their hashes, the
>   push skip by hash and by bytes;
> - `bolt fleet push` and `pull` by loopback, the one board being proxy
>   and fleet: pull, push, pull leaves the folder unchanged.
>
> Changes on the way: the bootloader fit its 64 KiB (60.5 KB), so it did
> not move. The script push skip came with the pulls (phase 5), as it needs
> the script list. Turning `ble.pair` on from a pushed config dropped the
> very link before its `done`; an unpaired central in an update window now
> keeps the transfer service.

## Context

A file reaches a board today in four ways, built one after another:

| path | what | how |
|---|---|---|
| `script put` on the console | scripts, ≤16 KiB, into RAM | 128-byte chunks, `ok` after each |
| bootloader recovery `fw put` (USB) | firmware | 4 KiB chunks, unframed, `ok` after each |
| console `fw put` (USB, NUS) | firmware, into the stager | `[offset][CRC][data]` frames, resend, a drop window, a binary filter, a delayed finish |
| fw GATT service (BLE) | firmware, into the stager | `[offset][data]` writes, ack every 4 KiB, rewind, resume |

The config has no upload at all: it changes one key at a time with
`config set`. Nothing can be read back off a board as a file.

The goal is one protocol, designed once, with its codec and receiver in one
host-tested crate, used in two places:
- the **bootloader's USB recovery**, for firmware;
- the **app's BLE transfer service**, for firmware, scripts and the whole
  config, pushed and pulled.

On top of that, fleet-wide pushes and pulls of the per-board files, kept in
one folder per board under version control.

This plan was reviewed independently by a Fable agent; its findings are in.

### Decisions (user)

- **Firmware is always signed** (`.kfw`, ed25519, no downgrades).
- **Scripts and config are unsigned,** and taken (and given, in a pull) in
  any update window, whether opened from the console or by the KNX control
  plane.
  - Known and accepted: anyone who can send a KNX `open` or `window` and is
    in BLE range can read or change a board's scripts and config while its
    window is open. The README says so.
- **In the app, uploads go over BLE only.** USB carries the console, and the
  bootloader's recovery.
- **A config upload is the whole `knixx.ini`:** checked, applied live and
  saved, or refused whole. `config set` stays for single keys.
- **One transfer at a time per board.** A fleet run works on several boards
  at once.
- **`MAX_SCRIPT` is a named constant,** alongside `MAX_IMAGE` and
  `config::MAX_FILE`.
- **The bootloader may grow** if it must: there is a single board in
  development.
- **Pulls** of the config and the scripts, per board and fleet-wide, go
  into the folder layout that the fleet push reads, overwriting.

## Design

### 1. The protocol (new crate `crates/knixx-xfer`, no_std by default; `std` feature for the host)

**Requests.** The host starts every transfer with a request: `put` sends a
file to the board, `get` takes one from it. From then on, the end with the
file is the **sender** and the other the **receiver**: bolt in a push, the
board in a pull.

**Messages.** Little-endian throughout.

| message | bytes | from |
|---|---|---|
| `put` | `0x01, kind u8, flags u8, size u32, crc32 u32, name_len u8, name…`: the host offers a file. The board answers `ack 0` (or `ack <offset>`, to resume a firmware push), or `error`. Flag bit 0 is `force` (section 3). | the host |
| `get` | `0x04, kind u8, name_len u8, name…`: the host asks for a file. The board answers `file` or `error`. | the host |
| `file` | `0x05, kind u8, size u32, crc32 u32, name_len u8, name…`: the board announces the file it sends; bolt answers `ack 0` | the board, to `get` |
| `data` | `0x02, offset u32, bytes…` | the sender |
| `abort` | `0x03`: either end gives up; ignored once the receiver has completed | either |
| `ack` | `0x10, offset u32`: the offset the receiver takes next; past the size it is a protocol error | the receiver |
| `rewind` | `0x11, offset u32`: data came for another offset | the receiver |
| `done` | `0x20, kind u8, crc32 u32, info u32`: the file's CRC (so that a late `done` of an earlier transfer is recognised) and the info (firmware: the counter; the others: the size) | the receiver |
| `error` | `0x21, code u8, crc32 u32, reason…`: the CRC of the file it is about, 0 for a refused request | the receiver, or the board to a request |

**Kinds:**

| kind | | cap | push to the bootloader | push to the app | pull from the app |
|---|---|---|---|---|---|
| `0` firmware | a `.kfw` | `MAX_IMAGE` | yes | yes, signed | no |
| `1` script | bytecode for `/scripts/<name>.rhgn` | `MAX_SCRIPT` | no | yes | yes |
| `2` config | the whole `knixx.ini` | `config::MAX_FILE` | no | yes | yes |
| `3` script list | text, one `<name> <size> <hash>` line per script (≤40 B per line, at most 2 KiB) | 2 KiB | no | no | yes |

- **`MAX_SCRIPT`** (16 KiB today) is defined in knixx-xfer. The firmware
  uses it in place of `script::MAX_SIZE` (`firmware/knixx-fw/src/script.rs`),
  and so do the runner's limits, bolt and the README's limit table.
- **Names** pass `knixx_console::upload::valid_name` (at most 24 bytes).
- **Size 0:** refused for script and config. The script list may be empty:
  `file` with size 0, then `done` after `ack 0`.

**The flow:**
- **Start:** `put` → `ack 0`, or `get` → `file` → `ack 0`.
- **Data:** the sender keeps up to its window in flight: 16 KiB over BLE, one
  4 KiB frame over USB (stop-and-wait).
- **Acks:** the receiver takes data only at the offset it expects, and acks
  every 4 KiB and at the end.
- **Gaps:** data at another offset gets a `rewind`, at most every 200 ms. A
  sender that hears nothing for 1.5 s goes back to its last ack.
- **Waiting for `done`:** once all is sent and acked, the sender stops
  counting stalls and waits up to 60 s for `done` or `error`.
- **The end:** the last `ack` goes out before the receiver checks the file
  (the board: signature, read-back, config parse; bolt: the CRC). Then comes
  `done` or `error`.
- **Resume:** a firmware push only, after a lost link. A `put` of the same
  kind, name, size and CRC as the transfer in progress answers `ack <where it
  got to>`. Pulls do not resume: a pulled file is at most 16 KiB, so a lost
  pull is asked for again from 0.
- **Stale notes:** the sender drops a `done` or `error` whose CRC is not its
  file's.

**Stream framing,** for USB:
- **A frame:** `0xA5 0x5A, len u16, message, crc32(message)`.
- **`len`** is capped at the largest message (`data` with 4 KiB), so that a
  false magic inside the payload (0xA5 0x5A will occur in a `.kfw`) fails at
  once. On a bad CRC or length, the receiver resyncs on the next magic.
- **Text and frames:** the recovery console's text commands (`version`,
  `info`, `boot`) live alongside the frames, since a frame starts with 0xA5,
  which no one types. The bootloader's replies to frames are frames too, and
  there is no prompt while a transfer runs.

**What the crate holds:**
- the codec;
- the stream framing;
- **`Receiver<S: Sink>`,** a pure state machine:
  - it handles offsets, acks, rewinds, resume, the caps per kind, the
    whole-file CRC, which kinds a place takes, and the idle replacement
    (section 3);
  - **`Sink`:** `open(kind, name, size)`, `write(bytes)`,
    `finish() -> Result<info>`, `abort()`;
  - the caller gives the time;
  - **completion:** a write that completes the file returns `Complete`, and
    the caller sends the last ack *before* it calls `finish` (section 3).

**The sender is written twice,** each small and tested against the shared
`Receiver`:
- **bolt's:** async, std. This is `push.rs`'s sender, generalised: the
  window, rewind, stalls, resume, the stale-note filter and progress.
- **the board's,** for pulls: send at most 16 KiB in `att_mtu − 3`-sized
  notifications, then wait for `ack`, `rewind` or `done`, with embassy
  timers.

**Tests (cargo test, both directions):**
- the codec round trips;
- stream resync after garbage and false magics;
- bolt's sender against the `Receiver` on a simulated link that drops,
  damages, reorders and duplicates, and loses the link mid-file (resume);
- a scripted board sender against the `Receiver` with a `Vec` sink (the
  pull);
- caps, size 0, kinds refused to `put` and to `get`, stale `done`, `ack`
  past the size, `abort` after completion, the idle replacement.

### 2. Bootloader recovery on knixx-xfer (`firmware/knixx-boot/src/recovery.rs`)

- **`fw put`, `upload` and `drain` go.** Framed messages take their place,
  for the firmware kind only, stop-and-wait. The `Sink` is the existing
  `Writer`, with the `Verifier` and the counter check as now.
  - The text commands `version`, `info` and `boot` stay.
  - `put`'s `force` flag takes the place of `fw put … force`.
- **The size budget:**
  - Measured: 60,128 of 65,536 bytes, so 5.4 KB free (`profile.boot` is
    `opt-level z` already).
  - Stop-and-wait keeps the receiver small; measure it first.
  - **If it does not fit,** `BOOT_SIZE` and `HEADER_ADDR` move up in 4 KiB
    steps (single constants in knixx-image). `memory.x`, xtask, the ACL and
    the layout test follow them, every image is rebuilt, and the Feather is
    `flash-probe`d. This is a one-way door: `Header.load` must equal
    `APP_START`, so images of one layout are refused under the other, by
    the bootloader and the app's stager alike. After a move, every board
    needs SWD once. That is fine with one board; not with a fleet.
- **Host:** `knixx-host/src/flash.rs` runs bolt's sender over the serial
  port.
  - **A stream transport:** a reader thread over the blocking `TTYPort`
    (50 ms reads) feeds the framing decoder; writes are blocking.
  - `bolt flash` and `xtask flash` are unchanged on the outside.
- **Moving boards:** recovery's protocol changes, and the bootloader cannot
  update itself, so the Feather is `flash-probe`d.

### 3. The app's transfer service (`fw_gatt.rs` becomes `xfer_gatt.rs`)

- **The characteristics:**
  - `ctrl`: write and notify, its value 48 bytes, so that a `put` with a
    24-byte name fits (35 bytes), and `error`'s reason too;
  - `data`: write without response, and **notify** for pulls. Its CCCD
    grows `ATTRIBUTES` (`ble.rs:93`) by one and changes the DB hash; the
    Service Changed path covers bonded centrals.
  - Notifications are sized by `conn.raw().att_mtu() - 3`.
- **Completion outside the write handler** (review B2). The GATT write
  handler (`events()` in `ble.rs`) only feeds the `Receiver`.
  - On `Complete` it signals a transfer task. That task sends the last
    `ack`, awaiting `notify_raw` (the SDC transmits from its interrupt even
    while the executor then blocks on flash), then runs `Sink::finish`, then
    notifies `done` or `error`.
  - So the laptop has its ack before a seconds-long check, and no 16 KiB
    window is resent and dropped meanwhile.
- **Sinks:**
  - **firmware:** the `Stager` (`knixx_image::stage`), as now.
  - **script:** appended to `/scripts/<name>.tmp`, read back for the CRC,
    renamed to `.rhgn`; then `runner::CONTROL.send(Control::Load(name))`.
  - **config** (review B4, B5, S6): buffered in RAM (≤1 KiB), then a shared
    `config_load(ctx, data, force)` in `crates/knixx-cli/src/lib.rs`
    (`config_set`'s path, factored out):
    1. **Parse** with `config::parse`.
    2. **Guard:** refuse a file whose `device.individual` is not the board's
       own, unless `force`.
    3. **Check** with `Ctx::check`.
    4. **Apply:** `Command::Config`, refused whole when the queue is full.
       Then `set_device`.
    5. **Save the bytes received, verbatim** (not re-formatted, so that
       comments and layout stay and a pull gives back what was pushed).
       Then `done`.

    What applies live, and on this very link:
    - `ble.tx_power` and `ble.coded`, through `bonds::CHANGED`, on the
      transfer's own connection (a coded request is refused by a Mac, which
      falls back to 1M);
    - `ble.pair` (an unpaired central in a window is let be);
    - `ble.knixx`, at the next advertising;
    - `device.individual`, at once in the engine and the control plane
      (hence the guard).

    The README lists them.
- **Pulls:** `get` on `ctrl` starts the board's sender, whose source is:
  - `/knixx.ini` for the config;
  - `/scripts/<name>.rhgn` for a script;
  - for the script list, a listing built in one `with_flash` call: name and
    size from `storage::list_dir`, and the source hash that each loaded
    script reports in `on_info` (`runner::STATUS`). The hash is `-` when
    the script is not loaded or has no `{{hash}}`.

  The sender times out when `ack 0` does not come (a central that did not
  subscribe to `data` drops notifications silently). bolt subscribes to
  both characteristics before `get`.
- **The window** is the gate. `fw window` becomes `upload window
  <min> | off` (this phase), and so does the KNX control plane's `open` and
  `Window` (section 5).
  - It is checked at `put` and at `get` only: a transfer that started in
    the window may finish after it.
  - `fw install`, `discard`, `status` and `recovery` stay. The service is
    not behind `ble.pair`, as now.
- **One transfer at a time per board.** A `put` or `get` during another gets
  `error busy`.
  - Exception: when nothing has flowed for 10 s, a new request replaces the
    stale transfer, so that a crashed bolt does not hold the board until the
    window closes.
  - The `Board.busy` of the control plane now means "a transfer is in
    progress".
  - A disconnect drops a pull; it keeps a firmware push for resume.
- **Removed** (review S8):
  - `fw put` and `script put` on the console, and with them `fw::Upload`,
    `console_busy`, the drop window, the binary filter, `finish_at` and the
    `FW_STATS` static;
  - `knixx_cli::Action::Upload` (knixx-shell follows);
  - `knixx_image::stage::{Frames, frame, frame_crc, MAX_CHUNK,
    FRAME_HEADER}` and their tests;
  - in `knixx_console::upload`: `Upload`, `Event`, `CHUNK`.

  Kept: `valid_name`, `Name`, `MAX_NAME`, `crc32`; the `Stager` and `Files`.
  The NUS ring goes from 4 KiB to 1 KiB (a pasted command of a few packets
  still fits); saves about 3 KiB of RAM.

### 4. bolt

- **Pushes** (bolt's sender over GATT):
  - **`bolt fw push <kfw>`:** as now. `xtask fw` becomes `fw push --open`.
  - **`bolt script push <script> [--name]`:**
    - takes a `.rhai`, which it builds, or a `.rhgn`;
    - replaces `script upload`;
    - skips the push when the board's listing shows the same source hash
      as `md5(.rhai)`, or the same bytes for a `.rhgn`; `--always` pushes
      anyway.
  - **`bolt config push <knixx.ini> [--force]`:** refused by the board when
    `device.individual` differs, unless `--force`.
- **Pulls**, written into the board's folder, overwriting:
  - **`bolt config pull [--out <dir>]`:** `<dir>/<board>/knixx.ini`, where
    `<board>` is the pulled file's `device.individual`. The default `<dir>`
    is `.`.
  - **`bolt script pull [<name> | --all] [--out <dir>]`:**
    `<dir>/<board>/scripts/<name>.rhgn`. It says whether a sibling
    `<name>.rhai` matches the listing's hash.
- **The window:** each push and pull needs a window. `--open <minutes>`
  opens it first over the board's console (USB or paired NUS), with `upload
  window`.
- **A folder with both `<name>.rhai` and `<name>.rhgn`:** the source wins.
  The `.rhgn` from a pull is a record of what runs.
- **macOS:** CoreBluetooth already holds writes without response back until
  it can send them. BlueZ does not, which is why the window and rewind
  stay. The code says so, so that no one adds sleeps.

### 5. Fleet push and pull

- **KNX control plane** (`crates/knixx-knx/src/fw.rs`, `fw_knx.rs`):
  - **`Window { minutes }`** opens the window with no image check, answered
    `Ready { ble }` or `Refused`. It is rate-limited through `LAST_OPEN`,
    as `open` is.
  - **`Close`** also aborts a transfer in progress.
  - `answer()` and its tests grow by these.
- **`bolt fleet push <dir>`:** one folder per board, by individual address:
  `<dir>/1.1.5/knixx.ini`, `<dir>/1.1.5/scripts/*.rhai|.rhgn`.
  1. Scan, and match the folders to the boards that answered. A folder
     whose name differs from its `knixx.ini`'s `device.individual` is
     refused, and fleet runs never force.
  2. `Window` on each.
  3. Find them over BLE (manufacturer data).
  4. Per board: the config, then the scripts, each skipped when the hashes
     or bytes already match; `--parallel` boards at a time, the proxy last,
     over its own link.
  5. Print a table per board and per file. Folders without a board, and
     boards without a folder, are listed.
  - **The proxy's config:** a change to `ble.knixx` or `ble.pair` gets a
    warning and needs `--force-proxy`, since it could strand the next run.
  - No reboots: config and scripts apply live. `bolt fleet update` stays
    for firmware.
- **`bolt fleet pull <dir>`:** scan, then `Window` on each, find them, then
  pull each board's `knixx.ini`, its script list and its scripts,
  `--parallel` at a time, the proxy last. It writes `<dir>/<board>/…`,
  overwriting, and prints what changed against what was there.
- **The round trip:** pull, then push the same folder, then pull again.
  The push skips everything (same config bytes, same script hashes), and
  the second pull leaves `git status` clean.

## Phases (each a commit series, tested on the Feather)

1. **knixx-xfer:** the codec, the framing, the `Receiver`, bolt's sender,
   and the simulated-link tests in both directions.
2. **Bootloader recovery** on knixx-xfer (stop-and-wait), and `bolt flash`
   on it. Measure the size; `flash-probe` the Feather.
3. **The app's transfer service,** firmware kind:
   - completion outside the write handler;
   - `ctrl` at 48 bytes;
   - `upload window`;
   - `bolt fw push` and `fleet update` on bolt's sender;
   - `xtask fw` becomes `fw push --open`;
   - the console's `fw put` and its framing go.
4. **Script and config kinds:** `config_load` (verbatim save, the
   individual guard), `bolt script push`, `bolt config push`. `script put`
   and `Action::Upload` go; the NUS ring becomes 1 KiB.
5. **Pulls:** `get`, the script list with hashes, the board's sender, `data`
   notify; `bolt config pull` and `bolt script pull`.
6. **KNX `Window`,** `Close` aborts; `bolt fleet push <dir>` and `bolt fleet
   pull <dir>`.
7. **README:**
   - one section on pushes and pulls;
   - the window's risk, for reads too;
   - what config applies live;
   - the README lines that mention `fw put`, `script put` and `fw window`.

   Save this plan in `plan/`.

## Verification

- **`cargo test`:** knixx-xfer (codec, framing, `Receiver`, both senders on
  simulated links); `config_load` against a fake `Ctx` (the guard, a full
  queue, saving verbatim); the `Window` answers.
  - The no_std loop of CI builds knixx-xfer for thumbv7em, as for every
    crate.
- **Sizes:** `cargo xtask build`:
  - the bootloader fits its region (xtask fails otherwise);
  - the app about the same or smaller;
  - RAM about 3 KiB smaller.
- **On the Feather:**
  - **`bolt flash` through the new recovery:** a link cut halfway, then
    `bolt flash` again, works.
  - **`bolt fw push`:** at about 16 KiB/s. bolt gets its last ack at once,
    then `done` after the check; a killed push resumes; then `fw install`.
  - **`bolt script push`:** the script runs (`script ls`, `script info`).
    A second push of the same `.rhai` is skipped. A changed one replaces it
    and reloads it.
  - **`bolt config push`:**
    - a changed `ble.tx_power` applies live (`ble link`) and survives a
      reboot;
    - the file is stored byte for byte as sent, comments included;
    - a bad file, or one for another `device.individual` (without
      `--force`), is refused whole, and the board keeps its config.
  - **Pulls:**
    - `bolt config pull` gives back the pushed file byte for byte;
    - `bolt script pull --all` writes every script, and reports which
      `.rhai` match.
  - **Outside a window,** pushes and pulls are refused. A transfer that
    started in the window finishes after it closes.
  - **A second request during a transfer** gets `busy`, but after 10 s
    without data it replaces the stale transfer.
  - **Fleet, by loopback (7.8.1):**
    - `bolt fleet push <dir>` pushes its `knixx.ini` and a script;
    - a folder for a board that is not there is listed;
    - a folder named differently from its `device.individual` is refused;
    - pull, then push, then pull again: the push skips everything, and
      `git status` is clean.
- **On a KNX line, later:** `fleet push` and `fleet pull` with several
  boards at once.
