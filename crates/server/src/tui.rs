//! `server tui` — a terminal debugger for the whole data pipeline:
//! raw feed JSON → parsed `Game`s → rendered 64×32 frames, side by side.
//!
//! Interactive: three panes (feeds / events / detail tabs), vim-ish keys.
//! Headless:    `server tui --feed nhl --dump [dir]` fetches one feed and
//!              writes raw.json + per-event game.txt + frame PNGs, then exits —
//!              scriptable for agents and CI alike.
//!
//! Every parse goes through `espn::parse_feed`, the same functions the poll
//! loop uses, so what this tool shows is what the server would serve.
//!
//! Full tour — keys, capture layout, debugging recipes: docs/cli.md.

use crate::espn::{self, EspnSource};
use crate::model::{Game, GameStatus};
use crate::render;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs};
use scoreboard_protocol::{SpoilerMode, FRAME_WIDTH};
use std::path::{Path, PathBuf};

/// Which detail tab is showing for the selected event.
#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Raw,
    Game,
    Frame,
}

/// Frame variants the Frame tab can cycle through with `v`.
#[derive(Clone, Copy, PartialEq)]
enum Variant {
    Normal,
    SpoiledNormal,
    SpoiledStrict,
}

impl Variant {
    fn label(self) -> &'static str {
        match self {
            Variant::Normal => "normal",
            Variant::SpoiledNormal => "spoiled-normal",
            Variant::SpoiledStrict => "spoiled-strict",
        }
    }
    fn next(self) -> Self {
        match self {
            Variant::Normal => Variant::SpoiledNormal,
            Variant::SpoiledNormal => Variant::SpoiledStrict,
            Variant::SpoiledStrict => Variant::Normal,
        }
    }
    fn spoiler(self) -> Option<SpoilerMode> {
        match self {
            Variant::Normal => None,
            Variant::SpoiledNormal => Some(SpoilerMode::Normal),
            Variant::SpoiledStrict => Some(SpoilerMode::Strict),
        }
    }
}

/// One feed's captured data.
struct Fetched {
    url: String,
    raw: String,
    games: Vec<Game>,
    parse_error: Option<String>,
}

struct App {
    source: EspnSource,
    out_dir: PathBuf,
    feeds: Vec<(&'static str, &'static str)>,
    data: Vec<Option<Fetched>>,
    feed_idx: usize,
    event_idx: usize,
    pane: usize, // 0 feeds, 1 events, 2 detail
    tab: Tab,
    variant: Variant,
    scroll: u16,
    status: String,
    /// Terminal advertises 24-bit color (COLORTERM). Without it, RGB escape
    /// codes render as garbage (macOS Terminal has no truecolor support), so
    /// frames fall back to the xterm-256 cube.
    truecolor: bool,
}

impl App {
    /// Switch the detail tab — and pull focus to the detail pane, so j/k
    /// scroll what you just asked to look at (leaving focus on the feeds list
    /// made j/k flip feeds instead — a trap). Each tab starts at the top;
    /// carrying a 40-line JSON scroll into a 16-row frame blanked it.
    fn set_tab(&mut self, tab: Tab) {
        if self.tab != tab {
            self.scroll = 0;
        }
        self.tab = tab;
        self.pane = 2;
    }
}

impl App {
    fn fetched(&self) -> Option<&Fetched> {
        self.data[self.feed_idx].as_ref()
    }
    fn game(&self) -> Option<&Game> {
        self.fetched().and_then(|f| f.games.get(self.event_idx))
    }

    /// Fetch (or regenerate) the selected feed. Blocking is fine — this is a
    /// debug tool and the status line says what's happening.
    fn fetch_selected(&mut self) {
        let (feed, name) = self.feeds[self.feed_idx];
        self.status = format!("fetching {name}…");
        let result = block_on_feed(&self.source, feed);
        match result {
            Ok(f) => {
                self.status = match &f.parse_error {
                    Some(e) => format!("{name}: fetched, PARSE ERROR: {e}"),
                    None => format!("{name}: {} events", f.games.len()),
                };
                self.data[self.feed_idx] = Some(f);
                self.event_idx = 0;
                self.scroll = 0;
            }
            Err(e) => self.status = format!("{name}: fetch failed: {e}"),
        }
    }

