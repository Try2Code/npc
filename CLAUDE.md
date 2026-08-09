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

cargo build --release        binary at target/release/npc
cargo cov-summary            coverage numbers only
```

If `~/.cargo/bin` is not on PATH, export it first. `check` must stay
executable — rewriting it with a script drops the bit.

The library to operate on comes from `library` in the config, or `--root`.
There are no hardcoded paths in the binary; adding one breaks it for every
machine but the author's.

Currently 177 tests, ~84% region / 86% line coverage.

## Layout

```
src/meta/shim.cc   the only C++, and the only writer   <- read this first
src/meta/mod.rs    cxx bridge + the Mutex serialising Exiv2
src/stack.rs       filename -> stack. Closed whitelist: nef, jpg/jpeg, xmp
src/scan.rs        walk, change detection, event derivation
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
  none, and the real-data tests skip with a message telling you how to get
  them rather than failing.

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
- A **black-frame rendering bug** on two specific files is unconfirmed. The
  thumbnail cache was proved clean by measurement; the fix — releasing
  off-screen images every frame — remains a hypothesis. `r` re-sends images to
  the terminal; if that clears it, the diagnosis was right.
- **Other applications write to originals.** Shotwell embeds ratings directly
  into JPGs, so anything sharing a library with this tool will disagree about
  ratings: Shotwell embeds, npc uses sidecars. `doctor` reports originals whose
  size or mtime has moved.
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
