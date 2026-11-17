//! Frontend loop and screen guard for `brgr tui`. Becomes `tui/` in slice 2.

use std::{
    env,
    io::{self, IsTerminal as _, Write as _},
};

use anyhow::{Result, bail};
use brgr_protocol::OwnerId;
use dialoguer::console::Term;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::{Duration, sleep};

use crate::{
    Paths,
    board::{self, Host, Scope, refresh_failed_lines},
    cli::TuiArgs,
    current_session,
};

pub(crate) struct BoardOptions {
    pub(crate) scope: Scope,
    pub(crate) host: Host,
    pub(crate) once: bool,
}

#[derive(Clone, Copy)]
enum Event {
    Tick,
    Resize,
    Quit,
}

struct State {
    model: Result<board::BoardModel, String>,
}

struct ScreenGuard {
    restored: bool,
}

impl ScreenGuard {
    fn enter() -> io::Result<Self> {
        let mut stdout = io::stdout();
        write!(stdout, "\x1b[?1049h\x1b[?25l")?;
        stdout.flush()?;
        Ok(Self { restored: false })
    }

    fn restore(&mut self) {
        if self.restored {
            return;
        }
        let mut stdout = io::stdout();
        let _ = write!(stdout, "\x1b[?25h\x1b[?1049l");
        let _ = stdout.flush();
        self.restored = true;
    }
}

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

pub(crate) async fn run(paths: &Paths, args: &TuiArgs, json_output: bool) -> Result<()> {
    if json_output {
        bail!("brgr tui does not support --json; use brgr --json status or brgr status --brief");
    }
    let scope = resolve_scope(args)?;
    run_board(
        paths,
        BoardOptions {
            scope,
            host: Host::Terminal,
            once: args.once,
        },
    )
    .await
}

pub(crate) async fn run_board(paths: &Paths, options: BoardOptions) -> Result<()> {
    let once = options.once || !io::stdout().is_terminal();
    if once {
        return print_snapshot(paths, &options);
    }
    live_loop(paths, &options).await
}

fn resolve_scope(args: &TuiArgs) -> Result<Scope> {
    let owner = env::var("BRGR_OWNER_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(OwnerId::new)
        .transpose()?;
    if args.mine {
        let Some(session) = current_session()? else {
            bail!(
                "brgr tui --mine needs an owner session; run it from the owner's session or pass --as SESSION"
            );
        };
        return Ok(Scope::Session { session, owner });
    }
    if args.all {
        return Ok(Scope::All);
    }
    match current_session()? {
        Some(session) => Ok(Scope::Session { session, owner }),
        None => Ok(Scope::All),
    }
}

fn apply_unbound_scope_line(model: &mut board::BoardModel, options: &BoardOptions) {
    if !matches!(options.host, Host::Terminal) || !matches!(options.scope, Scope::All) {
        return;
    }
    if current_session().ok().flatten().is_none() {
        "all owners · no owner session in this terminal (pass --as SESSION for my tasks)"
            .clone_into(&mut model.scope_line);
    }
}

fn load_state(paths: &Paths, options: &BoardOptions) -> State {
    match board::load(paths, &options.scope) {
        Ok(mut model) => {
            apply_unbound_scope_line(&mut model, options);
            State { model: Ok(model) }
        }
        Err(error) => State {
            model: Err(error.to_string()),
        },
    }
}

fn print_snapshot(paths: &Paths, options: &BoardOptions) -> Result<()> {
    match board::load(paths, &options.scope) {
        Ok(mut model) => {
            apply_unbound_scope_line(&mut model, options);
            let lines = board::render(&model, options.host, None);
            println!("{}", lines.join("\n"));
            Ok(())
        }
        Err(error) => {
            let lines = refresh_failed_lines(&error.to_string());
            eprintln!("{}", lines.join("\n"));
            Err(error)
        }
    }
}

fn terminal_size() -> Option<(u16, u16)> {
    let (rows, cols) = Term::stdout().size();
    if cols == 0 { None } else { Some((cols, rows)) }
}

fn draw_frame(state: &State, host: Host) -> io::Result<()> {
    let size = terminal_size();
    let lines = match &state.model {
        Ok(model) => board::render(model, host, size),
        Err(error) => refresh_failed_lines(error),
    };
    let mut stdout = io::stdout();
    write!(stdout, "\x1b[H")?;
    for line in &lines {
        writeln!(stdout, "{line}\x1b[K")?;
    }
    write!(stdout, "\x1b[J")?;
    stdout.flush()
}

fn apply_event(paths: &Paths, options: &BoardOptions, state: &mut State, event: Event) -> bool {
    match event {
        Event::Quit => false,
        Event::Tick => {
            *state = load_state(paths, options);
            true
        }
        Event::Resize => true,
    }
}

async fn next_event(
    sigint: &mut tokio::signal::unix::Signal,
    sigterm: &mut tokio::signal::unix::Signal,
    sighup: &mut tokio::signal::unix::Signal,
    sigwinch: &mut tokio::signal::unix::Signal,
) -> Event {
    tokio::select! {
        () = sleep(Duration::from_secs(2)) => Event::Tick,
        _ = sigint.recv() => Event::Quit,
        _ = sigterm.recv() => Event::Quit,
        _ = sighup.recv() => Event::Quit,
        _ = sigwinch.recv() => Event::Resize,
    }
}

async fn live_loop(paths: &Paths, options: &BoardOptions) -> Result<()> {
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut sigwinch = signal(SignalKind::from_raw(libc::SIGWINCH))?;
    let _screen = ScreenGuard::enter()?;
    let mut state = load_state(paths, options);
    loop {
        if draw_frame(&state, options.host).is_err() {
            return Ok(());
        }
        let event = next_event(&mut sigint, &mut sigterm, &mut sighup, &mut sigwinch).await;
        if !apply_event(paths, options, &mut state, event) {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{board::refresh_failed_lines, human};

    #[test]
    fn refresh_failure_text_is_sanitized() {
        let lines = refresh_failed_lines("boom\u{1b}[31m\u{202e}");
        let text = lines.join("\n");
        assert!(text.contains("board refresh failed:"));
        assert!(text.contains("No tasks were mutated or reconciled."));
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{202e}'));
        assert_eq!(human::safe_text("boom\u{1b}[31m"), "boom [31m");
    }
}
