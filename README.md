# npc

<img src="assets/npcg-icon.png" alt="npc icon: a grid of photos under a magnifying glass" width="128" align="right">

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

A native GUI (`npcg`) covers the same core -- grid/single view, tagging,
rating, the query language, a clickable timeline, the same action menu -- for
anyone who would rather point and click. The TUI remains the primary,
more complete interface; see [The browser](#the-browser).

**Written entirely by an AI agent, with no manual coding.** If that is why you
are here, [How this was built](#how-this-was-built) is the honest account and
comes first.

## Contents

- [How this was built](#how-this-was-built)
  - [The structural fact](#the-structural-fact)
  - [What actually worked](#what-actually-worked)
  - [What failed](#what-failed)
  - [Afterwards](#afterwards)
  - [Where the human time went](#where-the-human-time-went)
  - [The honest summary](#the-honest-summary)
- [Why the next photo catalog](#why-the-next-photo-catalog)
- [Requirements](#requirements)
- [Configuration](#configuration)
- [Commands](#commands)
- [The query language](#the-query-language)
- [The browser](#the-browser)
  - [npcg](#npcg)
- [What it will not do](#what-it-will-not-do)
- [Web export](#web-export)
- [Trash](#trash)
- [Watching and scanning](#watching-and-scanning)
- [Saving](#saving)
- [Offline](#offline)
- [Coverage](#coverage)
- [Portability](#portability)
- [Development](#development)
- [Icon](#icon)
- [License](#license)

## How this was built

This is first because it is the point. The application below is real and in
use, but it exists to answer a question: can a working program be produced
with no manual coding at all?

The first version was written by Claude Opus 5 in Claude Code, in one
sitting. It has since been used and extended for twelve weeks -- by Claude
Sonnet 5 from mid-August, by Claude Opus 5.5 from October. Not a line was
typed by hand. What follows is the honest account -- the numbers, what
failed, and what held -- for developers weighing that question. The
photography is incidental.

| | first sitting (2026-08-09) | now (0.5.13, 2026-10-31) |
|---|---|---|
| Wall clock | **7 h 50 m** | see below |
| — of which planning | ~1 h 15 m before the first line of code | |
| API time | **2 h 10 m** — 28% of wall clock | not recorded |
| Commits | 36 | 156, on 22 days; 14 releases since 0.5.0 |
| Rust | ~10,800 lines incl. tests | ~24,500 lines incl. tests |
| C++ | 447 lines (the Exiv2 shim) | 590 lines |
| Tests | 177, 84% region / 86% line coverage | 410, coverage not re-measured |
| Library | 4,188 files | ~95,000 stacks, 176,000 files, on a USB drive |

**Time and tokens.** Only one session's log survives: 2026-09-20 to
2026-10-31, one continuous Claude Code session on 14 days, mostly Sonnet 5,
Opus 5.5 from October. Counting only gaps under 15 minutes, it was active for
**~23 h**, with ~250 prompts from the human and ~2,600 tool calls by the
agent (1,860 of them shell commands). The sessions between mid-August and
mid-September left no log, and commit timestamps are too irregular to stand
in for one, so the total is not known -- somewhere above 30 hours.

| that one session | requests | output tokens | input read from cache | input written to cache |
|---|---|---|---|---|
| Sonnet 5 | 2,202 | 0.96 M | 1,140 M | 7.5 M |
| Opus 5.5 | 697 | 0.45 M | 191 M | 2.2 M |
| total | 2,900 | **1.4 M** | **1,332 M** | 9.6 M |

Nearly all of it is the conversation being read again on every request: an
average of ~460,000 tokens of context per request, against ~490 tokens
written. The conversation was compacted ten times. Uncached input was under
6,000 tokens in total.

### The structural fact

**No human read the code.** Not one review, not one line -- ~10,800 lines in
the first sitting, ~24,500 now, written, committed and pushed on the strength
of the agent's own tests and the human using the program. Every correction
below came from *using* the program or from questioning the process — never
from reading a diff.

That does not make the human a tester. They were the product designer: they
brought the context the agent could not know, set what the program must do
and what it must never do, chose between the options the agent laid out, and
accepted or rejected the result. The agent was architect, implementer and
debugger -- the Rust/C++ split, the catalog, the query language, how each
rule is enforced -- and it framed most of the options the human chose from,
which is influence too.

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

### Afterwards

Twelve weeks of daily use on the real library -- 95,000 photos on a USB
drive, two cards per shoot, other programs sharing the folders -- found a
second kind of failure: code that was correct on the test photographs and
wrong at scale or among other software. Again a partial list:

- **Importing one card read ~78 GB and took 35 minutes** to find nothing new.
  A shot's identity mapped to one file (the RAW), so every card JPG missed
  and was compared byte for byte. Now 1.7 s for two cards.
- **New photos inherited ratings from old shoots** with the same recycled
  file number -- `750_0001` had been rated in six past sessions.
- **Each rating key cost ~200 ms**, writing the sidecar and reloading 95,000
  rows. The catalog update itself takes 0.05 ms.
- **`npc scan .` stored relative folder names**: 1,834 stacks whose edits
  could never be written, reported only as `1 can't be written`.
- **npcg's window took 10 s to open**, waiting for a file watcher to walk
  2,951 folders with 187,262 stat calls. Now 0.015 s.
- **A one-line change took 206 s to build**, because of link-time
  optimisation that bought nothing measurable. Now 2-3 s.
- **The action menu silently dropped configured programs** that weren't on
  `PATH`, though the config gave their full path.
- **`npc trash 750_2117._spektra` trashed `750_2117`**, a different, rated
  photo: the path was cut at its last dot. The dry run printed only
  `1 stack(s)`. Caught by checking the result, restored, fixed.
- **Other programs' trash got cataloged**: the desktop's `.Trash-1000`,
  digiKam's `.dtrash` (112 deleted photos back in the grid, 62 rated), and
  npc's own `.trash` again under the spelling `.Trash` -- the drive is exFAT,
  where case doesn't matter. Each showed up as photos that shouldn't be
  there.
- **The agent stated a fact about its own code wrongly**: it told the human a
  scan would drop deleted photos from the catalog. A scan only marks them
  missing. Corrected after reading the code, before anything was deleted.

Most of these came from the human using the program and saying what looked
wrong -- "saved, 1 can't be written", "the first view is slow", "stack 8
seems missing" -- not from tests. The human also had to correct the agent's
working habits, more than once: compiling and implementing before an answer
was agreed ("implementing and building should be the very last step"), and
looking around the filesystem instead of asking for a path.

The one rule held throughout: nothing opens an original for writing, and
the tests that enforce it never changed. Every repair to the catalog was
preceded by a backup. (The tripwire checksums the test photographs on every
`./check`; the real library has not been checksummed since the first
sitting.)

### Where the human time went

Into design, and into diagnosis from running the program. The human never
wrote or read a line of code and never corrected a type error. What they did:

- **Gave the context.** RAW to one card and JPG to the other; Shotwell
  rewriting JPGs in the same folders; a removable drive that is sometimes not
  there; which editors and film-emulation tools the photos go through next.
  None of it can be read out of a repository.
- **Took the decisions.** The one rule itself: originals are never
  written. How an imported file is recognised -- name and size from the
  catalog first, camera serial and shutter count second; together with the
  agent's fix for the bug behind the 35 minutes, it took planning two cards
  to 1.7 s. The catalog as the single record,
  replacing per-card profiles. Web exports outside the library, with no
  sidecar and every tag inside the JPG. Full tool paths in the config
  rather than trusting `PATH`. What npcg shows, and what it leaves out.
- **Noticed from use.** "This is too slow", "this is unusable", "saved,
  1 can't be written", "stack 8 seems missing", and — decisively — "let me
  unplug the drive". The causes behind each were found by the agent; the
  precise report is what made that quick.
- **Set the working rules.** Answer before implementing, build last, ask for
  a path instead of searching the disk -- each said after the agent did the
  opposite.

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
Twelve weeks later it still does: the scale bugs above existed on 95,000
photos and on none of the test photographs.

## Why the next photo catalog

Shotwell's core, in a terminal, fast enough for a large library and stable
enough to trust with originals. Full-resolution thumbnails come out of the
RAW's embedded preview pyramid, so a 1790-shot library previews in 23 seconds
and rescans in 0.07.

## Requirements

- Linux, `libexiv2-dev` (0.27 or 0.28), a C++17 compiler, Rust
- **kitty** (or any terminal with the kitty graphics protocol) for the TUI's
  thumbnails. Sixel terminals work but band visibly on photographs;
  everything else falls back to unreadable half-blocks. `npcg` has no
  terminal requirement -- it draws through the GPU instead.

```
cargo build --release          # target/release/{npc,npcg}
cargo install --path .         # both into ~/.cargo/bin (or $CARGO_INSTALL_ROOT)
```

Nothing beyond the above is needed to build `npcg` too: its windowing and
GPU code (`eframe`/`winit`/`wgpu`) is pure Rust and talks to whatever X11 or
Wayland display and GPU driver (Vulkan, or OpenGL as a fallback) your desktop
already has at runtime, rather than linking against their development
headers at build time -- confirmed by `ldd` on the built binary, which shows
nothing beyond `libexiv2` and the C++ runtime.

## Configuration

`~/.config/npc/config.toml`. See [`config.example.toml`](config.example.toml)
for every setting, commented, including the action-menu placeholders/modes
and export presets:

```toml
library = "~/Pictures"                      # used when no root is given
protected_roots = ["/media/me/Archive"]     # never written to, ever

export_dir = "~/Pictures/graded"             # default -o/--out for `npc export`

[thumbs]
grid_px = 1280      # long edge for grid tiles
view_px = 1620      # match your RAW's preview tier; higher is much slower
quality = 80

[watch]
enabled = true      # auto-catalog new files that appear while tui/npcg is open

[tools]                                     # resolve a cmd[0] below to a path,
spektra = "~/local/bin/spektra"             # regardless of how npc/npcg was launched

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
| `rate-sync` | re-resolve rating and tags from the RAW's and JPG's own metadata |
| `tui` / `npcg` | the browser (terminal / native GUI): list, grid, single-image |
| `import` `dupes` | ingest from a card, find copies |
| `trash` `untrash` `trash-list` `trash-empty` | reversible delete, see [Trash](#trash) |
| `export` `exec` `thumbs` | resized copies, run an action, pre-warm thumbnails |
| `events` `doctor` `status` `sync` | shooting days, consistency check, catalog state, write pending sidecars |
| `probe` `volume` `term` `completion` | inspect a file, a volume, the terminal; shell completion |

`npc find --help-syntax` documents the query language.

`tag`, `rate`, `mark`, `rotate`, `set`, `exec` and `trash` all take the same
target: either `-q '<query>'`, or one or more explicit paths to act on
directly. A path is a stack (`dir/base`) or any of its files (`.NEF`, `.JPG`,
`.xmp`), absolute or relative to where you run `npc`. A base may contain dots
of its own -- `750_2117._spektra` is that stack, not `750_2117`; a path that
could mean two different stacks is refused with both named. A query is
almost always the more convenient choice day to day. Both forms refuse to run
with neither given, rather than silently acting on the whole library.
`--dry-run` lists the stacks it would act on:

```
npc tag add beach -q 'date=2026-08-09'
npc rate 4 /media/me/Archive/2026/08/09/750_0001.NEF
npc rate - -q 'name~750_0001'            # - clears a rating; 0 is a real one
npc tag add beach -q 'date=2026-08-09' --dry-run
```

`npc import` with no path detects mounted cards (any mount with a top-level
`DCIM` folder) and prompts you to pick one or more -- Enter takes them all,
`q` cancels; `npc import <path>`
still works directly for scripting. `--library` defaults to the configured
library like every other subcommand.

Each card file is checked in two levels. First from the catalog alone,
without reading the file: a library file with the same name (extension
included) and size means it is already imported. Each import also records
the name, size and mtime every file had on the card, and those are what is
compared from then on -- so a library copy another program rewrote since
(Shotwell embedding a rating) is still recognised instantly. What's left is
read once per shot and matched by camera serial + shutter count + file type
(a JPG only against JPGs): same size, already imported; different size, the
same file with rewritten metadata, which is merged rather than copied again;
found only as the other type, a missing RAW or JPG, which is copied into
that shot's existing stack. That also works across cards in one run, so
RAW on one card and JPG on another end up in one stack. A shared basename
alone never joins two shots: a new shot whose name is already taken in its
day folder gets a `_1`-style suffix. `--deep` skips the first level and
confirms every file by shutter count. Re-imports of a card that is never
reformatted take seconds; hashing every already-imported file was measured
too slow to be the default. `--verify-content` hash-verifies the full bytes on top of that, for
when you know a same-size edit is in play; when it finds an identity+size
match whose content actually differs, it reports a `near-miss` line with the
byte count and offset of the difference, rather than importing silently. A
file reprocessed externally (colour, curves) almost always changes size too,
so it imports alongside the original with a `_1`-style suffix under either
mode; a same-size, byte-preserving edit is only caught with
`--verify-content` on.

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

`g`/`G`/`Home`/`End` reach the true first/last of the whole result set, not
just what's loaded — paging past a result set larger than one page (500
stacks) turns the page automatically. Every text prompt (`/`, `t`, `T`, `i`)
supports normal line editing: arrow keys, Home/End, and readline's
`ctrl-a/e/b/f/k/u/w`.

### npcg

The same catalog, queries and action menu, driven by the mouse:

```
click / ctrl / shift  select one / toggle / range    double-click  single view
arrows  pgup pgdn     move (shift extends)           enter / esc   single view / back
space   esc           toggle selection, clear        wheel, drag   zoom, pan (single view)
0-5                   rate (0 clears)                ctrl+1-5      rating filter
`  x  c               flag / reject / clear          a  e          actions / export menu
ctrl+a                select every photo shown
```

- **Sidebar:** events by month, tags and folders as trees; a click on a
  parent filters to everything below it.
- **Info panel:** EXIF, the full path (with a copy button), tags with a
  remove button, an add-tag field, and the trash-can button. With several
  photos selected, it rates, marks, tags, untags and trashes them all.
- **Single view:** the zoom and pan stay put while you step through
  near-identical frames; leaving with `esc` resets them.
- **Export menu** (`e`): the web export and each configured preset, each
  into its own folder under `export_dir`.
- **Top bar:** query field, rating checkboxes, flagged/rejected toggles,
  tile size, light/dark theme, the save badge and the watcher's eye.
  "flagged", then `ctrl+a`, selects a culling pass's keepers for the export
  or action menu; "rejected", then `ctrl+a`, sets up the duds for the
  trash button.

| | `npc tui` | `npcg` |
|---|---|---|
| views | list, grid, single | grid, single |
| query, rate, flag, tag | keys and prompts | keys, mouse, info panel |
| sidebar | tags or events, flat lists | events, tags, folders as trees |
| timeline | keyboard bar, `[ ]` granularity | clickable bar |
| zoom in single view | `+ -` crop, arrows pan | wheel at cursor, drag |
| RAW/JPG member cycling (`o`) | yes | no |
| undo (`u`) | yes | no |
| copy path | `y` | button in info panel |
| trash | `npc trash` | trash-can button |
| images | terminal graphics (kitty etc.) | native |

In npcg, next to the save badge, a small eye shows the watcher (see
[Watching and scanning](#watching-and-scanning)): drowsy while it sets up,
open and blinking while it watches, circling while it catalogs a folder
that just changed, asleep when watching is off or the drive is offline,
crossed out if it failed. Hover for details.

## What it will not do

- **Edit pixels.** Crop, colour and retouching belong in RawTherapee, which
  the action menu launches on your selection.
- **Write to an original.** Sidecars only. Two functions in the codebase write
  anything, and a test fails if a third appears.
- **Touch a protected root.** Including refusing to launch an external editor
  against one, since an editor cannot be told not to write.

## Web export

The built-in export (npcg's "Quick web", or `npc export` without
`--preset`) makes a resized JPEG and nothing else -- no sidecar beside it.
Into the copy go:

- the original's EXIF: camera, lens, exposure, dates, GPS. Not the maker
  note or the embedded thumbnail; orientation is reset, since the pixels are
  already upright. Where the original has no value, the catalog's is used.
- every tag, three ways: IPTC Keywords (what web galleries read; cut to
  IPTC's 64 bytes), XMP `dc:subject`, and EXIF `XPKeywords` (Windows).
- rating, title and description.

It refuses any folder inside the library: it is a low-quality copy, not a
photo to catalog. Presets may still write into the library. To check a copy:
`exiftool -IPTC:Keywords -XMP-dc:Subject -XPKeywords copy.jpg`.

## Trash

`npc trash` (or npcg's trash-can button) moves a stack's files -- RAW, JPG,
sidecar, RawTherapee profile -- into `<library>/.trash/`, keeping their
folder layout. Nothing is deleted:

```
npc trash --dry-run -q 'rejected'    # lists what would go
npc trash-list                       # what is in the trash
npc untrash 750_0001                 # back where it came from
npc trash-empty --yes                # the only real delete; also removes
                                     # folders the trash left empty
```

Trash folders are never cataloged, whatever their case (the drive may be
exFAT, where `.Trash` is `.trash`): npc's own `.trash`, the desktop's
`.Trash-<uid>`, macOS's `.Trashes`, and digiKam's `.dtrash`.

## Watching and scanning

While `npc tui` or `npcg` is open, a watcher catalogs new or changed photos
by itself -- in every folder *outside* the dated import tree (`2026/...`)
and the trash, e.g. `export/`. It sets itself up in the background, so the
window opens at once. `[watch] exclude` in the config skips further
top-level folders.

`npc import` catalogs what it copies. Everything else needs `npc scan`:

- changes inside the dated tree made by another program -- photos deleted
  or renamed in a file manager, JPGs rewritten by Shotwell;
- anything that changed while neither browser was open, or on another
  machine.

A scan only reads files whose size or mtime changed. Files that disappeared
are marked missing, never removed from the catalog -- an unplugged drive or a
renamed folder must not cost you tags and ratings. `npc doctor` reports them.

## Saving

In `npc tui` and `npcg`, a rating, mark or tag changes the catalog at once
and is written to its sidecar when you pause for a second, so rating fast
doesn't wait on the disk. A badge shows the state: `saved`, `3 to write`,
`writing 3`. Quitting (`q`, or closing the window) waits until everything
queued is written. The queue lives in the catalog, so a crash loses nothing:
the next start writes it.

## Offline

The catalog lives on your machine, not the drive. With the drive unplugged you
can still search, browse thumbnails, tag and rate — edits queue and are written
to sidecars the next time it is connected. Commands that need the files
themselves say so rather than failing obscurely.

## Coverage

```
stack.rs   97%   query.rs   94%   edit.rs    91%   export.rs  92%
import.rs  93%   doctor.rs  84%   trash.rs   89%   scan.rs    86%
tui.rs     82%   watch.rs   93%   vol.rs     81%   action.rs  64%
gui.rs     14%   bin/npcg.rs 0%
                                              TOTAL 79% regions
```

The C++ shim is **not** in that number. Rust coverage does not cross the FFI
boundary, so `meta/mod.rs` reads as covered while `shim.cc` — the file that
actually writes to disk — is measured by nothing. It is covered behaviourally
instead, by the invariant tests, which is the coverage that matters for it.

`gui.rs` and `bin/npcg.rs` pull the total down the same way. `tui.rs` gets
its interaction logic tested headlessly and its rendering tested through
ratatui's `TestBackend`. For `gui.rs` the tests cover its logic (selection,
rating, tagging, trash, folder and tag trees, the watcher's states) and run
a few whole frames headlessly -- closing, the watcher's eye -- but nothing
asserts on what a frame looks like. That is checked by running it, against
the real library, by hand. (The numbers above predate those tests.)

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
./fetch-test-data    the test photographs (98 MB, once)
./check              fmt, clippy, tests, and a tripwire proving the library is unchanged
./check quick        fmt, clippy, unit + invariant only -- seconds
./check coverage     + HTML coverage report
./check fuzz         + the fuzz targets

git config core.hooksPath .githooks    run ./check quick before every commit
```

The hook is opt-in per clone and versioned in `.githooks/` rather than hidden in
`.git/hooks`, so it can be read and changed like anything else. Lint settings
live in `[lints]` in `Cargo.toml`, so `cargo build` and your editor enforce the
same bar the check script does.

The suite runs against real camera files rather than fixtures: maker notes,
preview pyramids and orientation tags are exactly what fixtures lack, and each
has caused a bug here. They are too large for git, so they are fetched once and
verified by checksum before extraction.

Without them, a sentinel test **fails** rather than letting the twelve
real-data tests report as passing while verifying nothing. Rust has no skipped
state, and a green suite that tested nothing is worse than a red one that says
so. `NPC_ALLOW_MISSING_TEST_DATA=1` acknowledges it if you only want to build.

See `CLAUDE.md` for design decisions worth not re-deriving, and `PLAN.md` for
the original design plus a record of where it turned out to be wrong.

## Icon

`assets/icon-h2-3x3-real-emoji.svg` embeds nine [Twemoji](https://github.com/twitter/twemoji)
graphics (mountain, cherry blossom, apple, dancer, person, tree, family,
cityscape at dusk, camera) directly, as base64-encoded PNGs, rather than
redrawing them. Twemoji graphics are © 2020 Twitter, Inc and contributors,
licensed under [CC-BY 4.0](https://creativecommons.org/licenses/by/4.0/);
this notice is that license's attribution requirement.

## License

GPL-3.0-or-later, see [`LICENSE`](LICENSE). npc links against Exiv2, which is
GPL-2.0-or-later, so a GPL license keeps the source and every built binary
under the same terms. The Rust dependencies are permissive (MIT, Apache-2.0,
BSD, Unicode-3.0, one MPL-2.0) and compatible with it. The icon's Twemoji
graphics stay CC-BY 4.0, as above.
