//! App themes (dark by default, in the style of Termius, and light) and
//! terminal palettes.

use std::rc::Rc;

use gpui::{App, Hsla, Rgba, Window, rgb};
use gpui_component::theme::{Theme, ThemeMode, ThemeSet};

/// Own themes in the `gpui-component` format. Whatever is not set uses the
/// defaults of the mode.
const THEMES: &str = r##"{
  "name": "Termoak",
  "author": "Termoak",
  "themes": [
    {
      "name": "Termoak Dark",
      "mode": "dark",
      "font.size": 14,
      "radius": 8,
      "radius.lg": 12,
      "colors": {
        "background": "#151924",
        "foreground": "#e6e9ef",
        "border": "#262b3a",
        "accent.background": "#232939",
        "accent.foreground": "#e6e9ef",
        "muted.background": "#1e2330",
        "muted.foreground": "#8a93a6",
        "secondary.background": "#1e2330",
        "secondary.hover.background": "#262c3b",
        "secondary.active.background": "#2d3446",
        "secondary.foreground": "#e6e9ef",
        "primary.background": "#4f7cff",
        "primary.hover.background": "#6a90ff",
        "primary.active.background": "#3f68e0",
        "primary.foreground": "#ffffff",
        "button.primary.background": "#4f7cff",
        "button.primary.hover.background": "#6a90ff",
        "button.primary.active.background": "#3f68e0",
        "button.primary.foreground": "#ffffff",
        "sidebar.background": "#10131b",
        "sidebar.foreground": "#c9cfdb",
        "sidebar.border": "#1e2330",
        "sidebar.accent.background": "#1f2433",
        "sidebar.accent.foreground": "#ffffff",
        "sidebar.primary.background": "#4f7cff",
        "sidebar.primary.foreground": "#ffffff",
        "popover.background": "#1a1e2a",
        "popover.foreground": "#e6e9ef",
        "input.border": "#2a3042",
        "list.background": "#151924",
        "list.active.background": "#4f7cff33",
        "list.active.border": "#4f7cff",
        "list.hover.background": "#1f2433",
        "tab_bar.background": "#10131b",
        "tab.background": "#00000000",
        "tab.active.background": "#151924",
        "tab.active.foreground": "#ffffff",
        "tab.foreground": "#9aa3b5",
        "title_bar.background": "#10131b",
        "title_bar.border": "#1e2330",
        "ring": "#4f7cff",
        "caret": "#e6e9ef",
        "selection.background": "#4f7cff66",
        "scrollbar.thumb.background": "#3a4155e6",
        "scrollbar.thumb.hover.background": "#4a5268",
        "switch.background": "#3a4155",
        "danger.background": "#e5484d",
        "danger.foreground": "#ffffff",
        "success.background": "#30a46c",
        "success.foreground": "#ffffff",
        "warning.background": "#f5a524",
        "warning.foreground": "#1a1a1a",
        "info.background": "#0ea5e9",
        "info.foreground": "#ffffff",
        "link": "#7ea2ff",
        "link.hover": "#a3bdff",
        "link.active": "#7ea2ff"
      }
    },
    {
      "name": "Termoak Light",
      "mode": "light",
      "font.size": 14,
      "radius": 8,
      "radius.lg": 12,
      "colors": {
        "background": "#ffffff",
        "foreground": "#1b1f2a",
        "border": "#e3e6ee",
        "muted.background": "#f2f4f8",
        "muted.foreground": "#667085",
        "secondary.background": "#f2f4f8",
        "secondary.hover.background": "#e9ecf3",
        "secondary.active.background": "#dfe3ec",
        "primary.background": "#3867f5",
        "primary.hover.background": "#4f7cff",
        "primary.active.background": "#2c55d6",
        "primary.foreground": "#ffffff",
        "button.primary.background": "#3867f5",
        "button.primary.hover.background": "#4f7cff",
        "button.primary.active.background": "#2c55d6",
        "button.primary.foreground": "#ffffff",
        "sidebar.background": "#f5f6fa",
        "sidebar.foreground": "#303646",
        "sidebar.border": "#e3e6ee",
        "sidebar.accent.background": "#e7ebf5",
        "sidebar.accent.foreground": "#101828",
        "sidebar.primary.background": "#3867f5",
        "sidebar.primary.foreground": "#ffffff",
        "list.active.background": "#3867f526",
        "list.active.border": "#3867f5",
        "tab_bar.background": "#eef0f5",
        "tab.active.background": "#ffffff",
        "tab.active.foreground": "#101828",
        "tab.foreground": "#475467",
        "title_bar.background": "#eef0f5",
        "title_bar.border": "#e3e6ee",
        "ring": "#3867f5",
        "selection.background": "#3867f544",
        "danger.background": "#e5484d",
        "danger.foreground": "#ffffff",
        "success.background": "#30a46c",
        "success.foreground": "#ffffff",
        "link": "#2c55d6"
      }
    }
  ]
}"##;

