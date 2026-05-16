use anyhow::{Context, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};
use std::{
    fs,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

type Term = Terminal<CrosstermBackend<io::Stdout>>;

// ─── Domain ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Mode { PressRelease, PressOnly, ReleaseOnly }

impl Mode {
    fn as_str(&self) -> &'static str {
        match self {
            Mode::PressRelease => "press+release",
            Mode::PressOnly    => "press only",
            Mode::ReleaseOnly  => "release only",
        }
    }
    fn from_str(s: &str) -> Self {
        match s.trim() {
            "press only"   => Mode::PressOnly,
            "release only" => Mode::ReleaseOnly,
            _              => Mode::PressRelease,
        }
    }
    fn all() -> &'static [&'static str] { &["press+release", "press only", "release only"] }
}

#[derive(Debug, Clone)]
enum EventKind {
    Key   { code: String, name: String, mode: Mode },
    Click { code: String, name: String, mode: Mode },  // mouse button via ydotool key
    Delay (String),
    Type  (String),
    LoopStart(u32),   // 0 = infinite
    LoopEnd,
}

impl EventKind {
    fn display(&self) -> String {
        match self {
            EventKind::Key { code, name, mode } =>
                format!("KEY {} ({}) {}", name, code, mode.as_str()),
            EventKind::Click { code, name, mode } =>
                format!("CLICK {} ({}) {}", name, code, mode.as_str()),
            EventKind::Delay(d)      => format!("DELAY {}s", d),
            EventKind::Type(s)       => format!("TYPE \"{}\"", s),
            EventKind::LoopStart(0)  => "LOOP ∞ (infinite)".to_string(),
            EventKind::LoopStart(n)  => format!("LOOP {} times", n),
            EventKind::LoopEnd       => "END LOOP".to_string(),
        }
    }

    /// Lines of bash this event produces, given nesting depth for indentation.
    fn bash_lines(&self, auto_delay: &str, depth: usize) -> Vec<String> {
        let indent = "  ".repeat(depth);
        let mut out = Vec::new();
        match self {
            EventKind::Key { code, name, mode } => {
                let cmd = match mode {
                    Mode::PressOnly    => format!("ydotool key {}:1 #{}", code, name),
                    Mode::ReleaseOnly  => format!("ydotool key {}:0 #{}", code, name),
                    Mode::PressRelease => format!("ydotool key {}:1 {}:0 #{}", code, code, name),
                };
                out.push(format!("{}{}", indent, cmd));
                if auto_delay != "0" { out.push(format!("{}sleep {}", indent, auto_delay)); }
            }
            EventKind::Click { code, name, mode } => {
                let cmd = match mode {
                    Mode::PressOnly    => format!("ydotool key {}:1 #{}", code, name),
                    Mode::ReleaseOnly  => format!("ydotool key {}:0 #{}", code, name),
                    Mode::PressRelease => format!("ydotool key {}:1 {}:0 #{}", code, code, name),
                };
                out.push(format!("{}{}", indent, cmd));
                if auto_delay != "0" { out.push(format!("{}sleep {}", indent, auto_delay)); }
            }
            EventKind::Delay(d)     => out.push(format!("{}sleep {}", indent, d)),
            EventKind::Type(s)      => {
                out.push(format!("{}ydotool type \"{}\"", indent, s));
                if auto_delay != "0" { out.push(format!("{}sleep {}", indent, auto_delay)); }
            }
            EventKind::LoopStart(0) => out.push(format!("{}while true; do", indent)),
            EventKind::LoopStart(n) => out.push(format!("{}for _i in $(seq {}); do", indent, n)),
            EventKind::LoopEnd      => out.push(format!("{}done", indent)),
        }
        out
    }

    fn save_str(&self) -> String {
        match self {
            EventKind::Key { code, name, mode } =>
                format!("KEY|{}|{}|{}", code, name, mode.as_str()),
            EventKind::Click { code, name, mode } =>
                format!("CLICK|{}|{}|{}", code, name, mode.as_str()),
            EventKind::Delay(d)     => format!("DELAY|{}||", d),
            EventKind::Type(s)      => format!("TYPE|{}||", s),
            EventKind::LoopStart(n) => format!("LOOP_START|{}||", n),
            EventKind::LoopEnd      => "LOOP_END|||".to_string(),
        }
    }

    fn from_saved(parts: &[&str]) -> Option<Self> {
        match parts[0] {
            "KEY" if parts.len() == 4 => Some(EventKind::Key {
                code: parts[1].to_string(),
                name: parts[2].to_string(),
                mode: Mode::from_str(parts[3]),
            }),
            "CLICK" if parts.len() >= 3 => Some(EventKind::Click {
                code: parts[1].to_string(),
                name: parts[2].to_string(),
                mode: if parts.len() >= 4 { Mode::from_str(parts[3]) } else { Mode::PressRelease },
            }),
            "DELAY" if parts.len() >= 2 => Some(EventKind::Delay(parts[1].to_string())),
            "TYPE"  if parts.len() >= 2 => Some(EventKind::Type(parts[1].to_string())),
            "LOOP_START" if parts.len() >= 2 =>
                Some(EventKind::LoopStart(parts[1].parse().unwrap_or(0))),
            "LOOP_END" => Some(EventKind::LoopEnd),
            _ => None,
        }
    }
}

// ─── Project ─────────────────────────────────────────────────────────────────

struct Project {
    name: String,
    project_file: PathBuf,
    output_file: PathBuf,
    auto_delay: String,
    global_loop: u32,   // 0 = no global loop
    events: Vec<EventKind>,
}

impl Project {
    fn new(name: &str) -> Self {
        let base = name.trim_end_matches(".ydotooler").trim_end_matches(".sh");
        Project {
            name: base.to_string(),
            project_file: PathBuf::from(format!("{}.ydotooler", base)),
            output_file: PathBuf::from(format!("{}.sh", base)),
            auto_delay: "0".to_string(),
            global_loop: 0,
            events: Vec::new(),
        }
    }

