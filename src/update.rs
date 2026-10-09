//! Automatic updates.
//!
//! - At startup, before opening any window, the update downloaded in the
//!   previous session is applied and the app relaunches already updated.
//! - While the app is open, a new version is looked for every 6 hours; if the
//!   installation can be replaced, it is downloaded silently and the user is
//!   told it will be applied on restart. If the system manages it, the user
//!   is only notified.
//!
//! Updates are signed (Ed25519). Without a compiled-in public key
//! (`TERMOAK_UPDATE_PUBKEY`) there are no updates. As a Flatpak there are
//! none either, whatever the build: Flatpak updates the app itself
//! (`flatpak update`, the software center) and the installation in `/app`
//! cannot be replaced.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui::{Context, EventEmitter, Task};
use termoak_update::{Applied, Install, UpdateConfig, Updater};

use crate::runtime;

/// Default manifest (latest version published on GitHub).
pub const DEFAULT_MANIFEST: &str =
    "https://github.com/TermoakSSH/desktop/releases/latest/download/latest.json";
/// Downloads page (for installations managed by the system).
pub const RELEASES_PAGE: &str = "https://github.com/TermoakSSH/desktop/releases/latest";

const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const FIRST_CHECK: Duration = Duration::from_secs(20);

/// Version of this build.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Who updates this installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Updates<'a> {
    /// The app, with this compiled-in public key.
    App(&'a str),
    /// Flatpak.
    Flatpak,
    /// Nobody: no public key in this build.
    Off,
}

fn updates(public_key: Option<&str>, flatpak: bool) -> Updates<'_> {
    if flatpak {
        return Updates::Flatpak;
    }
    match public_key.map(str::trim).filter(|k| !k.is_empty()) {
        Some(key) => Updates::App(key),
        None => Updates::Off,
    }
}

/// Builds the updater if a public key was compiled in and the app is not a
/// Flatpak.
pub fn build(data_dir: &Path) -> Option<Arc<Updater>> {
    let key = match updates(
        option_env!("TERMOAK_UPDATE_PUBKEY"),
        crate::flatpak::active(),
    ) {
        Updates::App(key) => key,
        Updates::Flatpak => {
            tracing::info!("running as a Flatpak: Flatpak updates the app");
            return None;
        }
        Updates::Off => return None,
    };
    let Some(public_key) = termoak_update::public_key_from_base64(key) else {
        tracing::warn!("TERMOAK_UPDATE_PUBKEY is not valid: updates disabled");
        return None;
    };
    // In order: the variable at run time (testing), the one set at build time
    // (CI takes it from the repository variable; with the private repository
    // it points to our own server) and, otherwise, the public GitHub release.
    let manifest_url = std::env::var("TERMOAK_UPDATE_URL")
        .ok()
        .or_else(|| option_env!("TERMOAK_UPDATE_URL").map(str::to_string))
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_MANIFEST.to_string());
    let current_version = semver::Version::parse(current_version()).ok()?;
    Some(Arc::new(Updater::new(UpdateConfig {
        manifest_url,
        public_key,
        current_version,
        target: termoak_update::current_target(),
        install: Install::detect(),
        staging_dir: data_dir.join("updates"),
    })))
}

/// Applies the pending update. If it was applied, relaunches the app (and
/// this process ends); if it could not relaunch, startup continues.
pub fn apply_at_startup(updater: &Updater) {
    match updater.apply_pending() {
        Ok(Applied::Updated(version)) => {
            tracing::info!(%version, "update applied; relaunching");
            relaunch(&updater.config().install);
        }
        Ok(Applied::Nothing) => {}
        Err(e) => tracing::warn!(error = %e, "could not apply the pending update"),
    }
}

/// Relaunches the app. If it cannot, this process carries on.
fn relaunch(install: &Install) {
    let e = termoak_update::relaunch(install);
    tracing::warn!(error = %e, "could not relaunch; carrying on with this process");
}

/// State of the updates (for Settings).
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateStatus {
    /// No public key: updates disabled in this build.
    Disabled,
    /// A Flatpak: Flatpak updates it (no checks of our own).
    Flatpak,
    Idle,
    Checking,
    UpToDate,
    Downloading(String),
    /// Downloaded and verified: it will be applied on restart.
    Ready(String),
    /// There is a new version but the system manages the installation.
    Available(String),
    Failed(String),
}

/// Events to show notifications.
#[derive(Debug, Clone)]
pub enum UpdateEvent {
    Ready(String),
    Available(String),
}

/// Periodic check in the background.
pub struct UpdateModel {
    pub updater: Option<Arc<Updater>>,
    pub status: UpdateStatus,
    announced: Option<String>,
    _loop: Option<Task<()>>,
}

impl EventEmitter<UpdateEvent> for UpdateModel {}

