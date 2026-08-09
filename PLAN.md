# `photo` — a terminal photo manager: Shotwell's core, fast and stable

## Context

The goal is a replacement for Shotwell's core functionality that runs in a terminal, is fast
on a large library, and is stable. Concretely: import from a card, browse with real
thumbnails, tag and rate, search, and hand selections to a real editor.

The photos live on an external SSD that is not always plugged in — which is why the catalog
lives on the laptop and the drive can be absent without the library becoming unusable.

A note on `/home/ram/src/tasks/scan-images/Rakefile`: it's a small standalone metadata
transformer, not a precursor to this and not something this replaces. It's referenced below
only because it documents a few facts about the workflow worth designing around — NEF and JPG
shot together and paired by basename, culling done on the JPGs, and enough care about file
mtimes to pass exiftool's `-P`.

**Working directory: `/home/ram/src/tasks/npc`.** Everything I create lives there.
**Sole file input: `/home/ram/src/tasks/test-data`.** No other directory gets read.

| | |
|---|---|
| Source of truth | The files on the SSD. SQLite is a rebuildable cache, never the only copy. |
| Catalog location | Laptop, `~/.local/share/photo-catalog/<volume-uuid>.sqlite` — library stays queryable with the SSD unplugged. |
| Language | **Rust**, with **Exiv2** through a `cxx` bridge to a small C++ shim. |
| Metadata | Exiv2: EXIF read, XMP sidecar read/write, embedded preview extraction. |
| Sidecar name | `DSC_0123.xmp` (Lightroom / Bridge / digiKam convention). |
| Interface | CLI plus a keyboard-driven TUI (ratatui) with in-terminal thumbnails. |
| Editing | The tool never edits pixels. It selects and launches RawTherapee, darktable, GIMP. |

---

## The invariant: originals are never written

**No code path in this program opens an original image file for writing.** Not by default,
not behind a flag, not with a confirmation prompt. Pixels and metadata are fully decoupled:
your `.NEF` and `.JPG` files are read-only inputs, and everything this tool produces lives in
a separate `.xmp` file with its own inode. Creating `DSC_0123.xmp` leaves `DSC_0123.NEF`'s
bytes, size, and mtime untouched.

This is an invariant, not a policy, and it's enforced structurally so it can be checked rather
than trusted:

- **One write function exists.** `shim.cc` exposes exactly one call that writes —
  `write_sidecar` — and it hard-asserts the path ends in `.xmp` before doing anything.
  `writeMetadata()` appears nowhere else in the codebase. That's greppable in one command.
- **Originals open `O_RDONLY`.** Read paths go through a helper that opens read-only, so a
  write attempt fails at the syscall rather than succeeding quietly.
- **Two tests hold the line.** The full suite runs against a `chmod -R a-w` photo directory
  and must pass; and a tripwire test fingerprints every original (size, mtime, blake3) before
  and after the entire suite, failing on any difference.
- **Metadata is visually distinct.** `.xmp` is metadata, `.NEF`/`.JPG` are pixels. You can see
  at a glance in any file listing what this tool has and hasn't produced, and delete every
  trace of it with a single glob.

Two consequences worth stating up front rather than discovering later:

**Rotation is sidecar-only, so plain viewers won't honour it.** `npc rotate` writes
`Xmp.tiff.Orientation` to the sidecar. This tool honours it, and so do Lightroom, Bridge, and
digiKam. But `eog` or `feh` opening the JPG directly read EXIF, ignore sidecars, and will show
it unrotated.

*A narrow exception is planned for M6, deliberately deferred:* optional in-file
`Exif.Image.Orientation` stamping **for JPGs only**, off by default and enabled in config.
NEFs stay untouched regardless — everything that opens a RAW already reads sidecars, and
Exiv2's RAW writer is its weakest area. When it lands it gets its own assertion (JPEG paths
only), and each write copies to a temp file, verifies the compressed scan data is
byte-identical, and only then renames over the original with mtime preserved. The camera's
original orientation is recorded at first scan in `file.exif_orientation`, so any rotation is
exactly reversible.

Until M6 the invariant is absolute, which is the point of deferring: every safety test through
M5 is written against "nothing ever opens an original for writing", so the exception arrives
as a separately-tested carve-out rather than a hole present from the start.

**`npc trash` is the one command that touches originals** — it *moves* them to `.trash/`.
No bytes change, it's reversible via `npc restore`, it never runs automatically, and
`--read-only` blocks it. Flagged here because it's the single exception to "this tool only
ever reads your photos", even though it isn't an exception to "never changes them".

---

## Environment

Verified present, nothing else needed to start:

