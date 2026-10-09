//! Local terminal: a shell of this computer in a pseudoterminal, with the
//! `tty` module of `alacritty_terminal` (PTY on Linux and macOS, ConPTY on
//! Windows).
//!
//! The PTY I/O runs in its own thread that waits with `polling` (like
//! Alacritty's loop) and talks to the view with the same commands ([`Cmd`])
//! and events ([`Out`]) as the SSH terminals.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{self, ChildEvent, EventedPty, EventedReadWrite, Options, Shell};
use bytes::Bytes;
use polling::{Event, Events, PollMode, Poller};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use super::backend::{Backend, Cmd, Out};

/// `polling` key of the child process notifications (same on Unix and Windows).
const CHILD_TOKEN: usize = 1;
/// Bytes read from the PTY at once.
const READ_CHUNK: usize = 64 * 1024;
/// Output sent to the view and not yet painted beyond which reading stops:
/// the shell is slowed down (as in any terminal) and the interface does not
/// choke on `yes` or a huge `cat`.
const MAX_IN_FLIGHT: usize = 1024 * 1024;

/// Shell program and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCommand {
    pub program: String,
    pub args: Vec<String>,
}

/// Shell of this computer: on Unix `$SHELL` (or `/bin/bash`, or `/bin/sh`) as
/// a login shell; on Windows PowerShell 7 (`pwsh.exe`), Windows PowerShell or
/// `%COMSPEC%`.
pub fn default_shell() -> ShellCommand {
    #[cfg(unix)]
    {
        unix_shell(std::env::var("SHELL").ok().as_deref(), is_executable)
    }
    #[cfg(windows)]
    {
        let var = |name: &str| std::env::var(name).ok();
        windows_shell(
            var("PATH").as_deref(),
            var("SystemRoot").as_deref(),
            var("COMSPEC").as_deref(),
            |p| std::path::Path::new(p).is_file(),
        )
    }
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Chooses the shell on Unix. `usable` says whether a path exists and is executable.
#[cfg(any(unix, test))]
fn unix_shell(env_shell: Option<&str>, usable: impl Fn(&std::path::Path) -> bool) -> ShellCommand {
    let from_env = env_shell
        .map(str::trim)
        .filter(|s| s.starts_with('/') && usable(std::path::Path::new(s)))
        .map(str::to_string);
    let program = from_env
        .or_else(|| {
            ["/bin/bash", "/bin/sh"]
                .into_iter()
                .find(|p| usable(std::path::Path::new(p)))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "/bin/sh".into());
    ShellCommand {
        program,
        // Login shell (reads the user profile).
        args: vec!["-l".into()],
    }
}

/// Chooses the shell on Windows. `exists` says whether a file exists.
#[cfg(any(windows, test))]
fn windows_shell(
    path: Option<&str>,
    system_root: Option<&str>,
    comspec: Option<&str>,
    exists: impl Fn(&str) -> bool,
) -> ShellCommand {
    let in_path = |exe: &str| {
        path.unwrap_or_default()
            .split(';')
            .map(|d| d.trim().trim_matches('"').trim_end_matches(['\\', '/']))
            .filter(|d| !d.is_empty())
            .map(|d| format!("{d}\\{exe}"))
            .find(|p| exists(p))
    };
    let powershell = || {
        in_path("powershell.exe").or_else(|| {
            let root = system_root?.trim_end_matches(['\\', '/']);
            let p = format!("{root}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe");
            exists(&p).then_some(p)
        })
    };
    match in_path("pwsh.exe").or_else(powershell) {
        Some(program) => ShellCommand {
            program,
            args: vec!["-NoLogo".into()],
        },
        None => ShellCommand {
            program: comspec
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .unwrap_or("cmd.exe")
                .to_string(),
            args: Vec::new(),
        },
    }
}

/// Link between the view and the PTY thread.
pub struct PtyLink {
    poller: Arc<Poller>,
    /// Output sent to the view that it has not processed yet.
    in_flight: AtomicUsize,
}

impl PtyLink {
    /// Wakes the thread (there are new commands).
    pub fn wake(&self) {
        let _ = self.poller.notify();
    }

    /// The view has processed `n` bytes of output.
    pub fn consumed(&self, n: usize) {
        let before = self
            .in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                Some(v.saturating_sub(n))
            })
            .unwrap_or(0);
        if before >= MAX_IN_FLIGHT && before.saturating_sub(n) < MAX_IN_FLIGHT {
            self.wake();
        }
    }

    fn throttled(&self) -> bool {
        self.in_flight.load(Ordering::Acquire) >= MAX_IN_FLIGHT
    }
}

