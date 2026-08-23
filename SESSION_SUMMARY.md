# Session summary — TUI timeline bugfixes, grid perf, and an egui GUI spike

_Written 2026-08-15, covering the conversation up to the end-of-day stop._

## 1. Primary Request and Intent

Continuing from a prior compacted session (see the previous summary content
this file replaces, which covered the production scan, rating-mismatch
backfill, and the TUI timeline panel's initial implementation). This session:

1. Confirmed the export-rating manual fix (`export/4gg/750_3311_modified.jpg`)
   had already been applied in a prior, uncaptured session — verified via its
   `.xmp` sidecar (naming caveat: npc strips only the last extension, so the
   sidecar is `750_3311_modified.xmp`, not `.jpg.xmp`).
2. Fixed a real bug in the new TUI timeline panel: picking a second bucket
   (or a sidebar tag/date) overwrote the whole query, discarding any filter
   manually added via `/`.
3. Installed `npc` globally via `cargo install --path . --bin npc` (later
   just `cargo install --path .` since `npcg` didn't exist yet), so the user
   runs `npc` directly rather than `target/release/npc`.
4. Diagnosed and fixed slow TUI grid navigation, in three real, separate
   contributing causes, each found by direct measurement/instrumentation
   rather than guessing (per project practice):
   - `grid_px` in the user's own config was tuned from 1280 → 500 (the
     user's own screen-based math in the config comment didn't account for
     the sidebar/borders eating into actual tile width).
   - The TUI's `ensure_decoded_at` was re-reading and re-decoding the source
     JPEG from disk on every scroll step (rect-based cache invalidation,
     originally added to fix a black-frame kitty-placement bug), even though
     nothing about a rect change invalidates the *pixels* — only the
     placement-bound protocol. Split into a `decoded` (pixel) cache and an
     `images` (placement) cache; rect changes now only rebuild the latter.
   - Remaining ~780ms of a ~950ms 27-tile page-turn is `ratatui-image`'s
     Kitty encoder transmitting raw, uncompressed RGBA pixel data
     (`f=32`, not PNG) — a third-party crate limitation, confirmed via
     targeted timing instrumentation (added, used, then reverted).
5. Merged `feature/tui-timeline` into `master` (fast-forward, pushed).
6. At the user's request, explored whether the TUI's layout could live in a
   native ("Qt-style") window instead — recommended `egui`/`eframe` — then
   built it as a real, separate branch (`feature/gui`, **not** pushed),
   producing a new binary `npcg` (`src/gui.rs` + `src/bin/npcg.rs`) that
   reuses all of npc's existing non-UI backend (`db`, `query`, `thumbs`,
   `edit`, `config`, `vol`) and installs independently
   (`cargo install --path . --bin npcg`) so it runs side by side with the
   master-built `npc` for comparison.
7. Iterated live on `npcg` against the user's real library
   (`/media/ram/MyPics`), fixing four real bugs found only by the user
   running it (this environment has no display): grid row misalignment,
   the query box not submitting on Enter, Page Up/Down and mouse-wheel
   scrolling, a synchronous-thumbnail-generation freeze on a query that hit
   many uncached stacks, and a grid-cell width bug (confirmed via a
   screenshot the user provided) that left a large dead gap and clipped the
   second column. Also added star-rating support (`0`-`5` keys), confirmed
   working live.
8. **Decision: `feature/gui` is parked, not deleted.** The original question
   — does native GPU-texture rendering avoid the kitty-protocol
   transmission cost — is answered yes, confirmed live. The user does not
   want to keep fighting egui's layout model; `tui.rs` remains the primary
   interface. Both `feature/gui` (local only) and `feature/tui-timeline`
   (local + remote, now fully merged into `master`) still exist as branches;
   neither was deleted.

## 2. Key Technical Concepts

- **`ratatui-image`'s Kitty protocol sends raw, uncompressed RGBA**
  (`transmit_virtual` in its `kitty.rs`, format `f=32`), not PNG. This is
  the structural reason the TUI's grid page-turn is slow on a kitty-protocol
  terminal (confirmed: Ghostty) and is not fixable in npc's own code without
  patching that dependency.
- **egui's `ScrollArea` auto-shrinks to fit content by default**
  (`auto_shrink: Vec2b::TRUE`); `egui::Grid` sizes itself tightly to its
  cells rather than filling the panel, so the two combined shrink the
  scroll area's mouse-hit region to the tile block, breaking wheel-scroll
  over the leftover margin. Fixed with `.auto_shrink([false, false])`.
- **`ui.vertical(|ui| {...})` with no prior content claims the full
  remaining row width** as its own size when used inside a `Grid` cell, and
  `Grid` measures column width from that claimed width, not from a
  fixed-size widget drawn inside it. Fixed with `ui.set_width(TILE_PX)` as
  the closure's first statement. Confirmed via a user-provided screenshot,
  not guessed.
- **`egui::Panel`/`CentralPanel::show` take `&mut Ui`, not `&egui::Context`**,
  and `eframe::App` requires `fn ui(&mut self, ui: &mut egui::Ui, frame)`,
  not `fn update(&mut self, ctx, frame)` — this particular egui/eframe
  version (0.36.1, resolved fresh via `cargo add` this session) has a
  meaningfully different panel/App API than older egui docs/memory suggest;
  verified directly against the locally-fetched crate source throughout
  rather than assumed.