```
pkg-config 1.8.1   g++ 13.3.0   cmake 3.28.3   cargo 1.75.0
libexiv2-dev 0.27.6  (have_xmptoolkit=1, expat 1.9.1)
kitty 0.32.2  (full graphics protocol)
exiftool  rawtherapee  rawtherapee-cli  darktable  sqlite3  git  xdg-open
```

**One setup step before M1:** install `rustup` — userspace, into `~/.cargo` / `~/.rustup`, no
root, leaving system cargo alone. System cargo 1.75 is from Dec 2023, old enough to force
pinning several crates, and the nightly-only tools (Miri, `cargo-fuzz`) are unavailable
without it.

Thumbnails need `npc tui` to be running *inside* kitty; `ratatui-image` detects the protocol
by querying the terminal at startup rather than trusting `$TERM`, and falls back cleanly if
you launch it somewhere else.

**Note: exiv2 is 0.27.6, not 0.28.** Everything the plan needs is present and I verified it
functionally rather than by version number (see below). The only API difference that touches
us is `Image::AutoPtr` where 0.28 has `Image::UniquePtr`; the shim version-guards on
`EXIV2_TEST_VERSION` so it builds against either.

### Two assumptions validated before committing to them

**Sidecar preservation works.** A sidecar carrying a foreign `darktable:history_end` property,
written through exiv2 to set `Xmp.dc.subject`, came back with the darktable property intact.
The silent-data-loss risk is tested, on this exact library version, before any code exists.

**The D750 preview pyramid is better than assumed.** `exiv2 -pp` on a test NEF:

```
Preview 1:  160x120    tiff    57 KB
Preview 2:  640x424    jpeg   140 KB   -> grid cache (384 px)
Preview 3: 1620x1080   jpeg  1.03 MB   -> view cache (1600 px)
Preview 4: 6016x4016   jpeg  1.75 MB   -> full 24 MP, native resolution
```

The tiers line up with the two cache sizes almost exactly, so a grid thumbnail costs a 140 KB
read rather than a 1.8 MB one. And preview 4 being full sensor resolution corrects the export
caveat below.

## Scope against Shotwell

| Shotwell feature | Here |
|---|---|
| Import from card into dated folders | **Yes** — `npc import`, copy-only, hash-verified |
| Duplicate detection on import | **Yes** — capture time + serial + shutter count, hash fallback |
| Thumbnail grid browsing | **Yes** — from embedded RAW previews |
| Event / date navigation | **Yes** — year/month/day tree plus gap-derived events |
| Tags (hierarchical) | **Yes** — `a/b` hierarchy, sidebar with counts |
| Star ratings | **Yes** — on the stack, so the NEF gets it too |
| Flag & reject | **Yes** — `Xmp.xmp.Label`, distinct from stars |
| Title & description | **Yes** — `Xmp.dc.title`, `Xmp.dc.description` |
| Search / filter | **Yes** — a query language shared by CLI and TUI |
| Lossless rotate | **Yes** — sidecar orientation; see the caveat above |
| Export with resize | **Yes** — writes new files only, with a caveat below |
| Trash & restore | **Yes** — move to `.trash/`, never unlink without asking |
| Slideshow | Trivial in single view with a timer; low priority |
| Crop, colour, red-eye, enhance | **No — deliberately.** See below |
| Face detection, publishing plugins | **No** |

### Why no built-in editor

Raster editing in a terminal is slow to build, awkward to use, and exactly where "stable" goes
to die — and it would require writing pixels, which the invariant forbids outright. The action
system hands a selection to RawTherapee, which is a far better editor than Shotwell's. Better
an excellent catalog that launches a real editor than a mediocre both.

## Stability: the architectural claim

A known Shotwell failure is that its non-destructive edits and some metadata live *only* in
its library database, so a corrupt DB loses real work. This design inverts that, and most of
the stability story follows from it:

- **The catalog is disposable.** Tags, ratings, titles, flags all live in sidecars on the
  drive. Total catalog loss costs `npc scan --full` and some waiting — nothing else.
- **Originals are never written** — see the invariant above.
- **Every write is atomic** — temp file plus `rename()` within the same directory, so an
  interrupted write cannot truncate a sidecar.
- **Import copies, verifies by hash, then records.** The card is never modified, and a failed
  verification aborts that file rather than reporting success.
- **Nothing is unlinked implicitly.** Trash is a rename within one filesystem: instant,
  atomic, reversible. Only `trash --empty` deletes, and it counts and confirms first.
- **Crash-safe queue.** WAL, one writer thread owning the `Connection`, and offline edits in a
  `pending` table that survives a kill.
- **Panic policy.** No `unwrap()` on anything file-derived; `catch_unwind` around the TUI so a
  panic restores the terminal and prints the error rather than leaving a wrecked tty. Exiv2
  exceptions arrive as `Result` via `cxx`, so a corrupt file is a skipped row, never an abort.
