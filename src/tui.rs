//! The browser.
//!
//! Three panes: a sidebar that switches between tags and dates, the stack
//! list, and a query/status bar. Tagging and rating keys are live at all
//! times, because the point of this screen is culling a shoot quickly.
//!
//! Edits go through the same `edit::apply` + `edit::flush` path as the CLI, so
//! everything the invariant guarantees on the command line holds here too.
//! An in-session undo log means a mis-keyed bulk tag is reversible before it
//! reaches the drive.

use crate::action::{self, Action};
use crate::config::Config;
use crate::db::Catalog;
use crate::edit::{self, Change};
use crate::query;
use crate::thumbs::{self, Size as ThumbSize};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui_image::picker::Picker;
use ratatui_image::StatefulImage;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

const PAGE: usize = 500;

#[derive(Debug, Clone)]
struct Row {
    id: i64,
    dir: String,
    base: String,
    rating: Option<i64>,
    flag: bool,
    reject: bool,
    captured: Option<i64>,
    roles: String,
    tags: String,
}

/// How the stacks are shown. Tagging and rating keys stay live in all three.
#[derive(Debug, PartialEq, Clone, Copy)]
enum View {
    List,
    Grid,
    Single,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Sidebar {
    Tags,
    Dates,
}

#[derive(PartialEq)]
enum Input {
    None,
    Query,
    AddTag,
    RemoveTag,
    Title,
}

/// One reversible batch. Undo restores the exact prior state of each stack
/// rather than guessing an inverse, so it is correct for partial overlaps
/// (tagging things that were already tagged, say).
struct UndoStep {
    label: String,
    prior: Vec<(i64, Vec<String>, Option<i64>, bool, bool)>,
}

pub struct App {
    cat: Catalog,
    config: Config,
    actions: Vec<Action>,
    root: PathBuf,

    query: String,
    rows: Vec<Row>,
    total: usize,
    offset: usize,
    cursor: usize,

    selected: HashSet<i64>,
    anchor: Option<usize>,

    sidebar: Sidebar,
    sidebar_items: Vec<(String, i64)>,
    sidebar_cursor: usize,
    sidebar_focus: bool,

    input: Input,
    buffer: String,
    status: String,
    show_help: bool,
    show_actions: bool,
    undo: Vec<UndoStep>,
    quit: bool,

    view: View,
    cache: Option<thumbs::Cache>,
    /// Decoded images ready to draw, keyed by cache-file path. Bounded so a
    /// long scroll cannot grow without limit.
    images: HashMap<PathBuf, ratatui_image::protocol::StatefulProtocol>,
    picker: Option<Picker>,
    /// Which member of the stack the single view is showing.
    member: usize,
    /// Tiles across in grid view. Adjustable live, because the right value
    /// depends on font size and window, which only the user can see.
    grid_cols: u16,
}

impl App {
    pub fn new(root: PathBuf) -> Result<Self> {
        let v = crate::vol::identify(&root)?;
        let cat = Catalog::open_for(&v.uuid, &root)?;
        Self::with_catalog(cat, root)
    }

    /// Split out so the interaction logic can be exercised without a terminal.
    pub fn with_catalog(cat: Catalog, root: PathBuf) -> Result<Self> {
        let mut app = Self {
            cat,
            config: {
                let c = Config::load()?;
                thumbs::configure(&c.thumbs);
                c
            },
            actions: action::load(),
            root,
            query: String::new(),
            rows: Vec::new(),
            total: 0,
            offset: 0,
            cursor: 0,
            selected: HashSet::new(),
            anchor: None,
            sidebar: Sidebar::Tags,
            sidebar_items: Vec::new(),
            sidebar_cursor: 0,
            sidebar_focus: false,
            input: Input::None,
            buffer: String::new(),
            status: String::new(),
            show_help: false,
            show_actions: false,
            undo: Vec::new(),
            quit: false,
            view: View::List,
            cache: None,
            images: HashMap::new(),
            picker: None,
            member: 0,
            grid_cols: 5,
        };
        app.reload()?;
        app.status = format!("{} stacks. ? for help.", app.total);
        Ok(app)
    }

    // --- data ------------------------------------------------------------