    /// The selected event's raw JSON subtree (falls back to the whole doc).
    fn raw_for_selection(&self) -> String {
        let Some(f) = self.fetched() else { return String::new() };
        let Some(g) = f.games.get(self.event_idx) else {
            return pretty_json(&f.raw);
        };
        event_subtree(&f.raw, &g.id).unwrap_or_else(|| pretty_json(&f.raw))
    }

    fn save_current(&mut self) {
        let (feed, _) = self.feeds[self.feed_idx];
        let Some(f) = self.data[self.feed_idx].as_ref() else {
            self.status = "nothing fetched yet".into();
            return;
        };
        let Some(g) = f.games.get(self.event_idx) else {
            self.status = "no event selected".into();
            return;
        };
        match save_event(&self.out_dir, feed, f, g) {
            Ok(dir) => self.status = format!("saved -> {}", dir.display()),
            Err(e) => self.status = format!("save failed: {e}"),
        }
    }

    fn save_feed(&mut self) {
        let (feed, _) = self.feeds[self.feed_idx];
        let Some(f) = self.data[self.feed_idx].as_ref() else {
            self.status = "nothing fetched yet".into();
            return;
        };
        match save_all(&self.out_dir, feed, f) {
            Ok(dir) => self.status = format!("saved {} events -> {}", f.games.len(), dir.display()),
            Err(e) => self.status = format!("save failed: {e}"),
        }
    }
}

/// Run a feed fetch + parse on the tokio runtime from the sync TUI loop.
fn block_on_feed(source: &EspnSource, feed: &str) -> Result<Fetched, String> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            if feed == "demo" {
                return Ok(Fetched {
                    url: "(generated locally)".into(),
                    raw: "demo feed — no raw upstream; games generated by demo_games(0, 0)".into(),
                    games: espn::parse_feed("demo", "").unwrap_or_default(),
                    parse_error: None,
                });
            }
            let url = source.feed_url(feed).unwrap_or_default();
            let raw = source.fetch_feed_raw(feed).await?;
            let (games, parse_error) = match espn::parse_feed(feed, &raw) {
                Ok(g) => (g, None),
                Err(e) => (Vec::new(), Some(e)),
            };
            Ok(Fetched { url, raw, games, parse_error })
        })
    })
}

/// Load a saved raw capture instead of fetching (offline debugging).
fn load_input(feed: &str, path: &str) -> Result<Fetched, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let (games, parse_error) = match espn::parse_feed(feed, &raw) {
        Ok(g) => (g, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    Ok(Fetched { url: format!("(file) {path}"), raw, games, parse_error })
}

fn pretty_json(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| raw.to_string())
}

/// Find the raw `events[i]` subtree whose id matches the game (F1 session ids
/// are "{event}-{session}", so match on the prefix too).
fn event_subtree(raw: &str, game_id: &str) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_str(raw).ok()?;
    let base = game_id.split('-').next().unwrap_or(game_id);
    let events = doc.get("events")?.as_array()?;
    let ev = events.iter().find(|e| {
        e.get("id").and_then(|i| i.as_str()).is_some_and(|i| i == game_id || i == base)
    })?;
    serde_json::to_string_pretty(ev).ok()
}

fn event_label(g: &Game) -> String {
    let title = if g.home.name.is_empty() {
        g.leaderboard.as_ref().map(|l| l.name.clone()).unwrap_or_else(|| g.away.abbrev.clone())
    } else {
        format!("{} @ {}", g.away.abbrev, g.home.abbrev)
    };
    let state = match g.status {
        GameStatus::Pregame => "pre",
        GameStatus::Active => "LIVE",
        GameStatus::Intermission => "int",
        GameStatus::Final => "final",
    };
    format!("{title:<14} {state:<5} {} {}", g.ordinal, g.clock).trim_end().to_string()
}