- **Migrations copy the DB first**; `npc doctor` is the consistency checker.
- Fuzz targets on the query parser and the sidecar reader — the two places untrusted input
  meets parsing logic.

## Why Rust + a C++ shim

The subtlest requirement here is that writing a tag must not destroy properties darktable or
Lightroom parked in the same sidecar. Hand-rolling that means hand-rolling an XMP
round-tripper *and* a TIFF/EXIF parser for NEF. Exiv2 has done both correctly for twenty
years, and it is C++. Everything else — threaded scanning, SQL, a query parser, a TUI — is
where Rust's guarantees pay and C++'s buy nothing.

So: C++ exactly where the irreplaceable library lives (~250 lines), Rust everywhere else.

- **`cxx` maps C++ exceptions onto Rust `Result`** — per-file error isolation is structural
  rather than remembered.
- **The XMP thread-safety hazard is solved, not managed.** Exiv2's XMP layer wraps the Adobe
  SDK and isn't thread-safe. Because the shim is the *only* entry point, a Rust `Mutex`
  guarding it makes exclusion provable instead of conventional.
- **The build is ordinary** — `pkg-config` against system `libexiv2`, no CMake project.
- It pays a third time on thumbnails, via `PreviewManager`.
- And a fourth: confining every Exiv2 call to one file is what makes the write invariant
  auditable at a glance.

Cost, stated plainly: both toolchains must be installed, and FFI is a real source of bugs —
though a narrow, well-typed one, since only plain structs and byte buffers cross.

## Stacks

`750_6926.xmp` is keyed on basename, so every file of one shot shares one sidecar. Make that
the model: the unit of tagging is the **stack** — one directory plus one basename, however
many files it holds. Per-file EXIF stays per file; tags, rating, title, description, label,
and orientation attach to the stack.

A useful side effect: because the sidecar is shared, rating a stack covers the JPG and the NEF
at once, so there's no separate transfer step and nothing gets rewritten to achieve it.

### Member roles

Your library has four kinds of file per shot, and they are not interchangeable — so each stack
member carries a `role`:

| File | Role | Meaning |
|---|---|---|
| `750_6926.NEF` | `raw` | the original |
| `750_6926.JPG` | `oocjpg` | out-of-camera JPG, what you cull on |
| `750_6926_NEF_embedded.jpg` | `derived` | an already-extracted preview |

Two rules fall out of this, both of which matter more than they look:

**Derived files fold into their parent stack.** Naive basename matching would make
`750_6926_NEF_embedded` its own stack — in your test set that's 1182 phantom stacks, each a
duplicate of a real one, with tags landing on the wrong object. A configurable list of derived
suffixes (default `_NEF_embedded`) strips them back to the parent. This is the single most
important thing the real data revealed.

**Extension matching is case-insensitive for type but case-preserving for identity.** `.JPG`
and `.jpg` mean different roles in your library, so the matcher classifies case-insensitively
while the stored name stays literal.

Roles also make the action placeholders resolve correctly: `{raw}` is the NEF, `{jpg}` is the
out-of-camera JPG — never the extracted preview, which you'd never want opened in RawTherapee.
RawTherapee `.pp3` profiles are not catalogued at all, but they still ride
along with their stack through trash and restore -- a profile left pointing at
an image that has moved is litter.

## Layout

```
Cargo.toml
build.rs                 cxx-build + pkg-config, C++17
src/main.rs              CLI dispatch (clap)
src/vol.rs               mount point -> filesystem UUID; online/offline
src/db.rs                schema, migrations, batched writer thread
src/meta/mod.rs          #[cxx::bridge] + the Mutex serializing Exiv2
src/meta/shim.cc/.h      the only C++ in the project; the only writer
src/scan.rs              parallel walk + change detection
src/import.rs            card ingest, dedupe, verify
src/query.rs             query language -> SQL
src/thumbs.rs            preview extraction, resize, on-disk cache
src/action.rs            config-driven external commands
src/export.rs            resized copies (new files only)
src/trash.rs             trash / restore / empty
src/tui/                 list, grid, single view, sidebars
```

Crates: `rusqlite` (bundled SQLite), `cxx`, `clap`, `ratatui` + `crossterm`, `ratatui-image`,
`image`, `fast_image_resize`, `rayon`, `walkdir`, `blake3`, `serde` + `toml`, `anyhow`,
`directories`, `rustix`.

### The bridge

```cpp
FileMeta    read_meta(rust::Str path);              // read-only
SidecarDoc  read_sidecar(rust::Str xmp_path);       // read-only
PreviewBlob extract_preview(rust::Str path, uint32_t min_dim);   // read-only

void        write_sidecar(rust::Str xmp_path, const SidecarDoc& doc);
//          ^ the ONLY write in the program. Asserts xmp_path ends in ".xmp"
//            and refuses anything else. Nothing else calls writeMetadata().

void        stamp_export(rust::Str dest_jpg, const SidecarDoc& doc);
//          writes metadata into a file THIS TOOL just created under the export
//          target. Asserts dest is inside the export directory; can never see
//          a library path.
```

