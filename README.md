# npc

A terminal photo catalog: tag, rate, browse with real thumbnails, and hand
selections to a proper editor. Built for a Nikon RAW+JPG library on a removable
drive.

It does not edit pixels, and it never opens an original for writing. Tags and
ratings go into `.xmp` sidecars beside your photographs; the SQLite catalog is
a rebuildable cache you can delete at any time without losing anything.

```
npc scan ~/Pictures            # build the catalog
npc find 'beach rating>=4'     # select
npc tui                        # browse
```

## Why

Shotwell's core, in a terminal, fast enough for a large library and stable
enough to trust with originals. Full-resolution thumbnails come out of the
RAW's embedded preview pyramid, so a 1790-shot library previews in 23 seconds
and rescans in 0.07.

## Requirements

- Linux, `libexiv2-dev` (0.27 or 0.28), a C++17 compiler, Rust
- **kitty** (or any terminal with the kitty graphics protocol) for thumbnails.
  Sixel terminals work but band visibly on photographs; everything else falls
  back to unreadable half-blocks.

```
cargo build --release          # target/release/npc
```

## Configuration

`~/.config/npc/config.toml`:

```toml
library = "~/Pictures"                      # default for --root
protected_roots = ["/media/me/Archive"]     # never written to, ever

[thumbs]
grid_px = 1280      # long edge for grid tiles
view_px = 1620      # match your RAW's preview tier; higher is much slower
quality = 80

[[action]]                                  # what the `a` menu launches
key  = "r"
name = "RawTherapee"
cmd  = ["rawtherapee", "{raw}"]
mode = "detach"

[[export]]                                  # named export presets
name = "web"
cmd  = ["convert", "{src}", "-resize", "2000x2000>", "-quality", "95", "{dst}"]
```

## Commands

| | |
|---|---|
| `scan` `find` `tags` | build the catalog and query it |
| `tag` `rate` `mark` `set` `rotate` | edit metadata, written to sidecars |
| `tui` | the browser: list, grid, single-image |
| `import` `dupes` `trash` `untrash` | ingest from a card, find copies, reversible delete |
| `export` `exec` `thumbs` | resized copies, run an action, pre-warm thumbnails |
| `events` `doctor` `status` `sync` | shooting days, consistency check, catalog state |

`npc find --help-syntax` documents the query language.

## The query language

Barewords are tags; adjacency means AND.

```
beach anna rating>=4
beach and (anna or ben) and -private
year=2026 camera~D750 untagged
date=2026-08-01..2026-08-09 iso>1600
flagged and not rejected
```

Keys: `tag rating year date event camera lens serial ext role iso shutter
width height path name title description`. Flags: `tagged untagged rated
unrated flagged rejected offline`.

## The browser

```
j k g G              move            page-up/down   one screen
space  v  esc        select, range, clear
enter / esc          zoom into / out of the single view
tab / shift-tab      list -> grid -> single
/                    query           t T   tag / untag
1-5  0               rate            f x c flag / reject / clear
- +                  grid tiles across
a                    actions         u     undo
```

## What it will not do

- **Edit pixels.** Crop, colour and retouching belong in RawTherapee, which
  the action menu launches on your selection.
- **Write to an original.** Sidecars only. Two functions in the codebase write
  anything, and a test fails if a third appears.
- **Touch a protected root.** Including refusing to launch an external editor
  against one, since an editor cannot be told not to write.

## Offline

The catalog lives on your machine, not the drive. With the drive unplugged you
can still search, browse thumbnails, tag and rate — edits queue and are written
to sidecars the next time it is connected. Commands that need the files
themselves say so rather than failing obscurely.

## How this was built

Written end to end by Claude Opus 5 in Claude Code, as a test of whether a
real application could be produced with no manual coding. Not a single line
was typed by hand; the human contribution was requirements, design decisions,
running it, and saying when it was wrong.

| | |
|---|---|
| Wall clock | **7 h 50 m**, one sitting |
| — of which planning | **~1 h 15 m** before the first line of code |
| API time | **2 h 10 m** — 28% of wall clock |
| Commits | 28 |
| Rust | ~10,800 lines incl. tests |
| C++ | 447 lines (the Exiv2 shim) |
| Tests | 177, 84% region / 86% line coverage |
| Docs | ~1,000 lines (PLAN.md, CLAUDE.md, README.md) |