/// Installs the own themes and applies the given mode.
pub fn init(cx: &mut App, dark: bool) {
    // `Theme::change` creates the global theme if it does not exist yet.
    Theme::change(ThemeMode::Dark, None, cx);
    match serde_json::from_str::<ThemeSet>(THEMES) {
        Ok(set) => {
            for config in set.themes {
                let config = Rc::new(config);
                let theme = Theme::global_mut(cx);
                if config.mode.is_dark() {
                    theme.dark_theme = config;
                } else {
                    theme.light_theme = config;
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not load the own themes"),
    }
    apply(dark, None, cx);
}

/// Switches between dark and light.
pub fn apply(dark: bool, window: Option<&mut Window>, cx: &mut App) {
    let mode = if dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    Theme::change(mode, window, cx);
}

/// Color from `0xRRGGBB`.
pub fn hex(v: u32) -> Hsla {
    rgb(v).into()
}

/// Color with transparency from `0xRRGGBB` and alpha (0..1).
pub fn hexa(v: u32, alpha: f32) -> Hsla {
    let mut c: Hsla = rgb(v).into();
    c.a = alpha;
    c
}

/// Color from RGB components (0..255).
pub fn rgb8(r: u8, g: u8, b: u8) -> Hsla {
    Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

/// Palette of a terminal.
#[derive(Clone, Copy, Debug)]
pub struct TermPalette {
    pub foreground: Hsla,
    pub background: Hsla,
    pub cursor: Hsla,
    pub selection: Hsla,
    /// The 16 ANSI colors (normal and bright).
    pub ansi: [Hsla; 16],
}

impl TermPalette {
    pub fn for_mode(dark: bool) -> Self {
        if dark { Self::dark() } else { Self::light() }
    }

    pub fn dark() -> Self {
        Self {
            foreground: hex(0xd6dbe4),
            background: hex(0x12151d),
            cursor: hex(0x6a90ff),
            selection: hexa(0x4f7cff, 0.38),
            ansi: [
                hex(0x1c2029),
                hex(0xf07178),
                hex(0x8bd49c),
                hex(0xffcb6b),
                hex(0x5b9dff),
                hex(0xc792ea),
                hex(0x5fd7d7),
                hex(0xc8ccd4),
                hex(0x5c6370),
                hex(0xff8a8a),
                hex(0xa8e6a3),
                hex(0xffe08a),
                hex(0x82b1ff),
                hex(0xe0a8ff),
                hex(0x89ddff),
                hex(0xffffff),
            ],
        }
    }

    pub fn light() -> Self {
        Self {
            foreground: hex(0x1f2328),
            background: hex(0xfbfbfd),
            cursor: hex(0x2f6feb),
            selection: hexa(0x2f6feb, 0.25),
            ansi: [
                hex(0x1f2328),
                hex(0xcf222e),
                hex(0x1a7f37),
                hex(0x9a6700),
                hex(0x0969da),
                hex(0x8250df),
                hex(0x1b7c83),
                hex(0x6e7781),
                hex(0x57606a),
                hex(0xa40e26),
                hex(0x2da44e),
                hex(0xbf8700),
                hex(0x218bff),
                hex(0xa475f9),
                hex(0x3192aa),
                hex(0x8c959f),
            ],
        }
    }

    /// Color of the xterm 256-color palette.
    pub fn indexed(&self, idx: u8) -> Hsla {
        match idx {
            0..=15 => self.ansi[idx as usize],
            16..=231 => {
                let i = idx - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                rgb8(level(i / 36), level((i / 6) % 6), level(i % 6))
            }
            _ => {
                let v = 8 + (idx - 232) * 10;
                rgb8(v, v, v)
            }
        }
    }
}

/// Label and color for the detected operating system of a host (the `ID`
/// of `/etc/os-release`, or `macos`, `windows`...).
pub fn os_badge(os: &str) -> (&'static str, Hsla) {
    let os = os.trim().to_ascii_lowercase();
    match os.as_str() {
        "ubuntu" => ("Ubuntu", hex(0xe95420)),
        "debian" => ("Debian", hex(0xd70a53)),
        "raspbian" => ("Raspbian", hex(0xc51a4a)),
        "linuxmint" | "mint" => ("Mint", hex(0x87cf3e)),
        "pop" => ("Pop!_OS", hex(0x48b9c7)),
        "kali" => ("Kali", hex(0x367bf0)),
        "fedora" => ("Fedora", hex(0x3c6eb4)),
        "centos" => ("CentOS", hex(0x9c27b0)),
        "rhel" | "redhat" => ("RHEL", hex(0xee0000)),
        "rocky" => ("Rocky", hex(0x10b981)),
        "alma" | "almalinux" => ("Alma", hex(0x0f4266)),
        "ol" | "oracle" => ("Oracle", hex(0xc74634)),
        "amzn" | "amazon" => ("Amazon", hex(0xff9900)),
        "arch" | "archarm" => ("Arch", hex(0x1793d1)),
        "manjaro" => ("Manjaro", hex(0x35bf5c)),
        "alpine" => ("Alpine", hex(0x0d597f)),
        "opensuse" | "opensuse-leap" | "opensuse-tumbleweed" | "suse" | "sles" | "sled" => {
            ("SUSE", hex(0x73ba25))
        }
        "gentoo" => ("Gentoo", hex(0x54487a)),
        "nixos" => ("NixOS", hex(0x5277c3)),
        "freebsd" => ("FreeBSD", hex(0xab2b28)),
        "openbsd" => ("OpenBSD", hex(0xf2ca30)),
        "netbsd" => ("NetBSD", hex(0xf26711)),
        "macos" | "darwin" | "osx" => ("macOS", hex(0x8e8e93)),
        "windows" => ("Windows", hex(0x0078d4)),
        "linux" => ("Linux", hex(0xf5a524)),
        _ => ("SSH", hex(0x4f7cff)),
    }
}

/// Stable color for a text (groups and host avatars without their own color).
pub fn color_for(text: &str) -> Hsla {
    const COLORS: [u32; 8] = [
        0x4f7cff, 0x30a46c, 0xf5a524, 0xe5484d, 0x8e4ec6, 0x0ea5e9, 0xd6409f, 0x12a594,
    ];
    let hash = text
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    hex(COLORS[(hash as usize) % COLORS.len()])
}

/// Parses a `#rrggbb` color saved in a host or group.
pub fn parse_color(s: &str) -> Option<Hsla> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    u32::from_str_radix(s, 16).ok().map(hex)
}

#[cfg(test)]
mod tests {
    use super::os_badge;

    #[test]
    fn os_ids_have_their_own_badge() {
        for (id, label) in [
            ("ubuntu", "Ubuntu"),
            ("rocky", "Rocky"),
            ("almalinux", "Alma"),
            ("macos", "macOS"),
            ("windows", "Windows"),
            ("freebsd", "FreeBSD"),
            ("opensuse-leap", "SUSE"),
            ("amzn", "Amazon"),
            (" Debian ", "Debian"),
        ] {
            assert_eq!(os_badge(id).0, label, "{id}");
        }
        assert_eq!(os_badge("haiku").0, "SSH");
    }
}