All return by value; the structs are `cxx` shared structs — plain fields and `Vec<u8>`, no
pointers or lifetimes crossing. Exiv2 types never escape `shim.cc`, so a future library swap
is one file and the unsafe surface is auditable in an afternoon.

`build.rs` locates `exiv2` via the `pkg-config` crate (0.27.6 here, confirmed discoverable)
and compiles the shim with `cxx_build` at C++17. A missing `libexiv2-dev` produces a build
error naming the package; a build without `have_xmptoolkit` fails at configure time rather
than silently losing sidecar support.

## Schema

```sql
volume(id, vol_uuid UNIQUE, label, last_root, last_seen)
event (id, name, start_time, end_time, auto)
stack (id, dir, base, event_id, sidecar_mtime, rating, flag, reject,
       title, description, orientation, dirty, UNIQUE(dir, base))
file  (id, stack_id, name, ext, role, size, mtime_ns, capture_time, camera, lens,
       iso, fnumber, exposure, focal, width, height, exif_orientation,
       serial, shutter_count, content_hash, present DEFAULT 1,
       UNIQUE(stack_id, name))
       -- role: 'raw' | 'oocjpg' | 'derived' | 'profile'
tag       (id, name UNIQUE COLLATE NOCASE)
stack_tag (stack_id, tag_id, PRIMARY KEY(stack_id, tag_id))
pending   (stack_id PRIMARY KEY, queued_at)
trashed   (stack_id PRIMARY KEY, orig_dir, trashed_at)
```

Plus `INDEX stack_tag_rev ON stack_tag(tag_id, stack_id)` — what makes select-by-tag instant —
and indexes on `file(capture_time)`, `file(serial, shutter_count)`, and `stack(dir)`.

Note `file.exif_orientation` (what the camera wrote, read-only) is separate from
`stack.orientation` (what you set, sidecar-backed). Display applies ours if present, otherwise
the camera's. The camera's value is never overwritten.

Vanished files are marked `present = 0`, never deleted. An unplugged drive or a renamed folder
must not destroy tag history. `content_hash` is computed lazily and only for dedupe
candidates — the library is never hashed wholesale.

**Events** are derived at scan time by a configurable capture-time gap (default 4 h) and
materialized into `stack.event_id`. Re-derivation preserves user-given names by time-range
overlap, so naming an event survives the next import.

## Scan pipeline

Target: an incremental rescan costs roughly one `stat()` per file and nothing more.

1. `walkdir` on one thread feeds a bounded channel. `.trash/` is excluded.
2. A `rayon` pool compares each `(size, mtime_ns)` against a `HashMap` preloaded from the DB
   in one query. **Unchanged files are dropped without ever being opened** — the whole trick,
   and what keeps Exiv2's per-file cost (milliseconds, not microseconds) off the daily path.
3. New or changed files go through `meta::read()`, taking the Exiv2 mutex. Sidecars are
   re-read only when the sidecar's own mtime moved.
4. One writer thread owns the `Connection`, committing ~500-row transactions with
   `journal_mode=WAL, synchronous=NORMAL`.

Because originals are opened read-only, a scan cannot alter the drive even if it panics
mid-walk — which is what makes it safe to point at your real library on day one.

The honest consequence of the Exiv2 mutex: metadata parsing is serialized. It's off the hot
path, but a **first** scan of a large library is bound by single-threaded Exiv2 throughput. If
M1's benchmark shows that hurting, the fix is a small pool of worker *processes* — deferred
until measured, not designed for speculatively.

## Import

```
npc import /media/card              copy into <library>/YYYY/MM/DD/ by EXIF capture date
  --in-place PATH                     register where files already are, no copy
  --dry-run                           show the plan
```

Per file: read capture date, check dedupe, copy to the dated target, `fsync`, re-hash the
destination and compare, then record. **The card is opened read-only and never modified** —
there is no `--move`, by choice. A name collision against a genuinely different file gets a
`_1` suffix; all members of a stack always land in the same directory together.

**Dedupe** matches on capture time + camera serial + shutter count, exact for Nikon bodies via
Exiv2's maker-note support. Files that collide on `(capture_time, size)` but lack serial data
fall back to a blake3 content hash. `npc dupes` reports groups across the library; resolving
them is left to trash and the action system rather than an automatic rule.

## Metadata via Exiv2

**Read** — `ImageFactory::open` then `readMetadata()`, for NEF and JPG alike, always
read-only. Exiv2 normalizes Nikon maker notes, so lens, serial, and shutter count work without
per-camera special cases.

