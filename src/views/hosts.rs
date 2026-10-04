//! Hosts: cards arranged by group, search, favorites, tags and detected
//! system (with its version). A click opens the editor; a double click
//! connects. `~/.ssh/config` is also imported from here.

use std::time::Duration;

use gpui::{
    AppContext, ClickEvent, Context, Entity, EventEmitter, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Task,
    Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme, Selectable, Sizable, StyledExt, h_flex, v_flex};
use termoak_core::Id;
use termoak_core::model::{Group, Host, HostSettings, Record, SecretUpdate};

use super::OpenRequest;
use super::host_editor::{EditorEvent, HostEditor};
use crate::state::AppModel;
use crate::theme;
use crate::ui::{self, IconName};

/// Margin to tell a click from a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(280);

/// System version without the name the label already shows
/// ("Ubuntu 24.04.1 LTS" next to "Ubuntu" → "24.04.1 LTS").
pub fn os_detail(label: &str, version: &str) -> String {
    let version = version.trim();
    match version.get(..label.len()) {
        Some(head) if head.eq_ignore_ascii_case(label) => {
            let rest = version[label.len()..].trim();
            if rest.is_empty() {
                version.to_string()
            } else {
                rest.to_string()
            }
        }
        _ => version.to_string(),
    }
}

/// Active group filter.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GroupFilter {
    All,
    Favorites,
    Ungrouped,
    Group(Id),
}

pub struct HostsView {
    model: Entity<AppModel>,
    search: Entity<InputState>,
    filter: GroupFilter,
    editor: Option<Entity<HostEditor>>,
    _subs: Vec<Subscription>,
    _editor_sub: Option<Subscription>,
    /// Pending single click: the editor opens if no double click arrives
    /// (opening it at once moves the cards and the second click is lost).
    pending_click: Option<(Id, Task<()>)>,
}

impl EventEmitter<OpenRequest> for HostsView {}

