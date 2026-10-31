//! A native GUI, evolving toward a Shotwell-style layout: a sidebar
//! (Events/Tags/Folders) to filter by, a thumbnail grid with a zoom slider,
//! an info panel for the selected photo, and a double-click single-image
//! view with prev/next. Built to answer one question first: does showing
//! thumbnails as real GPU textures avoid the terminal-transmission cost the
//! TUI pays on every grid page-turn? (Measured: ~780ms of a ~950ms 27-tile
//! page redraw is `ratatui-image`'s Kitty encoder writing raw, uncompressed
//! RGBA pixel data to the terminal -- not npc's own decode or resize work.)
//!
//! Not ported from the TUI: the timeline bar chart, pan/zoom-drag-crop in
//! the single view, RAW/JPG member cycling, undo, and real tagging (the tag
//! list here is read-only). All real features, all explicitly out of scope
//! for this pass -- see the plan this was built from.

// Tile-columns-per-row is a UI layout count derived from a pixel width; an
// available width beyond `usize::MAX` tiles, or negative, is not a case this
// window can be in. Same justification as the equivalent allow in thumbs.rs.
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use crate::action::{self, Action};
use crate::config::{Config, Theme};
use crate::db::Catalog;
use crate::edit::{self, Change};
use crate::exif;
use crate::export;
use crate::tui::{compute_timeline, Granularity, TimelineBucket, TrashView};
use crate::watch;
use crate::{query, thumbs, trash, vol};
use anyhow::{anyhow, Result};
use eframe::egui;
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MIN_TILE_PX: f32 = 64.0;
const MAX_TILE_PX: f32 = 1000.0;
const DEFAULT_TILE_PX: f32 = 160.0;
/// Bounds on `single_zoom`, the multiplier on top of fit-to-window.
const MIN_SINGLE_ZOOM: f32 = 0.1;
const MAX_SINGLE_ZOOM: f32 = 20.0;

struct Row {
    id: i64,
    rating: Option<i64>,
    flag: bool,
    reject: bool,
}

