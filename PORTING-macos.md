# Porting npc to macOS

Written for someone comfortable on macOS but new to porting Rust. The short
version: **one file needs real work, two scripts need small fixes, everything
else already compiles.**

Rust is not the hard part here. Cargo cross-compiles cleanly, and every
dependency except one is portable. The work is in the places where this code
reaches past Rust into the operating system — and there are only three.

---

## Scope

| | status |
|---|---|
| Rust dependencies | portable, nothing to do |
| SQLite | vendored and compiled from source by `rusqlite` — nothing to install |
| The TUI (ratatui/crossterm) | works; terminal graphics are better on macOS than Linux |
| The GUI (`npcg`, egui/eframe/wgpu) | portable; wgpu targets Metal natively, no protocol to negotiate at all |
| The background watcher (`notify`) | portable (FSEvents backend) but **verify, don't assume** — see §8 |
| **Exiv2** | needs installing, and `pkg-config` needs to find it |
| **`src/vol.rs`** | **the real work** — 74 of its 167 lines are Linux-only |
| `check`, `fetch-test-data` | two GNU-only commands to replace |
| The C++ shim | portable; `timegm` exists on BSD |

Budget: **half a day**, most of it on `vol.rs` and its verification.

---

## 1. Prerequisites

```sh
xcode-select --install                    # Apple Clang, for the C++ shim
brew install rust exiv2 pkg-config        # or rustup, if you prefer
```

Confirm pkg-config can see Exiv2 — the build fails immediately if it cannot:

```sh
pkg-config --modversion exiv2             # expect 0.27.x or 0.28.x
pkg-config --cflags --libs exiv2
```

If that fails on Apple Silicon, Homebrew is in `/opt/homebrew` rather than
`/usr/local` and pkg-config may not be looking there:

```sh
export PKG_CONFIG_PATH=/opt/homebrew/lib/pkgconfig:$PKG_CONFIG_PATH
```

Also confirm Exiv2 has XMP support, because a build without it passes every
test that does not touch sidecars and *then destroys metadata in the field*:

```sh
exiv2 --verbose --version | grep xmptoolkit    # expect have_xmptoolkit=1
```

### A terminal that can draw images

Thumbnails need a terminal graphics protocol. This is the one area where macOS
is *better served* than Linux — several good options, all natively supported.

| terminal | install | protocol |
|---|---|---|
| **kitty** | `brew install --cask kitty` | kitty graphics — the reference implementation |
| **Ghostty** | `brew install --cask ghostty` | kitty graphics; macOS-native, arguably the best fit |
| **WezTerm** | `brew install --cask wezterm` | kitty + iTerm2 + sixel |
| **iTerm2** | `brew install --cask iterm2` | its own inline-image protocol, which `ratatui-image` speaks |
| Terminal.app | preinstalled | **none** — thumbnails degrade to unreadable half-blocks |

kitty is a *cask* rather than a formula, because on macOS it ships as an app
bundle. Nothing about the thumbnail path needs porting; it is the same code
negotiating the same protocol.

**Run `npc term` before judging thumbnail quality.** It reports which protocol
was actually negotiated, the cell size in pixels, and the resulting tile size.

This is not a formality. On the Linux machine this was built on, three rounds
went into tuning thumbnail resolution before anyone discovered the program was
not running inside kitty at all — kitty was installed, but the TUI had been
launched from a terminal with no image support, and the half-block fallback
looks exactly like a resolution problem. `npc term` answers in one line what
took hours to work out.

## 2. First build

```sh
git clone <repo> && cd npc
cargo build --release
```

Expect this to succeed. If it fails it will be in `build.rs`, and it will be
about Exiv2 — the error names the package. Nothing else in the dependency tree
is platform-sensitive.

Then:

```sh
cargo test --release --lib
```

Most of this passes. Two tests in `vol` will fail, which is the whole port.

## 3. The actual work: `src/vol.rs`

This file answers one question — *which physical volume is this path on, and
what is its stable identifier?* — and answers it in a thoroughly Linux way.

The identifier matters because the catalog is named after it. Get it wrong and
a drive is not recognised as the same drive across replugs, which silently
creates a second catalog.

### What is Linux-specific

