//! A native GUI, built to answer one question: does showing thumbnails as
//! real GPU textures avoid the terminal-transmission cost the TUI pays on
//! every grid page-turn? (Measured: ~780ms of a ~950ms 27-tile page redraw
//! is `ratatui-image`'s Kitty encoder writing raw, uncompressed RGBA pixel
//! data to the terminal -- not npc's own decode or resize work.)
//!
//! Deliberately minimal: a query box, a thumbnail grid, and rating. Tagging,
//! the timeline, the sidebar and the single-image view all stay in `tui.rs`
//! for now. This is one slice, not a port.

// Tile-columns-per-row is a UI layout count derived from a pixel width; an
// available width beyond `usize::MAX` tiles, or negative, is not a case this
// window can be in. Same justification as the equivalent allow in thumbs.rs.
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use crate::db::Catalog;
use crate::edit::{self, Change};
use crate::{config::Config, query, thumbs, vol};
use anyhow::{anyhow, Result};
use eframe::egui;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const RESULT_LIMIT: usize = 200;
const TILE_PX: f32 = 160.0;

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
    /// enough.
    textures: HashMap<PathBuf, egui::TextureHandle>,
    /// Tiles per row, recorded during the previous frame's layout and used
    /// by arrow-key row-jumping -- the same "measured during draw, consumed
    /// next frame" pattern as `tui.rs`'s `page_size`.
    cols: usize,
}

impl GuiApp {
    fn new(cat: Catalog, cache: thumbs::Cache, config: Config) -> Self {
        let rows = run_query(&cat, "").unwrap_or_default();
        Self {
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
            if let Some(err) = &self.query_error {
                ui.colored_label(egui::Color32::RED, err);
            }
            if let Some(s) = &self.status {
                ui.label(s);
            }
        });
    }

    /// `0`-`5` rate the selected tile (`0` clears), matching `tui.rs`. Same
    /// keyboard-focus guard as `handle_nav_keys`, and for the same reason:
    /// digits are how you type a query like `rating>=3`.
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
            if i.key_pressed(egui::Key::PageDown) {
                scroll_by -= 3.0 * (TILE_PX + 8.0);
            }
            if i.key_pressed(egui::Key::PageUp) {
                scroll_by += 3.0 * (TILE_PX + 8.0);
            }
        });
        if scroll_by != 0.0 {
            ui.scroll_with_delta(egui::Vec2::new(0.0, scroll_by));
        }
    }

    fn draw_grid(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            self.cols = ((ui.available_width() / (TILE_PX + 8.0)).floor() as usize).max(1);
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
                        .min_col_width(TILE_PX)
                        .min_row_height(TILE_PX)
                        .spacing([8.0, 8.0])
                        .show(ui, |ui| {
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
                                    // not from the 160px button inside it.
                                    // That's the huge dead gap after the
                                    // first tile: column 0 was recorded as
                                    // almost the whole window's width.
                                    ui.set_width(TILE_PX);
                                    let resp = if let Some(tex) = tex {
                                        let btn = egui::Button::image(&tex)
                                            .selected(idx == self.selected);
                                        ui.add_sized([TILE_PX, TILE_PX], btn)
                                    } else {
                                        let btn =
                                            egui::Button::new("...").selected(idx == self.selected);
                                        ui.add_sized([TILE_PX, TILE_PX], btn)
                                    };
                                    if resp.clicked() {
                                        self.selected = idx;
                                    }
                                    let stars =
                                        "*".repeat(rating.unwrap_or(0).clamp(0, 5) as usize);
                                    ui.label(stars);
                                });
                            }
                        });
                });
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

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw_query_bar(ui);
        self.handle_nav_keys(ui);
        self.handle_rating_keys(ui);
        self.draw_grid(ui);
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

    eframe::run_native(
        "npcg",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(GuiApp::new(cat, cache, config)))),
    )
    .map_err(|e| anyhow!("eframe: {e}"))
}
