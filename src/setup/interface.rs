use super::*;

#[cfg(unix)]
/// The value a pick writes, as the confirmation's Setting row shows it.
fn written(pick: Pick) -> String {
    match pick {
        Pick::Mode(mode) => format!("\"{}\"", mode.name()),
        Pick::Number(value) => value.to_string(),
        Pick::Role(role) => format!("\"{}\"", role.name()),
        Pick::Inherit => "(removed)".into(),
        Pick::Flag(on) => on.to_string(),
    }
}

#[cfg(unix)]
/// The confirmation's rows for a caution pick: the setting, its effect, why it is a caution
/// option and where the acknowledgement goes, with the reason wrapped to `width`.
fn confirmation_rows(
    state: &State,
    config: &Config,
    field: Field,
    pick: Pick,
    option: &str,
    width: usize,
) -> Vec<String> {
    let mut why = wrap(
        justification(option),
        width.saturating_sub(15).clamp(40, 86),
    )
    .into_iter();
    let mut rows = vec![
        ("Setting", format!("{} = {}", field.key(), written(pick))),
        (
            "Effect",
            capitalize(&consequence(field, pick, config, state.host.as_ref())),
        ),
        ("Why", why.next().unwrap_or_default()),
    ];
    rows.extend(why.map(|line| ("", line)));
    rows.push((
        "Records",
        format!("{option} in contention.acknowledge_caution"),
    ));
    key_values(&rows, 2)
}

#[cfg(unix)]
fn cursor_control() -> bool {
    std::env::var("TERM").map_or(true, |term| term != "dumb" && !term.is_empty())
}

#[cfg(unix)]
/// Whether the locale says UTF-8, so list glyphs may leave ASCII.
fn unicode() -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("utf-8") || value.contains("utf8")
        })
}

#[cfg(unix)]
/// List glyphs: Unicode where the locale allows it, ASCII otherwise.
#[derive(Clone, Copy, Debug)]
pub struct Glyphs {
    /// Points at the highlighted row of the main list.
    pub pointer: &'static str,
    /// Marks the highlighted option in a list that picks one value.
    pub chosen: &'static str,
    /// Marks every other option there.
    pub other: &'static str,
    /// The keys that move the highlight.
    pub arrows: &'static str,
}

#[cfg(unix)]
impl Glyphs {
    pub fn detect() -> Self {
        if unicode() {
            Self {
                pointer: "❯",
                chosen: "●",
                other: "○",
                arrows: "↑/↓",
            }
        } else {
            Self {
                pointer: ">",
                chosen: ">",
                other: " ",
                arrows: "Up/Down",
            }
        }
    }
}

#[cfg(unix)]
/// The first row a list of `total` rows shows in `visible` rows so that `cursor` stays in view,
/// keeping it near the middle once the list scrolls.
pub fn window_start(total: usize, visible: usize, cursor: usize) -> usize {
    if visible == 0 || total <= visible {
        return 0;
    }
    cursor.saturating_sub(visible / 2).min(total - visible)
}

pub fn run(target: &Target, flags: Choices, host: Option<&Host>) -> Result<Option<Choices>> {
    let role = match (target.role, flags.role) {
        (Some(role), _) | (None, Some(role)) => role,
        (None, None) => {
            if cfg!(windows) {
                Role::Client
            } else {
                Role::Node
            }
        }
    };
    if role != Role::Node {
        return prompts::connection(target, role, flags);
    }
    let (_, base) = node_base(target)?;
    // The interface takes arrow keys and Enter only; a terminal that cannot move the cursor gets
    // the flags instead of a second, typed interface (FR-055).
    #[cfg(unix)]
    if cursor_control() {
        let mut state = State::new(target.clone(), base, flags, host.cloned());
        return match screen::run(&mut state, Style::detect().color, Glyphs::detect())? {
            screen::Outcome::Done(choices) => Ok(Some(choices)),
            screen::Outcome::Cancelled => Ok(None),
            screen::Outcome::Role(role) => prompts::connection(target, role, Choices::default()),
        };
    }
    #[cfg(not(unix))]
    let _ = (base, flags, host);
    Err(fail(
        2,
        "ERR_CEDEGRID_SETUP_TERMINAL",
        "this terminal cannot run setup's interface, which needs cursor movement",
    ))
}