/// Opens a local shell of `cols` × `rows`.
pub fn start(cols: u16, rows: u16) -> (Backend, mpsc::UnboundedReceiver<Out>) {
    let (tx, cmd_rx) = mpsc::unbounded_channel();
    let (out, out_rx) = mpsc::unbounded_channel();
    let poller = match Poller::new() {
        Ok(p) => Arc::new(p),
        Err(e) => {
            let _ = out.send(Out::Failed(
                t!("terminal.shell.prepare_failed", error = e).to_string(),
            ));
            return (Backend::new(tx), out_rx);
        }
    };
    let link = Arc::new(PtyLink {
        poller,
        in_flight: AtomicUsize::new(0),
    });
    let backend = Backend::with_pty(tx, link.clone());
    let spawned = std::thread::Builder::new()
        .name("termoak-pty".into())
        .spawn({
            let out = out.clone();
            move || run(link, cols, rows, cmd_rx, out)
        });
    if let Err(e) = spawned {
        let _ = out.send(Out::Failed(
            t!("terminal.shell.thread_failed", error = e).to_string(),
        ));
    }
    (backend, out_rx)
}

/// Locale for the shell if the environment defines none (e.g. the app is
/// opened from the macOS Finder): without it, the shell does not know the
/// terminal uses UTF-8 and accented letters come out as `\303\241`. Only
/// `LC_CTYPE` is set, not the language of the messages.
fn utf8_locale(var: impl Fn(&str) -> Option<String>) -> Option<(&'static str, &'static str)> {
    let defined = ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .any(|k| var(k).is_some_and(|v| !v.trim().is_empty()));
    if defined || cfg!(windows) {
        None
    } else if cfg!(target_os = "macos") {
        Some(("LC_CTYPE", "UTF-8"))
    } else {
        Some(("LC_CTYPE", "C.UTF-8"))
    }
}

fn window_size(cols: u16, rows: u16) -> WindowSize {
    WindowSize {
        num_lines: rows.max(1),
        num_cols: cols.max(2),
        cell_width: 8,
        cell_height: 16,
    }
}

fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
}

/// Why the I/O loop ended.
enum End {
    /// The shell exited.
    Exited(Option<std::process::ExitStatus>),
    /// The view closed the terminal.
    Closed,
    /// PTY error.
    Error(String),
}

fn run(
    link: Arc<PtyLink>,
    cols: u16,
    rows: u16,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    out: mpsc::UnboundedSender<Out>,
) {
    let shell = default_shell();
    let _ = out.send(Out::Status(
        t!("terminal.shell.opening", program = shell.program).to_string(),
    ));
    let env = [
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
        ("TERM_PROGRAM", "Termoak"),
        ("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION")),
    ]
    .into_iter()
    .chain(utf8_locale(|k| std::env::var(k).ok()))
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect::<std::collections::HashMap<_, _>>();
    // Flatpak: the user's shell on the host, not the sandbox's.
    #[cfg(unix)]
    let (program, args) = if crate::flatpak::active() {
        crate::flatpak::host_shell(&env)
    } else {
        (shell.program.clone(), shell.args.clone())
    };
    #[cfg(windows)]
    let (program, args) = (shell.program.clone(), shell.args.clone());
    let options = Options {
        shell: Some(Shell::new(program, args)),
        working_directory: home_dir(),
        drain_on_exit: true,
        env,
        ..Default::default()
    };
    #[cfg(windows)]
    let options = Options {
        escape_args: true,
        ..options
    };
    let mut pty = match tty::new(&options, window_size(cols, rows), 0) {
        Ok(p) => p,
        Err(e) => {
            let _ = out.send(Out::Failed(
                t!(
                    "terminal.shell.open_failed",
                    program = shell.program,
                    error = e
                )
                .to_string(),
            ));
            return;
        }
    };
    let _ = out.send(Out::Shell);

    let end = pump(&mut pty, &link, &mut cmd_rx, &out);
    let _ = pty.deregister(&link.poller);
    // Dropping the PTY closes the shell (SIGHUP on Unix, closing the ConPTY
    // on Windows) and waits for it to end.
    drop(pty);
    let reason = match end {
        End::Closed => return,
        End::Exited(status) => exit_message(status),
        End::Error(e) => t!("terminal.shell.error", error = e).to_string(),
    };
    let _ = out.send(Out::Closed(Some(reason)));
    // The view keeps showing the output until the tab is closed or reopened.
    while let Some(cmd) = cmd_rx.blocking_recv() {
        if matches!(cmd, Cmd::Close | Cmd::CloseSession) {
            break;
        }
    }
}