fn safe_id(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

/// Write one event's artifacts: raw subtree, processed Game, frame variants.
fn save_event(out: &Path, feed: &str, f: &Fetched, g: &Game) -> std::io::Result<PathBuf> {
    let dir = out.join(feed).join(safe_id(&g.id));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("raw.json"), event_subtree(&f.raw, &g.id).unwrap_or_default())?;
    std::fs::write(dir.join("game.txt"), format!("{g:#?}\n"))?;
    for variant in [Variant::Normal, Variant::SpoiledNormal, Variant::SpoiledStrict] {
        let (_, px, _) =
            render::render_board_frame(g, render::LEADERBOARD_DEFAULT_ROWS, variant.spoiler(), None);
        crate::save_png(dir.join(format!("frame-{}.png", variant.label())).to_str().unwrap(), &px, 8);
    }
    Ok(dir)
}

/// Write a whole feed: the full raw doc + every event's artifacts.
fn save_all(out: &Path, feed: &str, f: &Fetched) -> std::io::Result<PathBuf> {
    let dir = out.join(feed);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("raw.json"), pretty_json(&f.raw))?;
    std::fs::write(dir.join("url.txt"), format!("{}\n", f.url))?;
    for g in &f.games {
        save_event(out, feed, f, g)?;
    }
    Ok(dir)
}

/// An RGB pixel as a terminal color: true 24-bit when the terminal supports
/// it, else the nearest xterm-256 cube entry (RGB escapes on a non-truecolor
/// terminal don't degrade — they render as garbage).
fn cell_color(r: u8, g: u8, b: u8, truecolor: bool) -> Color {
    if truecolor {
        return Color::Rgb(r, g, b);
    }
    // Nearest 6x6x6 cube level (thresholds from the xterm palette's
    // 0/95/135/175/215/255 steps).
    let q = |v: u8| -> u16 {
        if v < 48 {
            0
        } else if v < 114 {
            1
        } else {
            (v as u16 - 35) / 40
        }
    };
    Color::Indexed((16 + 36 * q(r) + 6 * q(g) + q(b)) as u8)
}

/// Render a frame as terminal half-blocks (▀: fg = upper pixel, bg = lower).
fn frame_lines(px: &[u8], scroll: u16, truecolor: bool) -> Vec<Line<'static>> {
    let height = px.len() / (FRAME_WIDTH * 2);
    let mut lines = Vec::new();
    // Scroll counts terminal rows (2px each), clamped so a stale offset can
    // never scroll the whole frame out of view.
    let mut y = ((scroll as usize) * 2).min(height.saturating_sub(2));
    while y < height {
        let mut spans = Vec::with_capacity(FRAME_WIDTH);
        for x in 0..FRAME_WIDTH {
            let color = |yy: usize| -> Color {
                if yy >= height {
                    return Color::Black;
                }
                let i = (yy * FRAME_WIDTH + x) * 2;
                let c = u16::from_le_bytes([px[i], px[i + 1]]);
                let (r, g, b) = scoreboard_protocol::rgb565_to_888(c);
                cell_color(r, g, b, truecolor)
            };
            spans.push(Span::styled("▀", Style::default().fg(color(y)).bg(color(y + 1))));
        }
        lines.push(Line::from(spans));
        y += 2;
    }
    lines
}

