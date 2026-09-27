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
use crate::tui::{compute_timeline, Granularity, TimelineBucket, TrashView};
use crate::watch;
use crate::{query, thumbs, vol};
use anyhow::{anyhow, Result};
use eframe::egui;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MIN_TILE_PX: f32 = 64.0;
const MAX_TILE_PX: f32 = 1000.0;
const DEFAULT_TILE_PX: f32 = 160.0;

struct Row {
    id: i64,
    rating: Option<i64>,
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
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
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

struct GuiApp {
    cat: Catalog,
    cache: thumbs::Cache,
    config: Config,
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
    /// Cache-file path -> uploaded GPU texture. Unlike the TUI's
    /// `decoded`/`images`/`last_rect` three-way split (needed because a
    /// kitty placement is bound to a screen rect and must be rebuilt
    /// whenever a tile's position changes), a `TextureHandle` is retained
    /// GPU state with no such binding -- the same handle draws at a new
    /// screen position for free, any frame, forever. One flat cache is
    /// enough. `Size::Grid` and `Size::Full` textures for the same source
    /// coexist here under their own distinct cache-file paths -- no key
    /// collision, since `thumbs::Cache` keys by size too.
    textures: HashMap<PathBuf, egui::TextureHandle>,
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
    /// This session's theme; toggling flips it and re-applies
    /// `egui::Visuals` immediately, but never writes back to config.toml --
    /// nothing in this tool writes its own config file, and the user chose
    /// session-only over persisting the toggle.
    theme: Theme,
    /// The info panel's "add tag" text field. Not cleared automatically on
    /// selection change -- typing a tag, then clicking a different tile
    /// before pressing Enter, keeps the draft rather than silently losing it.
    tag_input: String,
    /// Background library watcher (see `watch.rs`). `None` when disabled in
    /// config or failed to start -- either way, just "no automatic
    /// cataloging this session," never fatal to launching the GUI.
    watcher: Option<watch::Watcher>,
    /// The `a` menu, ported from `tui.rs`: configured external programs
    /// (RawTherapee, GIMP, ...) launched against the current selection.
    /// Same `action::load()`/`action::gather()`/`action::run()` the TUI
    /// uses, so a program configured once works from either front end.
    actions: Vec<Action>,
    show_actions: bool,
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
    ) -> Self {
        let rows = run_query(&cat, "", [false; 5]).unwrap_or_default();
        let theme = config.gui.theme;
        let mut app = Self {
            cat,
            cache,
            config,
            query: String::new(),
            query_error: None,
            status: None,
            watcher,
            rows,
            selected: 0,
            multi_selected: std::collections::HashSet::new(),
            textures: HashMap::new(),
            cols: 1,
            mode: Mode::Grid,
            applied_filter: None,
            rating_filter: [false; 5],
            events: Vec::new(),
            tags: Vec::new(),
            folders: Vec::new(),
            tile_px: DEFAULT_TILE_PX,
            theme,
            tag_input: String::new(),
            actions: action::load(),
            show_actions: false,
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
        self.status = Some(match action::run(&a, &sel) {
            Ok(outcome) => outcome.description,
            Err(e) => format!("{}: {e}", a.name),
        });
    }

    /// Set (or clear, for `None`) the star rating of every target. Mirrors
    /// `tui.rs`'s rating keys: same `edit::apply` + `edit::flush` pair, then
    /// a full reload from the catalog rather than an optimistic local
    /// update, so the grid never shows a rating that failed to write. No
    /// undo stack here yet -- `tui.rs` has one; out of scope for this cut.
    fn rate_selected(&mut self, n: Option<i64>) {
        let ids = self.targets();
        if ids.is_empty() {
            return;
        }
        if let Err(e) = edit::apply(&mut self.cat, &ids, &[Change::SetRating(n)], &self.config) {
            self.status = Some(format!("rate failed: {e}"));
            return;
        }
        self.status = match edit::flush(&mut self.cat, &self.config) {
            Ok(stats) => Some(format!(
                "rated {} stack(s): {} sidecar(s) written",
                ids.len(),
                stats.sidecars_written
            )),
            Err(e) => Some(format!("rate failed: {e}")),
        };
        if let Ok(rows) = run_query(&self.cat, &self.query, self.rating_filter) {
            self.rows = rows;
        }
    }

    /// `Ctrl+1`..`Ctrl+5` flips the same array the checkboxes edit directly
    /// -- shared with their `.changed()` handler in `draw_query_bar` so a
    /// keypress and a click can never disagree about what happens next.
    fn toggle_rating_filter(&mut self, n: usize) {
        self.rating_filter[n - 1] = !self.rating_filter[n - 1];
        self.refresh_rows_after_rating_filter_change();
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
        self.status = match edit::flush(&mut self.cat, &self.config) {
            Ok(stats) => Some(format!(
                "tagged: {} sidecar(s) written",
                stats.sidecars_written
            )),
            Err(e) => Some(format!("tag failed: {e}")),
        };
        self.reload_sidebar();
    }

    fn texture_for(&mut self, ctx: &egui::Context, path: &Path) -> Option<egui::TextureHandle> {
        if let Some(t) = self.textures.get(path) {
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
        self.textures.insert(path.to_path_buf(), tex.clone());
        Some(tex)
    }

    fn draw_query_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("query").show(ui, |ui| {
            ui.horizontal(|ui| {
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
        egui::Panel::right("info").show(ui, |ui| {
            let Some(id) = id else {
                ui.label("no photo selected");
                return;
            };
            ui.heading("Info");
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
        });
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
        } else if ui.ctx().input(|i| i.key_pressed(egui::Key::A)) {
            self.show_actions = true;
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

    /// Arrow/Page keys move the grid selection or scroll, and Space toggles
    /// the tile under the cursor into or out of the batch selection (the
    /// keyboard equivalent of Ctrl/Cmd-click in the grid) -- but the query
    /// box is a text field, and Left/Right/digits are how you edit text in
    /// it. Without this guard, typing e.g. `rating>=3` into the query would
    /// also move the selection and rate the current tile.
    fn handle_nav_keys(&mut self, ui: &egui::Ui) {
        if ui.ctx().text_edit_focused() {
            return;
        }
        let cols = self.cols.max(1);
        // ScrollArea only answers to the wheel/drag by default -- Page
        // Up/Down need an explicit scroll request.
        let mut scroll_by = 0.0f32;
        let mut toggle_current = false;
        ui.ctx().input(|i| {
            if i.key_pressed(egui::Key::ArrowRight) && self.selected + 1 < self.rows.len() {
                self.selected += 1;
            }
            if i.key_pressed(egui::Key::ArrowLeft) {
                self.selected = self.selected.saturating_sub(1);
            }
            if i.key_pressed(egui::Key::ArrowDown) {
                self.selected = (self.selected + cols).min(self.rows.len().saturating_sub(1));
            }
            if i.key_pressed(egui::Key::ArrowUp) {
                self.selected = self.selected.saturating_sub(cols);
            }
            if i.key_pressed(egui::Key::Enter) && !self.rows.is_empty() {
                self.mode = Mode::Single;
            }
            if i.key_pressed(egui::Key::Space) {
                toggle_current = true;
            }
            if i.key_pressed(egui::Key::PageDown) {
                scroll_by -= 3.0 * (self.tile_px + 8.0);
            }
            if i.key_pressed(egui::Key::PageUp) {
                scroll_by += 3.0 * (self.tile_px + 8.0);
            }
        });
        if toggle_current {
            if let Some(id) = self.rows.get(self.selected).map(|r| r.id) {
                if !self.multi_selected.remove(&id) {
                    self.multi_selected.insert(id);
                }
            }
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
            egui::ScrollArea::vertical().show_rows(ui, row_height, grid_rows, |ui, visible| {
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
                                    self.multi_selected.clear();
                                    open_single = true;
                                } else if resp.clicked() {
                                    // Ctrl/Cmd-click toggles this tile into
                                    // or out of the batch selection without
                                    // disturbing the rest of it -- a plain
                                    // click starts a fresh single selection,
                                    // the common file-manager convention.
                                    let extend =
                                        ui.input(|inp| inp.modifiers.ctrl || inp.modifiers.command);
                                    if extend {
                                        if !self.multi_selected.remove(&id) {
                                            self.multi_selected.insert(id);
                                        }
                                    } else {
                                        self.multi_selected.clear();
                                    }
                                    self.selected = i;
                                }
                                let stars = "*".repeat(rating.unwrap_or(0).clamp(0, 5) as usize);
                                ui.label(stars);
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
            // `Size::View`, not `Size::Full` -- matching `tui.rs`'s own
            // *default* single view. `Full` is a `min_dim` past any real
            // sensor's long edge, so it forces the largest preview tier
            // available (measured elsewhere in this project at ~140ms/frame,
            // vs. `View`'s ~24ms) -- worth paying when the TUI's `+` key
            // explicitly asks to zoom in further, not on every single-view
            // open. This GUI has no such zoom escalation (yet), so it should
            // default to the tier the TUI itself defaults to, not the one it
            // reserves for an explicit zoom request.
            match self
                .cache
                .get(&src, thumbs::Size::View)
                .and_then(|path| self.texture_for(&ctx, &path))
            {
                Some(tex) => {
                    let avail = ui.available_size();
                    ui.add(egui::Image::new(&tex).max_size(avail).shrink_to_fit());
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
                Ok(watch::WatchEvent::Rescanned(_)) => rescanned = true,
                Ok(watch::WatchEvent::Error(e)) => {
                    self.status = Some(format!("watch: {e}"));
                }
                Err(_) => break,
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

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_watcher();
        self.draw_query_bar(ui);
        self.draw_sidebar(ui);
        self.draw_info_panel(ui);
        self.handle_action_keys(ui);
        match self.mode {
            Mode::Grid => {
                // While the actions menu is open, every other key is its
                // input (see `handle_action_keys`) -- same early-exit
                // `tui.rs`'s `on_key` does for `show_actions`, so e.g. its
                // digit keys can't also rate the tile underneath it.
                if !self.show_actions {
                    self.handle_nav_keys(ui);
                    self.handle_rating_keys(ui);
                }
                self.draw_grid(ui);
            }
            Mode::Single => {
                if !self.show_actions {
                    self.handle_single_keys(ui);
                    self.handle_rating_keys(ui);
                }
                self.draw_single(ui);
            }
        }
        self.draw_actions_menu(ui);
        self.poll_thumbnail_generation(ui.ctx());
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

    let (cat, online) = Catalog::open_for_root(root)?;
    if !online {
        return Err(anyhow!(
            "{} is not mounted; the GUI needs the files themselves",
            root.display()
        ));
    }

    let v = vol::identify(root)?;
    let cache = thumbs::Cache::new(&v.uuid)?;
    let theme = config.gui.theme;
    // Already confirmed online above; a failure here (disabled in config,
    // or the watcher itself erroring) is never fatal to launching the GUI.
    let watcher = watch::Watcher::spawn(root.to_path_buf(), &config).unwrap_or(None);

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
            // icon (unlike X11's _NET_WM_ICON). GNOME/Mutter looks the
            // icon up via this app_id, matched against a `.desktop`
            // file's own name -- see that file's Icon= key for the actual
            // installed icon.
            .with_app_id("npcg"),
        ..Default::default()
    };

    eframe::run_native(
        "npcg",
        native_options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(visuals_for(theme));
            Ok(Box::new(GuiApp::new(cat, cache, config, watcher)))
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
        let app = GuiApp::new(cat, cache, Config::default(), None);
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
}