/// The interface's choices so far, on top of the current configuration.
pub struct State {
    pub target: Target,
    pub base: Config,
    pub values: BTreeMap<Field, Pick>,
    pub acknowledged: BTreeSet<String>,
    pub host: Option<Host>,
    pub fresh: bool,
    flags: Choices,
}

impl State {
    pub fn new(target: Target, base: Config, flags: Choices, host: Option<Host>) -> Self {
        let fresh = target.original.is_none();
        let mut values = BTreeMap::new();
        // Start from the file, with any flags already applied.
        let preview = plan(&target.path, &flags, host.as_ref())
            .ok()
            .and_then(|plan| plan.config)
            .unwrap_or_else(|| base.clone());
        for field in node_fields() {
            values.insert(field, field.current(&preview));
        }
        if let Some(on) = preselected_priority(&target, &flags) {
            values.insert(Field::PlainPriority, Pick::Flag(on));
        }
        Self {
            acknowledged: flags.acknowledge.clone(),
            target,
            base,
            values,
            host,
            fresh,
            flags,
        }
    }

    pub fn rows(&self) -> Vec<Field> {
        node_fields()
    }

    pub fn choices(&self) -> Choices {
        let mut choices = self.flags.clone();
        choices.role = Some(Role::Node);
        for (field, pick) in &self.values {
            if *field != Field::Role {
                field.apply(&mut choices, *pick);
            }
        }
        choices.acknowledge = self.acknowledged.clone();
        choices
    }

    /// The configuration the current choices give, for consequence lines.
    pub fn preview(&self) -> Config {
        let mut choices = self.choices();
        choices.acknowledge = ACKNOWLEDGEMENTS.iter().map(|s| s.to_string()).collect();
        plan(&self.target.path, &choices, self.host.as_ref())
            .ok()
            .and_then(|plan| plan.config)
            .unwrap_or_else(|| self.base.clone())
    }

    pub fn risk(&self, field: Field, pick: Pick) -> Option<&'static str> {
        risk(&self.target, field, pick, self.host.as_ref())
    }

    /// Whether choosing `option` needs the confirmation screen.
    pub fn needs_confirmation(&self, option: &str) -> bool {
        !self.acknowledged.contains(option)
            && !self.base.contention.acknowledge_caution.contains(option)
    }

    pub fn plan(&self) -> Result<Plan> {
        plan(&self.target.path, &self.choices(), self.host.as_ref())
    }
}

/// Typed prompts for a coordinator's or client's addresses and file paths, which are text.
mod prompts {
    use super::*;

    fn ask(question: &str) -> Result<Option<String>> {
        print!("{question}");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line)? == 0 {
            return Ok(None);
        }
        Ok(Some(line.trim().to_string()))
    }

    pub fn connection(target: &Target, role: Role, flags: Choices) -> Result<Option<Choices>> {
        println!("{}", Style::detect().bold("CedeGrid setup"));
        println!("Role:   {}", role.name());
        println!("Config: {}", target.path.display());
        println!();
        let current: Option<DocumentMut> =
            target.original.as_ref().and_then(|text| text.parse().ok());
        let mut choices = flags;
        choices.role = Some(role);
        for key in ConnectionKey::for_role(role) {
            let (path, name) = key.path();
            if *key == ConnectionKey::OperatorFingerprint && current.is_some() {
                // An existing coordinator keeps its authorized clients.
                continue;
            }
            let existing = choices.connection.get(key).cloned().or_else(|| {
                let mut item = current.as_ref().map(|document| document.as_item())?;
                for segment in path {
                    item = item.get(segment)?;
                }
                item.get(name)?.as_str().map(str::to_string)
            });
            let shown = existing.clone().unwrap_or_default();
            let Some(answer) = ask(&format!("{} [{shown}]: ", key.label()))? else {
                return Ok(None);
            };
            match (answer.is_empty(), existing) {
                (false, _) => {
                    choices.connection.insert(*key, answer);
                }
                (true, Some(value)) => {
                    choices.connection.insert(*key, value);
                }
                (true, None) => {
                    return Err(missing(format!("{} needs a value", key.label())));
                }
            }
        }
        let plan = plan(&target.path, &choices, None)?;
        for line in summary_lines(&plan, None) {
            println!("{line}");
        }
        let Some(answer) = ask("Write this configuration? [y/N] ")? else {
            return Ok(None);
        };
        Ok(matches!(answer.as_str(), "y" | "Y" | "yes").then_some(choices))
    }
}