/// No SQL-level cap: a single day of a real library was measured at just
/// over 1000 stacks, and a hard 200-row limit silently dropped everything
/// past that with no indication anywhere that it had happened -- the
/// sidebar's own count (an independent, unlimited query) disagreed with
/// what the grid could ever show, and there was no way to reach the rest.
/// A `Row` is two `i64`s, so fetching every match (`tui.rs`'s own
/// `select_stacks(None)` does the same) costs nothing that matters; what
/// has to stay bounded is *rendering*, which `draw_grid`'s
/// `ScrollArea::show_rows` now handles by only drawing whatever is
/// actually scrolled into view.
fn run_query(cat: &Catalog, text: &str, ratings: [bool; 5]) -> Result<Vec<Row>> {
    let mut compiled = query::compile(text)?;
    // Excluded by default, same as `tui.rs`'s own `TrashView::Exclude`: a
    // trashed stack is on its way out of the library, not part of what
    // browsing shows. Without this, a day with a couple of hundred trashed
    // near-duplicates or bad frames mixed in among its real photos showed
    // every one of them right alongside the rest, arrows and all -- exactly
    // the "just-trashed stack resurfaces in the view you're browsing"
    // scenario `TrashView`'s own doc comment was written to prevent.
    compiled.where_sql = format!(
        "({}) AND {}",
        compiled.where_sql,
        TrashView::Exclude.clause("s.id")
    );
    // The rating checkboxes are a separate, structural filter ANDed onto
    // the compiled query rather than spliced into the visible query text --
    // there's no single fragment to cleanly remove when the last box is
    // unchecked, unlike a sidebar click's `splice_filter`. All boxes
    // unchecked means no rating filter at all (every rating shown), not
    // "match nothing".
    let selected: Vec<i64> = (1..=5).filter(|&n| ratings[(n - 1) as usize]).collect();
    if !selected.is_empty() {
        let placeholders = selected.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        compiled.where_sql = format!(
            "({}) AND COALESCE(s.rating, 0) IN ({placeholders})",
            compiled.where_sql
        );
        compiled
            .params
            .extend(selected.into_iter().map(rusqlite::types::Value::Integer));
    }
    let sql = compiled.select_stacks(None);
    let mut stmt = cat.conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = compiled
        .params
        .iter()
        .map(|p| p as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt
        .query_map(params.as_slice(), |r| {
            Ok(Row {
                id: r.get(0)?,
                rating: r.get(3)?,
                flag: r.get(4)?,
                reject: r.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The on-disk path of a stack's primary member -- raw preferred over a
/// derived jpg, the same priority `tui.rs`'s own `current_members` ordering
/// uses -- for the info panel, so the photo can be found in another file
/// browser without reconstructing it from the library root by hand.
fn full_path(cat: &Catalog, id: i64) -> Option<String> {
    cat.conn
        .query_row(
            "SELECT s.dir || '/' || f.name FROM stack s JOIN file f ON f.stack_id = s.id
             WHERE s.id = ?1 AND f.present = 1
             ORDER BY CASE f.role WHEN 'raw' THEN 0 WHEN 'oocjpg' THEN 1 ELSE 2 END, f.name
             LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .unwrap_or(None)
}

/// Sets both `CLIPBOARD` (Ctrl+V) and `PRIMARY` (middle-click paste) to
/// `text`. `egui::Context::copy_text` only ever reaches the former (it goes
/// through `egui-winit`'s own `Clipboard`, which has no notion of the X11
/// primary selection at all), so this bypasses it and talks to `arboard`
/// directly -- the same crate `egui-winit` itself uses, already in the
/// dependency tree. A fresh connection per call, not reused: this is a rare,
/// explicit user action, not a per-frame cost.
fn copy_to_clipboard(text: &str) -> Result<()> {
    use arboard::{LinuxClipboardKind, SetExtLinux};
    let mut cb = arboard::Clipboard::new()?;
    cb.set_text(text.to_string())?;
    cb.set()
        .clipboard(LinuxClipboardKind::Primary)
        .text(text.to_string())?;
    Ok(())
}

/// One row of the Events sidebar section: a named event, or a bare day when
/// none was ever named -- `label` is exactly what `event="{label}"` matches,
/// the same name-or-day convention `tui.rs`'s own Events/Dates sidebar uses.
/// `month` (`"YYYY-MM"`, from the same capture time) is `None` only for a
/// stack with no event and no capture time at all -- grouped as "Undated".
struct EventEntry {
    label: String,
    count: i64,
    month: Option<String>,
}

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// `"2026-08"` -> `"August 2026"`. Falls back to the raw key on anything
/// malformed, which cannot happen from `EventEntry::month` (always produced
/// by slicing `exif::format_date`'s own output) but keeps this total rather
/// than panicking if that ever changes.
fn month_label(month: &str) -> String {
    let Some((y, m)) = month.split_once('-') else {
        return month.to_string();
    };
    let Some(idx) = m.parse::<usize>().ok().and_then(|n| n.checked_sub(1)) else {
        return month.to_string();
    };
    match MONTH_NAMES.get(idx) {
        Some(name) => format!("{name} {y}"),
        None => month.to_string(),
    }
}

/// Bucket a start-time-descending-sorted event list into month-sized runs --
/// a linear scan rather than a sort, since months are contiguous ranges
/// under a descending time order and can never interleave once the input is
/// already sorted that way (which `reload_sidebar`'s query guarantees).
fn group_events_by_month(events: &[EventEntry]) -> Vec<(Option<String>, Vec<&EventEntry>)> {
    let mut groups: Vec<(Option<String>, Vec<&EventEntry>)> = Vec::new();
    for e in events {
        match groups.last_mut() {
            Some((last_month, items)) if *last_month == e.month => items.push(e),
            _ => groups.push((e.month.clone(), vec![e])),
        }
    }
    groups
}

/// A row from a flat `(name, count)` query, grouped into a tree by splitting
/// each name on `/` -- the same convention the query grammar already treats
/// as parent/child (`tag:"beach"` matches `beach` and everything nested
/// under it, per `query.rs`'s `tags_match_hierarchically`). A library with
/// no `/` in any tag name ends up as a flat list of single-level nodes.
struct TagNode {
    name: String,
    /// The full dotted-slash path from the root, used as the filter fragment
    /// and as this node's egui id (so two different branches sharing a leaf
    /// name, e.g. `Trips/Paris` and `Family/Paris`, don't collide).
    path: String,
    /// `Some` only when a real tag row exists at exactly this path; `None`
    /// for a synthetic node that exists only to group its children (no tag
    /// literally named e.g. `Trips` if only `Trips/Paris` was ever used).
    own_count: Option<i64>,
    children: Vec<TagNode>,
}

impl TagNode {
    /// This node's own count plus every descendant's -- what a parent
    /// header shows, so "Trips (12)" reflects everything nested under it,
    /// not just shots tagged with the bare word "Trips".
    fn total_count(&self) -> i64 {
        self.own_count.unwrap_or(0) + self.children.iter().map(TagNode::total_count).sum::<i64>()
    }
}

/// Intermediate tree while grouping a flat `(name, count)` list by `/` --
/// unlike `TagNode`, a `Builder` has no `path` of its own yet (that's only
/// known once its position in the tree is fixed) and no count until a real
/// tag row is found for it.
#[derive(Default)]
struct Builder {
    own_count: Option<i64>,
    children: std::collections::BTreeMap<String, Builder>,
}

fn into_nodes(children: std::collections::BTreeMap<String, Builder>, prefix: &str) -> Vec<TagNode> {
    children
        .into_iter()
        .map(|(seg, b)| {
            let path = if prefix.is_empty() {
                seg.clone()
            } else {
                format!("{prefix}/{seg}")
            };
            let children = into_nodes(b.children, &path);
            TagNode {
                name: seg,
                path,
                own_count: b.own_count,
                children,
            }
        })
        .collect()
}

fn build_tag_tree(flat: &[(String, i64)]) -> Vec<TagNode> {
    let mut root = Builder::default();
    for (full_name, count) in flat {
        let mut node = &mut root;
        for seg in full_name.split('/') {
            node = node.children.entry(seg.to_string()).or_default();
        }
        node.own_count = Some(*count);
    }
    into_nodes(root.children, "")
}

/// Renders one tag (and, recursively, its children); returns the filter
/// fragment if this node (or a descendant) was clicked this frame. A leaf
/// is a plain selectable label; a node with children is a collapsing
/// header whose own click both toggles expansion and filters by the whole
/// subtree -- one click doing both is a deliberate simplification for this
/// first cut, not an oversight; worth revisiting if it feels wrong in use.
fn draw_tag_node(ui: &mut egui::Ui, node: &TagNode) -> Option<String> {
    if node.children.is_empty() {
        let label = format!("{} ({})", node.name, node.own_count.unwrap_or(0));
        if ui.selectable_label(false, label).clicked() {
            return Some(format!("tag:\"{}\"", node.path));
        }
        return None;
    }
    let label = format!("{} ({})", node.name, node.total_count());
    let mut clicked = None;
    let resp = egui::CollapsingHeader::new(label)
        .id_salt(&node.path)
        .show(ui, |ui| {
            for child in &node.children {
                if let Some(f) = draw_tag_node(ui, child) {
                    clicked = Some(f);
                }
            }
        });
    if resp.header_response.clicked() {
        clicked = Some(format!("tag:\"{}\"", node.path));
    }
    clicked
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Grid,
    Single,
}

// A single immediate-mode app-state blob naturally accumulates independent
// UI toggles (show_actions, show_export, online, ...) that have nothing to
// do with each other -- splitting them into a bitflags type or sub-structs
// here would be ceremony standing between the field and the one place it's
// read, not a real grouping.
#[allow(clippy::struct_excessive_bools)]
struct GuiApp {
    cat: Catalog,
    cache: thumbs::Cache,
    config: Config,
    /// The library root, needed only for `export::export`'s reserved-path
    /// check (it refuses a target inside the dated import tree or trash).
    /// Nothing else here needs it -- every other query goes through `cat`.
    root: PathBuf,
    query: String,
    query_error: Option<String>,
    /// Feedback from the last rating edit -- separate from `query_error`,
    /// which is specifically about the query box.
    status: Option<String>,
    rows: Vec<Row>,
    selected: usize,
    /// A batch selection, by stack id -- Ctrl/Cmd-click in the grid, or
    /// Space on the keyboard, toggles membership. When non-empty, rating
    /// and tagging apply to the whole batch instead of just `selected`
    /// (`targets()`), mirroring `tui.rs`'s own `selected: HashSet<i64>` and
    /// its `targets()` method. Persists across filtering/query changes on
    /// purpose, same as the TUI: building a batch across more than one
    /// query, then editing it in one step, is the point.
    multi_selected: std::collections::HashSet<i64>,
    /// The fixed end of a Shift-click range: set on a plain click, left
    /// alone by Ctrl/Shift-click so repeated Shift-clicks all extend from
    /// the same starting tile rather than the last one clicked -- the
    /// standard file-manager convention.
    range_anchor: usize,
    /// Sends the grid's scroll position to follow `selected` exactly once,
    /// consumed by `draw_grid` the next time it runs. Arrow-key navigation
    /// sets this; mouse clicks and the wheel do not, since the viewport
    /// should only jump when the keyboard moved the selection somewhere not
    /// already visible, never fight a scroll the user is doing by hand.
    scroll_to_selected: bool,
    /// Cache-file path -> uploaded GPU texture. Unlike the TUI's
    /// `decoded`/`images`/`last_rect` three-way split (needed because a
    /// kitty placement is bound to a screen rect and must be rebuilt
    /// whenever a tile's position changes), a `TextureHandle` is retained
    /// GPU state with no such binding -- the same handle draws at a new
    /// screen position for free, any frame, forever. One flat cache is
    /// enough. `Size::Grid` and `Size::Full` textures for the same source
    /// coexist here under their own distinct cache-file paths -- no key
    /// collision, since `thumbs::Cache` keys by size too.
    ///
    /// Each entry carries the frame it was last drawn in. Kept unbounded,
    /// stepping through 1,000 photos in single view held ~7 GB of 1620 px
    /// textures; `evict_textures` drops the least recently drawn past
    /// `TEXTURE_BUDGET`.
    textures: HashMap<PathBuf, (egui::TextureHandle, u64)>,
    /// Frames drawn so far; the clock `textures` ages by.
    frame: u64,
    /// Tiles per row, recorded during the previous frame's layout and used
    /// by arrow-key row-jumping -- the same "measured during draw, consumed
    /// next frame" pattern as `tui.rs`'s `page_size`.
    cols: usize,
    /// Grid vs. single-image view. Sidebar and info panel stay mounted in
    /// both -- Shotwell's single view keeps its own info sidebar too.
    mode: Mode,
    /// The exact fragment last spliced into `query` by clicking a sidebar
    /// entry -- ported from `tui.rs`'s field of the same name and the same
    /// purpose: the next sidebar click replaces just this fragment instead
    /// of clobbering anything typed in the query box alongside it.
    applied_filter: Option<String>,
    /// Rating checkboxes 1..=5, independent of each other and of the free-
    /// text query -- lets "2 and 5, but nothing else" be selected in one
    /// step, which typing `rating>=N` alone can't express.
    rating_filter: [bool; 5],
    events: Vec<EventEntry>,
    tags: Vec<TagNode>,
    /// One entry per distinct `stack.dir` -- a flat list, not a real
    /// filesystem tree; this catalog has no folder-nesting concept beyond
    /// one string column per stack.
    folders: Vec<(String, i64)>,
    /// On-screen tile size in the grid, adjustable live via the zoom
    /// slider. Unlike `thumbs::Size::Grid`'s fixed pixel target (a global,
    /// config-driven cache tier), this only changes how big an
    /// already-loaded texture is drawn -- the GPU scales it for free, so no
    /// new cache entries or thumbnail regeneration happen as this changes.
    tile_px: f32,
    /// Single view's own zoom, RawTherapee-style: the mouse wheel zooms in
    /// and out anchored at the cursor (see `draw_single`'s zoom math),
    /// multiplying the base fit-to-window scale. `1.0` is "fit", matching
    /// what the view already showed before this existed.
    ///
    /// Deliberately *not* reset by prev/next (unlike `tui.rs`'s own single
    /// view, which does reset on navigation): checking the same detail
    /// across a burst of near-identical frames means the zoom and pan should
    /// follow you from frame to frame, not snap back to fit on every arrow
    /// press. Escape is the explicit reset instead -- see `handle_single_keys`.
    single_zoom: f32,
    /// Screen-space offset from center, adjusted alongside `single_zoom`
    /// to keep the point under the cursor fixed as it changes. Persists
    /// across navigation for the same reason `single_zoom` does.
    single_pan: egui::Vec2,
    /// This session's theme; toggling flips it and re-applies
    /// `egui::Visuals` immediately, but never writes back to config.toml --
    /// nothing in this tool writes its own config file, and the user chose
    /// session-only over persisting the toggle.
    theme: Theme,
    /// The info panel's "add tag" text field. Not cleared automatically on
    /// selection change -- typing a tag, then clicking a different tile
    /// before pressing Enter, keeps the draft rather than silently losing it.
    tag_input: String,
    /// The batch panel's "remove tag" field -- separate from `tag_input` so
    /// typing a tag to add and a tag to remove don't fight over one buffer.
    /// A batch has no single tag list to show per-tag "x" buttons next to
    /// (unlike the single-photo panel), so removal here is by typed name.
    remove_tag_input: String,
    /// Background library watcher (see `watch.rs`). `None` when disabled in
    /// config or failed to start -- either way, just "no automatic
    /// cataloging this session," never fatal to launching the GUI.
    watcher: Option<watch::Watcher>,
    /// What the watcher is doing, for the eye in the query bar.
    watch_state: WatchState,
    /// Writes queued sidecars while input pauses; see `edit::Writer`.
    writer: edit::Writer,
    /// The window was asked to close while sidecars were still queued: keep
    /// running, writing them as fast as possible, and close once they're done.
    closing: bool,
    /// Whether the library's volume is actually mounted right now, exactly
    /// `tui.rs`'s own `online` field. The catalog lives on this machine, not
    /// the drive (README's "Offline" section), so browsing cached
    /// thumbnails, tagging and rating all still work without it -- only
    /// generating a thumbnail that was never cached, launching an external
    /// editor, and exporting actually need the files themselves.
    online: bool,
    /// The `a` menu, ported from `tui.rs`: configured external programs
    /// (RawTherapee, GIMP, ...) launched against the current selection.
    /// Same `action::load()`/`action::gather()`/`action::run()` the TUI
    /// uses, so a program configured once works from either front end.
    actions: Vec<Action>,
    show_actions: bool,
    /// The `e` menu: a quick, no-external-tool resize+reencode (2000px long
    /// edge, quality 75 -- deliberately lower than `export`'s own 95 default,
    /// since this is for sharing/web rather than an archival copy) plus
    /// whichever `[[export]]` presets are configured, same `export::presets`
    /// the CLI's `--list-presets` reads. Listed together so both live behind
    /// one key, the same reasoning the `a` menu already settled on for
    /// external programs.
    export_presets: Vec<export::Preset>,
    show_export: bool,
    /// Whole-library bucket counts over time, independent of `query` -- a
    /// navigation aid, same as `tui.rs`'s timeline, and computed by the
    /// same shared `tui::compute_timeline` so the two front ends can never
    /// disagree about what a bucket contains. No cursor/offset here: a
    /// click fires `apply_filter` immediately, there's nothing to "have
    /// focus" the way the TUI's keyboard-driven bar needs.
    timeline: Vec<TimelineBucket>,
    timeline_granularity: Granularity,
}

impl GuiApp {
    fn new(
        cat: Catalog,
        cache: thumbs::Cache,
        config: Config,
        watcher: Option<watch::Watcher>,
        root: PathBuf,
        online: bool,
    ) -> Self {
        let rows = run_query(&cat, "", [false; 5]).unwrap_or_default();
        let theme = config.gui.theme;
        let mut app = Self {
            cat,
            cache,
            config,
            root,
            query: String::new(),
            query_error: None,
            status: None,
            watch_state: if watcher.is_some() {
                WatchState::Starting
            } else {
                WatchState::Off
            },
            watcher,
            writer: edit::Writer::default(),
            closing: false,
            online,
            rows,
            selected: 0,
            multi_selected: std::collections::HashSet::new(),
            range_anchor: 0,
            scroll_to_selected: false,
            textures: HashMap::new(),
            frame: 0,
            cols: 1,
            mode: Mode::Grid,
            applied_filter: None,
            rating_filter: [false; 5],
            events: Vec::new(),
            tags: Vec::new(),
            folders: Vec::new(),
            tile_px: DEFAULT_TILE_PX,
            single_zoom: 1.0,
            single_pan: egui::Vec2::ZERO,
            theme,
            tag_input: String::new(),
            remove_tag_input: String::new(),
            actions: action::load(),
            show_actions: false,
            export_presets: export::presets(),
            show_export: false,
            timeline: Vec::new(),
            timeline_granularity: Granularity::Month,
        };
        app.reload_sidebar();
        app.reload_timeline();
        app
    }

    /// Whole-library counts for the three sidebar sections, independent of
    /// the current query -- the same relationship `tui.rs`'s timeline/sidebar
    /// has to its own filtered result set. Called once at startup, and again
    /// after every tag edit (`apply_tag_change`), since that's the only thing
    /// in this app that can change what these three sections show; a rating
    /// edit touches none of them.
    fn reload_sidebar(&mut self) {
        // Excluded from every section here for the same reason `run_query`
        // excludes them: a trashed stack is leaving the library, not part
        // of what browsing shows, and a count that included them would
        // disagree with what a click could ever actually display.
        let exclude_stack = TrashView::Exclude.clause("s.id");
        self.events = self
            .cat
            .conn
            .prepare(&format!(
                "SELECT COALESCE(
                          NULLIF(e.name, ''),
                          date(e.start_time, 'unixepoch'),
                          'undated') AS d,
                        COUNT(s.id) AS n,
                        e.start_time
                 FROM stack s LEFT JOIN event e ON e.id = s.event_id
                 WHERE {exclude_stack}
                 GROUP BY COALESCE(e.id, -1)
                 ORDER BY (e.start_time IS NULL), e.start_time DESC"
            ))
            .and_then(|mut st| {
                let rows = st.query_map([], |r| {
                    let start: Option<i64> = r.get(2)?;
                    Ok(EventEntry {
                        label: r.get(0)?,
                        count: r.get(1)?,
                        month: start.map(|t| exif::format_date(t)[0..7].to_string()),
                    })
                })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
            })
            .unwrap_or_default();

        let flat_tags: Vec<(String, i64)> = self
            .cat
            .conn
            .prepare(&format!(
                "SELECT t.name, COUNT(st.stack_id) n FROM tag t
                 LEFT JOIN stack_tag st ON st.tag_id = t.id AND {}
                 GROUP BY t.id ORDER BY n DESC, t.name",
                TrashView::Exclude.clause("st.stack_id")
            ))
            .and_then(|mut st| {
                let rows =
                    st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
            })
            .unwrap_or_default();
        self.tags = build_tag_tree(&flat_tags);

        self.folders = self
            .cat
            .conn
            .prepare(&format!(
                "SELECT dir, COUNT(*) n FROM stack s WHERE {exclude_stack}
                 GROUP BY dir ORDER BY dir"
            ))
            .and_then(|mut st| {
                let rows =
                    st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
            })
            .unwrap_or_default();
    }

    /// Whole-library bucket counts at `self.timeline_granularity` -- see
    /// `tui::compute_timeline`'s doc comment for why this is a full scan
    /// and why that's fine. No cursor to preserve across a granularity
    /// change here (unlike `tui.rs`'s `reload_timeline`): a click always
    /// re-derives its filter fresh from the bucket clicked.
    fn reload_timeline(&mut self) {
        self.timeline = compute_timeline(&self.cat, self.timeline_granularity, TrashView::Exclude)
            .unwrap_or_default();
    }

    /// Bring a sidebar click into `query` without discarding anything typed
    /// alongside it. Ported verbatim from `tui.rs`'s method of the same
    /// name and the same comment: if the fragment from the *previous* click
    /// is still present verbatim, it is replaced in place; otherwise the new
    /// fragment is anded onto whatever is there (or becomes the whole
    /// query, if it was empty).
    fn splice_filter(&mut self, fragment: &str) {
        self.query = match self
            .applied_filter
            .as_deref()
            .filter(|prior| self.query.contains(*prior))
        {
            Some(prior) => self.query.replacen(prior, fragment, 1),
            None if self.query.trim().is_empty() => fragment.to_string(),
            None => format!("{fragment} and {}", self.query),
        };
        self.applied_filter = Some(fragment.to_string());
    }

    /// Splice a sidebar-click fragment into the query and re-run it.
    fn apply_filter(&mut self, fragment: &str) {
        self.splice_filter(fragment);
        match run_query(&self.cat, &self.query, self.rating_filter) {
            Ok(rows) => {
                self.rows = rows;
                self.selected = 0;
                self.query_error = None;
            }
            Err(e) => self.query_error = Some(e.to_string()),
        }
    }

    /// Stacks an edit applies to: the batch selection if there is one, else
    /// whatever is under the cursor. Mirrors `tui.rs`'s method of the same
    /// name and the same rule.
    fn targets(&self) -> Vec<i64> {
        if !self.multi_selected.is_empty() {
            return self.multi_selected.iter().copied().collect();
        }
        self.rows
            .get(self.selected)
            .map(|r| r.id)
            .into_iter()
            .collect()
    }

    /// Launch a configured action against the current selection. Mirrors
    /// `tui.rs`'s `run_action` -- same protected-root guard (a program this
    /// one launches, e.g. RawTherapee, is not bound by *this* program's
    /// write guard, so a protected library must never reach one), same
    /// `action::gather`/`action::run` pair. No `online` check here, unlike
    /// the TUI: `run()` below already refuses to open the GUI at all when
    /// the library isn't mounted, so there's no in-session offline state to
    /// guard against.
    fn run_action(&mut self, key: char) {
        let Some(a) = self
            .actions
            .iter()
            .find(|a| a.key == key.to_string())
            .cloned()
        else {
            self.status = Some(format!("no action bound to `{key}`"));
            return;
        };
        if !self.online {
            self.status = Some(format!(
                "{} not launched: the library is not mounted",
                a.name
            ));
            return;
        }
        // An external editor reads files, not the catalog: write anything
        // still queued first, so it sees the current rating and tags.
        if let Err(e) = edit::flush(&mut self.cat, &self.config) {
            self.status = Some(format!("{}: writing sidecars first: {e}", a.name));
            return;
        }
        let ids = self.targets();

        for &sid in &ids {
            let dir: String =
                match self
                    .cat
                    .conn
                    .query_row("SELECT dir FROM stack WHERE id = ?1", [sid], |r| r.get(0))
                {
                    Ok(d) => d,
                    Err(_) => continue,
                };
            if self
                .config
                .ensure_mutable(std::path::Path::new(&dir))
                .is_err()
            {
                self.status = Some(format!(
                    "{} not launched: {} is protected, and an external editor \
                     cannot be stopped from writing there",
                    a.name, dir
                ));
                return;
            }
        }

        let current = self.rows.get(self.selected).map(|r| r.id);
        let sel = match action::gather(&self.cat, &ids, current, &self.query) {
            Ok(sel) => sel,
            Err(e) => {
                self.status = Some(format!("{}: {e}", a.name));
                return;
            }
        };
        self.status = Some(match action::run(&a, &sel, &self.config) {
            Ok(outcome) => outcome.description,
            Err(e) => format!("{}: {e}", a.name),
        });
    }

    /// `n == 0` is the built-in quick resize (2000px, quality 75, no
    /// external tool -- see `export_presets`'s own doc comment for why that
    /// quality is lower than `export::DEFAULT_QUALITY`); `n >= 1` indexes
    /// `export_presets[n - 1]`. Each kind gets its own subdirectory under
    /// `export_dir` (named "web" for the quick resize, or the preset's own
    /// name) rather than sharing one flat directory the way the CLI's
    /// `--out` does -- the CLI leaves collision-avoidance to whoever picks
    /// `--out`, but this menu has no path field for the user to vary it by
    /// hand, and two kinds sharing a directory would silently overwrite each
    /// other's same-named output.
    fn run_export(&mut self, n: usize) {
        if !self.online {
            self.status = Some("export not available: the library is not mounted".into());
            return;
        }
        let Some(base) = self.config.default_export_dir() else {
            self.status = Some(format!(
                "no export destination set. Add export_dir to {}",
                Config::path().display()
            ));
            return;
        };
        let ids = self.targets();
        if ids.is_empty() {
            self.status = Some("nothing selected".into());
            return;
        }
        let (kind, preset) = if n == 0 {
            ("web", None)
        } else {
            let Some(p) = self.export_presets.get(n - 1) else {
                self.status = Some(format!("no export option {n}"));
                return;
            };
            (p.name.as_str(), Some(p))
        };
        let target = base.join(kind);
        let opts = export::Options {
            long_edge: 2000,
            quality: 75,
            keep_structure: false,
            strip: false,
            preset,
            dry_run: false,
        };
        self.status = Some(
            match export::export(&self.cat, &ids, &target, &self.root, &self.config, &opts) {
                Ok(stats) => {
                    let errors = if stats.errors.is_empty() {
                        String::new()
                    } else {
                        format!(" ({} error(s))", stats.errors.len())
                    };
                    format!(
                        "exported {} stack(s) -> {}{errors}",
                        stats.written,
                        target.display()
                    )
                }
                Err(e) => format!("export failed: {e}"),
            },
        );
    }

    /// Set (or clear, for `None`) the star rating of every target. Only the
    /// catalog changes here (0.05 ms); the sidecar is written by `writer`
    /// once input pauses -- writing it and reloading the whole result list
    /// on every key measured ~200 ms on a 95k-stack library, which is what
    /// made fast rating lag. The rated rows update in place and stay in view
    /// even if they no longer match a rating filter, until the query or
    /// filter changes. No undo stack here yet -- `tui.rs` has one.
    fn rate_selected(&mut self, n: Option<i64>) {
        self.edit_selected(&[Change::SetRating(n)], "rated");
    }

    /// Flag (pick), reject, or clear both marks for every target -- mirrors
    /// `tui.rs`'s `f`/`x`/`c` keys exactly, right down to letting `edit.rs`'s
    /// own `SetFlag`/`SetReject` handling enforce that the two are mutually
    /// exclusive (setting one clears the other at the SQL layer, not here).
    fn mark_selected(&mut self, changes: &[Change], verb: &str) {
        self.edit_selected(changes, verb);
    }

    /// Apply `changes` to every target in the catalog, queue the sidecars
    /// for `writer`, and refresh just those rows from the catalog -- what it
    /// actually stored, not an optimistic guess.
    fn edit_selected(&mut self, changes: &[Change], verb: &str) {
        let ids = self.targets();
        if ids.is_empty() {
            return;
        }
        if let Err(e) = edit::apply(&mut self.cat, &ids, changes, &self.config) {
            self.status = Some(format!("{verb} failed: {e}"));
            return;
        }
        self.writer.edited(&ids);
        self.refresh_rows(&ids);
        self.status = Some(format!("{verb} {} stack(s)", ids.len()));
    }

    /// Move every target into the library's `.trash/` -- the same
    /// `trash::trash` as `npc trash`, so reversible with `npc untrash` and
    /// nothing deleted until `npc trash-empty`. Queued sidecars are written
    /// first, so each `.xmp` moves with its latest edits in it.
    fn trash_selected(&mut self) {
        if !self.online {
            self.status = Some("trash not available: the library is not mounted".into());
            return;
        }
        let ids = self.targets();
        if ids.is_empty() {
            return;
        }
        if let Err(e) = edit::flush(&mut self.cat, &self.config) {
            self.status = Some(format!("trash: writing sidecars first: {e}"));
            return;
        }
        let stats = match trash::trash(&mut self.cat, &self.root, &ids, &self.config) {
            Ok(stats) => stats,
            Err(e) => {
                self.status = Some(format!("trash failed: {e}"));
                return;
            }
        };

        let trashed: Vec<i64> = ids
            .iter()
            .copied()
            .filter(|&id| {
                self.cat
                    .conn
                    .query_row(
                        "SELECT 1 FROM trashed WHERE stack_id = ?1",
                        [id],
                        |_| Ok(()),
                    )
                    .optional()
                    .is_ok_and(|hit| hit.is_some())
            })
            .collect();
        self.rows.retain(|r| !trashed.contains(&r.id));
        self.multi_selected.clear();
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.range_anchor = self.selected;
        self.reload_sidebar();
        self.reload_timeline();

        let errors = match stats.errors.first() {
            None => String::new(),
            Some(e) => format!(" ({} failed: {e})", stats.errors.len()),
        };
        self.status = Some(format!(
            "trashed {} stack(s) -> .trash/, restore with `npc untrash <base>`{errors}",
            stats.stacks
        ));
    }

    fn refresh_rows(&mut self, ids: &[i64]) {
        for row in self.rows.iter_mut().filter(|r| ids.contains(&r.id)) {
            if let Ok((rating, flag, reject)) = self.cat.conn.query_row(
                "SELECT rating, flag, reject FROM stack WHERE id = ?1",
                [row.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            ) {
                row.rating = rating;
                row.flag = flag;
                row.reject = reject;
            }
        }
    }

    /// `Ctrl+1`..`Ctrl+5` flips the same array the checkboxes edit directly
    /// -- shared with their `.changed()` handler in `draw_query_bar` so a
    /// keypress and a click can never disagree about what happens next.
    fn toggle_rating_filter(&mut self, n: usize) {
        self.rating_filter[n - 1] = !self.rating_filter[n - 1];
        self.refresh_rows_after_rating_filter_change();
    }

    /// What a grid-tile click does, pulled out of `draw_grid`'s rendering
    /// closure so it is exercised directly rather than through a simulated
    /// `egui::Response` -- same reasoning as `toggle_rating_filter`.
    ///
    /// Plain click: fresh single selection, same file-manager convention
    /// `tui.rs` has no equivalent of (the TUI has no mouse). Ctrl/Cmd-click:
    /// toggles `i` into or out of the batch without touching the rest of it.
    /// Shift-click: replaces the batch with everything between the fixed
    /// `range_anchor` and `i`, inclusive -- a plain or Ctrl-click moves the
    /// anchor to the clicked tile, but a Shift-click deliberately does not,
    /// so repeated Shift-clicks all extend from the same starting tile
    /// rather than the last one clicked.
    fn handle_grid_click(&mut self, i: usize, id: i64, ctrl: bool, shift: bool) {
        if shift {
            self.select_range(i);
        } else if ctrl {
            if !self.multi_selected.remove(&id) {
                self.multi_selected.insert(id);
            }
            self.selected = i;
            self.range_anchor = i;
        } else {
            self.multi_selected.clear();
            self.selected = i;
            self.range_anchor = i;
        }
    }

    /// Selects the inclusive range between the fixed `range_anchor` and
    /// `to`, replacing the batch -- the one definition of "select this
    /// range" a Shift-click and Shift+Arrow-key extension both defer to, so
    /// they can never silently disagree about it.
    fn select_range(&mut self, to: usize) {
        let (lo, hi) = (self.range_anchor.min(to), self.range_anchor.max(to));
        self.multi_selected = (lo..=hi)
            .filter_map(|j| self.rows.get(j).map(|r| r.id))
            .collect();
        self.selected = to;
    }

    /// What an arrow-key move (the cursor already at `to`) does to the batch
    /// selection -- pulled out of `handle_nav_keys`'s input-reading closure
    /// for direct testing, same reasoning as `handle_grid_click`. `extend`
    /// (Shift held) grows the batch via `select_range` from the fixed
    /// anchor; otherwise it collapses to a single selection at `to`, the
    /// same outcome a plain click already has, so the cursor is never left
    /// sitting outside a stale-looking batch.
    fn arrow_move(&mut self, to: usize, extend: bool) {
        if extend {
            self.select_range(to);
        } else {
            self.multi_selected.clear();
            self.selected = to;
            self.range_anchor = to;
        }
        self.scroll_to_selected = true;
    }

    /// Escape: cancel the batch without otherwise moving anything.
    fn clear_selection(&mut self) {
        self.multi_selected.clear();
        self.range_anchor = self.selected;
    }

    /// Single view's explicit zoom reset -- prev/next deliberately leave
    /// `single_zoom`/`single_pan` alone (see their doc comments), so this is
    /// the only way back to fit-to-window short of scrolling out by hand.
    /// Shared by Escape and the "<- grid" button, so leaving the view either
    /// way starts the next visit fresh.
    fn reset_zoom(&mut self) {
        self.single_zoom = 1.0;
        self.single_pan = egui::Vec2::ZERO;
    }

    fn refresh_rows_after_rating_filter_change(&mut self) {
        match run_query(&self.cat, &self.query, self.rating_filter) {
            Ok(rows) => {
                self.rows = rows;
                self.selected = 0;
                self.query_error = None;
            }
            Err(e) => self.query_error = Some(e.to_string()),
        }
    }

    /// Comma-separated tag names, same convention as `tui.rs`'s `t`/`T`
    /// prompts: split, trim, drop anything empty. Applies to every target
    /// (the batch selection, or just the cursor when nothing is batched).
    /// `reload_sidebar` runs afterward since a brand-new tag (or a tag's
    /// count reaching zero) needs to show up in the Tags tree, unlike a
    /// rating change, which touches nothing the sidebar displays.
    fn add_tags_to_selected(&mut self, raw: &str) {
        let tags: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if tags.is_empty() {
            return;
        }
        self.apply_tag_change(&self.targets(), Change::AddTags(tags));
    }

    fn remove_tag_from_selected(&mut self, name: &str) {
        self.apply_tag_change(&self.targets(), Change::RemoveTags(vec![name.to_string()]));
    }

    fn apply_tag_change(&mut self, ids: &[i64], change: Change) {
        if ids.is_empty() {
            return;
        }
        if let Err(e) = edit::apply(&mut self.cat, ids, &[change], &self.config) {
            self.status = Some(format!("tag failed: {e}"));
            return;
        }
        self.writer.edited(ids);
        self.status = Some(format!("tagged {} stack(s)", ids.len()));
        self.reload_sidebar();
    }

    fn texture_for(&mut self, ctx: &egui::Context, path: &Path) -> Option<egui::TextureHandle> {
        if let Some((t, used)) = self.textures.get_mut(path) {
            *used = self.frame;
            return Some(t.clone());
        }
        let img = image::open(path).ok()?.to_rgba8();
        let (w, h) = img.dimensions();
        let color =
            egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw());
        let tex = ctx.load_texture(
            path.to_string_lossy(),
            color,
            egui::TextureOptions::default(),
        );
        self.textures
            .insert(path.to_path_buf(), (tex.clone(), self.frame));
        Some(tex)
    }

    /// Drop the least recently drawn textures until the rest fit `budget`
    /// bytes. What this frame drew is never dropped.
    fn evict_textures(&mut self, budget: usize) {
        let bytes = |t: &egui::TextureHandle| {
            let [w, h] = t.size();
            w * h * 4
        };
        let mut total: usize = self.textures.values().map(|(t, _)| bytes(t)).sum();
        if total <= budget {
            return;
        }
        let mut by_age: Vec<(u64, PathBuf, usize)> = self
            .textures
            .iter()
            .map(|(p, (t, used))| (*used, p.clone(), bytes(t)))
            .collect();
        by_age.sort_unstable_by_key(|(used, ..)| *used);
        for (used, path, size) in by_age {
            if total <= budget || used == self.frame {
                break;
            }
            self.textures.remove(&path);
            total -= size;
        }
    }

    /// Single view: make the photos either side of `selected` ready while
    /// this one is looked at, so stepping to them finds a finished view
    /// image (generated on the cache's worker) and, for the next one, a
    /// texture already uploaded. A first view used to wait for both: up to
    /// ~350 ms of generation for a JPG-only stack, ~30 ms of decode on this
    /// thread for any.
    fn prefetch_neighbours(&mut self, ctx: &egui::Context, size: thumbs::Size) {
        let mut decoded_one = false;
        for offset in [1_isize, -1, 2] {
            let Some(i) = self.selected.checked_add_signed(offset) else {
                continue;
            };
            let Some(id) = self.rows.get(i).map(|r| r.id) else {
                continue;
            };
            let Ok(Some(src)) = thumbs::best_source(&self.cat, id) else {
                continue;
            };
            // `get` queues generation when it isn't cached yet. At most one
            // decode per frame, so prefetching never stalls a frame twice.
            if let Some(path) = self.cache.get(&src, size) {
                if !decoded_one && !self.textures.contains_key(&path) {
                    let _ = self.texture_for(ctx, &path);
                    decoded_one = true;
                }
            }
        }
    }

    fn draw_query_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("query").show(ui, |ui| {
            ui.horizontal(|ui| {
                self.draw_write_state(ui);
                self.draw_watch_eye(ui);
                ui.separator();
                ui.label("query:");
                // Neither documented "submit on Enter" idiom actually works
                // for this, both confirmed directly against egui's own
                // source rather than assumed:
                //   - `response.lost_focus()` is a *one-frame-deferred*
                //     signal for a mid-frame focus transfer -- egui's own
                //     regression test `lost_focus_fires_after_mid_frame_
                //     focus_transfer` (memory/mod.rs) asserts it explicitly:
                //     "Frame N: click B mid-frame" -> lost_focus for A is
                //     still false that same frame; only "Frame N+1" does it
                //     fire. Checking it right after this call, same frame,
                //     never sees it.
                //   - `response.has_focus()` evaluates *live* against
                //     current memory (`ctx.memory(|m| m.has_focus(id))`,
                //     response.rs), and a singleline `TextEdit` calls
                //     `surrender_focus` on Enter *during its own
                //     processing*, before returning -- so by the time we
                //     check `resp.has_focus()` right after, it has already
                //     surrendered and always reads false. This is why the
                //     query never actually resubmitted on Enter: the whole
                //     block was dead.
                // The fix: capture focus *before* the widget runs, via an
                // explicit id, so the widget's own internal surrender can't
                // erase what we needed to know.
                let query_id = ui.make_persistent_id("gui_query_text_edit");
                let was_focused = ui.memory(|m| m.has_focus(query_id));
                ui.add(egui::TextEdit::singleline(&mut self.query).id(query_id));
                if was_focused
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
                {
                    match run_query(&self.cat, &self.query, self.rating_filter) {
                        Ok(rows) => {
                            self.rows = rows;
                            self.selected = 0;
                            self.query_error = None;
                        }
                        Err(e) => self.query_error = Some(e.to_string()),
                    }
                }
                if self.multi_selected.is_empty() {
                    ui.label(format!("{} stack(s)", self.rows.len()));
                } else {
                    ui.label(format!(
                        "{} stack(s), {} selected",
                        self.rows.len(),
                        self.multi_selected.len()
                    ));
                }
            });
            ui.horizontal(|ui| {
                // Independent checkboxes rather than a single `rating>=N`
                // typed into the query -- lets "2 and 5, nothing else" be
                // picked in one step, which a single threshold can't
                // express. ANDed onto the query as a separate structural
                // filter (see `run_query`), not spliced into the query text.
                ui.label("rating:");
                let mut changed = false;
                for n in 1..=5usize {
                    changed |= ui
                        .checkbox(&mut self.rating_filter[n - 1], n.to_string())
                        .changed();
                }
                if changed {
                    self.refresh_rows_after_rating_filter_change();
                }
                ui.separator();
                ui.label("zoom:");
                ui.add(egui::Slider::new(
                    &mut self.tile_px,
                    MIN_TILE_PX..=MAX_TILE_PX,
                ));
                if ui
                    .button(match self.theme {
                        Theme::Dark => "\u{2600} light",
                        Theme::Light => "\u{1F319} dark",
                    })
                    .clicked()
                {
                    self.theme = match self.theme {
                        Theme::Dark => Theme::Light,
                        Theme::Light => Theme::Dark,
                    };
                    ui.ctx().set_visuals(visuals_for(self.theme));
                }
                if ui.button("actions").clicked() {
                    self.show_actions = true;
                }
                ui.separator();
                self.draw_timeline(ui);
            });
            if let Some(err) = &self.query_error {
                ui.colored_label(egui::Color32::RED, err);
            }
            if let Some(s) = &self.status {
                ui.label(s);
            }
        });
    }

    /// The whole-library timeline bar, ported from `tui.rs`'s keyboard-
    /// driven bar chart -- click-driven here instead: this is a mouse-first
    /// UI, so pointing at a bar directly replaces the TUI's focus-then-
    /// arrow-keys dance, and there's no on-screen cursor to draw. Assumed
    /// to be drawn as the last widget in its row (see `draw_query_bar`): it
    /// claims whatever width remains.
    fn draw_timeline(&mut self, ui: &mut egui::Ui) {
        const BAR_W: f32 = 5.0;
        const BAR_H: f32 = 28.0;
        ui.label("timeline:");
        if ui.small_button("-").clicked() {
            self.timeline_granularity = self.timeline_granularity.finer();
            self.reload_timeline();
        }
        ui.label(self.timeline_granularity.label());
        if ui.small_button("+").clicked() {
            self.timeline_granularity = self.timeline_granularity.coarser();
            self.reload_timeline();
        }
        if self.timeline.is_empty() {
            return;
        }

        let max_count = self
            .timeline
            .iter()
            .map(|b| b.count)
            .max()
            .unwrap_or(1)
            .max(1);
        let (rect, _resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), BAR_H),
            egui::Sense::hover(),
        );
        let painter = ui.painter_at(rect);
        // Right-aligned, most-recent-first: only as many bars as fit are
        // drawn, and they're the *last* ones in the library rather than the
        // first, so "did today's import land" never needs scrolling to find
        // the right edge -- there's no scrollbar here at all, unlike
        // `tui.rs`'s `timeline_offset`.
        let max_bars = (rect.width() / BAR_W).floor() as usize;
        let start = self.timeline.len().saturating_sub(max_bars.max(1));
        let mut clicked: Option<String> = None;
        for (i, bucket) in self.timeline[start..].iter().enumerate() {
            let x0 = rect.left() + i as f32 * BAR_W;
            let frac = bucket.count as f32 / max_count as f32;
            let h = (BAR_H * frac).max(1.0);
            let bar_rect = egui::Rect::from_min_max(
                egui::pos2(x0, rect.bottom() - h),
                egui::pos2(x0 + BAR_W - 1.0, rect.bottom()),
            );
            let id = ui.id().with("timeline_bar").with(start + i);
            let bar_resp = ui.interact(bar_rect, id, egui::Sense::click());
            let color = if bar_resp.hovered() {
                ui.visuals().selection.bg_fill
            } else {
                ui.visuals().widgets.inactive.bg_fill
            };
            painter.rect_filled(bar_rect, 0.0, color);
            if bar_resp.clicked() {
                clicked = Some(bucket.filter.clone());
            }
            bar_resp.on_hover_text(format!("{}: {}", bucket.label, bucket.count));
        }
        if let Some(fragment) = clicked {
            self.apply_filter(&fragment);
        }
    }

    /// Events / Tags / Folders, each a `CollapsingHeader` section in a left
    /// panel -- three separate, simultaneously-visible sections rather than
    /// the TUI's single Tags/Dates toggle, which was a ratatui-space
    /// compromise, not a design goal.
    fn draw_sidebar(&mut self, ui: &mut egui::Ui) {
        let mut clicked: Option<String> = None;
        egui::Panel::left("sidebar").show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::CollapsingHeader::new("Events")
                    .default_open(true)
                    .show(ui, |ui| {
                        // Grouped by month, Shotwell-style, rather than one
                        // flat list of days -- a library with a year of
                        // near-daily shoots turned that flat list into a
                        // long scroll with no structure to jump around by.
                        for (month, items) in group_events_by_month(&self.events) {
                            let total: i64 = items.iter().map(|e| e.count).sum();
                            let label = month
                                .as_deref()
                                .map_or_else(|| "Undated".to_string(), month_label);
                            let resp = egui::CollapsingHeader::new(format!("{label} ({total})"))
                                .id_salt(month.as_deref().unwrap_or("undated"))
                                .default_open(false)
                                .show(ui, |ui| {
                                    for e in &items {
                                        if ui
                                            .selectable_label(
                                                false,
                                                format!("{} ({})", e.label, e.count),
                                            )
                                            .clicked()
                                        {
                                            clicked = Some(format!("event=\"{}\"", e.label));
                                        }
                                    }
                                });
                            // Same combined expand+filter click as a Tags
                            // parent node -- clicking the month header
                            // filters to the whole month, via the
                            // already-supported `date="YYYY-MM"` whole-month
                            // range. "Undated" has no date to filter by, so
                            // its header only ever toggles.
                            if let Some(m) = &month {
                                if resp.header_response.clicked() {
                                    clicked = Some(format!("date=\"{m}\""));
                                }
                            }
                        }
                    });
                egui::CollapsingHeader::new("Tags")
                    .default_open(true)
                    .show(ui, |ui| {
                        for node in &self.tags {
                            if let Some(f) = draw_tag_node(ui, node) {
                                clicked = Some(f);
                            }
                        }
                    });
                egui::CollapsingHeader::new("Folders")
                    .default_open(false)
                    .show(ui, |ui| {
                        for (dir, count) in &self.folders {
                            if ui
                                .selectable_label(false, format!("{dir} ({count})"))
                                .clicked()
                            {
                                clicked = Some(format!("path=\"{dir}\""));
                            }
                        }
                    });
            });
        });
        if let Some(fragment) = clicked {
            self.apply_filter(&fragment);
        }
    }

    /// EXIF, rating, and tags (add/remove included) for the selected photo
    /// -- or, when a batch is active, a reduced panel whose rating stars and
    /// "add tag" field apply to the whole batch (`self.targets()`) instead.
    /// Reads `self.rows[self.selected]` for the single-photo case -- the
    /// same field the grid, single view's prev/next, and sidebar-filter
    /// reload all share, so this always shows whatever is actually selected
    /// regardless of which changed it.
    fn draw_info_panel(&mut self, ui: &mut egui::Ui) {
        if !self.multi_selected.is_empty() {
            self.draw_batch_info_panel(ui);
            return;
        }
        let id = self.rows.get(self.selected).map(|r| r.id);
        let rating = self.rows.get(self.selected).and_then(|r| r.rating);
        let marks = self.rows.get(self.selected).map(|r| (r.flag, r.reject));
        egui::Panel::right("info").show(ui, |ui| {
            let Some(id) = id else {
                ui.label("no photo selected");
                return;
            };
            ui.heading("Info");
            if let Some(path) = full_path(&self.cat, id) {
                ui.horizontal(|ui| {
                    ui.label(&path);
                    if ui.small_button("copy").clicked() {
                        if let Err(e) = copy_to_clipboard(&path) {
                            self.status = Some(format!("copy failed: {e}"));
                        }
                    }
                });
            }
            if let Some(e) = exif::for_stack(&self.cat, id) {
                if !e.camera.is_empty() {
                    ui.label(&e.camera);
                }
                if !e.lens.is_empty() {
                    ui.label(&e.lens);
                }
                if let Some(t) = e.captured {
                    ui.label(exif::format_datetime(t));
                }
                if let Some(x) = e.exposure.filter(|x| *x > 0.0) {
                    ui.label(format!("S {}", exif::format_shutter(x)));
                }
                if let Some(x) = e.fnumber.filter(|x| *x > 0.0) {
                    ui.label(format!("A f/{x:.1}"));
                }
                if let Some(x) = e.iso.filter(|x| *x > 0) {
                    ui.label(format!("I {x}"));
                }
                if let Some(x) = e.bias {
                    ui.label(format!("EV {}", exif::format_ev(x)));
                }
                if let Some(x) = e.focal.filter(|x| *x > 0.0) {
                    ui.label(format!("{x:.0}mm"));
                }
            } else {
                ui.label("no EXIF data");
            }

            ui.separator();
            ui.label("Rating");
            ui.horizontal(|ui| {
                let stars = rating.unwrap_or(0).clamp(0, 5);
                for n in 1..=5 {
                    if ui.selectable_label(n <= stars, "*").clicked() {
                        self.rate_selected(if n == stars { None } else { Some(n) });
                    }
                }
            });

            ui.label("Mark");
            ui.horizontal(|ui| {
                let (flag, reject) = marks.unwrap_or((false, false));
                // Toggles on click, same as `handle_mark_keys`' `` ` ``/`x`
                // and the rating stars just above.
                if ui.selectable_label(flag, "flag").clicked() {
                    self.mark_selected(&[Change::SetFlag(!flag)], "flag");
                }
                if ui.selectable_label(reject, "reject").clicked() {
                    self.mark_selected(&[Change::SetReject(!reject)], "reject");
                }
            });

            ui.separator();
            ui.label("Tags");
            let tags: Vec<String> = self
                .cat
                .conn
                .prepare(
                    "SELECT t.name FROM tag t JOIN stack_tag st ON st.tag_id = t.id
                     WHERE st.stack_id = ?1 ORDER BY t.name",
                )
                .and_then(|mut st| {
                    let rows = st.query_map([id], |r| r.get::<_, String>(0))?;
                    rows.collect::<std::result::Result<Vec<_>, _>>()
                })
                .unwrap_or_default();
            if tags.is_empty() {
                ui.label("(none)");
            } else {
                let mut to_remove = None;
                for tag in &tags {
                    ui.horizontal(|ui| {
                        ui.label(tag);
                        // A small, deliberately unlabelled button -- the
                        // TUI's `T` (remove tag) has the same one-key
                        // immediacy; this is its mouse equivalent, right
                        // next to the tag it removes rather than a separate
                        // prompt naming it again.
                        if ui.small_button("\u{2715}").clicked() {
                            to_remove = Some(tag.clone());
                        }
                    });
                }
                if let Some(name) = to_remove {
                    self.remove_tag_from_selected(&name);
                }
            }

            ui.horizontal(|ui| {
                // Same "focus before the widget runs" reasoning as the query
                // bar (see its comment for why `has_focus()`/`lost_focus()`
                // both fail here): the field's own Enter handling surrenders
                // its focus during this exact call, so `has_focus()` checked
                // on the response is always already false.
                let tag_id = ui.make_persistent_id("gui_tag_input_single");
                let was_focused = ui.memory(|m| m.has_focus(tag_id));
                ui.add(egui::TextEdit::singleline(&mut self.tag_input).id(tag_id));
                let submitted = was_focused
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                if (ui.button("add").clicked() || submitted) && !self.tag_input.trim().is_empty() {
                    self.add_tags_to_selected(&self.tag_input.clone());
                    self.tag_input.clear();
                }
            });

            ui.separator();
            self.draw_trash_button(ui, "Move to trash");
        });
    }

    /// Below the tag fields in both info panels. No confirmation: trashing
    /// moves files into `.trash/` and `npc untrash` puts them back.
    /// Just the icon; what it does is in the tooltip.
    fn draw_trash_button(&mut self, ui: &mut egui::Ui, what: &str) {
        if ui
            .add_enabled(self.online, egui::Button::new("\u{1F5D1}"))
            .on_hover_text(format!(
                "{what}. Nothing is deleted until `npc trash-empty`; \
                 `npc untrash <base>` restores."
            ))
            .on_disabled_hover_text("The library is not mounted")
            .clicked()
        {
            self.trash_selected();
        }
    }

    /// The multi-select variant of the info panel: no single stack's EXIF or
    /// per-photo tag list makes sense for a batch, so this shows just a
    /// count, plus rating stars and an "add tag" field that both apply to
    /// every stack in `self.multi_selected` via `rate_selected`/
    /// `add_tags_to_selected` (both already read `self.targets()`, which
    /// prefers the batch over the single cursor). No remove-tag list here --
    /// removing a tag that isn't on every selected stack is a real design
    /// question ("remove from all, or only those that have it?") deferred
    /// rather than guessed at.
    fn draw_batch_info_panel(&mut self, ui: &mut egui::Ui) {
        let count = self.multi_selected.len();
        egui::Panel::right("info").show(ui, |ui| {
            ui.heading("Info");
            ui.label(format!("{count} stack(s) selected"));
            if ui.button("clear selection").clicked() {
                self.multi_selected.clear();
            }

            ui.separator();
            ui.label("Rating (applies to all selected)");
            ui.horizontal(|ui| {
                for n in 1..=5 {
                    if ui.selectable_label(false, "*").clicked() {
                        self.rate_selected(Some(n));
                    }
                }
                if ui.small_button("clear").clicked() {
                    self.rate_selected(None);
                }
            });

            ui.label("Mark (applies to all selected)");
            ui.horizontal(|ui| {
                if ui.button("flag").clicked() {
                    self.mark_selected(&[Change::SetFlag(true)], "flag");
                }
                if ui.button("reject").clicked() {
                    self.mark_selected(&[Change::SetReject(true)], "reject");
                }
                if ui.button("clear").clicked() {
                    self.mark_selected(
                        &[Change::SetFlag(false), Change::SetReject(false)],
                        "clear",
                    );
                }
            });

            ui.separator();
            ui.label("Add tag (applies to all selected)");
            ui.horizontal(|ui| {
                // See `draw_query_bar`'s comment for why focus has to be
                // captured *before* this call, not read off its response.
                let tag_id = ui.make_persistent_id("gui_tag_input_batch");
                let was_focused = ui.memory(|m| m.has_focus(tag_id));
                ui.add(egui::TextEdit::singleline(&mut self.tag_input).id(tag_id));
                let submitted = was_focused
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                if (ui.button("add").clicked() || submitted) && !self.tag_input.trim().is_empty() {
                    self.add_tags_to_selected(&self.tag_input.clone());
                    self.tag_input.clear();
                }
            });

            ui.label("Remove tag (applies to all selected)");
            ui.horizontal(|ui| {
                let tag_id = ui.make_persistent_id("gui_tag_input_batch_remove");
                let was_focused = ui.memory(|m| m.has_focus(tag_id));
                ui.add(egui::TextEdit::singleline(&mut self.remove_tag_input).id(tag_id));
                let submitted = was_focused
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                if (ui.button("remove").clicked() || submitted)
                    && !self.remove_tag_input.trim().is_empty()
                {
                    let name = self.remove_tag_input.trim().to_string();
                    self.remove_tag_from_selected(&name);
                    self.remove_tag_input.clear();
                }
            });

            ui.separator();
            self.draw_trash_button(ui, &format!("Move {count} to trash"));
        });
    }

    /// `0`-`5` rate the selected tile (`0` clears), matching `tui.rs`. Same
    /// keyboard-focus guard as `handle_nav_keys`, and for the same reason:
    /// digits are how you type a query like `rating>=3`. Works in both grid
    /// and single view, since both read/write the same `selected` index.
    ///
    /// Guarded by `text_edit_focused()`, not the broader
    /// `egui_wants_keyboard_input()` -- the latter is true whenever ANY
    /// widget has focus, including a plain `Button` after it's been clicked,
    /// so rating a tile with a number key stopped working the instant you'd
    /// clicked anything. `text_edit_focused()` asks the narrower, actually
    /// intended question: is a text field currently taking the keyboard.
    fn handle_rating_keys(&mut self, ui: &egui::Ui) {
        const KEYS: [(egui::Key, Option<i64>); 6] = [
            (egui::Key::Num0, None),
            (egui::Key::Num1, Some(1)),
            (egui::Key::Num2, Some(2)),
            (egui::Key::Num3, Some(3)),
            (egui::Key::Num4, Some(4)),
            (egui::Key::Num5, Some(5)),
        ];
        const FILTER_KEYS: [(egui::Key, usize); 5] = [
            (egui::Key::Num1, 1),
            (egui::Key::Num2, 2),
            (egui::Key::Num3, 3),
            (egui::Key::Num4, 4),
            (egui::Key::Num5, 5),
        ];
        if ui.ctx().text_edit_focused() {
            return;
        }
        // `Ctrl+1`..`Ctrl+5` toggle a rating checkbox (Shotwell's
        // convention); the plain digit below rates the current selection.
        // Checked first and mutually exclusive via the `else`, since a
        // physical `Ctrl+3` keypress is still `key_pressed(Num3) == true`
        // regardless of the held modifier -- without this split it would
        // fire both.
        let ctrl_action = ui.ctx().input(|i| {
            if i.modifiers.ctrl {
                FILTER_KEYS.into_iter().find(|(k, _)| i.key_pressed(*k))
            } else {
                None
            }
        });
        if let Some((_, n)) = ctrl_action {
            self.toggle_rating_filter(n);
            return;
        }
        let action = ui
            .ctx()
            .input(|i| {
                if i.modifiers.ctrl {
                    return None;
                }
                KEYS.into_iter().find(|(k, _)| i.key_pressed(*k))
            })
            .map(|(_, n)| n);
        if let Some(n) = action {
            self.rate_selected(n);
        }
    }

    /// Flag/reject/clear. Bound to `` ` ``/`x`/`c` rather than `tui.rs`'s
    /// `f`/`x`/`c` -- flag sits next to the `1`-`5` rating keys on the same
    /// row of the keyboard instead, specifically because a culling pass
    /// alternates star-rating and pick/reject constantly and the two want to
    /// be reachable without moving off that row.
    ///
    /// Unlike `tui.rs` (a plain "set", always `true`, with `c` as the only
    /// way back to neutral), `` ` `` and `x` here toggle off again on a
    /// second press of the same key -- matching the mouse buttons in the
    /// info panel, which already toggle off on a second click for the same
    /// reason the rating stars do. `c` still exists for "both off in one
    /// press regardless of current state", e.g. moving straight from
    /// rejected to flagged.
    fn handle_mark_keys(&mut self, ui: &egui::Ui) {
        if ui.ctx().text_edit_focused() {
            return;
        }
        let action = ui.ctx().input(|i| {
            if i.key_pressed(egui::Key::Backtick) {
                Some("flag")
            } else if i.key_pressed(egui::Key::X) {
                Some("reject")
            } else if i.key_pressed(egui::Key::C) {
                Some("clear")
            } else {
                None
            }
        });
        let (flag, reject) = self
            .rows
            .get(self.selected)
            .map_or((false, false), |r| (r.flag, r.reject));
        match action {
            Some("flag") => self.mark_selected(&[Change::SetFlag(!flag)], "flag"),
            Some("reject") => self.mark_selected(&[Change::SetReject(!reject)], "reject"),
            Some("clear") => {
                self.mark_selected(&[Change::SetFlag(false), Change::SetReject(false)], "clear");
            }
            _ => {}
        }
    }

    /// The `a` menu, ported from `tui.rs`'s `show_actions` overlay: `a`
    /// opens it (guarded by the same text-field-focus check as every other
    /// key handler here, so typing "a" into the query box stays just
    /// typing), Escape closes it without running anything, and any other
    /// letter key while it's open attempts that action and closes the menu
    /// -- the same catch-all `KeyCode::Char(c) if self.show_actions` the
    /// TUI uses, including reporting "no action bound" for an unmapped key
    /// rather than silently doing nothing.
    fn handle_action_keys(&mut self, ui: &egui::Ui) {
        const LETTERS: [(egui::Key, char); 26] = [
            (egui::Key::A, 'a'),
            (egui::Key::B, 'b'),
            (egui::Key::C, 'c'),
            (egui::Key::D, 'd'),
            (egui::Key::E, 'e'),
            (egui::Key::F, 'f'),
            (egui::Key::G, 'g'),
            (egui::Key::H, 'h'),
            (egui::Key::I, 'i'),
            (egui::Key::J, 'j'),
            (egui::Key::K, 'k'),
            (egui::Key::L, 'l'),
            (egui::Key::M, 'm'),
            (egui::Key::N, 'n'),
            (egui::Key::O, 'o'),
            (egui::Key::P, 'p'),
            (egui::Key::Q, 'q'),
            (egui::Key::R, 'r'),
            (egui::Key::S, 's'),
            (egui::Key::T, 't'),
            (egui::Key::U, 'u'),
            (egui::Key::V, 'v'),
            (egui::Key::W, 'w'),
            (egui::Key::X, 'x'),
            (egui::Key::Y, 'y'),
            (egui::Key::Z, 'z'),
        ];
        if ui.ctx().text_edit_focused() {
            return;
        }
        if self.show_actions {
            let mut close = false;
            let mut fire: Option<char> = None;
            ui.ctx().input(|i| {
                if i.key_pressed(egui::Key::Escape) {
                    close = true;
                }
                if let Some((_, c)) = LETTERS.into_iter().find(|(k, _)| i.key_pressed(*k)) {
                    fire = Some(c);
                }
            });
            if let Some(c) = fire {
                self.show_actions = false;
                self.run_action(c);
            } else if close {
                self.show_actions = false;
            }
        } else if !self.show_export && ui.ctx().input(|i| i.key_pressed(egui::Key::A)) {
            self.show_actions = true;
        }
    }

    /// `e` menu: same open/close/dispatch shape as `handle_action_keys`, but
    /// by position (1 = quick web, 2.. = `export_presets` in config order)
    /// rather than a configured letter, since a preset carries no key of its
    /// own. Mutually exclusive with the actions menu -- each one's "open"
    /// branch checks the other isn't already up, the same way `show_actions`
    /// already suppresses `handle_nav_keys`/`handle_rating_keys` in `ui()`,
    /// so `e` can never simultaneously fire an `[[action]]` keyed `e` and
    /// open this menu.
    fn handle_export_keys(&mut self, ui: &egui::Ui) {
        const DIGITS: [(egui::Key, usize); 9] = [
            (egui::Key::Num1, 0),
            (egui::Key::Num2, 1),
            (egui::Key::Num3, 2),
            (egui::Key::Num4, 3),
            (egui::Key::Num5, 4),
            (egui::Key::Num6, 5),
            (egui::Key::Num7, 6),
            (egui::Key::Num8, 7),
            (egui::Key::Num9, 8),
        ];
        if ui.ctx().text_edit_focused() {
            return;
        }
        if self.show_export {
            let mut close = false;
            let mut fire: Option<usize> = None;
            ui.ctx().input(|i| {
                if i.key_pressed(egui::Key::Escape) {
                    close = true;
                }
                if let Some((_, n)) = DIGITS.into_iter().find(|(k, _)| i.key_pressed(*k)) {
                    fire = Some(n);
                }
            });
            if let Some(n) = fire {
                self.show_export = false;
                self.run_export(n);
            } else if close {
                self.show_export = false;
            }
        } else if !self.show_actions && ui.ctx().input(|i| i.key_pressed(egui::Key::E)) {
            self.show_export = true;
        }
    }

    /// The `a` menu's popup, ported from `tui.rs`'s overlay of the same
    /// purpose: lists every configured action next to its key. Clicking a
    /// row fires it directly -- a mouse affordance the TUI's text-only
    /// overlay has no equivalent of -- and the keys `handle_action_keys`
    /// handles still work while this is open.
    fn draw_actions_menu(&mut self, ui: &mut egui::Ui) {
        if !self.show_actions {
            return;
        }
        let mut close = false;
        let mut fire: Option<char> = None;
        egui::Window::new("actions")
            .collapsible(false)
            .resizable(false)
            .show(ui.ctx(), |ui| {
                if self.actions.is_empty() {
                    ui.label("no actions available (none of the configured programs are on PATH)");
                }
                for a in &self.actions {
                    if ui.button(format!("{}  {}", a.key, a.name)).clicked() {
                        fire = a.key.chars().next();
                    }
                }
                if ui.button("cancel").clicked() {
                    close = true;
                }
            });
        if let Some(c) = fire {
            self.show_actions = false;
            self.run_action(c);
        } else if close {
            self.show_actions = false;
        }
    }

    /// The `e` menu's popup, same shape as `draw_actions_menu`: one button
    /// per option, numbered to match what `handle_export_keys` reads.
    fn draw_export_menu(&mut self, ui: &mut egui::Ui) {
        if !self.show_export {
            return;
        }
        let mut close = false;
        let mut fire: Option<usize> = None;
        egui::Window::new("export")
            .collapsible(false)
            .resizable(false)
            .show(ui.ctx(), |ui| {
                if ui.button("1  Quick web (2000px, quality 75)").clicked() {
                    fire = Some(0);
                }
                for (i, p) in self.export_presets.iter().enumerate() {
                    if ui.button(format!("{}  {}", i + 2, p.name)).clicked() {
                        fire = Some(i + 1);
                    }
                }
                if ui.button("cancel").clicked() {
                    close = true;
                }
            });
        if let Some(n) = fire {
            self.show_export = false;
            self.run_export(n);
        } else if close {
            self.show_export = false;
        }
    }

    /// Arrow/Page keys move the grid selection or scroll, and Space toggles
    /// the tile under the cursor into or out of the batch selection (the
    /// keyboard equivalent of Ctrl/Cmd-click in the grid) -- but the query
    /// box is a text field, and Left/Right/digits are how you edit text in
    /// it. Without this guard, typing e.g. `rating>=3` into the query would
    /// also move the selection and rate the current tile.
    ///
    /// Shift held during an arrow key extends the batch via `select_range`
    /// the same way a Shift-click does, from the same fixed anchor -- a
    /// plain arrow press afterward collapses back to a single selection
    /// (resetting the anchor to wherever it lands), matching what a plain
    /// click already does, so the cursor is never left sitting outside a
    /// stale-looking batch. Escape cancels the batch outright without
    /// otherwise moving anything, independent of either kind of arrow move.
    fn handle_nav_keys(&mut self, ui: &egui::Ui) {
        if ui.ctx().text_edit_focused() {
            return;
        }
        let cols = self.cols.max(1);
        // ScrollArea only answers to the wheel/drag by default -- Page
        // Up/Down need an explicit scroll request.
        let mut scroll_by = 0.0f32;
        let mut toggle_current = false;
        let mut moved = false;
        let mut extend = false;
        let mut clear_selection = false;
        ui.ctx().input(|i| {
            let shift = i.modifiers.shift;
            if i.key_pressed(egui::Key::ArrowRight) && self.selected + 1 < self.rows.len() {
                self.selected += 1;
                moved = true;
                extend = shift;
            }
            if i.key_pressed(egui::Key::ArrowLeft) {
                self.selected = self.selected.saturating_sub(1);
                moved = true;
                extend = shift;
            }
            if i.key_pressed(egui::Key::ArrowDown) {
                self.selected = (self.selected + cols).min(self.rows.len().saturating_sub(1));
                moved = true;
                extend = shift;
            }
            if i.key_pressed(egui::Key::ArrowUp) {
                self.selected = self.selected.saturating_sub(cols);
                moved = true;
                extend = shift;
            }
            if i.key_pressed(egui::Key::Enter) && !self.rows.is_empty() {
                self.mode = Mode::Single;
            }
            if i.key_pressed(egui::Key::Space) {
                toggle_current = true;
            }
            if i.key_pressed(egui::Key::Escape) {
                clear_selection = true;
            }
            if i.key_pressed(egui::Key::PageDown) {
                scroll_by -= 3.0 * (self.tile_px + 8.0);
            }
            if i.key_pressed(egui::Key::PageUp) {
                scroll_by += 3.0 * (self.tile_px + 8.0);
            }
        });
        if moved {
            self.arrow_move(self.selected, extend);
        }
        if toggle_current {
            if let Some(id) = self.rows.get(self.selected).map(|r| r.id) {
                if !self.multi_selected.remove(&id) {
                    self.multi_selected.insert(id);
                }
            }
        }
        if clear_selection {
            self.clear_selection();
        }
        if scroll_by != 0.0 {
            ui.scroll_with_delta(egui::Vec2::new(0.0, scroll_by));
        }
    }

    /// Single view's own keys: Left/Right step through the same `rows`
    /// order the grid built, Escape returns to the grid without disturbing
    /// `selected`. Same keyboard-focus guard as the grid's nav keys.
    fn handle_single_keys(&mut self, ui: &egui::Ui) {
        if ui.ctx().text_edit_focused() {
            return;
        }
        ui.ctx().input(|i| {
            if i.key_pressed(egui::Key::Escape) {
                self.mode = Mode::Grid;
                self.reset_zoom();
            }
            if i.key_pressed(egui::Key::ArrowRight) && self.selected + 1 < self.rows.len() {
                self.selected += 1;
            }
            if i.key_pressed(egui::Key::ArrowLeft) {
                self.selected = self.selected.saturating_sub(1);
            }
        });
    }

    fn draw_grid(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            let tile_px = self.tile_px;
            self.cols = ((ui.available_width() / (tile_px + 8.0)).floor() as usize).max(1);
            let cols = self.cols;
            let ctx = ui.ctx().clone();
            ui.spacing_mut().item_spacing = egui::Vec2::splat(8.0);
            let label_height = ui.text_style_height(&egui::TextStyle::Body);
            // A grid-row's total height: the tile, the gap above the rating
            // label, and the label itself. `show_rows` adds one more
            // `item_spacing.y` between this and the next row on its own.
            let row_height = tile_px + ui.spacing().item_spacing.y + label_height;
            let grid_rows = self.rows.len().div_ceil(cols.max(1));
            let mut open_single = false;
            // `show_rows` only ever builds the tiles it reports as visible,
            // so a not-yet-drawn row has no `Response` for
            // `scroll_to_me` to act on -- the usual trick for scrolling a
            // virtualized list to an arbitrary row instead sets the
            // viewport's offset directly, before asking which rows are
            // visible. Only set when the keyboard just moved the selection
            // (`scroll_to_selected`, consumed here); any other frame leaves
            // the scroll area's own remembered offset alone, so this never
            // fights a scroll the user is doing by hand.
            let mut area = egui::ScrollArea::vertical();
            if self.scroll_to_selected {
                let viewport_height = ui.available_height();
                let row_top = (self.selected / cols.max(1)) as f32 * row_height;
                let centered = (row_top - (viewport_height - row_height) / 2.0).max(0.0);
                area = area.vertical_scroll_offset(centered);
                self.scroll_to_selected = false;
            }
            area.show_rows(ui, row_height, grid_rows, |ui, visible| {
                // Only the grid-rows egui actually reports as scrolled into
                // view are drawn -- and, critically, only for those does
                // `texture_for` ever run. Every stack matching the current
                // query is fetched (`run_query` has no row cap any more:
                // a query the sidebar counted at 1000+ silently showed only
                // a couple hundred before, with no way to reach the rest,
                // and no indication anything had been dropped). Decoding a
                // cached thumbnail into a GPU texture is still synchronous
                // on the UI thread the first time a path is seen
                // (`texture_for`), so without this, an unfiltered query
                // against a 90k-stack library would try to decode all
                // 90,000 thumbnails in one frame the instant it loaded.
                // `ScrollArea::show_rows` is egui's own answer to exactly
                // this shape of problem (see its doc example, a 10,000-row
                // list) -- it reports only the row indices whose vertical
                // span intersects the visible viewport.
                for row in visible {
                    let idx = row * cols;
                    ui.horizontal(|ui| {
                        for i in idx..(idx + cols).min(self.rows.len()) {
                            let id = self.rows[i].id;
                            let rating = self.rows[i].rating;
                            let (flag, reject) = (self.rows[i].flag, self.rows[i].reject);
                            let Ok(Some(src)) = thumbs::best_source(&self.cat, id) else {
                                continue;
                            };
                            // Non-blocking: `None` means generation was
                            // just queued on Cache's background worker
                            // thread. The blocking form (`make_now`) is
                            // what froze the window on a query that
                            // matched hundreds of stacks never before
                            // cached at this size -- generating a whole
                            // page's worth of thumbnails serially,
                            // synchronously, inside the UI thread's own
                            // frame.
                            let tex = self
                                .cache
                                .get(&src, thumbs::Size::Grid)
                                .and_then(|path| self.texture_for(&ctx, &path));
                            ui.vertical(|ui| {
                                ui.set_width(tile_px);
                                let is_selected =
                                    i == self.selected || self.multi_selected.contains(&id);
                                let resp = if let Some(tex) = tex {
                                    // `Button::image(&tex)` alone draws the
                                    // texture at its own native resolution
                                    // (whatever `Size::Grid`'s configured
                                    // pixel target is) regardless of the
                                    // button's requested size -- the zoom
                                    // slider changed the button's hit-box
                                    // but not what was actually drawn.
                                    // `fit_to_exact_size` makes the image
                                    // content itself scale to `tile_px`,
                                    // which is what the slider is
                                    // actually supposed to control.
                                    let img = egui::Image::new(&tex)
                                        .fit_to_exact_size(egui::Vec2::splat(tile_px));
                                    let btn = egui::Button::image(img).selected(is_selected);
                                    ui.add_sized([tile_px, tile_px], btn)
                                } else {
                                    let btn = egui::Button::new("...").selected(is_selected);
                                    ui.add_sized([tile_px, tile_px], btn)
                                };
                                if resp.double_clicked() {
                                    self.selected = i;
                                    self.range_anchor = i;
                                    self.multi_selected.clear();
                                    open_single = true;
                                } else if resp.clicked() {
                                    let (ctrl, shift) = ui.input(|inp| {
                                        (
                                            inp.modifiers.ctrl || inp.modifiers.command,
                                            inp.modifiers.shift,
                                        )
                                    });
                                    self.handle_grid_click(i, id, ctrl, shift);
                                }
                                let stars = "*".repeat(rating.unwrap_or(0).clamp(0, 5) as usize);
                                let mark = if flag {
                                    " F"
                                } else if reject {
                                    " X"
                                } else {
                                    ""
                                };
                                ui.label(format!("{stars}{mark}"));
                            });
                        }
                    });
                }
            });
            if open_single {
                self.mode = Mode::Single;
            }
        });
    }

    /// The full-resolution image, prev/next, and a way back to the grid.
    /// Sidebar and info panel stay mounted around this (drawn by the
    /// caller), so the info panel already shows the right stack -- it reads
    /// the same `selected` index prev/next mutate here.
    fn draw_single(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("\u{2190} grid").clicked() {
                    self.mode = Mode::Grid;
                    self.reset_zoom();
                }
                if ui.button("prev").clicked() {
                    self.selected = self.selected.saturating_sub(1);
                }
                if ui.button("next").clicked() && self.selected + 1 < self.rows.len() {
                    self.selected += 1;
                }
                ui.label(format!("{}/{}", self.selected + 1, self.rows.len().max(1)));
            });
            ui.separator();

            let Some(id) = self.rows.get(self.selected).map(|r| r.id) else {
                ui.label("nothing to show");
                return;
            };
            let Ok(Some(src)) = thumbs::best_source(&self.cat, id) else {
                ui.label("no image source for this stack");
                return;
            };

            let ctx = ui.ctx().clone();
            // `Size::View` by default -- matching `tui.rs`'s own default
            // single view, and cheap (~24ms/frame vs `Full`'s ~140ms,
            // measured elsewhere in this project). Escalates to `Full`
            // once actually zoomed in past fit, same "only pay for it on an
            // explicit zoom-in" rule the TUI's own `+` key follows -- past
            // that point `View`'s ~1620px tier is the thing visibly running
            // out of resolution, not a cost worth avoiding.
            let size_tier = if self.single_zoom > 1.01 {
                thumbs::Size::Full
            } else {
                thumbs::Size::View
            };
            match self
                .cache
                .get(&src, size_tier)
                .and_then(|path| self.texture_for(&ctx, &path))
            {
                Some(tex) => {
                    // Only once this one shows, so it never waits behind its
                    // neighbours. Not when zoomed: a full-resolution
                    // texture is ~100 MB, too much to make on speculation.
                    if size_tier == thumbs::Size::View {
                        self.prefetch_neighbours(&ctx, size_tier);
                    }
                    let avail = ui.available_size();
                    let (viewport, response) =
                        ui.allocate_exact_size(avail, egui::Sense::click_and_drag());

                    let tex_size = tex.size_vec2();
                    let fit_scale =
                        (viewport.width() / tex_size.x).min(viewport.height() / tex_size.y);
                    let scale = fit_scale * self.single_zoom;
                    let old_center = viewport.center() + self.single_pan;

                    // Mouse-wheel zoom anchored at the cursor, RawTherapee-
                    // style: convert the pointer to a fixed point in
                    // image-space first, then solve for the pan that keeps
                    // that same point under the pointer at the new scale,
                    // rather than zooming toward the viewport's center.
                    if let Some(pointer) = response.hover_pos() {
                        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                        if scroll != 0.0 {
                            let new_zoom = (self.single_zoom * (scroll * 0.002).exp())
                                .clamp(MIN_SINGLE_ZOOM, MAX_SINGLE_ZOOM);
                            let new_scale = fit_scale * new_zoom;
                            let ratio = new_scale / scale;
                            let new_center = pointer + (old_center - pointer) * ratio;
                            self.single_pan = new_center - viewport.center();
                            self.single_zoom = new_zoom;
                        }
                    }
                    if response.dragged() {
                        self.single_pan += response.drag_delta();
                    }

                    // Recompute after a possible zoom/drag this frame, so
                    // the image drawn below is never one frame stale.
                    let scale = fit_scale * self.single_zoom;
                    let center = viewport.center() + self.single_pan;
                    let image_rect = egui::Rect::from_center_size(center, tex_size * scale);

                    ui.painter_at(viewport).image(
                        tex.id(),
                        image_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }
                None => {
                    ui.label("loading image...");
                }
            }
        });
    }

    /// `Cache::get` queues generation on a background thread and returns
    /// immediately; a tile that was `None` this frame needs a later repaint
    /// to pick up the finished thumbnail. eframe is reactive by default (it
    /// does not redraw on a timer the way the TUI's 120ms event-poll does),
    /// so without this, thumbnails would only ever appear on the next frame
    /// triggered by some other input -- a mouse move, a keypress. Draining
    /// `done` mirrors `tui.rs`'s run loop: each completion is a reason to
    /// repaint, and draining keeps the channel from growing unbounded.
    fn poll_thumbnail_generation(&self, ctx: &egui::Context) {
        while self.cache.done.try_recv().is_ok() {}
        ctx.request_repaint_after(std::time::Duration::from_millis(120));
    }

    /// The watcher runs on its own thread and its own catalog connection;
    /// draining here is what turns "a directory was just rescanned" into
    /// "the screen reflects it," same shape as `poll_thumbnail_generation`
    /// just above. One re-run of the current query covers however many
    /// directories settled between frames.
    fn poll_watcher(&mut self) {
        let Some(watcher) = self.watcher.as_ref() else {
            return;
        };
        let mut rescanned = false;
        loop {
            match watcher.changed.try_recv() {
                Ok(watch::WatchEvent::Ready { dirs }) => {
                    self.watch_state = WatchState::Watching { dirs };
                }
                Ok(watch::WatchEvent::Scanning(dir)) => {
                    self.watch_state = WatchState::Scanning {
                        dirs: self.watch_state.dirs(),
                        dir,
                    };
                }
                Ok(watch::WatchEvent::Rescanned(_)) => {
                    rescanned = true;
                    self.watch_state = WatchState::Watching {
                        dirs: self.watch_state.dirs(),
                    };
                }
                Ok(watch::WatchEvent::Error(e)) => {
                    // During setup an error means the watcher never started.
                    if self.watch_state == WatchState::Starting {
                        self.watch_state = WatchState::Failed(e.clone());
                    }
                    self.status = Some(format!("watch: {e}"));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if !matches!(self.watch_state, WatchState::Failed(_)) {
                        self.watch_state = WatchState::Failed("the watcher stopped".into());
                    }
                    break;
                }
            }
        }
        if rescanned {
            if let Ok(rows) = run_query(&self.cat, &self.query, self.rating_filter) {
                self.rows = rows;
            }
            self.reload_sidebar();
            self.reload_timeline();
        }
    }
}

