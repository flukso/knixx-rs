# A mounted littlefs session, and a first `cargo xtask hil`

> **Status: done** (2026-11-12, commits ff1a0f2..65b861a, and the README).
>
> Measured with `cargo xtask hil` on the Feather, from a Mac:
>
> | push | before (mount per append) | after (one session) |
> |---|---|---|
> | firmware, 788 KB | 48.2 s, 16.0 KiB/s; 28.4 s on the flash (27.2 KiB/s) | 35.0 to 36.4 s, 21 to 22 KiB/s; 18 to 19 s on the flash (40 to 42 KiB/s) |
> | script, 12.5 KB | 10.5 s, 1.2 KiB/s | 1.0 to 1.3 s, 9 to 12 KiB/s |
>
> - `PIECE` at 16 KiB was no faster with the session (37.9 s), so it is
>   4 KiB: 12 KiB less RAM. The statics went from 128,952 to 119,616 bytes.
> - The host test, on an 800 KB stream: 8.7 MB read, 1.2 MB written and
>   407 erases with a mount per append; 0.86 MB, 0.80 MB and 196 with a
>   session.
> - The bootloader is 200 bytes larger (the free functions are sessions
>   of one call), and fits.
> - `--piece-sweep` was not built: the two builds were run by hand.
> - What the rest of the firmware push's flash time holds (erases, the
>   signature, the read-back) was not measured apart.

## Context