/// The keyboard interface (003 FR-030, FR-031): a host summary header, the settings grouped
/// into Modes, Safety floors and Advanced, and a side pane that explains the selected setting
/// with this host's numbers. Up and Down move, Left and Right change the value in place, Enter
/// reviews and writes, Esc cancels. A caution value keeps its separate confirmation.
#[cfg(unix)]
pub mod screen {
    use super::*;
    use crate::tui_style::Palette;
    use ratatui::Frame;
    use ratatui::layout::{Constraint, Direction, Layout, Rect};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
    pub use terminal::Key;

    pub enum Outcome {
        Done(Choices),
        Cancelled,
        Role(Role),
    }

    /// The setting groups, in order.
    pub const SECTIONS: [&str; 3] = ["Modes", "Safety floors", "Advanced"];

    /// The group a setting belongs to.
    pub fn section(field: Field) -> usize {
        match field {
            Field::Role
            | Field::DefaultMode
            | Field::Mode(_)
            | Field::MaxMode
            | Field::PlainPriority => 0,
            Field::RamFloor | Field::GpuFloor => 1,
            Field::Margin(..)
            | Field::HintDeadline
            | Field::NoiseTolerance
            | Field::AttributionTolerance => 2,
        }
    }

    pub enum View {
        Main,
        Confirm {
            field: Field,
            pick: Pick,
            kept: Pick,
            option: &'static str,
            cursor: usize,
        },
        Summary(Box<Result<Plan, String>>),
    }

    /// The screen between keys: the settings in group order, the highlighted row (the settings,
    /// then Review and write, then Cancel) and what is shown.
    pub struct Screen {
        pub rows: Vec<Field>,
        pub row: usize,
        pub view: View,
        pub notice: Option<String>,
    }

    impl Screen {
        pub fn new(state: &State) -> Self {
            let mut rows = state.rows();
            rows.sort_by_key(|field| section(*field));
            Self {
                rows,
                row: 0,
                view: View::Main,
                notice: None,
            }
        }

