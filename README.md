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

**Written entirely by an AI agent, with no manual coding.** If that is why you
are here, [How this was built](#how-this-was-built) is the honest account and
comes first.

## Contents

- [How this was built](#how-this-was-built)
  - [The structural fact](#the-structural-fact)
  - [What actually worked](#what-actually-worked)
  - [What failed](#what-failed)
  - [Where the human time went](#where-the-human-time-went)
  - [The honest summary](#the-honest-summary)
- [Why the next photo catalog](#why-the-next-photo-catalog)
- [Requirements](#requirements)
- [Configuration](#configuration)
- [Commands](#commands)
- [The query language](#the-query-language)
- [The browser](#the-browser)
- [What it will not do](#what-it-will-not-do)
- [Offline](#offline)
- [Coverage](#coverage)
- [Portability](#portability)
- [Development](#development)

## How this was built

This is first because it is the point. The application below is real and in
use, but it exists to answer a question: can a working program be produced
with no manual coding at all?

Written end to end by Claude Opus 5 in Claude Code, in one sitting. Not a line
was typed by hand. What follows is the honest account -- the numbers, what
failed, and what held -- for developers weighing that question. The
photography is incidental.

| | |
|---|---|
| Wall clock | **7 h 50 m** |
| — of which planning | ~1 h 15 m before the first line of code |
| API time | **2 h 10 m** — 28% of wall clock |
| Commits | 36 |
| Rust | ~10,800 lines incl. tests |
| C++ | 447 lines (the Exiv2 shim) |
| Tests | 177, 84% region / 86% line coverage |

Token counts are omitted: not exposed to the agent, and inventing them would
defeat the point.

### The structural fact

**No human read the code.** Not one review, not one line. ~10,800 lines were
written, committed and pushed on the strength of the agent's own tests and the
human running the binary. Every correction below came from *using* the program
or from questioning the process — never from reading a diff.

That is the experiment, and everything else is a consequence of it.

### What actually worked

The safety property held. This tool writes to files that cannot be
regenerated, and across ~12 hours and a dozen agent errors, **not one
photograph was altered** — verified by SHA-256 over 4,188 files, repeatedly.

It held for a reason worth generalising: it was made *checkable* rather than
*careful*. One function may write; it refuses any path not ending in `.xmp`;
a test asserts the codebase contains exactly two `writeMetadata()` calls and
fails if a third appears. When a second writer became genuinely necessary, the
test forced the argument to be written down before the count could change.

Nothing about that depended on the agent being careful, which is fortunate,
because it repeatedly was not.

### What failed

The agent's failure mode is not "writes broken code". It is **writes plausible
code that passes its own tests and confirms its own assumptions**. A partial
list, all of it real:

- **Committed twice before reading test output.** Both times the tests were
  failing and right.
- **The offline mode never worked.** A headline feature, written in the
  morning, described in the plan, broken from its first line — resolving a
  volume canonicalises a path, which cannot work when the drive is gone.
  Discovered at hour seven when the human pulled the cable.
- **The protected-root guard reported success while writing nothing.** It
  checked at flush time instead of dispatch, so a command printed
  `tag add 71 stack(s)` and exited zero having written nothing, leaving the
  catalog claiming tags that did not exist on disk.
- **Protected roots did not gate external editors at all.** The agent's own
  action system launched RawTherapee against a drive it was told never to
  touch, and reported everything as fine. Twenty-eight files were created there
  before anyone noticed.
- **The plan's central number was wrong** (1722 stacks; ground truth 1790).
  Caught only because the agent computed ground truth independently before
  trusting itself — the one habit here that reliably paid.
- **Thumbnail sourcing was backwards**, costing 4.7x, because the plan called
  a full-resolution file a "shortcut" and the agent believed it.
- **Image rendering used a hardcoded rectangle**, so every thumbnail was
  encoded for the wrong area — the user-visible symptom being "the thumbnails
  look like brackets".
- **A cache key omitted the pixel size**, so changing thumbnail resolution
  silently served the old ones, reporting `generated 0, cached 1790` on a run
  that should have rebuilt everything.
- **Tests written too tightly broke when the program was used.** Twice.
  Opening a photo in RawTherapee failed the suite.
- **Fuzz assertions were wrong twice**, both times firing on the tool's own
  SQL templates rather than on any real defect.
- **Left two stray files in the user's library** while debugging, because
  `exiv2 -ep` writes next to the source, not the working directory.
- **Scanned the user's filesystem unprompted** at the start, twice, after
  being asked to stop.

Roughly a third of the elapsed time went on rework of the agent's own output.

### Where the human time went

Almost all of it was *interaction design and diagnosis from running the
program* — see the two lists below. The human never specified an algorithm,
never corrected a type error, never touched the architecture after the initial
planning hour. They said "this is too slow", "this is unusable", "this is
inconsistent", and — decisively — "let me unplug the drive".

Two contributions changed the outcome disproportionately:

1. **Asking for coverage measurement.** It immediately exposed an
   import-safety test suite the agent had written into its own plan and never
   implemented. The agent had not noticed.
2. **Physically unplugging the drive.** No test the agent could write would
   have found that bug, because the agent cannot pull a cable.

### The honest summary

For a developer weighing this: the agent is a fast, tireless implementer that
verifies its work against *its own understanding of the requirement*. That
understanding is frequently subtly wrong, and its tests inherit the error.
What it cannot do is notice that the thing it built is not the thing you
wanted, or that the environment it assumed does not exist.

The mitigations that worked were structural, not attentional:

- make safety properties **checkable by a test**, not upheld by care
- compute **ground truth independently** before trusting a plan
- **measure** rather than reason about performance; the agent's intuitions
  were wrong in both directions and by large factors
- **run the thing**, in the real environment, with the real hardware attached
  and then detached

That last one found more than every test in this repository combined.

## Why the next photo catalog

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