| function | what it does | macOS equivalent |
|---|---|---|
| `mount_for_dev` | parses `/proc/self/mountinfo` for the mount point and backing device | `statfs()` gives both directly |
| `lookup_uuid` / `lookup_label` | scans `/dev/disk/by-uuid` and `/dev/disk/by-label` symlinks | DiskArbitration framework, or `diskutil info -plist` |
| `unescape` | mountinfo escapes spaces as octal | not needed; delete or keep unused |
| `identify` | uses `rustix::fs::major`/`minor` on `st_dev` | `statfs` makes this unnecessary |

`mount_point_by_walking` — walking up until `st_dev` changes — is portable and
already the fallback. It works on macOS as-is.

### The macOS shape

`statfs(2)` gives you everything the mountinfo parsing was for:

- `f_mntonname` — the mount point (`/Volumes/MyPics`)
- `f_mntfromname` — the device (`/dev/disk4s2`)
- `f_fstypename` — the filesystem (`exfat`, `apfs`, `hfs`)

In Rust, `rustix::fs::statfs()` exposes this without an extra dependency —
`rustix` is already in `Cargo.toml`. That replaces ~50 lines with about 10.

The volume **UUID** is the one thing `statfs` does not give you. Two routes:

1. **Shell out to `diskutil`** — simplest, no new dependencies:
   ```sh
   diskutil info -plist /dev/disk4s2      # look for VolumeUUID
   ```
   Ugly, but this runs once per command, not per file. Parse with `plist` or
   grep for the key.

2. **DiskArbitration framework** — the correct way, via the
   `core-foundation` and `disk-arbitration` crates. More code, more
   dependencies, no user-visible difference.

Start with `diskutil`. It is honest about being a stopgap and can be replaced
later without touching any caller.

**A trap specific to your library:** exFAT volumes do not have a real UUID.
macOS reports the volume serial, just as Linux did — the existing fallback
(`dev:MAJOR:MINOR`, with `stable: false`) already covers the case where none is
found. Keep that path and keep the flag; `npc volume` prints a warning when it
fires, and that warning is the difference between a confusing bug and an
obvious one.

### How to structure it

Do **not** litter call sites with `#[cfg]`. Five files call `vol::identify` and
none of them should care what OS they are on. Keep the public API exactly as
it is:

```rust
pub struct Volume { uuid, label, mount, device, stable }
pub fn identify(path: &Path) -> Result<Volume>
```

and split only the private half:

```rust
#[cfg(target_os = "linux")]
mod imp { /* the existing mountinfo + /dev/disk code */ }

#[cfg(target_os = "macos")]
mod imp { /* statfs + diskutil */ }
```

`identify` then calls `imp::resolve(path)`. Everything else compiles unchanged.

### Verifying it

This is the part that matters, and the part where this project has been bitten
before. Three checks, in order:

```sh
npc volume /Volumes/SomeDrive     # UUID, label, device, mount point
npc scan /Volumes/SomeDrive/…     # builds a catalog
npc status /Volumes/SomeDrive/…   # same volume UUID on a second run
```

Then the one that actually proves it — **eject the drive and run the same
commands**. The offline path is resolved by a `last_root` recorded in the
catalog and needs no volume at all, so this must keep working:

```sh
npc find '' --root /Volumes/SomeDrive/…    # OFFLINE banner, results from cache
```

On Linux this exact path was broken from the first line of code and nobody
noticed for seven hours, because no test can unplug a drive. Do not skip it.

## 4. The two script fixes

`check` line 65 uses GNU `find -printf`, which BSD find does not have:

```sh
# Linux
find "$LIBRARY" -type f -printf '%s\t%T@\t%p\n'
# portable
find "$LIBRARY" -type f -exec stat -f '%z%t%m%t%N' {} +     # BSD stat
```

`fetch-test-data` line 50 uses `sha256sum`; macOS has `shasum`:

```sh
sha256sum file | cut -d' ' -f1        # GNU
shasum -a 256 file | cut -d' ' -f1    # macOS
```

Both are worth making portable rather than duplicating — detect once at the
top of each script and set a variable.

## 5. Things that will differ but are not bugs

- **Config and catalog locations move.** The `directories` crate follows
  platform convention, so on macOS these become
  `~/Library/Application Support/npc/` and `~/Library/Caches/npc/` rather than
  `~/.config` and `~/.cache`. `catalog_dir` in the config overrides both if
  you would rather they lived somewhere familiar.
- **Removable drives mount under `/Volumes/`**, not `/media/<user>/`. Only
  relevant to what you write in `protected_roots`.