        /// One key; an outcome ends the screen.
        pub fn key(&mut self, state: &mut State, key: Key) -> Option<Outcome> {
            let total = self.rows.len() + 2;
            let view = std::mem::replace(&mut self.view, View::Main);
            self.view = match view {
                View::Main => match key {
                    Key::Cancel => return Some(Outcome::Cancelled),
                    Key::Up => {
                        self.row = (self.row + total - 1) % total;
                        View::Main
                    }
                    Key::Down => {
                        self.row = (self.row + 1) % total;
                        View::Main
                    }
                    Key::Left | Key::Right if self.row < self.rows.len() => {
                        let field = self.rows[self.row];
                        let options = field.options(&state.preview(), state.fresh);
                        let kept = state.values[&field];
                        let at = options.iter().position(|pick| *pick == kept).unwrap_or(0);
                        let next = if key == Key::Left {
                            at.saturating_sub(1)
                        } else {
                            (at + 1).min(options.len().saturating_sub(1))
                        };
                        match options.get(next).copied() {
                            Some(Pick::Role(role)) if role != Role::Node => {
                                return Some(Outcome::Role(role));
                            }
                            Some(pick) if pick != kept => match state.risk(field, pick) {
                                Some(option) if state.needs_confirmation(option) => View::Confirm {
                                    field,
                                    pick,
                                    kept,
                                    option,
                                    cursor: 0,
                                },
                                _ => {
                                    state.values.insert(field, pick);
                                    View::Main
                                }
                            },
                            _ => View::Main,
                        }
                    }
                    Key::Enter if self.row == self.rows.len() => {
                        self.notice = None;
                        View::Summary(Box::new(state.plan().map_err(|error| error.to_string())))
                    }
                    Key::Enter if self.row == self.rows.len() + 1 => {
                        return Some(Outcome::Cancelled);
                    }
                    _ => View::Main,
                },
                View::Confirm {
                    field,
                    pick,
                    kept,
                    option,
                    cursor,
                } => match key {
                    // Esc keeps the value; only Esc on the main screen cancels setup.
                    Key::Cancel => View::Main,
                    Key::Up | Key::Down | Key::Left | Key::Right => View::Confirm {
                        field,
                        pick,
                        kept,
                        option,
                        cursor: 1 - cursor,
                    },
                    Key::Enter => {
                        if cursor == 1 {
                            state.acknowledged.insert(option.to_string());
                            state.values.insert(field, pick);
                        }
                        View::Main
                    }
                    _ => View::Confirm {
                        field,
                        pick,
                        kept,
                        option,
                        cursor,
                    },
                },
                View::Summary(plan) => match (key, *plan) {
                    (Key::Enter, Ok(_)) => return Some(Outcome::Done(state.choices())),
                    (Key::Enter, Err(error)) => {
                        self.notice = Some(error);
                        View::Main
                    }
                    (Key::Cancel, _) => View::Main,
                    (_, plan) => View::Summary(Box::new(plan)),
                },
            };
            None
        }
    }

    /// The host summary line: its CPU, memory and each GPU's memory.
    pub fn host_line(host: Option<&Host>) -> String {
        let Some(host) = host else {
            return "Host: not measured".into();
        };
        let mut parts = vec![
            format!("{} CPUs", group(host.cpu_millicores / 1000)),
            format!("{} MiB RAM", group(host.ram_mib)),
        ];
        if host.gpus.is_empty() {
            parts.push("no GPU".into());
        }
        for (uuid, total) in &host.gpus {
            parts.push(format!("{uuid} with {} MiB", group(*total)));
        }
        format!("Host: {}", parts.join(", "))
    }