impl HostsView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("hosts.search_placeholder")));
        let subs = vec![
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.subscribe(&search, |_, _, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        Self {
            model,
            search,
            filter: GroupFilter::All,
            editor: None,
            _subs: subs,
            _editor_sub: None,
            pending_click: None,
        }
    }

    /// Opens the editor of a host (or an empty one to create it).
    pub fn edit(
        &mut self,
        host: Option<Record<Host>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = self.model.clone();
        let group = match self.filter {
            GroupFilter::Group(g) => Some(g),
            _ => None,
        };
        let editor = cx.new(|cx| HostEditor::new(model, host, group, window, cx));
        self._editor_sub = Some(cx.subscribe_in(
            &editor,
            window,
            |this, _, ev: &EditorEvent, _, cx| match ev {
                EditorEvent::Close => {
                    this.editor = None;
                    cx.notify();
                }
                EditorEvent::Open(req) => cx.emit(req.clone()),
            },
        ));
        self.editor = Some(editor);
        cx.notify();
    }

    fn connect(&mut self, host_id: Id, server: bool, cx: &mut Context<Self>) {
        if server {
            cx.emit(OpenRequest::Server { host_id });
        } else {
            cx.emit(OpenRequest::Local { host_id });
        }
    }

    fn toggle_favorite(&mut self, rec: &Record<Host>, window: &mut Window, cx: &mut Context<Self>) {
        let mut host = rec.data.clone();
        host.favorite = !host.favorite;
        let task = self
            .model
            .update(cx, |m, cx| m.save(host, SecretUpdate::Keep, None, cx));
        cx.spawn_in(window, async move |_, cx| {
            if let Err(e) = task.await {
                let _ = cx.update(|window, cx| ui::error(window, cx, e));
            }
        })
        .detach();
    }

    fn delete_host(&mut self, rec: &Record<Host>, window: &mut Window, cx: &mut Context<Self>) {
        let id = rec.data.id;
        let label = rec.data.label.clone();
        let model = self.model.clone();
        ui::confirm(
            window,
            cx,
            t!("hosts.delete.title"),
            t!("hosts.delete.message", name = label),
            t!("hosts.delete.ok"),
            true,
            move |window, cx| {
                let task = model.update(cx, |m, cx| m.delete::<Host>(id, cx));
                let label = label.clone();
                window
                    .spawn(cx, async move |cx| {
                        let res = task.await;
                        let _ = cx.update(|window, cx| match res {
                            Ok(()) => {
                                ui::success(window, cx, t!("hosts.delete.done", name = label))
                            }
                            Err(e) => ui::error(window, cx, e),
                        });
                    })
                    .detach();
            },
        );
    }

    /// Dialog to create or rename a group (with an inheritable user and port).
    pub fn edit_group(
        &mut self,
        group: Option<Group>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("hosts.group.name_placeholder"))
                .default_value(group.as_ref().map(|g| g.name.clone()).unwrap_or_default())
        });
        let user = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("hosts.group.no_default"))
                .default_value(
                    group
                        .as_ref()
                        .and_then(|g| g.settings.username.clone())
                        .unwrap_or_default(),
                )
        });
        let port = cx.new(|cx| {
            InputState::new(window, cx).placeholder("22").default_value(
                group
                    .as_ref()
                    .and_then(|g| g.settings.port)
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
            )
        });
        ui::focus_later(&name, window, cx);
        let model = self.model.clone();
        let editing = group.is_some();
        let (n2, u2, p2) = (name.clone(), user.clone(), port.clone());
        ui::open_form_dialog(
            window,
            cx,
            if editing {
                t!("hosts.group.edit_title")
            } else {
                t!("hosts.group.new_title")
            },
            t!("common.save"),
            440.,
            move |_, cx| {
                v_flex()
                    .gap_3()
                    .child(ui::field(t!("common.name"), Input::new(&name), cx))
                    .child(ui::field_with_hint(
                        t!("hosts.group.default_user"),
                        Input::new(&user),
                        t!("hosts.group.default_user_hint"),
                        cx,
                    ))
                    .child(ui::field(
                        t!("hosts.group.default_port"),
                        Input::new(&port),
                        cx,
                    ))
                    .into_any_element()
            },
            move |window, cx| {
                let name = n2.read(cx).value().trim().to_string();
                if name.is_empty() {
                    ui::error(window, cx, t!("hosts.group.error.name"));
                    return false;
                }
                let port_text = p2.read(cx).value().trim().to_string();
                let port = if port_text.is_empty() {
                    None
                } else {
                    match port_text.parse::<u16>() {
                        Ok(p) if p > 0 => Some(p),
                        _ => {
                            ui::error(window, cx, t!("hosts.group.error.port"));
                            return false;
                        }
                    }
                };
                let username =
                    Some(u2.read(cx).value().trim().to_string()).filter(|u| !u.is_empty());
                let mut g = group.clone().unwrap_or(Group {
                    id: Id::nil(),
                    name: String::new(),
                    parent_id: None,
                    color: None,
                    settings: HostSettings::default(),
                });
                g.name = name;
                g.settings.username = username;
                g.settings.port = port;
                let task = model.update(cx, |m, cx| m.save(g, SecretUpdate::Keep, None, cx));
                window
                    .spawn(cx, async move |cx| {
                        if let Err(e) = task.await {
                            let _ = cx.update(|window, cx| ui::error(window, cx, e));
                        }
                    })
                    .detach();
                true
            },
        );
    }

    fn delete_group(&mut self, group: Group, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        ui::confirm(
            window,
            cx,
            t!("hosts.group.delete.title"),
            t!("hosts.group.delete.message", name = group.name),
            t!("hosts.delete.ok"),
            true,
            move |window, cx| {
                let id = group.id;
                let task = model.update(cx, |m, cx| m.delete::<Group>(id, cx));
                window
                    .spawn(cx, async move |cx| {
                        if let Err(e) = task.await {
                            let _ = cx.update(|window, cx| ui::error(window, cx, e));
                        }
                    })
                    .detach();
            },
        );
        if self.filter == GroupFilter::Group(group.id) {
            self.filter = GroupFilter::All;
        }
    }

    fn matches(query: &str, rec: &Record<Host>, group_name: Option<&str>) -> bool {
        if query.is_empty() {
            return true;
        }
        let h = &rec.data;
        let hay = format!(
            "{} {} {} {} {} {}",
            h.label,
            h.address,
            h.settings.username.clone().unwrap_or_default(),
            h.tags.join(" "),
            h.os.clone().unwrap_or_default(),
            group_name.unwrap_or("")
        )
        .to_lowercase();
        query
            .split_whitespace()
            .all(|word| hay.contains(&word.to_lowercase()))
    }

    fn render_chip(
        &self,
        id: &'static str,
        ix: usize,
        label: SharedString,
        count: usize,
        filter: GroupFilter,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let active = self.filter == filter;
        Button::new((id, ix))
            .small()
            .label(format!("{label} · {count}"))
            .selected(active)
            .map(|b| if active { b.primary() } else { b.ghost() })
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.filter = filter;
                cx.notify();
            }))
    }

    fn render_card(
        &self,
        rec: &Record<Host>,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let host = &rec.data;
        // Effective user: the host's, its group's or its identity's.
        let user = {
            let m = self.model.read(cx);
            host.settings
                .username
                .clone()
                .or_else(|| {
                    host.group_id
                        .and_then(|g| m.groups.iter().find(|x| x.data.id == g))
                        .and_then(|g| g.data.settings.username.clone())
                })
                .or_else(|| {
                    host.settings
                        .identity_id
                        .and_then(|i| m.identities.iter().find(|x| x.data.id == i))
                        .map(|i| i.data.username.clone())
                })
        };
        let theme = cx.theme();
        let selected = self.editor.as_ref().and_then(|e| e.read(cx).host_id()) == Some(host.id);
        let (os_label, os_color) = host
            .os
            .as_deref()
            .map(theme::os_badge)
            .unwrap_or(("SSH", theme::color_for(&host.label)));
        // Version detected when connecting ("Ubuntu 24.04.1 LTS").
        let os_version = host
            .os
            .as_ref()
            .and(host.os_version.clone())
            .filter(|v| !v.trim().is_empty());
        let avatar_color = host
            .color
            .as_deref()
            .and_then(theme::parse_color)
            .unwrap_or(os_color);
        let port = host.settings.port;
        let subtitle = format!(
            "{}{}{}",
            user.map(|u| format!("{u}@")).unwrap_or_default(),
            host.address,
            port.filter(|p| *p != 22)
                .map(|p| format!(":{p}"))
                .unwrap_or_default()
        );
        let logged_in = self.model.read(cx).logged_in();
        let id = host.id;
        let rec_fav = rec.clone();
        let rec_edit = rec.clone();
        let rec_del = rec.clone();
        let weak = cx.entity().downgrade();
        let initials: String = host
            .label
            .split_whitespace()
            .filter_map(|w| w.chars().next())
            .take(2)
            .collect::<String>()
            .to_uppercase();

        v_flex()
            .id(("host-card", ix))
            .w(px(290.))
            .p_3()
            .gap_2()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(if selected {
                theme.primary
            } else {
                theme.border
            })
            .bg(theme.secondary)
            .hover(|s| s.bg(theme.secondary_hover))
            .cursor_pointer()
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                if ev.click_count() >= 2 {
                    this.pending_click = None;
                    this.connect(id, false, cx);
                } else {
                    let task = cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor().timer(DOUBLE_CLICK).await;
                        let _ = this.update_in(cx, |this, window, cx| {
                            if this.pending_click.as_ref().is_some_and(|(p, _)| *p == id) {
                                this.pending_click = None;
                                let rec = this.model.read(cx).host_record(id).cloned();
                                this.edit(rec, window, cx);
                            }
                        });
                    });
                    this.pending_click = Some((id, task));
                }
            }))
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .size(px(40.))
                            .flex_shrink_0()
                            .rounded(theme.radius)
                            .bg(avatar_color)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(gpui::white())
                            .font_semibold()
                            .text_sm()
                            .child(if initials.is_empty() {
                                "?".to_string()
                            } else {
                                initials
                            }),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .child(
                                        div()
                                            .font_semibold()
                                            .text_sm()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .child(host.label.clone()),
                                    )
                                    .when(host.favorite, |this| {
                                        this.child(
                                            ui::icon(IconName::StarFill)
                                                .size(px(12.))
                                                .text_color(theme.warning),
                                        )
                                    })
                                    .when(
                                        rec.meta.sync_mode
                                            == termoak_core::model::SyncMode::DeviceOnly,
                                        |this| {
                                            this.child(
                                                ui::icon(IconName::Lock)
                                                    .size(px(12.))
                                                    .text_color(theme.muted_foreground),
                                            )
                                        },
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(subtitle),
                            ),
                    )
                    .child(
                        Button::new(("card-menu", ix))
                            .xsmall()
                            .ghost()
                            .icon(ui::icon(IconName::EllipsisVertical))
                            .dropdown_menu(move |menu, _, _| {
                                let w = weak.clone();
                                let (w1, w2, w3, w4, w5, w6) = (
                                    w.clone(),
                                    w.clone(),
                                    w.clone(),
                                    w.clone(),
                                    w.clone(),
                                    w.clone(),
                                );
                                let (rf, re, rd) =
                                    (rec_fav.clone(), rec_edit.clone(), rec_del.clone());
                                let menu = menu
                                    .item(
                                        PopupMenuItem::new(t!("hosts.menu.connect"))
                                            .icon(ui::icon(IconName::SquareTerminal))
                                            .on_click(move |_, _, cx| {
                                                if let Some(v) = w1.upgrade() {
                                                    v.update(cx, |v, cx| v.connect(id, false, cx));
                                                }
                                            }),
                                    )
                                    .when(logged_in, |menu| {
                                        menu.item(
                                            PopupMenuItem::new(t!("hosts.menu.connect_server"))
                                                .icon(ui::icon(IconName::Cloud))
                                                .on_click(move |_, _, cx| {
                                                    if let Some(v) = w2.upgrade() {
                                                        v.update(cx, |v, cx| {
                                                            v.connect(id, true, cx)
                                                        });
                                                    }
                                                }),
                                        )
                                    })
                                    .item(
                                        PopupMenuItem::new(t!("hosts.menu.open_sftp"))
                                            .icon(ui::icon(IconName::FolderOpen))
                                            .on_click(move |_, _, cx| {
                                                if let Some(v) = w3.upgrade() {
                                                    v.update(cx, |_, cx| {
                                                        cx.emit(OpenRequest::Sftp {
                                                            host_id: id,
                                                            conn: None,
                                                        })
                                                    });
                                                }
                                            }),
                                    )
                                    .separator()
                                    .item(
                                        PopupMenuItem::new(t!("hosts.menu.edit"))
                                            .icon(ui::icon(IconName::Pencil))
                                            .on_click(move |_, window, cx| {
                                                if let Some(v) = w4.upgrade() {
                                                    let re = re.clone();
                                                    v.update(cx, |v, cx| {
                                                        v.edit(Some(re), window, cx)
                                                    });
                                                }
                                            }),
                                    )
                                    .item(
                                        PopupMenuItem::new(if rf.data.favorite {
                                            t!("hosts.menu.unfavorite")
                                        } else {
                                            t!("hosts.menu.favorite")
                                        })
                                        .icon(ui::icon(IconName::Star))
                                        .on_click(
                                            move |_, window, cx| {
                                                if let Some(v) = w5.upgrade() {
                                                    let rf = rf.clone();
                                                    v.update(cx, |v, cx| {
                                                        v.toggle_favorite(&rf, window, cx)
                                                    });
                                                }
                                            },
                                        ),
                                    )
                                    .separator()
                                    .item(
                                        PopupMenuItem::new(t!("hosts.menu.delete"))
                                            .icon(ui::icon(IconName::Trash))
                                            .on_click(move |_, window, cx| {
                                                if let Some(v) = w6.upgrade() {
                                                    let rd = rd.clone();
                                                    v.update(cx, |v, cx| {
                                                        v.delete_host(&rd, window, cx)
                                                    });
                                                }
                                            }),
                                    );
                                menu
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .items_center()
                    .map(|this| match os_version {
                        Some(version) => {
                            let detail = os_detail(os_label, &version);
                            this.child(
                                h_flex()
                                    .id(("host-os", ix))
                                    .gap_1()
                                    .items_center()
                                    .max_w(px(200.))
                                    .child(ui::pill(os_label, os_color))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .child(detail),
                                    )
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(t!("hosts.os_tooltip", version = version))
                                            .build(window, cx)
                                    }),
                            )
                        }
                        None => this.child(ui::pill(os_label, os_color)),
                    })
                    .children(
                        host.tags
                            .iter()
                            .take(4)
                            .map(|t| ui::pill(t.clone(), theme.muted_foreground)),
                    ),
            )
    }

    fn render_group_header(
        &self,
        group: Option<&Group>,
        count: usize,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let name = group
            .map(|g| g.name.clone())
            .unwrap_or_else(|| t!("hosts.ungrouped").to_string());
        let color = group
            .and_then(|g| g.color.as_deref().and_then(theme::parse_color))
            .unwrap_or_else(|| theme::color_for(&name));
        let weak = cx.entity().downgrade();
        let group_owned = group.cloned();
        h_flex()
            .gap_2()
            .items_center()
            .child(ui::icon(IconName::Folder).size(px(16.)).text_color(color))
            .child(div().font_semibold().child(name))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(tn!("hosts.count", count)),
            )
            .when_some(group_owned, |this, g| {
                this.child(
                    Button::new(("group-menu", ix))
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::Ellipsis))
                        .dropdown_menu(move |menu, _, _| {
                            let (w1, w2) = (weak.clone(), weak.clone());
                            let (g1, g2) = (g.clone(), g.clone());
                            menu.item(
                                PopupMenuItem::new(t!("hosts.group.menu.edit"))
                                    .icon(ui::icon(IconName::Pencil))
                                    .on_click(move |_, window, cx| {
                                        if let Some(v) = w1.upgrade() {
                                            let g = g1.clone();
                                            v.update(cx, |v, cx| v.edit_group(Some(g), window, cx));
                                        }
                                    }),
                            )
                            .item(
                                PopupMenuItem::new(t!("hosts.group.menu.delete"))
                                    .icon(ui::icon(IconName::Trash))
                                    .on_click(move |_, window, cx| {
                                        if let Some(v) = w2.upgrade() {
                                            let g = g2.clone();
                                            v.update(cx, |v, cx| v.delete_group(g, window, cx));
                                        }
                                    }),
                            )
                        }),
                )
            })
    }
}