- **Case-insensitive filesystem — a risk, but not one this library runs.**
  APFS is case-insensitive by default, and this tool treats `.JPG` and `.jpg`
  as different things: out-of-camera versus extracted preview. If a shot ever
  had both `X.JPG` and `X.jpg`, copying it to APFS would silently merge them.

  Measured before writing this down: **zero** case-only collisions across the
  full 41 GB library, and **zero** stems appearing as both `.JPG` and `.jpg`.
  The naming convention prevents it — derived files are
  `X_NEF_embedded.jpg`, so the stems always differ. Nothing to do.

  Worth knowing only if that convention changes. A `doctor` check for
  case-colliding names would be cheap insurance if it ever does.

## 6. Checking the thumbnails

Once `vol.rs` is done and a library scans, verify the display path
independently of it:

```sh
npc term                      # which protocol, cell size, resulting tile size
npc thumbs                    # pre-warm; reports throughput
npc tui                       # grid view, run inside one of the terminals above
```

Expect the tile sizing to want retuning. `grid_px` and `view_px` in the config
were derived from a 3440x1440 display; on a Retina panel the pixel-per-cell
ratio is different and the defaults will be conservative. `-` and `+` change
the number of tiles across while the TUI is running, which is the fastest way
to find the right value before writing it into the config.

`view_px` should match the preview tier your camera embeds — 1620 for a D750.
Asking for more forces a full-resolution decode per frame, which is a 6x
slowdown for no visible gain.

## 7. The GUI (`npcg`)

Nothing here needs porting the way `vol.rs` does — `npcg` draws its own
window via `wgpu`, which targets Metal on macOS directly, so there is no
kitty/sixel-style protocol negotiation to redo and no equivalent of
`npc term` to run first. Once `vol.rs` (§3) is ported, `npcg` gets it for
free: it calls the same `vol::identify`, the same `action.rs`, the same
`Config::path()`. Two things specific to it are worth checking rather than
assuming, in the spirit of §3's own "verify it" section:

- **The background watcher's behavior under FSEvents.** `watch.rs`'s
  directory-creation race fix (arming a rescan on `EventKind::Create
  (CreateKind::Folder)` for the directory *itself*, not its parent — see
  the doc comment on `qualifying_dir`) was confirmed against `notify`'s
  **inotify** backend specifically, by reading its source and by a
  dedicated integration test. FSEvents coalesces and delays events
  differently (it is directory-granularity by design, and can report a
  batch of changes well after they happened), so that specific test
  (`a_subdirectory_created_after_the_watcher_starts_is_still_caught`) needs
  rerunning on macOS, not assumed to pass because it passes on Linux — this
  project has been bitten before by assuming one platform's event semantics
  apply to another's (see `vol.rs`'s own history in §3, and CLAUDE.md's
  "measure rather than reason about performance").
- **The default `Open` action's `xdg-open` is Linux-only.** macOS has no
  `xdg-open`; the equivalent is `open`. `action::load()`'s `on_path()` check
  already makes this a silent no-op rather than a crash (the action simply
  does not appear in the menu), but it is worth fixing in config rather than
  leaving the menu one entry short:
  ```toml
  [[action]]
  key  = "o"
  name = "Open"
  cmd  = ["open", "{file}"]
  mode = "detach"
  ```

## 8. Order of work

1. Install prerequisites; confirm `pkg-config --modversion exiv2` and
   `have_xmptoolkit=1`.
2. `cargo build --release` — expect success.
3. `./fetch-test-data` — confirm 11 files extract.
4. `cargo test --release --lib` — note which `vol` tests fail.
5. Split `vol.rs` into `imp` modules; implement the macOS one with `statfs`
   plus `diskutil`.
6. `./check` — green, except the two script fixes.
7. Fix `check` and `fetch-test-data` for BSD tools.
8. Verify against a real removable drive, **including ejected**.
9. `npc term`, then the TUI, in one of the image-capable terminals.
10. `cargo build --release --bin npcg`; rerun
    `a_subdirectory_created_after_the_watcher_starts_is_still_caught` (and
    the rest of `tests/watch_integration.rs`) to confirm FSEvents still
    catches the same race inotify does (§7) -- do not assume it from the
    Linux result.

Steps 1–4 are an hour. Step 5 is the port. Step 8 is the one that will find
the bug.