    /// One frame.
    pub fn draw(frame: &mut Frame, state: &State, screen: &Screen, color: bool, glyphs: Glyphs) {
        let palette = Palette { color };
        let area = frame.area();
        let [header, body, footer] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(4),
                Constraint::Length(2),
            ])
            .areas(area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("CedeGrid setup", palette.title()),
                Line::raw(host_line(state.host.as_ref())),
                Line::raw(format!("Configuration: {}", state.target.path.display())),
            ]),
            header,
        );
        let config = state.preview();
        let hint = match &screen.view {
            View::Main => format!(
                "{} move, Left/Right change, Enter on Review writes, Esc cancels",
                glyphs.arrows
            ),
            View::Confirm { .. } => "Up/Down choose, Enter applies, Esc keeps the value".into(),
            View::Summary(_) => "Enter writes, Esc goes back".into(),
        };
        let mut footer_lines = Vec::new();
        if let Some(notice) = &screen.notice {
            footer_lines.push(Line::from(vec![
                Span::styled("Cannot write: ", palette.refusal()),
                Span::raw(notice.clone()),
            ]));
        }
        footer_lines.push(Line::styled(hint, palette.dim()));
        frame.render_widget(Paragraph::new(footer_lines), footer);
        match &screen.view {
            View::Main => main(frame, body, state, screen, &config, palette, glyphs),
            View::Confirm {
                field,
                pick,
                kept,
                option,
                cursor,
            } => {
                let mut lines = vec![
                    Line::styled("Confirm a caution option", palette.title()),
                    Line::raw(""),
                ];
                lines.extend(
                    confirmation_rows(
                        state,
                        &config,
                        *field,
                        *pick,
                        option,
                        usize::from(body.width),
                    )
                    .into_iter()
                    .map(Line::raw),
                );
                lines.push(Line::raw(""));
                for (index, text) in [
                    format!("No, keep {}", field.show(*kept, &config)),
                    format!(
                        "Yes, use {} and record {option}",
                        field.show(*pick, &config)
                    ),
                ]
                .into_iter()
                .enumerate()
                {
                    let mark = if index == *cursor {
                        glyphs.chosen
                    } else {
                        glyphs.other
                    };
                    let style = if index == *cursor {
                        palette.selected()
                    } else {
                        Default::default()
                    };
                    lines.push(Line::from(vec![
                        Span::raw(format!("  {mark} ")),
                        Span::styled(text, style),
                    ]));
                }
                frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
            }
            View::Summary(plan) => {
                let mut lines = vec![
                    Line::styled("Review before writing", palette.title()),
                    Line::raw(""),
                ];
                match plan.as_ref() {
                    Ok(plan) => {
                        lines.extend(summary_lines(plan, state.host.as_ref()).into_iter().map(
                            |line| {
                                if line.contains(CAUTION) {
                                    Line::styled(line, palette.caution())
                                } else {
                                    Line::raw(line)
                                }
                            },
                        ))
                    }
                    Err(error) => lines.push(Line::from(vec![
                        Span::styled("Cannot write: ", palette.refusal()),
                        Span::raw(error.clone()),
                    ])),
                }
                frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
            }
        }
    }

    /// The grouped settings on the left and the side pane on the right.
    fn main(
        frame: &mut Frame,
        body: Rect,
        state: &State,
        screen: &Screen,
        config: &Config,
        palette: Palette,
        glyphs: Glyphs,
    ) {
        let [list, pane] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
            .areas(body);
        let label_width = screen
            .rows
            .iter()
            .map(|field| field.label().chars().count())
            .max()
            .unwrap_or(0)
            + 2;
        // Each line with the row it shows, so the window can keep the highlight in view.
        let mut lines: Vec<(Option<usize>, Line)> = Vec::new();
        let mut last = None;
        for (index, field) in screen.rows.iter().enumerate() {
            let group = section(*field);
            if last != Some(group) {
                if last.is_some() {
                    lines.push((None, Line::raw("")));
                }
                lines.push((None, Line::styled(SECTIONS[group], palette.title())));
                last = Some(group);
            }
            let pick = state.values[field];
            let selected = index == screen.row;
            let mut spans = vec![
                Span::raw(if selected {
                    format!("{} ", glyphs.pointer)
                } else {
                    "  ".into()
                }),
                Span::styled(
                    format!("{:<label_width$}", field.label()),
                    if selected {
                        palette.selected()
                    } else {
                        Default::default()
                    },
                ),
                Span::raw(field.show(pick, config)),
            ];
            if state.risk(*field, pick).is_some() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(CAUTION, palette.caution()));
            }
            lines.push((Some(index), Line::from(spans)));
        }
        lines.push((None, Line::raw("")));
        for (index, text) in [
            (screen.rows.len(), "Review and write"),
            (screen.rows.len() + 1, "Cancel"),
        ] {
            let selected = index == screen.row;
            lines.push((
                Some(index),
                Line::from(vec![
                    Span::raw(if selected {
                        format!("{} ", glyphs.pointer)
                    } else {
                        "  ".into()
                    }),
                    Span::styled(
                        text,
                        if selected {
                            palette.selected()
                        } else {
                            Default::default()
                        },
                    ),
                ]),
            ));
        }
        let height = usize::from(list.height);
        let at = lines
            .iter()
            .position(|(row, _)| *row == Some(screen.row))
            .unwrap_or(0);
        let start = window_start(lines.len(), height, at);
        frame.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .skip(start)
                    .take(height)
                    .map(|(_, line)| line)
                    .collect::<Vec<_>>(),
            ),
            list,
        );
        frame.render_widget(
            Paragraph::new(side_pane(state, screen, config, palette))
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::LEFT)
                        .title(" About this setting "),
                ),
            pane,
        );
    }

    /// What the side pane says about the highlighted row, with this host's numbers.
    pub fn side_pane(
        state: &State,
        screen: &Screen,
        config: &Config,
        palette: Palette,
    ) -> Vec<Line<'static>> {
        let Some(field) = screen.rows.get(screen.row).copied() else {
            let text = if screen.row == screen.rows.len() {
                "Shows every change and caution before anything is written; Enter there writes the file."
            } else {
                "Leaves the configuration as it is."
            };
            return vec![Line::raw(text)];
        };
        let pick = state.values[&field];
        let mut lines = vec![Line::styled(field.label(), palette.title())];
        if let Some(note) = field.note() {
            lines.push(Line::raw(note));
        }
        lines.push(Line::raw(""));
        lines.push(Line::raw(format!("Value: {}", field.show(pick, config))));
        lines.push(Line::raw(capitalize(&consequence(
            field,
            pick,
            config,
            state.host.as_ref(),
        ))));
        if let Some(option) = state.risk(field, pick) {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!("{CAUTION} {}", justification(option)),
                palette.caution(),
            ));
        }
        lines
    }

    pub fn run(state: &mut State, color: bool, glyphs: Glyphs) -> Result<Outcome> {
        let _raw = terminal::Raw::enter().context("cannot drive the terminal")?;
        let mut keys = terminal::Keys::default();
        // The window's size only: asking the terminal for the cursor would race the key reader.
        let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 40));
        let area = ratatui::layout::Rect::new(0, 0, columns, rows);
        let mut screen = Screen::new(state);
        if color {
            let mut terminal = ratatui::Terminal::with_options(
                ratatui::backend::CrosstermBackend::new(std::io::stdout()),
                ratatui::TerminalOptions {
                    viewport: ratatui::Viewport::Fixed(area),
                },
            )
            .context("cannot drive the terminal")?;
            // Cleared directly: ratatui's clear asks the terminal for the cursor first.
            crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                crossterm::cursor::MoveTo(0, 0)
            )?;
            let outcome = loop {
                terminal.draw(|frame| draw(frame, state, &screen, true, glyphs))?;
                if let Some(outcome) = screen.key(state, keys.next()?) {
                    break outcome;
                }
            };
            drop(terminal);
            crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                crossterm::cursor::MoveTo(0, 0),
                crossterm::cursor::Show
            )?;
            return Ok(outcome);
        }
        // Without color no escape sequence is written: each screen is printed as plain text
        // after a blank line (001 setup contract).
        let mut memory = ratatui::Terminal::with_options(
            ratatui::backend::TestBackend::new(columns, rows),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(area),
            },
        )?;
        let mut out = std::io::stdout();
        loop {
            memory.draw(|frame| draw(frame, state, &screen, false, glyphs))?;
            let buffer = memory.backend().buffer();
            let mut text = String::from("\n");
            for y in 0..buffer.area.height {
                let line: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                text.push_str(line.trim_end());
                text.push('\n');
            }
            out.write_all(text.trim_end_matches('\n').as_bytes())?;
            out.write_all(b"\n")?;
            out.flush()?;
            if let Some(outcome) = screen.key(state, keys.next()?) {
                return Ok(outcome);
            }
        }
    }
}