/// Text for the end of the shell.
fn exit_message(status: Option<std::process::ExitStatus>) -> String {
    let Some(status) = status else {
        return t!("terminal.shell.exited").to_string();
    };
    if let Some(code) = status.code() {
        return t!("terminal.shell.exited_code", code = code).to_string();
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return t!("terminal.shell.exited_signal", signal = signal).to_string();
        }
    }
    t!("terminal.shell.exited").to_string()
}

/// I/O loop: reads the output, writes what is typed and handles the commands
/// until the shell exits or the view closes the terminal.
fn pump(
    pty: &mut tty::Pty,
    link: &PtyLink,
    cmd_rx: &mut mpsc::UnboundedReceiver<Cmd>,
    out: &mpsc::UnboundedSender<Out>,
) -> End {
    let poller = &link.poller;
    let mode = PollMode::Level;
    let mut interest = Event::readable(0);
    if let Err(e) = unsafe { pty.register(poller, interest, mode) } {
        return End::Error(e.to_string());
    }
    let mut events = Events::with_capacity(NonZeroUsize::new(64).unwrap_or(NonZeroUsize::MIN));
    let mut buf = vec![0u8; READ_CHUNK];
    let mut pending: VecDeque<Bytes> = VecDeque::new();
    // The other end of the PTY was closed: only the child notification is left.
    let mut hung_up = false;

    loop {
        events.clear();
        // Every so often, also check whether the view is gone.
        if let Err(e) = poller.wait(&mut events, Some(Duration::from_secs(1)))
            && e.kind() != ErrorKind::Interrupted
        {
            return End::Error(e.to_string());
        }
        if out.is_closed() {
            return End::Closed;
        }

        // Commands from the view.
        loop {
            match cmd_rx.try_recv() {
                Ok(Cmd::Input(b)) => {
                    if !b.is_empty() {
                        pending.push_back(b);
                    }
                }
                Ok(Cmd::Resize(cols, rows)) => pty.on_resize(window_size(cols, rows)),
                // Sharing goes through the relay.
                Ok(Cmd::Share(_) | Cmd::Latency(_)) => {}
                Ok(Cmd::Close | Cmd::CloseSession) | Err(TryRecvError::Disconnected) => {
                    return End::Closed;
                }
                Err(TryRecvError::Empty) => break,
            }
        }

        // Did the shell exit? Checked on its notification and also on every
        // wait without events, in case the child ended before the
        // notification was registered.
        let child_event = events.is_empty() || events.iter().any(|event| event.key == CHILD_TOKEN);
        if child_event && let Some(ChildEvent::Exited(status)) = pty.next_child_event() {
            // Last pending output.
            if !hung_up {
                let _ = read_available(pty, link, &mut buf, out, false);
            }
            return End::Exited(status);
        }

        for event in events.iter() {
            if event.key == CHILD_TOKEN {
                continue;
            }
            if event.is_interrupt() {
                continue;
            }
            if event.readable && !hung_up && !link.throttled() {
                match read_available(pty, link, &mut buf, out, true) {
                    // End of file or read error (EIO on Linux): the other end
                    // was closed and only the child notification is left.
                    Ok(Drained::Eof) => hung_up = true,
                    Ok(_) => {}
                    Err(_) if cfg!(unix) => hung_up = true,
                    Err(e) => return End::Error(e.to_string()),
                }
            }
            if event.writable
                && let Err(e) = write_pending(pty, &mut pending)
            {
                if cfg!(windows) {
                    return End::Error(e.to_string());
                }
                // On Unix, the shell no longer reads: wait for its exit notification.
                hung_up = true;
            }
        }
        if hung_up {
            pending.clear();
        }

        // Interest for the next wait.
        let wanted = (!hung_up && !link.throttled(), !pending.is_empty());
        if wanted != (interest.readable, interest.writable) {
            interest.readable = wanted.0;
            interest.writable = wanted.1;
            if let Err(e) = pty.reregister(poller, interest, mode) {
                return End::Error(e.to_string());
            }
        }
    }
}

