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

// Terminal geometry is `u16` (ratatui's `Rect`), indices are `usize`, and
// percentages are computed in between. Converting between them is what layout
// code in this crate *is*, and a terminal is never 65535 columns wide, so the
// truncation these lints warn about cannot occur. Annotating each of the forty
// sites individually would bury the arithmetic it is meant to draw attention
// to. The data paths -- scan, db, edit, import -- are deliberately not
// exempted, so a cast there still has to be argued for.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use crate::action::{self, Action};
use crate::config::Config;
use crate::db::Catalog;
use crate::edit::{self, Change};
use crate::exif::{self, format_date, format_datetime, format_ev, format_shutter, Exif};
use crate::query;
use crate::thumbs::{self, Size as ThumbSize};
use crate::watch;
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
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

const PAGE: usize = 500;

/// Single-view zoom: each `+`/`-` multiplies or divides `zoom_level` by this.
const ZOOM_STEP: f32 = 1.4;
/// A crop narrower than this starts showing individual source pixels as
/// visible blocks rather than detail -- past it, "more zoom" stops helping.
const ZOOM_MAX: f32 = 8.0;
/// Pan step per arrow press, as a fraction of the *crop's* width/height, so
/// the same key press feels like the same amount of motion whether zoomed in
/// a little or a lot -- a fixed fraction of the whole image would barely
/// move a highly zoomed-in crop and fly across a barely-zoomed one.
const PAN_STEP: f32 = 0.1;

/// Eighth-block glyphs, low to high -- the timeline bar's height encoding.
const TIMELINE_LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

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

/// Whether trashed stacks appear alongside the rest of the library.
/// `Exclude` is the default: a trashed stack is not part of what you're
/// browsing, it's a stack on its way out, and mixing the two made a
/// just-trashed duplicate resurface in the very view used to find and
/// trash duplicates in the first place. `--all` asks for `Include`; a
/// dedicated `--trash` (not a query anyone would think to type) asks for
/// `Only`, so the trash itself is still reachable to review before
/// `trash-empty`.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum TrashView {
    Exclude,
    Include,
    Only,
}

impl TrashView {
    /// The clause to AND onto a query whose stack-id column is `col`
    /// (`"s.id"`, `"f.stack_id"`, `"st.stack_id"`, ...). `Include` needs no
    /// clause at all, since `1=1` would otherwise force every caller to
    /// special-case "no filter" separately.
    pub(crate) fn clause(self, col: &str) -> String {
        match self {
            TrashView::Exclude => format!("{col} NOT IN (SELECT stack_id FROM trashed)"),
            TrashView::Include => "1=1".to_string(),
            TrashView::Only => format!("{col} IN (SELECT stack_id FROM trashed)"),
        }
    }
}

/// How the timeline buckets capture time. Day is finest, Year coarsest;
/// `[`/`]` step between them and clamp at the ends (like `grid_cols`'s
/// `+`/`-`, not wrapping) rather than jumping from Year back to Day.
#[derive(Debug, PartialEq, Clone, Copy)]
pub(crate) enum Granularity {
    Day,
    Week,
    Month,
    Year,
}

impl Granularity {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Granularity::Day => "day",
            Granularity::Week => "week",
            Granularity::Month => "month",
            Granularity::Year => "year",
        }
    }

    pub(crate) fn coarser(self) -> Self {
        match self {
            Granularity::Day => Granularity::Week,
            Granularity::Week => Granularity::Month,
            Granularity::Month | Granularity::Year => Granularity::Year,
        }
    }

    pub(crate) fn finer(self) -> Self {
        match self {
            Granularity::Day | Granularity::Week => Granularity::Day,
            Granularity::Month => Granularity::Week,
            Granularity::Year => Granularity::Month,
        }
    }
}

/// One bar in the timeline: a span of capture time, how many stacks fall in
/// it, and the query fragment that selects exactly them.
#[derive(Debug, Clone)]
pub(crate) struct TimelineBucket {
    pub(crate) label: String,
    pub(crate) filter: String,
    pub(crate) count: i64,
}

/// Whole-library bucket counts at `granularity`, computed live in SQL rather
/// than through any persisted table -- shared by `tui.rs`'s own
/// `reload_timeline` (which layers cursor/offset bookkeeping on top) and
/// `gui.rs`'s equivalent, so the two front ends can never disagree about
/// what a bucket contains. See `App::reload_timeline`'s doc comment for why
/// this is a full scan rather than an indexed lookup, and why that's fine.
pub(crate) fn compute_timeline(
    cat: &Catalog,
    granularity: Granularity,
    trash_view: TrashView,
) -> Result<Vec<TimelineBucket>> {
    let sql = match granularity {
        Granularity::Day => {
            "SELECT date(f.capture_time,'unixepoch'),
                    date(f.capture_time,'unixepoch'),
                    COUNT(DISTINCT f.stack_id)
             FROM file f WHERE f.capture_time IS NOT NULL AND {trash}
             GROUP BY date(f.capture_time,'unixepoch')
             ORDER BY date(f.capture_time,'unixepoch')"
        }
        // SQLite's 'weekday 0' modifier moves forward to the next Sunday;
        // backing that up 6 days lands on the Monday of the same week, so
        // this needs no date math of its own.
        Granularity::Week => {
            "SELECT date(f.capture_time,'unixepoch','weekday 0','-6 days'),
                    date(f.capture_time,'unixepoch','weekday 0'),
                    COUNT(DISTINCT f.stack_id)
             FROM file f WHERE f.capture_time IS NOT NULL AND {trash}
             GROUP BY date(f.capture_time,'unixepoch','weekday 0','-6 days')
             ORDER BY date(f.capture_time,'unixepoch','weekday 0','-6 days')"
        }
        Granularity::Month => {
            "SELECT strftime('%Y-%m', f.capture_time,'unixepoch'),
                    strftime('%Y-%m', f.capture_time,'unixepoch'),
                    COUNT(DISTINCT f.stack_id)
             FROM file f WHERE f.capture_time IS NOT NULL AND {trash}
             GROUP BY strftime('%Y-%m', f.capture_time,'unixepoch')
             ORDER BY strftime('%Y-%m', f.capture_time,'unixepoch')"
        }
        Granularity::Year => {
            "SELECT strftime('%Y', f.capture_time,'unixepoch'),
                    strftime('%Y', f.capture_time,'unixepoch'),
                    COUNT(DISTINCT f.stack_id)
             FROM file f WHERE f.capture_time IS NOT NULL AND {trash}
             GROUP BY strftime('%Y', f.capture_time,'unixepoch')
             ORDER BY strftime('%Y', f.capture_time,'unixepoch')"
        }
    };
    let sql = sql.replace("{trash}", &trash_view.clause("f.stack_id"));

    Ok(cat
        .conn
        .prepare(&sql)?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .filter_map(std::result::Result::ok)
        .map(|(start, end, count)| {
            let filter = if start == end {
                format!("date:{start}")
            } else {
                format!("date:{start}..{end}")
            };
            TimelineBucket {
                label: start,
                filter,
                count,
            }
        })
        .collect())
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
/// Prior state of one stack: tags, rating, flag, reject.
type StackState = (i64, Vec<String>, Option<i64>, bool, bool);

struct UndoStep {
    label: String,
    prior: Vec<StackState>,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "independent UI modes, not a state machine that could be an enum"
)]
pub struct App {
    cat: Catalog,
    config: Config,
    actions: Vec<Action>,
    root: PathBuf,

    query: String,
    /// Rating checkboxes, Shotwell-style: `Ctrl+1`..`Ctrl+5` toggles the
    /// corresponding entry independently, letting "2 and 5, nothing else"
    /// be selected in one step, which typing `rating>=N` alone can't
    /// express. Unlike `gui.rs`'s own `rating_filter` (a hidden structural
    /// AND onto the compiled query, shown via checkboxes), the TUI has no
    /// checkbox widget to show that state in -- so this is spliced
    /// directly into the visible `query` text instead (an `(rating=2 or
    /// rating=5)` fragment, tracked and replaced the same way
    /// `applied_filter` tracks a sidebar click), and this array exists
    /// purely to know what to rebuild that fragment into on the next
    /// toggle, not to filter anything itself.
    rating_filter: [bool; 5],
    /// The exact `rating=N` / `(rating=N or ...)` fragment last spliced
    /// into `query` by `toggle_rating_filter` -- separate from
    /// `applied_filter` (a sidebar/timeline click) so the two can't clobber
    /// each other's tracking.
    rating_query_fragment: Option<String>,
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

    /// The exact fragment last spliced into `query` by picking a timeline
    /// bucket or a sidebar entry -- so the next pick can replace just that
    /// fragment instead of overwriting anything typed alongside it via `/`.
    /// `None` once the fragment no longer appears verbatim (the user edited
    /// or cleared it by hand), which falls back to a plain splice-in.
    applied_filter: Option<String>,

    /// Whole-library bucket counts over time, independent of `query` -- a
    /// navigation aid, like the sidebar's dates list, not a view of the
    /// current filter.
    timeline: Vec<TimelineBucket>,
    timeline_cursor: usize,
    /// Leftmost visible bucket; scrolls to keep `timeline_cursor` in view.
    timeline_offset: usize,
    /// How many buckets the last draw had room for. Only the draw pass knows
    /// this, same reasoning as `page_size`.
    timeline_width: usize,
    timeline_focus: bool,
    granularity: Granularity,

    input: Input,
    buffer: String,
    /// Char index (not byte index -- tags and titles may be non-ASCII) of
    /// the edit cursor within `buffer`.
    input_cursor: usize,
    status: String,
    show_help: bool,
    show_actions: bool,
    undo: Vec<UndoStep>,
    quit: bool,

    view: View,
    cache: Option<thumbs::Cache>,
    /// Background library watcher (see `watch.rs`). `None` when disabled in
    /// config, offline, or failed to start -- any of which just means "no
    /// automatic cataloging this session," never a fatal error.
    watcher: Option<watch::Watcher>,
    /// Raw decoded pixels, keyed by cache-file path -- content only, no
    /// screen position or protocol state. Reading and JPEG-decoding a
    /// thumbnail is the expensive part of showing one; nothing about *where*
    /// it is drawn invalidates the pixels, so this survives rect changes,
    /// resizes and the manual `r` re-send. Bounded the same way as `images`.
    decoded: HashMap<PathBuf, image::DynamicImage>,
    /// Decoded images ready to draw, keyed by cache-file path. Bounded so a
    /// long scroll cannot grow without limit.
    images: HashMap<PathBuf, ratatui_image::protocol::StatefulProtocol>,
    /// The screen rect each path was drawn at last frame. A kitty-protocol
    /// placement is bound to a screen position; reusing a cached protocol
    /// after its path moved to a new rect without telling the terminal is
    /// what produced black tiles (confirmed: `r`, which force-clears
    /// `images`, always fixed it). Comparing against this on every draw is
    /// what lets that clearing happen automatically instead -- by rebuilding
    /// a fresh `StatefulProtocol` from `decoded`, not by re-reading the file.
    last_rect: HashMap<PathBuf, Rect>,
    picker: Option<Picker>,
    /// Which member of the stack the single view is showing.
    member: usize,
    /// How many rows a screen holds in the current view. Only the draw pass
    /// knows this -- it depends on the window and, in the grid, on the tile
    /// size -- so it is recorded there and used by the paging keys.
    page_size: usize,
    /// Cache paths drawn in the current frame. Anything not in here is
    /// released after the frame, so the terminal only ever holds images that
    /// are actually on screen.
    frame_used: HashSet<PathBuf>,
    /// False when the library is not mounted. Browsing and tagging still work
    /// -- edits queue -- but nothing that needs the files themselves does.
    online: bool,
    /// Tiles across in grid view. Adjustable live, because the right value
    /// depends on font size and window, which only the user can see.
    grid_cols: u16,
    /// Set once at startup from `--all`/`--trash`; nothing in a session
    /// changes it.
    trash_view: TrashView,
    /// Single view only: `+` requests `Size::Full` instead of `Size::View`
    /// for the current image -- a full-resolution decode instead of the
    /// RAW's fast preview tier -- and enters zoom mode. A second `+` crops
    /// in further (`zoom_level` above 1.0); `-` backs out, leaving zoom mode
    /// entirely once `zoom_level` would drop back to 1.0. Does not persist
    /// across `h`/`l`, unlike `grid_cols` persisting across pages -- panned
    /// into a corner of one shot is not a framing anyone wants inherited by
    /// the next.
    single_zoom: bool,
    /// 1.0 is the whole `Size::Full` frame; higher crops a smaller, centered
    /// region out of it. Never below 1.0 while `single_zoom` is true.
    zoom_level: f32,
    /// Normalized center of the crop within the full image, each axis
    /// 0.0..=1.0, clamped so the crop rectangle never leaves the image.
    zoom_center: (f32, f32),
    /// The cropped-and-resized protocol currently on screen, plus the
    /// (path, rect, level, center) it was built for -- rebuilt only when one
    /// of those actually changes, the same invalidation shape as
    /// `ensure_decoded_at` uses for `last_rect`. Kept separate from `images`
    /// because that map is one full-frame protocol per source path; a zoom
    /// crop is a different image on every pan step without the underlying
    /// file, or its path, ever changing.
    zoom_image: Option<ratatui_image::protocol::StatefulProtocol>,
    zoom_image_key: Option<(PathBuf, Rect, u32, u32, u32)>,
}

impl App {
    pub fn new(root: PathBuf, trash_view: TrashView) -> Result<Self> {
        let (cat, online) = Catalog::open_for_root(&root)?;
        let mut app = Self::with_catalog(cat, root, trash_view)?;
        app.online = online;
        Ok(app)
    }