/// Raw mode through termios, restored by a drop guard and on signals.
#[cfg(unix)]
pub(super) mod terminal {
    use std::sync::atomic::{AtomicI32, Ordering};

    static SAVED_FD: AtomicI32 = AtomicI32::new(-1);
    static mut SAVED: std::mem::MaybeUninit<libc::termios> = std::mem::MaybeUninit::uninit();
    const SIGNALS: [libc::c_int; 4] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT];

    extern "C" fn restore_and_reraise(signal: libc::c_int) {
        let fd = SAVED_FD.load(Ordering::SeqCst);
        // tcsetattr, signal and raise are async-signal-safe.
        unsafe {
            if fd >= 0 {
                libc::tcsetattr(fd, libc::TCSANOW, (&raw const SAVED).cast());
            }
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    /// Raw input on stdin until dropped.
    pub struct Raw {
        saved: libc::termios,
        previous: Vec<(libc::c_int, libc::sigaction)>,
    }

    impl Raw {
        pub fn enter() -> std::io::Result<Self> {
            let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
            if unsafe { libc::tcgetattr(0, saved.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let saved = unsafe { saved.assume_init() };
            unsafe {
                (&raw mut SAVED).write(std::mem::MaybeUninit::new(saved));
            }
            SAVED_FD.store(0, Ordering::SeqCst);
            let mut previous = Vec::new();
            for signal in SIGNALS {
                let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
                action.sa_sigaction = restore_and_reraise as *const () as libc::sighandler_t;
                let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
                if unsafe { libc::sigaction(signal, &action, &mut old) } == 0 {
                    previous.push((signal, old));
                }
            }
            let guard = Self { saved, previous };
            let mut raw = saved;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
            raw.c_iflag &= !(libc::IXON | libc::ICRNL | libc::INLCR);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(guard)
        }
    }

    impl Drop for Raw {
        fn drop(&mut self) {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &self.saved);
            }
            SAVED_FD.store(-1, Ordering::SeqCst);
            for (signal, old) in &self.previous {
                unsafe {
                    libc::sigaction(*signal, old, std::ptr::null_mut());
                }
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Key {
        Up,
        Down,
        Left,
        Right,
        Space,
        Enter,
        Cancel,
        Other,
    }

    /// Keys read straight from fd 0; bytes after one key stay queued.
    #[derive(Default)]
    pub struct Keys {
        pending: std::collections::VecDeque<u8>,
    }

    impl Keys {
        fn fill(&mut self, wait_ms: i32) -> std::io::Result<bool> {
            if wait_ms >= 0 {
                let mut poll = libc::pollfd {
                    fd: 0,
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut poll, 1, wait_ms) } <= 0 {
                    return Ok(false);
                }
            }
            let mut buffer = [0u8; 64];
            let read = unsafe { libc::read(0, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    return Ok(true);
                }
                return Err(error);
            }
            self.pending.extend(&buffer[..read as usize]);
            Ok(read > 0)
        }

        pub fn next(&mut self) -> std::io::Result<Key> {
            if self.pending.is_empty() && !self.fill(-1)? {
                // End of input cancels.
                return Ok(Key::Cancel);
            }
            let Some(first) = self.pending.pop_front() else {
                return Ok(Key::Cancel);
            };
            Ok(match first {
                0x1b => {
                    // A lone Esc, or the start of an arrow key.
                    if self.pending.is_empty() {
                        self.fill(50)?;
                    }
                    match (self.pending.front().copied(), self.pending.get(1).copied()) {
                        (Some(b'[' | b'O'), Some(code)) => {
                            self.pending.drain(..2);
                            match code {
                                b'A' => Key::Up,
                                b'B' => Key::Down,
                                b'C' => Key::Right,
                                b'D' => Key::Left,
                                _ => Key::Other,
                            }
                        }
                        _ => Key::Cancel,
                    }
                }
                b' ' => Key::Space,
                b'\r' | b'\n' => Key::Enter,
                b'q' | b'Q' | 0x03 => Key::Cancel,
                _ => Key::Other,
            })
        }
    }
}
