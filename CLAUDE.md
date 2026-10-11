# npc — notes for whoever picks this up

A terminal photo catalog for a Nikon RAW+JPG library on a removable drive.
Rust, with Exiv2 reached through a `cxx` bridge to one small C++ shim.

`README.md` is for users. `PLAN.md` is the original design, and carries a
section at the end recording where that design turned out to be wrong — read
that before trusting the rest of it.

---

## The one rule

**No code path opens an original image file for writing.** Not by default, not
behind a flag, not with a confirmation. Pixels and metadata are decoupled:
`.NEF` and `.JPG` are read-only inputs, and everything this tool produces lives
in a separate `.xmp` sidecar.

This is enforced rather than promised:

- `shim.cc` contains exactly **two** calls to `writeMetadata()` (a third
  textual match is the comment marking one of them). `write_sidecar` refuses
  any path not ending in `.xmp`; `stamp_export` writes only to a copy the tool
  has just created, under a target already proved to be outside every library.
- `tests/invariant.rs` asserts that count. **A third call fails the test.** If
  one is genuinely needed, make the argument in the test's doc comment first —
  that is what the count is for.
- Protected roots refuse whole trees, *including* refusing to launch external
  editors against them. A guard that stops at the boundary of our own code is
  not a guard: RawTherapee writes a `.pp3`, an image viewer embeds a rating,
  and neither asks permission.

If a change makes any of this harder to verify, the change is wrong.

## Commands

```
./check              fmt, clippy, tests, tripwire     <- before claiming done
./check coverage     + HTML report in target/coverage
./check fuzz         + 30s per fuzz target (needs nightly)
./check quick        skips the slow real-library tests

git config core.hooksPath .githooks     <- runs `check quick` before each commit

cargo build --release        binary at target/release/npc
cargo cov-summary            coverage numbers only
```

If `~/.cargo/bin` is not on PATH, export it first. `check` must stay
executable — rewriting it with a script drops the bit. Its tests build under
the `release-nolto` profile (release opt-level, no LTO): LTO is paid at link
time per test binary and took a full `./check` from 48 s to 4m38s.

The library to operate on comes from `library` in the config, or from the root
given on the command line. Every subcommand accepts `--root`; `scan`, `status`
and `tui` also take it positionally.
There are no hardcoded paths in the binary; adding one breaks it for every
machine but the author's.

Currently 385 tests. Coverage was ~84% region / 86% line at 177 tests and
has not been re-measured since.

## Layout

```
src/meta/shim.cc   the only C++, and the only writer   <- read this first
src/meta/mod.rs    cxx bridge + the Mutex serialising Exiv2
src/stack.rs       filename -> stack. Closed whitelist: nef, jpg/jpeg, xmp
src/scan.rs        walk, change detection, event derivation, rating inheritance
src/import.rs      card -> library: two-level identification, verified copy
src/db.rs          schema, migrations, offline catalog resolution
src/query.rs       query language -> SQL, plus its own reference text
src/thumbs.rs      preview extraction, cache, sizing
src/tui.rs         the browser (largest file; list / grid / single)
src/doctor.rs      consistency checks
```

## Established by measurement — do not re-derive

Every one of these was got wrong first and then measured. The intuitive answer
is the wrong one in each case:

- **Thumbnails come from the RAW, not from `_NEF_embedded.jpg`.** Those files
  look like a shortcut and are the *full-resolution* preview tier. Letting
  Exiv2 select a tier is 4.7x faster.
- **`view_px` matches the RAW's preview tier** (1620 for a D750). Asking for
  more forces the full-resolution tier: 140 ms per frame instead of 24 ms.
- **Derived previews must fold into their parent stack**, or every shot with
  an extracted preview becomes two.
- **Cache keys must include the pixel size.** Omitting it silently served
  stale thumbnails after a size change, reporting `generated 0, cached 1790`
  on a run that should have rebuilt everything.
- **Orientation is applied when the thumbnail is generated.** Nikon writes
  upright previews carrying an orientation tag; skipping it shows a
  portrait-heavy library sideways.
- **exiv2 0.27 uses `Image::AutoPtr`**, 0.28 uses `UniquePtr`; the shim
  version-guards. XMP support is asserted at build time, because an Exiv2
  built without it passes every test that does not touch sidecars and then
  destroys metadata in the field.
- **Import identity is per file *and per role*.** Serial + shutter count is
  shot-level: a RAW and its JPG share it. Mapping it to one file per shot
  (the RAW) made every card JPG miss on size and fall through to reading
  both files in full — 35 min and ~78 GB to plan one JPG card, where the
  user shoots RAW to one card and JPG to the other.
- **Import level 1 is the catalog, not the card.** Name (extension
  included) + size alone never matched two *different* shots across 115k
  real files. The *card's* name, size and mtime are recorded at import
  (`file.src_*`), because library copies get rewritten afterwards — ~200
  by Shotwell on two cards — and their own size no longer matches. Planning
  two fully imported cards: 1.7 s. Basename alone never decides: a new shot
  never joins a stem already used in its day folder.
- **A camera identity that matches nothing is authoritative.** Rating
  inheritance fell back to the basename whenever identity found no match —
  which is every fresh shot — and handed new imports ratings from old shoots
  with the same recycled number (`750_0001` was rated in six past sessions).