**Sidecars** — `ImageFactory::open("DSC_0123.xmp")` yields an `XmpSidecar` that reads and
writes that file and only that file. Tags `Xmp.dc.subject`, rating `Xmp.xmp.Rating`, title
`Xmp.dc.title`, description `Xmp.dc.description`, flag and reject `Xmp.xmp.Label`, rotation
`Xmp.tiff.Orientation`.

**The preservation rule**: read the existing sidecar into `xmpData()`, then **modify only our
keys in place**. Never build a fresh `XmpData`, never `clear()` — that is exactly how other
apps' properties get silently destroyed. The invariant protects your pixels; this rule
protects everyone else's metadata.

## Thumbnails

**Never demosaic the RAW.** NEFs carry embedded JPEG previews, and Exiv2's `PreviewManager`
enumerates them with dimensions, so `extract_preview` takes the smallest one at or above the
requested size. That's a header read plus a seek — milliseconds, against seconds for a real
RAW decode. This is what makes thumbnails practical at all.

**And where a `derived` member already exists, skip Exiv2 entirely.** Much of your library
already has `_NEF_embedded.jpg` sitting next to the RAW — an extracted preview, exactly what
the thumbnailer wants. When a stack has one, decode it directly: no FFI call, no Exiv2 mutex,
no contention. Preference order per stack is `derived` → `oocjpg` → preview extracted from
`raw`.

Extraction takes the Exiv2 mutex (cheap, I/O-bound); decode and resize run **outside** the
lock on a rayon pool, so the CPU-heavy half parallelizes even though Exiv2 is serialized.

**Cache** at `~/.cache/photo-catalog/<vol-uuid>/`, two sizes — `grid` (384 px, sourced from
preview 2 at 640x424) and `view` (1600 px, from preview 3 at 1620x1080) — keyed by stack plus
source mtime. The pyramid means each cache tier reads the smallest sufficient preview rather
than the 1.75 MB full-resolution one.

The cache lives on the **laptop** deliberately: nothing is ever written beside your photos for
display purposes, and with offline queries it means browsing and tagging visually with the SSD
unplugged. Generation is lazy (visible page plus prefetch window), with `npc thumbs` to
pre-warm after an import.

**Rendering** via `ratatui-image`. Targeting the **kitty graphics protocol** as the primary
path, with automatic fallback through iTerm2 → Sixel → Unicode half-blocks so the tool still
works if you run it elsewhere.

## TUI

`Tab` cycles three view modes; tagging and rating keys stay live in all of them.

- **List** — dense rows: basename, extension badges, date, stars, flags, tags.
- **Grid** — thumbnail tiles with rating and tag overlays.
- **Single** — one large preview. `h`/`l` walk the current query result, `o` cycles which file
  of the stack is shown, `Esc` returns.

Single view is the culling loop: big preview, `3` to rate, `l` to advance. The rating lands on
the stack, so it covers the NEF via the shared sidecar without modifying either file.

The sidebar has two tabs, `[tags]` and `[dates]` — the latter a year/month/day tree with
counts and named events, which is how Shotwell expects you to move around a library.

```
j/k g/G  move           space select        v  range select
/        query          t     tag           T  untag
1-5      rate           0     clear         f  flag       x  reject
i        title/descr    [ ]   rotate ccw/cw
Tab      view mode      o     cycle file    s  sidebar tab
a        action menu    Enter default action
w        flush to disk  u     undo          q  quit
F1-F9    your nine most-used tags
```

DB and thumbnail work run on worker threads, delivered over channels the event loop selects
on — the UI never blocks on a query or a decode. The model holds only the visible page (SQL
`LIMIT`/`OFFSET`), so it opens instantly at 10k or 500k stacks. An in-session op log backs
`u`, so a mis-keyed bulk tag is reversible before it reaches the drive.

## Actions

The tool never edits pixels — it selects and hands off. Config at
`~/.config/photo-catalog/config.toml`:

```toml
[[action]]
key = "r"; name = "RawTherapee"; cmd = ["rawtherapee", "{raw}"]; mode = "detach"

[[action]]
key = "d"; name = "darktable";   cmd = ["darktable", "{raw}"];   mode = "detach"

[[action]]
key = "e"; name = "Export web";  cmd = ["photo", "export", "--size", "2048", "-o", "~/web", "{stacks}"]
```

Placeholders expand to **separate argv entries**: `{files}` every file of every selected
stack · `{raw}` / `{jpg}` the matching member of each · `{file}` the current file · `{dir}` ·
`{query}`.

**No `sh -c` anywhere.** argv is built directly, so a filename with a space, quote, or newline
cannot alter a command — which matters precisely because these paths come off a drive rather
than from you. `shell = true` exists as an explicit opt-in and is documented as your problem.