impl Render for HostsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let query = self.search.read(cx).value().trim().to_string();
        let groups: Vec<Group> = model.groups.iter().map(|g| g.data.clone()).collect();
        let hosts: Vec<Record<Host>> = model.hosts.clone();
        let loaded = model.loaded;
        let group_exists = |id: Option<Id>| id.is_some_and(|g| groups.iter().any(|x| x.id == g));

        // Search filter.
        let visible: Vec<&Record<Host>> = hosts
            .iter()
            .filter(|h| {
                let gname = h
                    .data
                    .group_id
                    .and_then(|g| groups.iter().find(|x| x.id == g))
                    .map(|g| g.name.as_str());
                Self::matches(&query, h, gname)
            })
            .collect();
        let fav_count = visible.iter().filter(|h| h.data.favorite).count();
        let ungrouped_count = visible
            .iter()
            .filter(|h| !group_exists(h.data.group_id))
            .count();

        // Group chips.
        let mut chips: Vec<gpui::AnyElement> = vec![
            self.render_chip(
                "chip-all",
                0,
                t!("hosts.filter.all"),
                visible.len(),
                GroupFilter::All,
                cx,
            )
            .into_any_element(),
            self.render_chip(
                "chip-fav",
                0,
                t!("hosts.filter.favorites"),
                fav_count,
                GroupFilter::Favorites,
                cx,
            )
            .into_any_element(),
        ];
        for (i, g) in groups.iter().enumerate() {
            let count = visible
                .iter()
                .filter(|h| h.data.group_id == Some(g.id))
                .count();
            chips.push(
                self.render_chip(
                    "chip-group",
                    i,
                    g.name.clone().into(),
                    count,
                    GroupFilter::Group(g.id),
                    cx,
                )
                .into_any_element(),
            );
        }
        if ungrouped_count > 0 && !groups.is_empty() {
            chips.push(
                self.render_chip(
                    "chip-none",
                    0,
                    t!("hosts.ungrouped"),
                    ungrouped_count,
                    GroupFilter::Ungrouped,
                    cx,
                )
                .into_any_element(),
            );
        }

        // Sections by group according to the filter.
        let mut sections: Vec<(Option<Group>, Vec<&Record<Host>>)> = Vec::new();
        match self.filter {
            GroupFilter::Favorites => {
                sections.push((
                    None,
                    visible
                        .iter()
                        .copied()
                        .filter(|h| h.data.favorite)
                        .collect(),
                ));
            }
            GroupFilter::Group(gid) => {
                let g = groups.iter().find(|g| g.id == gid).cloned();
                sections.push((
                    g,
                    visible
                        .iter()
                        .copied()
                        .filter(|h| h.data.group_id == Some(gid))
                        .collect(),
                ));
            }
            GroupFilter::Ungrouped => {
                sections.push((
                    None,
                    visible
                        .iter()
                        .copied()
                        .filter(|h| !group_exists(h.data.group_id))
                        .collect(),
                ));
            }
            GroupFilter::All => {
                for g in &groups {
                    let list: Vec<&Record<Host>> = visible
                        .iter()
                        .copied()
                        .filter(|h| h.data.group_id == Some(g.id))
                        .collect();
                    if !list.is_empty() || query.is_empty() {
                        sections.push((Some(g.clone()), list));
                    }
                }
                let rest: Vec<&Record<Host>> = visible
                    .iter()
                    .copied()
                    .filter(|h| !group_exists(h.data.group_id))
                    .collect();
                if !rest.is_empty() {
                    sections.push((None, rest));
                }
            }
        }

        let warning = cx.theme().warning;
        let total = hosts.len();
        let mut card_ix = 0usize;
        let mut body = v_flex().gap_6().p_6();
        if loaded && total == 0 {
            body = body.child(ui::empty_state(
                IconName::Server,
                t!("hosts.empty.title"),
                t!("hosts.empty.detail"),
                cx,
            ));
        } else if loaded && visible.is_empty() {
            body = body.child(ui::empty_state(
                IconName::Search,
                t!("hosts.no_results.title"),
                t!("hosts.no_results.detail"),
                cx,
            ));
        }
        for (si, (group, list)) in sections.iter().enumerate() {
            let title_group = match self.filter {
                GroupFilter::Favorites => None,
                _ => group.as_ref(),
            };
            let header = if self.filter == GroupFilter::Favorites {
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        ui::icon(IconName::StarFill)
                            .size(px(16.))
                            .text_color(warning),
                    )
                    .child(div().font_semibold().child(t!("hosts.filter.favorites")))
                    .into_any_element()
            } else {
                self.render_group_header(title_group, list.len(), si, cx)
                    .into_any_element()
            };
            let mut grid = h_flex().flex_wrap().gap_3();
            for rec in list {
                grid = grid.child(self.render_card(rec, card_ix, cx));
                card_ix += 1;
            }
            if list.is_empty() {
                grid = grid.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(t!("hosts.group.empty")),
                );
            }
            body = body.child(v_flex().gap_3().child(header).child(grid));
        }

        let theme = cx.theme();
        let content = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(ui::section_header(
                t!("hosts.title"),
                tn!("hosts.subtitle", total),
                h_flex()
                    .gap_2()
                    .child(
                        div().w(px(260.)).child(
                            Input::new(&self.search)
                                .cleanable(true)
                                .prefix(ui::icon(IconName::Search).size(px(14.))),
                        ),
                    )
                    .child(
                        Button::new("open-shell")
                            .icon(ui::icon(IconName::Laptop))
                            .tooltip(t!("hosts.local_terminal_tooltip"))
                            .on_click(
                                cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(OpenRequest::Shell)),
                            ),
                    )
                    .child(
                        Button::new("open-serial")
                            .icon(ui::icon(IconName::ArrowLeftRight))
                            .tooltip(t!("hosts.serial_tooltip"))
                            .on_click(cx.listener(|_, _: &ClickEvent, window, cx| {
                                let weak = cx.entity().downgrade();
                                super::serial::open(window, cx, move |params, _, cx| {
                                    if let Some(v) = weak.upgrade() {
                                        v.update(cx, |_, cx| cx.emit(OpenRequest::Serial(params)));
                                    }
                                });
                            })),
                    )
                    .child(
                        Button::new("import-ssh-config")
                            .icon(ui::icon(IconName::Import))
                            .label(t!("hosts.import"))
                            .tooltip(t!("hosts.import_tooltip"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                super::import::open(this.model.clone(), window, cx);
                            })),
                    )
                    .child(
                        Button::new("new-group")
                            .icon(ui::icon(IconName::FolderPlus))
                            .label(t!("hosts.new_group"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.edit_group(None, window, cx);
                            })),
                    )
                    .child(
                        Button::new("new-host")
                            .primary()
                            .icon(ui::icon(IconName::Plus))
                            .label(t!("hosts.new_host"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.edit(None, window, cx);
                            })),
                    ),
                cx,
            ))
            .child(
                h_flex()
                    .px_6()
                    .py_2()
                    .gap_1()
                    .flex_wrap()
                    .border_b_1()
                    .border_color(theme.border)
                    .children(chips),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(v_flex().size_full().overflow_y_scrollbar().child(body)),
            );

        h_flex()
            .size_full()
            .child(content)
            .when_some(self.editor.clone(), |this, editor| {
                this.child(
                    div()
                        .w(px(440.))
                        .h_full()
                        .flex_shrink_0()
                        .border_l_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().sidebar)
                        .child(editor),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::os_detail;

    #[test]
    fn os_version_without_repeating_the_name() {
        assert_eq!(os_detail("Ubuntu", "Ubuntu 24.04.1 LTS"), "24.04.1 LTS");
        assert_eq!(
            os_detail("Debian", "Debian GNU/Linux 12 (bookworm)"),
            "GNU/Linux 12 (bookworm)"
        );
        assert_eq!(os_detail("macOS", "macOS 14.5"), "14.5");
        assert_eq!(
            os_detail("RHEL", "Red Hat Enterprise Linux 9.4"),
            "Red Hat Enterprise Linux 9.4"
        );
        assert_eq!(os_detail("Alpine", "alpine 3.20.3"), "3.20.3");
        assert_eq!(os_detail("Windows", "Windows"), "Windows");
        // Without breaking multi-byte characters.
        assert_eq!(os_detail("Ubuntu", "Ubü"), "Ubü");
    }
}
