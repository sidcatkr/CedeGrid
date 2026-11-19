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
    fn new(target: Target, base: Config, flags: Choices, host: Option<Host>) -> Self {
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
        println!(
            "{}",
            Style::detect().bold(&format!(
                "CedeGrid setup · {} · {}",
                role.name(),
                target.path.display()
            ))
        );
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

/// The keyboard interface (FR-046 to FR-051): arrow keys move, Enter chooses, Esc or q cancels.
#[cfg(unix)]
mod screen {
    use super::*;
    use terminal::Key;

    pub enum Outcome {
        Done(Choices),
        Cancelled,
        Role(Role),
    }

    enum View {
        Main,
        Options {
            field: Field,
            options: Vec<Pick>,
            cursor: usize,
        },
        Confirm {
            field: Field,
            pick: Pick,
            option: &'static str,
            cursor: usize,
        },
        Summary(Box<Result<Plan, String>>),
    }

    struct Painter {
        color: bool,
        glyphs: Glyphs,
        out: std::io::Stdout,
    }

    impl Painter {
        fn style(&self) -> Style {
            Style { color: self.color }
        }

        /// One full frame. Without color the frame is plain text, so no
        /// escape sequence is written at all.
        fn frame(&mut self, lines: &[String]) -> Result<()> {
            let mut text = String::new();
            if self.color {
                text.push_str("\x1b[H\x1b[2J");
            } else {
                text.push('\n');
            }
            for line in lines {
                text.push_str(line);
                text.push('\n');
            }
            self.out.write_all(text.as_bytes())?;
            self.out.flush()?;
            Ok(())
        }
    }

    pub fn run(state: &mut State, color: bool, glyphs: Glyphs) -> Result<Outcome> {
        let _raw = terminal::Raw::enter().context("cannot drive the terminal")?;
        let mut keys = terminal::Keys::default();
        let mut painter = Painter {
            color,
            glyphs,
            out: std::io::stdout(),
        };
        let rows = state.rows();
        let mut row = 0usize;
        let mut view = View::Main;
        let mut notice: Option<String> = None;
        loop {
            let lines = render(state, &painter, &rows, row, &view, notice.as_deref());
            painter.frame(&lines)?;
            let key = keys.next()?;
            if key == Key::Cancel {
                return Ok(Outcome::Cancelled);
            }
            let total = rows.len() + 2;
            view = match view {
                View::Main => match key {
                    Key::Up => {
                        row = (row + total - 1) % total;
                        View::Main
                    }
                    Key::Down => {
                        row = (row + 1) % total;
                        View::Main
                    }
                    Key::Enter if row == rows.len() => {
                        notice = None;
                        View::Summary(Box::new(state.plan().map_err(|error| error.to_string())))
                    }
                    Key::Enter if row == rows.len() + 1 => return Ok(Outcome::Cancelled),
                    Key::Enter => {
                        let field = rows[row];
                        let config = state.preview();
                        let options = field.options(&config, state.fresh);
                        let cursor = options
                            .iter()
                            .position(|pick| *pick == state.values[&field])
                            .unwrap_or(0);
                        View::Options {
                            field,
                            options,
                            cursor,
                        }
                    }
                    _ => View::Main,
                },
                View::Options {
                    field,
                    options,
                    cursor,
                } => match key {
                    Key::Up => View::Options {
                        field,
                        cursor: cursor.saturating_sub(1),
                        options,
                    },
                    Key::Down => View::Options {
                        field,
                        cursor: (cursor + 1).min(options.len() - 1),
                        options,
                    },
                    Key::Enter => {
                        let pick = options[cursor];
                        match pick {
                            Pick::Role(role) if role != Role::Node => {
                                return Ok(Outcome::Role(role));
                            }
                            Pick::Role(_) => View::Main,
                            _ => match state.risk(field, pick) {
                                Some(option) if state.needs_confirmation(option) => View::Confirm {
                                    field,
                                    pick,
                                    option,
                                    cursor: 0,
                                },
                                _ => {
                                    state.values.insert(field, pick);
                                    View::Main
                                }
                            },
                        }
                    }
                    _ => View::Options {
                        field,
                        options,
                        cursor,
                    },
                },
                View::Confirm {
                    field,
                    pick,
                    option,
                    cursor,
                } => match key {
                    Key::Up | Key::Down => View::Confirm {
                        field,
                        pick,
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
                        option,
                        cursor,
                    },
                },
                View::Summary(plan) => match (key, *plan) {
                    (Key::Enter, Ok(_)) => return Ok(Outcome::Done(state.choices())),
                    (Key::Enter, Err(error)) => {
                        notice = Some(error);
                        View::Main
                    }
                    (_, plan) => View::Summary(Box::new(plan)),
                },
            };
        }
    }

    fn render(
        state: &State,
        painter: &Painter,
        rows: &[Field],
        row: usize,
        view: &View,
        notice: Option<&str>,
    ) -> Vec<String> {
        let config = state.preview();
        let style = painter.style();
        let glyphs = painter.glyphs;
        let (height, width) = terminal::size().map_or((40, 80), |(rows, columns)| {
            (usize::from(rows), usize::from(columns))
        });
        let pointer = |selected: bool| {
            if selected {
                format!("{} ", glyphs.pointer)
            } else {
                "  ".to_owned()
            }
        };
        let marker = |risky: bool| {
            if risky {
                format!("  {}", style.caution(CAUTION))
            } else {
                String::new()
            }
        };
        let title = |text: &str| style.bold(&format!("CedeGrid setup · {text}"));
        let hint = |action: &str| {
            style.dim(&format!(
                "{} move · Enter {action} · Esc cancel",
                glyphs.arrows
            ))
        };
        let mut lines = Vec::new();
        match view {
            View::Main => {
                lines.push(title(&format!("node · {}", state.target.path.display())));
                lines.push(String::new());
                let label_width = rows
                    .iter()
                    .map(|field| field.label().chars().count())
                    .max()
                    .unwrap_or(0)
                    + 3;
                // A short terminal shows the rows around the highlight, with what is hidden
                // counted above and below.
                let fixed = 7 + if notice.is_some() { 2 } else { 0 };
                let visible = height.saturating_sub(fixed).max(3);
                let (start, end) = if rows.len() <= visible {
                    (0, rows.len())
                } else {
                    let inner = visible - 2;
                    let start = window_start(rows.len(), inner, row.min(rows.len() - 1));
                    (start, start + inner)
                };
                if start > 0 {
                    lines.push(style.dim(&format!("  {start} more above")));
                }
                for (index, field) in rows.iter().enumerate().take(end).skip(start) {
                    let pick = state.values[field];
                    // Floors also show what applies on this host.
                    let host = match (field, state.host.as_ref()) {
                        (Field::RamFloor, Some(host)) => format!(
                            " · {} MiB on this host",
                            group(config.ram_floor_mib(host.ram_mib))
                        ),
                        (Field::GpuFloor, Some(host)) => host
                            .gpus
                            .first()
                            .map(|(uuid, total)| {
                                format!(" · {} MiB on {uuid}", group(config.vram_floor_mib(*total)))
                            })
                            .unwrap_or_else(|| " · no GPU on this host".into()),
                        // Priority says when this host cannot apply it (006 FR-025).
                        (Field::PlainPriority, Some(host)) if pick == Pick::Flag(true) => {
                            match host.plain_priority {
                                PlainPriorityHost::Permitted(weight) => {
                                    format!(" · weight {weight} on this host")
                                }
                                _ => " · no effect on this host".into(),
                            }
                        }
                        _ => String::new(),
                    };
                    let label = format!("{:<label_width$}", field.label());
                    lines.push(format!(
                        "{}{}{}{}{}",
                        pointer(index == row),
                        if index == row {
                            style.bold(&label)
                        } else {
                            label
                        },
                        field.show(pick, &config),
                        style.dim(&host),
                        marker(state.risk(*field, pick).is_some())
                    ));
                }
                if end < rows.len() {
                    lines.push(style.dim(&format!("  {} more below", rows.len() - end)));
                }
                lines.push(String::new());
                lines.push(format!("{}Review and write", pointer(row == rows.len())));
                lines.push(format!("{}Cancel", pointer(row == rows.len() + 1)));
                if let Some(notice) = notice {
                    lines.push(String::new());
                    lines.push(format!("{} {notice}", style.refusal("Cannot write:")));
                }
                lines.push(String::new());
                lines.push(hint("open"));
            }
            View::Options {
                field,
                options,
                cursor,
            } => {
                lines.push(title(&field.label()));
                if let Some(note) = field.note() {
                    lines.push(style.dim(note));
                }
                lines.push(String::new());
                let current = state.values[field];
                let rows: Vec<OptionRow> = options
                    .iter()
                    .enumerate()
                    .map(|(index, option)| OptionRow {
                        prefix: format!(
                            "  {} ",
                            if index == *cursor {
                                glyphs.chosen
                            } else {
                                glyphs.other
                            }
                        ),
                        label: field.show(*option, &config),
                        description: format!(
                            "{}{}",
                            consequence(*field, *option, &config, state.host.as_ref()),
                            if *option == current { " (current)" } else { "" }
                        ),
                        caution: state.risk(*field, *option).is_some(),
                        selected: index == *cursor,
                    })
                    .collect();
                lines.extend(option_lines(&rows, style, Some(width)));
                lines.push(String::new());
                lines.push(hint("choose"));
            }
            View::Confirm {
                field,
                pick,
                option,
                cursor,
            } => {
                let kept = state.values[field];
                lines.push(title("Confirm a caution option"));
                lines.push(String::new());
                lines.push(format!(
                    "  {} {}: {}",
                    style.caution(CAUTION),
                    field.label(),
                    field.show(*pick, &config)
                ));
                lines.push(String::new());
                lines.extend(confirmation_rows(
                    state, &config, *field, *pick, option, width,
                ));
                lines.push(String::new());
                for (index, text) in [
                    format!("No, keep {}", field.show(kept, &config)),
                    format!(
                        "Yes, use {} and record {option}",
                        field.show(*pick, &config)
                    ),
                ]
                .into_iter()
                .enumerate()
                {
                    lines.push(if *cursor == index {
                        format!("  {} {}", glyphs.chosen, style.bold(&text))
                    } else {
                        format!("  {} {text}", glyphs.other)
                    });
                }
                lines.push(String::new());
                lines.push(hint("choose"));
            }
            View::Summary(plan) => {
                lines.push(title("Review before writing"));
                lines.push(String::new());
                match plan.as_ref() {
                    Ok(plan) => {
                        for line in summary_lines(plan, state.host.as_ref()) {
                            lines.push(line.replace(CAUTION, &style.caution(CAUTION)));
                        }
                        lines.push(String::new());
                        lines.push(style.dim("Enter write · Esc cancel"));
                    }
                    Err(error) => {
                        lines.push(format!("{} {error}", style.refusal("Cannot write:")));
                        lines.push(String::new());
                        lines.push(style.dim("Enter back · Esc cancel"));
                    }
                }
            }
        }
        lines
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

    /// The terminal's rows and columns.
    pub fn size() -> Option<(u16, u16)> {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        (unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_row > 0)
            .then_some((size.ws_row, size.ws_col))
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Key {
        Up,
        Down,
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