Note the boundary: RawTherapee and darktable are non-destructive editors and write their own
sidecars, so the invariant survives the handoff. GIMP is not — if you open a JPG in GIMP and
save, that's your edit, outside this tool's control. `npc doctor` reports when a file's
mtime changed underneath the catalog, so at least it won't be silent.

Modes: `detach` (GUI launches, TUI stays live), `wait` (leave the alternate screen, restore
the terminal, run in foreground, resume cleanly), `term` (output into an overlay pane).

`a` opens a menu of every action with its key. Ships with RawTherapee, darktable, GIMP, and
`xdg-open` entries, active only when the binary exists on `PATH`.

## Export

```
npc export --size 2048 --quality 88 -o ~/web -- 'beach rating>=4'
  --strip          drop metadata
  --keep-structure mirror YYYY/MM/DD under the target
```

Export **only ever creates new files under `-o`**, and refuses to run if the target resolves
inside the library. Resize with `fast_image_resize` (Lanczos3), encode JPEG, then `stamp_export`
carries tags and rating onto the copy unless `--strip`.

**One honest caveat, narrower than I first wrote it**: exporting from a NEF uses the largest
embedded preview — which on your D750 is 6016x4016, the sensor's native resolution, so
resolution is *not* a limitation. What you get is the camera's own JPEG rendering: its picture
control, white balance and noise reduction, not a developed RAW. For anything where the
development matters, use an action calling `rawtherapee-cli` — especially since your `.pp3`
profiles are already sitting next to those files. The subcommand says exactly this in its
`--help` rather than quietly producing something you'd assume was fully processed.

## Trash

```
npc trash -- 'rejected'      move stacks to <library>/.trash/, path preserved
photo restore PATH             put one back
npc trash --list             what's in there
npc trash --empty            the only real delete; counts and confirms
```

A rename within one filesystem: atomic, instant, reversible. Every member moves together —
sidecar, `_NEF_embedded.jpg`, and `.pp3` profile alongside the RAW and JPG — `.trash/` is
excluded from scanning, and the `trashed` table remembers where each came from. This is the
only command that relocates an original; `--read-only` blocks it.

## Query language

Recursive descent to SQL. Barewords are tags; adjacency implies AND:

```
beach and (anna or ben) and rating>=4
beach anna -private
year=2024 camera~D850 untagged
date=2024-06-01..2024-06-30 iso>1600
flagged and not rejected
event~"Ostsee" title~sunset
```

Keys: `tag: rating year date event camera lens iso ext path title description
untagged flagged rejected offline`. Operators `= != > >= < <= ~` and `..` ranges. Each tag
term becomes an `EXISTS` against `stack_tag`, served directly by the reverse index. Values are
always bound as parameters, never interpolated.

One parser backs `npc find`, `npc exec`, every bulk subcommand, and the TUI's `/`.

## Commands

```
npc import PATH        ingest from a card; --in-place, --dry-run
npc scan [ROOT]        incremental; --full to reparse
npc find QUERY         --print0, --files|--stacks, --format, --limit
npc tags               names with counts; --tree for hierarchies
npc tag add|rm NAMES -- (QUERY | paths)
npc rate N             -- (QUERY | paths)
npc mark flag|reject|clear -- (QUERY | paths)
npc set title|description VALUE -- (QUERY | paths)
npc rotate cw|ccw|180  -- (QUERY | paths)      sidecar only
npc events             list; --name ID NAME to name one
npc dupes              duplicate groups
npc export ...         resized copies into a directory outside the library
npc trash ...          trash / restore / list / empty
npc thumbs [-- QUERY]  pre-warm the cache
npc exec ACTION -- QUERY
npc sync               flush queued sidecar writes
npc completion SHELL   shell completion script
npc doctor             drift, orphan sidecars, pending writes, broken actions, cache size,
                         and any original whose mtime changed behind our back
npc tui                the browser

--read-only              global; fails any mutating operation at dispatch
```

`npc find --print0 | xargs -0` stays the escape hatch into your existing workflow.

## Offline behaviour

`vol.rs` resolves the root's mount point (walking up until `st_dev` changes, confirmed against
`/proc/self/mountinfo`) and reads the filesystem UUID from `/dev/disk/by-uuid/*`, falling back
to the label. When that UUID isn't mounted:

- queries answer from the catalog, results marked `offline`
- thumbnails still render from the laptop cache
- tag, rating, title, and flag edits queue in `pending`
- actions, import, export, and trash refuse with a clear message rather than operating on dead
  paths
- the next `scan` or `sync` with the drive attached flushes the queue

The TUI shows `OFFLINE — n pending` so this is never silent.

## Test data

`/home/ram/src/tasks/test-data` — a copy of part of the catalog, so the originals on the SSD
are never involved during development.

