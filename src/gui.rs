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

use crate::config::{Config, Theme};
use crate::db::Catalog;
use crate::edit::{self, Change};
use crate::exif;
use crate::{query, thumbs, vol};
use anyhow::{anyhow, Result};
use eframe::egui;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const RESULT_LIMIT: usize = 200;
const MIN_TILE_PX: f32 = 64.0;
const MAX_TILE_PX: f32 = 400.0;
const DEFAULT_TILE_PX: f32 = 160.0;

struct Row {
    id: i64,
    rating: Option<i64>,
}

fn run_query(cat: &Catalog, text: &str) -> Result<Vec<Row>> {
    let compiled = query::compile(text)?;
    let sql = compiled.select_stacks(Some(RESULT_LIMIT));
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
    events: Vec<(String, i64)>,
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
}

impl GuiApp {
    fn new(cat: Catalog, cache: thumbs::Cache, config: Config) -> Self {
        let rows = run_query(&cat, "").unwrap_or_default();
        let theme = config.gui.theme;
        let mut app = Self {
            cat,
            cache,
            config,
            query: String::new(),
            query_error: None,
            status: None,
            rows,
            selected: 0,
            textures: HashMap::new(),
            cols: 1,
            mode: Mode::Grid,
            applied_filter: None,
            events: Vec::new(),
            tags: Vec::new(),
            folders: Vec::new(),
            tile_px: DEFAULT_TILE_PX,
            theme,
        };
        app.reload_sidebar();
        app
    }

    /// Whole-library counts for the three sidebar sections, independent of
    /// the current query -- the same relationship `tui.rs`'s timeline/sidebar
    /// has to its own filtered result set. Nothing in this cut changes tag,
    /// event, or folder membership, so this only needs to run once at
    /// startup; a future tagging feature would call it again after an edit.
    fn reload_sidebar(&mut self) {
        self.events = self
            .cat
            .conn
            .prepare(
                "SELECT COALESCE(
                          NULLIF(e.name, ''),
                          date(e.start_time, 'unixepoch'),
                          'undated') AS d,
                        COUNT(s.id) AS n
                 FROM stack s LEFT JOIN event e ON e.id = s.event_id
                 GROUP BY COALESCE(e.id, -1)
                 ORDER BY (e.start_time IS NULL), e.start_time DESC",
            )
            .and_then(|mut st| {
                let rows =
                    st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
            })
            .unwrap_or_default();