fn visuals_for(theme: Theme) -> egui::Visuals {
    match theme {
        Theme::Dark => egui::Visuals::dark(),
        Theme::Light => egui::Visuals::light(),
    }
}

/// GPU memory for decoded thumbnails and view images: ~70 single-view
/// images at 1620 px, or ~750 grid tiles at 500 px.
const TEXTURE_BUDGET: usize = 512 * 1024 * 1024;

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame += 1;
        self.poll_watcher();
        self.draw_query_bar(ui);
        self.draw_sidebar(ui);
        self.draw_info_panel(ui);
        // Snapshotted *before* either handler runs, not re-read after: both
        // handlers can flip their own flag to `false` within this same call
        // (a digit/letter that both dispatches and closes the menu), and the
        // menu's own key is still "pressed" for the rest of this frame --
        // egui's `key_pressed` isn't consumed just because one handler acted
        // on it. Gating on the post-call value let that identical keypress
        // fall through into `handle_rating_keys` this same frame: closing
        // the export menu with `1` also rated the tile `1` star, silently
        // overwriting whatever rating it already had. Reproduced live and
        // confirmed by a real user before this fix.
        let actions_was_open = self.show_actions;
        let export_was_open = self.show_export;
        self.handle_action_keys(ui);
        self.handle_export_keys(ui);
        match self.mode {
            Mode::Grid => {
                // While the actions or export menu is open, every other key
                // is its input (see `handle_action_keys`/`handle_export_keys`)
                // -- same early-exit `tui.rs`'s `on_key` does for
                // `show_actions`, so e.g. its digit keys can't also rate the
                // tile underneath it.
                if !actions_was_open && !export_was_open {
                    self.handle_nav_keys(ui);
                    self.handle_rating_keys(ui);
                    self.handle_mark_keys(ui);
                }
                self.draw_grid(ui);
            }
            Mode::Single => {
                if !actions_was_open && !export_was_open {
                    self.handle_single_keys(ui);
                    self.handle_rating_keys(ui);
                    self.handle_mark_keys(ui);
                }
                self.draw_single(ui);
            }
        }
        self.draw_actions_menu(ui);
        self.draw_export_menu(ui);
        self.poll_thumbnail_generation(ui.ctx());
        self.evict_textures(TEXTURE_BUDGET);
        // Last, so an edit made this frame already counts as "not idle".
        self.drive_writer(ui.ctx());
    }
}