/// Result of reading what is available.
enum Drained {
    /// Something was read (or it stopped for flow control).
    Data,
    /// There was nothing.
    Empty,
    /// The other end of the PTY was closed (Unix only).
    Eof,
}

/// Reads everything available and sends it to the view. With `throttle` it
/// stops when the maximum of output waiting to be painted is reached.
fn read_available(
    pty: &mut tty::Pty,
    link: &PtyLink,
    buf: &mut [u8],
    out: &mpsc::UnboundedSender<Out>,
    throttle: bool,
) -> std::io::Result<Drained> {
    let mut any = false;
    loop {
        if throttle && link.throttled() {
            return Ok(Drained::Data);
        }
        match pty.reader().read(buf) {
            Ok(0) if any => return Ok(Drained::Data),
            // On Windows 0 means "nothing for now"; on Unix, that it hung up.
            Ok(0) if cfg!(windows) => return Ok(Drained::Empty),
            Ok(0) => return Ok(Drained::Eof),
            Ok(n) => {
                any = true;
                link.in_flight.fetch_add(n, Ordering::AcqRel);
                let _ = out.send(Out::Data(Bytes::copy_from_slice(&buf[..n])));
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                return Ok(if any { Drained::Data } else { Drained::Empty });
            }
            // The error will show up on the next read.
            Err(_) if any => return Ok(Drained::Data),
            Err(e) => return Err(e),
        }
    }
}