pub fn run(args: &[String]) {
    let get_flag = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
    };
    let out_dir = PathBuf::from(get_flag("--out").unwrap_or_else(|| "sbtui-out".into()));
    let feed_arg = get_flag("--feed");
    let input = get_flag("--input");
    let dump = args.iter().any(|a| a == "--dump");

    let source = EspnSource::new();
    let feeds: Vec<(&str, &str)> = espn::FEEDS.to_vec();

    // Headless: fetch/load one feed, write everything, print paths, exit.
    // This is the scriptable path (agents, CI, quick captures).
    if dump {
        let Some(feed) = feed_arg.as_deref() else {
            eprintln!("--dump needs --feed <id>; feeds: {:?}", feeds.iter().map(|f| f.0).collect::<Vec<_>>());
            std::process::exit(2);
        };
        let fetched = match &input {
            Some(path) => load_input(feed, path),
            None => block_on_feed(&source, feed),
        };
        match fetched {
            Ok(f) => {
                if let Some(e) = &f.parse_error {
                    eprintln!("PARSE ERROR ({feed}): {e}");
                }
                match save_all(&out_dir, feed, &f) {
                    Ok(dir) => {
                        println!("{}: {} events -> {}", feed, f.games.len(), dir.display());
                        for g in &f.games {
                            println!("  {}  {}", safe_id(&g.id), event_label(g));
                        }
                    }
                    Err(e) => {
                        eprintln!("save failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => {
                eprintln!("fetch failed ({feed}): {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let mut app = App {
        source,
        out_dir,
        data: (0..feeds.len()).map(|_| None).collect(),
        feeds,
        feed_idx: 0,
        event_idx: 0,
        pane: 0,
        tab: Tab::Frame,
        variant: Variant::Normal,
        scroll: 0,
        status: "Enter/r fetch · Tab panes · 1/2/3 tabs · v variant · s/S save · q quit".into(),
        truecolor: std::env::var("COLORTERM")
            .map(|v| v.contains("truecolor") || v.contains("24bit"))
            .unwrap_or(false),
    };

    // Preload: --feed selects; --input loads a capture into it.
    if let Some(feed) = feed_arg.as_deref() {
        if let Some(i) = app.feeds.iter().position(|(id, _)| *id == feed) {
            app.feed_idx = i;
            if let Some(path) = &input {
                match load_input(feed, path) {
                    Ok(f) => {
                        app.status = format!("loaded {} events from {path}", f.games.len());
                        app.data[i] = Some(f);
                    }
                    Err(e) => app.status = format!("load failed: {e}"),
                }
            }
        }
    }

    let mut terminal = ratatui::init();
    loop {
        terminal
            .draw(|frame| {
                let outer = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(10), Constraint::Length(1)])
                    .split(frame.area());
                let panes = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(30),
                        Constraint::Length(34),
                        Constraint::Min(40),
                    ])
                    .split(outer[0]);
                draw_feeds(frame, panes[0], &app);
                draw_events(frame, panes[1], &app);
                draw_detail(frame, panes[2], &app);
                frame.render_widget(
                    Paragraph::new(app.status.as_str()).style(Style::default().fg(Color::DarkGray)),
                    outer[1],
                );
            })
            .expect("draw");

        let Ok(ev) = event::read() else { break };
        let Event::Key(key) = ev else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::Tab => app.pane = (app.pane + 1) % 3,
            KeyCode::BackTab => app.pane = (app.pane + 2) % 3,
            KeyCode::Char('1') => app.set_tab(Tab::Raw),
            KeyCode::Char('2') => app.set_tab(Tab::Game),
            KeyCode::Char('3') => app.set_tab(Tab::Frame),
            KeyCode::Char('v') => app.variant = app.variant.next(),
            KeyCode::Char('r') => app.fetch_selected(),
            KeyCode::Char('s') => app.save_current(),
            KeyCode::Char('S') => app.save_feed(),
            KeyCode::Char('g') => app.scroll = 0,
            KeyCode::Enter => match app.pane {
                0 => app.fetch_selected(),
                _ => {}
            },
            KeyCode::Up | KeyCode::Char('k') => match app.pane {
                0 => {
                    app.feed_idx = app.feed_idx.saturating_sub(1);
                    app.event_idx = 0;
                    app.scroll = 0;
                }
                1 => {
                    app.event_idx = app.event_idx.saturating_sub(1);
                    app.scroll = 0;
                }
                _ => app.scroll = app.scroll.saturating_sub(1),
            },
            KeyCode::Down | KeyCode::Char('j') => match app.pane {
                0 => {
                    app.feed_idx = (app.feed_idx + 1).min(app.feeds.len() - 1);
                    app.event_idx = 0;
                    app.scroll = 0;
                }
                1 => {
                    let n = app.fetched().map(|f| f.games.len()).unwrap_or(0);
                    app.event_idx = (app.event_idx + 1).min(n.saturating_sub(1));
                    app.scroll = 0;
                }
                _ => app.scroll = app.scroll.saturating_add(1),
            },
            KeyCode::PageUp => app.scroll = app.scroll.saturating_sub(20),
            KeyCode::PageDown => app.scroll = app.scroll.saturating_add(20),
            _ => {}
        }
    }
    ratatui::restore();
}

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default().borders(Borders::ALL).title(title).border_style(style)
}

