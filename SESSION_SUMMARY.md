# Session summary — npc production scan, TUI improvements, rating-mismatch backfill

_Written 2026-08-14, covering the conversation up to the `/compact` point._

## 1. Primary Request and Intent

The user worked through several major, sequential goals:

1. **Scan a production Nikon photo library** with npc — library path `/media/ram/MyPics` (821GB, 121,725 files).
2. **Add linting as a standing/automatic check** to the npc codebase (not just on-demand via `./check`).
3. **Fix CLI defects** discovered while preparing for the scan (`--root` inconsistency, dead `--read-only` flag).
4. **Fix a tag/rating gap**: this library had been actively used by Shotwell, which embeds tags/ratings directly into JPG/NEF files; npc's scanner never read or seeded these into its catalog. Rating tie-break rule given verbatim: "when only nef xor jpg has a rating, use it. if both have a rating, tag this higher one." Explicitly rejected a separate migration subcommand — wanted it folded directly into scan/sync.
5. **TUI improvements**: pagination past 500 rows, readline support for query editing in TUI mode, and in-place query editing. Also: "make sure there is a guard to ensure the thumbs are there" (the black-frame bug).
6. **Git workflow**: stage and commit all fixes (organized logically), then push everything to origin.
7. **RawTherapee configuration question** (external tool, not npc code): how to make RawTherapee always copy the input's rating to exported files.
8. **Find and fix rating inconsistencies between exported JPGs and their true source stacks**: scan for cases where exports don't carry the rating of the original JPG/NEF, then backfill the sidecars.
9. **Generate a reusable script** to scan and fix this rating mismatch again later; moved into `scripts/` in the repo per direct instruction.
10. Ran the script with `--apply` after explicit authorization.
11. Follow-up questions on shutter_count query syntax, meaning of `*` in output, and a specific real-world example (`750_3311_modified.jpg` in `export/4gg/`) where the user visually confirmed a rating mismatch still exists.

## 2. Key Technical Concepts