/// What the library watcher is doing, as the eye in the query bar shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WatchState {
    /// Switched off in config, or the drive is offline: nothing to watch.
    Off,
    /// Placing its watches, which walks every watched folder.
    Starting,
    Watching {
        dirs: usize,
    },
    /// Cataloging a folder that just changed.
    Scanning {
        dirs: usize,
        dir: PathBuf,
    },
    Failed(String),
}

impl WatchState {
    /// Folders watched, once known.
    fn dirs(&self) -> usize {
        match self {
            WatchState::Watching { dirs } | WatchState::Scanning { dirs, .. } => *dirs,
            _ => 0,
        }
    }
}

/// What the query bar's badge says about the sidecar queue.
#[derive(Debug, PartialEq, Eq)]
enum WriteState {
    /// Nothing queued: every edit is on disk.
    Saved,
    /// Queued, waiting for input to pause.
    Waiting(i64),
    /// Input paused: writing now.
    Writing(i64),
    /// Window close held until these are written.
    Closing(i64),
    /// Drive not mounted: queued in the catalog until it is.
    Offline(i64),
}

impl GuiApp {
    fn write_state(&self) -> WriteState {
        let queued = edit::queued(&self.cat).unwrap_or(0);
        let todo = queued - i64::try_from(self.writer.stuck()).unwrap_or(0);
        if !self.online {
            WriteState::Offline(queued)
        } else if todo <= 0 {
            WriteState::Saved
        } else if self.closing {
            WriteState::Closing(todo)
        } else if self.writer.is_idle() {
            WriteState::Writing(todo)
        } else {
            WriteState::Waiting(todo)
        }
    }