The first ~1 h 15 m was planning, before any code existed: requirements,
four rounds of questions, and the design going Go → Rust → C++ → Rust-with-a-
C++-shim as the constraints came out. Two decisions taken there — that
originals are never opened for writing, and that the catalog is a disposable
cache — shaped every file afterwards and were never revisited. Two others were
wrong and had to be measured out later, which is why `PLAN.md` carries a
section recording where it was mistaken.

The gap between the two duration numbers is the other interesting part. Under
a third of the elapsed time was the model thinking; the rest was compiling,
running 177 tests against a 41 GB library, scanning 4,188 files repeatedly,
hashing 41 GB to prove nothing had changed — and the human looking at the
result and saying what was wrong.

Token counts are not included: the tooling does not expose them to the agent,
and inventing them would defeat the purpose of the exercise. The API and wall
figures came from the human's side of the session.

**What the human caught that the machine did not.** Worth recording, since it
is the honest measure of where the boundary sat:

- thumbnails being unusably small — twice, before the real cause was found
- the single view being too slow, and the suggestion (use a smaller embedded
  preview) that fixed it
- an inconsistency between three different names for the same program
- a hardcoded path that made the binary useless on any other machine
- unplugging the drive, which revealed that the entire offline mode — a
  headline feature, written hours earlier — had never worked once

The last one is the sharpest: no test written by the agent would have found
it, because the agent never pulled the cable.

**What the machine caught that a human might not have.** Coverage measurement
immediately exposed an import-safety suite that had been written into the plan
and never implemented. The consistency checker found 540 byte-identical
duplicate pairs in the library, and identified Shotwell silently embedding
ratings into originals.

## Coverage

```
stack.rs   97%   query.rs   93%   edit.rs    92%   export.rs  91%
import.rs  89%   doctor.rs  89%   trash.rs   89%   scan.rs    81%
tui.rs     79%   thumbs.rs  79%   vol.rs     80%   action.rs  65%
                                              TOTAL 84% regions
```

The C++ shim is **not** in that number. Rust coverage does not cross the FFI
boundary, so `meta/mod.rs` reads as covered while `shim.cc` — the file that
actually writes to disk — is measured by nothing. It is covered behaviourally
instead, by the invariant tests, which is the coverage that matters for it.

## Portability

Today: Linux, dynamically linked against the system `libexiv2`. That was a
deliberate choice — portability was not needed during development and buying
it early would have cost a static-linking fight for nothing.

For distribution the constraint is **Exiv2, not Rust**. The Rust half
cross-compiles to `x86_64-linux`, `aarch64-linux` and `aarch64-darwin` without
ceremony; the C++ shim needs a libexiv2 built for each target. Three routes,
in increasing order of effort:

1. **Vendor and statically link Exiv2** (plus expat and zlib) per target. One
   binary, no runtime dependency. Needs a CI job per platform, and
   `aarch64-apple-darwin` needs a real macOS runner — it cannot be
   cross-compiled from Linux in any way worth maintaining.
2. **Shrink the C++ surface.** A pure-Rust EXIF reader could cover the read
   path, leaving Exiv2 needed only for sidecar writes. Less to build per
   target, at the cost of two metadata implementations.
3. **Distribute as source** with a documented `libexiv2-dev` dependency. What
   happens today, and adequate for Linux.

Rewriting in C++ was considered and rejected: it does not remove Exiv2, it
makes every other dependency share Exiv2's build problem. See `PLAN.md`.

Whichever route, each release artifact must assert `have_xmptoolkit` at build
time. An Exiv2 built without XMP passes every test that does not touch
sidecars, then destroys metadata in the field.

## Development

```
./check              fmt, clippy, tests, and a tripwire proving the library is unchanged
./check coverage     + HTML coverage report
./check fuzz         + the fuzz targets
```

See `CLAUDE.md` for design decisions worth not re-deriving, and `PLAN.md` for
the original design plus a record of where it turned out to be wrong.