    fn load(&mut self) -> Result<()> {
        self.events.clear();
        self.auto_delay = "0".to_string();
        self.global_loop = 0;
        if !self.project_file.exists() {
            fs::write(&self.project_file, "#AUTO_DELAY=0\n#GLOBAL_LOOP=0\n")?;
            return Ok(());
        }
        let f = fs::File::open(&self.project_file)?;
        for line in BufReader::new(f).lines() {
            let line = line?.trim_end_matches('\r').to_string();
            if line.starts_with("#AUTO_DELAY=") {
                self.auto_delay = line["#AUTO_DELAY=".len()..].to_string();
                continue;
            }
            if line.starts_with("#GLOBAL_LOOP=") {
                self.global_loop = line["#GLOBAL_LOOP=".len()..].parse().unwrap_or(0);
                continue;
            }
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.is_empty() || parts[0].is_empty() { continue; }
            if let Some(ev) = EventKind::from_saved(&parts) {
                self.events.push(ev);
            }
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let mut f = fs::File::create(&self.project_file)?;
        writeln!(f, "#AUTO_DELAY={}", self.auto_delay)?;
        writeln!(f, "#GLOBAL_LOOP={}", self.global_loop)?;
        for ev in &self.events { writeln!(f, "{}", ev.save_str())?; }
        Ok(())
    }

    fn write_script(&self) -> Result<PathBuf> {
        let mut f = fs::File::create(&self.output_file)?;
        writeln!(f, "#!/usr/bin/env bash")?;
        writeln!(f, "# Generated by ydotooler")?;
        writeln!(f)?;

        let body = self.render_body(0);

        if self.global_loop > 0 {
            writeln!(f, "for _i in $(seq {}); do", self.global_loop)?;
            for line in &body { writeln!(f, "  {}", line)?; }
            writeln!(f, "done")?;
        } else {
            for line in &body { writeln!(f, "{}", line)?; }
        }

        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&self.output_file)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&self.output_file, perms)?;
        }
        Ok(self.output_file.clone())
    }

    fn bash_preview(&self) -> String {
        let body = self.render_body(0);
        let mut lines = vec![
            "#!/usr/bin/env bash".to_string(),
            "# Generated by ydotooler".to_string(),
            String::new(),
        ];
        if self.global_loop > 0 {
            lines.push(format!("for _i in $(seq {}); do", self.global_loop));
            for l in body { lines.push(format!("  {}", l)); }
            lines.push("done".to_string());
        } else {
            lines.extend(body);
        }
        lines.join("\n")
    }

    /// For each event index, return the 1-based line number of its first bash
    /// line in the preview output. This lets the events pane show the same
    /// numbers as the bash preview gutter.
    fn event_line_numbers(&self) -> Vec<usize> {
        // Header: "#!/usr/bin/env bash", "# Generated by ydotooler", ""  = 3 lines
        // If global loop wraps everything, add 1 more for the "for …; do" line.
        let header_lines: usize = if self.global_loop > 0 { 4 } else { 3 };
        // Each body line is indented by 2 extra spaces inside a global loop,
        // but that doesn't add extra lines — same count.
        let mut result = Vec::with_capacity(self.events.len());
        let mut cursor = header_lines + 1; // 1-based
        let mut depth: usize = 0;
        for ev in &self.events {
            if matches!(ev, EventKind::LoopEnd) { depth = depth.saturating_sub(1); }
            result.push(cursor);
            cursor += ev.bash_lines(&self.auto_delay, depth).len();
            if matches!(ev, EventKind::LoopStart(_)) { depth += 1; }
        }
        result
    }

    /// Render events to bash lines, tracking loop depth for indentation.
    fn render_body(&self, base_depth: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut depth = base_depth;
        for ev in &self.events {
            if matches!(ev, EventKind::LoopEnd) && depth > base_depth { depth -= 1; }
            out.extend(ev.bash_lines(&self.auto_delay, depth));
            if matches!(ev, EventKind::LoopStart(_)) { depth += 1; }
        }
        out
    }
}

// ─── Terminal helpers ─────────────────────────────────────────────────────────

fn pick_key_code(terminal: &mut Term, codes_path: &Path) -> Result<Option<String>> {
    let content = fs::read_to_string(codes_path)?;
    let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let mut query = String::new();
    let mut list_state = ListState::default();
    list_state.select(Some(0));
    terminal.clear()?;

    loop {
        let filtered: Vec<&String> = lines.iter()
            .filter(|l| l.to_lowercase().contains(&query.to_lowercase()))
            .collect();
        let sel = list_state.selected().unwrap_or(0).min(filtered.len().saturating_sub(1));
        list_state.select(Some(sel));

        terminal.draw(|f| {
            let area = f.size();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(0)])
                .split(area);
            let search = Paragraph::new(format!(" {}_", query))
                .block(Block::default().borders(Borders::ALL)
                    .title(" Search key code (ESC=cancel, Enter=select) ")
                    .border_style(Style::default().fg(Color::Yellow)));
            f.render_widget(search, chunks[0]);
            let items: Vec<ListItem> = filtered.iter().map(|l| ListItem::new(l.as_str())).collect();
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(" Key codes "))
                .highlight_style(Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD))
                .highlight_symbol("▶ ");
            f.render_stateful_widget(list, chunks[1], &mut list_state);
        })?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Esc       => { terminal.clear()?; return Ok(None); }
                KeyCode::Enter     => { let r = filtered.get(sel).map(|s| s.to_string()); terminal.clear()?; return Ok(r); }
                KeyCode::Char(c)   => { query.push(c); list_state.select(Some(0)); }
                KeyCode::Backspace => { query.pop(); list_state.select(Some(0)); }
                KeyCode::Down      => { let n = (sel+1).min(filtered.len().saturating_sub(1)); list_state.select(Some(n)); }
                KeyCode::Up        => { list_state.select(Some(sel.saturating_sub(1))); }
                _ => {}
            }
        }
    }
}