    fn reload(&mut self) -> Result<()> {
        let compiled = query::compile(&self.query)?;

        self.total = {
            let sql = format!(
                "SELECT COUNT(*) FROM stack s WHERE {}",
                compiled.where_sql
            );
            let params: Vec<&dyn rusqlite::ToSql> = compiled
                .params
                .iter()
                .map(|p| p as &dyn rusqlite::ToSql)
                .collect();
            self.cat
                .conn
                .query_row(&sql, params.as_slice(), |r| r.get::<_, i64>(0))? as usize
        };

        // Only the visible page is materialised, so this stays instant whether
        // the library holds 2k stacks or 500k.
        let sql = format!(
            "{} LIMIT {PAGE} OFFSET {}",
            compiled.select_stacks(None),
            self.offset
        );
        self.rows = {
            let mut stmt = self.cat.conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = compiled
                .params
                .iter()
                .map(|p| p as &dyn rusqlite::ToSql)
                .collect();
            let rows = stmt.query_map(params.as_slice(), |r| {
                Ok(Row {
                    id: r.get(0)?,
                    dir: r.get(1)?,
                    base: r.get(2)?,
                    rating: r.get(3)?,
                    flag: r.get::<_, i64>(4)? == 1,
                    reject: r.get::<_, i64>(5)? == 1,
                    captured: r.get(6)?,
                    roles: String::new(),
                    tags: String::new(),
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        for row in &mut self.rows {
            row.roles = self
                .cat
                .conn
                .prepare(
                    "SELECT DISTINCT ext FROM file WHERE stack_id = ?1 AND present = 1
                     AND role IN ('raw','oocjpg') ORDER BY role",
                )?
                .query_map([row.id], |r| r.get::<_, String>(0))?
                .filter_map(Result::ok)
                .collect::<Vec<_>>()
                .join(" ");
            row.tags = self
                .cat
                .conn
                .prepare(
                    "SELECT t.name FROM tag t JOIN stack_tag st ON st.tag_id = t.id
                     WHERE st.stack_id = ?1 ORDER BY t.name",
                )?
                .query_map([row.id], |r| r.get::<_, String>(0))?
                .filter_map(Result::ok)
                .collect::<Vec<_>>()
                .join(", ");
        }

        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
        self.reload_sidebar()?;
        Ok(())
    }

    fn reload_sidebar(&mut self) -> Result<()> {
        self.sidebar_items = match self.sidebar {
            Sidebar::Tags => self
                .cat
                .conn
                .prepare(
                    "SELECT t.name, COUNT(st.stack_id) n FROM tag t
                     LEFT JOIN stack_tag st ON st.tag_id = t.id
                     GROUP BY t.id ORDER BY n DESC, t.name",
                )?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .filter_map(Result::ok)
                .collect(),
            // Stacks with no capture time get an explicit bucket rather than
            // vanishing from this pane -- undated shots are exactly the ones
            // you need to find in order to fix them.
            Sidebar::Dates => self
                .cat
                .conn
                .prepare(
                    "SELECT COALESCE(
                              strftime('%Y-%m-%d',
                                (SELECT MIN(f.capture_time) FROM file f
                                  WHERE f.stack_id = s.id AND f.capture_time IS NOT NULL),
                                'unixepoch'),
                              'undated') AS d,
                            COUNT(*) AS n
                     FROM stack s
                     GROUP BY d ORDER BY (d = 'undated'), d DESC",
                )?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .filter_map(Result::ok)
                .collect(),
        };
        if self.sidebar_cursor >= self.sidebar_items.len() {
            self.sidebar_cursor = 0;
        }
        Ok(())
    }

    /// Attach the thumbnail cache and terminal graphics protocol. Only done
    /// when a real terminal is present, so headless tests stay unaffected.
    pub fn enable_images(&mut self) {
        if let Ok(v) = crate::vol::identify(&self.root) {
            self.cache = thumbs::Cache::new(&v.uuid).ok();
        }
        self.picker = Picker::from_query_stdio().ok();
        // Say plainly what was detected. A silent fallback to halfblocks looks
        // like broken thumbnails rather than an unsupported terminal.
        self.status = match self.picker.as_ref() {
            Some(p) => {
                let (cw, ch) = p.font_size();
                match p.protocol_type() {
                    ratatui_image::picker::ProtocolType::Halfblocks => format!(
                        "no graphics protocol - falling back to halfblocks ({cw}x{ch}px cells). \
                         Photos will not be identifiable. Run `photo term` to diagnose."
                    ),
                    other => format!("{other:?} graphics, {cw}x{ch}px cells. ? for help."),
                }
            }
            None => "terminal graphics query failed; thumbnails unavailable".into(),
        };
    }

    /// Cached thumbnail for a row, decoded and ready to draw. Returns `None`
    /// while generation is still in flight -- the UI shows a placeholder and
    /// picks it up on a later frame rather than blocking.
    fn image_for(&mut self, row_index: usize, size: ThumbSize) -> Option<PathBuf> {
        let id = self.rows.get(row_index)?.id;
        let cache = self.cache.as_ref()?;
        let src = thumbs::best_source(&self.cat, id).ok().flatten()?;
        cache.get(&src, size)
    }

    fn ensure_decoded(&mut self, path: &PathBuf) -> bool {
        if self.images.contains_key(path) {
            return true;
        }
        let Some(picker) = self.picker.as_mut() else {
            return false;
        };
        let Ok(img) = image::open(path) else {
            return false;
        };
        let protocol = picker.new_resize_protocol(img);
        // Bounded: a long scroll must not grow this without limit.
        if self.images.len() > 120 {
            self.images.clear();
        }
        self.images.insert(path.clone(), protocol);
        true
    }

    /// Queue thumbnails around the cursor so scrolling finds them ready.
    fn prefetch(&mut self, size: ThumbSize, span: usize) {
        let start = self.cursor.saturating_sub(span);
        let end = (self.cursor + span).min(self.rows.len().saturating_sub(1));
        for i in start..=end {
            let _ = self.image_for(i, size);
        }
    }

    /// What is under the cursor, as a path relative to the library root plus
    /// the actual filename. In grid view the tile shows only stars and marks,
    /// so this is the only place the identity of a shot appears.
    fn current_label(&self) -> Option<String> {
        let row = self.rows.get(self.cursor)?;
        let members = self.current_members();
        let filename = members
            .get(self.member.min(members.len().saturating_sub(1)))
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| row.base.clone());

        let dir = std::path::Path::new(&row.dir)
            .strip_prefix(&self.root)
            .unwrap_or_else(|_| std::path::Path::new(&row.dir))
            .to_string_lossy()
            .to_string();

        Some(if dir.is_empty() {
            filename
        } else {
            format!("{dir}/{filename}")
        })
    }

    /// Files of the stack under the cursor, for the single view's `o` cycling.
    fn current_members(&self) -> Vec<(String, String)> {
        let Some(row) = self.rows.get(self.cursor) else {
            return Vec::new();
        };
        self.cat
            .conn
            .prepare(
                "SELECT name, role FROM file WHERE stack_id = ?1 AND present = 1
                 ORDER BY CASE role WHEN 'raw' THEN 0 WHEN 'oocjpg' THEN 1 ELSE 2 END, name",
            )
            .and_then(|mut st| {
                let rows = st.query_map([row.id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .unwrap_or_default()
    }

    // --- editing ---------------------------------------------------------

    /// Stacks an edit applies to: the multi-selection if there is one, else
    /// whatever is under the cursor.
    fn targets(&self) -> Vec<i64> {
        if !self.selected.is_empty() {
            return self.selected.iter().copied().collect();
        }
        self.rows.get(self.cursor).map(|r| r.id).into_iter().collect()
    }

    fn snapshot(&self, ids: &[i64]) -> Result<Vec<(i64, Vec<String>, Option<i64>, bool, bool)>> {
        let mut out = Vec::new();
        for &id in ids {
            let tags: Vec<String> = self
                .cat
                .conn
                .prepare(
                    "SELECT t.name FROM tag t JOIN stack_tag st ON st.tag_id = t.id
                     WHERE st.stack_id = ?1",
                )?
                .query_map([id], |r| r.get::<_, String>(0))?
                .filter_map(Result::ok)
                .collect();
            let (rating, flag, reject): (Option<i64>, i64, i64) = self.cat.conn.query_row(
                "SELECT rating, flag, reject FROM stack WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            out.push((id, tags, rating, flag == 1, reject == 1));
        }
        Ok(out)
    }

    fn edit(&mut self, changes: Vec<Change>, label: &str) -> Result<()> {
        let ids = self.targets();
        if ids.is_empty() {
            self.status = "nothing selected".into();
            return Ok(());
        }
        let prior = self.snapshot(&ids)?;

        match edit::apply(&mut self.cat, &ids, &changes, &self.config) {
            Ok(_) => {}
            Err(e) => {
                self.status = format!("refused: {e}");
                return Ok(());
            }
        }
        let stats = edit::flush(&mut self.cat, &self.config)?;

        self.undo.push(UndoStep {
            label: label.to_string(),
            prior,
        });
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }

        self.status = if stats.errors.is_empty() {
            format!(
                "{label}: {} stack(s), {} sidecar(s) written",
                ids.len(),
                stats.sidecars_written
            )
        } else {
            format!("{label}: {}", stats.errors[0])
        };
        self.reload()
    }

    fn undo(&mut self) -> Result<()> {
        let Some(step) = self.undo.pop() else {
            self.status = "nothing to undo".into();
            return Ok(());
        };
        for (id, tags, rating, flag, reject) in &step.prior {
            let changes = vec![
                Change::ClearTags,
                Change::AddTags(tags.clone()),
                Change::SetRating(*rating),
                Change::SetFlag(*flag),
                Change::SetReject(*reject),
            ];
            edit::apply(&mut self.cat, &[*id], &changes, &self.config)?;
        }
        edit::flush(&mut self.cat, &self.config)?;
        self.status = format!("undid: {}", step.label);
        self.reload()
    }

    // --- input -----------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.input != Input::None {
            return self.on_input_key(key);
        }
        if self.show_help || self.show_actions {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    self.show_help = false;
                    self.show_actions = false;
                }
                KeyCode::Char(c) if self.show_actions => {
                    self.show_actions = false;
                    self.run_action(c)?;
                }
                _ => {}
            }
            return Ok(());
        }

        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('a') => self.show_actions = true,

            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_cursor(20)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_cursor(-20)
            }
            KeyCode::Char('g') => self.cursor = 0,
            KeyCode::Char('G') => self.cursor = self.rows.len().saturating_sub(1),

            KeyCode::Char(' ') => {
                if let Some(r) = self.rows.get(self.cursor) {
                    if !self.selected.insert(r.id) {
                        self.selected.remove(&r.id);
                    }
                    self.move_cursor(1);
                }
            }
            KeyCode::Char('v') => {
                match self.anchor.take() {
                    Some(start) => {
                        let (a, b) = if start <= self.cursor {
                            (start, self.cursor)
                        } else {
                            (self.cursor, start)
                        };
                        for r in &self.rows[a..=b] {
                            self.selected.insert(r.id);
                        }
                        self.status = format!("{} selected", self.selected.len());
                    }
                    None => {
                        self.anchor = Some(self.cursor);
                        self.status = "range start; press v again to close".into();
                    }
                }
            }
            KeyCode::Esc => {
                self.selected.clear();
                self.anchor = None;
                self.status = "selection cleared".into();
            }

            KeyCode::Char('/') => {
                self.input = Input::Query;
                self.buffer = self.query.clone();
            }
            KeyCode::Char('t') => {
                self.input = Input::AddTag;
                self.buffer.clear();
            }
            KeyCode::Char('T') => {
                self.input = Input::RemoveTag;
                self.buffer.clear();
            }
            KeyCode::Char('i') => {
                self.input = Input::Title;
                self.buffer.clear();
            }

            KeyCode::Char(c @ '0'..='5') => {
                let n = c.to_digit(10).unwrap() as i64;
                let value = if n == 0 { None } else { Some(n) };
                self.edit(vec![Change::SetRating(value)], &format!("rate {n}"))?;
            }
            KeyCode::Char('f') => self.edit(vec![Change::SetFlag(true)], "flag")?,
            KeyCode::Char('x') => self.edit(vec![Change::SetReject(true)], "reject")?,
            KeyCode::Char('c') => self.edit(
                vec![Change::SetFlag(false), Change::SetReject(false)],
                "clear marks",
            )?,

            KeyCode::Char('s') => {
                self.sidebar = match self.sidebar {
                    Sidebar::Tags => Sidebar::Dates,
                    Sidebar::Dates => Sidebar::Tags,
                };
                self.sidebar_cursor = 0;
                self.reload_sidebar()?;
            }
            KeyCode::Tab => {
                self.view = match self.view {
                    View::List => View::Grid,
                    View::Grid => View::Single,
                    View::Single => View::List,
                };
                self.member = 0;
                self.status = format!("{:?} view", self.view);
            }
            KeyCode::BackTab => {
                self.sidebar_focus = !self.sidebar_focus;
            }
            KeyCode::Char('+') | KeyCode::Char('=') if self.view == View::Grid => {
                self.grid_cols = (self.grid_cols + 1).min(12);
                self.images.clear();
                self.status = format!("{} tiles across (smaller)", self.grid_cols);
            }
            KeyCode::Char('-') if self.view == View::Grid => {
                self.grid_cols = self.grid_cols.saturating_sub(1).max(1);
                self.images.clear();
                self.status = format!("{} tiles across (bigger)", self.grid_cols);
            }
            KeyCode::Char('o') => {
                let n = self.current_members().len().max(1);
                self.member = (self.member + 1) % n;
            }
            KeyCode::Char('h') | KeyCode::Left if self.view == View::Single => {
                self.move_cursor(-1);
                self.member = 0;
            }
            KeyCode::Char('l') | KeyCode::Right if self.view == View::Single => {
                self.move_cursor(1);
                self.member = 0;
            }
            KeyCode::Enter if self.sidebar_focus => {
                if let Some((name, _)) = self.sidebar_items.get(self.sidebar_cursor) {
                    self.query = match self.sidebar {
                        Sidebar::Tags => format!("tag:\"{name}\""),
                        Sidebar::Dates => format!("date={name}"),
                    };
                    self.offset = 0;
                    self.cursor = 0;
                    self.reload()?;
                    self.status = format!("filter: {}", self.query);
                }
            }
            KeyCode::Char('u') => self.undo()?,
            KeyCode::Char('w') => {
                let stats = edit::flush(&mut self.cat, &self.config)?;
                self.status = format!("flushed {} sidecar(s)", stats.sidecars_written);
            }
            _ => {}
        }

        if self.sidebar_focus {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.sidebar_cursor =
                        (self.sidebar_cursor + 1).min(self.sidebar_items.len().saturating_sub(1))
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.sidebar_cursor = self.sidebar_cursor.saturating_sub(1)
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn on_input_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.input = Input::None;
                self.buffer.clear();
            }
            KeyCode::Backspace => {
                self.buffer.pop();
            }
            KeyCode::Char(c) => self.buffer.push(c),
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.buffer);
                let mode = std::mem::replace(&mut self.input, Input::None);
                match mode {
                    Input::Query => {
                        // Validate before committing, so a typo does not blank
                        // the view with an opaque error.
                        match query::compile(&text) {
                            Ok(_) => {
                                self.query = text;
                                self.offset = 0;
                                self.cursor = 0;
                                self.reload()?;
                                self.status = format!("{} match(es)", self.total);
                            }
                            Err(e) => self.status = format!("bad query: {e}"),
                        }
                    }
                    Input::AddTag => {
                        let tags: Vec<String> = text
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        if !tags.is_empty() {
                            self.edit(vec![Change::AddTags(tags)], "tag")?;
                        }
                    }
                    Input::RemoveTag => {
                        let tags: Vec<String> = text
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        if !tags.is_empty() {
                            self.edit(vec![Change::RemoveTags(tags)], "untag")?;
                        }
                    }
                    Input::Title => self.edit(vec![Change::SetTitle(text)], "title")?,
                    Input::None => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.rows.len() as isize;
        if len == 0 {
            return;
        }
        let next = (self.cursor as isize + delta).clamp(0, len - 1);
        self.cursor = next as usize;
    }

    fn run_action(&mut self, key: char) -> Result<()> {
        let Some(a) = self.actions.iter().find(|a| a.key == key.to_string()).cloned() else {
            self.status = format!("no action bound to `{key}`");
            return Ok(());
        };
        let ids = self.targets();
        let current = self.rows.get(self.cursor).map(|r| r.id);
        let sel = action::gather(&self.cat, &ids, current, &self.query)?;

        match action::run(&a, &sel) {
            Ok(outcome) => self.status = outcome.description,
            Err(e) => self.status = format!("{}: {e}", a.name),
        }
        Ok(())
    }

    // --- drawing ---------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(2)])
            .split(f.area());

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(28), Constraint::Min(20)])
            .split(outer[0]);