```
4188 files, 41 GB, 9 directories, laid out MM/DD (08/01 … 08/09)

  1722  .NEF                      raw
  1250  .JPG                      out-of-camera
  1182  *_NEF_embedded.jpg        extracted previews
    34  *.NEF.pp3                 RawTherapee profiles
     0  .xmp                      no existing sidecars

Nikon D750 (750_NNNN naming). In 08/09 the NEF/JPG/embedded split is exactly 71/71/71.
```

Four things this changes in the plan, all folded in above:

1. **Derived-file folding is mandatory, not a nicety.** 1182 `_NEF_embedded.jpg` files would
   otherwise become 1182 phantom stacks — a bug that would have looked like "the catalog is
   double-counting everything" and been annoying to diagnose after the fact.
2. **`.pp3` files mean RawTherapee is already in the workflow.** They ride with
   their stack through trash/restore but are otherwise ignored -- see the
   note at the end of this document.
3. **`.JPG` and `.jpg` carry different meanings here**, so extension matching classifies
   case-insensitively while preserving the literal name.
4. **Thumbnails get cheaper** — where an `_NEF_embedded.jpg` exists, no Exiv2 call is needed.

### Rules I work under

- **The directory stays read-only.** I'll `chmod -R a-w` it, which honours the constraint and
  doubles as enforcement: any code path attempting a write to an original fails loudly rather
  than succeeding quietly.
- **Reads run directly against it** — `scan`, `find`, `thumbs`, every EXIF and preview path.
  Real D750 maker notes, real files, from day one.
- **Write tests run on a `cp -a` copy in scratch**, so not even legitimate sidecars appear
  beside these files.
- **A hash manifest is the tripwire.** Size, mtime and blake3 for all 4188 files recorded
  before any work starts, re-checked after every test run and before I report anything as
  working. One byte or mtime moves, that's a failure and you hear about it.
- Checked-in fixtures still cover what this set doesn't contain: a sidecar full of foreign
  darktable properties, a NEF with no embedded preview, a truncated file, a filename with
  quotes and a newline.

### What this copy can't exercise

Two gaps, both needing the real drive:

- **Removable-volume detection.** Sitting on internal storage, this resolves to the laptop's
  filesystem UUID, so neither volume identification nor the entire offline path gets tested
  here. Those need a read-only `scan` pointed at the mounted SSD.
- **Library-scale performance.** 4188 files gives solid per-file throughput but says nothing
  about how a full library behaves. I'll extrapolate, then confirm with a real `scan`.

Both are non-mutating, so they're safe to run against the SSD whenever you're comfortable —
which matches your instinct that a run against the target data is the right final check once
the suite passes here.

## Verification

1. **The invariant tests** — the suite run against a `chmod -R a-w` photo directory must pass
   in full; and the hash-manifest tripwire must show every original byte-identical and
   mtime-identical before and after. Plus a source-level check that `writeMetadata()` appears
   only in `write_sidecar` and `stamp_export`.
2. **The preservation test** — a sidecar carrying foreign darktable properties must keep every
   one of them byte-identical after `npc tag add` rewrites `dc:subject`.
3. **Argv injection** — a stack named `DSC 01'23"; rm -rf x.NEF` must reach the child as one
   intact argv element, asserted against a recording stub for every placeholder.
4. **Import safety** — the source directory is byte-identical after import (mtimes included);
   a truncated destination fails verification and is not recorded; interrupting mid-import
   leaves no half-registered stack.
5. **Trash round-trip** — trash, scan, restore, scan; tags and ratings survive intact.
6. **Export containment** — `-o` pointing inside the library is refused; `stamp_export` given
   a library path is refused.
7. **Bridge tests** — round-trip every `FileMeta` field including non-ASCII paths and tags,
   empty strings, absent EXIF, and a NEF with no embedded preview. Under Miri where
   applicable, ASan for the C++ side. FFI is the risk this design adds, so it's tested as one.
8. **Stacking against real data** — scanning `test-data` must produce 1722 stacks, not 2904.
   Every `_NEF_embedded.jpg` folds into its parent and `08/09` resolves to exactly 71 stacks
   of three members each. This is the assertion that catches the phantom-stack bug, and it can
   only be written against your data.
9. **Unit tests** — query parser against golden SQL; volume resolution against synthetic
   mountinfo; change detection against fabricated stat data; event derivation across gap
   boundaries; cache-key invalidation on mtime.
10. **Fuzzing** — `cargo-fuzz` on the query parser and on sidecar parsing.
11. **Interop** — confirm with `exiftool -XMP:Subject -XMP:Rating` that what Exiv2 wrote is
    what a standard reader sees. exiftool is a test oracle, not a runtime dependency.
12. **End to end**, on a copy in scratch — import, scan, tag, find, sync, re-scan and confirm
    tags come back off disk identically. Launch RawTherapee on a selection and confirm it
    opens the NEF and not the extracted preview.