fn pick_from_list(terminal: &mut Term, title: &str, options: &[&str]) -> Result<Option<String>> {
    let mut list_state = ListState::default();
    list_state.select(Some(0));
    terminal.clear()?;

    loop {
        let sel = list_state.selected().unwrap_or(0);
        terminal.draw(|f| {
            let area = centered_rect(40, options.len() as u16 + 4, f.size());
            let items: Vec<ListItem> = options.iter().map(|o| ListItem::new(*o)).collect();
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL)
                    .title(format!(" {} (ESC=cancel) ", title))
                    .border_style(Style::default().fg(Color::Yellow)))
                .highlight_style(Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD))
                .highlight_symbol("▶ ");
            f.render_stateful_widget(list, area, &mut list_state);
        })?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Esc   => { terminal.clear()?; return Ok(None); }
                KeyCode::Enter => { terminal.clear()?; return Ok(Some(options[sel].to_string())); }
                KeyCode::Down  => list_state.select(Some((sel+1).min(options.len()-1))),
                KeyCode::Up    => list_state.select(Some(sel.saturating_sub(1))),
                _ => {}
            }
        }
    }
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100u16.saturating_sub(height.min(100))) / 2),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vert[1])[1]
}

// ─── App state ────────────────────────────────────────────────────────────────

const MENU: &[&str] = &[
    "Add key",
    "Add delay",
    "Add type",
    "Add loop block",
    "Move up",
    "Move down",
    "Edit event",
    "Delete event",
    "Set auto delay",
    "Set global loop",
    "Save",
    "Load",
    "Generate",
    "Quit",
];

#[derive(PartialEq)]
enum Focus { Menu, Events }

/// What the status bar is doing right now.
enum InputMode {
    Normal,
    /// Inline prompt: label shown, buffer being edited, and what to do on confirm.
    Prompting {
        label: String,
        buffer: String,
        target: PromptTarget,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum PromptTarget {
    AddDelay,
    AddType,
    EditDelay(usize),
    EditType(usize),
    EditLoopCount(usize),
    SetAutoDelay,
    SetGlobalLoop,
    AddLoopBlock,   // reuse buffer as loop count for a new loop pair
}

struct App {
    project: Project,
    menu_state: ListState,
    event_state: ListState,
    /// Which event indices are multi-selected.
    selected: Vec<bool>,
    focus: Focus,
    input_mode: InputMode,
    status: String,
    /// Top line of the bash preview (0-based), kept in sync with cursor.
    preview_scroll: u16,
}

impl App {
    fn new(project: Project) -> Self {
        let len = project.events.len();
        let mut menu_state = ListState::default();
        menu_state.select(Some(0));
        let mut event_state = ListState::default();
        if len > 0 { event_state.select(Some(0)); }
        App {
            project,
            menu_state,
            event_state,
            selected: vec![false; len],
            focus: Focus::Menu,
            input_mode: InputMode::Normal,
            status: "Ready — Tab to switch pane".to_string(),
            preview_scroll: 0,
        }
    }

    fn menu_sel(&self) -> usize { self.menu_state.selected().unwrap_or(0) }
    fn cursor(&self) -> Option<usize> { self.event_state.selected() }

    /// Recompute preview_scroll so the selected event's first bash line is
    /// visible, with a small context margin above it.
    fn sync_preview_scroll(&mut self) {
        let line_nos = self.project.event_line_numbers();
        if let Some(cursor) = self.event_state.selected() {
            let target = line_nos.get(cursor).copied().unwrap_or(1);
            // 2-line margin above, convert to 0-based scroll offset.
            self.preview_scroll = (target as u16).saturating_sub(3);
        }
    }

    fn sync_selected_len(&mut self) {
        self.selected.resize(self.project.events.len(), false);
    }

    fn clamp_cursor(&mut self) {
        let len = self.project.events.len();
        if len == 0 {
            self.event_state.select(None);
        } else {
            let cur = self.event_state.selected().unwrap_or(0).min(len - 1);
            self.event_state.select(Some(cur));
        }
        self.sync_selected_len();
        self.sync_preview_scroll();
    }

    /// Insert an event immediately after the cursor, or at the end if no
    /// cursor. Leaves the cursor on the newly inserted event.
    fn insert_after_cursor(&mut self, ev: EventKind) {
        let insert_pos = match self.event_state.selected() {
            Some(cur) => cur + 1,
            None      => self.project.events.len(),
        };
        self.project.events.insert(insert_pos, ev);
        self.sync_selected_len();
        self.event_state.select(Some(insert_pos));
        self.sync_preview_scroll();
    }

    /// Duplicate all multi-selected events (or the cursor event), inserting
    /// the copies immediately after the last selected index.
    fn duplicate_selection(&mut self) {
        let indices = self.effective_selection();
        if indices.is_empty() { return; }
        let copies: Vec<EventKind> = indices.iter()
            .map(|&i| self.project.events[i].clone())
            .collect();
        let insert_at = *indices.last().unwrap() + 1;
        for (offset, ev) in copies.into_iter().enumerate() {
            self.project.events.insert(insert_at + offset, ev);
        }
        // Move cursor to last inserted copy
        let new_cursor = insert_at + indices.len() - 1;
        self.clear_selection();
        self.sync_selected_len();
        self.event_state.select(Some(new_cursor));
        self.sync_preview_scroll();
    }

    fn clear_selection(&mut self) {
        self.selected.iter_mut().for_each(|s| *s = false);
    }

    /// Indices that are part of the effective move set:
    /// if anything is multi-selected, use those; otherwise use the cursor.
    fn effective_selection(&self) -> Vec<usize> {
        let multi: Vec<usize> = self.selected.iter().enumerate()
            .filter_map(|(i, &s)| if s { Some(i) } else { None })
            .collect();
        if !multi.is_empty() {
            multi
        } else if let Some(c) = self.cursor() {
            vec![c]
        } else {
            vec![]
        }
    }

    fn is_prompting(&self) -> bool { matches!(self.input_mode, InputMode::Prompting { .. }) }

    fn start_prompt(&mut self, label: &str, target: PromptTarget) {
        self.input_mode = InputMode::Prompting {
            label: label.to_string(),
            buffer: String::new(),
            target,
        };
    }
}

// ─── Draw ─────────────────────────────────────────────────────────────────────

fn draw(f: &mut Frame, app: &mut App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(f.size());

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(22), Constraint::Percentage(35), Constraint::Min(0)])
        .split(root[0]);

