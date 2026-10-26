use super::*;

/// Whether color may be used (FR-050).
#[cfg(unix)]
fn color_allowed() -> bool {
    std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        && std::env::var("TERM").map_or(true, |term| term != "dumb")
        && std::io::stdout().is_terminal()
}

#[cfg(unix)]
fn cursor_control() -> bool {
    std::env::var("TERM").map_or(true, |term| term != "dumb" && !term.is_empty())
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
    let mut state = State::new(target.clone(), base, flags, host.cloned());
    #[cfg(unix)]
    if cursor_control()
        && let Some(size) = terminal::size()
        && size.0 as usize >= state.rows().len() + 8
        && size.1 >= 72
    {
        return match screen::run(&mut state, color_allowed())? {
            screen::Outcome::Done(choices) => Ok(Some(choices)),
            screen::Outcome::Cancelled => Ok(None),
            screen::Outcome::Role(role) => prompts::connection(target, role, Choices::default()),
        };
    }
    prompts::node(&mut state)
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

/// Line-based prompts, for terminals the interface cannot drive (FR-055).
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

    pub fn node(state: &mut State) -> Result<Option<Choices>> {
        println!("CedeGrid setup: {} (node)", state.target.path.display());
        println!("Press Enter to keep the current value. Ctrl-D cancels.");
        for field in state.rows() {
            if field == Field::Role {
                continue;
            }
            let config = state.preview();
            let options = field.options(&config, state.fresh);
            println!();
            println!(
                "{}: {}",
                field.label(),
                field.show(state.values[&field], &config)
            );
            for (index, option) in options.iter().enumerate() {
                let marker = state
                    .risk(field, *option)
                    .map_or(String::new(), |_| format!(" {CAUTION}"));
                println!(
                    "  {}) {}{marker}: {}",
                    index + 1,
                    field.show(*option, &config),
                    consequence(field, *option, &config, state.host.as_ref())
                );
            }
            loop {
                let Some(answer) = ask(&format!("Choose 1-{}: ", options.len()))? else {
                    return Ok(None);
                };
                if answer.is_empty() {
                    break;
                }
                let Some(pick) = answer
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|n| options.get(n))
                    .copied()
                else {
                    println!("Enter a number from 1 to {}.", options.len());
                    continue;
                };
                if let Some(option) = state.risk(field, pick)
                    && state.needs_confirmation(option)
                {
                    println!("{CAUTION} {}", justification(option));
                    let Some(answer) =
                        ask(&format!("Type yes to record {option} and use this value: "))?
                    else {
                        return Ok(None);
                    };
                    if answer != "yes" {
                        println!("Kept {}.", field.show(state.values[&field], &config));
                        break;
                    }
                    state.acknowledged.insert(option.to_string());
                }
                state.values.insert(field, pick);
                break;
            }
        }
        let plan = state.plan()?;
        println!();
        for line in summary_lines(&plan, state.host.as_ref()) {
            println!("{line}");
        }
        let Some(answer) = ask("Write this configuration? [y/N] ")? else {
            return Ok(None);
        };
        Ok(matches!(answer.as_str(), "y" | "Y" | "yes").then(|| state.choices()))
    }

    pub fn connection(target: &Target, role: Role, flags: Choices) -> Result<Option<Choices>> {
        println!(
            "CedeGrid setup: {} ({})",
            target.path.display(),
            role.name()
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

/// The keyboard interface (FR-046 to FR-051).
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
            marked: usize,
        },
        Confirm {
            field: Field,
            pick: Pick,
            option: &'static str,
            cursor: usize,
            marked: usize,
        },
        Summary(Box<Result<Plan, String>>),
    }

    struct Painter {
        color: bool,
        out: std::io::Stdout,
    }

    impl Painter {
        fn caution(&self) -> String {
            if self.color {
                format!("\x1b[33m{CAUTION}\x1b[0m")
            } else {
                CAUTION.to_string()
            }
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

    pub fn run(state: &mut State, color: bool) -> Result<Outcome> {
        let _raw = terminal::Raw::enter().context("cannot drive the terminal")?;
        let mut keys = terminal::Keys::default();
        let mut painter = Painter {
            color,
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
                    Key::Space | Key::Enter if row == rows.len() => {
                        notice = None;
                        View::Summary(Box::new(state.plan().map_err(|error| error.to_string())))
                    }
                    Key::Space | Key::Enter if row == rows.len() + 1 => {
                        return Ok(Outcome::Cancelled);
                    }
                    Key::Space | Key::Enter => {
                        let field = rows[row];
                        let config = state.preview();
                        let options = field.options(&config, state.fresh);
                        let current = options
                            .iter()
                            .position(|pick| *pick == state.values[&field])
                            .unwrap_or(0);
                        View::Options {
                            field,
                            options,
                            cursor: current,
                            marked: current,
                        }
                    }
                    _ => View::Main,
                },
                View::Options {
                    field,
                    options,
                    mut cursor,
                    mut marked,
                } => match key {
                    Key::Up => {
                        cursor = cursor.saturating_sub(1);
                        View::Options {
                            field,
                            options,
                            cursor,
                            marked,
                        }
                    }
                    Key::Down => {
                        cursor = (cursor + 1).min(options.len() - 1);
                        View::Options {
                            field,
                            options,
                            cursor,
                            marked,
                        }
                    }
                    Key::Space => {
                        marked = cursor;
                        View::Options {
                            field,
                            options,
                            cursor,
                            marked,
                        }
                    }
                    Key::Enter => {
                        let pick = options[marked];
                        if let Pick::Role(role) = pick {
                            if role != Role::Node {
                                return Ok(Outcome::Role(role));
                            }
                            View::Main
                        } else {
                            match state.risk(field, pick) {
                                Some(option) if state.needs_confirmation(option) => View::Confirm {
                                    field,
                                    pick,
                                    option,
                                    cursor: 0,
                                    marked: 0,
                                },
                                _ => {
                                    state.values.insert(field, pick);
                                    View::Main
                                }
                            }
                        }
                    }
                    _ => View::Options {
                        field,
                        options,
                        cursor,
                        marked,
                    },
                },
                View::Confirm {
                    field,
                    pick,
                    option,
                    mut cursor,
                    mut marked,
                } => match key {
                    Key::Up => {
                        cursor = 0;
                        View::Confirm {
                            field,
                            pick,
                            option,
                            cursor,
                            marked,
                        }
                    }
                    Key::Down => {
                        cursor = 1;
                        View::Confirm {
                            field,
                            pick,
                            option,
                            cursor,
                            marked,
                        }
                    }
                    Key::Space => {
                        marked = cursor;
                        View::Confirm {
                            field,
                            pick,
                            option,
                            cursor,
                            marked,
                        }
                    }
                    Key::Enter => {
                        if marked == 1 {
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
                        marked,
                    },
                },
                View::Summary(plan) => match (key, *plan) {
                    (Key::Enter, Ok(_)) => return Ok(Outcome::Done(state.choices())),
                    (Key::Enter | Key::Space, Err(error)) => {
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
        let pointer = |selected: bool| if selected { "> " } else { "  " };
        let marker = |risky: bool| {
            if risky {
                format!(" {}", painter.caution())
            } else {
                String::new()
            }
        };
        let mut lines = Vec::new();
        match view {
            View::Main => {
                lines.push(format!(
                    "CedeGrid setup: {} (node)",
                    state.target.path.display()
                ));
                lines.push("Up/Down move, Space or Enter open, Esc or q cancel".into());
                lines.push(String::new());
                for (index, field) in rows.iter().enumerate() {
                    let pick = state.values[field];
                    // Floors also show what applies on this host.
                    let host = match (field, state.host.as_ref()) {
                        (Field::RamFloor, Some(host)) => format!(
                            "; {} MiB on this host",
                            group(config.ram_floor_mib(host.ram_mib))
                        ),
                        (Field::GpuFloor, Some(host)) => host
                            .gpus
                            .first()
                            .map(|(uuid, total)| {
                                format!("; {} MiB on {uuid}", group(config.vram_floor_mib(*total)))
                            })
                            .unwrap_or_else(|| "; no GPU on this host".into()),
                        _ => String::new(),
                    };
                    lines.push(format!(
                        "{}{:<36}{}{host}{}",
                        pointer(index == row),
                        field.label(),
                        field.show(pick, &config),
                        marker(state.risk(*field, pick).is_some())
                    ));
                }
                lines.push(format!("{}Review and write", pointer(row == rows.len())));
                lines.push(format!("{}Cancel", pointer(row == rows.len() + 1)));
                if let Some(notice) = notice {
                    lines.push(String::new());
                    lines.push(format!("Cannot write: {notice}"));
                }
            }
            View::Options {
                field,
                options,
                cursor,
                marked,
            } => {
                lines.push(field.label());
                lines.push("Up/Down move, Space select, Enter confirm, Esc or q cancel".into());
                lines.push(String::new());
                for (index, option) in options.iter().enumerate() {
                    lines.push(format!(
                        "{}({}) {}{}",
                        pointer(index == *cursor),
                        if index == *marked { '*' } else { ' ' },
                        field.show(*option, &config),
                        marker(state.risk(*field, *option).is_some())
                    ));
                    lines.push(format!(
                        "        {}",
                        consequence(*field, *option, &config, state.host.as_ref())
                    ));
                }
            }
            View::Confirm {
                field,
                pick,
                option,
                cursor,
                marked,
            } => {
                let kept = state.values[field];
                lines.push("Confirm a caution option".into());
                lines.push(String::new());
                lines.push(format!(
                    "{} {}: {}",
                    painter.caution(),
                    field.label(),
                    field.show(*pick, &config)
                ));
                lines.push(consequence(*field, *pick, &config, state.host.as_ref()));
                lines.push(justification(option).into());
                lines.push(String::new());
                lines.push("Up/Down move, Space select, Enter confirm, Esc or q cancel".into());
                lines.push(format!(
                    "{}({}) No, keep {}",
                    pointer(*cursor == 0),
                    if *marked == 0 { '*' } else { ' ' },
                    field.show(kept, &config)
                ));
                lines.push(format!(
                    "{}({}) Yes, use {} and record {option} in contention.acknowledge_caution",
                    pointer(*cursor == 1),
                    if *marked == 1 { '*' } else { ' ' },
                    field.show(*pick, &config)
                ));
            }
            View::Summary(plan) => {
                lines.push("Review before writing".into());
                lines.push(String::new());
                match plan.as_ref() {
                    Ok(plan) => {
                        for line in summary_lines(plan, state.host.as_ref()) {
                            lines.push(line.replace(CAUTION, &painter.caution()));
                        }
                        lines.push(String::new());
                        lines.push("Enter write, Esc or q cancel".into());
                    }
                    Err(error) => {
                        lines.push(format!("Cannot write: {error}"));
                        lines.push(String::new());
                        lines.push("Enter back, Esc or q cancel".into());
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