- **A rating key must not write.** Per key it was ~65 ms for the sidecar plus
  ~140 ms reloading the whole result list (95k stacks); the catalog update
  itself is 0.05 ms. The browsers now `apply` only and update the row in
  place; `edit::Writer` writes the `pending` queue one sidecar per idle step
  and quitting waits for it. Keep it on the UI thread with the UI's own
  connection — a second writer would race edits made between steps.
- **Import catalogs only what it copied**, card by card, before planning the
  next card (so a RAW on card 2 joins its JPG from card 1). The old full-
  library scan after every import cost 8–10 min to find nothing more.

## Test data

The suite runs against **real photographs**, not fixtures. Maker notes,
preview pyramids and orientation tags are exactly what fixtures lack, and each
has caused a bug here.

- **`./fetch-test-data`** downloads them into `./testdata` — 11 files, 98 MB,
  checksum-verified before extraction. Every property under test appears
  exactly once; `tests/real_data.rs` documents what each file is for. Ground
  truth was computed in shell over the files, not read out of the code.
- `NPC_TEST_LIBRARY=<path>` runs the property tests against a larger library.
  The exact-count tests skip themselves there, because those numbers are
  ground truth for one directory and mean nothing anywhere else.
- The data is **not in the repository**, being 98 MB of RAW. A fresh clone has
  none.
- **A sentinel test fails when the photographs are absent.** Rust has no
  skipped state, so twelve guarded tests returning early would report as
  twelve passes — a green suite that verified nothing, which is the exact
  failure this project keeps designing against. `NPC_ALLOW_MISSING_TEST_DATA=1`
  acknowledges it deliberately. Do not reach for that to make a red suite go
  away.

## Working practice

- **Do not scan the filesystem to orient yourself. Ask for the path.** This
  was asked for explicitly, more than once.
- **Treat every configured protected root as read-only, permanently** — and
  assume it may sit on a filesystem without Unix permissions, where the code
  is the only guard there is.
- **Run `./check` before reporting anything as done.** Twice in this project a
  commit was made before reading test output, and both times the tests were
  right.
- **Compute ground truth independently** before trusting a number written in
  the plan. The plan's central figure was wrong; this habit is what caught it.
- **Measure performance rather than reasoning about it.** The intuitions here
  were wrong in both directions and by factors of five.
- Report failures plainly, with the output. Do not soften.

## Known gaps

- **Miri and ASan have never been run** on the FFI boundary (`PLAN.md`
  verification item 7). That boundary is the risk this design introduced, and
  it is the one thing the plan said to test as such and then did not.
- A **black-frame rendering bug**, reproduced at real-library scale (92k
  stacks), was diagnosed rather than left as a hypothesis: `r` (full cache
  clear) reliably fixed it, confirming a stale kitty-protocol placement
  rather than a bad thumbnail. Two concrete triggers were found and fixed —
  `ensure_decoded_at` now forces a fresh decode whenever a path is drawn at a
  new screen rect (ordinary scrolling reshuffles tile positions constantly),
  and any terminal resize now clears the whole image cache (`App::on_resize`,
  wired to `Event::Resize`), since a resize can invalidate the picker's
  cell/font geometry. `r` still exists as a manual fallback. **Not yet
  confirmed against a real kitty terminal** — that verification can't be
  automated (a pty smoke test was tried and abandoned, see Testing notes
  below) and is the one thing left to check by using the browser normally.
- **Other applications write to originals.** Shotwell embeds ratings directly
  into JPGs, so anything sharing a library with this tool will disagree about
  ratings: Shotwell embeds, npc uses sidecars. `doctor` reports originals whose
  size or mtime has moved. Import copes (card stats in `file.src_*`, metadata
  merge at level 2), but the library copies are no longer the camera's bytes.
- **The catalog is at schema 6** (since 0.5.1). A build older than that refuses
  to open it — update `npc` and `npcg` together. Rows imported before 0.5.0
  have no card stats until a card holding them is imported again.
- **No license file**, so the code is not legally reusable as published.
- Branch is `master`.

## Testing notes

- `tests/real_data.rs` asserts properties of the *photographs* — stack counts,
  role counts, events. It deliberately does **not** assert `.pp3` or `.xmp`
  counts: those grow whenever the tool is used, and a test that fails because
  someone used the program is a broken test. This was got wrong twice.
- The TUI is tested two ways: interaction logic headlessly against real
  `KeyEvent`s, and rendering through ratatui's `TestBackend`, including
  cramped 20x6 terminals where a layout panic would wreck a real terminal. A
  pty smoke test was attempted and abandoned — piping into `script` does not
  reach crossterm, and a test that proves nothing is worse than none.
- Fuzzing is for totality, not injection. Two attempts at "no quote from the
  input reaches the SQL" both fired on the tool's own `COALESCE(x,'')` and
  `date(t,'unixepoch')` templates. Injection is pinned deterministically by
  `values_are_never_interpolated_into_sql` with a known-hostile string.
- `exiv2 -ep` writes next to the **source** file, not the working directory.
  It once left two extracted previews inside a library and broke the suite.