    fn draw_write_state(&self, ui: &mut egui::Ui) {
        let amber = egui::Color32::from_rgb(230, 170, 40);
        let green = egui::Color32::from_rgb(90, 170, 90);
        let red = egui::Color32::from_rgb(220, 80, 60);
        match self.write_state() {
            WriteState::Saved => {
                ui.colored_label(green, "saved")
                    .on_hover_text("Every rating, mark and tag is written to its sidecar.");
            }
            WriteState::Waiting(n) => {
                ui.colored_label(amber, format!("{n} to write"))
                    .on_hover_text(
                        "Changes are in the catalog; their sidecars are written \
                     once you pause. Closing the window waits for them.",
                    );
            }
            WriteState::Writing(n) => {
                ui.spinner();
                ui.colored_label(amber, format!("writing {n}"));
            }
            WriteState::Closing(n) => {
                ui.spinner();
                ui.colored_label(amber, format!("saving {n} before closing"));
            }
            WriteState::Offline(n) => {
                let text = if n > 0 {
                    format!("OFFLINE - {n} queued")
                } else {
                    "OFFLINE".to_string()
                };
                ui.colored_label(amber, text).on_hover_text(
                    "The library drive isn't mounted. Changes stay queued in \
                     the catalog and are written the next time it is.",
                );
            }
        }
        let stuck = self.writer.stuck();
        if stuck > 0 && self.online {
            ui.colored_label(red, format!("{stuck} can't be written"))
                .on_hover_text(
                    "Their folder is missing or protected. They stay queued in \
                 the catalog; editing one tries it again, `npc sync` retries all.",
                );
        }
    }

