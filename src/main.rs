use anyhow::{bail, Context, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
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
enum Mode {
    PressRelease,
    PressOnly,
    ReleaseOnly,
}

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
    fn all() -> &'static [&'static str] {
        &["press+release", "press only", "release only"]
    }
}

#[derive(Debug, Clone)]
enum EventKind {
    Key { code: String, name: String, mode: Mode },
    Delay(String),
}

impl EventKind {
    fn display(&self) -> String {
        match self {
            EventKind::Key { code, name, mode } =>
                format!("KEY {} ({}) {}", name, code, mode.as_str()),
            EventKind::Delay(d) =>
                format!("DELAY {}s", d),
        }
    }
    fn bash_lines(&self, auto_delay: &str) -> Vec<String> {
        let mut out = Vec::new();
        match self {
            EventKind::Key { code, name, mode } => {
                let line = match mode {
                    Mode::PressOnly    => format!("ydotool key {}:1 #{}", code, name),
                    Mode::ReleaseOnly  => format!("ydotool key {}:0 #{}", code, name),
                    Mode::PressRelease => format!("ydotool key {}:1 {}:0 #{}", code, code, name),
                };
                out.push(line);
                if auto_delay != "0" {
                    out.push(format!("sleep {}", auto_delay));
                }
            }
            EventKind::Delay(d) => out.push(format!("sleep {}", d)),
        }
        out
    }
}

// ─── Project ─────────────────────────────────────────────────────────────────

struct Project {
    name: String,
    project_file: PathBuf,
    output_file: PathBuf,
    auto_delay: String,
    events: Vec<EventKind>,
}

impl Project {
    fn new(name: &str) -> Self {
        let base = name
            .trim_end_matches(".ydotooler")
            .trim_end_matches(".sh");
        Project {
            name: base.to_string(),
            project_file: PathBuf::from(format!("{}.ydotooler", base)),
            output_file: PathBuf::from(format!("{}.sh", base)),
            auto_delay: "0".to_string(),
            events: Vec::new(),
        }
    }

    fn load(&mut self) -> Result<()> {
        self.events.clear();
        self.auto_delay = "0".to_string();
        if !self.project_file.exists() {
            fs::write(&self.project_file, "#AUTO_DELAY=0\n")?;
            return Ok(());
        }
        let f = fs::File::open(&self.project_file)?;
        for line in BufReader::new(f).lines() {
            let line = line?.trim_end_matches('\r').to_string();
            if line.starts_with("#AUTO_DELAY=") {
                self.auto_delay = line["#AUTO_DELAY=".len()..].to_string();
                continue;
            }
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.is_empty() || parts[0].is_empty() { continue; }
            match parts[0] {
                "KEY" if parts.len() == 4 => self.events.push(EventKind::Key {
                    code: parts[1].to_string(),
                    name: parts[2].to_string(),
                    mode: Mode::from_str(parts[3]),
                }),
                "DELAY" if parts.len() >= 2 => self.events.push(EventKind::Delay(parts[1].to_string())),
                _ => {}
            }
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let mut f = fs::File::create(&self.project_file)?;
        writeln!(f, "#AUTO_DELAY={}", self.auto_delay)?;
        for ev in &self.events {
            match ev {
                EventKind::Key { code, name, mode } =>
                    writeln!(f, "KEY|{}|{}|{}", code, name, mode.as_str())?,
                EventKind::Delay(d) =>
                    writeln!(f, "DELAY|{}||", d)?,
            }
        }
        Ok(())
    }

    fn write_script(&self) -> Result<PathBuf> {
        let mut f = fs::File::create(&self.output_file)?;
        writeln!(f, "#!/usr/bin/env bash")?;
        writeln!(f, "# Generated by ydotooler")?;
        writeln!(f)?;
        for ev in &self.events {
            for line in ev.bash_lines(&self.auto_delay) {
                writeln!(f, "{}", line)?;
            }
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
        let mut lines = vec![
            "#!/usr/bin/env bash".to_string(),
            "# Generated by ydotooler".to_string(),
            String::new(),
        ];
        for ev in &self.events {
            lines.extend(ev.bash_lines(&self.auto_delay));
        }
        lines.join("\n")
    }
}

// ─── Terminal helpers ─────────────────────────────────────────────────────────

/// Suspend the TUI, run a plain-terminal closure, then restore and clear.
fn with_plain_terminal<F, T>(terminal: &mut Term, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;

    let result = f();

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    // Force a full repaint — this is the key fix.
    terminal.clear()?;

    result
}

fn prompt_line(terminal: &mut Term, label: &str) -> Result<Option<String>> {
    with_plain_terminal(terminal, || {
        print!("{}", label);
        io::stdout().flush()?;
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        let val = buf.trim().to_string();
        Ok(if val.is_empty() { None } else { Some(val) })
    })
}

// ─── In-TUI pickers (reuse the existing terminal, no new Terminal::new) ───────

fn pick_key_code(terminal: &mut Term, codes_path: &Path) -> Result<Option<String>> {
    let content = fs::read_to_string(codes_path)?;
    let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let mut query = String::new();
    let mut list_state = ListState::default();
    list_state.select(Some(0));

    // Clear before taking over the screen
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
            let items: Vec<ListItem> = filtered.iter()
                .map(|l| ListItem::new(l.as_str())).collect();
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(" Key codes "))
                .highlight_style(Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD))
                .highlight_symbol("▶ ");
            f.render_stateful_widget(list, chunks[1], &mut list_state);
        })?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Esc     => { terminal.clear()?; return Ok(None); }
                KeyCode::Enter   => {
                    let chosen = filtered.get(sel).map(|s| s.to_string());
                    terminal.clear()?;
                    return Ok(chosen);
                }
                KeyCode::Char(c) => { query.push(c); list_state.select(Some(0)); }
                KeyCode::Backspace => { query.pop(); list_state.select(Some(0)); }
                KeyCode::Down    => { let n = (sel + 1).min(filtered.len().saturating_sub(1)); list_state.select(Some(n)); }
                KeyCode::Up      => { list_state.select(Some(sel.saturating_sub(1))); }
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
                KeyCode::Down  => list_state.select(Some((sel + 1).min(options.len() - 1))),
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
    "Move up",
    "Move down",
    "Edit event",
    "Delete event",
    "Set auto delay",
    "Save",
    "Load",
    "Generate",
    "Quit",
];