13. **Benchmark** — `scan --full` versus incremental `scan` over the 4188 files, thumbnails
    per second, import throughput. Incremental scan should be bounded by stat throughput; if
    it isn't, change detection is wrong.
14. **Against the SSD, read-only, last** — a `scan` of the real drive to exercise
    volume-UUID detection, the offline path, and library-scale performance. Non-mutating, run
    only once everything above passes, with the hash manifest checked afterwards.

### Running them: local, no CI

`git init` in `npc` for history and bisectability, plus a `check` script running fmt, clippy,
the suite, and the hash tripwire. No pipeline, for two reasons:

- The tests that matter most — the tripwire, the read-only invariant, the real-data stacking
  assertion — depend on 41 GB of your photographs. Making CI green would mean uploading them.
- exiv2 0.27.6 is what this machine ships. A runner on a different image would test a
  different library than the one you run, so a green pipeline could coexist with a broken
  local build.

The split for adding CI later is already in place: the small synthetic fixtures are checked in
and shareable, so a pipeline can run build + clippy + fixture-only tests while the real-data
tests stay local. Worth doing when a second machine or person is involved, not before.

## Milestones

| | |
|---|---|
| **M1** | The invariant first: `--read-only`, the shim's write assertion, and the hash-manifest tripwire, before anything can write. Then the bridge, `vol`, `db`, `scan`, `find`. Catalog builds, queries answer, nothing on the drive can change. |
| **M2** | Sidecar writes: tags, ratings, title, description, flag, reject, orientation. `sync`, offline queue. Round-trips to disk. |
| **M3** | `import` + `dupes` + `trash`/`restore`. The commands that move files, with their safety tests. |
| **M4** | TUI shell — list view, tag and date sidebars, query bar, tagging keys, actions. Usable daily without thumbnails. |
| **M5** | Thumbnails — `extract_preview`, cache, grid and single view. The culling loop. |
| **M6** | `rotate`, `export`, `events --name`, `doctor`, undo, tag hierarchies, shell completion, fuzz targets. |

## Later: pre-compiled releases

Targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`aarch64-apple-darwin`.

This partly reverses an early decision, deliberately. Portability was dropped
during development because it bought nothing and cost a static-link fight;
shipping binaries reintroduces exactly that requirement. Worth naming so the
work is not underestimated:

- **Exiv2 is the whole difficulty.** The Rust half cross-compiles trivially;
  the C++ shim needs a libexiv2 (plus expat and zlib) built for each target.
  Linking against a system libexiv2 is fine for a machine you control and
  useless for a release artifact.
- **Static linking comes back.** Each target wants exiv2 built from source with
  `EXIV2_ENABLE_XMP=ON`, `BUILD_SHARED_LIBS=OFF`, and BMFF/curl/NLS off, then
  linked in. That is a per-target build step, not a cargo flag.
- **darwin-aarch64 cannot be cross-compiled from Linux** in any way worth
  maintaining. It needs a macOS runner. Linux aarch64 can be done with a
  sysroot or a QEMU container, but a native arm runner is less painful.
- **The XMP toolkit must be verified per artifact.** A build that silently
  loses XMP support would pass every test that does not touch sidecars, and
  then destroy metadata in the field. `have_xmptoolkit` should be asserted at
  build time for each target, and the preservation test run against each
  produced binary rather than only against the development build.

Practical shape: one CI job per target producing a static binary, plus a
smoke-test job that runs the invariant and preservation suites against the
artifact itself. That is also the point at which the "no CI" decision above
should be revisited -- releases are the use case that justifies it, not tests.

## What changed during the build

The plan was mostly right. Four things were not, and are recorded here rather
than quietly corrected above:

- **Stack count.** The plan asserted 1722; ground truth is 1790, because 68
  JPGs have no NEF. Computed in shell before writing the code.
- **Thumbnail sources.** The plan called the `_NEF_embedded.jpg` files a
  shortcut. They are the *full-resolution* preview tier, so using them cost
  4.7x. Letting Exiv2 pick a tier from the RAW is the cheap path.
- **`.pp3` tracking and the `edited` key.** Both removed. Profiles are
  RawTherapee's business; the tool now uses a closed whitelist of `.nef`,
  `.jpg`/`.jpeg` and `.xmp`.
- **The offline path.** Written in M2, described here, and broken from the
  first line until the drive was physically unplugged in M6. Resolving a
  volume canonicalises the path, which cannot work when the path is gone.

## Milestones (continued)

M1 deliberately builds the safety machinery before the features, so that every later milestone
is developed under a harness that makes violating the invariant a test failure rather than a
discovery. It also front-loads the two risks specific to this stack: proving the FFI boundary
carries real data, and measuring first-scan speed with Exiv2 serialized.