    /// Split out so the interaction logic can be exercised without a terminal.
    pub fn with_catalog(cat: Catalog, root: PathBuf, trash_view: TrashView) -> Result<Self> {
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
            rating_filter: [false; 5],
            rating_query_fragment: None,
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
            applied_filter: None,
            timeline: Vec::new(),
            timeline_cursor: 0,
            timeline_offset: 0,
            timeline_width: 1,
            timeline_focus: false,
            granularity: Granularity::Month,
            input: Input::None,
            buffer: String::new(),
            input_cursor: 0,
            status: String::new(),
            show_help: false,
            show_actions: false,
            undo: Vec::new(),
            quit: false,
            view: View::List,
            cache: None,
            watcher: None,
            decoded: HashMap::new(),
            images: HashMap::new(),
            last_rect: HashMap::new(),
            picker: None,
            member: 0,
            frame_used: HashSet::new(),
            page_size: 20,
            grid_cols: 5,
            online: true,
            trash_view,
            single_zoom: false,
            zoom_level: 1.0,
            zoom_center: (0.5, 0.5),
            zoom_image: None,
            zoom_image_key: None,
        };
        app.reload()?;
        app.status = format!("{} stacks. ? for help.", app.total);
        Ok(app)
    }

    // --- data ------------------------------------------------------------

    /// Bring a structural pick (a timeline bucket, a sidebar entry) into
    /// `query` without discarding anything the user typed alongside it via
    /// `/`. If the fragment from the *previous* pick is still present
    /// verbatim, it is replaced in place; otherwise the new fragment is
    /// anded onto whatever is there (or becomes the whole query, if it was
    /// empty). Either way `applied_filter` is updated so the next pick can
    /// find this one.
    fn splice_filter(&mut self, fragment: &str) {
        self.query = match self
            .applied_filter
            .as_deref()
            .filter(|prior| self.query.contains(prior))
        {
            Some(prior) => self.query.replacen(prior, fragment, 1),
            None if self.query.trim().is_empty() => fragment.to_string(),
            None => format!("{fragment} and {}", self.query),
        };
        self.applied_filter = Some(fragment.to_string());
    }

    fn reload(&mut self) -> Result<()> {
        let mut compiled = query::compile(&self.query)?;
        compiled.where_sql = format!(
            "({}) AND {}",
            compiled.where_sql,
            self.trash_view.clause("s.id")
        );

        self.total = {
            let sql = format!("SELECT COUNT(*) FROM stack s WHERE {}", compiled.where_sql);
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
        self.reload_timeline()?;
        Ok(())
    }

    /// `Ctrl+1`..`Ctrl+5`: flip one rating checkbox, splice the resulting
    /// `rating=N` / `(rating=A or rating=B)` fragment into the visible
    /// query, and reload. `n` is 1-based, matching the key pressed.
    fn toggle_rating_filter(&mut self, n: usize) -> Result<()> {
        self.rating_filter[n - 1] = !self.rating_filter[n - 1];
        let selected: Vec<usize> = (1..=5).filter(|&i| self.rating_filter[i - 1]).collect();
        let new_fragment = match selected.as_slice() {
            [] => String::new(),
            [only] => format!("rating={only}"),
            many => format!(
                "({})",
                many.iter()
                    .map(|n| format!("rating={n}"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            ),
        };

        // The new fragment is always prepended at the front (below), so
        // removing the old one is either an exact match (it was the whole
        // query) or a `<fragment> and ` prefix -- same "only touch it if
        // it's still there verbatim" rule `splice_filter` uses for a
        // sidebar/timeline pick.
        if let Some(prior) = self.rating_query_fragment.take() {
            if let Some(rest) = self.query.strip_prefix(&format!("{prior} and ")) {
                self.query = rest.to_string();
            } else if self.query == prior {
                self.query.clear();
            }
        }

        if new_fragment.is_empty() {
            self.rating_query_fragment = None;
        } else {
            self.query = if self.query.trim().is_empty() {
                new_fragment.clone()
            } else {
                format!("{new_fragment} and {}", self.query)
            };
            self.rating_query_fragment = Some(new_fragment);
        }

        self.reload()
    }

    fn reload_sidebar(&mut self) -> Result<()> {
        self.sidebar_items = match self.sidebar {
            Sidebar::Tags => self
                .cat
                .conn
                .prepare(&format!(
                    "SELECT t.name, COUNT(st.stack_id) n FROM tag t
                     LEFT JOIN stack_tag st ON st.tag_id = t.id AND {}
                     GROUP BY t.id ORDER BY n DESC, t.name",
                    self.trash_view.clause("st.stack_id")
                ))?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .filter_map(Result::ok)
                .collect(),
            // Stacks with no capture time get an explicit bucket rather than
            // vanishing from this pane -- undated shots are exactly the ones
            // you need to find in order to fix them.
            // Events are days of shooting. A named one shows its name, so the
            // pane reads as a list of shoots rather than a list of dates.
            Sidebar::Dates => self
                .cat
                .conn
                .prepare(&format!(
                    "SELECT COALESCE(
                              NULLIF(e.name, ''),
                              date(e.start_time, 'unixepoch'),
                              'undated') AS d,
                            COUNT(s.id) AS n
                     FROM stack s LEFT JOIN event e ON e.id = s.event_id
                     WHERE {}
                     GROUP BY COALESCE(e.id, -1)
                     ORDER BY (e.start_time IS NULL), e.start_time DESC",
                    self.trash_view.clause("s.id")
                ))?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .filter_map(Result::ok)
                .collect(),
        };
        if self.sidebar_cursor >= self.sidebar_items.len() {
            self.sidebar_cursor = 0;
        }
        Ok(())
    }

    /// Whole-library bucket counts at the current `granularity`, computed
    /// live in SQL rather than through any persisted table -- unlike day
    /// events, week/month/year buckets have no need to survive a rescan by
    /// name, so there's nothing to persist. Wrapping `capture_time` in
    /// `date()`/`strftime()` means this is a full scan of `file`, not an
    /// index seek on `file_capture` (`db.rs`) -- measured on a real
    /// 92k-stack library regardless: 1257 day buckets in ~60ms, cheap enough
    /// that it isn't worth an expression index. Day/month/year filter with a
    /// single `date:label` fragment, since `query::day_bounds` already
    /// expands a partial date to its full range; week has no such
    /// shorthand, so its bucket carries its own real start/end and filters
    /// with `date:start..end`.
    fn reload_timeline(&mut self) -> Result<()> {
        // This runs on every reload() -- tagging, filtering, editing, none
        // of which change the timeline's own (whole-library) data -- so the
        // previously-selected bucket is kept by label rather than jumping
        // back to the most recent one every time. Falls back to "most
        // recent" only when there was nothing selected yet (first load) or
        // the label no longer exists (a granularity change: "2026-08" isn't
        // a day-granularity label, so this doubles as that reset too).
        let previous = self
            .timeline
            .get(self.timeline_cursor)
            .map(|b| b.label.clone());

        self.timeline = compute_timeline(&self.cat, self.granularity, self.trash_view)?;

        self.timeline_cursor = previous
            .and_then(|label| self.timeline.iter().position(|b| b.label == label))
            .unwrap_or_else(|| self.timeline.len().saturating_sub(1));
        self.clamp_timeline_offset();
        Ok(())
    }

    /// Scrolls `timeline_offset` to keep `timeline_cursor` inside the window
    /// the last draw had room for -- same shape as the list's page scrolling,
    /// just client-side rather than a new SQL page, since even day buckets
    /// across years are only a few thousand rows.
    fn clamp_timeline_offset(&mut self) {
        let width = self.timeline_width.max(1);
        if self.timeline_cursor < self.timeline_offset {
            self.timeline_offset = self.timeline_cursor;
        } else if self.timeline_cursor >= self.timeline_offset + width {
            self.timeline_offset = self.timeline_cursor + 1 - width;
        }
        let max_offset = self.timeline.len().saturating_sub(width);
        self.timeline_offset = self.timeline_offset.min(max_offset);
    }

    /// Attach the thumbnail cache and terminal graphics protocol. Only done
    /// when a real terminal is present, so headless tests stay unaffected.
    pub fn enable_images(&mut self) {
        if let Ok(v) = crate::vol::identify(&self.root) {
            self.cache = thumbs::Cache::new(&v.uuid).ok();
        }
        // Thumbnails come from the laptop cache, so they still render with the
        // drive away; only generating new ones needs it.
        if self.online {
            if let Ok(v) = crate::vol::identify(&self.root) {
                self.cache = thumbs::Cache::new(&v.uuid).ok();
            }
        } else {
            let uuid: String = self
                .cat
                .conn
                .query_row("SELECT vol_uuid FROM volume LIMIT 1", [], |r| r.get(0))
                .unwrap_or_default();
            self.cache = thumbs::Cache::new(&uuid).ok();
        }
        // Watching needs the real filesystem, same precondition as
        // generating a new thumbnail -- offline, there is nothing to watch.
        // A failure to start (disabled in config, or the watcher itself
        // erroring) is never fatal to the session; it just means no
        // automatic cataloging this time.
        let watch_error = if self.online {
            match watch::Watcher::spawn(self.root.clone(), &self.config) {
                Ok(w) => {
                    self.watcher = w;
                    None
                }
                Err(e) => Some(e.to_string()),
            }
        } else {
            None
        };
        self.picker = Picker::from_query_stdio().ok();
        // Say plainly what was detected. A silent fallback to halfblocks looks
        // like broken thumbnails rather than an unsupported terminal.
        self.status = match self.picker.as_ref() {
            Some(p) => {
                let (cw, ch) = p.font_size();
                match p.protocol_type() {
                    ratatui_image::picker::ProtocolType::Halfblocks => format!(
                        "no graphics protocol - falling back to halfblocks ({cw}x{ch}px cells). \
                         Photos will not be identifiable. Run `npc term` to diagnose."
                    ),
                    other => format!("{other:?} graphics, {cw}x{ch}px cells. ? for help."),
                }
            }
            None => "terminal graphics query failed; thumbnails unavailable".into(),
        };
        if let Some(e) = watch_error {
            use std::fmt::Write as _;
            let _ = write!(self.status, " (library watcher failed to start: {e})");
        }
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
        let img = if let Some(img) = self.decoded.get(path) {
            img.clone()
        } else {
            let Ok(img) = image::open(path) else {
                return false;
            };
            self.decoded.insert(path.clone(), img.clone());
            img
        };
        let protocol = picker.new_resize_protocol(img);
        self.images.insert(path.clone(), protocol);
        true
    }

    /// Like `ensure_decoded`, but stops after populating `decoded` -- a
    /// zoom crop needs the raw pixels, never the full-frame protocol
    /// `ensure_decoded` would otherwise build and immediately discard.
    fn ensure_full_decoded(&mut self, path: &PathBuf) -> bool {
        if self.decoded.contains_key(path) {
            return true;
        }
        let Ok(img) = image::open(path) else {
            return false;
        };
        self.decoded.insert(path.clone(), img);
        true
    }

    /// Crops `decoded[path]` to the `zoom_level`/`zoom_center` window and
    /// builds a fresh protocol for just that crop, into `zoom_image` --
    /// never `images`, which holds one full-frame protocol per path, not a
    /// continuously-changing crop of one. Rebuilds only when the key
    /// (path, screen rect, level, center) actually changed, the same
    /// invalidation shape `ensure_decoded_at` uses via `last_rect`.
    fn ensure_zoom_image(&mut self, path: &PathBuf, rect: Rect) -> bool {
        if !self.ensure_full_decoded(path) {
            return false;
        }
        // Quantized so tiny float drift from repeated pans doesn't defeat
        // the cache -- a few pan steps that end up back where they started
        // must compare equal, not rebuild on floating-point noise.
        let key = (
            path.clone(),
            rect,
            (self.zoom_level * 1000.0).round() as u32,
            (self.zoom_center.0 * 1_000_000.0).round() as u32,
            (self.zoom_center.1 * 1_000_000.0).round() as u32,
        );
        if self.zoom_image_key.as_ref() == Some(&key) {
            return true;
        }
        let Some(img) = self.decoded.get(path) else {
            return false;
        };
        let (w, h) = (img.width(), img.height());
        let crop_w = ((w as f32 / self.zoom_level).round() as u32).clamp(1, w);
        let crop_h = ((h as f32 / self.zoom_level).round() as u32).clamp(1, h);
        // Center in pixel space, then pull back inside the image if the crop
        // box would otherwise hang off an edge -- `clamp_zoom_center` keeps
        // this from being needed in the normal pan/zoom path, but a stack
        // change can land here with a center computed for a differently
        // sized image (a portrait RAW's JPG sibling, say).
        let cx = (self.zoom_center.0 * w as f32).round() as i64;
        let cy = (self.zoom_center.1 * h as f32).round() as i64;
        let x = (cx - i64::from(crop_w) / 2).clamp(0, i64::from(w - crop_w)) as u32;
        let y = (cy - i64::from(crop_h) / 2).clamp(0, i64::from(h - crop_h)) as u32;
        let cropped = img.crop_imm(x, y, crop_w, crop_h);

        let Some(picker) = self.picker.as_mut() else {
            return false;
        };
        self.zoom_image = Some(picker.new_resize_protocol(cropped));
        self.zoom_image_key = Some(key);
        true
    }

    /// Like `ensure_decoded`, but also forces a fresh protocol -- a new
    /// `StatefulProtocol`, hence a full retransmit -- whenever `path` is
    /// about to be drawn at a different rect than it was last frame, the
    /// moment a stale kitty-protocol placement can occur. This still costs a
    /// re-encode and re-send, which is unavoidable since the tile genuinely
    /// moved, but not a re-read and re-decode of the source file: `decoded`
    /// keeps the pixels around independent of screen position, so only
    /// `images` (the placement-bound state) gets dropped here.
    /// Every call site that draws an image should go through this, not
    /// `ensure_decoded` directly.
    fn ensure_decoded_at(&mut self, path: &PathBuf, rect: Rect) -> bool {
        if self.last_rect.get(path) != Some(&rect) {
            self.images.remove(path);
        }
        self.last_rect.insert(path.clone(), rect);
        self.ensure_decoded(path)
    }

    /// Drop every built protocol, forcing a fresh one -- and so a fresh
    /// transmit -- for whatever is drawn next frame. Shared by the manual `r`
    /// key and by `on_resize`. Deliberately leaves `decoded` alone: the pixel
    /// content a resize or a stale placement invalidates is `images`, not
    /// the source bytes. Returns how many were dropped, for the status line.
    fn clear_images(&mut self) -> usize {
        let dropped = self.images.len();
        self.images.clear();
        self.last_rect.clear();
        dropped
    }

    /// A terminal resize can change the cell/font geometry a `StatefulProtocol`
    /// was built against, so any cached protocol may no longer match what the
    /// terminal now expects. Resizes are rare, so a full clear costs nothing
    /// that matters.
    fn on_resize(&mut self) {
        self.clear_images();
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
            .map_or_else(|| row.base.clone(), |(name, _)| name.clone());

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

    /// Same file as `current_label`, but the real on-disk path rather than
    /// one relative to the library root -- for the single view's info panel,
    /// which has room for it and is where "where is this on disk" belongs,
    /// unlike the width-constrained status line `current_label` feeds.
    fn current_full_path(&self) -> Option<String> {
        let row = self.rows.get(self.cursor)?;
        let members = self.current_members();
        let filename = members
            .get(self.member.min(members.len().saturating_sub(1)))
            .map_or_else(|| row.base.clone(), |(name, _)| name.clone());
        Some(format!("{}/{filename}", row.dir))
    }

    /// Shooting data for the file under the cursor.
    fn current_exif(&self) -> Option<Exif> {
        let row = self.rows.get(self.cursor)?;
        exif::for_stack(&self.cat, row.id)
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
        self.rows
            .get(self.cursor)
            .map(|r| r.id)
            .into_iter()
            .collect()
    }

    fn snapshot(&self, ids: &[i64]) -> Result<Vec<StackState>> {
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

    fn edit(&mut self, changes: &[Change], label: &str) -> Result<()> {
        let ids = self.targets();
        if ids.is_empty() {
            self.status = "nothing selected".into();
            return Ok(());
        }
        let prior = self.snapshot(&ids)?;

        match edit::apply(&mut self.cat, &ids, changes, &self.config) {
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

            // Split from the arrow keys below: j/k always move the cursor,
            // even while a zoomed single view has claimed the arrows for
            // panning -- only the arrows change meaning in that mode.
            KeyCode::Char('j') if !self.sidebar_focus => self.move_cursor(1)?,
            KeyCode::Char('k') if !self.sidebar_focus => self.move_cursor(-1)?,
            KeyCode::Down if !self.sidebar_focus && !self.single_zoom => self.move_cursor(1)?,
            KeyCode::Up if !self.sidebar_focus && !self.single_zoom => self.move_cursor(-1)?,
            KeyCode::PageDown => self.move_cursor(self.page_size as isize)?,
            KeyCode::PageUp => self.move_cursor(-(self.page_size as isize))?,
            // Half a screen, the vi convention.
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_cursor((self.page_size / 2).max(1) as isize)?;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_cursor(-((self.page_size / 2).max(1) as isize))?;
            }
            KeyCode::Char('g') | KeyCode::Home => self.jump_to_first()?,
            KeyCode::Char('G') | KeyCode::End => self.jump_to_last()?,

            KeyCode::Char(' ') => {
                if let Some(r) = self.rows.get(self.cursor) {
                    if !self.selected.insert(r.id) {
                        self.selected.remove(&r.id);
                    }
                    self.move_cursor(1)?;
                }
            }
            KeyCode::Char('v') => {
                if let Some(start) = self.anchor.take() {
                    let (a, b) = if start <= self.cursor {
                        (start, self.cursor)
                    } else {
                        (self.cursor, start)
                    };
                    for r in &self.rows[a..=b] {
                        self.selected.insert(r.id);
                    }
                    self.status = format!("{} selected", self.selected.len());
                } else {
                    self.anchor = Some(self.cursor);
                    self.status = "range start; press v again to close".into();
                }
            }
            // Esc zooms out of the single view, pairing with Enter zooming in.
            // Elsewhere it clears the selection, which is what it is for when
            // there is nothing to back out of.
            KeyCode::Esc if self.view == View::Single => {
                self.view = View::Grid;
                self.member = 0;
                self.reset_zoom();
                self.status = "grid".into();
            }
            KeyCode::Esc => {
                self.selected.clear();
                self.anchor = None;
                self.status = "selection cleared".into();
            }

            KeyCode::Char('/') => {
                self.input = Input::Query;
                self.buffer = self.query.clone();
                self.input_cursor = self.buffer.chars().count();
            }
            KeyCode::Char('t') => {
                self.input = Input::AddTag;
                self.buffer.clear();
                self.input_cursor = 0;
            }
            KeyCode::Char('T') => {
                self.input = Input::RemoveTag;
                self.buffer.clear();
                self.input_cursor = 0;
            }
            KeyCode::Char('i') => {
                self.input = Input::Title;
                self.buffer.clear();
                self.input_cursor = 0;
            }

            KeyCode::Char(c @ '1'..='5') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let n = c.to_digit(10).unwrap() as usize;
                self.toggle_rating_filter(n)?;
            }
            KeyCode::Char(c @ '0'..='5') => {
                let n = i64::from(c.to_digit(10).unwrap());
                let value = if n == 0 { None } else { Some(n) };
                self.edit(&[Change::SetRating(value)], &format!("rate {n}"))?;
            }
            KeyCode::Char('f') => self.edit(&[Change::SetFlag(true)], "flag")?,
            KeyCode::Char('x') => self.edit(&[Change::SetReject(true)], "reject")?,
            KeyCode::Char('c') => self.edit(
                &[Change::SetFlag(false), Change::SetReject(false)],
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
            // Mirrors the sidebar's own focus toggle, as a single dedicated
            // key rather than Left/Right (already taken) since the timeline
            // sits below rather than beside the main pane.
            KeyCode::Char('p') => {
                self.timeline_focus = !self.timeline_focus;
                if self.timeline_focus {
                    self.sidebar_focus = false;
                }
            }
            KeyCode::Char('[') if self.timeline_focus => {
                self.granularity = self.granularity.finer();
                self.reload_timeline()?;
            }
            KeyCode::Char(']') if self.timeline_focus => {
                self.granularity = self.granularity.coarser();
                self.reload_timeline()?;
            }
            KeyCode::Tab => {
                self.view = match self.view {
                    View::List => View::Grid,
                    View::Grid => View::Single,
                    View::Single => View::List,
                };
                self.member = 0;
                self.reset_zoom();
                self.status = format!("{:?} view", self.view);
            }
            KeyCode::BackTab => {
                self.view = match self.view {
                    View::List => View::Single,
                    View::Grid => View::List,
                    View::Single => View::Grid,
                };
                self.member = 0;
                self.reset_zoom();
                self.status = format!("{:?} view", self.view);
            }
            // Timeline focus claims the arrows first -- checked before the
            // sidebar's own Left/Right below, so the two never fight over
            // the same keys. Mutual exclusion holds because `p` above is the
            // only way in, and it already clears sidebar_focus.
            KeyCode::Left if self.timeline_focus && self.view != View::Single => {
                self.timeline_cursor = self.timeline_cursor.saturating_sub(1);
                self.clamp_timeline_offset();
            }
            KeyCode::Right if self.timeline_focus && self.view != View::Single => {
                self.timeline_cursor =
                    (self.timeline_cursor + 1).min(self.timeline.len().saturating_sub(1));
                self.clamp_timeline_offset();
            }
            // Sidebar focus moves to the arrows: in a two-pane layout, left
            // and right are what a hand reaches for anyway.
            KeyCode::Left if self.view != View::Single => {
                self.sidebar_focus = true;
            }
            KeyCode::Right if self.view != View::Single => {
                self.sidebar_focus = false;
            }
            KeyCode::Char('+' | '=') if self.view == View::Grid => {
                self.grid_cols = (self.grid_cols + 1).min(12);
                self.images.clear();
                self.status = format!("{} tiles across (smaller)", self.grid_cols);
            }
            KeyCode::Char('-') if self.view == View::Grid => {
                self.grid_cols = self.grid_cols.saturating_sub(1).max(1);
                self.images.clear();
                self.status = format!("{} tiles across (bigger)", self.grid_cols);
            }
            KeyCode::Char('+' | '=') if self.view == View::Single && !self.single_zoom => {
                self.single_zoom = true;
                self.status = "full resolution".into();
            }
            KeyCode::Char('+' | '=') if self.view == View::Single && self.single_zoom => {
                self.adjust_zoom(1);
            }
            KeyCode::Char('-') if self.view == View::Single && self.single_zoom => {
                self.adjust_zoom(-1);
            }
            // Every arrow direction, while zoomed, pans the crop instead of
            // moving the cursor -- see the split j/k-vs-arrows guard above
            // for why this is arrows-only, not h/l too.
            KeyCode::Up if self.view == View::Single && self.single_zoom => {
                self.pan_zoom(0.0, -1.0);
            }
            KeyCode::Down if self.view == View::Single && self.single_zoom => {
                self.pan_zoom(0.0, 1.0);
            }
            KeyCode::Left if self.view == View::Single && self.single_zoom => {
                self.pan_zoom(-1.0, 0.0);
            }
            KeyCode::Right if self.view == View::Single && self.single_zoom => {
                self.pan_zoom(1.0, 0.0);
            }
            KeyCode::Char('o') => {
                let n = self.current_members().len().max(1);
                self.member = (self.member + 1) % n;
            }
            KeyCode::Char('h') | KeyCode::Left if self.view == View::Single => {
                self.move_cursor(-1)?;
                self.member = 0;
                self.reset_zoom();
            }
            KeyCode::Char('l') | KeyCode::Right if self.view == View::Single => {
                self.move_cursor(1)?;
                self.member = 0;
                self.reset_zoom();
            }
            KeyCode::Enter if self.timeline_focus => {
                if let Some(b) = self.timeline.get(self.timeline_cursor) {
                    self.splice_filter(&b.filter.clone());
                    self.offset = 0;
                    self.cursor = 0;
                    self.reload()?;
                    self.status = format!("filter: {}", self.query);
                }
            }
            // Enter zooms into whatever is under the cursor.
            KeyCode::Enter if !self.sidebar_focus && self.view != View::Single => {
                self.view = View::Single;
                self.member = 0;
                self.status = "single".into();
            }
            KeyCode::Enter if self.sidebar_focus => {
                if let Some((name, _)) = self.sidebar_items.get(self.sidebar_cursor) {
                    let fragment = match self.sidebar {
                        Sidebar::Tags => format!("tag:\"{name}\""),
                        Sidebar::Dates => format!("event=\"{name}\""),
                    };
                    self.splice_filter(&fragment);
                    self.offset = 0;
                    self.cursor = 0;
                    self.reload()?;
                    self.status = format!("filter: {}", self.query);
                }
            }
            // Force a re-upload of the current image. Also a diagnostic: if a
            // frame renders black and this fixes it, the pixels were fine and
            // the terminal lost the placement.
            KeyCode::Char('r') => {
                let dropped = self.clear_images();
                self.status = format!("re-sent {dropped} image(s) to the terminal");
            }
            KeyCode::Char('y') => {
                self.status = match self.current_full_path() {
                    Some(path) => match copy_to_clipboard(&path) {
                        Ok(()) => format!("copied {path}"),
                        Err(e) => format!("copy failed: {e}"),
                    },
                    None => "nothing to copy".into(),
                };
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
                        (self.sidebar_cursor + 1).min(self.sidebar_items.len().saturating_sub(1));
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.sidebar_cursor = self.sidebar_cursor.saturating_sub(1);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Readline-style editing, shared by every text prompt (`Input::Query`,
    /// `AddTag`, `RemoveTag`, `Title`) since they all go through this one
    /// buffer. `input_cursor` is a char index, not a byte index, so a
    /// multi-byte tag or title can't have its cursor land mid-codepoint.
    ///
    /// Deliberately no `Alt`-based word-jump: many terminals intercept
    /// Alt+letter for their own bindings or deliver it as a raw `Esc` prefix
    /// instead of a modifier flag, which would make the feature work
    /// inconsistently across terminals. `Ctrl-w` alone covers the common
    /// case (delete the last word) without needing forward word-movement.
    fn on_input_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let mut chars: Vec<char> = self.buffer.chars().collect();
        let cursor = self.input_cursor.min(chars.len());

        match key.code {
            KeyCode::Esc => {
                self.input = Input::None;
                self.buffer.clear();
                self.input_cursor = 0;
            }
            KeyCode::Left => self.input_cursor = cursor.saturating_sub(1),
            KeyCode::Char('b') if ctrl => self.input_cursor = cursor.saturating_sub(1),
            KeyCode::Right => self.input_cursor = (cursor + 1).min(chars.len()),
            KeyCode::Char('f') if ctrl => self.input_cursor = (cursor + 1).min(chars.len()),
            KeyCode::Home => self.input_cursor = 0,
            KeyCode::Char('a') if ctrl => self.input_cursor = 0,
            KeyCode::End => self.input_cursor = chars.len(),
            KeyCode::Char('e') if ctrl => self.input_cursor = chars.len(),
            // Kill to end of line.
            KeyCode::Char('k') if ctrl => {
                chars.truncate(cursor);
                self.buffer = chars.into_iter().collect();
            }
            // Kill to start of line.
            KeyCode::Char('u') if ctrl => {
                chars.drain(0..cursor);
                self.buffer = chars.into_iter().collect();
                self.input_cursor = 0;
            }
            // Delete the word before the cursor.
            KeyCode::Char('w') if ctrl => {
                let mut start = cursor;
                while start > 0 && chars[start - 1].is_whitespace() {
                    start -= 1;
                }
                while start > 0 && !chars[start - 1].is_whitespace() {
                    start -= 1;
                }
                chars.drain(start..cursor);
                self.buffer = chars.into_iter().collect();
                self.input_cursor = start;
            }
            // Forward-delete. No conflict with the list view's Ctrl-d
            // (page down) -- this function only runs while `input != None`.
            KeyCode::Char('d') if ctrl => {
                if cursor < chars.len() {
                    chars.remove(cursor);
                    self.buffer = chars.into_iter().collect();
                }
            }
            KeyCode::Delete => {
                if cursor < chars.len() {
                    chars.remove(cursor);
                    self.buffer = chars.into_iter().collect();
                }
            }
            KeyCode::Backspace => {
                if cursor > 0 {
                    chars.remove(cursor - 1);
                    self.buffer = chars.into_iter().collect();
                    self.input_cursor = cursor - 1;
                }
            }
            KeyCode::Char(c) => {
                chars.insert(cursor, c);
                self.buffer = chars.into_iter().collect();
                self.input_cursor = cursor + 1;
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.buffer);
                self.input_cursor = 0;
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
                            self.edit(&[Change::AddTags(tags)], "tag")?;
                        }
                    }
                    Input::RemoveTag => {
                        let tags: Vec<String> = text
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        if !tags.is_empty() {
                            self.edit(&[Change::RemoveTags(tags)], "untag")?;
                        }
                    }
                    Input::Title => self.edit(&[Change::SetTitle(text)], "title")?,
                    Input::None => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Move the cursor by `delta` rows. `self.rows` only ever holds one SQL
    /// page (`PAGE` rows) at a time, so a move that would run off either edge
    /// turns the page instead of stopping there -- otherwise nothing could
    /// ever reach a result past the first `PAGE` matches. Deliberately coarse
    /// (land on the opposite edge of the new page, like `less`) rather than
    /// preserving exact scroll distance across the seam.
    fn move_cursor(&mut self, delta: isize) -> Result<()> {
        let len = self.rows.len() as isize;
        if len == 0 {
            return Ok(());
        }
        let target = self.cursor as isize + delta;
        if target < 0 {
            if self.offset > 0 {
                // Pages are always fetched in fixed `PAGE`-sized windows, so
                // the previous page's offset is a multiple of `PAGE` back --
                // NOT `self.rows.len()`, which is only `PAGE` on a full page
                // and the (smaller) remainder on the last one.
                self.offset = self.offset.saturating_sub(PAGE);
                self.reload()?;
                self.cursor = self.rows.len().saturating_sub(1);
            } else {
                self.cursor = 0;
            }
            return Ok(());
        }
        if target >= len {
            if self.offset + self.rows.len() < self.total {
                self.offset += PAGE;
                self.reload()?;
                self.cursor = 0;
            } else {
                self.cursor = (len - 1) as usize;
            }
            return Ok(());
        }
        self.cursor = target as usize;
        Ok(())
    }

    /// Back to the plain preview -- whenever the framing a zoom crop was
    /// centered on stops being valid: leaving single view, or moving to a
    /// different image within it.
    fn reset_zoom(&mut self) {
        self.single_zoom = false;
        self.zoom_level = 1.0;
        self.zoom_center = (0.5, 0.5);
        self.zoom_image = None;
        self.zoom_image_key = None;
    }

    /// `+`/`-` on an already-zoomed image: positive `delta` crops in,
    /// negative backs out. Backing out of `zoom_level` 1.0 leaves zoom mode
    /// entirely -- that is `reset_zoom`'s job, not a `zoom_level` below 1.0.
    fn adjust_zoom(&mut self, delta: i32) {
        if delta > 0 {
            self.zoom_level = (self.zoom_level * ZOOM_STEP).min(ZOOM_MAX);
            self.clamp_zoom_center();
            self.status = format!("zoomed {:.1}x", self.zoom_level);
        } else if self.zoom_level <= 1.0 {
            // Already at the floor -- one more '-' leaves zoom mode, rather
            // than landing on some level below 1.0 that '+' would then have
            // to climb back up from.
            self.reset_zoom();
            self.status = "preview".into();
        } else {
            self.zoom_level = (self.zoom_level / ZOOM_STEP).max(1.0);
            self.clamp_zoom_center();
            self.status = format!("zoomed {:.1}x", self.zoom_level);
        }
    }

    /// Move the crop center by `PAN_STEP` of the crop's own size -- see
    /// `PAN_STEP`'s doc comment for why it scales with `zoom_level` rather
    /// than being a fixed fraction of the whole image.
    fn pan_zoom(&mut self, dx: f32, dy: f32) {
        self.zoom_center.0 += dx * PAN_STEP / self.zoom_level;
        self.zoom_center.1 += dy * PAN_STEP / self.zoom_level;
        self.clamp_zoom_center();
    }

    /// Keeps the crop rectangle -- width and height `1.0 / zoom_level` in
    /// normalized image space -- entirely within the image. At `zoom_level`
    /// 1.0 this pins the center to exactly `(0.5, 0.5)`, correctly making
    /// panning a no-op when there is nowhere left to pan.
    fn clamp_zoom_center(&mut self) {
        let half = 0.5 / self.zoom_level;
        self.zoom_center.0 = self.zoom_center.0.clamp(half, 1.0 - half);
        self.zoom_center.1 = self.zoom_center.1.clamp(half, 1.0 - half);
    }

    /// Jump to the true first row of the whole result set, not just the
    /// current page.
    fn jump_to_first(&mut self) -> Result<()> {
        if self.offset != 0 {
            self.offset = 0;
            self.reload()?;
        }
        self.cursor = 0;
        Ok(())
    }

    /// Jump to the true last row of the whole result set, computing which
    /// page it falls on from `self.total`.
    fn jump_to_last(&mut self) -> Result<()> {
        if self.total == 0 {
            return Ok(());
        }
        let last_offset = (self.total - 1) / PAGE * PAGE;
        if self.offset != last_offset {
            self.offset = last_offset;
            self.reload()?;
        }
        self.cursor = self.rows.len().saturating_sub(1);
        Ok(())
    }

    fn run_action(&mut self, key: char) -> Result<()> {
        let Some(a) = self
            .actions
            .iter()
            .find(|a| a.key == key.to_string())
            .cloned()
        else {
            self.status = format!("no action bound to `{key}`");
            return Ok(());
        };
        if !self.online {
            self.status = format!("{} not launched: the library is not mounted", a.name);
            return Ok(());
        }
        let ids = self.targets();

        // A protected root protects against *this* program's writes. It says
        // nothing about a program this program launches -- RawTherapee will
        // write a .pp3, an image viewer will happily embed a rating, and the
        // guard never sees it. Refuse rather than hand a protected file to
        // something that can modify it.
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
                self.status = format!(
                    "{} not launched: {} is protected, and an external editor \
                     cannot be stopped from writing there",
                    a.name, dir
                );
                return Ok(());
            }
        }

        let current = self.rows.get(self.cursor).map(|r| r.id);
        let sel = action::gather(&self.cat, &ids, current, &self.query)?;

        match action::run(&a, &sel, &self.config) {
            Ok(outcome) => self.status = outcome.description,
            Err(e) => self.status = format!("{}: {e}", a.name),
        }
        Ok(())
    }

    // --- drawing ---------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        self.frame_used.clear();
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(3),
                Constraint::Length(2),
            ])
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
                    Span::styled(
                        format!("{n:>6}  "),
                        Style::default().fg(self.config.colors.muted),
                    ),
                    Span::raw(name.clone()),
                ]))
            })
            .collect();
        let mut side_state = ListState::default();
        side_state.select(Some(self.sidebar_cursor));
        let side_border = if self.sidebar_focus {
            Style::default().fg(self.config.colors.focus)
        } else {
            Style::default().fg(self.config.colors.muted)
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

        self.draw_timeline(f, outer[1]);

        // Bottom area is two rows: what is selected, then what just happened.
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(outer[2]);

        let exif = self.current_exif();
        let position = if self.rows.is_empty() {
            String::new()
        } else {
            format!(" {}/{} ", self.cursor + 1, self.rows.len())
        };
        let pending: i64 = self
            .cat
            .conn
            .query_row("SELECT COUNT(*) FROM stack WHERE dirty = 1", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        let offline_badge = if self.online {
            String::new()
        } else {
            format!(
                "  OFFLINE{}  ",
                if pending > 0 {
                    format!(" - {pending} pending")
                } else {
                    String::new()
                }
            )
        };

        let identity = Line::from(vec![
            Span::styled(
                offline_badge,
                Style::default()
                    .fg(Color::Black)
                    .bg(self.config.colors.accent),
            ),
            Span::styled(
                if self.query.is_empty() {
                    "  all  ".to_string()
                } else {
                    format!("  {}  ", self.query)
                },
                Style::default()
                    .fg(Color::Black)
                    .bg(self.config.colors.focus),
            ),
            Span::styled(position, Style::default().fg(self.config.colors.muted)),
            Span::styled(
                self.current_label().unwrap_or_default(),
                Style::default()
                    .fg(self.config.colors.text)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                self.rows
                    .get(self.cursor)
                    .filter(|r| !r.tags.is_empty())
                    .map(|r| format!("   [{}]", r.tags))
                    .unwrap_or_default(),
                Style::default().fg(self.config.colors.tag),
            ),
            Span::styled(
                exif.as_ref()
                    .map(|e| format!("   {}", e.line()))
                    .unwrap_or_default(),
                Style::default().fg(self.config.colors.accent),
            ),
        ]);
        f.render_widget(Paragraph::new(identity), rows[0]);

        let bottom = if self.input == Input::None {
            Line::from(vec![Span::styled(
                format!("  {}", self.status),
                Style::default().fg(self.config.colors.muted),
            )])
        } else {
            let prompt = match self.input {
                Input::Query => "query",
                Input::AddTag => "tag",
                Input::RemoveTag => "untag",
                Input::Title => "title",
                Input::None => "",
            };
            // A real cursor, not a fake trailing `_`: split the buffer at
            // `input_cursor` and reverse-video the character sitting there,
            // or a blank cell if the cursor is past the last character --
            // the ordinary terminal look for "end of line".
            let chars: Vec<char> = self.buffer.chars().collect();
            let at = self.input_cursor.min(chars.len());
            let before: String = chars[..at].iter().collect();
            let (cursor_ch, after): (String, String) = if at < chars.len() {
                (chars[at].to_string(), chars[at + 1..].iter().collect())
            } else {
                (" ".to_string(), String::new())
            };
            Line::from(vec![
                Span::styled(
                    format!("  {prompt}  "),
                    Style::default()
                        .fg(Color::Black)
                        .bg(self.config.colors.accent),
                ),
                Span::raw(format!(" {before}")),
                Span::styled(cursor_ch, Style::default().add_modifier(Modifier::REVERSED)),
                Span::raw(after),
            ])
        };
        f.render_widget(Paragraph::new(bottom), rows[1]);

        // Release every image not drawn this frame. Dropping a protocol tells
        // the terminal to forget that image, so the graphics store holds only
        // what is on screen. Previously this kept up to 120 uploaded images
        // and then dropped all of them at once, which is a good way to make a
        // terminal lose track of placements.
        let used = std::mem::take(&mut self.frame_used);
        self.images.retain(|k, _| used.contains(k));
        self.last_rect.retain(|k, _| used.contains(k));
        self.decoded.retain(|k, _| used.contains(k));
        self.frame_used = used;

        if self.show_help {
            self.overlay(f, " keys ", HELP);
        }
        if self.show_actions {
            let mut text = String::from("Press a key to run:\n\n");
            if self.actions.is_empty() {
                text.push_str(
                    "  no actions available\n  (none of the configured programs are on PATH)",
                );
            }
            for a in &self.actions {
                let _ = writeln!(text, "  {}   {}", a.key, a.name);
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
        // A white frame is zoom mode's only screen-wide cue that the arrows
        // now pan instead of moving the cursor -- distinct from the normal
        // focus/muted pair so it can never be mistaken for either.
        let border = if self.view == View::Single && self.single_zoom {
            Style::default().fg(Color::White)
        } else if self.sidebar_focus {
            Style::default().fg(self.config.colors.muted)
        } else {
            Style::default().fg(self.config.colors.focus)
        };
        Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .title(title)
    }

    /// Three interior rows (header, bar, ticks) inside a bordered block, the
    /// same focus-color convention the sidebar uses. `timeline_width` is
    /// recorded here, same reasoning as `page_size` -- only the draw pass
    /// knows how many columns are actually available.
    fn draw_timeline(&mut self, f: &mut Frame, area: Rect) {
        // No border, deliberately: a bordered block cost 2 extra rows for a
        // panel that only ever holds 3 lines of content, which more than
        // halved the list's usable height on a modest terminal. Focus reads
        // from color alone -- the header line and the highlighted bar
        // column -- the same information a border would have carried.
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);

        self.timeline_width = usize::from(area.width).max(1);
        self.clamp_timeline_offset();

        // No border to color for a focus cue (dropped for vertical space, see
        // above), so an explicit marker carries it instead -- also the only
        // thing about focus a symbol-only render test can observe, since
        // color/style differences alone are invisible to it.
        let (marker, header_style) = if self.timeline_focus {
            (
                "\u{25b8} ",
                Style::default()
                    .fg(self.config.colors.focus)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            ("  ", Style::default().fg(self.config.colors.muted))
        };

        if self.timeline.is_empty() {
            f.render_widget(
                Paragraph::new(format!(
                    "{marker}timeline ({}): no dated photos yet",
                    self.granularity.label()
                ))
                .style(header_style),
                rows[0],
            );
            return;
        }

        let width = self.timeline_width;
        let end = (self.timeline_offset + width).min(self.timeline.len());
        let visible = &self.timeline[self.timeline_offset..end];
        let max = visible.iter().map(|b| b.count).max().unwrap_or(1).max(1);

        let selected = &self.timeline[self.timeline_cursor];
        f.render_widget(
            Paragraph::new(format!(
                "{marker}timeline ({}): {}  ({} stack(s))",
                self.granularity.label(),
                selected.label,
                selected.count
            ))
            .style(header_style),
            rows[0],
        );

        let bar: Vec<Span> = visible
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let level =
                    ((b.count as f64 / max as f64) * (TIMELINE_LEVELS.len() - 1) as f64).round();
                let ch = TIMELINE_LEVELS[(level as usize).min(TIMELINE_LEVELS.len() - 1)];
                let style = timeline_bar_style(
                    self.timeline_offset + i == self.timeline_cursor,
                    self.config.colors.focus,
                    self.config.colors.accent,
                );
                Span::styled(ch.to_string(), style)
            })
            .collect();
        f.render_widget(Paragraph::new(Line::from(bar)), rows[1]);

        // A sparse tick under every Nth bar, spaced so consecutive labels
        // never collide (all labels share one granularity's fixed width),
        // so which period a bar is can be read without moving the cursor
        // there first -- not just the currently-selected one.
        let label_width = visible.iter().map(|b| b.label.len()).max().unwrap_or(0);
        let stride = (label_width + 1).max(1);
        let mut tick_line = String::new();
        for (i, b) in visible.iter().enumerate() {
            if i % stride == 0 {
                while tick_line.chars().count() < i {
                    tick_line.push(' ');
                }
                tick_line.push_str(&b.label);
            }
        }
        f.render_widget(
            Paragraph::new(tick_line).style(Style::default().fg(self.config.colors.muted)),
            rows[2],
        );
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let rows: Vec<ListItem> = self
            .rows
            .iter()
            .map(|r| {
                let sel = if self.selected.contains(&r.id) {
                    "*"
                } else {
                    " "
                };
                let stars = "*".repeat(r.rating.unwrap_or(0).clamp(0, 5) as usize);
                let mark = if r.flag {
                    "F"
                } else if r.reject {
                    "X"
                } else {
                    " "
                };
                let date = r.captured.map_or_else(|| "----------".into(), format_date);
                ListItem::new(Line::from(vec![
                    Span::styled(
                        sel.to_string(),
                        Style::default().fg(self.config.colors.accent),
                    ),
                    Span::raw(format!(" {:<20}", r.base)),
                    Span::styled(
                        format!("{:<9}", r.roles),
                        Style::default().fg(self.config.colors.muted),
                    ),
                    Span::raw(format!("{date}  ")),
                    Span::styled(
                        format!("{stars:<5}"),
                        Style::default().fg(self.config.colors.accent),
                    ),
                    Span::styled(
                        format!("{mark} "),
                        Style::default().fg(if r.reject {
                            self.config.colors.warn
                        } else {
                            self.config.colors.ok
                        }),
                    ),
                    Span::styled(r.tags.clone(), Style::default().fg(self.config.colors.tag)),
                ]))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(self.cursor));
        // Two rows of border.
        self.page_size = area.height.saturating_sub(2).max(1) as usize;
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
        self.page_size = per_page;

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
                Style::default().fg(self.config.colors.focus)
            } else if is_sel {
                Style::default().fg(self.config.colors.accent)
            } else {
                Style::default().fg(self.config.colors.muted)
            };
            let rating_col = "*".repeat(row.rating.unwrap_or(0).clamp(0, 5) as usize);
            let mark = if row.flag {
                " F"
            } else if row.reject {
                " X"
            } else {
                ""
            };
            let tile = Block::default()
                .borders(Borders::ALL)
                .border_style(border)
                .title(Line::from(vec![Span::styled(
                    format!(" {rating_col}{mark} "),
                    Style::default().fg(if row.reject {
                        self.config.colors.warn
                    } else {
                        self.config.colors.accent
                    }),
                )]));
            let picture = tile.inner(cell);
            f.render_widget(tile, cell);

            match self.image_for(idx, ThumbSize::Grid) {
                Some(path) if self.ensure_decoded_at(&path, picture) => {
                    self.frame_used.insert(path.clone());
                    if let Some(p) = self.images.get_mut(&path) {
                        f.render_stateful_widget(StatefulImage::default(), picture, p);
                    }
                }
                _ => {
                    f.render_widget(
                        Paragraph::new(row.base.clone())
                            .style(Style::default().fg(self.config.colors.muted)),
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
                    r.captured.map_or_else(|| "--".into(), format_date),
                    self.cursor + 1,
                    self.rows.len()
                )
            }
            None => " nothing to show ".to_string(),
        };

        // Paging through one image at a time is what h/l are for; a page here
        // means a stride worth using.
        self.page_size = 10;

        let block = self.frame_block(title);
        let inner = block.inner(area);
        f.render_widget(block, area);

        // A portrait frame leaves width unused, a landscape frame leaves
        // height. Put the shooting data wherever the picture is not.
        let exif = self.current_exif();
        let portrait = exif.as_ref().is_some_and(Exif::is_portrait);
        let (picture, panel) = if portrait && inner.width > 60 {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(20), Constraint::Length(34)])
                .split(inner);
            (cols[0], Some(cols[1]))
        } else if !portrait && inner.height > 12 {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(6), Constraint::Length(4)])
                .split(inner);
            (rows[0], Some(rows[1]))
        } else {
            (inner, None)
        };

        // Neighbors prefetch at the fast preview tier regardless of zoom --
        // a full-resolution decode is the expensive path this project
        // measured at ~140ms, not worth paying for images not yet on screen.
        self.prefetch(ThumbSize::View, 2);

        let size = if self.single_zoom {
            ThumbSize::Full
        } else {
            ThumbSize::View
        };
        let drawn = match self.image_for(self.cursor, size) {
            Some(path) if self.single_zoom => {
                if self.ensure_zoom_image(&path, picture) {
                    self.frame_used.insert(path.clone());
                    if let Some(p) = self.zoom_image.as_mut() {
                        f.render_stateful_widget(StatefulImage::default(), picture, p);
                    }
                    true
                } else {
                    false
                }
            }
            Some(path) if self.ensure_decoded_at(&path, picture) => {
                self.frame_used.insert(path.clone());
                if let Some(p) = self.images.get_mut(&path) {
                    f.render_stateful_widget(StatefulImage::default(), picture, p);
                }
                true
            }
            _ => false,
        };
        if !drawn {
            f.render_widget(
                Paragraph::new("rendering...").style(Style::default().fg(self.config.colors.muted)),
                picture,
            );
        }

        if let Some(area) = panel {
            let label = |k: &str, v: String| {
                Line::from(vec![
                    Span::styled(
                        format!("{k:<3}"),
                        Style::default().fg(self.config.colors.muted),
                    ),
                    Span::styled(v, Style::default().fg(self.config.colors.accent)),
                ])
            };
            let path_line = self.current_full_path().map(|p| {
                Line::from(Span::styled(
                    p,
                    Style::default().fg(self.config.colors.muted),
                ))
            });
            let mut lines = Vec::new();
            if let Some(e) = exif.as_ref() {
                if portrait {
                    // Stacked: one field per line, room to spare.
                    if let Some(t) = e.captured {
                        lines.push(label("", format_datetime(t)));
                        lines.push(Line::from(""));
                    }
                    if let Some(x) = e.exposure.filter(|x| *x > 0.0) {
                        lines.push(label("S", format_shutter(x)));
                    }
                    if let Some(x) = e.fnumber.filter(|x| *x > 0.0) {
                        lines.push(label("A", format!("f/{x:.1}")));
                    }
                    if let Some(x) = e.iso.filter(|x| *x > 0) {
                        lines.push(label("I", format!("{x}")));
                    }
                    if let Some(x) = e.bias {
                        lines.push(label("EV", format_ev(x)));
                    }
                    if let Some(x) = e.focal.filter(|x| *x > 0.0) {
                        lines.push(label("", format!("{x:.0}mm")));
                    }
                    lines.push(Line::from(""));
                    if !e.lens.is_empty() {
                        lines.push(Line::from(Span::styled(
                            e.lens.clone(),
                            Style::default().fg(self.config.colors.muted),
                        )));
                    }
                    if !e.camera.is_empty() {
                        lines.push(Line::from(Span::styled(
                            e.camera.clone(),
                            Style::default().fg(self.config.colors.muted),
                        )));
                    }
                } else {
                    // One row across, under a landscape frame.
                    lines.push(Line::from(Span::styled(
                        e.line(),
                        Style::default().fg(self.config.colors.accent),
                    )));
                    let mut tail = String::new();
                    if !e.lens.is_empty() {
                        tail.push_str(&e.lens);
                    }
                    if !e.camera.is_empty() {
                        if !tail.is_empty() {
                            tail.push_str("   ");
                        }
                        tail.push_str(&e.camera);
                    }
                    if !tail.is_empty() {
                        lines.push(Line::from(Span::styled(
                            tail,
                            Style::default().fg(self.config.colors.muted),
                        )));
                    }
                }
            }
            if let Some(path_line) = path_line {
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                lines.push(path_line);
            }
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        }
    }

    #[allow(
        clippy::unused_self,
        reason = "a render method; grouping it with the others is the point"
    )]
    fn overlay(&self, f: &mut Frame, title: &str, body: &str) {
        let area = centered(68, 98, f.area());
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(body).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.config.colors.focus))
                    .title(title),
            ),
            area,
        );
    }
}