    /// The watcher as a little eye beside the saved/writing badge: drowsy
    /// while it sets up, open and blinking now and then while it watches,
    /// pupil following the pointer, pupil circling while a folder is being
    /// cataloged, asleep when there's nothing to watch, a red cross when it
    /// failed. Following the pointer is free -- pointer moves repaint
    /// anyway -- and the blink costs a few frames every few seconds.
    fn draw_watch_eye(&self, ui: &mut egui::Ui) {
        use egui::{pos2, vec2, Color32, Shape, Stroke};
        let (rect, response) = ui.allocate_exact_size(vec2(26.0, 18.0), egui::Sense::hover());
        let painter = ui.painter_at(rect.expand(3.0));
        let centre = rect.center();
        let radius = vec2(10.0, 6.5);
        let ink = ui.visuals().text_color();
        let line = Stroke::new(1.4, ink);
        let sclera = if ui.visuals().dark_mode {
            Color32::from_gray(225)
        } else {
            Color32::WHITE
        };
        let time = ui.input(|i| i.time);
        let ctx = ui.ctx().clone();

        // A closed lid: the lower half of the eye's outline, with lashes.
        let closed = |painter: &egui::Painter, stroke: Stroke| {
            let pts: Vec<egui::Pos2> = (0..=12)
                .map(|i| {
                    let angle = std::f32::consts::PI * i as f32 / 12.0;
                    pos2(
                        centre.x + radius.x * angle.cos(),
                        centre.y + 1.5 + radius.y * 0.55 * angle.sin(),
                    )
                })
                .collect();
            painter.add(Shape::line(pts, stroke));
            for (dx, len) in [(-5.0, 3.0), (0.0, 3.5), (5.0, 3.0)] {
                let base = pos2(
                    centre.x + dx,
                    centre.y + 1.5 + radius.y * 0.55 * (1.0 - (dx / radius.x).powi(2)).sqrt(),
                );
                painter.line_segment([base, base + vec2(dx * 0.15, len)], stroke);
            }
        };
        // An open eye with the iris at `look` (offset from centre).
        let open = |painter: &egui::Painter, iris: Color32, look: egui::Vec2, lid: f32| {
            let er = vec2(radius.x, radius.y * lid);
            painter.add(Shape::ellipse_filled(centre, er, sclera));
            let ic = centre + vec2(look.x.clamp(-4.5, 4.5), (look.y * lid).clamp(-2.0, 2.0));
            painter.circle_filled(ic, 4.2 * lid.max(0.6), iris);
            painter.circle_filled(ic, 2.0 * lid.max(0.6), Color32::from_gray(20));
            painter.circle_filled(ic + vec2(-1.2, -1.3), 0.9, Color32::WHITE);
            painter.add(Shape::ellipse_stroke(centre, er, line));
        };
        let green = Color32::from_rgb(90, 170, 90);
        let amber = Color32::from_rgb(230, 170, 40);
        let blue = Color32::from_rgb(80, 150, 230);
        let red = Color32::from_rgb(220, 80, 60);

        let tip = match &self.watch_state {
            WatchState::Off => {
                closed(&painter, Stroke::new(1.4, ink.gamma_multiply(0.5)));
                painter.text(
                    rect.right_top() + vec2(-1.0, -2.0),
                    egui::Align2::RIGHT_TOP,
                    "z",
                    egui::FontId::proportional(9.0),
                    ink.gamma_multiply(0.6),
                );
                if self.online {
                    "Not watching the library: switched off ([watch] enabled = false, \
                     or --no-watch)."
                        .to_string()
                } else {
                    "Not watching: the library drive isn't mounted.".to_string()
                }
            }
            WatchState::Starting => {
                // Drowsy: half-open, the lid slowly breathing.
                let lid = 0.45 + 0.1 * (time as f32 * 2.5).sin();
                open(&painter, amber, vec2(0.0, 1.0), lid);
                ctx.request_repaint_after(std::time::Duration::from_millis(60));
                "Waking up: placing watches on the library's folders.".to_string()
            }
            WatchState::Watching { dirs } => {
                const PERIOD: f64 = 4.7;
                const BLINK: f64 = 0.14;
                let phase = time % PERIOD;
                if phase < BLINK {
                    closed(&painter, line);
                    ctx.request_repaint_after(std::time::Duration::from_secs_f64(BLINK - phase));
                } else {
                    let look = ctx
                        .pointer_latest_pos()
                        .map_or(egui::Vec2::ZERO, |pointer| {
                            let delta = pointer - centre;
                            delta.normalized() * (delta.length() / 60.0).min(1.0) * 4.5
                        });
                    open(&painter, green, look, 1.0);
                    ctx.request_repaint_after(std::time::Duration::from_secs_f64(PERIOD - phase));
                }
                format!(
                    "Watching {dirs} folder(s) outside the dated import tree: \
                     new files there are cataloged on their own."
                )
            }
            WatchState::Scanning { dir, .. } => {
                let angle = time as f32 * 7.0;
                open(&painter, blue, vec2(angle.cos(), angle.sin()) * 3.5, 1.0);
                ctx.request_repaint();
                format!("Cataloging {}", dir.display())
            }
            WatchState::Failed(e) => {
                closed(&painter, Stroke::new(1.4, ink.gamma_multiply(0.5)));
                let cross = Stroke::new(2.0, red);
                painter.line_segment([centre + vec2(-4.0, -5.0), centre + vec2(4.0, 3.0)], cross);
                painter.line_segment([centre + vec2(4.0, -5.0), centre + vec2(-4.0, 3.0)], cross);
                format!("The library watcher stopped: {e}")
            }
        };
        response.on_hover_text(tip);
    }

    /// Sidecars the writer could still write: queued, drive mounted, and not
    /// stuck (folder gone, protected).
    fn writable(&self) -> bool {
        let queued = edit::queued(&self.cat).unwrap_or(0);
        self.online && queued > i64::try_from(self.writer.stuck()).unwrap_or(i64::MAX)
    }

    /// Writes queued sidecars while input pauses, and holds a window close
    /// until nothing writable is left. Offline there is nothing to write to:
    /// the close goes through and the queue waits in the catalog.
    fn drive_writer(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && self.writable() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.closing = true;
        }
        if !self.writable() {
            if self.closing {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            return;
        }
        if !self.closing && !self.writer.is_idle() {
            ctx.request_repaint_after(self.writer.until_idle());
            return;
        }
        // Idle: one ~65 ms write per frame, so a key pressed meanwhile waits
        // at most one write. Closing: several per frame, progress still shown.
        let budget = if self.closing { 8 } else { 1 };
        for _ in 0..budget {
            match self.writer.step(&mut self.cat, &self.config) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    self.status = Some(format!("writing sidecars: {e}"));
                    break;
                }
            }
        }
        ctx.request_repaint();
    }
}

