//! Host logos: the avatar of a host shows its chosen logo (`Host::icon`),
//! else the logo of the system detected when connecting (`Host::os`), else
//! its colored initials. Hosts, tabs, palette rows and split pane headers
//! show the same logo.
//!
//! Two families:
//! - systems: white logos drawn on the system's color, the set the Android
//!   app ships (`assets/logos/*.svg`). The paths come from Simple Icons
//!   (<https://simpleicons.org>, CC0 1.0); the Windows one is drawn by
//!   hand. The logos are trademarks of their owners and only identify each
//!   system.
//! - generic: icons of the app's Lucide set (server, database, router...).
//!
//! The ids are stored in the host and synced: never rename one. An id this
//! version does not know (a later app's) falls back to the automatic logo.

use std::borrow::Cow;

use gpui::{AssetSource, Div, Hsla, ParentElement, Pixels, SharedString, Styled, div, px};
use gpui_component::Icon;
use gpui_component::StyledExt;
use termoak_core::model::Host;

use crate::theme;
use crate::ui::IconName;

/// What a logo is drawn with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Glyph {
    /// `logos/<id>.svg` of [`Assets`].
    System,
    Lucide(IconName),
}

/// Kind of logo (the picker groups them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogoKind {
    System,
    Generic,
}

/// A logo a host can show.
#[derive(Debug, PartialEq, Eq)]
pub struct Logo {
    /// Stored in `Host::icon`.
    pub id: &'static str,
    /// Name of a system (not translated); generic ones use `logo.<id>`.
    name: &'static str,
    /// Background of the avatar (`0xrrggbb`).
    color: u32,
    glyph: Glyph,
    pub kind: LogoKind,
}

const fn system(id: &'static str, name: &'static str, color: u32) -> Logo {
    Logo {
        id,
        name,
        color,
        glyph: Glyph::System,
        kind: LogoKind::System,
    }
}

const fn generic(id: &'static str, icon: IconName, color: u32) -> Logo {
    Logo {
        id,
        name: "",
        color,
        glyph: Glyph::Lucide(icon),
        kind: LogoKind::Generic,
    }
}

/// Every logo, in the order of the picker.
pub static LOGOS: &[Logo] = &[
    system("ubuntu", "Ubuntu", 0xe95420),
    system("debian", "Debian", 0xd70a53),
    system("fedora", "Fedora", 0x3c6eb4),
    system("rhel", "Red Hat", 0xee0000),
    system("centos", "CentOS", 0x9c27b0),
    system("rocky", "Rocky Linux", 0x10b981),
    system("alma", "AlmaLinux", 0x0f4266),
    system("arch", "Arch Linux", 0x1793d1),
    system("manjaro", "Manjaro", 0x35bf5c),
    system("endeavouros", "EndeavourOS", 0x7f3fbf),
    system("alpine", "Alpine", 0x0d597f),
    system("opensuse", "openSUSE", 0x73ba25),
    system("suse", "SUSE", 0x30ba78),
    system("mint", "Linux Mint", 0x87cf3e),
    system("popos", "Pop!_OS", 0x48b9c7),
    system("elementary", "elementary OS", 0x64baff),
    system("zorin", "Zorin OS", 0x15a6f0),
    system("kali", "Kali", 0x367bf0),
    system("gentoo", "Gentoo", 0x54487a),
    system("nixos", "NixOS", 0x5277c3),
    system("void", "Void Linux", 0x478061),
    system("raspberrypi", "Raspberry Pi", 0xc51a4a),
    system("linux", "Linux", 0xf5a524),
    system("freebsd", "FreeBSD", 0xab2b28),
    system("macos", "macOS", 0x8e8e93),
    system("windows", "Windows", 0x0078d4),
    generic("server", IconName::Server, 0x4f7cff),
    generic("database", IconName::Database, 0x12a594),
    generic("router", IconName::Router, 0x0ea5e9),
    generic("firewall", IconName::BrickWall, 0xe5484d),
    generic("cloud", IconName::Cloud, 0x3b82f6),
    generic("container", IconName::Container, 0x1d63ed),
    generic("kubernetes", IconName::ShipWheel, 0x326ce5),
    generic("web", IconName::Globe, 0x30a46c),
    generic("mail", IconName::Mail, 0xd6409f),
    generic("storage", IconName::HardDrive, 0x8e4ec6),
    generic("terminal", IconName::Terminal, 0x475569),
    generic("iot", IconName::Cpu, 0xf5a524),
    generic("security", IconName::Lock, 0xbf8700),
];