Every file operation of `knixx-storage` (`crates/knixx-storage/src/lib.rs`)
mounts the littlefs, does its work and unmounts: `Lfs::with_mounted`. For a
streamed file that is costly at each `append`:
- **the mount** reads the superblock and the directory pairs;
- **the first allocation** after a mount scans the whole filesystem
  (`lfs_fs_traverse`, which walks every file's block list). So it grows
  with the file being staged;
- **the reopen** with O_APPEND goes back to the file's last block; if that
  block is partly filled, littlefs copies it to a new block;
- **the close** commits the metadata.

The result, measured in the retrospective (point 11): staging tops out at
about 27 KiB/s and slows down as the file grows. `bolt fw push` from a Mac
reaches 17 KiB/s, so the 790 KiB firmware takes 45 s (README).

Two workarounds exist today:
- the `Stager` buffers 16 KiB in RAM (`stage::PIECE`) to append less often;
- scripts get no buffer at all: `AppSink::write` appends each BLE write of
  about 240 bytes, with a mount each time.

**The goal:** the app mounts littlefs once at boot and keeps it mounted.
A file being streamed stays open between appends.

**The proof:** a first `cargo xtask hil` (retrospective point 8) measures
the throughput before and after, on the Feather.

### Decisions (user)

- **Mount once at boot** for the whole app: staging, scripts, bonds, config.
  The bootloader keeps its mount-per-call functions.
- **Build a first `cargo xtask hil`:**
  - it flashes, then checks pushes and pulls through bolt;
  - it prints the throughput;
  - it runs on main first for the baseline, then on the change.

## Design

### 1. `knixx-storage`: a `Session`

**The type.** A new `pub struct Session<D: BlockDevice>` owns:
- the device;
- `lfs_t` and `lfs_config`;
- the `Buffers`, plus a second file buffer for the stream;
- an optional **stream:** one open `lfs_file_t`, with its path (a `CPath`).

It holds pointers into itself, so it is `!Unpin` (`PhantomPinned`) and used
through `Pin<&mut Session<D>>`.

**The methods:**
- `new(dev)`, then `mount()`, and `load(buf, log)`. `load` is the boot
  path that exists today: format a blank flash, write the default
  `knixx.ini`, read it. It leaves the session mounted.
- **The whole-file operations:** `read`, `write_atomic`, `save`, `remove`,
  `mkdir`, `create`, `append`, `read_at`, `rename`, `free`, `stat` and
  `list`. They keep their meaning; only the mount goes.
- **`append(path, data)`:**
  - when `path` is the open stream, it writes and returns the new size;
  - otherwise it closes any stream, opens `path` with O_WRONLY | O_APPEND,
    and keeps it open as the stream.
- **`create(path)`** also closes a stream on that path first.
- **A stream on its own path gets closed first:**
  - by `remove`, `rename`, `read_at`, `read` and `stat`, so they see the
    file whole;
  - by `write_atomic`, when the path or its `.tmp` is the stream.

  Other paths leave the stream open: littlefs keeps open files consistent
  across other commits (`lfs->mlist`).
- **`close_stream()`** commits the stream, and is public.
- **An error on the stream** closes it (any close error is ignored), so a
  failed append does not leave a half-open handle.

**What a reset costs:** what was appended since the stream opened is lost,
since littlefs commits on close or sync. Nothing depends on it:
- `Stager::begin` removes `STAGED_TMP` and starts over;
- the script sink does the same with its `.tmp`;
- resume after a lost BLE link keeps the stream open (same boot).

The `append` doc comment, which promises survival over a reset, is
corrected.

**The free functions stay, for the bootloader** (`knixx-boot/src/install.rs`
uses `stat` and `read_at`) and for the host tests:
- each becomes a short session on the stack: `pin!(Session::new(dev))`,
  `mount`, the call, then drop, which unmounts;
- a blanket `impl BlockDevice for &mut D` lets the session borrow the
  device;
- so there is one code path. `Lfs` and `with_mounted` go;
- `xtask build` checks that the bootloader still fits.

### 2. Firmware: `firmware/knixx-fw/src/storage.rs`

- **The static:** `FLASH` and `MOUNTED` become one
  `static SESSION: Mutex<ThreadModeRawMutex, RefCell<Option<Pin<&'static mut Session<QspiFlash>>>>>`.
  - The session lives in a `StaticCell` (so it stays in place), is set up
    by `load()` and stored there once mounted.
  - A flash that does not mount leaves it `None`, so everything answers
    `Unavailable`, as today.
- **`with_flash` hands out the session.** `save`, `read_file`,
  `write_file`, `make_dir`, `remove_file`, `list_dir`, `file_size` and
  `Fs` (`knixx_image::stage::Files`) call its methods. Their signatures do
  not change, so `bonds.rs`, `script.rs`, `runner.rs`, `fw.rs`,
  `fw_knx.rs`, `console.rs` and `xfer_gatt.rs` are untouched, apart from
  the counters below.
- **RAM:** about 2.6 KiB more statics: the buffers, `lfs_t`, and the stream
  buffer and handle. Today they sit on the stack for each call.

### 3. The stager: `crates/knixx-image/src/stage.rs`

- `PIECE` drops from 16 KiB to 4 KiB: littlefs's own 512-byte cache now
  programs page by page. This saves 12 KiB of RAM: the `Stager` lives in
  the `XFER` static.
- `finish` already flushes, then reads back via `read_at`, which closes the
  stream first, then renames.
- The `Files` trait does not change. `stage_tests.rs` keeps passing.
- The doc comment on `PIECE` changes.
- The HIL measures both values, 4 KiB and 16 KiB: if 16 KiB is no faster,
  4 KiB stays.

### 4. Board-side timing, to tell the flash from the radio

In `xfer_gatt.rs`, the receiving side keeps for the last push:
- the bytes;
- the time from `open` to `finish`;
- the time spent inside `Sink::write` and `finish`, summed with
  `Instant`.

`upload`, with no arguments (`fw.rs::upload_command` → `window_status`),
prints one more line:
`last: firmware 789676 B in 12.3 s, 4.1 s in flash`.

The HIL reads it, so both rates show:
- end to end (bolt's KiB/s);
- the flash alone.

### 5. `cargo xtask hil` (new `xtask/src/hil.rs`)

**Usage:** `cargo xtask hil [--port <serial>] [--no-flash] [--probe <bmp port>] [--piece-sweep]`.

**The steps:**
1. **Build and flash.** `--probe` uses `flash_probe`, else the existing
   USB `flash()`. Check that `version` reports the build. `--no-flash`
   skips this step.
2. **A higher counter.** Re-sign the same build as `build/hil.kfw` with
   counter = now, through the signing path that `xtask build` uses
   (`knixx_host::sign`, `Image`), so that the stager takes it.
3. **The window:** `knixx_host::usbserial::query(port, "upload window 10")`.
4. **Firmware:**
   - run `target/debug/bolt fw push build/hil.kfw` as a child process (the
     real CLI), and parse its closing `sent N KiB in T s (R KiB/s)`;
   - check on the console: `fw status` shows the staged counter, and
     `upload` gives the board-side line;
   - then `fw discard`.
5. **A script:**
   - generate a `.rhai` holding a long string constant, so that its
     bytecode is about 12 KiB, in a temporary directory;
   - `bolt script push` (time it); check `script ls`;
   - `bolt script pull <name>` into the temporary directory, and compare
     the bytes;
   - remove it on the console.
6. **Config, a round trip:**
   - `bolt config pull` into the temporary directory, then
     `bolt config push` of that file (it must be accepted);
   - then pull again: the bytes must be identical.
7. **A table:** pass or fail per step, and for each push the bytes,
   seconds, KiB/s end to end, and KiB/s in flash. It is appended to
   `build/hil.log` with the commit, so runs compare.

   The window is shut at the end, and on failure.

`--piece-sweep` (optional) repeats step 4 for each `PIECE` build. It
rebuilds through a `KNIXX_PIECE` env override that is read by `stage.rs`
via `option_env!`, debug-only. **Or simpler:** I run the two builds by hand
and drop the flag. I'll take the simpler one unless the sweep turns out to
be needed.

**Preconditions,** printed when not met:
- the Feather is on USB;
- the Mac can reach it over BLE: `ble.pair` off, or a bond. The memory says
  the Feather has 0 bonds; the HIL says so and stops.

## Phases (each a commit, checked on the Feather)

1. **`cargo xtask hil` on current main.** Run it, and record the baseline
   (fw and script KiB/s, board-side flash time) in the commit message.
2. **`Session` in knixx-storage,** with the free functions on top of it,
   and host tests:
   - the existing `tests.rs` passes unchanged;
   - new tests:
     - a stream of 800 KiB appended in 240-byte pieces matches the data
       read back;
     - interleaved `write_atomic` of another file, and `mkdir`, while the
       stream is open;
     - `remove`, `rename` and `read_at` on the stream's path;
     - dropping the session mid-stream, then remounting: the filesystem
       mounts, and the file has its last committed size;
     - **a cost test:** `RamFlash` counts the bytes read, programmed and
       erased. For the session, the bytes read per append stay constant as
       the file grows; for mount-per-append, they grow. It prints both
       totals for the 800 KiB stream.
3. **The firmware on the session** (`storage.rs`), `PIECE` at 4 KiB, the
   board-side timing and the `upload` line. Run the HIL and compare.
4. **README and docs:**
   - the speed line (README:406);
   - the retrospective's point 11, which stays uncommitted;
   - `plan/littlefs-session.md` with the before and after numbers.

## Critical files

- `crates/knixx-storage/src/lib.rs`, `tests.rs`: `Session`, the free
  functions on top of it, the counting `RamFlash`.
- `firmware/knixx-fw/src/storage.rs`: the static session.
- `crates/knixx-image/src/stage.rs`: `PIECE`.
- `firmware/knixx-fw/src/xfer_gatt.rs`, `fw.rs`: the timing and the
  `upload` line.
- `xtask/src/main.rs` (the `hil` entry), new `xtask/src/hil.rs`. It reuses
  `flash`, `flash_probe`, the signing path and `knixx_host::usbserial::query`.
- `firmware/knixx-boot`: unchanged; its size is checked.

## Verification

- `cargo test` (knixx-storage, knixx-image), `cargo clippy`, and
  `cargo xtask build`:
  - the bootloader fits;
  - the app's statics are about 9 KiB smaller (−12 KiB for `PIECE`,
    +2.6 KiB for the session);
  - the no_std CI loop builds knixx-storage for thumbv7em.
- `cargo xtask hil` on main (the baseline), then on the change. Every step
  passes, with the numbers side by side.
  - **Expected:** the flash share of the firmware push drops from most of
    the time to a small part, and the end-to-end KiB/s rises well above
    17, up to whatever the BLE link from the Mac allows (the flash itself
    is good for about 60 KiB/s: a 4 KiB erase is about 50 ms).
  - The script push should gain the most, since it mounted per 240 bytes.
- **By hand, once:**
  - `fw install` of a HIL-staged image boots;
  - a reset in the middle of a `fw push`, then a new push, works;
  - `config set` and the bonds still save while no transfer runs, and
    during a `fw push` (a second open file next to the stream).