impl UpdateModel {
    pub fn new(updater: Option<Arc<Updater>>, cx: &mut Context<Self>) -> Self {
        let status = initial_status(updater.is_some(), crate::flatpak::active());
        let task = updater.as_ref().map(|_| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(FIRST_CHECK).await;
                loop {
                    if this.update(cx, |m, cx| m.check_now(cx)).is_err() {
                        break;
                    }
                    cx.background_executor().timer(CHECK_EVERY).await;
                }
            })
        });
        Self {
            updater,
            status,
            announced: None,
            _loop: task,
        }
    }

    /// Whether this app looks for updates itself (Settings shows "Check for
    /// updates" only then).
    pub fn checks(&self) -> bool {
        self.updater.is_some()
    }

    /// Version it can relaunch with right now, if one was downloaded.
    pub fn ready_version(&self) -> Option<&str> {
        match &self.status {
            UpdateStatus::Ready(v) => Some(v),
            _ => None,
        }
    }

    /// Looks for (and downloads, when possible) a new version.
    pub fn check_now(&mut self, cx: &mut Context<Self>) {
        let Some(updater) = self.updater.clone() else {
            return;
        };
        if matches!(
            self.status,
            UpdateStatus::Checking | UpdateStatus::Downloading(_) | UpdateStatus::Ready(_)
        ) {
            return;
        }
        self.status = UpdateStatus::Checking;
        cx.notify();
        let u = updater.clone();
        runtime::run(cx, async move { u.check().await }, move |m, res, cx| {
            match res {
                Ok(None) => m.status = UpdateStatus::UpToDate,
                Ok(Some(av)) if av.notify_only => {
                    m.status = UpdateStatus::Available(av.version.clone());
                    if m.announced.as_deref() != Some(&av.version) {
                        m.announced = Some(av.version.clone());
                        cx.emit(UpdateEvent::Available(av.version));
                    }
                }
                Ok(Some(av)) => {
                    m.status = UpdateStatus::Downloading(av.version.clone());
                    let u = updater.clone();
                    runtime::run(
                        cx,
                        async move {
                            u.download(&av, |_, _| {}).await?;
                            Ok::<_, termoak_update::UpdateError>(av.version)
                        },
                        |m, res, cx| {
                            match res {
                                Ok(version) => {
                                    m.status = UpdateStatus::Ready(version.clone());
                                    cx.emit(UpdateEvent::Ready(version));
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "update download failed");
                                    m.status = UpdateStatus::Failed(e);
                                }
                            }
                            cx.notify();
                        },
                    );
                }
                Err(e) => {
                    tracing::debug!(error = %e, "update check failed");
                    m.status = UpdateStatus::Failed(e);
                }
            }
            cx.notify();
        });
    }

    /// Relaunches the app to apply the downloaded update.
    pub fn restart_now(&self) {
        if let Some(updater) = &self.updater {
            relaunch(&updater.config().install);
        }
    }

    /// Text of the state for Settings.
    pub fn status_text(&self) -> String {
        let text = match &self.status {
            UpdateStatus::Disabled => t!("update.status.disabled"),
            UpdateStatus::Flatpak => t!("update.status.flatpak"),
            UpdateStatus::Idle => t!("update.status.idle"),
            UpdateStatus::Checking => t!("update.status.checking"),
            UpdateStatus::UpToDate => t!("update.status.up_to_date"),
            UpdateStatus::Downloading(v) => t!("update.status.downloading", version = v),
            UpdateStatus::Ready(v) => t!("update.status.ready", version = v),
            UpdateStatus::Available(v) => t!("update.status.available", version = v),
            UpdateStatus::Failed(e) => t!("update.status.failed", error = e),
        };
        text.to_string()
    }
}

/// State before the first check.
fn initial_status(has_updater: bool, flatpak: bool) -> UpdateStatus {
    match (has_updater, flatpak) {
        (true, _) => UpdateStatus::Idle,
        (false, true) => UpdateStatus::Flatpak,
        (false, false) => UpdateStatus::Disabled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "zwbxnEY2xFDcaICtxAW8BVhccqXWdjXae4bsDB+Kkt4=";

    #[test]
    fn a_flatpak_never_updates_itself() {
        // Even with the official public key compiled in.
        assert_eq!(updates(Some(KEY), true), Updates::Flatpak);
        assert_eq!(updates(None, true), Updates::Flatpak);
        assert_eq!(updates(Some(KEY), false), Updates::App(KEY));
        assert_eq!(updates(Some(" \n"), false), Updates::Off);
        assert_eq!(updates(None, false), Updates::Off);
        assert!(termoak_update::public_key_from_base64(KEY).is_some());
    }

    #[test]
    fn status_without_updater() {
        assert_eq!(initial_status(true, false), UpdateStatus::Idle);
        assert_eq!(initial_status(false, true), UpdateStatus::Flatpak);
        assert_eq!(initial_status(false, false), UpdateStatus::Disabled);
    }
}