const HELP: &str = "\
 j k  up  down        move
 g G  home end        first / last of the WHOLE result, not just this page
 page-up page-down    one screen  (crossing to the next/previous page of
 ctrl-u ctrl-d        half a screen   results automatically at the edge)
 space                select / deselect
 v                    range: press once to start, again to close
 esc                  clear selection (outside single view)

 /                    query  (same syntax as `npc find`)
 t  T                 add / remove tags (comma separated)
 i                    set title
 1-5                  rate    0  clear rating
 f  x  c              flag / reject / clear marks

 while typing (/, t, T, i): left/right home/end to move the cursor,
 backspace/delete, ctrl-a/e/b/f/k/u/w -- the usual readline keys

 enter                zoom in   (-> single view)
 esc                  zoom out  (single -> grid)
 tab / shift-tab      cycle views forward / backward
 left right           focus / leave the sidebar
 s                    sidebar: tags <-> dates
 enter                (sidebar focused) filter by the highlighted entry
 p                    focus / leave the timeline
 [ ]                  (timeline focused) coarser / finer period (day..year)
 left right           (timeline focused) move the highlighted period
 enter                (timeline focused) filter by the highlighted period
 h l                  previous / next  (single view)
 o                    cycle which file of the stack is shown
 - +                  fewer / more tiles across (grid view)
 - +                  crop out / in past full res (single view; arrows pan)

 a                    actions menu
 r                    re-send images if one renders blank (scrolling/resizing
                      already do this on their own where it's needed)
 y                    copy the full path of the current file to the clipboard
 w                    flush pending sidecars
 u                    undo last edit
 q                    quit

Edits are written to <base>.xmp immediately. Original images are
never opened for writing.";

/// Every timeline bar already uses `bar`; inverting bar-on-default to
/// black-on-bar for the cursor just swaps which half of the same hue is
/// filled, which reads as another bar, not a cursor. `accent` is this app's
/// "here" color elsewhere (the list's selection mark, the offline badge), so
/// it's the one color a bar chart in `bar` is guaranteed not to already show.
fn timeline_bar_style(is_cursor: bool, bar: Color, accent: Color) -> Style {
    if is_cursor {
        Style::default().fg(Color::Black).bg(accent)
    } else {
        Style::default().fg(bar)
    }
}

/// Copies `text` to the system clipboard via OSC 52 -- the terminal-level
/// clipboard escape sequence kitty, iTerm2, alacritty, wezterm and others
/// already implement, so no clipboard crate is needed for one key's worth of
/// a feature. Writes straight to stdout alongside ratatui's own output,
/// the same way `scan.rs`'s neighbors already send raw escape sequences
/// (`EnterAlternateScreen` etc.) through `crossterm::execute!`; an
/// out-of-band control sequence like this one does not disturb the cell
/// buffer ratatui is otherwise managing.
fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::io::Write;
    write!(
        std::io::stdout(),
        "\x1b]52;c;{}\x07",
        base64_encode(text.as_bytes())
    )?;
    std::io::stdout().flush()?;
    Ok(())
}