        let flat_tags: Vec<(String, i64)> = self
            .cat
            .conn
            .prepare(
                "SELECT t.name, COUNT(st.stack_id) n FROM tag t
                 LEFT JOIN stack_tag st ON st.tag_id = t.id
                 GROUP BY t.id ORDER BY n DESC, t.name",
            )
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
            .prepare("SELECT dir, COUNT(*) n FROM stack GROUP BY dir ORDER BY dir")
            .and_then(|mut st| {
                let rows =
                    st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
            })
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
        match run_query(&self.cat, &self.query) {
            Ok(rows) => {
                self.rows = rows;
                self.selected = 0;
                self.query_error = None;
            }
            Err(e) => self.query_error = Some(e.to_string()),
        }
    }

    /// Set (or clear, for `None`) the star rating of the selected stack.
    /// Mirrors `tui.rs`'s rating keys: same `edit::apply` + `edit::flush`
    /// pair, then a full reload from the catalog rather than an optimistic
    /// local update, so the grid never shows a rating that failed to write.
    /// No undo stack here yet -- `tui.rs` has one; out of scope for this
    /// first cut.
    fn rate_selected(&mut self, n: Option<i64>) {
        let Some(id) = self.rows.get(self.selected).map(|r| r.id) else {
            return;
        };
        if let Err(e) = edit::apply(&mut self.cat, &[id], &[Change::SetRating(n)], &self.config) {
            self.status = Some(format!("rate failed: {e}"));
            return;
        }
        self.status = match edit::flush(&mut self.cat, &self.config) {
            Ok(stats) => Some(format!(
                "rated: {} sidecar(s) written",
                stats.sidecars_written
            )),
            Err(e) => Some(format!("rate failed: {e}")),
        };
        if let Ok(rows) = run_query(&self.cat, &self.query) {
            self.rows = rows;
        }
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
                ui.text_edit_singleline(&mut self.query);
                // Deliberately not gated on the text field's own focus state
                // (`response.lost_focus()`) -- that's the documented egui
                // idiom, but it didn't fire reliably here. There is exactly
                // one text field in this app, so an unconditional check on
                // Enter being pressed anywhere this frame is just as
                // correct and doesn't depend on focus-tracking behaving as
                // expected in this (very recent) egui release.
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    match run_query(&self.cat, &self.query) {
                        Ok(rows) => {
                            self.rows = rows;
                            self.selected = 0;
                            self.query_error = None;
                        }
                        Err(e) => self.query_error = Some(e.to_string()),
                    }
                }
                ui.label(format!("{} stack(s)", self.rows.len()));
            });
            ui.horizontal(|ui| {
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
            });
            if let Some(err) = &self.query_error {
                ui.colored_label(egui::Color32::RED, err);
            }
            if let Some(s) = &self.status {
                ui.label(s);
            }
        });
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
                        for (name, count) in &self.events {
                            if ui
                                .selectable_label(false, format!("{name} ({count})"))
                                .clicked()
                            {
                                clicked = Some(format!("event=\"{name}\""));
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

    /// EXIF, rating, and a read-only tag list for the selected photo. Reads
    /// `self.rows[self.selected]` -- the same field the grid, single view's
    /// prev/next, and sidebar-filter reload all share, so this always shows
    /// whatever is actually selected regardless of which of those changed it.
    fn draw_info_panel(&mut self, ui: &mut egui::Ui) {
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
                ui.label(tags.join(", "));
            }
        });
    }

    /// `0`-`5` rate the selected tile (`0` clears), matching `tui.rs`. Same
    /// keyboard-focus guard as `handle_nav_keys`, and for the same reason:
    /// digits are how you type a query like `rating>=3`. Works in both grid
    /// and single view, since both read/write the same `selected` index.
    fn handle_rating_keys(&mut self, ui: &egui::Ui) {
        const KEYS: [(egui::Key, Option<i64>); 6] = [
            (egui::Key::Num0, None),
            (egui::Key::Num1, Some(1)),
            (egui::Key::Num2, Some(2)),
            (egui::Key::Num3, Some(3)),
            (egui::Key::Num4, Some(4)),
            (egui::Key::Num5, Some(5)),
        ];
        if ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        let action = ui
            .ctx()
            .input(|i| KEYS.into_iter().find(|(k, _)| i.key_pressed(*k)))
            .map(|(_, n)| n);
        if let Some(n) = action {
            self.rate_selected(n);
        }
    }

    /// Arrow/Page keys move the grid selection or scroll -- but the query
    /// box is a text field, and Left/Right/digits are how you edit text in
    /// it. Without this guard, typing e.g. `rating>=3` into the query would
    /// also move the selection and rate the current tile. Checked once per
    /// frame and threaded through rather than re-checked per key, since it
    /// answers one question ("does some widget want the keyboard right
    /// now") that does not change mid-frame.
    fn handle_nav_keys(&mut self, ui: &egui::Ui) {
        if ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        let cols = self.cols.max(1);
        // ScrollArea only answers to the wheel/drag by default -- Page
        // Up/Down need an explicit scroll request.
        let mut scroll_by = 0.0f32;
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
            if i.key_pressed(egui::Key::PageDown) {
                scroll_by -= 3.0 * (self.tile_px + 8.0);
            }
            if i.key_pressed(egui::Key::PageUp) {
                scroll_by += 3.0 * (self.tile_px + 8.0);
            }
        });
        if scroll_by != 0.0 {
            ui.scroll_with_delta(egui::Vec2::new(0.0, scroll_by));
        }
    }

    /// Single view's own keys: Left/Right step through the same `rows`
    /// order the grid built, Escape returns to the grid without disturbing
    /// `selected`. Same keyboard-focus guard as the grid's nav keys.
    fn handle_single_keys(&mut self, ui: &egui::Ui) {
        if ui.ctx().egui_wants_keyboard_input() {
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
            // `ScrollArea` auto-shrinks to fit its content by default, and
            // `egui::Grid` sizes itself tightly to its tiles rather than the
            // full panel width -- so the scroll area's hit-test rect shrank
            // to match, and the wheel stopped registering over the margin
            // outside the tiles. Force it to fill the panel instead.
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // `horizontal_wrapped` is a flow layout (built for wrapping
                    // text and mixed-size content); it does not guarantee a
                    // shared row baseline for uniformly-sized items, and tiles
                    // came out vertically offset within a row. `egui::Grid`
                    // is an actual table layout -- every cell in a row shares
                    // the row's height by construction.
                    egui::Grid::new("thumbnail-grid")
                        .min_col_width(tile_px)
                        .min_row_height(tile_px)
                        .spacing([8.0, 8.0])
                        .show(ui, |ui| {
                            let mut open_single = false;
                            for idx in 0..self.rows.len() {
                                let id = self.rows[idx].id;
                                let rating = self.rows[idx].rating;
                                if idx > 0 && idx % cols == 0 {
                                    ui.end_row();
                                }
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
                                    // `vertical()` otherwise claims the
                                    // full remaining width of the row as
                                    // this cell's width (it has no content
                                    // yet to size itself by), and Grid
                                    // measures column width from that --
                                    // not from the tile-sized button inside
                                    // it. That's the huge dead gap after the
                                    // first tile: column 0 was recorded as
                                    // almost the whole window's width.
                                    ui.set_width(tile_px);
                                    let resp = if let Some(tex) = tex {
                                        let btn = egui::Button::image(&tex)
                                            .selected(idx == self.selected);
                                        ui.add_sized([tile_px, tile_px], btn)
                                    } else {
                                        let btn =
                                            egui::Button::new("...").selected(idx == self.selected);
                                        ui.add_sized([tile_px, tile_px], btn)
                                    };
                                    if resp.double_clicked() {
                                        self.selected = idx;
                                        open_single = true;
                                    } else if resp.clicked() {
                                        self.selected = idx;
                                    }
                                    let stars =
                                        "*".repeat(rating.unwrap_or(0).clamp(0, 5) as usize);
                                    ui.label(stars);
                                });
                            }
                            if open_single {
                                self.mode = Mode::Single;
                            }
                        });
                });
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
            // `Size::Full` is a `min_dim` past any real sensor's long edge,
            // so `extract_preview` can never satisfy it from a smaller
            // preview tier and falls back to the largest one available --
            // the same "asking for more forces full resolution" behavior
            // `tui.rs`'s single view already relies on for its own zoom.
            match self
                .cache
                .get(&src, thumbs::Size::Full)
                .and_then(|path| self.texture_for(&ctx, &path))
            {
                Some(tex) => {
                    let avail = ui.available_size();
                    ui.add(egui::Image::new(&tex).max_size(avail).shrink_to_fit());
                }
                None => {
                    ui.label("loading full-resolution image...");
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
}

fn visuals_for(theme: Theme) -> egui::Visuals {
    match theme {
        Theme::Dark => egui::Visuals::dark(),
        Theme::Light => egui::Visuals::light(),
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw_query_bar(ui);
        self.draw_sidebar(ui);
        self.draw_info_panel(ui);
        match self.mode {
            Mode::Grid => {
                self.handle_nav_keys(ui);
                self.handle_rating_keys(ui);
                self.draw_grid(ui);
            }
            Mode::Single => {
                self.handle_single_keys(ui);
                self.handle_rating_keys(ui);
                self.draw_single(ui);
            }
        }
        self.poll_thumbnail_generation(ui.ctx());
    }
}

/// Resolve config, open the catalog, and launch the window. Mirrors
/// `tui::run`'s startup order.
pub fn run(root: &Path) -> Result<()> {
    let config = Config::load().unwrap_or_default();
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

    eframe::run_native(
        "npcg",
        eframe::NativeOptions::default(),
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(visuals_for(theme));
            Ok(Box::new(GuiApp::new(cat, cache, config)))
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
}