#[derive(PartialEq)]
enum Focus { Menu, Events }

struct App {
    project: Project,
    menu_state: ListState,
    event_state: ListState,
    status: String,
    focus: Focus,
}

impl App {
    fn new(project: Project) -> Self {
        let mut menu_state = ListState::default();
        menu_state.select(Some(0));
        let mut event_state = ListState::default();
        if !project.events.is_empty() { event_state.select(Some(0)); }
        App {
            project,
            menu_state,
            event_state,
            status: "Ready — Tab to switch pane".to_string(),
            focus: Focus::Menu,
        }
    }
    fn menu_sel(&self) -> usize { self.menu_state.selected().unwrap_or(0) }
    fn event_sel(&self) -> Option<usize> { self.event_state.selected() }
    fn clamp_event_sel(&mut self) {
        let len = self.project.events.len();
        if len == 0 {
            self.event_state.select(None);
        } else {
            let cur = self.event_state.selected().unwrap_or(0).min(len - 1);
            self.event_state.select(Some(cur));
        }
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
    let focused = app.focus == Focus::Menu;
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
    let focused = app.focus == Focus::Events;
    let border_style = if focused { Style::default().fg(Color::Cyan) } else { Style::default().fg(Color::DarkGray) };
    let items: Vec<ListItem> = app.project.events.iter().enumerate()
        .map(|(i, ev)| {
            let (icon, color) = match ev {
                EventKind::Key { .. } => ("⌨ ", Color::Green),
                EventKind::Delay(_)   => ("⏱ ", Color::Magenta),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:>3} ", i), Style::default().fg(Color::DarkGray)),
                Span::styled(icon, Style::default().fg(color)),
                Span::raw(ev.display()),
            ]))
        })
        .collect();
    let title = format!(" Events ({}) ", app.project.events.len());
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL)
            .title(Span::styled(title, Style::default().add_modifier(Modifier::BOLD)))
            .border_style(border_style))
        .highlight_style(Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, area, &mut app.event_state);
}

fn draw_bash_preview(f: &mut Frame, app: &mut App, area: Rect) {
    let preview = app.project.bash_preview();
    let lines: Vec<Line> = preview.lines().map(|line| {
        if line.starts_with('#') {
            Line::from(Span::styled(line.to_string(), Style::default().fg(Color::DarkGray)))
        } else if line.starts_with("ydotool") {
            let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
            Line::from(vec![
                Span::styled(format!("{} ", cmd), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(rest.to_string(), Style::default().fg(Color::Green)),
            ])
        } else if line.starts_with("sleep") {
            let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
            Line::from(vec![
                Span::styled(format!("{} ", cmd), Style::default().fg(Color::Magenta)),
                Span::styled(rest.to_string(), Style::default().fg(Color::White)),
            ])
        } else if line.starts_with("#!/") {
            Line::from(Span::styled(line.to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)))
        } else {
            Line::from(Span::raw(line.to_string()))
        }
    }).collect();

    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL)
            .title(Span::styled(
                format!(" Bash Preview → {} ", app.project.output_file.display()),
                Style::default().add_modifier(Modifier::BOLD),
            ))
            .border_style(Style::default().fg(Color::DarkGray)))
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn draw_statusbar(f: &mut Frame, app: &App, area: Rect) {
    let help = match app.focus {
        Focus::Menu   => "↑↓ navigate  Enter select  Tab→Events  q quit",
        Focus::Events => "↑↓ navigate  d delete  k up  j down  Tab→Menu",
    };
    let text = format!(
        " {} │ auto_delay={} │ {}  │  {}",
        app.project.name, app.project.auto_delay, help, app.status,
    );
    let para = Paragraph::new(text)
        .style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(para, area);
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
        (sel[..pos].trim().to_string(), sel[pos + 1..].trim().to_string())
    } else {
        (sel.trim().to_string(), sel.trim().to_string())
    };
    let mode_str = match pick_from_list(terminal, "Key mode", Mode::all())? { Some(s) => s, None => return Ok(()) };
    app.project.events.push(EventKind::Key { code, name, mode: Mode::from_str(&mode_str) });
    app.clamp_event_sel();
    app.status = "Key added.".to_string();
    Ok(())
}

