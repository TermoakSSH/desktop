//! Host picker of Ctrl/Cmd+T: a search over the saved hosts plus the local
//! terminal and the serial port. It also adds a terminal to a split view
//! and does quick connect: typing `user@host:port` (or
//! `telnet://host:port`) connects to that address (saving it as a host the
//! first time).

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::{ActiveTheme, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use termoak_core::Id;
use termoak_core::model::{Host, HostProtocol, HostSettings};

use super::OpenRequest;
use crate::app::AppView;
use crate::state::AppModel;
use crate::ui::{self, IconName};

/// What the host picker opens.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PickMode {
    /// A new tab (Ctrl/Cmd+T).
    Tab,
    /// A new pane of the current tab (split view).
    Pane,
    /// Quick connect: the search also takes `user@host:port`.
    Quick,
}

/// Address typed in the picker to connect without looking for a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickTarget {
    pub protocol: HostProtocol,
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

impl QuickTarget {
    /// `user@host:port` as typed back (IPv6 addresses in brackets when there
    /// is a port), with `telnet://` in front for Telnet.
    pub fn display(&self) -> String {
        let host = match self.port {
            Some(_) if self.host.contains(':') => format!("[{}]", self.host),
            _ => self.host.clone(),
        };
        format!(
            "{}{}{host}{}",
            if self.protocol.is_telnet() {
                "telnet://"
            } else {
                ""
            },
            self.user
                .as_deref()
                .map(|u| format!("{u}@"))
                .unwrap_or_default(),
            self.port.map(|p| format!(":{p}")).unwrap_or_default()
        )
    }
}

/// Reads `user@host:port`, `host:port`, `user@host`, `[v6]:port` or
/// `ssh user@host -p port`, and for Telnet `telnet://[user@]host[:port]`
/// or `telnet host [port]` (`ssh://` works too). Only text that looks like
/// an address (it has `@`, `:` or `.`, or a scheme) counts, so a plain
/// search word is not taken as a host.
pub fn parse_quick_target(text: &str) -> Option<QuickTarget> {
    let text = text.trim();
    if let Some(rest) = strip_prefix_ci(text, "telnet://") {
        let mut t = parse_address(rest.trim_end_matches('/'), true)?;
        t.protocol = HostProtocol::Telnet;
        return Some(t);
    }
    if let Some(rest) = strip_prefix_ci(text, "ssh://") {
        return parse_address(rest.trim_end_matches('/'), true);
    }
    if let Some(rest) = strip_prefix_ci(text, "telnet ") {
        // `telnet host [port]`, as on the command line.
        let mut words = rest.split_whitespace();
        let host = words.next()?;
        let port = match words.next() {
            Some(p) => Some(p.parse::<u16>().ok().filter(|p| *p > 0)?),
            None => None,
        };
        if words.next().is_some() {
            return None;
        }
        let mut t = parse_address(host, true)?;
        if port.is_some() {
            t.port = port;
        }
        t.protocol = HostProtocol::Telnet;
        return Some(t);
    }
    let text = text.strip_prefix("ssh ").map(str::trim).unwrap_or(text);
    let (text, flag_port) = match text.split_once(" -p ") {
        Some((t, p)) => (
            t.trim(),
            Some(p.trim().parse::<u16>().ok().filter(|p| *p > 0)?),
        ),
        None => (text, None),
    };
    if !text.contains(['@', ':', '.']) {
        return None;
    }
    let mut t = parse_address(text, false)?;
    if flag_port.is_some() {
        t.port = flag_port;
    }
    Some(t)
}

fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .filter(|p| p.eq_ignore_ascii_case(prefix))
        .map(|_| &text[prefix.len()..])
}

/// `[user@]host[:port]` (SSH; `any`: a bare name counts too).
fn parse_address(text: &str, any: bool) -> Option<QuickTarget> {
    if text.is_empty() || text.chars().any(char::is_whitespace) {
        return None;
    }
    if !any && !text.contains(['@', ':', '.']) {
        return None;
    }
    let (user, rest) = match text.rsplit_once('@') {
        Some((u, r)) if !u.is_empty() => (Some(u.to_string()), r),
        Some(_) => return None,
        None => (None, text),
    };
    let (host, port) = if let Some(v6) = rest.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p.parse::<u16>().ok().filter(|p| *p > 0)?),
            None if after.is_empty() => None,
            None => return None,
        };
        (host.to_string(), port)
    } else if rest.matches(':').count() == 1 {
        let (h, p) = rest.split_once(':')?;
        (
            h.to_string(),
            Some(p.parse::<u16>().ok().filter(|p| *p > 0)?),
        )
    } else {
        (rest.to_string(), None)
    };
    let valid = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%'));
    valid.then(|| QuickTarget {
        protocol: HostProtocol::Ssh,
        user,
        host,
        port,
    })
}