/// Writes what is pending until the PTY accepts no more.
fn write_pending(pty: &mut tty::Pty, pending: &mut VecDeque<Bytes>) -> std::io::Result<()> {
    while let Some(front) = pending.front_mut() {
        match pty.writer().write(front) {
            Ok(0) => break,
            Ok(n) if n >= front.len() => {
                pending.pop_front();
            }
            Ok(n) => {
                let _ = front.split_to(n);
            }
            Err(e) if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_shell_prefers_env_then_bash_then_sh() {
        let all = |_: &std::path::Path| true;
        assert_eq!(
            unix_shell(Some("/usr/bin/zsh"), all),
            ShellCommand {
                program: "/usr/bin/zsh".into(),
                args: vec!["-l".into()],
            }
        );
        // $SHELL that does not exist, is empty or relative: bash is used.
        let only_system = |p: &std::path::Path| p.starts_with("/bin");
        assert_eq!(
            unix_shell(Some("/opt/missing/fish"), only_system).program,
            "/bin/bash"
        );
        assert_eq!(unix_shell(Some(""), only_system).program, "/bin/bash");
        assert_eq!(unix_shell(Some("zsh"), all).program, "/bin/bash");
        assert_eq!(unix_shell(None, all).program, "/bin/bash");
        // Without bash: sh.
        let only_sh = |p: &std::path::Path| p == std::path::Path::new("/bin/sh");
        assert_eq!(unix_shell(None, only_sh).program, "/bin/sh");
        assert_eq!(unix_shell(None, |_| false).program, "/bin/sh");
    }

    #[test]
    fn windows_shell_prefers_pwsh_then_powershell_then_comspec() {
        let path = Some(
            "C:\\Windows\\system32;C:\\Program Files\\PowerShell\\7\\;\"C:\\Tools\";C:\\Windows\\System32\\WindowsPowerShell\\v1.0",
        );
        let root = Some("C:\\Windows");
        let comspec = Some("C:\\Windows\\system32\\cmd.exe");

        let everything = |_: &str| true;
        let sh = windows_shell(path, root, comspec, everything);
        assert_eq!(sh.program, "C:\\Windows\\system32\\pwsh.exe");
        assert_eq!(sh.args, vec!["-NoLogo".to_string()]);

        let pwsh7 = |p: &str| p == "C:\\Program Files\\PowerShell\\7\\pwsh.exe";
        assert_eq!(
            windows_shell(path, root, comspec, pwsh7).program,
            "C:\\Program Files\\PowerShell\\7\\pwsh.exe"
        );

        let winps = |p: &str| p.ends_with("WindowsPowerShell\\v1.0\\powershell.exe");
        assert_eq!(
            windows_shell(path, root, comspec, winps).program,
            "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
        );
        // PowerShell outside the PATH: it is looked for in %SystemRoot%.
        assert_eq!(
            windows_shell(Some("C:\\Tools"), root, comspec, winps).program,
            "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
        );

        let nothing = |_: &str| false;
        let cmd = windows_shell(path, root, comspec, nothing);
        assert_eq!(cmd.program, "C:\\Windows\\system32\\cmd.exe");
        assert!(cmd.args.is_empty());
        assert_eq!(windows_shell(None, None, None, nothing).program, "cmd.exe");
    }

    #[test]
    fn utf8_locale_only_when_missing() {
        let with =
            |name: &'static str| move |k: &str| (k == name).then(|| "es_ES.UTF-8".to_string());
        assert_eq!(utf8_locale(with("LANG")), None);
        assert_eq!(utf8_locale(with("LC_ALL")), None);
        assert_eq!(utf8_locale(with("LC_CTYPE")), None);
        let none = |_: &str| None;
        let expected = if cfg!(windows) {
            None
        } else if cfg!(target_os = "macos") {
            Some(("LC_CTYPE", "UTF-8"))
        } else {
            Some(("LC_CTYPE", "C.UTF-8"))
        };
        assert_eq!(utf8_locale(none), expected);
        assert_eq!(utf8_locale(|_: &str| Some(" ".into())), expected);
    }

    #[cfg(unix)]
    #[test]
    fn exit_messages() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            exit_message(Some(std::process::ExitStatus::from_raw(0))),
            "The shell exited (code 0)"
        );
        assert_eq!(
            exit_message(Some(std::process::ExitStatus::from_raw(2 << 8))),
            "The shell exited (code 2)"
        );
        assert_eq!(
            exit_message(Some(std::process::ExitStatus::from_raw(9))),
            "The shell was ended by signal 9"
        );
        assert_eq!(exit_message(None), "The shell exited");
    }

    /// A real shell in a PTY: a command is written and its output read, the
    /// size is changed and it is closed.
    #[cfg(unix)]
    #[test]
    fn local_shell_round_trip() {
        let (backend, mut rx) = start(80, 24);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut seen = String::new();
        let mut ready = false;
        let mut sent = false;
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(Out::Shell) => ready = true,
                Ok(Out::Data(b)) => {
                    backend.consumed(b.len());
                    seen.push_str(&String::from_utf8_lossy(&b));
                }
                Ok(Out::Failed(e)) => panic!("could not open the shell: {e}"),
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
            if ready && !sent {
                backend.send(Cmd::Resize(100, 30));
                backend.input(&b"stty size; echo end-$((40+2))\r"[..]);
                sent = true;
            }
            if seen.contains("end-42") {
                break;
            }
        }
        assert!(seen.contains("30 100"), "output: {seen}");
        assert!(seen.contains("end-42"), "output: {seen}");
        // `exit`: the shell exits and the code is reported.
        backend.input(&b"exit 3\r"[..]);
        let mut closed = None;
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(Out::Closed(reason)) => {
                    closed = reason;
                    break;
                }
                Ok(Out::Data(b)) => backend.consumed(b.len()),
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        assert_eq!(closed.as_deref(), Some("The shell exited (code 3)"));
        backend.send(Cmd::Close);
    }
}