/// The logo with this id (as stored in `Host::icon`).
pub fn by_id(id: &str) -> Option<&'static Logo> {
    let id = id.trim();
    LOGOS.iter().find(|l| l.id.eq_ignore_ascii_case(id))
}

/// The logo of a detected system (the `ID` of `/etc/os-release`, or
/// `macos`, `windows`...).
pub fn for_os(os: &str) -> Option<&'static Logo> {
    let os = os.trim().to_ascii_lowercase();
    let id = match os.as_str() {
        "devuan" => "debian",
        "raspbian" => "raspberrypi",
        "linuxmint" => "mint",
        "pop" => "popos",
        "nobara" => "fedora",
        "redhat" => "rhel",
        "almalinux" => "alma",
        "artix" | "garuda" | "archarm" => "arch",
        "postmarketos" => "alpine",
        "opensuse-leap" | "opensuse-tumbleweed" | "opensuse-microos" => "opensuse",
        "sles" | "sled" => "suse",
        "darwin" | "osx" => "macos",
        // A generic id would make "server" look detected.
        id if by_id(id).is_some_and(|l| l.kind == LogoKind::Generic) => return None,
        id => id,
    };
    by_id(id)
}

/// What a host shows: its chosen logo, else its system's, else none (the
/// initials).
pub fn resolve(icon: Option<&str>, os: Option<&str>) -> Option<&'static Logo> {
    icon.and_then(by_id).or_else(|| os.and_then(for_os))
}

/// [`resolve`] for a host.
pub fn of_host(host: &Host) -> Option<&'static Logo> {
    resolve(host.icon.as_deref(), host.os.as_deref())
}

/// Up to two initials of a label ("web server" → "WS"), `?` without any.
pub fn initials(label: &str) -> String {
    let s: String = label
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase();
    if s.is_empty() { "?".into() } else { s }
}

impl Logo {
    /// Name shown in the picker.
    pub fn name(&self) -> SharedString {
        match self.kind {
            LogoKind::System => self.name.into(),
            LogoKind::Generic => {
                let key = format!("logo.{}", self.id);
                t!(key.as_str())
            }
        }
    }

    pub fn color(&self) -> Hsla {
        theme::hex(self.color)
    }

    /// The logo as a monochrome icon (it takes the text color).
    pub fn icon(&self) -> Icon {
        match self.glyph {
            Glyph::System => Icon::empty().path(format!("logos/{}.svg", self.id)),
            Glyph::Lucide(name) => Icon::new(name),
        }
    }
}

/// The square avatar of a host: its logo in white on the logo's color (or
/// the host's own color), or its initials.
pub fn avatar(host: &Host, size: f32, radius: Pixels) -> Div {
    let logo = of_host(host);
    let bg = host
        .color
        .as_deref()
        .and_then(theme::parse_color)
        .or(logo.map(Logo::color))
        .unwrap_or_else(|| theme::color_for(&host.label));
    tile(logo, &host.label, bg, size, radius)
}

/// An avatar for a logo (or the initials of `label`) on `bg`.
pub fn tile(logo: Option<&Logo>, label: &str, bg: Hsla, size: f32, radius: Pixels) -> Div {
    let base = div()
        .size(px(size))
        .flex_shrink_0()
        .rounded(radius)
        .bg(bg)
        .flex()
        .items_center()
        .justify_center()
        .text_color(gpui::white());
    match logo {
        Some(l) => base.child(l.icon().size(px((size * 0.55).round()))),
        None => base
            .font_semibold()
            .text_size(px((size * 0.36).round().max(9.)))
            .child(initials(label)),
    }
}

