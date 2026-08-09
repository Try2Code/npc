# npc — working notes

A terminal photo catalog for a Nikon library on a removable SSD. Tag, rate,
browse with thumbnails, hand selections to RawTherapee. Rust, with Exiv2
reached through a `cxx` bridge to one small C++ shim.

`PLAN.md` is the design document, including a section at the end recording
where it was wrong. Read that section before trusting the rest of it.

---

## The one rule

**No code path opens an original image file for writing.** Not by default, not
behind a flag, not with a confirmation. Pixels and metadata are decoupled:
`.NEF` and `.JPG` are read-only inputs, everything this tool produces lives in
a separate `.xmp` sidecar.

This is enforced, not promised:

- `shim.cc` has exactly **two** calls to `writeMetadata()` (a third mention is
  the comment marking one of them). `write_sidecar`
  refuses any path not ending in `.xmp`; `stamp_export` writes only to a copy
  the tool just created under a target proved to be outside every library.
- `tests/invariant.rs` asserts that count. **A third call fails the test.** If
  you need one, make the argument in the test's doc comment first.
- Protected roots (`config.toml`) refuse whole trees, including refusing to
  *launch external editors* against them — a guard that stops at the boundary
  of our own code is not a guard.

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

`~/.cargo/bin` is not on the default PATH here; export it first. `check` must
stay executable — rewriting it with a script drops the bit.

The library comes from `library` in the config, or `--root`. There are no
hardcoded paths left in the binary.

Currently: 176 tests, ~83% region coverage.

## Layout

```
src/meta/shim.cc   the only C++, the only writer      <- read this first
src/meta/mod.rs    cxx bridge + the Mutex serialising Exiv2
src/stack.rs       filename -> stack. Closed whitelist: nef, jpg/jpeg, xmp
src/scan.rs        walk, change detection, event derivation
src/db.rs          schema, migrations, offline catalog resolution
src/query.rs       query language -> SQL, plus its own reference text
src/thumbs.rs      preview extraction, cache, sizing
src/tui.rs         the browser (largest file; list/grid/single)
src/doctor.rs      consistency checks
```

## Things established by measurement — don't re-derive

Each of these was got wrong first, then measured:

- **Thumbnails come from the RAW, not from `_NEF_embedded.jpg`.** Those files
  look like a shortcut and are the *full-resolution* 6016×4016 tier. Letting
  Exiv2 pick a tier is 4.7× faster (105s → 22s over the library).
- **`view_px` is 1620 to match the NEF's preview tier.** Anything higher forces
  the 24 MP tier and costs 140 ms/frame instead of 24 ms.
- **Derived previews must fold into their parent stack**, or every shot with
  an extracted preview becomes two. `tests/real_data.rs` asserts this on real
  files, with ground truth computed in shell rather than from the code.
- **exiv2 is 0.27.6, not 0.28.** `Image::AutoPtr`, not `UniquePtr`. XMP
  toolkit is present and verified.
- **Cache keys include the pixel size.** Omitting it silently served stale
  thumbnails after a size change.
- **Orientation is applied at thumbnail generation.** Nikon writes upright
  previews with an orientation tag; skipping it shows half the library
  sideways.

## Working agreements

- **Never scan the user's filesystem to orient yourself.** Ask for the path.
  This was asked for explicitly, more than once.
- **`/media/ram/MyPics` is the real library and must stay untouched.** It is a
  protected root. It is exFAT, so `chmod -R a-w` is not available as a
  backstop — the code is the only guard.
- **`/home/ram/src/tasks/test-data-min` is the test set the suite uses.**
  11 files, 98 MB, chosen so every property under test appears exactly once.
  Ground truth for it is 5 stacks / 3 raw / 4 oocjpg / 2 derived / 2 events.
- **`/home/ram/src/tasks/test-data` is the full throwaway copy**, 4188 files
  and 41 GB. `NPC_TEST_LIBRARY=<path> cargo test` runs the property tests
  against it; the exact-count tests skip themselves, because those numbers are
  ground truth for one directory and meaningless anywhere else.
- **Run `./check` before reporting anything as done.** Twice in this project a
  commit was made before reading test output, and twice the tests were right.
- Report failures plainly, with the output. Don't soften.

## Known gaps

- **Miri and ASan have never been run** on the FFI boundary (PLAN.md
  verification item 7). That boundary is the risk this design added.
- A **black-frame rendering bug** on two files (`750_6901`, `750_6904`) is
  unconfirmed. The cache was proved clean by measurement; the fix (releasing
  off-screen images each frame) is a hypothesis. `r` re-sends images — if that
  fixes it, the diagnosis was right.
- **Shotwell writes ratings in-place** into the user's JPGs. Seven files
  already carry embedded `IFD0:Rating`. The two tools will disagree about
  ratings: Shotwell embeds, npc uses sidecars.
- Branch is `master`; GitLab's default is `main`.

## Testing notes

- `tests/real_data.rs` runs against the real library and asserts counts that
  are properties of the *photographs* (1790 stacks, 1722 raw, 1250 oocjpg,
  1182 derived). It deliberately does **not** assert `.pp3` or `.xmp` counts —
  those grow when the tool is used, and a test that fails because someone used
  the program is a broken test.
- The TUI is tested two ways: interaction logic headlessly against real
  `KeyEvent`s, and rendering through ratatui's `TestBackend` including cramped
  20×6 terminals. A pty smoke test was tried and abandoned — piping into
  `script` does not reach crossterm.
- Fuzzing is for totality, not injection. Two attempts at "no quote reaches
  the SQL" both fired on the tool's own `COALESCE(x,'')` templates. Injection
  is pinned deterministically in `values_are_never_interpolated_into_sql`.
- `exiv2 -ep` writes next to the **source** file, not the working directory.
  It once left two extracted previews in the library and broke the suite.