/// Resolve config, open the catalog, and launch the window. Mirrors
/// `tui::run`'s startup order.
pub fn run(root: &Path, no_watch: bool) -> Result<()> {
    let mut config = Config::load().unwrap_or_default();
    if no_watch {
        config.watch.enabled = false;
    }
    // Must happen before any `thumbs::Cache` use, or generation silently
    // falls back to the hardcoded 1280px default -- see thumbs::configure.
    thumbs::configure(&config.thumbs);

    // The catalog lives on this machine, not the drive (README's "Offline"
    // section) -- browsing cached thumbnails, tagging and rating all still
    // work without it, matching `tui.rs`'s own offline mode exactly rather
    // than refusing to launch at all.
    let (cat, online) = Catalog::open_for_root(root)?;
    // `vol::identify` needs the path to actually resolve to a mounted
    // filesystem, which is exactly what's missing offline -- the catalog's
    // own `volume` table remembers the UUID the cache was built under
    // regardless, so thumbnails already on disk are still found by it.
    let cache_uuid = if online {
        vol::identify(root)?.uuid
    } else {
        cat.conn
            .query_row("SELECT vol_uuid FROM volume LIMIT 1", [], |r| r.get(0))
            .unwrap_or_default()
    };
    let cache = thumbs::Cache::new(&cache_uuid)?;
    let theme = config.gui.theme;
    // The watcher needs the real filesystem, same precondition as
    // generating a thumbnail that was never cached -- offline, there is
    // nothing to watch. A failure to start otherwise (disabled in config, or
    // the watcher itself erroring) is never fatal to launching the GUI.
    let watcher = if online {
        watch::Watcher::spawn(root.to_path_buf(), &config).unwrap_or(None)
    } else {
        None
    };

    // Embedded at compile time -- see assets/icon-h2-3x3-real-emoji.svg for
    // the source and README.md's "Icon" section for the required Twemoji
    // attribution. A decode failure here (never expected; it's our own
    // checked-in asset) just means no icon, not a launch failure.
    let icon = image::load_from_memory(include_bytes!("../assets/npcg-icon.png"))
        .ok()
        .map(|img| {
            let img = img.to_rgba8();
            let (width, height) = img.dimensions();
            egui::IconData {
                rgba: img.into_raw(),
                width,
                height,
            }
        });

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_icon(icon.unwrap_or_default())
            // `with_icon` alone does nothing on Wayland: there is no
            // protocol for an app to hand the compositor its own window
            // icon (unlike X11's _NET_WM_ICON, which `with_icon` sets
            // directly and unconditionally). GNOME/Mutter looks the icon
            // up via this app_id instead, matched against a `.desktop`
            // file's own name -- see that file's Icon= key for the actual
            // installed icon. Harmless to also set now that X11 is forced
            // below, in case that ever changes back.
            .with_app_id("npcg"),
        // Forced rather than left to winit's own Wayland-first autodetection
        // (real here, since `WAYLAND_DISPLAY` is normally set): this
        // project's own measurement found `egui-winit`'s native-Wayland
        // clipboard path (`smithay-clipboard`) silently does nothing on this
        // compositor -- no error anywhere, `Response::scroll_to_me`-style
        // "it just didn't happen". `arboard`'s X11 path (reached once
        // winit hands it an X11/XWayland display instead) was confirmed
        // working end-to-end (`xclip -o` read back exactly what was copied).
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::x11::EventLoopBuilderExtX11;
            builder.with_x11();
        })),
        ..Default::default()
    };

    let root = root.to_path_buf();
    eframe::run_native(
        "npcg",
        native_options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(visuals_for(theme));
            let ctx = cc.egui_ctx.clone();
            cache.on_done(move || ctx.request_repaint());
            Ok(Box::new(GuiApp::new(
                cat, cache, config, watcher, root, online,
            )))
        }),
    )
    .map_err(|e| anyhow!("eframe: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(name: &str, count: i64) -> (String, i64) {
        (name.to_string(), count)
    }

    fn ev(label: &str, count: i64, month: Option<&str>) -> EventEntry {
        EventEntry {
            label: label.to_string(),
            count,
            month: month.map(str::to_string),
        }
    }

    /// A three-stack library on disk, ratings pre-set, so the rating filter
    /// has something real to narrow -- mirrors `tui.rs`'s own `app()` test
    /// fixture.
    fn app() -> (tempfile::TempDir, GuiApp) {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026/08/09");
        std::fs::create_dir_all(&day).unwrap();
        for n in 1..=3 {
            std::fs::write(day.join(format!("750_000{n}.NEF")), format!("raw{n}")).unwrap();
        }
        let mut cat = Catalog::open_memory().unwrap();
        crate::scan::scan(&mut cat, dir.path(), true).unwrap();
        let ids: Vec<i64> = cat
            .conn
            .prepare("SELECT id FROM stack ORDER BY base")
            .unwrap()
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        edit::apply(
            &mut cat,
            &[ids[0]],
            &[Change::SetRating(Some(2))],
            &Config::default(),
        )
        .unwrap();
        edit::apply(
            &mut cat,
            &[ids[1]],
            &[Change::SetRating(Some(5))],
            &Config::default(),
        )
        .unwrap();
        let cache = thumbs::Cache::new("gui-test-rating-filter").unwrap();
        let app = GuiApp::new(
            cat,
            cache,
            Config::default(),
            None,
            dir.path().to_path_buf(),
            true,
        );
        (dir, app)
    }

    #[test]
    fn month_keys_render_as_a_name_and_year() {
        assert_eq!(month_label("2026-08"), "August 2026");
        assert_eq!(month_label("2026-01"), "January 2026");
        assert_eq!(month_label("2026-12"), "December 2026");
    }

    /// Malformed input can't come from `EventEntry::month` in practice (it's
    /// always sliced from `exif::format_date`'s own output), but the
    /// fallback must not panic if that ever changes.
    #[test]
    fn a_malformed_month_key_falls_back_to_itself_rather_than_panicking() {
        assert_eq!(month_label("not-a-month"), "not-a-month");
        assert_eq!(month_label("2026-13"), "2026-13");
        assert_eq!(month_label("2026-00"), "2026-00");
    }

    /// Consecutive same-month entries merge into one run; a month change
    /// starts a new one, and "Undated" (`month: None`) is its own run at
    /// the end -- matching the descending-time-then-NULLs-last order
    /// `reload_sidebar`'s query actually produces.
    #[test]
    fn events_group_into_contiguous_month_runs() {
        let events = vec![
            ev("2026-08-09", 2, Some("2026-08")),
            ev("Berlin trip", 5, Some("2026-08")),
            ev("2026-07-14", 1, Some("2026-07")),
            ev("undated", 3, None),
            ev("also undated", 1, None),
        ];
        let groups = group_events_by_month(&events);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].0.as_deref(), Some("2026-08"));
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0.as_deref(), Some("2026-07"));
        assert_eq!(groups[1].1.len(), 1);
        assert_eq!(groups[2].0, None);
        assert_eq!(groups[2].1.len(), 2);
    }

    #[test]
    fn a_flat_list_with_no_slashes_stays_flat() {
        let tree = build_tag_tree(&[n("beach", 3), n("family", 2)]);
        assert_eq!(tree.len(), 2);
        assert!(tree.iter().all(|node| node.children.is_empty()));
    }

    #[test]
    fn a_slash_name_nests_under_its_parent() {
        let tree = build_tag_tree(&[n("Trips", 1), n("Trips/Paris", 4), n("Trips/Paris/Day1", 2)]);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].name, "Trips");
        assert_eq!(tree[0].own_count, Some(1));
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].name, "Paris");
        assert_eq!(tree[0].children[0].path, "Trips/Paris");
        assert_eq!(tree[0].children[0].children[0].name, "Day1");
    }

    /// A parent that is never itself a real tag -- only its children are --
    /// must still appear, as a synthetic grouping node with no own count.
    #[test]
    fn a_synthetic_parent_groups_its_children_without_its_own_tag_row() {
        let tree = build_tag_tree(&[n("Trips/Paris", 4), n("Trips/Berlin", 1)]);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].name, "Trips");
        assert_eq!(tree[0].own_count, None);
        assert_eq!(tree[0].children.len(), 2);
    }

    /// A parent header's count must reflect everything nested under it, not
    /// just shots literally tagged with the bare parent name.
    #[test]
    fn total_count_sums_a_node_and_all_its_descendants() {
        let tree = build_tag_tree(&[n("Trips", 1), n("Trips/Paris", 4), n("Trips/Berlin", 2)]);
        assert_eq!(tree[0].total_count(), 7);
    }

    #[test]
    fn two_branches_sharing_a_leaf_name_do_not_collide() {
        let tree = build_tag_tree(&[n("Trips/Paris", 1), n("Family/Paris", 1)]);
        assert_eq!(tree.len(), 2);
        let paths: Vec<&str> = tree
            .iter()
            .flat_map(|t| t.children.iter().map(|c| c.path.as_str()))
            .collect();
        assert!(paths.contains(&"Trips/Paris"));
        assert!(paths.contains(&"Family/Paris"));
    }

    /// `toggle_rating_filter` is what both the checkbox click and the
    /// `Ctrl+N` shortcut call -- this exercises the shared logic directly,
    /// same reasoning `handle_rating_keys`'s doc comment gives for why the
    /// two must go through one path rather than duplicating it.
    #[test]
    fn toggle_rating_filter_narrows_and_widens_the_rows() {
        let (_dir, mut app) = app();
        assert_eq!(app.rows.len(), 3, "no filter set yet");

        app.toggle_rating_filter(2);
        assert!(app.rating_filter[1]);
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.rows[0].rating, Some(2));

        app.toggle_rating_filter(5);
        assert_eq!(
            app.rows.len(),
            2,
            "rating=2 or rating=5 should both show once both are checked"
        );

        app.toggle_rating_filter(2);
        assert!(!app.rating_filter[1]);
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.rows[0].rating, Some(5));

        app.toggle_rating_filter(5);
        assert_eq!(
            app.rows.len(),
            3,
            "clearing every box shows everything again"
        );
    }

    /// The info panel's path is absolute, unlike the grid/single-view label
    /// elsewhere in this project (`tui.rs`'s own `current_label`, kept
    /// relative so it fits a terminal line) -- this is the one place meant
    /// to answer "where is this on disk" without the library root in hand.
    #[test]
    fn full_path_is_absolute_and_names_the_file() {
        let (dir, app) = app();
        let path = full_path(&app.cat, app.rows[0].id).unwrap();
        assert!(path.starts_with(dir.path().to_str().unwrap()), "got {path}");
        // The fixture names really are uppercase -- this is asserting the
        // fixture's own naming, not classifying a real extension case-sensitively.
        #[allow(
            clippy::case_sensitive_file_extension_comparisons,
            reason = "checking the fixture's literal name, not classifying a role"
        )]
        {
            assert!(path.ends_with(".NEF"), "got {path}");
        }
    }

    #[test]
    fn shift_click_selects_the_inclusive_range_from_the_anchor() {
        let (_d, mut app) = app();
        let ids: Vec<i64> = app.rows.iter().map(|r| r.id).collect();
        app.handle_grid_click(0, ids[0], false, false);
        app.handle_grid_click(2, ids[2], false, true);
        assert_eq!(app.selected, 2);
        assert_eq!(app.multi_selected, ids.iter().copied().collect());
    }

    /// A plain or Ctrl-click moves the anchor; a Shift-click must not, or a
    /// second Shift-click would extend from wherever the first one landed
    /// instead of the original starting tile.
    #[test]
    fn a_second_shift_click_still_extends_from_the_original_anchor() {
        let (_d, mut app) = app();
        let ids: Vec<i64> = app.rows.iter().map(|r| r.id).collect();
        app.handle_grid_click(0, ids[0], false, false);
        app.handle_grid_click(2, ids[2], false, true);
        app.handle_grid_click(1, ids[1], false, true);
        assert_eq!(
            app.multi_selected,
            [ids[0], ids[1]].into_iter().collect(),
            "anchor should still be tile 0, not tile 2 from the previous shift-click"
        );
    }

    #[test]
    fn ctrl_click_toggles_without_clearing_the_rest() {
        let (_d, mut app) = app();
        let ids: Vec<i64> = app.rows.iter().map(|r| r.id).collect();
        app.handle_grid_click(0, ids[0], true, false);
        app.handle_grid_click(1, ids[1], true, false);
        assert_eq!(app.multi_selected, [ids[0], ids[1]].into_iter().collect());

        app.handle_grid_click(0, ids[0], true, false);
        assert_eq!(app.multi_selected, [ids[1]].into_iter().collect());
    }

    #[test]
    fn a_plain_click_clears_the_batch_and_resets_the_anchor() {
        let (_d, mut app) = app();
        let ids: Vec<i64> = app.rows.iter().map(|r| r.id).collect();
        app.handle_grid_click(0, ids[0], true, false);
        app.handle_grid_click(2, ids[2], false, false);
        assert!(app.multi_selected.is_empty());
        assert_eq!(app.selected, 2);
        assert_eq!(app.range_anchor, 2);
    }

    #[test]
    fn an_out_of_range_export_option_is_reported() {
        let (_d, mut app) = app();
        let out = tempfile::tempdir().unwrap();
        app.config.export_dir = Some(out.path().to_path_buf());
        app.run_export(5);
        assert_eq!(app.status.as_deref(), Some("no export option 5"));
    }

    /// `cp` stands in for a real preset command, same reasoning as
    /// `export.rs`'s own `a_preset_is_invoked_and_its_output_required` --
    /// this exercises the dispatch and subdirectory naming, not image
    /// decoding (the fixture's NEF bytes are not a real photograph).
    #[test]
    fn a_preset_export_writes_into_a_subdirectory_named_after_it() {
        let (_d, mut app) = app();
        let out = tempfile::tempdir().unwrap();
        app.config.export_dir = Some(out.path().to_path_buf());
        app.export_presets = vec![export::Preset {
            name: "copy".into(),
            cmd: vec!["cp".into(), "{src}".into(), "{dst}".into()],
            ext: "jpg".into(),
        }];
        app.run_export(1);
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .starts_with("exported 1 "),
            "got {:?}",
            app.status
        );
        assert!(out.path().join("copy/750_0001.jpg").exists());
    }

    #[test]
    fn shift_arrow_extends_the_batch_from_the_fixed_anchor() {
        let (_d, mut app) = app();
        let ids: Vec<i64> = app.rows.iter().map(|r| r.id).collect();
        app.range_anchor = 0;
        app.arrow_move(2, true);
        assert_eq!(app.selected, 2);
        assert_eq!(app.multi_selected, ids.iter().copied().collect());

        // A second Shift-move from the same anchor, not from where the
        // first one landed.
        app.arrow_move(1, true);
        assert_eq!(
            app.multi_selected,
            [ids[0], ids[1]].into_iter().collect(),
            "should still extend from tile 0, not tile 2"
        );
    }

    #[test]
    fn a_plain_arrow_move_collapses_the_batch_and_resets_the_anchor() {
        let (_d, mut app) = app();
        let ids: Vec<i64> = app.rows.iter().map(|r| r.id).collect();
        app.multi_selected = ids.iter().copied().collect();
        app.arrow_move(1, false);
        assert!(app.multi_selected.is_empty());
        assert_eq!(app.selected, 1);
        assert_eq!(app.range_anchor, 1);
    }

    #[test]
    fn escape_clears_the_batch_without_moving_the_cursor() {
        let (_d, mut app) = app();
        let ids: Vec<i64> = app.rows.iter().map(|r| r.id).collect();
        app.multi_selected = ids.iter().copied().collect();
        app.selected = 2;
        app.clear_selection();
        assert!(app.multi_selected.is_empty());
        assert_eq!(app.selected, 2, "escape must not move the cursor");
        assert_eq!(app.range_anchor, 2);
    }

    /// The batch panel's "remove tag" field has no per-tag list to click --
    /// it applies a typed name to every selected stack, so a stack that
    /// never had the tag is an unaffected no-op, not an error.
    #[test]
    fn remove_tag_from_selected_strips_it_from_every_target_in_the_batch() {
        let (_d, mut app) = app();
        let ids: Vec<i64> = app.rows.iter().map(|r| r.id).collect();
        edit::apply(
            &mut app.cat,
            &[ids[0], ids[1]],
            &[Change::AddTags(vec!["beach".into()])],
            &Config::default(),
        )
        .unwrap();
        app.multi_selected = [ids[0], ids[1]].into_iter().collect();

        app.remove_tag_from_selected("beach");

        for &id in &ids[..2] {
            let tags: Vec<String> = app
                .cat
                .conn
                .prepare(
                    "SELECT t.name FROM tag t JOIN stack_tag st ON st.tag_id = t.id
                     WHERE st.stack_id = ?1",
                )
                .unwrap()
                .query_map([id], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            assert!(tags.is_empty(), "tag survived on stack {id}: {tags:?}");
        }
    }

    /// Checking the same detail across a burst means the zoom must survive
    /// moving to the next frame -- nothing in `handle_single_keys` or the
    /// prev/next buttons touches `single_zoom`/`single_pan` any more, unlike
    /// the removed per-stack auto-reset this replaced. `reset_zoom` (Escape,
    /// or the "<- grid" button) is the only way back to fit-to-window.
    #[test]
    fn zoom_survives_navigation_and_only_resets_explicitly() {
        let (_d, mut app) = app();
        app.single_zoom = 2.5;
        app.single_pan = egui::Vec2::new(10.0, -5.0);

        app.selected = 1;
        // Values set directly above, not computed -- an exact comparison is
        // correct here, same reasoning as thumbs.rs's own float_cmp allow.
        #[allow(clippy::float_cmp, reason = "values set directly above, not computed")]
        {
            assert_eq!(app.single_zoom, 2.5);
            assert_eq!(app.single_pan, egui::Vec2::new(10.0, -5.0));
        }

        app.reset_zoom();
        #[allow(
            clippy::float_cmp,
            reason = "comparing against reset_zoom's own literal constants"
        )]
        {
            assert_eq!(app.single_zoom, 1.0);
            assert_eq!(app.single_pan, egui::Vec2::ZERO);
        }
    }

    /// `mark_selected` must agree with `tui.rs`'s own mutual-exclusivity
    /// guarantee (enforced in `edit.rs`, not here): flagging a rejected
    /// stack clears the reject, and vice versa.
    #[test]
    fn flag_and_reject_are_mutually_exclusive() {
        let (_dir, mut app) = app();
        app.mark_selected(&[Change::SetReject(true)], "reject");
        assert_eq!((app.rows[0].flag, app.rows[0].reject), (false, true));

        app.mark_selected(&[Change::SetFlag(true)], "flag");
        assert_eq!(
            (app.rows[0].flag, app.rows[0].reject),
            (true, false),
            "flagging should clear the prior reject"
        );
    }

    #[test]
    fn clearing_marks_resets_both() {
        let (_dir, mut app) = app();
        app.mark_selected(&[Change::SetFlag(true)], "flag");
        assert!(app.rows[0].flag);

        app.mark_selected(&[Change::SetFlag(false), Change::SetReject(false)], "clear");
        assert_eq!((app.rows[0].flag, app.rows[0].reject), (false, false));
    }

    /// `mark_selected` goes through `self.targets()`, same as `rate_selected`
    /// -- a non-empty `multi_selected` means every selected stack, not just
    /// the cursor.
    #[test]
    fn mark_selected_applies_to_the_whole_batch() {
        let (_dir, mut app) = app();
        let ids: Vec<i64> = app.rows.iter().map(|r| r.id).collect();
        app.multi_selected = [ids[0], ids[1]].into_iter().collect();

        app.mark_selected(&[Change::SetFlag(true)], "flag");

        assert!(app.rows[0].flag);
        assert!(app.rows[1].flag);
        assert!(
            !app.rows[2].flag,
            "the unselected third stack must be untouched"
        );
    }

    /// Launching an external editor against a file that may not exist is
    /// exactly what `tui.rs`'s own `online` guard exists to refuse -- same
    /// message, same reasoning.
    #[test]
    fn run_action_refuses_while_offline() {
        let (_dir, mut app) = app();
        app.online = false;
        app.actions = vec![Action {
            key: "z".into(),
            name: "Test Editor".into(),
            cmd: vec!["true".into()],
            mode: action::Mode::Detach,
            shell: false,
        }];
        app.run_action('z');
        assert_eq!(
            app.status.as_deref(),
            Some("Test Editor not launched: the library is not mounted")
        );
    }

    #[test]
    fn the_trash_button_moves_the_batch_and_drops_it_from_view() {
        let (dir, mut app) = app();
        let day = dir.path().join("2026/08/09");
        let gone: Vec<i64> = app.rows.iter().take(2).map(|r| r.id).collect();
        let kept = app.rows[2].id;
        app.multi_selected = gone.iter().copied().collect();
        app.selected = 2;
        // Queued, not yet written: the sidecar has to be written before the
        // stack moves, or the edit would land beside nothing.
        app.rate_selected(Some(3));

        app.trash_selected();

        let trash_day = dir.path().join(".trash/2026/08/09");
        let left: Vec<PathBuf> = std::fs::read_dir(&day)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "NEF"))
            .collect();
        assert_eq!(left.len(), 1, "only the unselected stack stays: {left:?}");
        let moved = std::fs::read_dir(&trash_day).unwrap().count();
        assert_eq!(moved, 4, "two NEFs and their two sidecars");
        assert_eq!(app.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [kept]);
        assert!(app.multi_selected.is_empty());
        assert_eq!(app.selected, 0);
        assert!(
            app.status
                .as_deref()
                .unwrap()
                .starts_with("trashed 2 stack(s)"),
            "{:?}",
            app.status
        );
    }

    #[test]
    fn the_trash_button_refuses_while_offline() {
        let (dir, mut app) = app();
        app.online = false;
        let before = app.rows.len();
        app.trash_selected();
        assert_eq!(app.rows.len(), before);
        assert!(!dir.path().join(".trash").exists());
        assert_eq!(
            app.status.as_deref(),
            Some("trash not available: the library is not mounted")
        );
    }

    fn sidecar_of(app: &GuiApp, dir: &Path, id: i64) -> std::path::PathBuf {
        let base: String = app
            .cat
            .conn
            .query_row("SELECT base FROM stack WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        dir.join("2026/08/09").join(format!("{base}.xmp"))
    }

    /// A rating key is a catalog update only: the row changes in place, the
    /// sidecar waits for the idle writer, and the badge tracks the queue.
    #[test]
    fn rating_queues_the_sidecar_and_the_idle_writer_writes_it() {
        let (dir, mut app) = app();
        edit::flush(&mut app.cat, &Config::default()).unwrap();
        assert_eq!(app.write_state(), WriteState::Saved);

        let id = app.rows[0].id;
        let sidecar = sidecar_of(&app, dir.path(), id);
        let before = std::fs::read(&sidecar).ok();
        app.rate_selected(Some(4));
        assert_eq!(app.rows[0].rating, Some(4), "updated in place");
        assert_eq!(
            std::fs::read(&sidecar).ok(),
            before,
            "nothing written on the key"
        );
        assert_eq!(app.write_state(), WriteState::Waiting(1));

        app.writer.idle_after = std::time::Duration::ZERO;
        assert_eq!(app.write_state(), WriteState::Writing(1));
        assert!(app.writer.step(&mut app.cat, &Config::default()).unwrap());
        assert_eq!(app.write_state(), WriteState::Saved);
        assert_ne!(std::fs::read(&sidecar).ok(), before, "written once idle");
    }

    /// Rated under a rating filter, a photo stays in view with its new
    /// rating -- fast culling never shifts the selection under you -- until
    /// the filter or query changes.
    #[test]
    fn a_rated_photo_stays_in_view_until_the_filter_changes() {
        let (_dir, mut app) = app();
        app.rating_filter = [false, false, false, false, true];
        app.refresh_rows_after_rating_filter_change();
        assert_eq!(app.rows.len(), 1);

        app.rate_selected(Some(1));
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.rows[0].rating, Some(1));

        app.refresh_rows_after_rating_filter_change();
        assert!(app.rows.is_empty());
    }

    fn frame(app: &mut GuiApp, ctx: &egui::Context, close: bool) -> Vec<egui::ViewportCommand> {
        let mut input = egui::RawInput::default();
        if close {
            input.viewports.insert(
                egui::ViewportId::ROOT,
                egui::ViewportInfo {
                    events: vec![egui::ViewportEvent::Close],
                    ..Default::default()
                },
            );
        }
        let out = ctx.run_ui(input, |ui| app.drive_writer(ui.ctx()));
        out.viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.commands.clone())
            .unwrap_or_default()
    }

    /// Closing the window with sidecars still queued is held -- even mid
    /// rating, before the idle pause -- until they're all written; then the
    /// window closes itself.
    #[test]
    fn closing_waits_until_every_queued_sidecar_is_written() {
        let (_dir, mut app) = app();
        let ctx = egui::Context::default();
        app.rate_selected(Some(3));
        assert!(edit::queued(&app.cat).unwrap() > 0);

        let first = frame(&mut app, &ctx, true);
        assert!(first.contains(&egui::ViewportCommand::CancelClose));
        assert!(matches!(
            app.write_state(),
            WriteState::Closing(_) | WriteState::Saved
        ));

        let mut closed = false;
        for _ in 0..10 {
            if frame(&mut app, &ctx, false).contains(&egui::ViewportCommand::Close) {
                closed = true;
                break;
            }
        }
        assert!(closed, "the window never closed itself");
        assert_eq!(
            edit::queued(&app.cat).unwrap(),
            0,
            "closed with writes outstanding"
        );
    }

    /// Offline there's nothing to write to: the queue waits in the catalog
    /// and a close goes straight through.
    #[test]
    fn offline_a_close_is_not_held() {
        let (_dir, mut app) = app();
        let ctx = egui::Context::default();
        app.online = false;
        app.rate_selected(Some(3));
        assert!(matches!(app.write_state(), WriteState::Offline(n) if n > 0));
        let cmds = frame(&mut app, &ctx, true);
        assert!(!cmds.contains(&egui::ViewportCommand::CancelClose));
    }

    /// Export reads the original pixels, so it needs the drive exactly the
    /// same way an external editor does.
    #[test]
    fn run_export_refuses_while_offline() {
        let (_dir, mut app) = app();
        let out = tempfile::tempdir().unwrap();
        app.config.export_dir = Some(out.path().to_path_buf());
        app.online = false;
        app.run_export(0);
        assert_eq!(
            app.status.as_deref(),
            Some("export not available: the library is not mounted")
        );
    }

    /// A real JPEG at `path`, `w`x`h`, so `texture_for` has something to load.
    fn write_jpeg(path: &Path, w: u32, h: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::RgbImage::new(w, h).save(path).unwrap();
    }

    /// Stepping through photos must not keep every one it showed: least
    /// recently drawn go first, and what this frame drew stays even when it
    /// alone is over budget.
    #[test]
    fn textures_past_the_budget_are_dropped_oldest_first() {
        let (dir, mut app) = app();
        let ctx = egui::Context::default();
        let paths: Vec<PathBuf> = (0..3)
            .map(|i| dir.path().join(format!("t{i}.jpg")))
            .collect();
        for (frame, path) in paths.iter().enumerate() {
            write_jpeg(path, 100, 100); // 40,000 bytes as RGBA
            app.frame = frame as u64;
            assert!(app.texture_for(&ctx, path).is_some());
        }
        // Draw the oldest again: now the middle one is least recent.
        app.frame = 3;
        assert!(app.texture_for(&ctx, &paths[0]).is_some());

        app.evict_textures(80_000);
        assert!(app.textures.contains_key(&paths[0]), "redrawn this frame");
        assert!(!app.textures.contains_key(&paths[1]), "least recent stayed");
        assert!(app.textures.contains_key(&paths[2]));

        app.evict_textures(1);
        assert_eq!(
            app.textures.keys().collect::<Vec<_>>(),
            vec![&paths[0]],
            "this frame's texture must survive any budget"
        );
    }

    /// Single view readies the next photo while this one is looked at: its
    /// view image is queued for generation, and once cached, uploaded
    /// before the user steps to it -- one decode per frame at most.
    #[test]
    fn single_view_prepares_the_neighbours() {
        let (_dir, mut app) = app();
        let cache_dir = tempfile::tempdir().unwrap();
        app.cache = thumbs::Cache::in_dir(cache_dir.path().to_path_buf()).unwrap();
        let ctx = egui::Context::default();
        app.selected = 1;
        let view_path = |app: &GuiApp, i: usize| {
            let src = thumbs::best_source(&app.cat, app.rows[i].id)
                .unwrap()
                .unwrap();
            app.cache.path_for(&src, thumbs::Size::View)
        };
        let (prev, next) = (view_path(&app, 0), view_path(&app, 2));
        write_jpeg(&prev, 30, 20);
        write_jpeg(&next, 30, 20);

        app.prefetch_neighbours(&ctx, thumbs::Size::View);
        assert!(
            app.textures.contains_key(&next),
            "the next photo comes first"
        );
        assert!(!app.textures.contains_key(&prev), "one decode per frame");

        app.prefetch_neighbours(&ctx, thumbs::Size::View);
        assert!(app.textures.contains_key(&prev));
    }

    /// The eye follows what the watcher reports: drowsy until `Ready`,
    /// watching, cataloging while it rescans, then watching again; an error
    /// during setup or a vanished thread shows as failed.
    #[test]
    fn the_watch_eye_follows_the_watchers_events() {
        let (_dir, mut app) = app();
        let (tx, rx) = std::sync::mpsc::channel();
        app.watcher = Some(watch::Watcher::from_channel(rx));
        app.watch_state = WatchState::Starting;

        app.poll_watcher();
        assert_eq!(
            app.watch_state,
            WatchState::Starting,
            "nothing reported yet"
        );

        tx.send(watch::WatchEvent::Ready { dirs: 7 }).unwrap();
        app.poll_watcher();
        assert_eq!(app.watch_state, WatchState::Watching { dirs: 7 });

        let dir = PathBuf::from("/lib/export/bw");
        tx.send(watch::WatchEvent::Scanning(dir.clone())).unwrap();
        app.poll_watcher();
        assert_eq!(
            app.watch_state,
            WatchState::Scanning {
                dirs: 7,
                dir: dir.clone()
            }
        );

        tx.send(watch::WatchEvent::Rescanned(dir)).unwrap();
        app.poll_watcher();
        assert_eq!(app.watch_state, WatchState::Watching { dirs: 7 });

        // A runtime error is reported but doesn't stop the watching.
        tx.send(watch::WatchEvent::Error("one folder vanished".into()))
            .unwrap();
        app.poll_watcher();
        assert_eq!(app.watch_state, WatchState::Watching { dirs: 7 });

        drop(tx);
        app.poll_watcher();
        assert!(
            matches!(app.watch_state, WatchState::Failed(_)),
            "thread gone"
        );

        let (tx, rx) = std::sync::mpsc::channel();
        app.watcher = Some(watch::Watcher::from_channel(rx));
        app.watch_state = WatchState::Starting;
        tx.send(watch::WatchEvent::Error("watching /lib: denied".into()))
            .unwrap();
        app.poll_watcher();
        assert_eq!(
            app.watch_state,
            WatchState::Failed("watching /lib: denied".into())
        );
    }

    /// Every state of the eye draws, in both themes, at any time of the
    /// blink cycle, without panicking.
    #[test]
    fn the_watch_eye_draws_in_every_state() {
        let (_dir, mut app) = app();
        let ctx = egui::Context::default();
        let states = [
            WatchState::Off,
            WatchState::Starting,
            WatchState::Watching { dirs: 3 },
            WatchState::Scanning {
                dirs: 3,
                dir: PathBuf::from("/lib/export"),
            },
            WatchState::Failed("x".into()),
        ];
        for dark in [true, false] {
            ctx.set_visuals(if dark {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            });
            for state in &states {
                app.watch_state = state.clone();
                for time in [0.0, 0.05, 2.0] {
                    let input = egui::RawInput {
                        time: Some(time),
                        ..Default::default()
                    };
                    let _ = ctx.run_ui(input, |ui| app.draw_watch_eye(ui));
                }
            }
        }
    }
}