        // sidebar
        let title = match self.sidebar {
            Sidebar::Tags => " tags ",
            Sidebar::Dates => " dates ",
        };
        let items: Vec<ListItem> = self
            .sidebar_items
            .iter()
            .map(|(name, n)| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{n:>6}  "), Style::default().fg(Color::DarkGray)),
                    Span::raw(name.clone()),
                ]))
            })
            .collect();
        let mut side_state = ListState::default();
        side_state.select(Some(self.sidebar_cursor));
        let side_border = if self.sidebar_focus {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        f.render_stateful_widget(
            List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(side_border)
                        .title(title),
                )
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
            cols[0],
            &mut side_state,
        );

        match self.view {
            View::List => self.draw_list(f, cols[1]),
            View::Grid => self.draw_grid(f, cols[1]),
            View::Single => self.draw_single(f, cols[1]),
        }

        // Bottom area is two rows: what is selected, then what just happened.
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(outer[1]);

        let position = if self.rows.is_empty() {
            String::new()
        } else {
            format!(" {}/{} ", self.cursor + 1, self.rows.len())
        };
        let identity = Line::from(vec![
            Span::styled(
                if self.query.is_empty() {
                    "  all  ".to_string()
                } else {
                    format!("  {}  ", self.query)
                },
                Style::default().fg(Color::Black).bg(Color::Cyan),
            ),
            Span::styled(position, Style::default().fg(Color::DarkGray)),
            Span::styled(
                self.current_label().unwrap_or_default(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                self.rows
                    .get(self.cursor)
                    .filter(|r| !r.tags.is_empty())
                    .map(|r| format!("   [{}]", r.tags))
                    .unwrap_or_default(),
                Style::default().fg(Color::Blue),
            ),
        ]);
        f.render_widget(Paragraph::new(identity), rows[0]);

        let bottom = match self.input {
            Input::None => Line::from(vec![Span::styled(
                format!("  {}", self.status),
                Style::default().fg(Color::DarkGray),
            )]),
            _ => {
                let prompt = match self.input {
                    Input::Query => "query",
                    Input::AddTag => "tag",
                    Input::RemoveTag => "untag",
                    Input::Title => "title",
                    Input::None => "",
                };
                Line::from(vec![
                    Span::styled(
                        format!("  {prompt}  "),
                        Style::default().fg(Color::Black).bg(Color::Yellow),
                    ),
                    Span::raw(format!(" {}_", self.buffer)),
                ])
            }
        };
        f.render_widget(Paragraph::new(bottom), rows[1]);

        if self.show_help {
            self.overlay(f, " keys ", HELP);
        }
        if self.show_actions {
            let mut text = String::from("Press a key to run:\n\n");
            if self.actions.is_empty() {
                text.push_str("  no actions available\n  (none of the configured programs are on PATH)");
            }
            for a in &self.actions {
                text.push_str(&format!("  {}   {}\n", a.key, a.name));
            }
            self.overlay(f, " actions ", &text);
        }
    }

    fn heading(&self) -> String {
        format!(
            " {:?}  {} / {} stacks{} ",
            self.view,
            self.rows.len(),
            self.total,
            if self.selected.is_empty() {
                String::new()
            } else {
                format!("  |  {} selected", self.selected.len())
            }
        )
    }

    fn frame_block(&self, title: String) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .border_style(if self.sidebar_focus {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Cyan)
            })
            .title(title)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let rows: Vec<ListItem> = self
            .rows
            .iter()
            .map(|r| {
                let sel = if self.selected.contains(&r.id) { "*" } else { " " };
                let stars = "*".repeat(r.rating.unwrap_or(0).clamp(0, 5) as usize);
                let mark = if r.flag { "F" } else if r.reject { "X" } else { " " };
                let date = r.captured.map(format_date).unwrap_or_else(|| "----------".into());
                ListItem::new(Line::from(vec![
                    Span::styled(sel.to_string(), Style::default().fg(Color::Yellow)),
                    Span::raw(format!(" {:<20}", r.base)),
                    Span::styled(format!("{:<9}", r.roles), Style::default().fg(Color::DarkGray)),
                    Span::raw(format!("{date}  ")),
                    Span::styled(format!("{stars:<5}"), Style::default().fg(Color::Yellow)),
                    Span::styled(
                        format!("{mark} "),
                        Style::default().fg(if r.reject { Color::Red } else { Color::Green }),
                    ),
                    Span::styled(r.tags.clone(), Style::default().fg(Color::Blue)),
                ]))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(self.cursor));
        let block = self.frame_block(self.heading());
        f.render_stateful_widget(
            List::new(rows)
                .block(block)
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
            area,
            &mut state,
        );
    }

    /// A tiled grid of thumbnails. Tile size is fixed in cells; how many fit
    /// depends on the window, which is also what bounds image resolution.
    fn draw_grid(&mut self, f: &mut Frame, area: Rect) {
        let block = self.frame_block(self.heading());
        let inner = block.inner(area);
        f.render_widget(block, area);

        // Tiles are sized from the window rather than fixed: a thumbnail can
        // only occupy whole character cells, so the number across is what
        // actually determines resolution.
        let cols = self.grid_cols.clamp(1, inner.width.max(1));
        let tw = (inner.width / cols).max(4);
        // Keep tiles roughly square in pixels. Cells are about twice as tall
        // as they are wide, so half the width in cells is close to square.
        let th = (tw / 2).max(3) + 1;
        let rows_fit = (inner.height / th).max(1);
        let per_page = (cols * rows_fit) as usize;

        // Keep the cursor on screen by scrolling in whole rows.
        let cursor_row = self.cursor / cols as usize;
        let first_row = cursor_row.saturating_sub(rows_fit as usize - 1);
        let start = first_row * cols as usize;
        let end = (start + per_page).min(self.rows.len());

        self.prefetch(ThumbSize::Grid, per_page);

        for (n, idx) in (start..end).enumerate() {
            let cx = inner.x + (n as u16 % cols) * tw;
            let cy = inner.y + (n as u16 / cols) * th;
            let cell = Rect::new(cx, cy, tw.min(inner.width), th.min(inner.height));
            if cell.bottom() > inner.bottom() || cell.right() > inner.right() {
                continue;
            }

            let row = self.rows[idx].clone();
            let is_cursor = idx == self.cursor;
            let is_sel = self.selected.contains(&row.id);

            let border = if is_cursor {
                Style::default().fg(Color::Cyan)
            } else if is_sel {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let stars = "*".repeat(row.rating.unwrap_or(0).clamp(0, 5) as usize);
            let mark = if row.flag { " F" } else if row.reject { " X" } else { "" };
            let tile = Block::default()
                .borders(Borders::ALL)
                .border_style(border)
                .title(Line::from(vec![Span::styled(
                    format!(" {}{} ", stars, mark),
                    Style::default().fg(if row.reject { Color::Red } else { Color::Yellow }),
                )]));
            let picture = tile.inner(cell);
            f.render_widget(tile, cell);

            match self.image_for(idx, ThumbSize::Grid) {
                Some(path) if self.ensure_decoded(&path) => {
                    if let Some(p) = self.images.get_mut(&path) {
                        f.render_stateful_widget(StatefulImage::default(), picture, p);
                    }
                }
                _ => {
                    f.render_widget(
                        Paragraph::new(row.base.clone())
                            .style(Style::default().fg(Color::DarkGray)),
                        picture,
                    );
                }
            }
        }
    }

    /// One large preview. This is the culling loop: look, rate, advance.
    fn draw_single(&mut self, f: &mut Frame, area: Rect) {
        let members = self.current_members();
        let member = members.get(self.member.min(members.len().saturating_sub(1)));

        let title = match self.rows.get(self.cursor) {
            Some(r) => {
                let stars = "*".repeat(r.rating.unwrap_or(0).clamp(0, 5) as usize);
                let which = member
                    .map(|(n, role)| format!("  [{n}  {role}]"))
                    .unwrap_or_default();
                format!(
                    " {}  {}  {}/{}  {stars}{which} ",
                    r.base,
                    r.captured.map(format_date).unwrap_or_else(|| "--".into()),
                    self.cursor + 1,
                    self.rows.len()
                )
            }
            None => " nothing to show ".to_string(),
        };

        let block = self.frame_block(title);
        let inner = block.inner(area);
        f.render_widget(block, area);

        self.prefetch(ThumbSize::View, 2);

        match self.image_for(self.cursor, ThumbSize::View) {
            Some(path) if self.ensure_decoded(&path) => {
                if let Some(p) = self.images.get_mut(&path) {
                    f.render_stateful_widget(StatefulImage::default(), inner, p);
                }
            }
            _ => f.render_widget(
                Paragraph::new("rendering...").style(Style::default().fg(Color::DarkGray)),
                inner,
            ),
        }
    }

    fn overlay(&self, f: &mut Frame, title: &str, body: &str) {
        let area = centered(68, 92, f.area());
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(body)
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan))
                        .title(title),
                ),
            area,
        );
    }
}

