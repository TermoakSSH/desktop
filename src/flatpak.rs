//! Running as a Flatpak (`com.termoak.Termoak`, from Flathub or from
//! pkg.termoak.com/flatpak; manifest in `flatpak/`).
//!
//! Inside the sandbox:
//! - Flatpak updates the app (`flatpak update`, the software center), so the
//!   app's own updater is off (`update.rs`).
//! - The local terminal opens the user's shell on the host with
//!   `flatpak-spawn --host` (`terminal/shell.rs`): the sandbox only has the
//!   runtime's tools, not the user's.

#[cfg(unix)]
use std::collections::HashMap;
use std::sync::OnceLock;

/// Whether this process runs inside a Flatpak sandbox: `FLATPAK_ID` is set
/// by `flatpak run` and `/.flatpak-info` exists in every sandbox (also when
/// the environment was cleared).
pub fn active() -> bool {
    static ACTIVE: OnceLock<bool> = OnceLock::new();
    *ACTIVE.get_or_init(|| {
        detect(
            std::env::var_os("FLATPAK_ID").as_deref(),
            std::path::Path::new("/.flatpak-info").is_file(),
        )
    })
}

fn detect(flatpak_id: Option<&std::ffi::OsStr>, info_file: bool) -> bool {
    flatpak_id.is_some_and(|id| !id.is_empty()) || info_file
}

/// Command that runs a login shell of the host from the sandbox: the user's
/// shell from the password database (or `$SHELL`, or `/bin/sh` on the host),
/// in the same folder, with the terminal variables (the host process does
/// not inherit the sandbox's environment). `--watch-bus` ends it if the app
/// goes away; the PTY passed as stdin becomes its controlling terminal.
#[cfg(unix)]
pub fn host_shell(env: &HashMap<String, String>) -> (String, Vec<String>) {
    const LOGIN_SHELL: &str = r#"s="$(getent passwd "$(id -un)" | cut -d: -f7)"; [ -x "$s" ] || s="${SHELL:-}"; [ -x "$s" ] || s=/bin/sh; exec "$s" -l"#;
    let mut args = vec!["--host".to_string(), "--watch-bus".to_string()];
    let mut vars: Vec<_> = env.iter().collect();
    vars.sort();
    args.extend(vars.into_iter().map(|(k, v)| format!("--env={k}={v}")));
    args.extend(["--", "/bin/sh", "-c", LOGIN_SHELL].map(str::to_string));
    ("flatpak-spawn".to_string(), args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn detected_by_the_variable_or_the_info_file() {
        assert!(detect(Some(OsStr::new("com.termoak.Termoak")), false));
        assert!(detect(None, true));
        assert!(!detect(None, false));
        assert!(!detect(Some(OsStr::new("")), false));
    }

    #[cfg(unix)]
    #[test]
    fn host_shell_passes_the_environment_and_a_login_shell() {
        let env = HashMap::from([
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ]);
        let (program, args) = host_shell(&env);
        assert_eq!(program, "flatpak-spawn");
        assert_eq!(
            &args[..4],
            [
                "--host",
                "--watch-bus",
                "--env=COLORTERM=truecolor",
                "--env=TERM=xterm-256color"
            ]
        );
        let sep = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(&args[sep + 1..sep + 3], ["/bin/sh", "-c"]);
        assert!(args[sep + 3].ends_with(r#"exec "$s" -l"#));
        assert_eq!(args.len(), sep + 4);
    }
}
