# Rhai Grain scripting for the knixx thunder firmware

> **Status: completed.** The implementation starts with commit `d83f096`
> (2026-11-01, "phase 1 of scripting: a heap for Rhai Grain only"); phases 1
> to 7 run through `0c5c160`, followed by `8c0edb7` (the callbacks became
> methods of the script's state). Released in v0.2.0 and v0.3.0. The phase 0
> spike has no commit of its own: its numbers are in the plan below.

## Context
The thunder firmware (knixx-rs: embassy, nRF52840, no heap) should run user logic without reflashing: scripts written on the host from a template, compiled to Rhai Grain bytecode, uploaded with bolt into littlefs `scripts/*.rhgn`, and called by the firmware on every received telegram and every second, able to send telegrams. Requirements: Rhai Grain VM with a minimal feature set; per-script scope persisting across calls; `on_info()`, `on_init(scope)`, `on_telegram(scope, tg)`, `on_time(scope, uptime)`; scripts call the Rust send function; CLI list/info/rm.

User decisions: heap allocations are allowed in the firmware **only for Rhai Grain**; `on_telegram` gets every telegram, the board's own echoes too, with `tg.own = true`; numbers are `only_i32` + `f32_float` (hardware single precision on the M4F).

Rhai Grain facts (rhai 1.27, `grain` feature, experimental, merged Aug 2026): host `Engine::compile` → `grain::Compiler::new().compile(&ast)` → `Program` (`residual_count()` must be 0) → `write_stripped()` = artifact + host-side sidecar. Device: `Program::read(&bytes)` (verifies; ABI fingerprint covers integer widths and restriction features), `into_owned()`, `Vm::new(&engine)`, `Vm::call_fn_with_options(opts, &mut scope, &program, name, args)`, `fault_trace()`. `no_ast` drops parser/AST for a bytecode-only device. `no_std` pulls core-error, hashbrown, libm, no-std-compat and needs `alloc`. Unverified: Grain under `no_std` on stable, thumbv7em code size, writability of programs with script functions — hence the spike.

## Design

**Rhai features (one list in `crates/knixx-script/Cargo.toml`, shared by host and device so the ABI matches):** `grain, only_i32, f32_float, no_module, no_custom_syntax, no_time, no_position` (+ device: `no_std, no_ast`; host: `std`). Kept: functions, arrays/blobs, object maps, closures only if the shared-scope option wins. Not `unchecked`: limits are the safety net.

**Heap, for Rhai only** (`firmware/knixx-fw/src/heap.rs`): `rlsf` TLSF on a static 48 KiB (sized by the spike), no lock. The `#[global_allocator]` wrapper serves only in thread mode while the script task holds a `Grant`; otherwise it panics with "alloc outside Rhai Grain". All VM work, loads and drops run under the grant. Firmware feature `scripts` (default on); without it the build keeps today's no-allocator check. `xtask build` reports the heap; `sys` shows heap in use and peak. Everything outside `knixx-script` and the script task stays `no_std` without `alloc`.

**Spike results (phase 0, on the board):** engine 2.8 ms / 9 KB heap; 844 B artifact loads in 2.9 ms / 2.7 KB; on_telegram 0.63 ms incl. a 0.15 ms telegram map; VM ≈ 60 ops/ms; heap peak 15.5 KB, all freed; stack high-water 9.9 KB; flash 634 KB without the iterator package (686 KB with). Rhai at opt-level 3 does not fit. Grain 1.27 bug: `eval_ast(false)` + `rewind_scope(false)` turns script errors into a panic — use `eval_ast(false)` only (state lives in the shared map).

**Decisions after the spike:** go; no `for` loops (no iterator package, bolt rejects `for` via `disable_symbol`), `while`/`loop` only; device engine is `Engine::new_raw()` plus only what scripts need.

**Limits:** `max_operations` = 1,200 so a call stays ≤ ~20 ms; `max_call_levels` 8; max string/array/map/blob sizes so the worst case fits the heap. OOM or a Rhai panic resets the board (no unwinding), so a boot guard records the running script id in the `.uninit` panic record; the next boot starts with that script disabled and says so in the banner. A script that errors N times in a row is disabled.

**Scripts:** up to 4 slots. Each slot owns its `Program<'static>` (read from the file into a heap buffer, then `into_owned()`), its state and its cached info. Script API:
- `fn on_info()` → map `#{ name, author, date, compiled, version, hash }`. bolt fills the last three at transpile time by replacing placeholders in the template (`{{compiled}}`, `{{version}}`, `{{hash}}`): `compiled` the UTC build time, `version` the short commit hash of the script's git repo (`-dirty` when the file differs from it, `unversioned` outside a repo), `hash` the md5 of the original script source (before substitution). `bolt script build` checks that on_info returns all six keys with the filled values.
- `fn on_init(scope)`, `fn on_telegram(scope, tg)`, `fn on_time(scope, uptime)`. `scope` is a per-slot object map that persists; the spike picks how it is passed: shared map argument (`scope.x = 1` writes through), else `this`-bound, else returned map.
- `tg`: a read-only `Tg` type (or map, spike decides) with `src`, `dest`, `group` (bool), `apci` ("write"/"read"/"response"/…), `priority`, `hops`, `data` (blob), `own`, plus `bool()`, `f16()` decoders.
- Natives: `knx_write(ga, blob)`, `knx_read(ga)`, `knx_response(ga, blob)`, `dpt1(bool)`, `dpt9(f32)` encoders, `ga("1/2/3")`, `print`/`debug` → console. Sends return true/false; a token bucket (10/s, 4 per call) stops feedback loops on own echoes.

**Bus** (`crates/knixx-bus`): `Receiver::handle` unpacks every L_Data.ind (not only while sniffing) and calls a new `Output::telegram(&tg, own)` (default no-op; own = `src == dev.individual`). `Command::Send(Telegram)` (stays `Copy`) with a small raw queue in the engine (not a bigger `MAX_DATA`); the engine forces `src` to the device address and group destinations only. Scripts use their own `SCRIPT_CMDS` channel (4), served after `CMDS`, so scripts never crowd out the switches. UART RX ring 256 B → 1 KiB (~267 ms of bus time) to ride out VM calls and flash writes.

**Storage** (`crates/knixx-storage`): public `read(path, buf)`, `write_atomic(path, data)` (`.tmp` + `lfs_rename`), `remove`, `mkdir`, `list(dir, |name, size|)`, a NUL-terminated `CPath<64>`; whole-file only. `/knixx.ini` handling unchanged.

**CLI** (firmware commands): `script list` (name, size, state, errors), `script info <name>` (cached on_info, heap, last error, fault trace), `script rm <name>`, and `script put <name> <size> <crc32>`. `put` returns a new `Action::Upload`: the console then routes raw bytes into an upload state machine — no editor, echo or history — taking chunks of ≤ 192 bytes, answering `ok <offset>` after each (paces BLE, whose 256 B RX ring has no back-pressure), checking the CRC at the end, aborting after 5 s idle. It stages into a static 16 KiB buffer (not the heap), writes once with `write_atomic`, then signals the script task to (re)load. Info/list read a plain-data `STATUS` table that the script task fills (Rhai values are `!Send`); rm/reload go to the task by channel.

**Host** (`host/bolt`, using `knixx-script` with `std`): `bolt script new <name>` writes the template (`host/bolt/templates/script.rhai`, documented, with examples); `bolt script build <file.rhai>` compiles, checks residuals, the four functions and their arities, runs `on_info` on the host, writes `<name>.rhgn` and the sidecar; `bolt script upload <file> [--port|--ble|--addr]` builds if given source and runs the put protocol over the term.rs `Conn` (USB or NUS), waiting for each ack; `bolt script ls|info|rm` wrap the console commands, and info maps fault addresses to source lines with the sidecar.

## Phases (each verifiable, committed after the user's checks)
0. **Spike (go/no-go).** Rhai with the device features builds for thumbv7em on the pinned stable toolchain; a host-compiled artifact (`include_bytes!`) runs on the board under the heap. Measure flash, heap peak, stack, ops/ms, read/verify cost; check script functions are writable, host↔device ABI match, the scope options, Tg vs map. *Exit:* a numbers table; flash growth ≤ ~250 KB, heap ≤ 48 KiB, worst call ≤ 20 ms — else stop and rethink.
1. **Heap infrastructure:** heap.rs, grant, `scripts` feature, panic marker, xtask/image.rs checks. *Exit:* default checks unchanged; alloc without grant panics clearly.
2. **Bus hooks:** telegram hook, `Command::Send`, `SCRIPT_CMDS`, 1 KiB ring. *Exit:* knixx-bus tests (own/foreign, raw send, src forced); sniffer golden strings unchanged.
3. **Storage API.** *Exit:* host tests incl. the Go littlefs image still mounting.
4. **knixx-script crate** + template. *Exit:* host tests compile the template, round-trip write/read, call all four functions, record sends.
5. **Upload and CLI** + bolt script commands. *Exit:* 16 KiB upload over USB and BLE with CRC; history untouched; timeout aborts.
6. **Script task:** boot load, on_init, telegram/1 s/control select, STATUS, errors, boot guard. *Exit:* demo script toggles a group on a switch telegram; an endless loop is cut off; a forced OOM reboots with the script disabled.
7. **Hardening and docs:** sys heap/drop counters, token bucket, fault decoding, README.

## Critical files
`crates/knixx-bus/src/lib.rs` (Receiver::handle, Output, Command, run), `firmware/knixx-fw/src/{console.rs,bus.rs,main.rs,panic.rs}` + new `heap.rs`, `script.rs`, `crates/knixx-storage/src/lib.rs` (Lfs helpers), `crates/knixx-cli/src/lib.rs` (Action), new `crates/knixx-script/`, `host/bolt/src/{main.rs,term.rs}` + new `script.rs`, `host/knixx-host/src/image.rs`, `xtask/src/main.rs`, README.

Reuse: `Telegram`/`unpack`/`pack_to` (knixx-knx tp1.rs), `group_write` (knixx-device thunder.rs), `f16_write`/`f16` (knixx-bus format.rs, made public), `console::write_crlf`, `leds::signal`, term.rs `Conn` + `ble::connect`, `usbserial::open`, the panic `.uninit` record, stack.rs painting.

## Verification
- Host: `cargo test` (bus, storage, script crate incl. template round trip), clippy/fmt, `cargo xtask build` with heap report and allocator checks for both feature sets.
- Board, per phase: spike numbers; `script put` over USB and BLE; `script list/info/rm`; demo script reacting to a wall switch telegram and to `on_time`; sniffer shows script sends with positive L_Data.con; endless loop and OOM guards; `sys` heap peak stable over hours.
