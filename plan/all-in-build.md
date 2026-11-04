# One all-in firmware build: thunder

> **Status: completed.** The implementation starts with commit `175b341`
> (2026-11-04, "one build with every feature in, named thunder"), followed by
> `7ae0da7` (CI and release) and the README. Measured: the full build takes
> 743 972 bytes (91%) and 116 KiB of statics; `--no-scripts` 486 KB,
> `--no-web-ble` 738 KB, `--no-coded-phy` 722 KB, all three 458 KB with no
> allocator. Flashed on the board over USB: it kept `ble.knixx` and
> `ble.coded` on, and `ble` showed both as built in.

## Context

The firmware has three Cargo features: `scripts` (on by default), and
`web-ble` and `coded-phy` (off by default, switched on with `--web-ble` and
`--coded-phy`). As a result, every release carries three builds of the
firmware (`knixx.*`, `knixx-web-ble.*`, `knixx-web-ble-coded.*`, 12 files),
and CI builds five combinations. Users have to pick the right file. The full
build fits (742 KB, 91% of the app flash; RAM statics 116 KiB). Also, its
runtime defaults (`ble.knixx = off`, `ble.coded = off`) make it behave like
the plain build until the user switches a key on.

The goal is one build with every feature, which is the default, and
`--no-…` flags that take a feature out. Its output files are named
`thunder.*`. CI and the release build only that build.

## Design

### 1. Features on by default (`firmware/knixx-fw/Cargo.toml`)

- `default = ["scripts", "web-ble", "coded-phy"]`. Update the comments so
  they say each feature is on by default and which `--no-…` flag removes it.
- The `#[cfg(feature = …)]` code in `firmware/knixx-fw/src` stays as it is.
  The features still exist, they only flip their default.

### 2. xtask flags reversed (`xtask/src/main.rs`)

- In `Features::take`, all three fields start `true`.
- The flags are `--no-scripts`, `--no-web-ble` and `--no-coded-phy`. These
  follow the existing `--no-scripts` form, rather than `-no_…` as written in
  the request.
- `--web-ble` and `--coded-phy` are no longer accepted. They get a clear
  error, e.g. "web-ble is built in by default; --no-web-ble leaves it out",
  so that old commands don't fail silently.
- `build_with`:
  - with all three features on, it runs a plain `cargo build --release`;
  - otherwise it runs `--no-default-features --features <the ones still on>`.
  - The heap-rule checks keep using `f.scripts`.
- Output files: `build/thunder.elf`, `thunder.uf2`, `thunder.hex` and
  `thunder-dfu.zip`. The DFU zip's inner name changes from `"knixx"` to
  `"thunder"` (`Package::write_zip(..., "thunder", ...)`), giving
  `thunder.bin` and `thunder.dat` inside.
- Update the size line, the `ota` zip path (line 108), the usage text and
  the module doc (lines 4, 39–47, 97).
- Name the artifact in one place, `const ARTIFACT: &str = "thunder";`, so the
  paths can't drift apart.

### 3. Host defaults pointing at the build

- `host/bolt/src/main.rs:78,125`: change the defaults to
  `build/thunder-dfu.zip` and `build/thunder.uf2`.
- `host/knixx-host/src/image.rs:326` and `legacydfu/genpkg.rs:298`: these
  tests read `build/knixx.elf` and `.hex`, which become `thunder.*`.
  - The genpkg `write_zip` test's name `"knixx"` can stay, since it is an
    arbitrary name.

### 4. Console messages (`firmware/knixx-fw/src/console.rs:194,197`)

The hints change to say how the feature was left out:

- "the knixx service is not built in (a --no-web-ble build)"
- "coded PHY is not built in (a --no-coded-phy build)"

"not built in" in `ble` stays, and so does the knixx-cli test.

### 5. CI (`.github/workflows/ci.yml`)

- **Lint:** keep `cargo clippy --all-targets` for the host. The firmware is
  linted only with `(cd firmware && cargo clippy --release -- -D warnings)`,
  which now covers the all-in build. Drop the `--features web-ble` and
  `web-ble,coded-phy` lines.
- **Firmware build and size:** only `cargo xtask build`.
- `cargo bloat` already uses the default features, so it now measures
  all-in. The other steps (host tests, no_std crates, web app, deny) are
  unchanged.
- **Caveat:** CI no longer compiles the `--no-…` combinations. A cfg mistake
  in a stripped build would only show when someone builds it.

### 6. Release (`.github/workflows/release.yml`)

- **Firmware job:**
  - one step, `cargo xtask build`;
  - drop `keep_as` and the `grep` guards;
  - upload `build/thunder.elf`, `.uf2`, `.hex` and `build/thunder-dfu.zip`.
- **Assets:** a release then has 4 firmware files plus the 3 bolt tarballs,
  7 in all.
- **Rerunning an old tag:** a manual `workflow_dispatch` of an older tag
  would build `knixx.*`, which the new upload list misses. Releases before
  this change already have their files, so only document in the comment
  that the workflow applies from this tag on.

### 7. README

- **Build section (lines 132–176):**
  - outputs are `build/thunder.*`;
  - the full build is the default at 742 KB and 91%;
  - `--no-scripts`, `--no-web-ble` and `--no-coded-phy` remove features,
    with the sizes each saves (Rhai ~165 KB, web-ble ~6 KB, coded ~22 KB);
  - RAM is 116 KiB by default.
- **From a release (lines 210–223):** one row each for `thunder.uf2`,
  `thunder-dfu.zip` and `thunder.hex`/`.elf`, then bolt; `bolt uf2
  thunder.uf2`. Drop the web-ble rows.
- **Radio (line 499):** coded comes with the default firmware, so only
  `config set ble.coded on` is needed.
- **Web app setup (line 798):** step 1 becomes "the default firmware has the
  service; `config set ble.knixx on`".
- Also update the `ota` paths (lines 268, 282).

### 8. Plan record

Save this plan as `plan/all-in-build.md`. Mark it completed with its first
commit, as for the earlier plans.

## Order of work (one commit each)

1. Firmware + xtask + host defaults + console hints: `[fw|xtask|host]`.
2. CI and release workflows: `[ci]`.
3. README: `[readme]`.
4. `plan/all-in-build.md`: `[plan]`.

## Verification

- `cargo test` passes, and `cargo clippy --all-targets -- -D warnings` too.
- In `firmware/`:
  - `cargo clippy --release -- -D warnings`;
  - `cargo fmt --check`.
- `cargo xtask build` writes the four `build/thunder.*` files, reports about
  742 KB (91%) and passes the heap rule. `unzip -l build/thunder-dfu.zip`
  shows `thunder.bin` and `thunder.dat`.
- Each stripped build still compiles and fits: `cargo xtask build
  --no-scripts`, `--no-web-ble`, `--no-coded-phy`, and all three together
  (the last must link no allocator).
- `cargo xtask build --web-ble` errors with the hint.
- Flash the all-in build to the USB board (`cargo xtask flash`) and check
  on its console:
  - `ble` shows the knixx service and coded as off, not "not built in";
  - `config set ble.knixx on` and `ble.coded on` are accepted;
  - the web app connects.
  - Then restore the board's previous settings (`ble.coded on`).
- After the push, CI is green with the single build. The next tag's release
  carries 7 files.
