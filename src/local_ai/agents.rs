//! AI agents installed on this computer: Codex (`codex`), Claude Code
//! (`claude`), Antigravity (`agy`) and OpenCode (`opencode`).
//!
//! They are looked for in `PATH` and in the usual install folders of each
//! system: apps opened from the desktop (above all on macOS) do not get the
//! `PATH` of the user's shell. Each one uses its own sign-in or subscription.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Maximum wait for `--version`.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// An agent Termoak knows how to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentKind {
    /// Value saved in the settings.
    pub id: &'static str,
    /// Product name (not translated).
    pub name: &'static str,
    /// Executable.
    pub command: &'static str,
    /// Installation instructions.
    pub install_url: &'static str,
    /// What to run in a terminal to sign in.
    pub login_command: &'static str,
}

pub const AGENTS: [AgentKind; 4] = [
    AgentKind {
        id: "codex",
        name: "Codex",
        command: "codex",
        install_url: "https://developers.openai.com/codex/cli",
        login_command: "codex login",
    },
    AgentKind {
        id: "claude",
        name: "Claude Code",
        command: "claude",
        install_url: "https://code.claude.com/docs/en/setup",
        login_command: "claude",
    },
    AgentKind {
        id: "agy",
        name: "Antigravity",
        command: "agy",
        install_url: "https://antigravity.google/download",
        login_command: "agy",
    },
    AgentKind {
        id: "opencode",
        name: "OpenCode",
        command: "opencode",
        install_url: "https://opencode.ai/docs/",
        login_command: "opencode auth login",
    },
];

/// The agent with that id.
pub fn kind(id: &str) -> Option<&'static AgentKind> {
    AGENTS.iter().find(|a| a.id == id)
}

/// What was found of an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    pub id: &'static str,
    /// Executable (`None` = not installed).
    pub path: Option<PathBuf>,
    /// What `--version` says (`None` if it did not answer in time).
    pub version: Option<String>,
}

/// Folders to search, in order: `PATH`, then the usual install folders.
/// Pure (the environment comes in) so it can be tested on any system.
fn search_dirs_from(
    path: Option<OsString>,
    home: Option<&Path>,
    appdata: Option<&Path>,
    local_appdata: Option<&Path>,
    windows: bool,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let mut extra: Vec<PathBuf> = Vec::new();
    if let Some(home) = home {
        for sub in [
            ".local/bin",
            ".npm-global/bin",
            ".bun/bin",
            ".opencode/bin",
            ".claude/local",
            ".volta/bin",
            "bin",
        ] {
            extra.push(home.join(sub));
        }
        if windows {
            extra.push(home.join("scoop").join("shims"));
        }
    }
    if windows {
        if let Some(d) = appdata {
            extra.push(d.join("npm"));
        }
        if let Some(d) = local_appdata {
            extra.push(d.join("Microsoft").join("WinGet").join("Links"));
            extra.push(d.join("Programs").join("Antigravity").join("bin"));
        }
    } else {
        for d in [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/snap/bin",
            "/Applications/Antigravity.app/Contents/Resources/app/bin",
        ] {
            extra.push(PathBuf::from(d));
        }
    }
    dirs.extend(extra);
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| !d.as_os_str().is_empty() && seen.insert(d.clone()));
    dirs
}

/// Node versions installed with nvm (`~/.nvm/versions/node/*/bin`), newest
/// first: global npm packages (Codex, Claude Code...) live there.
fn nvm_dirs(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(home.join(".nvm/versions/node")) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path().join("bin"))
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs.reverse();
    dirs
}

fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Folders where agents are looked for on this computer.
pub fn search_dirs() -> Vec<PathBuf> {
    let home = home_dir();
    let mut dirs = search_dirs_from(
        std::env::var_os("PATH"),
        home.as_deref(),
        env_dir("APPDATA").as_deref(),
        env_dir("LOCALAPPDATA").as_deref(),
        cfg!(windows),
    );
    if let Some(home) = &home {
        for d in nvm_dirs(home) {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    dirs
}

/// `PATH` for running an agent: npm-installed ones are scripts that need
/// `node`, which may also be outside the app's `PATH`.
pub fn child_path(dirs: &[PathBuf]) -> Option<OsString> {
    std::env::join_paths(dirs).ok()
}

/// File names an executable may have.
fn exe_names(command: &str, windows: bool) -> Vec<String> {
    if windows {
        ["exe", "cmd", "bat"]
            .iter()
            .map(|ext| format!("{command}.{ext}"))
            .collect()
    } else {
        vec![command.to_string()]
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The first executable called `command` in `dirs`.
pub fn find_in(dirs: &[PathBuf], command: &str) -> Option<PathBuf> {
    let names = exe_names(command, cfg!(windows));
    dirs.iter()
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| is_executable(p))
}

/// The version in what `--version` printed: the first word that looks like
/// a version (`codex-cli 0.46.0`, `1.0.120 (Claude Code)`, `v0.15.3`...)
/// or, if there is none, its first line (shortened).
pub fn parse_version(output: &str) -> Option<String> {
    let looks_like_version = |t: &str| {
        let core = t.split(['-', '+']).next().unwrap_or("");
        let parts: Vec<&str> = core.split('.').collect();
        parts.len() >= 2
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    };
    for line in output.lines() {
        for word in line.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';')) {
            let word = word
                .trim_end_matches(['.', ':'])
                .trim_start_matches(['v', 'V']);
            if looks_like_version(word) {
                return Some(word.to_string());
            }
        }
    }
    let first = output.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(if first.chars().count() > 60 {
        format!("{}…", first.chars().take(60).collect::<String>())
    } else {
        first.to_string()
    })
}

/// Runs `<path> --version` (with a time limit) and reads the version.
pub async fn version(path: &Path, child_path: Option<&OsString>) -> Option<String> {
    let mut cmd = tokio::process::Command::new(path);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(p) = child_path {
        cmd.env("PATH", p);
    }
    #[cfg(windows)]
    {
        // No console window flashing.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = match tokio::time::timeout(VERSION_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            tracing::debug!(path = %path.display(), error = %e, "could not run the agent");
            return None;
        }
        Err(_) => {
            tracing::debug!(path = %path.display(), "the agent did not answer --version in time");
            return None;
        }
    };
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    if text.trim().is_empty() {
        text = String::from_utf8_lossy(&out.stderr).into_owned();
    }
    parse_version(&text)
}