const HELP: &str = "\
 j k  up  down        move
 g G                  first / last
 ctrl-d ctrl-u        page
 space                select / deselect
 v                    range: press once to start, again to close
 esc                  clear selection

 /                    query  (same syntax as `photo find`)
 t  T                 add / remove tags (comma separated)
 i                    set title
 1-5                  rate    0  clear rating
 f  x  c              flag / reject / clear marks

 tab                  list -> grid -> single
 shift-tab            focus sidebar    s  tags <-> dates
 enter                filter by the highlighted sidebar entry
 h l                  previous / next  (single view)
 o                    cycle which file of the stack is shown
 - +                  fewer / more tiles across (grid view)

 a                    actions menu
 w                    flush pending sidecars
 u                    undo last edit
 q                    quit

Edits are written to <base>.xmp immediately. Original images are
never opened for writing.";

fn format_date(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn centered(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}

/// Run the browser. The terminal is restored even on panic: leaving a user in
/// a wrecked tty because of a bug in a photo cataloguer is not acceptable.
pub fn run(root: PathBuf) -> Result<()> {
    let mut app = App::new(root)?;

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
        previous(info);
    }));

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Graphics protocol detection queries the terminal, so it has to happen
    // after raw mode is on.
    app.enable_images();

    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|f| app.draw(f))?;

            // Poll rather than block: thumbnails finish on a worker thread and
            // the frame they belong to has to be redrawn when they land.
            if event::poll(Duration::from_millis(120))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == event::KeyEventKind::Press {
                        app.on_key(key)?;
                    }
                }
            }
            // Drain whatever the generator finished; each one is a reason to
            // repaint, and draining keeps the channel from growing.
            if let Some(cache) = app.cache.as_ref() {
                while cache.done.try_recv().is_ok() {}
            }
            if app.quit {
                break;
            }
        }
        Ok(())
    })();

    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_render_with_the_same_convention_as_capture_times() {
        assert_eq!(format_date(1_786_233_666), "2026-08-09");
        assert_eq!(format_date(0), "1970-01-01");
    }

    /// A three-stack library on disk, so edits really do write sidecars.
    fn app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026/08/09");
        std::fs::create_dir_all(&day).unwrap();
        for n in 1..=3 {
            std::fs::write(day.join(format!("750_000{n}.NEF")), format!("raw{n}")).unwrap();
            std::fs::write(day.join(format!("750_000{n}.JPG")), format!("jpg{n}")).unwrap();
        }
        let mut cat = Catalog::open_memory().unwrap();
        crate::scan::scan(&mut cat, dir.path(), true).unwrap();
        // The fixture bytes carry no EXIF; stamp a capture time so date
        // grouping is exercised. Real EXIF is covered in tests/real_data.rs.
        cat.conn
            .execute("UPDATE file SET capture_time = 1786233666", [])
            .unwrap();
        let app = App::with_catalog(cat, dir.path().to_path_buf()).unwrap();
        (dir, app)
    }

    fn press(app: &mut App, c: char) {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
    }
    fn press_code(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
    }
    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, c);
        }
        press_code(app, KeyCode::Enter);
    }

    #[test]
    fn opens_showing_the_whole_library() {
        let (_d, app) = app();
        assert_eq!(app.total, 3);
        assert_eq!(app.rows.len(), 3);
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn movement_stays_in_bounds() {
        let (_d, mut app) = app();
        for _ in 0..10 {
            press(&mut app, 'j');
        }
        assert_eq!(app.cursor, 2, "cursor ran past the end");
        for _ in 0..10 {
            press(&mut app, 'k');
        }
        assert_eq!(app.cursor, 0, "cursor ran past the start");
    }

    #[test]
    fn space_toggles_selection_and_advances() {
        let (_d, mut app) = app();
        press(&mut app, ' ');
        assert_eq!(app.selected.len(), 1);
        assert_eq!(app.cursor, 1);

        press(&mut app, 'k'); // back onto it
        press(&mut app, ' '); // toggle off
        assert!(app.selected.is_empty());
    }

    #[test]
    fn rating_the_cursor_row_writes_a_sidecar() {
        let (dir, mut app) = app();
        press(&mut app, '3');

        let sidecar = dir.path().join("2026/08/09/750_0001.xmp");
        assert!(sidecar.exists(), "rating did not reach disk");
        assert_eq!(app.rows[0].rating, Some(3));
    }

    #[test]
    fn a_selection_is_edited_as_a_batch() {
        let (dir, mut app) = app();
        press(&mut app, ' ');
        press(&mut app, ' '); // two selected
        press(&mut app, '5');

        assert!(dir.path().join("2026/08/09/750_0001.xmp").exists());
        assert!(dir.path().join("2026/08/09/750_0002.xmp").exists());
        assert!(
            !dir.path().join("2026/08/09/750_0003.xmp").exists(),
            "an unselected stack was edited"
        );
    }

    #[test]
    fn tagging_through_the_prompt_reaches_disk_and_the_sidebar() {
        let (_d, mut app) = app();
        press(&mut app, 't');
        type_str(&mut app, "beach, anna");

        assert!(app.rows[0].tags.contains("anna"));
        assert!(app.rows[0].tags.contains("beach"));
        let names: Vec<&String> = app.sidebar_items.iter().map(|(n, _)| n).collect();
        assert!(names.iter().any(|n| n.as_str() == "beach"));
    }

    #[test]
    fn queries_filter_and_a_bad_one_is_reported_without_clearing_the_view() {
        let (_d, mut app) = app();
        press(&mut app, '4'); // rate the first
        press(&mut app, '/');
        type_str(&mut app, "rating>=4");
        assert_eq!(app.total, 1);

        press(&mut app, '/');
        type_str(&mut app, "colour=red");
        assert!(app.status.contains("bad query"), "status: {}", app.status);
        assert_eq!(app.total, 1, "a bad query blanked the view");
    }

    #[test]
    fn undo_restores_the_previous_state_exactly() {
        let (_d, mut app) = app();
        press(&mut app, 't');
        type_str(&mut app, "keeper");
        press(&mut app, '4');
        assert_eq!(app.rows[0].rating, Some(4));

        press(&mut app, 'u'); // undo the rating
        assert_eq!(app.rows[0].rating, None);
        assert!(
            app.rows[0].tags.contains("keeper"),
            "undo of a rating removed a tag it should not have touched"
        );

        press(&mut app, 'u'); // undo the tag
        assert_eq!(app.rows[0].tags, "");
    }

    /// Undo must restore prior state rather than apply a guessed inverse:
    /// tagging something already tagged, then undoing, must not remove the
    /// tag that was there first.
    #[test]
    fn undo_is_correct_when_edits_overlap() {
        let (_d, mut app) = app();
        press(&mut app, 't');
        type_str(&mut app, "beach");

        press(&mut app, ' ');
        press(&mut app, ' ');
        press(&mut app, 't');
        type_str(&mut app, "beach,ostsee"); // row 0 already had beach

        press(&mut app, 'u');
        assert!(
            app.rows[0].tags.contains("beach"),
            "undo removed a pre-existing tag: {}",
            app.rows[0].tags
        );
        assert!(!app.rows[0].tags.contains("ostsee"));
    }

    #[test]
    fn flag_and_reject_are_mutually_exclusive_in_the_ui() {
        let (_d, mut app) = app();
        press(&mut app, 'f');
        assert!(app.rows[0].flag && !app.rows[0].reject);
        press(&mut app, 'x');
        assert!(app.rows[0].reject && !app.rows[0].flag);
        press(&mut app, 'c');
        assert!(!app.rows[0].reject && !app.rows[0].flag);
    }

    #[test]
    fn the_sidebar_switches_and_filters() {
        let (_d, mut app) = app();
        press(&mut app, 't');
        type_str(&mut app, "beach");

        assert_eq!(app.sidebar, Sidebar::Tags);
        press(&mut app, 's');
        assert_eq!(app.sidebar, Sidebar::Dates);
        assert!(
            app.sidebar_items.iter().any(|(d, _)| d == "2026-08-09"),
            "dates sidebar is empty: {:?}",
            app.sidebar_items
        );

        press_code(&mut app, KeyCode::BackTab); // focus sidebar
        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.query, "date=2026-08-09");
        assert_eq!(app.total, 3);
    }

    /// Undated stacks must remain reachable rather than silently missing from
    /// the dates pane.
    #[test]
    fn undated_stacks_get_their_own_bucket() {
        let (_d, mut app) = app();
        app.cat
            .conn
            .execute("UPDATE file SET capture_time = NULL", [])
            .unwrap();
        press(&mut app, 's');
        assert!(
            app.sidebar_items.iter().any(|(d, n)| d == "undated" && *n == 3),
            "undated bucket missing: {:?}",
            app.sidebar_items
        );
    }

    #[test]
    fn escape_clears_a_selection() {
        let (_d, mut app) = app();
        press(&mut app, ' ');
        press(&mut app, ' ');
        assert_eq!(app.selected.len(), 2);
        press_code(&mut app, KeyCode::Esc);
        assert!(app.selected.is_empty());
    }

    /// Render into an in-memory buffer. Covers the draw path -- widget
    /// construction, layout arithmetic, overlay geometry -- without a tty,
    /// which is where a panic would otherwise wreck the user's terminal.
    fn render(app: &mut App, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_screen_renders_and_shows_the_library() {
        let (_d, mut app) = app();
        press(&mut app, 't');
        type_str(&mut app, "beach");
        press(&mut app, '4');

        let screen = render(&mut app, 120, 24);
        assert!(screen.contains("750_0001"), "list missing:\n{screen}");
        assert!(screen.contains("beach"), "tag missing:\n{screen}");
        assert!(screen.contains("tags"), "sidebar title missing:\n{screen}");
        assert!(screen.contains("****"), "rating stars missing:\n{screen}");
    }

    #[test]
    fn overlays_render_without_overflowing_the_frame() {
        let (_d, mut app) = app();
        app.show_help = true;
        let screen = render(&mut app, 120, 40);
        assert!(screen.contains("keys"), "help overlay missing");
        assert!(
            screen.contains("undo last edit"),
            "the key list is taller than its overlay and got truncated:\n{screen}"
        );

        app.show_help = false;
        app.show_actions = true;
        assert!(render(&mut app, 120, 40).contains("actions"));
    }

    /// A narrow terminal must not panic on layout arithmetic.
    #[test]
    fn renders_in_a_cramped_terminal() {
        let (_d, mut app) = app();
        for (w, h) in [(40u16, 10u16), (30, 8), (20, 6)] {
            let _ = render(&mut app, w, h);
        }
        app.show_help = true;
        let _ = render(&mut app, 30, 8);
    }

    #[test]
    fn the_query_prompt_is_visible_while_typing() {
        let (_d, mut app) = app();
        press(&mut app, '/');
        for c in "rating>=4".chars() {
            press(&mut app, c);
        }
        let screen = render(&mut app, 120, 24);
        assert!(screen.contains("query"), "prompt missing:\n{screen}");
        assert!(screen.contains("rating>=4"), "buffer missing:\n{screen}");
    }

    /// The three views must all render. Without a terminal there is no
    /// graphics protocol, so images fall back to placeholders -- which is
    /// exactly the path a non-kitty terminal takes, and worth covering.
    #[test]
    fn every_view_renders_without_a_graphics_protocol() {
        let (_d, mut app) = app();
        press(&mut app, '3');

        for expected in [View::Grid, View::Single, View::List] {
            press_code(&mut app, KeyCode::Tab);
            assert_eq!(app.view, expected);
            let screen = render(&mut app, 120, 30);
            assert!(!screen.trim().is_empty(), "{expected:?} drew nothing");
        }
    }

    #[test]
    fn tab_cycles_views_and_shift_tab_focuses_the_sidebar() {
        let (_d, mut app) = app();
        assert_eq!(app.view, View::List);
        press_code(&mut app, KeyCode::Tab);
        assert_eq!(app.view, View::Grid);
        press_code(&mut app, KeyCode::Tab);
        assert_eq!(app.view, View::Single);
        press_code(&mut app, KeyCode::Tab);
        assert_eq!(app.view, View::List);

        assert!(!app.sidebar_focus);
        press_code(&mut app, KeyCode::BackTab);
        assert!(app.sidebar_focus);
    }

    /// The culling loop: h/l walk the result set in single view, and each move
    /// resets which member is displayed.
    #[test]
    fn single_view_walks_the_result_set() {
        let (_d, mut app) = app();
        press_code(&mut app, KeyCode::Tab);
        press_code(&mut app, KeyCode::Tab); // single
        assert_eq!(app.cursor, 0);

        press(&mut app, 'l');
        assert_eq!(app.cursor, 1);
        press(&mut app, 'l');
        assert_eq!(app.cursor, 2);
        press(&mut app, 'l');
        assert_eq!(app.cursor, 2, "walked past the end");

        press(&mut app, 'h');
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn o_cycles_through_the_members_of_a_stack() {
        let (_d, mut app) = app();
        // fixture stacks hold a NEF and a JPG
        assert_eq!(app.current_members().len(), 2);
        assert_eq!(app.member, 0);
        press(&mut app, 'o');
        assert_eq!(app.member, 1);
        press(&mut app, 'o');
        assert_eq!(app.member, 0, "member index did not wrap");
    }

    /// Rating must work identically in every view -- that is the whole point
    /// of the single view existing.
    #[test]
    fn rating_works_in_grid_and_single_view() {
        let (dir, mut app) = app();
        press_code(&mut app, KeyCode::Tab); // grid
        press(&mut app, '2');
        assert_eq!(app.rows[0].rating, Some(2));

        press_code(&mut app, KeyCode::Tab); // single
        press(&mut app, 'l');
        press(&mut app, '5');
        assert_eq!(app.rows[1].rating, Some(5));
        assert!(dir.path().join("2026/08/09/750_0002.xmp").exists());
    }

    /// The grid tile shows only stars, so the status area is the only place
    /// the identity of a shot appears. It must be there in every view.
    #[test]
    fn the_status_area_names_the_selected_file() {
        let (_d, mut app) = app();
        for view in [View::List, View::Grid, View::Single] {
            while app.view != view {
                press_code(&mut app, KeyCode::Tab);
            }
            let screen = render(&mut app, 120, 30);
            assert!(
                screen.contains("750_0001"),
                "{view:?} view does not name the selection:\n{screen}"
            );
        }
    }

    #[test]
    fn the_label_follows_the_cursor_and_the_member() {
        let (_d, mut app) = app();
        assert!(app.current_label().unwrap().contains("750_0001"));
        press(&mut app, 'j');
        assert!(app.current_label().unwrap().contains("750_0002"));

        // `o` cycles members, and the label must name the actual file.
        let first = app.current_label().unwrap();
        press(&mut app, 'o');
        let second = app.current_label().unwrap();
        assert_ne!(first, second, "the label ignored the member cycle");
        assert!(first.ends_with(".NEF") || first.ends_with(".JPG"));
    }

    /// The path is shown relative to the library root, not absolutely -- an
    /// absolute path would push the filename off the end of the line.
    #[test]
    fn the_label_is_relative_to_the_library_root() {
        let (dir, app) = app();
        let label = app.current_label().unwrap();
        assert!(!label.contains(dir.path().to_str().unwrap()));
        assert!(label.starts_with("2026/08/09/"), "got {label}");
    }

    #[test]
    fn tags_appear_next_to_the_filename() {
        let (_d, mut app) = app();
        press(&mut app, 't');
        type_str(&mut app, "beach");
        let screen = render(&mut app, 120, 30);
        assert!(screen.contains("[beach]"), "tags missing:\n{screen}");
    }

    #[test]
    fn quit_sets_the_flag() {
        let (_d, mut app) = app();
        assert!(!app.quit);
        press(&mut app, 'q');
        assert!(app.quit);
    }

    /// A protected library must not be editable from the UI either, and the
    /// refusal has to be visible rather than silent.
    #[test]
    fn a_protected_library_is_refused_in_the_ui() {
        let (dir, mut app) = app();
        app.config = Config {
            protected_roots: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        press(&mut app, '5');
        assert!(app.status.contains("refused"), "status: {}", app.status);
        assert!(!dir.path().join("2026/08/09/750_0001.xmp").exists());
    }
}