/// A minimal standard-alphabet, padded base64 encoder. OSC 52's payload is
/// the only thing in this project that needs one; pulling in a crate for a
/// dozen lines of bit arithmetic would be one more dependency than the
/// feature is worth.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0b11) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        out.push(match b1 {
            Some(b1) => ALPHABET[(((b1 & 0b1111) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char,
            None => '=',
        });
        out.push(match b2 {
            Some(b2) => ALPHABET[(b2 & 0b11_1111) as usize] as char,
            None => '=',
        });
    }
    out
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
/// a wrecked tty because of a bug in a photo cataloger is not acceptable.
pub fn run(root: PathBuf, trash_view: TrashView, no_watch: bool) -> Result<()> {
    let mut app = App::new(root, trash_view)?;
    if no_watch {
        app.config.watch.enabled = false;
    }

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;

    // Without this, `Ctrl+1`..`Ctrl+5` (the rating-filter shortcut) arrive
    // indistinguishable from a bare `1`..`5` on most terminals: there is no
    // legacy escape sequence for Ctrl+digit the way there is for
    // Ctrl+letter, so the modifier is simply lost. Only terminals
    // implementing this protocol (kitty among them) can report it at all,
    // hence the capability check rather than assuming it.
    let keyboard_enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if keyboard_enhanced {
        crossterm::execute!(
            stdout,
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )?;
    }

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if keyboard_enhanced {
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::event::PopKeyboardEnhancementFlags
            );
        }
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
                match event::read()? {
                    Event::Key(key) => {
                        if key.kind == event::KeyEventKind::Press {
                            app.on_key(key)?;
                        }
                    }
                    // A resize can change the terminal's cell/font geometry,
                    // which any cached image protocol was built against --
                    // see `App::on_resize`.
                    Event::Resize(_, _) => app.on_resize(),
                    _ => {}
                }
            }
            // Drain whatever the generator finished; each one is a reason to
            // repaint, and draining keeps the channel from growing.
            if let Some(cache) = app.cache.as_ref() {
                while cache.done.try_recv().is_ok() {}
            }
            // The watcher runs on its own thread and its own catalog
            // connection; draining here is what turns "a directory was just
            // rescanned" into "the screen reflects it," same shape as the
            // thumbnail cache's `done` channel just above. One `reload()`
            // covers however many directories settled between frames.
            if let Some(watcher) = app.watcher.as_ref() {
                let mut rescanned = false;
                loop {
                    match watcher.changed.try_recv() {
                        Ok(watch::WatchEvent::Rescanned(_)) => rescanned = true,
                        Ok(watch::WatchEvent::Error(e)) => app.status = format!("watch: {e}"),
                        Err(_) => break,
                    }
                }
                if rescanned {
                    let _ = app.reload();
                }
            }
            if app.quit {
                break;
            }
        }
        Ok(())
    })();

    if keyboard_enhanced {
        crossterm::execute!(
            terminal.backend_mut(),
            crossterm::event::PopKeyboardEnhancementFlags
        )?;
    }
    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cursor bar used to be black-on-cyan against cyan bars -- a
    /// same-hue highlight that reads as just another bar, not a cursor. This
    /// must hold for whatever colors the config supplies, not just the
    /// defaults -- a `bar` and `accent` set to the same color would recreate
    /// the exact bug, so that's the case worth pinning here.
    #[test]
    fn the_timeline_cursor_is_not_just_another_bar_of_the_same_color() {
        let (bar, accent) = (Color::Cyan, Color::Yellow);
        let cursor = timeline_bar_style(true, bar, accent);
        let other = timeline_bar_style(false, bar, accent);
        assert_eq!(cursor.bg, Some(accent));
        assert_eq!(other.fg, Some(bar));
        assert_ne!(
            cursor.bg, other.bg,
            "cursor and non-cursor bars must not share a background"
        );
        assert_ne!(
            (cursor.fg, cursor.bg),
            (other.fg, other.bg),
            "the cursor style must be visually distinct from a plain bar"
        );
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
        // Events derive from capture time, which the fixture stamps after the
        // scan, so they have to be recomputed here.
        crate::scan::derive_events(&mut cat).unwrap();
        let app = App::with_catalog(cat, dir.path().to_path_buf(), TrashView::Exclude).unwrap();
        (dir, app)
    }

    fn press(app: &mut App, c: char) {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
    }
    fn press_code(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
    }
    fn press_ctrl(app: &mut App, c: char) {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
            .unwrap();
    }
    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, c);
        }
        press_code(app, KeyCode::Enter);
    }
    /// Type without committing, so mid-edit state (buffer, input_cursor) can
    /// be inspected before `Enter`.
    fn type_str_uncommitted(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, c);
        }
    }

    #[test]
    fn opens_showing_the_whole_library() {
        let (_d, app) = app();
        assert_eq!(app.total, 3);
        assert_eq!(app.rows.len(), 3);
        assert_eq!(app.cursor, 0);
    }

    /// Marks a stack trashed exactly the way `trash::trash` does, without
    /// pulling in the filesystem-moving half of that module -- only the
    /// catalog row this test cares about.
    fn mark_trashed(cat: &Catalog, stack_id: i64) {
        cat.conn
            .execute(
                "INSERT OR REPLACE INTO trashed (stack_id, orig_dir, trashed_at)
                 VALUES (?1, '', 0)",
                rusqlite::params![stack_id],
            )
            .unwrap();
    }

    /// The bug report this exists for: a stack just moved to `.trash/` kept
    /// showing up in the very browser used to find and trash duplicates,
    /// making it look like nothing had happened.
    #[test]
    fn trashed_stacks_are_excluded_by_default() {
        let (dir, mut app) = app();
        let id = app
            .cat
            .stack_id(&dir.path().join("2026/08/09").to_string_lossy(), "750_0001")
            .unwrap()
            .expect("750_0001 must be cataloged");
        mark_trashed(&app.cat, id);
        app.reload().unwrap();

        assert_eq!(app.total, 2, "the trashed stack must not be counted");
        assert!(
            app.rows.iter().all(|r| r.id != id),
            "the trashed stack must not appear in the rows"
        );
    }

    #[test]
    fn include_shows_trashed_stacks_alongside_the_rest() {
        let (dir, mut app) = app();
        let id = app
            .cat
            .stack_id(&dir.path().join("2026/08/09").to_string_lossy(), "750_0001")
            .unwrap()
            .expect("750_0001 must be cataloged");
        mark_trashed(&app.cat, id);
        app.trash_view = TrashView::Include;
        app.reload().unwrap();

        assert_eq!(app.total, 3, "Include must count everything");
        assert!(app.rows.iter().any(|r| r.id == id));
    }

    #[test]
    fn trash_only_shows_nothing_but_the_trash() {
        let (dir, mut app) = app();
        let id = app
            .cat
            .stack_id(&dir.path().join("2026/08/09").to_string_lossy(), "750_0001")
            .unwrap()
            .expect("750_0001 must be cataloged");
        mark_trashed(&app.cat, id);
        app.trash_view = TrashView::Only;
        app.reload().unwrap();

        assert_eq!(app.total, 1);
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.rows[0].id, id);
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

    /// The point of readline support: fixing a typo in the middle of a
    /// prefilled query without deleting everything after it.
    #[test]
    fn the_query_prompt_supports_in_place_cursor_editing() {
        let (_d, mut app) = app();
        press(&mut app, '/');
        type_str_uncommitted(&mut app, "beah");
        assert_eq!(app.input_cursor, 4);

        // Move left past the trailing 'h', insert the missing 'c'.
        press_code(&mut app, KeyCode::Left);
        press(&mut app, 'c');
        assert_eq!(app.buffer, "beach");
        assert_eq!(app.input_cursor, 4, "cursor sits after the inserted char");

        press_code(&mut app, KeyCode::End);
        assert_eq!(app.input_cursor, 5);
        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.query, "beach");
    }

    #[test]
    fn ctrl_a_and_ctrl_e_reach_the_ends() {
        let (_d, mut app) = app();
        press(&mut app, '/');
        type_str_uncommitted(&mut app, "beach");
        press_ctrl(&mut app, 'a');
        assert_eq!(app.input_cursor, 0);
        press_ctrl(&mut app, 'e');
        assert_eq!(app.input_cursor, 5);
    }

    #[test]
    fn ctrl_k_kills_to_end_of_line() {
        let (_d, mut app) = app();
        press(&mut app, '/');
        type_str_uncommitted(&mut app, "beach anna");
        for _ in 0..4 {
            press_code(&mut app, KeyCode::Left); // cursor after "beach "
        }
        press_ctrl(&mut app, 'k');
        assert_eq!(app.buffer, "beach ");
    }

    #[test]
    fn ctrl_u_kills_to_start_of_line() {
        let (_d, mut app) = app();
        press(&mut app, '/');
        type_str_uncommitted(&mut app, "beach anna");
        for _ in 0..4 {
            press_code(&mut app, KeyCode::Left); // cursor before "anna"
        }
        press_ctrl(&mut app, 'u');
        assert_eq!(app.buffer, "anna");
        assert_eq!(app.input_cursor, 0);
    }

    #[test]
    fn ctrl_w_deletes_the_word_before_the_cursor() {
        let (_d, mut app) = app();
        press(&mut app, '/');
        type_str_uncommitted(&mut app, "beach anna");
        press_ctrl(&mut app, 'w');
        assert_eq!(app.buffer, "beach ");

        // Trailing whitespace before the cursor is skipped too.
        press_ctrl(&mut app, 'w');
        assert_eq!(app.buffer, "");
    }

    #[test]
    fn backspace_and_delete_remove_from_the_right_side_of_the_cursor() {
        let (_d, mut app) = app();
        press(&mut app, '/');
        type_str_uncommitted(&mut app, "beach");
        press_code(&mut app, KeyCode::Left); // cursor between 'c' and 'h'
        press_code(&mut app, KeyCode::Backspace);
        assert_eq!(app.buffer, "beah", "backspace removes before the cursor");

        press_code(&mut app, KeyCode::Delete);
        assert_eq!(app.buffer, "bea", "delete removes at the cursor");
    }

    /// Readline editing is shared code (`on_input_key`) -- confirm it behaves
    /// the same for a non-query input, not just `/`.
    #[test]
    fn readline_editing_also_works_for_the_tag_prompt() {
        let (_d, mut app) = app();
        press(&mut app, 't');
        type_str_uncommitted(&mut app, "beah");
        press_code(&mut app, KeyCode::Left);
        press(&mut app, 'c');
        assert_eq!(app.buffer, "beach");
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
            "events sidebar should list the shooting day: {:?}",
            app.sidebar_items
        );

        press_code(&mut app, KeyCode::Left); // focus sidebar
        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.query, "event=\"2026-08-09\"");
        assert_eq!(app.total, 3);
    }

    /// Down/Up used to move the main cursor unconditionally, then move
    /// `sidebar_cursor` too in the sidebar-focused block below it -- so
    /// browsing the sidebar silently changed the grid selection underneath.
    #[test]
    fn arrows_move_only_the_sidebar_while_it_has_focus() {
        let (_d, mut app) = app_multi_month();
        press(&mut app, 's'); // dates sidebar, which has three entries here
        assert!(
            app.sidebar_items.len() > 1,
            "need more than one entry to see the cursor move: {:?}",
            app.sidebar_items
        );
        press_code(&mut app, KeyCode::Left); // focus sidebar
        let cursor_before = app.cursor;
        let sidebar_cursor_before = app.sidebar_cursor;

        press_code(&mut app, KeyCode::Down);
        assert_eq!(
            app.cursor, cursor_before,
            "main selection moved while the sidebar had focus"
        );
        assert_ne!(
            app.sidebar_cursor, sidebar_cursor_before,
            "sidebar cursor should advance on Down"
        );
    }

    /// Same 3-stack fixture as `app()`, but spread across three different
    /// months (2026-05-01, 2026-06-30, 2026-08-09) so timeline
    /// navigation has more than one bucket to move across.
    fn app_multi_month() -> (tempfile::TempDir, App) {
        let (dir, mut app) = app();
        for (n, t) in (1..=3).zip([1_777_593_666i64, 1_782_777_666, 1_786_233_666]) {
            app.cat
                .conn
                .execute(
                    "UPDATE file SET capture_time = ?1 WHERE name LIKE ?2",
                    rusqlite::params![t, format!("750_000{n}%")],
                )
                .unwrap();
        }
        crate::scan::derive_events(&mut app.cat).unwrap();
        app.reload().unwrap();
        (dir, app)
    }

    #[test]
    fn focusing_the_timeline_clears_sidebar_focus_and_vice_versa() {
        let (_d, mut app) = app();
        press_code(&mut app, KeyCode::Left); // focus sidebar
        assert!(app.sidebar_focus);

        press(&mut app, 'p');
        assert!(app.timeline_focus);
        assert!(
            !app.sidebar_focus,
            "entering the timeline should leave the sidebar"
        );

        press_code(&mut app, KeyCode::Left); // now a timeline move, not sidebar focus
        assert!(
            app.timeline_focus,
            "still timeline-focused, not toggled back to sidebar"
        );
        assert!(!app.sidebar_focus);

        press(&mut app, 'p');
        assert!(!app.timeline_focus);
        press_code(&mut app, KeyCode::Left); // now free to focus the sidebar again
        assert!(app.sidebar_focus);
        assert!(!app.timeline_focus);
    }

    #[test]
    fn bracket_keys_cycle_granularity_one_step_at_a_time_and_clamp_at_the_ends() {
        let (_d, mut app) = app();
        assert_eq!(app.granularity, Granularity::Month, "documented default");
        press(&mut app, 'p');

        press(&mut app, ']');
        assert_eq!(app.granularity, Granularity::Year);
        press(&mut app, ']');
        assert_eq!(
            app.granularity,
            Granularity::Year,
            "clamps, does not wrap to Day"
        );

        press(&mut app, '[');
        assert_eq!(app.granularity, Granularity::Month);
        press(&mut app, '[');
        assert_eq!(app.granularity, Granularity::Week);
        press(&mut app, '[');
        assert_eq!(app.granularity, Granularity::Day);
        press(&mut app, '[');
        assert_eq!(
            app.granularity,
            Granularity::Day,
            "clamps, does not wrap to Year"
        );
    }

    #[test]
    fn bracket_keys_do_nothing_when_the_timeline_is_not_focused() {
        let (_d, mut app) = app();
        press(&mut app, ']');
        assert_eq!(
            app.granularity,
            Granularity::Month,
            "unchanged: not focused"
        );
    }

    #[test]
    fn enter_on_a_highlighted_bucket_filters_like_the_sidebar_does() {
        let (_d, mut app) = app();
        press(&mut app, 'p');
        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.query, "date:2026-08");
        assert_eq!(app.total, 3);
    }

    /// Regression: picking a bucket used to overwrite `query` outright, so a
    /// filter added afterwards via `/` (e.g. `rating>=4`) vanished the moment
    /// a second bucket was picked. `splice_filter` must replace only the
    /// fragment it inserted last time.
    #[test]
    fn a_manually_added_filter_survives_picking_a_second_bucket() {
        let (_d, mut app) = app_multi_month();
        press(&mut app, 'p');
        press_code(&mut app, KeyCode::Enter); // pick the newest bucket
        let first_filter = app.query.clone();

        press(&mut app, '/');
        type_str(&mut app, " and rating>=4");
        assert_eq!(app.query, format!("{first_filter} and rating>=4"));

        press_code(&mut app, KeyCode::Left); // still timeline-focused: move to another bucket
        press_code(&mut app, KeyCode::Enter); // pick it
        assert!(
            app.query.contains("rating>=4"),
            "manually added filter should survive picking a different bucket: {}",
            app.query
        );
        assert_ne!(
            app.query, first_filter,
            "the date fragment should have moved to the newly picked bucket"
        );
    }

    #[test]
    fn left_right_move_the_timeline_cursor_across_buckets() {
        let (_d, mut app) = app_multi_month();
        assert_eq!(app.timeline.len(), 3, "three distinct months");
        assert_eq!(app.timeline_cursor, 2, "defaults to the most recent bucket");

        press(&mut app, 'p');
        press_code(&mut app, KeyCode::Left);
        assert_eq!(app.timeline_cursor, 1);
        press_code(&mut app, KeyCode::Left);
        assert_eq!(app.timeline_cursor, 0);
        press_code(&mut app, KeyCode::Left);
        assert_eq!(app.timeline_cursor, 0, "clamped at the oldest bucket");

        press_code(&mut app, KeyCode::Right);
        press_code(&mut app, KeyCode::Right);
        press_code(&mut app, KeyCode::Right);
        assert_eq!(app.timeline_cursor, 2, "clamped at the newest bucket");
    }

    /// `reload_timeline()` runs on every `reload()` -- tagging, filtering,
    /// anything -- and none of those change the timeline's own whole-library
    /// data. Losing the cursor on every such reload (jumping back to the
    /// most recent bucket) is exactly what leaving the timeline and coming
    /// back looked like before this was fixed.
    #[test]
    fn the_selected_bucket_survives_an_unrelated_reload() {
        let (_d, mut app) = app_multi_month();
        press(&mut app, 'p');
        press_code(&mut app, KeyCode::Left); // move off the default (most recent)
        assert_eq!(app.timeline_cursor, 1);

        // Anything that calls reload() but has nothing to do with the
        // timeline -- tagging, filtering, editing -- all go through this.
        app.reload().unwrap();

        assert_eq!(
            app.timeline_cursor, 1,
            "cursor should stay on the same bucket across an unrelated reload"
        );
    }

    /// The one granularity `query.rs` has no native single-value syntax for:
    /// confirms the emitted `date:start..end` fragment actually covers the
    /// real Monday-Sunday week the fixture's capture time falls in (2026-08-09
    /// is a Sunday, so its week is 2026-08-03..2026-08-09) and nothing else.
    #[test]
    fn week_bucketing_filters_the_right_seven_days() {
        let (_d, mut app) = app();
        press(&mut app, 'p');
        press(&mut app, '['); // Month -> Week
        assert_eq!(app.granularity, Granularity::Week);
        assert_eq!(app.timeline.len(), 1);
        assert_eq!(app.timeline[0].filter, "date:2026-08-03..2026-08-09");

        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.total, 3, "all three stacks fall in that week");
    }

    /// Undated stacks must remain reachable rather than silently missing from
    /// the events pane.
    #[test]
    fn undated_stacks_get_their_own_bucket() {
        let (_d, mut app) = app();
        app.cat
            .conn
            .execute("UPDATE file SET capture_time = NULL", [])
            .unwrap();
        crate::scan::derive_events(&mut app.cat).unwrap();
        press(&mut app, 's');
        assert!(
            app.sidebar_items
                .iter()
                .any(|(d, n)| d == "undated" && *n == 3),
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
    fn the_timeline_renders_and_shows_focus_with_a_marker() {
        let (_d, mut app) = app();
        let unfocused = render(&mut app, 120, 24);
        assert!(
            unfocused.contains("timeline (month)"),
            "timeline header missing:\n{unfocused}"
        );
        assert!(
            unfocused.contains("2026-08"),
            "bucket label missing:\n{unfocused}"
        );
        assert!(unfocused.contains('█'), "bar missing:\n{unfocused}");
        assert!(
            !unfocused.contains('\u{25b8}'),
            "focus marker shown while unfocused:\n{unfocused}"
        );

        press(&mut app, 'p');
        let focused = render(&mut app, 120, 24);
        assert!(
            focused.contains('\u{25b8}'),
            "focus marker missing while focused:\n{focused}"
        );
    }

    /// The whole point of the ticks row: every bucket's label must be
    /// readable without first moving the cursor there, not just the
    /// currently-selected one shown in the header.
    #[test]
    fn the_timeline_shows_an_x_axis_label_under_each_visible_bucket() {
        let (_d, mut app) = app_multi_month();
        let screen = render(&mut app, 120, 24);
        for label in ["2026-05", "2026-06", "2026-08"] {
            assert!(
                screen.contains(label),
                "bucket label {label} missing from the ticks row:\n{screen}"
            );
        }
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

    /// The black-frame bug: a cached protocol was reused even after its path
    /// moved to a new screen rect, which is what leaves a stale kitty
    /// placement behind. The fix rebuilds the protocol on a rect change, but
    /// must not need the source file again to do it -- `decoded` carries the
    /// pixels across that rebuild. `Picker::from_fontsize` builds a real
    /// picker without a terminal, so this exercises the real decode path,
    /// not a stub.
    #[test]
    fn a_rect_change_rebuilds_the_protocol_without_touching_disk_again() {
        let (_d, mut app) = app();
        let src_dir = tempfile::tempdir().unwrap();
        let path = src_dir.path().join("thumb.png");
        image::RgbImage::new(2, 2).save(&path).unwrap();
        app.picker = Some(Picker::from_fontsize((8, 16)));

        let rect_a = Rect::new(0, 0, 10, 10);
        assert!(
            app.ensure_decoded_at(&path, rect_a),
            "first decode of a real file must succeed"
        );

        // Delete the source: from here on, any read from disk fails. If
        // every call below still succeeds, none of them touched the file.
        std::fs::remove_file(&path).unwrap();

        assert!(
            app.ensure_decoded_at(&path, rect_a),
            "same rect must reuse the cached protocol outright"
        );

        let rect_b = Rect::new(0, 0, 20, 20);
        assert!(
            app.ensure_decoded_at(&path, rect_b),
            "a different rect must rebuild the protocol from the decoded-pixel \
             cache, not by re-reading the now-deleted file"
        );
    }

    /// The other half of the split: once the decoded pixels themselves are
    /// gone (evicted because the tile scrolled off screen, say), a rect
    /// change has nothing to rebuild from and must fall back to disk --
    /// proving `decoded`, not wishful thinking, is what made the case above
    /// work.
    #[test]
    fn a_rect_change_falls_back_to_disk_once_the_decoded_pixels_are_gone() {
        let (_d, mut app) = app();
        let src_dir = tempfile::tempdir().unwrap();
        let path = src_dir.path().join("thumb.png");
        image::RgbImage::new(2, 2).save(&path).unwrap();
        app.picker = Some(Picker::from_fontsize((8, 16)));

        assert!(app.ensure_decoded_at(&path, Rect::new(0, 0, 10, 10)));
        app.decoded.remove(&path);
        std::fs::remove_file(&path).unwrap();

        assert!(
            !app.ensure_decoded_at(&path, Rect::new(0, 0, 20, 20)),
            "with neither the decoded cache nor the file available, this must fail"
        );
    }

    #[test]
    fn resize_clears_every_cached_image() {
        let (_d, mut app) = app();
        let src_dir = tempfile::tempdir().unwrap();
        let path = src_dir.path().join("thumb.png");
        image::RgbImage::new(2, 2).save(&path).unwrap();
        app.picker = Some(Picker::from_fontsize((8, 16)));

        assert!(app.ensure_decoded_at(&path, Rect::new(0, 0, 10, 10)));
        assert_eq!(app.images.len(), 1);
        assert_eq!(app.last_rect.len(), 1);

        app.on_resize();
        assert!(app.images.is_empty(), "resize must drop every cached image");
        assert!(app.last_rect.is_empty());
        assert_eq!(
            app.decoded.len(),
            1,
            "a resize invalidates screen placement, not the decoded pixels"
        );
    }

    #[test]
    fn tab_cycles_forward_shift_tab_backward_and_arrows_reach_the_sidebar() {
        let (_d, mut app) = app();
        assert_eq!(app.view, View::List);
        press_code(&mut app, KeyCode::Tab);
        assert_eq!(app.view, View::Grid);
        press_code(&mut app, KeyCode::Tab);
        assert_eq!(app.view, View::Single);
        press_code(&mut app, KeyCode::Tab);
        assert_eq!(app.view, View::List);

        // ..and backwards, so a two-key detour is never needed.
        press_code(&mut app, KeyCode::BackTab);
        assert_eq!(app.view, View::Single);
        press_code(&mut app, KeyCode::BackTab);
        assert_eq!(app.view, View::Grid);
        press_code(&mut app, KeyCode::BackTab);
        assert_eq!(app.view, View::List);

        assert!(!app.sidebar_focus);
        press_code(&mut app, KeyCode::Left);
        assert!(app.sidebar_focus);
        press_code(&mut app, KeyCode::Right);
        assert!(!app.sidebar_focus);
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

    /// `Ctrl+N` toggles a rating checkbox and filters; plain `N` still rates
    /// the cursor row, unaffected -- the two must never both fire from the
    /// same keypress.
    #[test]
    fn ctrl_digit_toggles_a_rating_filter_independently_of_rating_the_cursor() {
        let (_dir, mut app) = app();
        press(&mut app, '2'); // rate row 0
        press_code(&mut app, KeyCode::Down);
        press(&mut app, '5'); // rate row 1
        assert_eq!(app.rows.len(), 3, "all 3 stacks visible with no filter");

        press_ctrl(&mut app, '2');
        assert!(app.rating_filter[1]);
        assert_eq!(app.query, "rating=2", "spliced into the visible query");
        assert_eq!(app.rows.len(), 1, "only the rating=2 stack should show");
        assert_eq!(app.rows[0].rating, Some(2));

        press_ctrl(&mut app, '5');
        assert_eq!(app.query, "(rating=2 or rating=5)");
        assert_eq!(
            app.rows.len(),
            2,
            "rating=2 or rating=5 should both show once both are checked"
        );

        press_ctrl(&mut app, '2');
        assert!(!app.rating_filter[1]);
        assert_eq!(app.query, "rating=5");
        assert_eq!(app.rows.len(), 1, "only the rating=5 stack should remain");
        assert_eq!(app.rows[0].rating, Some(5));

        press_ctrl(&mut app, '5');
        assert_eq!(app.query, "", "clearing every box clears the fragment too");
        assert_eq!(
            app.rows.len(),
            3,
            "clearing every box shows everything again"
        );
    }

    /// A rating fragment spliced in by `Ctrl+N` must not clobber, or be
    /// clobbered by, text the user typed by hand -- same coexistence
    /// `splice_filter` already gives a sidebar click alongside `/`.
    #[test]
    fn rating_filter_coexists_with_hand_typed_query_text() {
        let (_dir, mut app) = app();
        press(&mut app, '2');
        press_code(&mut app, KeyCode::Down);
        press(&mut app, '5');
        press(&mut app, '/');
        type_str(&mut app, "beach");

        press_ctrl(&mut app, '2');
        assert_eq!(app.query, "rating=2 and beach");

        press_ctrl(&mut app, '5');
        assert_eq!(app.query, "(rating=2 or rating=5) and beach");

        press_ctrl(&mut app, '2');
        press_ctrl(&mut app, '5');
        assert_eq!(
            app.query, "beach",
            "clearing every box leaves the hand-typed text untouched"
        );
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
        // The fixture names really are uppercase -- this is asserting the
        // fixture's own naming, not classifying a real extension case-sensitively.
        #[allow(
            clippy::case_sensitive_file_extension_comparisons,
            reason = "checking the fixture's literal name, not classifying a role"
        )]
        {
            assert!(first.ends_with(".NEF") || first.ends_with(".JPG"));
        }
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

    /// `y` copies this, not `current_label`'s root-relative one -- "where is
    /// this on disk" needs the real path, same reasoning as the single
    /// view's info panel.
    #[test]
    fn current_full_path_is_absolute() {
        let (dir, app) = app();
        let path = app.current_full_path().unwrap();
        assert!(path.starts_with(dir.path().to_str().unwrap()), "got {path}");
        assert!(path.contains("750_0001"));
    }

    #[test]
    fn y_copies_the_full_path_and_reports_it_in_the_status() {
        let (_d, mut app) = app();
        press(&mut app, 'y');
        assert!(app.status.starts_with("copied "), "got {}", app.status);
        assert!(app.status.contains("750_0001"));
    }

    #[test]
    fn base64_encode_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
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
    fn the_status_area_shows_the_exposure_data() {
        let (_d, mut app) = app();
        app.cat
            .conn
            .execute(
                "UPDATE file SET exposure = 0.0025, fnumber = 4.5, iso = 2500,
                                 exposure_bias = -0.6666667, focal = 200.0",
                [],
            )
            .unwrap();
        let screen = render(&mut app, 160, 30);
        for want in ["S 1/400", "A f/4.5", "I 2500", "EV -0.7"] {
            assert!(screen.contains(want), "`{want}` missing:\n{screen}");
        }
    }

    /// The single view's info panel is where "where is this on disk" lives
    /// (room for a full path, unlike the width-constrained status line) --
    /// shown even when there's no EXIF to go with it.
    #[test]
    fn the_single_view_panel_shows_the_full_path() {
        let (_d, mut app) = app();
        while app.view != View::Single {
            press_code(&mut app, KeyCode::Tab);
        }
        let screen = render(&mut app, 120, 40);
        let path = app.current_full_path().unwrap();
        assert!(
            screen.contains(&path),
            "panel missing the full path:\n{screen}"
        );
    }

    /// Enter and Esc are a zoom pair: one key each way, rather than cycling
    /// through an unwanted view to get back.
    #[test]
    fn enter_zooms_in_and_esc_zooms_back_to_the_grid() {
        let (_d, mut app) = app();
        assert_eq!(app.view, View::List);

        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.view, View::Single);

        press_code(&mut app, KeyCode::Esc);
        assert_eq!(app.view, View::Grid, "one key must get back to the grid");

        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.view, View::Single);
    }

    /// `+`/`-` in single view toggle the decode tier, distinct from the same
    /// keys resizing the grid -- the two must not interfere with each other.
    #[test]
    fn plus_minus_toggle_full_resolution_in_single_view_only() {
        let (_d, mut app) = app();
        let cols_before = app.grid_cols;

        press_code(&mut app, KeyCode::Enter); // -> single view
        assert!(!app.single_zoom);

        press(&mut app, '+');
        assert!(app.single_zoom, "+ must switch to full resolution");
        assert_eq!(app.grid_cols, cols_before, "must not also resize the grid");

        press(&mut app, '-');
        assert!(!app.single_zoom, "- must switch back to the preview tier");

        press_code(&mut app, KeyCode::Esc); // -> grid view
        press(&mut app, '+');
        assert_eq!(
            app.grid_cols,
            cols_before + 1,
            "+ in grid view must resize the grid, not touch single_zoom"
        );
    }

    /// A second `+` (and beyond) crops in further rather than doing nothing
    /// -- the whole point of asking for more than the plain toggle.
    #[test]
    fn further_plus_presses_crop_in_and_minus_backs_out_before_leaving_zoom() {
        let (_d, mut app) = app();
        press_code(&mut app, KeyCode::Enter);

        press(&mut app, '+'); // enter zoom mode: level 1.0
        assert!((app.zoom_level - 1.0).abs() < 1e-6);
        press(&mut app, '+'); // crop in
        assert!(app.single_zoom);
        assert!(
            (app.zoom_level - ZOOM_STEP).abs() < 1e-6,
            "expected {ZOOM_STEP}, got {}",
            app.zoom_level
        );

        press(&mut app, '-'); // back to level 1.0
        assert!(app.single_zoom, "must still be zoomed, just at level 1.0");
        assert!((app.zoom_level - 1.0).abs() < 1e-6);

        press(&mut app, '-'); // and now leave zoom mode entirely
        assert!(!app.single_zoom);
        assert!((app.zoom_level - 1.0).abs() < 1e-6);
    }

    #[test]
    fn zoom_level_is_clamped_at_the_configured_maximum() {
        let (_d, mut app) = app();
        press_code(&mut app, KeyCode::Enter);
        for _ in 0..30 {
            press(&mut app, '+');
        }
        assert!(app.zoom_level <= ZOOM_MAX);
    }

    /// Arrows pan the crop while zoomed; j/k must still move the cursor
    /// exactly like they do everywhere else, since only the arrows changed
    /// meaning.
    #[test]
    fn arrows_pan_while_zoomed_but_j_k_still_move_the_cursor() {
        let (_d, mut app) = app();
        press_code(&mut app, KeyCode::Enter);
        press(&mut app, '+');
        press(&mut app, '+'); // level > 1.0, so there is room to pan

        let center_before = app.zoom_center;
        press_code(&mut app, KeyCode::Right);
        assert!(
            app.zoom_center.0 > center_before.0,
            "Right must pan toward higher x, not move the cursor"
        );
        assert_eq!(app.cursor, 0, "the cursor must not have moved");

        press_code(&mut app, KeyCode::Left);
        press_code(&mut app, KeyCode::Left);
        assert!(app.zoom_center.0 < center_before.0);

        let cursor_before = app.cursor;
        press(&mut app, 'j');
        assert_eq!(
            app.cursor,
            cursor_before + 1,
            "j must still move the cursor while zoomed"
        );
    }

    /// The crop can be panned to any edge of the image, but never past one --
    /// `clamp_zoom_center` is what makes "reach every area" also mean
    /// "never point the crop partway off the picture".
    #[test]
    fn panning_never_pushes_the_crop_past_the_image_edge() {
        let (_d, mut app) = app();
        press_code(&mut app, KeyCode::Enter);
        press(&mut app, '+');
        press(&mut app, '+'); // zoom_level == ZOOM_STEP, half-width = 0.5/ZOOM_STEP

        for _ in 0..50 {
            press_code(&mut app, KeyCode::Right);
            press_code(&mut app, KeyCode::Down);
        }
        let half = 0.5 / app.zoom_level;
        assert!((app.zoom_center.0 - (1.0 - half)).abs() < 1e-4);
        assert!((app.zoom_center.1 - (1.0 - half)).abs() < 1e-4);

        for _ in 0..50 {
            press_code(&mut app, KeyCode::Left);
            press_code(&mut app, KeyCode::Up);
        }
        assert!((app.zoom_center.0 - half).abs() < 1e-4);
        assert!((app.zoom_center.1 - half).abs() < 1e-4);
    }

    /// Panning is a no-op at zoom_level 1.0: the crop is already the whole
    /// image, so there is nowhere for the center to go.
    #[test]
    fn panning_does_nothing_at_the_unzoomed_level() {
        let (_d, mut app) = app();
        press_code(&mut app, KeyCode::Enter);
        press(&mut app, '+'); // level 1.0
        press_code(&mut app, KeyCode::Right);
        press_code(&mut app, KeyCode::Down);
        assert_eq!(app.zoom_center, (0.5, 0.5));
    }

    /// Leaving single view, or moving to a different image within it, must
    /// not carry a zoom/pan state that was framed for a different picture.
    #[test]
    fn zoom_resets_on_navigation_away_from_the_current_image() {
        let (_d, mut app) = app();
        press_code(&mut app, KeyCode::Enter);
        press(&mut app, '+');
        press(&mut app, '+');
        assert!(app.single_zoom);

        press_code(&mut app, KeyCode::Esc); // -> grid
        assert!(!app.single_zoom);
        assert!((app.zoom_level - 1.0).abs() < 1e-6);

        press_code(&mut app, KeyCode::Enter); // -> single, fresh
        press(&mut app, '+');
        press(&mut app, '+');
        press(&mut app, 'l'); // next image
        assert!(!app.single_zoom, "l must reset zoom for the new image");
        assert!((app.zoom_level - 1.0).abs() < 1e-6);
    }

    /// ..but Esc must still clear a selection where there is nothing to back
    /// out of, which is what it does everywhere else.
    #[test]
    fn esc_still_clears_selection_outside_the_single_view() {
        let (_d, mut app) = app();
        press(&mut app, ' ');
        press(&mut app, ' ');
        assert_eq!(app.selected.len(), 2);

        press_code(&mut app, KeyCode::Esc);
        assert!(app.selected.is_empty());
        assert_eq!(app.view, View::List, "Esc changed view outside single");
    }

    /// Enter belongs to the sidebar when the sidebar has focus -- it filters
    /// by the highlighted entry, and must not be stolen by the zoom.
    #[test]
    fn enter_still_filters_when_the_sidebar_has_focus() {
        let (_d, mut app) = app();
        press(&mut app, 's'); // dates
        press_code(&mut app, KeyCode::Left); // focus sidebar
        press_code(&mut app, KeyCode::Enter);
        assert_eq!(app.view, View::List, "zoom stole the sidebar's Enter");
        assert!(app.query.starts_with("event="), "query: {}", app.query);
    }

    /// Protected roots must gate actions too. The guard stops this program
    /// writing; it cannot stop RawTherapee or an image viewer, so the only
    /// safe thing is not to hand them the file. Learned the hard way: five
    /// JPGs on the protected drive picked up embedded ratings from an external
    /// application while the guard reported everything was fine.
    #[test]
    fn actions_are_refused_inside_a_protected_root() {
        let (dir, mut app) = app();
        app.config = Config {
            protected_roots: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        app.actions = vec![crate::action::Action {
            key: "z".into(),
            name: "Editor".into(),
            cmd: vec!["true".into(), "{raw}".into()],
            mode: crate::action::Mode::Detach,
            shell: false,
        }];

        app.run_action('z').unwrap();
        assert!(
            app.status.contains("protected"),
            "an action was launched against a protected root: {}",
            app.status
        );
    }

    /// Paging must move by whatever a screen actually holds, which only the
    /// draw pass knows -- a fixed stride would page past content in a tall
    /// window and stall in a short one.
    #[test]
    fn page_keys_move_by_a_real_screen() {
        let (_d, mut app) = app();
        // A 12-row frame: 3 for the timeline, 2 for the status strip, 2 more
        // for the list's own border, leaving 5 usable rows.
        let _ = render(&mut app, 100, 12);
        assert_eq!(app.page_size, 5, "page size not taken from the drawn area");

        // With only 3 stacks, a page lands on the last one rather than past it.
        press_code(&mut app, KeyCode::PageDown);
        assert_eq!(app.cursor, 2);
        press_code(&mut app, KeyCode::PageUp);
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn home_and_end_reach_the_ends() {
        let (_d, mut app) = app();
        press_code(&mut app, KeyCode::End);
        assert_eq!(app.cursor, 2);
        press_code(&mut app, KeyCode::Home);
        assert_eq!(app.cursor, 0);
    }

    /// A catalog with more than one SQL page (`PAGE = 500`) of stacks, so a
    /// query result that exceeds the page size is reachable in a test.
    fn app_with_many_stacks(n: usize) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::open_memory().unwrap();
        for i in 0..n {
            cat.conn
                .execute(
                    "INSERT INTO stack (dir, base) VALUES (?1, ?2)",
                    rusqlite::params![dir.path().to_string_lossy(), format!("750_{i:05}")],
                )
                .unwrap();
        }
        let app = App::with_catalog(cat, dir.path().to_path_buf(), TrashView::Exclude).unwrap();
        (dir, app)
    }

    /// The whole point of the fix: nothing past the first `PAGE` (500) rows
    /// was reachable before this, no matter which key you pressed.
    #[test]
    fn g_and_shift_g_reach_the_true_ends_of_a_result_set_spanning_pages() {
        let (_d, mut app) = app_with_many_stacks(520);
        assert_eq!(app.total, 520);
        assert_eq!(app.rows.len(), 500, "first page should be full");

        press(&mut app, 'G');
        assert_eq!(app.offset, 500, "G must land on the second page");
        assert_eq!(app.rows.len(), 20, "remainder of 520 stacks over 500");
        assert_eq!(app.cursor, 19, "cursor on the true last row");

        press(&mut app, 'g');
        assert_eq!(app.offset, 0, "g must return to the first page");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn moving_past_the_last_row_of_a_page_turns_to_the_next_page() {
        let (_d, mut app) = app_with_many_stacks(520);
        press(&mut app, 'G'); // land on the last row of the last page first,
        press(&mut app, 'g'); // then back to the first page, cursor at 0.

        // Walk to the last row of page one without leaving it.
        app.cursor = 499;
        press(&mut app, 'j');
        assert_eq!(app.offset, 500, "moving past row 500 must turn the page");
        assert_eq!(app.cursor, 0, "lands on the first row of the new page");
        assert_eq!(app.rows.len(), 20);
    }

    #[test]
    fn moving_before_the_first_row_of_a_page_turns_back() {
        let (_d, mut app) = app_with_many_stacks(520);
        press(&mut app, 'G');
        assert_eq!(app.offset, 500);
        app.cursor = 0;

        press(&mut app, 'k');
        assert_eq!(app.offset, 0, "moving before row 0 must turn back a page");
        assert_eq!(
            app.cursor, 499,
            "lands on the last row of the previous page"
        );
    }

    /// With a result set that fits on one page (the common case), paging
    /// keys must behave exactly as they did before this change.
    #[test]
    fn small_result_sets_are_unaffected_by_paging() {
        let (_d, mut app) = app();
        press(&mut app, 'G');
        assert_eq!(app.offset, 0);
        assert_eq!(app.cursor, 2);
        press(&mut app, 'j'); // past the last row -- nothing to page to
        assert_eq!(app.offset, 0);
        assert_eq!(app.cursor, 2, "clamped, not left dangling past the end");
        press(&mut app, 'g');
        press(&mut app, 'k'); // before the first row -- nothing to page to
        assert_eq!(app.offset, 0);
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn the_grid_pages_by_tiles_on_screen() {
        let (_d, mut app) = app();
        press_code(&mut app, KeyCode::Tab); // grid
        let _ = render(&mut app, 120, 30);
        assert!(app.page_size >= 1, "grid page size was never set");
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
