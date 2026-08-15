//! A native GUI, built to answer one question: does showing thumbnails as
//! real GPU textures avoid the terminal-transmission cost the TUI pays on
//! every grid page-turn? (Measured: ~780ms of a ~950ms 27-tile page redraw
//! is `ratatui-image`'s Kitty encoder writing raw, uncompressed RGBA pixel
//! data to the terminal -- not npc's own decode or resize work.)
//!
//! Deliberately minimal: a query box and a thumbnail grid, nothing else.
//! Editing, the timeline, the sidebar and the single-image view all stay in
//! `tui.rs` for now. This is one slice, not a port.

// Tile-columns-per-row is a UI layout count derived from a pixel width; an
// available width beyond `usize::MAX` tiles, or negative, is not a case this
// window can be in. Same justification as the equivalent allow in thumbs.rs.
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use crate::db::Catalog;
use crate::{config::Config, query, thumbs, vol};
use anyhow::{anyhow, Result};
use eframe::egui;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const RESULT_LIMIT: usize = 200;
const TILE_PX: f32 = 160.0;

struct Row {
    id: i64,
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
        .query_map(params.as_slice(), |r| Ok(Row { id: r.get(0)? }))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

struct GuiApp {
    cat: Catalog,
    cache: thumbs::Cache,
    query: String,
    query_error: Option<String>,
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
    fn new(cat: Catalog, cache: thumbs::Cache) -> Self {
        let rows = run_query(&cat, "").unwrap_or_default();
        Self {
            cat,
            cache,
            query: String::new(),
            query_error: None,
            rows,
            selected: 0,
            textures: HashMap::new(),
            cols: 1,
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
                let resp = ui.text_edit_singleline(&mut self.query);
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
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
        });
    }

    fn handle_nav_keys(&mut self, ui: &egui::Ui) {
        let cols = self.cols.max(1);
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
        });
    }

    fn draw_grid(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            self.cols = ((ui.available_width() / (TILE_PX + 8.0)).floor() as usize).max(1);
            let ctx = ui.ctx().clone();
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for idx in 0..self.rows.len() {
                        let id = self.rows[idx].id;
                        let Ok(Some(src)) = thumbs::best_source(&self.cat, id) else {
                            continue;
                        };
                        let Ok(path) = self.cache.make_now(&src, thumbs::Size::Grid) else {
                            continue;
                        };
                        let Some(tex) = self.texture_for(&ctx, &path) else {
                            continue;
                        };
                        let btn = egui::Button::image(&tex).selected(idx == self.selected);
                        if ui.add_sized([TILE_PX, TILE_PX], btn).clicked() {
                            self.selected = idx;
                        }
                    }
                });
            });
        });
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw_query_bar(ui);
        self.handle_nav_keys(ui);
        self.draw_grid(ui);
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
        Box::new(|_cc| Ok(Box::new(GuiApp::new(cat, cache)))),
    )
    .map_err(|e| anyhow!("eframe: {e}"))
}