fn action_add_delay(terminal: &mut Term, app: &mut App) -> Result<()> {
    if let Some(d) = prompt_line(terminal, "Delay (seconds, e.g. 0.5): ")? {
        app.project.events.push(EventKind::Delay(d));
        app.clamp_event_sel();
        app.status = "Delay added.".to_string();
    }
    Ok(())
}

fn action_edit(terminal: &mut Term, app: &mut App) -> Result<()> {
    let idx = match app.event_sel() {
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
            }
        }
        EventKind::Delay(_) => {
            if let Some(d) = prompt_line(terminal, "New delay (seconds): ")? {
                app.project.events[idx] = EventKind::Delay(d);
                app.status = "Delay updated.".to_string();
            }
        }
    }
    Ok(())
}

fn action_delete(app: &mut App) {
    match app.event_sel() {
        Some(idx) => { app.project.events.remove(idx); app.clamp_event_sel(); app.status = "Event deleted.".to_string(); }
        None => { app.status = "No event selected.".to_string(); }
    }
}

fn action_move_up(app: &mut App) {
    if let Some(idx) = app.event_sel() {
        if idx > 0 {
            app.project.events.swap(idx, idx - 1);
            app.event_state.select(Some(idx - 1));
            app.status = "Moved up.".to_string();
        }
    }
}

fn action_move_down(app: &mut App) {
    if let Some(idx) = app.event_sel() {
        if idx + 1 < app.project.events.len() {
            app.project.events.swap(idx, idx + 1);
            app.event_state.select(Some(idx + 1));
            app.status = "Moved down.".to_string();
        }
    }
}

fn action_set_auto_delay(terminal: &mut Term, app: &mut App) -> Result<()> {
    if let Some(d) = prompt_line(terminal, "Auto delay (0 for off): ")? {
        app.project.auto_delay = d;
        app.status = "Auto delay updated.".to_string();
    }
    Ok(())
}

fn action_generate(app: &mut App) -> Result<()> {
    let path = app.project.write_script()?;
    let abs = fs::canonicalize(&path).unwrap_or(path);
    app.status = format!("Generated: {}", abs.display());
    Ok(())
}

// ─── Main loop ────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    print!("Project name: ");
    io::stdout().flush()?;
    let mut name_buf = String::new();
    io::stdin().read_line(&mut name_buf)?;
    let name = name_buf.trim().to_string();
    if name.is_empty() { bail!("No project name given."); }

    let mut project = Project::new(&name);
    project.load()?;

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
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
        KeyCode::Down => { let c = app.menu_sel(); app.menu_state.select(Some((c + 1).min(MENU.len() - 1))); }
        KeyCode::Char('q') => return Err(anyhow::anyhow!("quit")),
        KeyCode::Enter => {
            let mutating = matches!(MENU[app.menu_sel()],
                "Add key" | "Add delay" | "Move up" | "Move down" |
                "Edit event" | "Delete event" | "Set auto delay");

            match MENU[app.menu_sel()] {
                "Add key"        => action_add_key(terminal, app)?,
                "Add delay"      => action_add_delay(terminal, app)?,
                "Move up"        => action_move_up(app),
                "Move down"      => action_move_down(app),
                "Edit event"     => action_edit(terminal, app)?,
                "Delete event"   => action_delete(app),
                "Set auto delay" => action_set_auto_delay(terminal, app)?,
                "Save"           => { app.project.save()?; app.status = "Saved.".to_string(); }
                "Load"           => { app.project.load()?; app.clamp_event_sel(); app.status = "Loaded.".to_string(); }
                "Generate"       => action_generate(app)?,
                "Quit"           => return Err(anyhow::anyhow!("quit")),
                _ => {}
            }
            if mutating {
                if let Err(e) = app.project.save() {
                    app.status = format!("Auto-save error: {e}");
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_event_pane_key(app: &mut App, code: KeyCode) {
    let len = app.project.events.len();
    if len == 0 { return; }
    let sel = app.event_state.selected().unwrap_or(0);
    match code {
        KeyCode::Up        => { app.event_state.select(Some(sel.saturating_sub(1))); }
        KeyCode::Down      => { app.event_state.select(Some((sel + 1).min(len - 1))); }
        KeyCode::Char('d') => action_delete(app),
        KeyCode::Char('k') => action_move_up(app),
        KeyCode::Char('j') => action_move_down(app),
        _ => {}
    }
}