/// Host picker of Ctrl/Cmd+T (also for a new pane and quick connect).
pub struct HostPicker {
    model: Entity<AppModel>,
    app: gpui::WeakEntity<AppView>,
    mode: PickMode,
    search: Entity<InputState>,
    _sub: Subscription,
}

impl HostPicker {
    pub fn new(
        model: Entity<AppModel>,
        app: gpui::WeakEntity<AppView>,
        mode: PickMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(if mode == PickMode::Quick {
                t!("app.picker.search_quick")
            } else {
                t!("app.picker.search")
            })
        });
        ui::focus_later(&search, window, cx);
        let sub = cx.subscribe_in(
            &search,
            window,
            // Enter is handled by the dialog (see `open_host_picker`).
            |_, _, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    cx.notify();
                }
            },
        );
        Self {
            model,
            app,
            mode,
            search,
            _sub: sub,
        }
    }

    /// Whether the "Local terminal" entry is shown: always without a search,
    /// or when the search matches one of its names (in English and in the
    /// current language).
    fn shows_shell(&self, cx: &App) -> bool {
        let q = self.search.read(cx).value().trim().to_lowercase();
        if q.is_empty() {
            return true;
        }
        let terms = t!("app.picker.shell_search_terms").to_lowercase();
        terms
            .split(',')
            .chain(["local terminal", "local", "shell", "this computer"])
            .any(|name| name.trim().contains(q.as_str()))
    }

    /// Address typed in the search (quick connect), when no saved host
    /// matches it.
    fn quick_target(&self, cx: &App) -> Option<QuickTarget> {
        let target = parse_quick_target(&self.search.read(cx).value())?;
        self.matches(cx).is_empty().then_some(target)
    }

    /// Opens the first entry of the list (Enter).
    pub fn pick_first(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = self.quick_target(cx) {
            self.quick_connect(target, window, cx);
        } else if self.shows_shell(cx) {
            self.pick_shell(window, cx);
        } else if let Some(id) = self.matches(cx).first().map(|(id, _, _)| *id) {
            self.pick(id, false, window, cx);
        }
    }

    /// Opens the request as a tab or, for a new pane, in the current tab.
    fn send(&self, req: OpenRequest, window: &mut Window, cx: &mut App) {
        let pane = self.mode == PickMode::Pane;
        if let Some(app) = self.app.upgrade() {
            app.update(cx, |app, cx| {
                if pane {
                    app.open_as_pane(req, window, cx)
                } else {
                    app.open(req, window, cx)
                }
            });
        }
    }

    fn pick_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        self.send(OpenRequest::Shell, window, cx);
    }

    fn pick_serial(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        let app = self.app.clone();
        let pane = self.mode == PickMode::Pane;
        crate::views::serial::open(window, cx, move |params, window, cx| {
            if let Some(app) = app.upgrade() {
                app.update(cx, |app, cx| {
                    let req = OpenRequest::Serial(params);
                    if pane {
                        app.open_as_pane(req, window, cx)
                    } else {
                        app.open(req, window, cx)
                    }
                });
            }
        });
    }

    fn matches(&self, cx: &App) -> Vec<(Id, String, String)> {
        let q = self.search.read(cx).value().trim().to_lowercase();
        self.model
            .read(cx)
            .hosts
            .iter()
            .filter(|h| {
                q.is_empty()
                    || h.data.label.to_lowercase().contains(&q)
                    || h.data.address.to_lowercase().contains(&q)
                    || h.data.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .map(|h| (h.data.id, h.data.label.clone(), h.data.address.clone()))
            .collect()
    }

    fn pick(&mut self, host_id: Id, server: bool, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        let req = if server {
            OpenRequest::Server { host_id }
        } else {
            OpenRequest::Local { host_id }
        };
        self.send(req, window, cx);
    }

    /// Connects to a typed address: to the saved host with that address,
    /// user and port if there is one, otherwise it is saved as a new host
    /// first (so its password, fingerprint and history have a place).
    fn quick_connect(&mut self, target: QuickTarget, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        let existing = {
            let m = self.model.read(cx);
            m.hosts
                .iter()
                .find(|h| {
                    h.data.address.eq_ignore_ascii_case(&target.host)
                        && h.data.protocol == target.protocol
                        && target
                            .user
                            .as_ref()
                            .is_none_or(|u| m.effective_user(&h.data).as_ref() == Some(u))
                        && m.effective_port(&h.data)
                            == target.port.unwrap_or(target.protocol.default_port())
                })
                .map(|h| h.data.id)
        };
        if let Some(host_id) = existing {
            self.send(OpenRequest::Local { host_id }, window, cx);
            return;
        }
        let host = Host {
            id: Id::nil(),
            label: target.display(),
            address: target.host.clone(),
            group_id: None,
            tags: Vec::new(),
            settings: HostSettings {
                username: target.user.clone(),
                // Telnet hosts keep their port written out (see
                // `HostProtocol::switch_port`).
                port: target.port.or(target
                    .protocol
                    .is_telnet()
                    .then(|| target.protocol.default_port())),
                ..HostSettings::default()
            },
            notes: String::new(),
            color: None,
            os: None,
            os_version: None,
            favorite: false,
            protocol: target.protocol.clone(),
            icon: None,
        };
        let task = self
            .model
            .update(cx, |m, cx| m.save_host(host, None, None, None, None, cx));
        let app = self.app.clone();
        let pane = self.mode == PickMode::Pane;
        window
            .spawn(cx, async move |cx| {
                let res = task.await;
                let _ = cx.update(|window, cx| match res {
                    Ok(rec) => {
                        let req = OpenRequest::Local {
                            host_id: rec.data.id,
                        };
                        if let Some(app) = app.upgrade() {
                            app.update(cx, |app, cx| {
                                if pane {
                                    app.open_as_pane(req, window, cx)
                                } else {
                                    app.open(req, window, cx)
                                }
                            });
                        }
                    }
                    Err(e) => ui::error(window, cx, e),
                });
            })
            .detach();
    }
}

impl Render for HostPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let logged_in = self.model.read(cx).logged_in();
        let items = self.matches(cx);
        // Logo and protocol of each host.
        let extras: Vec<(Option<gpui_component::Icon>, bool)> = {
            let m = self.model.read(cx);
            items
                .iter()
                .map(|(id, _, _)| {
                    let h = m.host(*id);
                    (
                        h.and_then(crate::logos::small_icon),
                        h.is_some_and(|h| h.protocol.is_telnet()),
                    )
                })
                .collect()
        };
        let shows_shell = self.shows_shell(cx);
        let quick = self.quick_target(cx);
        let shell_shortcut = if cfg!(target_os = "macos") {
            SharedString::from("⌘⇧T")
        } else {
            t!("app.shortcut.new_local_terminal")
        };
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .child(Input::new(&self.search).prefix(ui::icon(IconName::Search).size(px(14.))))
            .child(
                v_flex()
                    .id("picker-list")
                    .max_h(px(360.))
                    .gap_1()
                    .overflow_y_scrollbar()
                    .when_some(quick, |this, target| {
                        let shown = target.display();
                        this.child(
                            h_flex()
                                .id("pick-quick")
                                .px_3()
                                .py_2()
                                .gap_3()
                                .items_center()
                                .rounded(theme.radius)
                                .cursor_pointer()
                                .bg(theme.secondary)
                                .hover(|s| s.bg(theme.secondary_hover))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.quick_connect(target.clone(), window, cx)
                                }))
                                .child(
                                    ui::icon(IconName::Zap)
                                        .size(px(16.))
                                        .text_color(theme.primary),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            div().text_sm().font_medium().child(t!(
                                                "app.picker.quick_connect",
                                                target = shown
                                            )),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(t!("app.picker.quick_connect_detail")),
                                        ),
                                ),
                        )
                    })
                    .when(shows_shell, |this| {
                        this.child(
                            h_flex()
                                .id("pick-shell")
                                .px_3()
                                .py_2()
                                .gap_3()
                                .items_center()
                                .rounded(theme.radius)
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.secondary_hover))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.pick_shell(window, cx)
                                }))
                                .child(
                                    ui::icon(IconName::Laptop)
                                        .size(px(16.))
                                        .text_color(theme.success),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_medium()
                                                .child(t!("app.picker.shell_title")),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(t!("app.picker.shell_detail")),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(shell_shortcut),
                                ),
                        )
                    })
                    .when(shows_shell, |this| {
                        this.child(
                            h_flex()
                                .id("pick-serial")
                                .px_3()
                                .py_2()
                                .gap_3()
                                .items_center()
                                .rounded(theme.radius)
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.secondary_hover))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.pick_serial(window, cx)
                                }))
                                .child(
                                    ui::icon(IconName::ArrowLeftRight)
                                        .size(px(16.))
                                        .text_color(theme.warning),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_medium()
                                                .child(t!("app.picker.serial_title")),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(t!("app.picker.serial_detail")),
                                        ),
                                ),
                        )
                    })
                    .when(items.is_empty() && !shows_shell, |this| {
                        this.child(
                            div()
                                .p_4()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("app.picker.no_matches")),
                        )
                    })
                    .children(items.into_iter().zip(extras).enumerate().map(
                        |(i, ((id, label, address), (logo, telnet)))| {
                            h_flex()
                                .id(("pick", i))
                                .px_3()
                                .py_2()
                                .gap_3()
                                .items_center()
                                .rounded(theme.radius)
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.secondary_hover))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.pick(id, false, window, cx)
                                }))
                                .child(
                                    logo.unwrap_or_else(|| ui::icon(IconName::Server))
                                        .size(px(16.))
                                        .text_color(theme.primary),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(div().text_sm().font_medium().child(label))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(address),
                                        ),
                                )
                                // The server has no Telnet sessions.
                                .when(logged_in && !telnet, |this| {
                                    this.child(
                                        Button::new(("pick-server", i))
                                            .xsmall()
                                            .ghost()
                                            .icon(ui::icon(IconName::Cloud))
                                            .label(t!("app.picker.on_server"))
                                            .on_click(cx.listener(
                                                move |this, _: &ClickEvent, window, cx| {
                                                    cx.stop_propagation();
                                                    this.pick(id, true, window, cx);
                                                },
                                            )),
                                    )
                                })
                        },
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(user: Option<&str>, host: &str, port: Option<u16>) -> Option<QuickTarget> {
        Some(QuickTarget {
            protocol: HostProtocol::Ssh,
            user: user.map(str::to_string),
            host: host.to_string(),
            port,
        })
    }

    fn telnet(user: Option<&str>, host: &str, port: Option<u16>) -> Option<QuickTarget> {
        t(user, host, port).map(|mut q| {
            q.protocol = HostProtocol::Telnet;
            q
        })
    }

    #[test]
    fn telnet_urls() {
        assert_eq!(
            parse_quick_target("telnet://10.0.0.1"),
            telnet(None, "10.0.0.1", None)
        );
        assert_eq!(
            parse_quick_target(" TELNET://admin@switch1:2323/ "),
            telnet(Some("admin"), "switch1", Some(2323))
        );
        // A bare name is fine after the scheme.
        assert_eq!(
            parse_quick_target("telnet://router"),
            telnet(None, "router", None)
        );
        assert_eq!(
            parse_quick_target("telnet://[2001:db8::1]:23"),
            telnet(None, "2001:db8::1", Some(23))
        );
        assert_eq!(
            parse_quick_target("telnet towel.blinkenlights.nl 23"),
            telnet(None, "towel.blinkenlights.nl", Some(23))
        );
        assert_eq!(parse_quick_target("telnet bbs"), telnet(None, "bbs", None));
        assert_eq!(parse_quick_target("ssh://web"), t(None, "web", None));
        assert_eq!(parse_quick_target("telnet://"), None);
        assert_eq!(parse_quick_target("telnet://h:0"), None);
        assert_eq!(parse_quick_target("telnet h 23 extra"), None);
        assert_eq!(
            telnet(Some("a"), "h", Some(2323)).unwrap().display(),
            "telnet://a@h:2323"
        );
    }

    #[test]
    fn quick_targets() {
        assert_eq!(
            parse_quick_target("root@10.0.0.5"),
            t(Some("root"), "10.0.0.5", None)
        );
        assert_eq!(
            parse_quick_target(" ana@web.example.com:2222 "),
            t(Some("ana"), "web.example.com", Some(2222))
        );
        assert_eq!(
            parse_quick_target("db.local:22"),
            t(None, "db.local", Some(22))
        );
        assert_eq!(
            parse_quick_target("ssh pi@raspberrypi.lan -p 2200"),
            t(Some("pi"), "raspberrypi.lan", Some(2200))
        );
        assert_eq!(
            parse_quick_target("[2001:db8::1]:2222"),
            t(None, "2001:db8::1", Some(2222))
        );
        assert_eq!(
            parse_quick_target("me@2001:db8::1"),
            t(Some("me"), "2001:db8::1", None)
        );
        // A search word is not an address.
        assert_eq!(parse_quick_target("web"), None);
        assert_eq!(parse_quick_target(""), None);
        assert_eq!(parse_quick_target("@host.com"), None);
        assert_eq!(parse_quick_target("host.com:0"), None);
        assert_eq!(parse_quick_target("host.com:99999"), None);
        assert_eq!(parse_quick_target("two words.com"), None);
    }

    #[test]
    fn shown_back() {
        assert_eq!(
            t(Some("ana"), "h.io", Some(22)).unwrap().display(),
            "ana@h.io:22"
        );
        assert_eq!(t(None, "::1", Some(2222)).unwrap().display(), "[::1]:2222");
        assert_eq!(t(None, "::1", None).unwrap().display(), "::1");
    }
}