fn draw_feeds(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .feeds
        .iter()
        .enumerate()
        .map(|(i, (_, name))| {
            let marker = match &app.data[i] {
                Some(f) if f.parse_error.is_some() => " ✗",
                Some(f) => {
                    if f.games.is_empty() {
                        " ·"
                    } else {
                        " ✓"
                    }
                }
                None => "",
            };
            ListItem::new(format!("{name}{marker}"))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.feed_idx));
    frame.render_stateful_widget(
        List::new(items)
            .block(pane_block("Feeds (Enter = fetch)", app.pane == 0))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        area,
        &mut state,
    );
}

fn draw_events(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = match app.fetched() {
        Some(f) => f.games.iter().map(|g| ListItem::new(event_label(g))).collect(),
        None => vec![ListItem::new("(not fetched)")],
    };
    let mut state = ListState::default().with_selected(Some(app.event_idx));
    let n = app.fetched().map(|f| f.games.len()).unwrap_or(0);
    frame.render_stateful_widget(
        List::new(items)
            .block(pane_block(&format!("Events ({n})"), app.pane == 1))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        area,
        &mut state,
    );
}

fn draw_detail(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(5)])
        .split(area);
    let titles = ["1 Raw", "2 Game", "3 Frame"];
    let selected = match app.tab {
        Tab::Raw => 0,
        Tab::Game => 1,
        Tab::Frame => 2,
    };
    frame.render_widget(
        Tabs::new(titles.iter().map(|t| Line::from(*t)).collect::<Vec<_>>())
            .select(selected)
            .highlight_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    let body = match app.tab {
        Tab::Raw => Paragraph::new(app.raw_for_selection())
            .scroll((app.scroll, 0))
            .block(pane_block("raw json (j/k scroll)", app.pane == 2)),
        Tab::Game => {
            let text = app.game().map(|g| format!("{g:#?}")).unwrap_or_else(|| "(no event)".into());
            Paragraph::new(text)
                .scroll((app.scroll, 0))
                .block(pane_block("processed Game (j/k scroll)", app.pane == 2))
        }
        Tab::Frame => {
            let (lines, title) = match app.game() {
                Some(g) => {
                    let (_, px, scrolls) = render::render_board_frame(
                        g,
                        render::LEADERBOARD_DEFAULT_ROWS,
                        app.variant.spoiler(),
                        None,
                    );
                    let h = px.len() / (FRAME_WIDTH * 2);
                    (
                        frame_lines(&px, app.scroll, app.truecolor),
                        format!(
                            "frame · {} · {}x{}{} (v = variant)",
                            app.variant.label(),
                            FRAME_WIDTH,
                            h,
                            if scrolls { " · scrolls on board" } else { "" }
                        ),
                    )
                }
                None => (vec![Line::from("(no event)")], "frame".to_string()),
            };
            frame.render_widget(
                Paragraph::new(lines).block(pane_block(&title, app.pane == 2)),
                chunks[1],
            );
            return;
        }
    };
    frame.render_widget(body, chunks[1]);
}