    draw_menu(f, app, body[0]);
    draw_events(f, app, body[1]);
    draw_bash_preview(f, app, body[2]);
    draw_statusbar(f, app, root[1]);
}

fn draw_menu(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Menu && !app.is_prompting();
    let border_style = if focused { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::DarkGray) };
    let items: Vec<ListItem> = MENU.iter().map(|m| ListItem::new(*m)).collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL)
            .title(Span::styled(" Menu ", Style::default().add_modifier(Modifier::BOLD)))
            .border_style(border_style))
        .highlight_style(Style::default().bg(Color::Yellow).fg(Color::Black).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, area, &mut app.menu_state);
}

fn draw_events(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Events && !app.is_prompting();
    let border_style = if focused { Style::default().fg(Color::Cyan) } else { Style::default().fg(Color::DarkGray) };

    let depths   = compute_depths(&app.project.events);
    let line_nos = app.project.event_line_numbers();

    // Match the preview gutter width so the numbers are visually comparable.
    let preview_total = app.project.bash_preview().lines().count();
    let gutter_w = preview_total.to_string().len().max(1);

    let items: Vec<ListItem> = app.project.events.iter().enumerate()
        .map(|(i, ev)| {
            let is_sel = app.selected.get(i).copied().unwrap_or(false);
            let depth  = depths.get(i).copied().unwrap_or(0);
            let indent = "  ".repeat(depth);
            let lineno = line_nos.get(i).copied().unwrap_or(i + 1);

            let (icon, color) = match ev {
                EventKind::Key { .. }   => ("⌨ ", Color::Green),
                EventKind::Click { .. } => ("🖱 ", Color::LightRed),
                EventKind::Delay(_)     => ("⏱ ", Color::Magenta),
                EventKind::Type(_)      => ("T ", Color::Cyan),
                EventKind::LoopStart(_) => ("↩ ", Color::Yellow),
                EventKind::LoopEnd      => ("↪ ", Color::Yellow),
            };

            let sel_marker = if is_sel {
                Span::styled("● ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
            } else {
                Span::styled("  ", Style::default())
            };

            ListItem::new(Line::from(vec![
                sel_marker,
                Span::styled(
                    format!("{:>width$} ", lineno, width = gutter_w),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(indent),
                Span::styled(icon, Style::default().fg(color)),
                Span::raw(ev.display()),
            ]))
        })
        .collect();

    let multi_count = app.selected.iter().filter(|&&s| s).count();
    let title = if multi_count > 0 {
        format!(" Events ({}) [{} selected] ", app.project.events.len(), multi_count)
    } else {
        format!(" Events ({}) ", app.project.events.len())
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL)
            .title(Span::styled(title, Style::default().add_modifier(Modifier::BOLD)))
            .border_style(border_style))
        .highlight_style(Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, area, &mut app.event_state);
}

/// Returns the display indent depth for each event.
fn compute_depths(events: &[EventKind]) -> Vec<usize> {
    let mut depths = Vec::with_capacity(events.len());
    let mut depth: usize = 0;
    for ev in events {
        if matches!(ev, EventKind::LoopEnd) { depth = depth.saturating_sub(1); }
        depths.push(depth);
        if matches!(ev, EventKind::LoopStart(_)) { depth += 1; }
    }
    depths
}

fn draw_bash_preview(f: &mut Frame, app: &mut App, area: Rect) {
    let preview = app.project.bash_preview();
    let raw_lines: Vec<&str> = preview.lines().collect();
    let total = raw_lines.len();
    let gutter_w = total.to_string().len();
    let gutter_style = Style::default().fg(Color::DarkGray);

    let lines: Vec<Line> = raw_lines.iter().enumerate().map(|(ln, line)| {
        let line_num = Span::styled(
            format!("{:>width$} ", ln + 1, width = gutter_w),
            gutter_style,
        );
        let trimmed = line.trim_start();
        let leading = &line[..line.len() - trimmed.len()];
        let mut spans: Vec<Span> = vec![line_num];

        if trimmed.starts_with('#') {
            spans.push(Span::styled(line.to_string(), Style::default().fg(Color::DarkGray)));
        } else if trimmed.starts_with("ydotool click") {
            // legacy click lines (shouldn't appear in new scripts, kept for old saves)
            let rest = &trimmed["ydotool click ".len()..];
            let (code, comment) = rest.split_once(" #").unwrap_or((rest, ""));
            spans.extend([
                Span::raw(leading.to_string()),
                Span::styled("ydotool ".to_string(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled("key ".to_string(), Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD)),
                Span::styled(code.to_string(), Style::default().fg(Color::White)),
            ]);
            if !comment.is_empty() {
                spans.push(Span::styled(format!(" #{}", comment), Style::default().fg(Color::DarkGray)));
            }
        } else if trimmed.starts_with("ydotool type") {
            let rest = trimmed["ydotool type".len()..].to_string();
            spans.extend([
                Span::raw(leading.to_string()),
                Span::styled("ydotool ".to_string(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled("type ".to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled(rest, Style::default().fg(Color::White)),
            ]);
        } else if trimmed.starts_with("ydotool") {
            // Mouse button events (BTN_ codes) get light red to distinguish from keyboard keys
            let is_mouse = trimmed.contains("#BTN_");
            let key_color = if is_mouse { Color::LightRed } else { Color::Green };
            let (cmd, rest) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
            // Split args from trailing comment for separate styling
            let (args, comment) = rest.split_once(" #").unwrap_or((rest, ""));
            spans.extend([
                Span::raw(leading.to_string()),
                Span::styled(format!("{} ", cmd), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(args.to_string(), Style::default().fg(key_color)),
            ]);
            if !comment.is_empty() {
                spans.push(Span::styled(format!(" #{}", comment), Style::default().fg(Color::DarkGray)));
            }
        } else if trimmed.starts_with("sleep") {
            let (cmd, rest) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
            spans.extend([
                Span::raw(leading.to_string()),
                Span::styled(format!("{} ", cmd), Style::default().fg(Color::Magenta)),
                Span::styled(rest.to_string(), Style::default().fg(Color::White)),
            ]);
        } else if trimmed.starts_with("for ") || trimmed.starts_with("while ") || trimmed == "done" {
            spans.push(Span::styled(line.to_string(), Style::default().fg(Color::Yellow)));
        } else if trimmed.starts_with("#!/") {
            spans.push(Span::styled(line.to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
        } else {
            spans.push(Span::raw(line.to_string()));
        }

        Line::from(spans)
    }).collect();

    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL)
            .title(Span::styled(
                format!(" Bash Preview → {} ", app.project.output_file.display()),
                Style::default().add_modifier(Modifier::BOLD),
            ))
            .border_style(Style::default().fg(Color::DarkGray)))
        .scroll((app.preview_scroll, 0));
    f.render_widget(para, area);
}

fn draw_statusbar(f: &mut Frame, app: &App, area: Rect) {
    match &app.input_mode {
        InputMode::Prompting { label, buffer, .. } => {
            let text = format!(" {} {}_", label, buffer);
            let para = Paragraph::new(text)
                .style(Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD));
            f.render_widget(para, area);
        }
        InputMode::Normal => {
            let help = match app.focus {
                Focus::Menu   => "↑↓ navigate  Enter select  Tab→Events  q quit",
                Focus::Events => "↑↓ navigate  Space=toggle-sel  k/K up  j/J down  u dup  d delete  Tab→Menu",
            };
            let global = if app.project.global_loop > 0 {
                format!("global_loop={}x", app.project.global_loop)
            } else {
                "global_loop=off".to_string()
            };
            let text = format!(
                " {} │ auto_delay={} │ {} │ {}  │  {}",
                app.project.name, app.project.auto_delay, global, help, app.status,
            );
            let para = Paragraph::new(text)
                .style(Style::default().bg(Color::DarkGray).fg(Color::White));
            f.render_widget(para, area);
        }
    }
}

// ─── Multi-select move helpers ────────────────────────────────────────────────

/// Move a set of indices up by one position. Indices must be sorted ascending.
fn move_indices_up(events: &mut Vec<EventKind>, selected: &mut Vec<bool>, indices: &[usize]) {
    if indices.is_empty() || indices[0] == 0 { return; }
    for &i in indices {
        events.swap(i - 1, i);
        selected.swap(i - 1, i);
    }
}

/// Move a set of indices down by one position. Indices must be sorted ascending.
fn move_indices_down(events: &mut Vec<EventKind>, selected: &mut Vec<bool>, indices: &[usize]) {
    let len = events.len();
    if indices.is_empty() || *indices.last().unwrap() + 1 >= len { return; }
    for &i in indices.iter().rev() {
        events.swap(i, i + 1);
        selected.swap(i, i + 1);
    }
}

// ─── Inline prompt confirm ────────────────────────────────────────────────────

fn confirm_prompt(app: &mut App) {
    // Swap out the input mode to inspect it
    let mode = std::mem::replace(&mut app.input_mode, InputMode::Normal);
    if let InputMode::Prompting { buffer, target, .. } = mode {
        let val = buffer.trim().to_string();
        match target {
            PromptTarget::AddDelay => {
                if !val.is_empty() {
                    app.insert_after_cursor(EventKind::Delay(val.clone()));
                    app.status = format!("Delay {}s added.", val);
                }
            }
            PromptTarget::AddType => {
                if !val.is_empty() {
                    app.insert_after_cursor(EventKind::Type(val.clone()));
                    app.status = "Type event added.".to_string();
                }
            }
            PromptTarget::EditDelay(idx) => {
                if !val.is_empty() {
                    app.project.events[idx] = EventKind::Delay(val.clone());
                    app.status = format!("Delay updated to {}s.", val);
                }
            }
            PromptTarget::EditType(idx) => {
                if !val.is_empty() {
                    app.project.events[idx] = EventKind::Type(val.clone());
                    app.status = "Type text updated.".to_string();
                }
            }
            PromptTarget::EditLoopCount(idx) => {
                if let Ok(n) = val.parse::<u32>() {
                    app.project.events[idx] = EventKind::LoopStart(n);
                    app.status = format!("Loop count updated to {}.", if n == 0 { "∞".to_string() } else { n.to_string() });
                } else if !val.is_empty() {
                    app.status = "Invalid number.".to_string();
                }
            }
            PromptTarget::SetAutoDelay => {
                if !val.is_empty() {
                    app.project.auto_delay = val.clone();
                    app.status = format!("Auto delay set to {}.", val);
                }
            }
            PromptTarget::SetGlobalLoop => {
                match val.parse::<u32>() {
                    Ok(n) => {
                        app.project.global_loop = n;
                        app.status = if n == 0 {
                            "Global loop disabled.".to_string()
                        } else {
                            format!("Global loop set to {} iteration(s).", n)
                        };
                    }
                    Err(_) if !val.is_empty() => { app.status = "Invalid number.".to_string(); }
                    _ => {}
                }
            }
            PromptTarget::AddLoopBlock => {
                let count = val.parse::<u32>().unwrap_or(0);
                app.insert_after_cursor(EventKind::LoopStart(count));
                app.insert_after_cursor(EventKind::LoopEnd);
                app.status = format!(
                    "Loop block added ({}). Add events between LOOP and END LOOP.",
                    if count == 0 { "∞".to_string() } else { format!("{}x", count) }
                );
            }
        }
        // Auto-save after any mutation
        if let Err(e) = app.project.save() {
            app.status = format!("Auto-save error: {e}");
        }
    }
}

// ─── Actions ─────────────────────────────────────────────────────────────────

fn action_add_key(terminal: &mut Term, app: &mut App) -> Result<()> {
    let candidates = [
        PathBuf::from("input-event-codes_edited"),
        std::env::current_exe().unwrap_or_default()
            .parent().unwrap_or(Path::new("."))
            .join("input-event-codes_edited"),
    ];
    let codes_path = candidates.iter().find(|p| p.exists()).cloned()
        .context("input-event-codes_edited not found next to binary or in cwd")?;

    let sel = match pick_key_code(terminal, &codes_path)? { Some(s) => s, None => return Ok(()) };
    let (code, name) = if let Some(pos) = sel.find(':') {
        (sel[..pos].trim().to_string(), sel[pos+1..].trim().to_string())
    } else {
        (sel.trim().to_string(), sel.trim().to_string())
    };

    let mode_str = match pick_from_list(terminal, "Key mode", Mode::all())? { Some(s) => s, None => return Ok(()) };
    let mode = Mode::from_str(&mode_str);

    // BTN_ codes are stored as Click so the UI shows the mouse icon and
    // light-red colour, but the output is identical ydotool key syntax.
    if name.starts_with("BTN_") {
        app.insert_after_cursor(EventKind::Click { code, name: name.clone(), mode });
        app.status = format!("Mouse button {} added ({}).", name, mode_str);
    } else {
        app.insert_after_cursor(EventKind::Key { code, name, mode });
        app.status = "Key added.".to_string();
    }
    Ok(())
}

fn action_edit(terminal: &mut Term, app: &mut App) -> Result<()> {
    let idx = match app.cursor() {
        Some(i) => i,
        None => { app.status = "No event selected.".to_string(); return Ok(()); }
    };
    match &app.project.events[idx] {
        EventKind::Key { .. } => {
            if let Some(mode_str) = pick_from_list(terminal, "New mode", Mode::all())? {
                if let EventKind::Key { mode, .. } = &mut app.project.events[idx] {
                    *mode = Mode::from_str(&mode_str);
                }
                app.status = "Mode updated.".to_string();
                app.project.save()?;
            }
        }
        EventKind::Click { .. } => {
            if let Some(mode_str) = pick_from_list(terminal, "New mode", Mode::all())? {
                if let EventKind::Click { mode, .. } = &mut app.project.events[idx] {
                    *mode = Mode::from_str(&mode_str);
                }
                app.status = "Mouse button mode updated.".to_string();
                app.project.save()?;
            }
        }
        EventKind::Delay(d) => {
            let current = d.clone();
            app.start_prompt(&format!("Edit delay [{}s] → ", current), PromptTarget::EditDelay(idx));
        }
        EventKind::Type(s) => {
            let current = s.clone();
            app.start_prompt(&format!("Edit type text [{}] → ", current), PromptTarget::EditType(idx));
        }
        EventKind::LoopStart(n) => {
            let current = *n;
            app.start_prompt(
                &format!("Loop count (0=∞) [{}] → ", current),
                PromptTarget::EditLoopCount(idx),
            );
        }
        EventKind::LoopEnd => {
            app.status = "END LOOP has no editable fields.".to_string();
        }
    }
    Ok(())
}

fn action_delete(app: &mut App) {
    // Delete all multi-selected, or cursor if none selected
    let to_delete: Vec<usize> = {
        let multi: Vec<usize> = app.selected.iter().enumerate()
            .filter_map(|(i, &s)| if s { Some(i) } else { None })
            .collect();
        if !multi.is_empty() { multi }
        else if let Some(c) = app.cursor() { vec![c] }
        else { return; }
    };
    // Remove in reverse order to preserve indices
    for &i in to_delete.iter().rev() {
        app.project.events.remove(i);
    }
    app.clear_selection();
    app.clamp_cursor();
    app.status = format!("Deleted {} event(s).", to_delete.len());
}

fn action_generate(app: &mut App) -> Result<()> {
    let path = app.project.write_script()?;
    let abs = fs::canonicalize(&path).unwrap_or(path);
    app.status = format!("Generated: {}", abs.display());
    Ok(())
}

// ─── Startup screen ──────────────────────────────────────────────────────────

/// Full-TUI startup: pick New / Load, return a ready Project or None to quit.
fn startup_screen(terminal: &mut Term) -> Result<Option<Project>> {
    #[derive(Clone, Copy, PartialEq)]
    enum StartupState { Menu, NamingNew, BrowsingFiles }

    let menu_items = ["New script", "Load existing .ydotooler", "Quit"];
    let mut menu_state = ListState::default();
    menu_state.select(Some(0));

    let mut state   = StartupState::Menu;
    let mut name_buf = String::new();
    let mut err_msg  = String::new();

    // For file browser
    let mut file_query = String::new();
    let mut file_state = ListState::default();

    loop {
        // Collect .ydotooler files fresh each frame (cheap, allows external creation)
        let ydotooler_files: Vec<String> = std::fs::read_dir(".")
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if name.ends_with(".ydotooler") { Some(name) } else { None }
            })
            .collect();

        let filtered_files: Vec<&String> = ydotooler_files.iter()
            .filter(|f| f.to_lowercase().contains(&file_query.to_lowercase()))
            .collect();

        terminal.draw(|f| {
            let area = f.size();

            // ── Centred card ─────────────────────────────────────────────────
            let card = centered_rect(50, 18, area);

            // Background block with title
            let outer = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(Span::styled(
                    " ydotooler ",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ));
            f.render_widget(outer, card);

            let inner = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints([
                    Constraint::Length(1), // subtitle
                    Constraint::Length(1), // spacer
                    Constraint::Min(0),    // content
                    Constraint::Length(1), // error / hint
                ])
                .split(card);

            let subtitle = Paragraph::new("ydotool script editor")
                .style(Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Center);
            f.render_widget(subtitle, inner[0]);

            match state {
                StartupState::Menu => {
                    let items: Vec<ListItem> = menu_items.iter()
                        .map(|m| ListItem::new(*m))
                        .collect();
                    let list = List::new(items)
                        .block(Block::default().borders(Borders::ALL)
                            .title(" What would you like to do? ")
                            .border_style(Style::default().fg(Color::Cyan)))
                        .highlight_style(Style::default().bg(Color::Yellow).fg(Color::Black).add_modifier(Modifier::BOLD))
                        .highlight_symbol("▶ ");
                    f.render_stateful_widget(list, inner[2], &mut menu_state);

                    let hint = Paragraph::new("↑↓ navigate   Enter select   Ctrl-C quit")
                        .style(Style::default().fg(Color::DarkGray))
                        .alignment(Alignment::Center);
                    f.render_widget(hint, inner[3]);
                }

                StartupState::NamingNew => {
                    let display = format!(" {}_", name_buf);
                    let input = Paragraph::new(display)
                        .block(Block::default().borders(Borders::ALL)
                            .title(" New project name (Enter to confirm, ESC to go back) ")
                            .border_style(Style::default().fg(Color::Green)));
                    f.render_widget(input, inner[2]);

                    let hint_text = if err_msg.is_empty() {
                        "A .ydotooler file will be created in the current directory.".to_string()
                    } else {
                        err_msg.clone()
                    };
                    let hint = Paragraph::new(hint_text)
                        .style(Style::default().fg(if err_msg.is_empty() { Color::DarkGray } else { Color::Red }))
                        .alignment(Alignment::Center);
                    f.render_widget(hint, inner[3]);
                }

                StartupState::BrowsingFiles => {
                    let chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Length(3), Constraint::Min(0)])
                        .split(inner[2]);

                    let search = Paragraph::new(format!(" {}_", file_query))
                        .block(Block::default().borders(Borders::ALL)
                            .title(" Search (ESC to go back) ")
                            .border_style(Style::default().fg(Color::Cyan)));
                    f.render_widget(search, chunks[0]);

                    let items: Vec<ListItem> = if filtered_files.is_empty() {
                        vec![ListItem::new(Span::styled(
                            "  No .ydotooler files found in current directory.",
                            Style::default().fg(Color::DarkGray),
                        ))]
                    } else {
                        filtered_files.iter()
                            .map(|f| ListItem::new(f.as_str()))
                            .collect()
                    };
                    let list = List::new(items)
                        .block(Block::default().borders(Borders::ALL).title(" .ydotooler files "))
                        .highlight_style(Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD))
                        .highlight_symbol("▶ ");
                    f.render_stateful_widget(list, chunks[1], &mut file_state);

                    let hint = Paragraph::new("↑↓ navigate   Enter load   ESC back")
                        .style(Style::default().fg(Color::DarkGray))
                        .alignment(Alignment::Center);
                    f.render_widget(hint, inner[3]);
                }
            }
        })?;

        if let Event::Key(key) = event::read()? {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Ok(None);
            }

            match state {
                // ── Main menu ────────────────────────────────────────────────
                StartupState::Menu => match key.code {
                    KeyCode::Up => {
                        let cur = menu_state.selected().unwrap_or(0);
                        menu_state.select(Some(cur.saturating_sub(1)));
                    }
                    KeyCode::Down => {
                        let cur = menu_state.selected().unwrap_or(0);
                        menu_state.select(Some((cur + 1).min(menu_items.len() - 1)));
                    }
                    KeyCode::Enter => match menu_state.selected().unwrap_or(0) {
                        0 => { // New
                            name_buf.clear();
                            err_msg.clear();
                            state = StartupState::NamingNew;
                        }
                        1 => { // Load
                            file_query.clear();
                            file_state.select(Some(0));
                            state = StartupState::BrowsingFiles;
                        }
                        _ => return Ok(None), // Quit
                    },
                    KeyCode::Char('q') => return Ok(None),
                    _ => {}
                },

                // ── New project name prompt ───────────────────────────────────
                StartupState::NamingNew => match key.code {
                    KeyCode::Esc => {
                        state = StartupState::Menu;
                        err_msg.clear();
                    }
                    KeyCode::Char(c) => { name_buf.push(c); err_msg.clear(); }
                    KeyCode::Backspace => { name_buf.pop(); err_msg.clear(); }
                    KeyCode::Enter => {
                        let name = name_buf.trim().to_string();
                        if name.is_empty() {
                            err_msg = "Name cannot be empty.".to_string();
                        } else {
                            let mut project = Project::new(&name);
                            project.load()?;
                            return Ok(Some(project));
                        }
                    }
                    _ => {}
                },

                // ── File browser ─────────────────────────────────────────────
                StartupState::BrowsingFiles => {
                    let n_files = filtered_files.len();
                    let cur = file_state.selected().unwrap_or(0);
                    match key.code {
                        KeyCode::Esc => { state = StartupState::Menu; }
                        KeyCode::Char(c) => { file_query.push(c); file_state.select(Some(0)); }
                        KeyCode::Backspace => { file_query.pop(); file_state.select(Some(0)); }
                        KeyCode::Up   => { file_state.select(Some(cur.saturating_sub(1))); }
                        KeyCode::Down => { if n_files > 0 { file_state.select(Some((cur + 1).min(n_files - 1))); } }
                        KeyCode::Enter => {
                            if let Some(chosen) = filtered_files.get(cur) {
                                let mut project = Project::new(chosen);
                                project.load()?;
                                return Ok(Some(project));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

// ─── Main loop ────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let project = match startup_screen(&mut terminal)? {
        Some(p) => p,
        None => {
            disable_raw_mode()?;
            execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
            terminal.show_cursor()?;
            return Ok(());
        }
    };
    terminal.clear()?;

    let mut app = App::new(project);
    let result = run_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;

    if let Err(e) = &result {
        if e.to_string() != "quit" { eprintln!("Error: {e}"); }
    }
    Ok(())
}

fn run_loop(terminal: &mut Term, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| draw(f, app))?;

        if let Event::Key(key) = event::read()? {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Err(anyhow::anyhow!("quit"));
            }

            // ── Inline prompt mode intercepts all keys ──────────────────────
            if app.is_prompting() {
                match key.code {
                    KeyCode::Enter => confirm_prompt(app),
                    KeyCode::Esc   => {
                        app.input_mode = InputMode::Normal;
                        app.status = "Cancelled.".to_string();
                    }
                    KeyCode::Backspace => {
                        if let InputMode::Prompting { buffer, .. } = &mut app.input_mode {
                            buffer.pop();
                        }
                    }
                    KeyCode::Char(c) => {
                        if let InputMode::Prompting { buffer, .. } = &mut app.input_mode {
                            buffer.push(c);
                        }
                    }
                    _ => {}
                }
                continue;
            }

            // ── Normal mode ─────────────────────────────────────────────────
            if key.code == KeyCode::Tab {
                app.focus = match app.focus { Focus::Menu => Focus::Events, Focus::Events => Focus::Menu };
                continue;
            }
            match app.focus {
                Focus::Menu   => handle_menu_key(terminal, app, key.code)?,
                Focus::Events => handle_event_pane_key(app, key.code),
            }
        }
    }
}

fn handle_menu_key(terminal: &mut Term, app: &mut App, code: KeyCode) -> Result<()> {
    match code {
        KeyCode::Up   => { let c = app.menu_sel(); app.menu_state.select(Some(c.saturating_sub(1))); }
        KeyCode::Down => { let c = app.menu_sel(); app.menu_state.select(Some((c+1).min(MENU.len()-1))); }
        KeyCode::Char('q') => return Err(anyhow::anyhow!("quit")),
        KeyCode::Enter => {
            match MENU[app.menu_sel()] {
                "Add key"        => action_add_key(terminal, app)?,
                "Add delay"      => app.start_prompt("Delay (seconds, e.g. 0.5) → ", PromptTarget::AddDelay),
                "Add type"       => app.start_prompt("Type text → ", PromptTarget::AddType),
                "Add loop block" => app.start_prompt("Loop count (0=∞) → ", PromptTarget::AddLoopBlock),
                "Move up"        => {
                    let sel = app.effective_selection();
                    move_indices_up(&mut app.project.events, &mut app.selected, &sel);
                    if let Some(&first) = sel.first() {
                        app.event_state.select(Some(first.saturating_sub(1)));
                    }
                    if let Err(e) = app.project.save() { app.status = format!("Save error: {e}"); }
                }
                "Move down"      => {
                    let sel = app.effective_selection();
                    move_indices_down(&mut app.project.events, &mut app.selected, &sel);
                    if let Some(&last) = sel.last() {
                        let new = (last + 1).min(app.project.events.len() - 1);
                        app.event_state.select(Some(new));
                    }
                    if let Err(e) = app.project.save() { app.status = format!("Save error: {e}"); }
                }
                "Edit event"     => action_edit(terminal, app)?,
                "Delete event"   => {
                    action_delete(app);
                    if let Err(e) = app.project.save() { app.status = format!("Save error: {e}"); }
                }
                "Set auto delay" => app.start_prompt("Auto delay (0=off, e.g. 0.1) → ", PromptTarget::SetAutoDelay),
                "Set global loop"=> app.start_prompt("Global loop count (0=off, e.g. 5) → ", PromptTarget::SetGlobalLoop),
                "Save"           => { app.project.save()?; app.status = "Saved.".to_string(); }
                "Load"           => { app.project.load()?; app.clamp_cursor(); app.status = "Loaded.".to_string(); }
                "Generate"       => action_generate(app)?,
                "Quit"           => return Err(anyhow::anyhow!("quit")),
                _ => {}
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_event_pane_key(app: &mut App, code: KeyCode) {
    let len = app.project.events.len();
    if len == 0 { return; }
    let cur = app.event_state.selected().unwrap_or(0);

    match code {
        KeyCode::Up   => { app.event_state.select(Some(cur.saturating_sub(1))); }
        KeyCode::Down => { app.event_state.select(Some((cur+1).min(len-1))); }

        // Space = toggle multi-select on cursor
        KeyCode::Char(' ') => {
            app.sync_selected_len();
            app.selected[cur] = !app.selected[cur];
            // Advance cursor after toggle
            app.event_state.select(Some((cur+1).min(len-1)));
        }

        // k/j = move cursor item (or selection) up/down
        KeyCode::Char('k') => {
            let sel = app.effective_selection();
            move_indices_up(&mut app.project.events, &mut app.selected, &sel);
            if let Some(&first) = sel.first() { app.event_state.select(Some(first.saturating_sub(1))); }
            let _ = app.project.save();
        }
        KeyCode::Char('j') => {
            let sel = app.effective_selection();
            move_indices_down(&mut app.project.events, &mut app.selected, &sel);
            if let Some(&last) = sel.last() {
                let new = (last+1).min(app.project.events.len()-1);
                app.event_state.select(Some(new));
            }
            let _ = app.project.save();
        }

        // Escape = clear multi-selection
        KeyCode::Esc => { app.clear_selection(); app.status = "Selection cleared.".to_string(); }

        KeyCode::Char('u') => {
            app.duplicate_selection();
            let _ = app.project.save();
            app.status = "Duplicated.".to_string();
        }
        KeyCode::Char('d') => {
            action_delete(app);
            let _ = app.project.save();
        }
        _ => {}
    }
    app.sync_preview_scroll();
}