/// Small monochrome logo of a host for tabs, palette rows and pane
/// headers; `None` if it has none (they keep their usual icon).
pub fn small_icon(host: &Host) -> Option<Icon> {
    of_host(host).map(Logo::icon)
}

/// The app's assets: the Lucide icons plus the system logos.
pub struct Assets;

macro_rules! system_svgs {
    ($($id:literal),* $(,)?) => {
        &[$(($id, include_bytes!(concat!("../assets/logos/", $id, ".svg")))),*]
    };
}

static SYSTEM_SVGS: &[(&str, &[u8])] = system_svgs![
    "alma",
    "alpine",
    "arch",
    "centos",
    "debian",
    "elementary",
    "endeavouros",
    "fedora",
    "freebsd",
    "gentoo",
    "kali",
    "linux",
    "macos",
    "manjaro",
    "mint",
    "nixos",
    "opensuse",
    "popos",
    "raspberrypi",
    "rhel",
    "rocky",
    "suse",
    "ubuntu",
    "void",
    "windows",
    "zorin",
];

fn system_svg(path: &str) -> Option<&'static [u8]> {
    let id = path.strip_prefix("logos/")?.strip_suffix(".svg")?;
    SYSTEM_SVGS.iter().find(|(i, _)| *i == id).map(|(_, b)| *b)
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        match system_svg(path) {
            Some(bytes) => Ok(Some(Cow::Borrowed(bytes))),
            None => gpui_kit_assets::AllAssets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut list = gpui_kit_assets::AllAssets.list(path)?;
        list.extend(
            SYSTEM_SVGS
                .iter()
                .map(|(id, _)| format!("logos/{id}.svg"))
                .filter(|p| p.starts_with(path))
                .map(SharedString::from),
        );
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chosen_logo_then_system_then_initials() {
        // The chosen one wins over the detected system.
        assert_eq!(
            resolve(Some("database"), Some("ubuntu")).unwrap().id,
            "database"
        );
        assert_eq!(resolve(Some("Debian"), None).unwrap().id, "debian");
        // Automatic: the detected system, with its other ids.
        assert_eq!(resolve(None, Some("ubuntu")).unwrap().id, "ubuntu");
        assert_eq!(resolve(None, Some("almalinux")).unwrap().id, "alma");
        assert_eq!(resolve(None, Some("opensuse-leap")).unwrap().id, "opensuse");
        assert_eq!(resolve(None, Some("raspbian")).unwrap().id, "raspberrypi");
        assert_eq!(resolve(None, Some("darwin")).unwrap().id, "macos");
        // A logo of a later version falls back to the system.
        assert_eq!(
            resolve(Some("quantum"), Some("fedora")).unwrap().id,
            "fedora"
        );
        // Nothing known: the initials.
        assert_eq!(resolve(None, None), None);
        assert_eq!(resolve(None, Some("haiku")), None);
        assert_eq!(resolve(Some("nope"), Some("plan9")), None);
        // A system id that is a generic logo is not a detected system.
        assert_eq!(for_os("server"), None);
    }

    #[test]
    fn initials_of_labels() {
        assert_eq!(initials("web server"), "WS");
        assert_eq!(initials("db"), "D");
        assert_eq!(initials("  "), "?");
        assert_eq!(initials("ñandú rápido veloz"), "ÑR");
    }

    #[test]
    fn every_system_logo_has_its_svg() {
        let mut ids = std::collections::HashSet::new();
        for l in LOGOS {
            assert!(ids.insert(l.id), "repeated id {}", l.id);
            assert!(!l.id.contains(char::is_whitespace) && l.id.len() <= 64);
            match l.glyph {
                Glyph::System => {
                    let svg = system_svg(&format!("logos/{}.svg", l.id))
                        .unwrap_or_else(|| panic!("no svg for {}", l.id));
                    assert!(std::str::from_utf8(svg).unwrap().starts_with("<svg"));
                }
                Glyph::Lucide(_) => assert_eq!(l.kind, LogoKind::Generic),
            }
        }
        assert_eq!(
            SYSTEM_SVGS.len(),
            LOGOS.iter().filter(|l| l.kind == LogoKind::System).count()
        );
    }
}
