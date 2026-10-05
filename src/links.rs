//! `termoak://` links opened from outside the app (a browser, another app):
//! `termoak://join?server=…&token=…` joins a shared session and
//! `termoak://invite?server=…&token=…` signs up with an invitation.
//! `aceitunoak://`, the scheme before the rename, still works.
//!
//! How they arrive:
//! - macOS: the system hands them to the running app (`on_open_urls`).
//! - Linux and Windows: as an argument of a new process (the scheme is
//!   registered by the `.desktop` file, or by the app itself on Windows).
//!   On Linux, if the app is already running, the new process passes the
//!   link to it through a socket in the data folder and exits.

use std::path::Path;
use std::sync::OnceLock;

use parking_lot::Mutex;
use tokio::sync::mpsc;

/// Schemes this app opens.
pub const SCHEMES: [&str; 2] = ["termoak", "aceitunoak"];

/// The link is one of ours.
pub fn is_app_link(text: &str) -> bool {
    let text = text.trim();
    SCHEMES.iter().any(|s| {
        text.len() > s.len() + 3
            && text[..s.len()].eq_ignore_ascii_case(s)
            && text[s.len()..].starts_with("://")
    })
}

/// The first app link among the arguments of the process.
pub fn from_args(args: impl IntoIterator<Item = String>) -> Option<String> {
    args.into_iter().skip(1).find(|a| is_app_link(a))
}

/// Links waiting for the window (arrive before it opens or while it runs).
struct Inbox {
    tx: mpsc::UnboundedSender<String>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
}

fn inbox() -> &'static Inbox {
    static INBOX: OnceLock<Inbox> = OnceLock::new();
    INBOX.get_or_init(|| {
        let (tx, rx) = mpsc::unbounded_channel();
        Inbox {
            tx,
            rx: Mutex::new(Some(rx)),
        }
    })
}

/// A link for the window (from any thread).
pub fn deliver(link: String) {
    if is_app_link(&link) {
        let _ = inbox().tx.send(link.trim().to_string());
    }
}

/// The links that arrive, for the window (only once).
pub fn take_receiver() -> Option<mpsc::UnboundedReceiver<String>> {
    inbox().rx.lock().take()
}

#[cfg(unix)]
fn socket_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("links.sock")
}

/// Passes `link` to an instance that is already running. `true` if it took
/// it (this process can exit).
#[cfg(unix)]
pub fn forward_to_running(data_dir: &Path, link: &str) -> bool {
    use std::io::Write;
    match std::os::unix::net::UnixStream::connect(socket_path(data_dir)) {
        Ok(mut s) => s.write_all(format!("{}\n", link.trim()).as_bytes()).is_ok(),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
pub fn forward_to_running(_data_dir: &Path, _link: &str) -> bool {
    false
}

/// Receives the links of later launches (Linux; macOS gets them from the
/// system).
#[cfg(unix)]
pub fn listen(data_dir: &Path) {
    use std::io::BufRead;
    let path = socket_path(data_dir);
    // Another instance listens already: it keeps receiving them.
    if std::os::unix::net::UnixStream::connect(&path).is_ok() {
        return;
    }
    // Left over by an instance that did not exit cleanly (nobody answered).
    let _ = std::fs::remove_file(&path);
    let listener = match std::os::unix::net::UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::debug!(error = %e, "links socket not available");
            return;
        }
    };
    let _ = std::thread::Builder::new()
        .name("termoak-links".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                // A link is short: whatever is longer is not read.
                let mut reader = std::io::BufReader::new(std::io::Read::take(stream, 8 * 1024));
                if reader.read_line(&mut line).is_ok() {
                    deliver(line);
                }
            }
        });
}

#[cfg(not(unix))]
pub fn listen(_data_dir: &Path) {}

/// Windows: registers `termoak://` and `aceitunoak://` for this user so the
/// browser opens them with this program (done again if the program moved).
#[cfg(windows)]
pub fn register_scheme(data_dir: &Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let exe = exe.display().to_string();
    let marker = data_dir.join("url-scheme");
    if std::fs::read_to_string(&marker).is_ok_and(|m| m == exe) {
        return;
    }
    let reg = |args: &[&str]| {
        std::process::Command::new("reg")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    let command = format!("\"{exe}\" \"%1\"");
    let mut ok = true;
    for scheme in SCHEMES {
        let key = format!(r"HKCU\Software\Classes\{scheme}");
        ok &= reg(&["add", &key, "/ve", "/d", "URL:Termoak", "/f"]);
        ok &= reg(&["add", &key, "/v", "URL Protocol", "/d", "", "/f"]);
        ok &= reg(&[
            "add",
            &format!(r"{key}\shell\open\command"),
            "/ve",
            "/d",
            &command,
            "/f",
        ]);
    }
    if ok {
        let _ = std::fs::write(&marker, exe);
    }
}

#[cfg(not(windows))]
pub fn register_scheme(_data_dir: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_links_in_arguments() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            from_args(args(&[
                "termoak-desktop",
                "--x",
                "termoak://join?server=a&token=b"
            ])),
            Some("termoak://join?server=a&token=b".into())
        );
        assert_eq!(
            from_args(args(&["termoak-desktop", "AceitunoaK://invite?x"])),
            Some("AceitunoaK://invite?x".into())
        );
        // The program itself is never a link, nor are other schemes.
        assert_eq!(from_args(args(&["termoak://join"])), None);
        assert_eq!(
            from_args(args(&["termoak-desktop", "https://a.b/join/x", "termoak:"])),
            None
        );
        assert!(!is_app_link("termoak://"));
    }
}