- **npc's write invariant**: exactly two `writeMetadata()` calls exist in the C++ shim (`write_sidecar`, `stamp_export`), enforced by `tests/invariant.rs::writemetadata_appears_only_inside_the_guarded_writers`. Sidecars (`.xmp`) are the only write target; originals are never touched.
- **Stack/role model**: `stack(dir, base)` unique key; `file` table with `role` (raw/oocjpg/derived/sidecar), `serial`, `shutter_count`, `capture_time`, etc.
- **Exiv2 C++ shim** (`src/meta/shim.cc`) bridged via `cxx` crate; `FileMeta` struct crosses the FFI boundary.
- **Tiered matching for cross-derivative identity**: exact `serial`+`shutter_count` (tier 1, same evidence as npc's own `dupes.rs`), falling back to `capture_time` ONLY when unique among candidates (tier 2) — never guessing on ambiguous timestamps, since 18,937 distinct capture_times were found to be shared across multiple stacks (burst-mode collisions) in this real library.
- **A third, unhandled failure mode**: an export matching NEITHER tier (no serial/shutter AND no unique/any capture_time) is absent from the report entirely — not even flagged as ambiguous. This was explicitly identified as a known gap and confirmed live via `export/4gg/750_3311_modified.jpg` (see below).
- **npc query language**: `key op value` predicates compiled to SQL; e.g. `shutter` maps to `shutter_count` column via `EXISTS (SELECT 1 FROM file f WHERE f.stack_id = s.id AND f.shutter_count {op} {value})`.
- **`Target.paths` CLI resolution bug**: uses `path.file_stem()` which strips only the LAST extension — breaks for filenames with multiple dots (e.g. `X.tif.out.jpg`). Worked around by always passing full on-disk filenames rather than bare catalog `base` values.
- **Rust SIGPIPE handling**: Rust CLIs panic on broken pipe rather than exiting quietly like C programs — fixed in the script by capturing full output before parsing.
- **ratatui-image / kitty graphics protocol**: `StatefulProtocol` caching; stale placements occur when a cached protocol is reused at a new screen position or after a terminal resize without forcing retransmission.
- **Readline-style line editing**: char-indexed cursor (not byte-indexed, for UTF-8 safety), standard Ctrl-key bindings, explicit avoidance of Alt-based bindings for terminal portability.

## 3. Files Changed

- `src/main.rs` — removed `--read-only`; added unified `RootArg` (positional or `--root`) for `Scan`/`Status`/`Tui`; scan summary line for seeded stacks.
- `src/scan.rs` — `combine_rating`/`union_embedded_tags` pure functions; seeding wired into stack-write loop, gated on `!has_sidecar` (critical guard — prevents resurrecting user-cleared tags/ratings by blocking the later unconditional sidecar-ingestion pass).
- `src/meta/mod.rs` — added `pub tags: Vec<String>` to `FileMeta`.
- `src/meta/shim.cc` — reads `Xmp.dc.subject` / `Iptc.Application2.Keywords`; fixed a dangling-temporary bug in tag dedup (`std::equal` over two separately-constructed temporaries); added XMP rating fallback (`Xmp.xmp.Rating`) when `Exif.Image.Rating` is absent.
- `src/edit.rs` — `normalise_tag` made `pub(crate)` for reuse; pedantic-lint cast fixes.
- `src/tui.rs` — pagination (`move_cursor` turns pages at boundaries; `jump_to_first`/`jump_to_last` for `g`/`G`/`Home`/`End`); full readline editing (`input_cursor` field, Ctrl-a/e/b/f/k/u/w/d, Left/Right/Home/End/Delete/Backspace) replacing append/backspace-only editing; black-frame fix (`last_rect: HashMap<PathBuf, Rect>`, `ensure_decoded_at`, `clear_images`, `on_resize` wired to `Event::Resize`); ~10 new tests; `HELP` const updated.
- `tests/invariant.rs` — added `every_declared_cli_flag_is_actually_read`.
- `tests/real_data.rs` — count bumped 12 → 18; added tests for embedded rating/tag round-trip, seeding-then-sync, the sidecar-resurrection guard (initially failed, proving the guard necessary), XMP-only rating, and tag dedup across sources.
- `Cargo.toml` — added `[lints.clippy]` with `pedantic = warn` plus targeted allows.
- `.githooks/pre-commit` (new) — runs `./check quick`; enabled via `git config core.hooksPath .githooks`.
- `scripts/fix-export-ratings` (new) — reusable dry-run-by-default script implementing the tiered matching + backfill; requires `--apply` to write; uses real on-disk filenames throughout to avoid the `file_stem()` bug; avoids the SIGPIPE panic.
- `CLAUDE.md`, `README.md`, `PLAN.md` — updated for `--root` uniformity, `--read-only` removal, pre-commit hook, new TUI keybindings, black-frame bug status.

## 4. Notable Bugs Found and Fixed

1. `move_cursor` didn't reset cursor to 0 when there was no previous page (caught by `page_keys_move_by_a_real_screen`).
2. `move_cursor` offset arithmetic used `self.rows.len()` (current page's count) instead of the `PAGE` constant — wrong on a partial last page (caught by `moving_before_the_first_row_of_a_page_turns_back`).
3. Missing `!has_sidecar` guard in scan.rs seeding — caused `a_full_rescan_does_not_resurrect_a_tag_cleared_via_the_sidecar` to fail; root cause was marking a stack dirty during seeding blocking the later unconditional sidecar-ingestion pass.
4. C++ dedup bug: `std::equal` called on two separately-constructed `std::string(existing)` temporaries never matched — found via direct instrumentation, fixed by binding to one named local.
5. Missing XMP rating fallback in `read_meta` — some tools only write `Xmp.xmp.Rating`, not `Exif.Image.Rating`.
6. Git mis-staging during `git add -p` hunk-splitting — caught via `git show --stat HEAD`; fixed with `git reset --soft HEAD~1` (safe, unpushed) and redone as two correct commits.
7. `npc rate` path parsing: `Target.paths` resolution strips only the last extension via `file_stem()`, mangling filenames with multiple dots (e.g. `750_4333_modified.tif.out.jpg`). Worked around using real on-disk filenames; the reusable script was designed from the start to avoid this.
8. Capture-time matching was unreliable at full scale: 18,937 distinct capture_times shared across multiple stacks (burst shooting) — tier 2 restricted to only unique timestamps.
9. Broken pipe panic in the reusable script (`npc status | head -1`) — fixed by capturing full output before parsing.
10. **Open at time of writing**: exports matching neither tier (no serial/shutter, no capture_time at all) are silently absent from every report — confirmed as a live instance via `/media/ram/MyPics/export/4gg/750_3311_modified.jpg`, which has no serial, no shutter_count, and no capture_time whatsoever. True source confirmed as `/media/ram/MyPics/2025/12/11/750_3311.JPG` (serial=6094708, shutter_count=28841, capture_time=1765479452, rating=1) via matching capture_time against the sibling export `export/bw/750_3311_modified.jpg` (capture_time=1765479452, rating=0 in catalog, `.xmp` sidecar shows `xmp:Rating="1"` was already written for the `bw` one — so `bw` was already fixed by a prior backfill round; `4gg` was not, and cannot be, by tier-based matching alone).

## 5. Pending / Next Steps (as of the compaction point)

- **Add a third reporting category to `scripts/fix-export-ratings`**: "no reliable signal, can't verify" — offered to the user, not yet actioned. This should report a count of unmatched exports instead of staying silent, so a clean report doesn't imply "all exports are fine."
- **Fix `export/4gg/750_3311_modified.jpg` by hand**: confirmed true source is `2025/12/11/750_3311.JPG` (rating=1). Since it has zero identifying metadata (no serial, no shutter_count, no capture_time), no automated tier can safely match it — this one needs a direct, manually-specified fix (e.g. `npc rate 1 --root /media/ram/MyPics -- export/4gg/750_3311_modified.jpg`), not a script run.
- Both of these were proposed to the user; awaiting confirmation/instruction to proceed.

## 6. Design Note Still Live

There is an approved plan on disk at `/home/ram/.claude/plans/i-want-to-scan-jazzy-goose.md` covering TUI pagination, readline editing, and the black-frame fix — all three parts of that plan were implemented and tested during this session (see `src/tui.rs` changes above). If anything in that plan's verification checklist (particularly the manual real-kitty-terminal checks in Part 3, item 7) hasn't yet been confirmed by the user, that confirmation is still outstanding.