- **`Cache::get` (async, background worker thread) vs `Cache::make_now`
  (synchronous)**: the TUI always used the former; `npcg` initially used the
  latter for simplicity and froze the whole window on a query that matched
  many stacks never before thumbnailed at the current `grid_px`. Fixed by
  switching to `Cache::get` plus a `...` placeholder for pending tiles and
  `ctx.request_repaint_after(...)` each frame (eframe doesn't redraw on a
  timer the way the TUI's 120ms terminal-event-poll loop does, so a
  background-thread completion needs an explicit repaint request to ever be
  shown).
- **`ctx.egui_wants_keyboard_input()`**: needed to gate npcg's global
  arrow/digit key handlers, since without it, typing a query like
  `rating>=3` into the one text field also moved the grid selection and
  rated the current tile.
- **npc's sidecar naming strips only the last extension**: a `.jpg` file's
  sidecar is `<name>.xmp`, not `<name>.jpg.xmp` — caused a false "not fixed
  yet" read early this session before the correct path was checked.

## 3. Files Changed

- `src/tui.rs` — `splice_filter` (timeline/sidebar Enter handlers no longer
  clobber a manually-added query fragment); `decoded: HashMap<PathBuf,
  DynamicImage>` field split out from `images`/`last_rect`, with matching
  changes to `ensure_decoded`, `ensure_decoded_at`, `clear_images`, and the
  per-frame `frame_used` retain block; several new/rewritten tests for both.
- `~/.config/npc/config.toml` (outside the repo) — `grid_px` 1280 → 500,
  comment rewritten to explain why (sidebar/borders eat into the naive
  screen-width math).
- `Cargo.toml`, `Cargo.lock` — `eframe`, `egui` added (branch `feature/gui`
  only; not present on `master`).
- `src/lib.rs` — `pub mod gui;` (branch `feature/gui` only).
- `src/gui.rs` (new, `feature/gui` only) — the `npcg` app: query box,
  `egui::Grid` thumbnail grid, click/arrow/Page Up-Down selection and
  scrolling, `0`-`5` star ratings via `edit::apply`/`edit::flush`, async
  thumbnail loading via `thumbs::Cache::get`.
- `src/bin/npcg.rs` (new, `feature/gui` only) — thin entry point, resolves
  the library root the same way `main.rs::library()` does, calls
  `npc::gui::run`.

## 4. Notable Bugs Found and Fixed

1. Timeline/sidebar `Enter` overwrote the whole query, discarding a `/`-added
   filter — root cause and fix in `splice_filter` (§2/§3 above).
2. `ensure_decoded_at`'s rect-based cache invalidation re-read the source
   file from disk on every scroll step even when only the screen position
   changed — split `decoded` from `images`/`last_rect`.
3. `grid_px` was 2.5x oversized relative to what a grid tile actually
   displays on this user's screen once the sidebar/borders are accounted
   for — confirmed by direct measurement of real cached thumbnail files
   (853×1280px, 32KB), not assumed.
4. **`npcg`, all found live by the user, none reproducible in this
   environment (no display)**:
   - Grid tiles vertically offset within a row (`horizontal_wrapped` is a
     flow layout with no shared row baseline; switched to `egui::Grid`).
   - Query box never submitted on Enter (the documented egui idiom,
     `response.lost_focus() && Enter`, didn't fire reliably in this egui
     version; replaced with an unconditional Enter-pressed-anywhere check).
   - Fixing the grid layout broke mouse-wheel scrolling (`ScrollArea`
     auto-shrink interacting with `Grid`'s tight content width — see §2).
   - `rating=1` (a query matching many never-before-cached-at-500px stacks)
     froze the whole window — synchronous `Cache::make_now` on the UI
     thread; switched to async `Cache::get` + placeholder + periodic
     repaint.
   - Large dead gap after the first grid tile, second tile's sliver pinned
     against the window edge — `ui.vertical()`'s width-claiming inside a
     `Grid` cell (see §2); confirmed via a user-provided screenshot after
     several purely-verbal-description-based fixes failed to converge.

## 5. Pending / Next Steps

- Nothing pending from this session's own scope — all of it was resolved,
  confirmed live, or explicitly parked at the user's request.
- `feature/gui` (local only, not pushed) is parked at commit `809c1de`
  (`npcg: fix grid cell width claiming the full row`). Contains a working
  minimal `npcg`: query, grid, ratings, paging, scrolling — no tagging, no
  timeline/sidebar, no single-image view. If ever revisited, those would be
  natural next slices, but the user does not currently want to continue
  down this path.
- `feature/tui-timeline` still exists as a branch (local + remote) even
  though it's fully merged into `master` — never deleted, harmless to leave.
- The still-open items from the *previous* summary (a third
  "no reliable signal" reporting category for `scripts/fix-export-ratings`)
  were not revisited this session and remain open if still wanted.
- `CLAUDE.md`'s "Known gaps" section (Miri/ASan never run on the FFI
  boundary; black-frame fix not confirmed on a real kitty terminal — though
  this session did confirm live on Ghostty that black frames do not
  reproduce there) has not been updated to reflect this session's findings;
  worth a pass if picked back up.

## 6. Design Note Still Live

Two prior design artifacts from earlier sessions
(`/home/ram/.claude/plans/i-want-to-scan-jazzy-goose.md`,
`/home/ram/.claude/plans/greedy-yawning-cloud.md` for the `npcg` spike) are
both fully implemented; the latter's branch is parked per §1/§5 above rather
than merged.