/// Where each known agent is, without running it (fast).
pub async fn locate() -> Vec<AgentStatus> {
    tokio::task::spawn_blocking(|| {
        let dirs = search_dirs();
        AGENTS
            .iter()
            .map(|a| AgentStatus {
                id: a.id,
                path: find_in(&dirs, a.command),
                version: None,
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// Looks for every known agent and asks each one found for its version (in
/// parallel). Runs on the tokio runtime, never on the interface thread.
pub async fn detect() -> Vec<AgentStatus> {
    let found = tokio::task::spawn_blocking(|| {
        let dirs = search_dirs();
        let paths: Vec<Option<PathBuf>> =
            AGENTS.iter().map(|a| find_in(&dirs, a.command)).collect();
        (dirs, paths)
    })
    .await;
    let Ok((dirs, paths)) = found else {
        return AGENTS
            .iter()
            .map(|a| AgentStatus {
                id: a.id,
                path: None,
                version: None,
            })
            .collect();
    };
    let env_path = child_path(&dirs);
    let versions = futures::future::join_all(paths.iter().map(|p| {
        let env_path = env_path.clone();
        async move {
            match p {
                Some(p) => version(p, env_path.as_ref()).await,
                None => None,
            }
        }
    }))
    .await;
    AGENTS
        .iter()
        .zip(paths)
        .zip(versions)
        .map(|((a, path), version)| AgentStatus {
            id: a.id,
            path,
            version,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_from_the_usual_outputs() {
        assert_eq!(
            parse_version("codex-cli 0.46.0\n").as_deref(),
            Some("0.46.0")
        );
        assert_eq!(
            parse_version("1.0.120 (Claude Code)\n").as_deref(),
            Some("1.0.120")
        );
        assert_eq!(parse_version("opencode v0.15.3").as_deref(), Some("0.15.3"));
        assert_eq!(parse_version("0.15.3").as_deref(), Some("0.15.3"));
        assert_eq!(
            parse_version("Antigravity 1.11.2-beta.1, commit abc1234").as_deref(),
            Some("1.11.2-beta.1")
        );
        // Warnings before the version.
        assert_eq!(
            parse_version("WARNING: something odd\nagy version 2.0.1.\n").as_deref(),
            Some("2.0.1")
        );
        // No version: the first line, shortened.
        assert_eq!(parse_version("dev build").as_deref(), Some("dev build"));
        assert_eq!(
            parse_version(&"x".repeat(80)).map(|v| v.chars().count()),
            Some(61)
        );
        assert_eq!(parse_version("  \n\n"), None);
        assert_eq!(parse_version(""), None);
        // Not versions: a single number, words with dots.
        assert_eq!(parse_version("build 42").as_deref(), Some("build 42"));
        assert_eq!(parse_version("see a.b").as_deref(), Some("see a.b"));
    }

    #[test]
    fn search_dirs_cover_the_usual_install_folders() {
        let home = Path::new("/home/ana");
        let path = std::env::join_paths(["/usr/bin", "/home/ana/.local/bin", ""]).unwrap();
        let dirs = search_dirs_from(Some(path), Some(home), None, None, false);
        // PATH first, without duplicates nor empty entries.
        assert_eq!(dirs[0], PathBuf::from("/usr/bin"));
        assert_eq!(dirs[1], PathBuf::from("/home/ana/.local/bin"));
        assert_eq!(
            dirs.iter()
                .filter(|d| **d == PathBuf::from("/usr/bin"))
                .count(),
            1
        );
        assert!(!dirs.iter().any(|d| d.as_os_str().is_empty()));
        for d in [
            "/home/ana/.npm-global/bin",
            "/home/ana/.bun/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
        ] {
            assert!(dirs.contains(&PathBuf::from(d)), "{d}");
        }

        let dirs = search_dirs_from(
            None,
            Some(Path::new("C:/Users/ana")),
            Some(Path::new("C:/Users/ana/AppData/Roaming")),
            Some(Path::new("C:/Users/ana/AppData/Local")),
            true,
        );
        assert!(dirs.contains(&PathBuf::from("C:/Users/ana/AppData/Roaming").join("npm")));
        assert!(dirs.contains(&Path::new("C:/Users/ana").join("scoop").join("shims")));
        assert!(!dirs.contains(&PathBuf::from("/opt/homebrew/bin")));
    }

    #[test]
    fn executable_names_per_system() {
        assert_eq!(exe_names("codex", false), ["codex"]);
        assert_eq!(
            exe_names("codex", true),
            ["codex.exe", "codex.cmd", "codex.bat"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn finds_only_executables() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("termoak-agents-{}", uuid::Uuid::new_v4()));
        let (a, b) = (base.join("a"), base.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        // Not executable in `a`; executable in `b`.
        std::fs::write(a.join("codex"), "#!/bin/sh\n").unwrap();
        std::fs::write(b.join("codex"), "#!/bin/sh\necho codex-cli 9.8.7\n").unwrap();
        std::fs::set_permissions(b.join("codex"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let dirs = vec![a.clone(), b.clone()];
        assert_eq!(find_in(&dirs, "codex"), Some(b.join("codex")));
        assert_eq!(find_in(&dirs, "claude"), None);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let v = rt.block_on(version(&b.join("codex"), None));
        assert_eq!(v.as_deref(), Some("9.8.7"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn every_agent_is_known() {
        for a in AGENTS {
            assert_eq!(kind(a.id), Some(&a));
            assert!(a.install_url.starts_with("https://"));
        }
        assert_eq!(kind("nope"), None);
    }
}
