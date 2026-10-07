//! Hosts: cards arranged by group, search, favorites, tags and detected
//! system (with its version). A click opens the editor; a double click
//! connects. The "Import/Export" menu of the header imports hosts from
//! files of other apps and exports them.
//!
//! Right click (or Shift+F10 / the menu key on the focused card) opens the
//! host menu; on a group header, "Connect to all" and "Open all in split
//! view". Cmd/Ctrl+click and Shift+click select several hosts, with a bar to
//! connect them in tabs or in a split view, move or delete them.
//!
//! With the status checks turned on (Settings → General; off by default),
//! each card shows whether its host answers (a dot and the time, see
//! `host_status.rs`), checked while the list is on screen.

use std::collections::HashMap;
use std::time::Duration;

use gpui::{
    AppContext, ClickEvent, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, actions, anchored,
    deferred, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt, DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme, Selectable, Sizable, StyledExt, h_flex, v_flex};
use termoak_client::Scope;
use termoak_core::Id;
use termoak_core::model::{Group, Host, HostSettings, SecretUpdate};
use termoak_core::transfer::TransferMode;

use super::OpenRequest;
use super::host_editor::{EditorEvent, HostEditor};
use crate::accounts::{self as vm, AccountRow, Place};
use crate::host_status::{self, HostStatus, Probe, Shown, Skip};
use crate::runtime;
use crate::state::{AppModel, Item, ToastKind};
use crate::theme;
use crate::ui::{self, IconName};
use crate::workspaces::{self, SavedWorkspace, Workspaces};

/// Margin to tell a click from a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(280);

const CONTEXT: &str = "HostList";

actions!(
    hosts,
    [
        /// Opens the menu of the focused host (Shift+F10, the menu key).
        OpenHostMenu,
        SelectAllHosts,
        ClearSelection,
        ConnectFocused,
        DeleteSelected,
        FocusPrevHost,
        FocusNextHost
    ]
);

/// Keys of the host list (when it has the focus, not the search box).
pub fn init(cx: &mut gpui::App) {
    cx.bind_keys([
        KeyBinding::new("shift-f10", OpenHostMenu, Some(CONTEXT)),
        KeyBinding::new("menu", OpenHostMenu, Some(CONTEXT)),
        KeyBinding::new("escape", ClearSelection, Some(CONTEXT)),
        KeyBinding::new("enter", ConnectFocused, Some(CONTEXT)),
        KeyBinding::new("delete", DeleteSelected, Some(CONTEXT)),
        KeyBinding::new("left", FocusPrevHost, Some(CONTEXT)),
        KeyBinding::new("up", FocusPrevHost, Some(CONTEXT)),
        KeyBinding::new("right", FocusNextHost, Some(CONTEXT)),
        KeyBinding::new("down", FocusNextHost, Some(CONTEXT)),
    ]);
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("cmd-a", SelectAllHosts, Some(CONTEXT)),
        KeyBinding::new("cmd-backspace", DeleteSelected, Some(CONTEXT)),
    ]);
    #[cfg(not(target_os = "macos"))]
    cx.bind_keys([KeyBinding::new("ctrl-a", SelectAllHosts, Some(CONTEXT))]);
}

/// Several hosts chosen with Cmd/Ctrl+click and Shift+click, in the order
/// they were chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection<T> {
    ids: Vec<T>,
    /// Where a Shift+click range starts (the last plain or Cmd/Ctrl click).
    anchor: Option<T>,
}

impl<T> Default for Selection<T> {
    fn default() -> Self {
        Self {
            ids: Vec::new(),
            anchor: None,
        }
    }
}

impl<T: Copy + PartialEq> Selection<T> {
    pub fn ids(&self) -> &[T] {
        &self.ids
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn contains(&self, id: T) -> bool {
        self.ids.contains(&id)
    }

    /// Plain click: nothing selected, the range starts here.
    pub fn click(&mut self, id: T) {
        self.ids.clear();
        self.anchor = Some(id);
    }

    /// Cmd/Ctrl+click: adds or removes one host.
    pub fn toggle(&mut self, id: T) {
        if let Some(pos) = self.ids.iter().position(|x| *x == id) {
            self.ids.remove(pos);
        } else {
            self.ids.push(id);
        }
        self.anchor = Some(id);
    }

    /// Shift+click: the hosts between the anchor and `id` in the order they
    /// are shown (`order`), replacing the previous range.
    pub fn extend_to(&mut self, order: &[T], id: T) {
        let to = order.iter().position(|x| *x == id);
        let from = self
            .anchor
            .and_then(|a| order.iter().position(|x| *x == a))
            .or(to);
        let (Some(from), Some(to)) = (from, to) else {
            return;
        };
        let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
        self.ids = order[lo..=hi].to_vec();
        if self.anchor.is_none() {
            self.anchor = Some(id);
        }
    }

    pub fn select_all(&mut self, order: &[T]) {
        self.ids = order.to_vec();
    }

    pub fn clear(&mut self) {
        self.ids.clear();
    }

    /// Drops what no longer exists (deleted, or hidden by the search).
    pub fn retain(&mut self, existing: &[T]) {
        self.ids.retain(|id| existing.contains(id));
        if self.anchor.is_some_and(|a| !existing.contains(&a)) {
            self.anchor = None;
        }
    }
}

/// Address to copy: `user@host:port` (IPv6 in brackets).
pub fn ssh_address(user: Option<&str>, host: &str, port: u16) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    match user.filter(|u| !u.is_empty()) {
        Some(u) => format!("{u}@{host}:{port}"),
        None => format!("{host}:{port}"),
    }
}

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
    /// Hosts chosen with Cmd/Ctrl+click or Shift+click.
    selection: Selection<Id>,
    /// Host with the keyboard focus (arrows, Enter, Shift+F10).
    cursor: Option<Id>,
    /// Hosts in the order they are shown (for Shift ranges and the keyboard).
    order: Vec<Id>,
    focus: FocusHandle,
    /// Menu opened from the keyboard on the focused card.
    kbd_menu: Option<(Id, Entity<PopupMenu>, Subscription)>,
    /// Whether each host answers (shared by the windows).
    status: Entity<HostStatus>,
    /// What each card on screen shows (worked out when rendering).
    shown: HashMap<Id, Shown>,
    /// Saved workspaces ("Add to workspace").
    workspaces: Entity<Workspaces>,
}

impl EventEmitter<OpenRequest> for HostsView {}

impl HostsView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("hosts.search_placeholder")));
        let status = HostStatus::global(&model, cx);
        let workspaces = Workspaces::global(&model, cx);
        let subs = vec![
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.observe(&status, |_, _, cx| cx.notify()),
            cx.observe(&workspaces, |_, _, cx| cx.notify()),
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
            selection: Selection::default(),
            cursor: None,
            order: Vec::new(),
            focus: cx.focus_handle(),
            kbd_menu: None,
            status,
            shown: HashMap::new(),
            workspaces,
        }
    }

    // ----- Status -----

    /// What to check for a host, or why it is not checked.
    fn probe_of(&self, rec: &Item<Host>, cx: &gpui::App) -> Result<Probe, Skip> {
        let m = self.model.read(cx);
        let groups: Vec<&Group> = m.groups.iter().map(|g| &g.data).collect();
        let settings = host_status::effective_settings(&rec.data, &groups);
        let strict = m.vault_entry_of(rec).is_some_and(vm::VaultEntry::strict);
        let off = m.settings.host_status_off.contains(&rec.data.id);
        let (target, needs_password) = host_status::target_of(
            &rec.data,
            &settings,
            strict,
            rec.access.can_read_secrets(),
            off,
        )?;
        Ok(Probe {
            host_id: rec.data.id,
            target,
            secret: needs_password.then(|| rec.item_ref()),
        })
    }

    /// Hosts to check among `ids` (the ones that can be checked).
    fn probes(&self, ids: &[Id], cx: &gpui::App) -> Vec<Probe> {
        let m = self.model.read(cx);
        ids.iter()
            .filter_map(|id| m.host_record(*id))
            .filter_map(|rec| self.probe_of(rec, cx).ok())
            .collect()
    }

    /// "Check now" (the header button or the host menu).
    fn check_now(&mut self, ids: Vec<Id>, cx: &mut Context<Self>) {
        if !self.model.read(cx).settings.host_status {
            return;
        }
        let probes = self.probes(&ids, cx);
        self.status.update(cx, |s, cx| s.check_now(probes, cx));
    }

    /// Turns the check off or on for one host (on this device).
    fn toggle_status_check(&mut self, id: Id, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            if let Some(pos) = s.host_status_off.iter().position(|x| *x == id) {
                s.host_status_off.remove(pos);
            } else {
                s.host_status_off.push(id);
            }
            m.save_settings(s, cx);
        });
    }

    // ----- Secrets -----

    /// "Copy password": after Touch ID / Windows Hello if the lock asks.
    fn copy_password(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.model.read(cx).host_record(id).map(Item::item_ref) else {
            return;
        };
        let model = self.model.clone();
        crate::app_lock::guard_secret(&self.model, window, cx, move |window, cx| {
            let ws = model.read(cx).ws.clone();
            let task = runtime::spawn(cx, async move {
                ws.item_secret::<Host>(item).await.map(|s| s.password)
            });
            window
                .spawn(cx, async move |cx| {
                    let res = task.await;
                    let _ = cx.update(|window, cx| match res {
                        Ok(Some(password)) => {
                            cx.write_to_clipboard(ClipboardItem::new_string(password));
                            ui::success(window, cx, t!("hosts.password_copied"));
                        }
                        Ok(None) => {
                            ui::notify(window, cx, ToastKind::Info, t!("hosts.no_password"))
                        }
                        Err(e) => ui::error(window, cx, e),
                    });
                })
                .detach();
        });
    }

    // ----- Workspaces -----

    /// "Add to workspace" → a saved one.
    fn add_to_workspace(
        &mut self,
        ws: Id,
        ids: Vec<Id>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = self
            .workspaces
            .read(cx)
            .get(ws)
            .map(|w| w.name.clone())
            .unwrap_or_default();
        self.workspaces
            .update(cx, |w, cx| w.add_hosts(ws, &ids, cx));
        ui::success(
            window,
            cx,
            tn!("workspaces.hosts_added", ids.len(), name = name),
        );
    }

    /// "Add to workspace" → "New workspace…".
    fn new_workspace_with(&mut self, ids: Vec<Id>, window: &mut Window, cx: &mut Context<Self>) {
        let default = workspaces::next_name(
            &t!("workspaces.default_name"),
            &self.workspaces.read(cx).list,
        );
        let list = self.workspaces.clone();
        workspaces::ask_name(
            window,
            cx,
            t!("workspaces.new_title"),
            default,
            None,
            move |name, window, cx| {
                let mut layout = workspaces::Layout::default();
                for id in &ids {
                    layout.add_host(*id);
                }
                list.update(cx, |w, cx| {
                    w.add(SavedWorkspace::new(name.clone(), layout), cx)
                });
                ui::success(window, cx, t!("workspaces.saved", name = name));
            },
        );
    }

    /// The "Add to workspace" submenu.
    fn workspace_submenu(
        menu: PopupMenu,
        view: WeakEntity<Self>,
        ids: Vec<Id>,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let list: Vec<(Id, String)> = view
            .upgrade()
            .map(|v| {
                v.read(cx)
                    .workspaces
                    .read(cx)
                    .list
                    .iter()
                    .map(|w| (w.id, w.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        menu.submenu_with_icon(
            Some(ui::icon(IconName::LayoutPanelLeft)),
            t!("workspaces.add_to"),
            window,
            cx,
            move |mut sub, _, _| {
                for (ws, name) in list.clone() {
                    let (w, ids) = (view.clone(), ids.clone());
                    sub = sub.item(PopupMenuItem::new(name).on_click(move |_, window, cx| {
                        if let Some(v) = w.upgrade() {
                            let ids = ids.clone();
                            v.update(cx, |v, cx| v.add_to_workspace(ws, ids, window, cx));
                        }
                    }));
                }
                if !list.is_empty() {
                    sub = sub.separator();
                }
                let (w, ids) = (view.clone(), ids.clone());
                sub.item(
                    PopupMenuItem::new(t!("workspaces.new_menu"))
                        .icon(ui::icon(IconName::Plus))
                        .on_click(move |_, window, cx| {
                            if let Some(v) = w.upgrade() {
                                let ids = ids.clone();
                                v.update(cx, |v, cx| v.new_workspace_with(ids, window, cx));
                            }
                        }),
                )
            },
        )
    }

    /// Opens the editor of a host (or an empty one to create it).
    pub fn edit(&mut self, host: Option<Item<Host>>, window: &mut Window, cx: &mut Context<Self>) {
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

    /// Marks or unmarks a host as a favourite (the opposite of what the
    /// list shows), changing only that: the rest is read again from the
    /// store, which may be newer (e.g. its detected OS).
    fn toggle_favorite(&mut self, rec: &Item<Host>, window: &mut Window, cx: &mut Context<Self>) {
        let favorite = !rec.data.favorite;
        let task = self.model.update(cx, |m, cx| {
            m.update_item::<Host>(rec.data.id, move |h| h.favorite = favorite, cx)
        });
        cx.spawn_in(window, async move |_, cx| {
            if let Err(e) = task.await {
                let _ = cx.update(|window, cx| ui::error(window, cx, e));
            }
        })
        .detach();
    }

    fn delete_host(&mut self, rec: &Item<Host>, window: &mut Window, cx: &mut Context<Self>) {
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

    // ----- Several hosts -----

    /// Connects to each host in its own tab.
    fn connect_many(&mut self, ids: &[Id], cx: &mut Context<Self>) {
        for host_id in ids {
            cx.emit(OpenRequest::Local { host_id: *host_id });
        }
    }

    /// Opens the hosts together in a split view: a new tab or, with
    /// `current`, added to the current one.
    fn open_split(&mut self, ids: Vec<Id>, current: bool, cx: &mut Context<Self>) {
        if !ids.is_empty() {
            cx.emit(OpenRequest::Split {
                hosts: ids,
                current,
            });
        }
    }

    /// "Ask AI": a new AI task on these hosts (one conversation per host).
    fn ask_ai(&mut self, ids: Vec<Id>, cx: &mut Context<Self>) {
        if !ids.is_empty() {
            cx.emit(OpenRequest::AiTask { host_ids: ids });
        }
    }

    fn duplicate_host(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        let task = self.model.update(cx, |m, cx| m.duplicate_host(id, cx));
        cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update_in(cx, |this, window, cx| match res {
                Ok(rec) => {
                    ui::success(window, cx, t!("hosts.duplicated", name = rec.data.label));
                    this.cursor = Some(rec.data.id);
                    this.edit(Some(rec), window, cx);
                }
                Err(e) => ui::error(window, cx, e),
            });
        })
        .detach();
    }

    fn copy_address(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        let m = self.model.read(cx);
        let Some(host) = m.host(id) else {
            return;
        };
        let mut text = ssh_address(
            m.effective_user(host).as_deref(),
            &host.address,
            m.effective_port(host),
        );
        // What quick connect reads back.
        if host.protocol.is_telnet() {
            text.insert_str(0, "telnet://");
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        ui::notify(
            window,
            cx,
            crate::state::ToastKind::Info,
            t!("hosts.address_copied", address = text),
        );
    }

    fn move_to_group(
        &mut self,
        ids: Vec<Id>,
        group: Option<Id>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = self.model.update(cx, |m, cx| m.move_hosts(ids, group, cx));
        cx.spawn_in(window, async move |_, cx| {
            if let Err(e) = task.await {
                let _ = cx.update(|window, cx| ui::error(window, cx, e));
            }
        })
        .detach();
    }

    /// "Move to…" / "Copy to…" another vault, account or This device. All
    /// the hosts must be in the same place.
    fn transfer(
        &mut self,
        ids: Vec<Id>,
        mode: TransferMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items: Vec<_> = {
            let m = self.model.read(cx);
            ids.iter()
                .filter_map(|id| m.host_record(*id).map(Item::item_ref))
                .collect()
        };
        // An open editor of a moved host would save over its old place.
        let me = cx.entity().downgrade();
        let done: super::transfer::OnDone = std::rc::Rc::new(move |_, cx| {
            if let Some(v) = me.upgrade() {
                v.update(cx, |v, cx| {
                    v.editor = None;
                    v.selection.clear();
                    cx.notify();
                });
            }
        });
        super::transfer::open(self.model.clone(), items, mode, Some(done), window, cx);
    }

    /// Deletes the selected hosts, after asking.
    fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self.selection.ids().to_vec();
        match ids.len() {
            0 => {
                if let Some(rec) = self
                    .cursor
                    .and_then(|id| self.model.read(cx).host_record(id).cloned())
                {
                    self.delete_host(&rec, window, cx);
                }
            }
            1 => {
                if let Some(rec) = self.model.read(cx).host_record(ids[0]).cloned() {
                    self.delete_host(&rec, window, cx);
                }
            }
            n => {
                let model = self.model.clone();
                let weak = cx.entity().downgrade();
                ui::confirm(
                    window,
                    cx,
                    tn!("hosts.bulk.delete_title", n),
                    tn!("hosts.bulk.delete_message", n),
                    t!("hosts.delete.ok"),
                    true,
                    move |window, cx| {
                        let task = model.update(cx, |m, cx| m.delete_hosts(ids.clone(), cx));
                        let weak = weak.clone();
                        window
                            .spawn(cx, async move |cx| {
                                let res = task.await;
                                let _ = cx.update(|window, cx| match res {
                                    Ok(()) => {
                                        if let Some(v) = weak.upgrade() {
                                            v.update(cx, |v, cx| {
                                                v.selection.clear();
                                                cx.notify();
                                            });
                                        }
                                        ui::success(window, cx, tn!("hosts.bulk.deleted", n));
                                    }
                                    Err(e) => ui::error(window, cx, e),
                                });
                            })
                            .detach();
                    },
                );
            }
        }
    }

    /// Hosts the menu of `id` acts on: the selection if `id` is part of it,
    /// otherwise just `id`.
    fn targets(&self, id: Id) -> Vec<Id> {
        if self.selection.len() > 1 && self.selection.contains(id) {
            self.selection.ids().to_vec()
        } else {
            vec![id]
        }
    }

    /// Menu of a host (right click, "…" button and Shift+F10). With several
    /// hosts selected, including this one, it acts on all of them.
    fn host_menu(
        menu: PopupMenu,
        view: WeakEntity<Self>,
        id: Id,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let Some(this) = view.upgrade() else {
            return menu;
        };
        let (status_on, status_off) = view
            .upgrade()
            .map(|v| {
                let s = &v.read(cx).model.read(cx).settings;
                (s.host_status, s.host_status_off.contains(&id))
            })
            .unwrap_or((false, false));
        let (targets, rec, groups, server, caps, same_place, all_writable) = {
            let v = this.read(cx);
            let m = v.model.read(cx);
            let targets = v.targets(id);
            let rec = m.host_record(id).cloned();
            let place = rec.as_ref().map(|r| (r.scope, m.vault_of(r)));
            // Groups of the same place (references stay inside a vault).
            let groups = m
                .groups
                .iter()
                .filter(|g| Some((g.scope, m.vault_of(g))) == place)
                .map(|g| (g.data.id, g.data.name.clone()))
                .collect::<Vec<_>>();
            let hosts: Vec<&Item<Host>> =
                targets.iter().filter_map(|t| m.host_record(*t)).collect();
            let same_place = hosts
                .iter()
                .all(|h| Some((h.scope, m.vault_of(h))) == place);
            let all_writable = hosts.iter().all(|h| h.access.can_write());
            (
                targets,
                rec.clone(),
                groups,
                m.api_for_host(id).is_some(),
                rec.as_ref().map(|r| m.caps_of(r)),
                same_place,
                all_writable,
            )
        };
        let (Some(rec), Some(caps)) = (rec, caps) else {
            return menu;
        };
        let move_ids = targets.clone();
        let ws_ids = targets.clone();
        let n = targets.len();
        let w = view.clone();
        let act = move |f: fn(&mut HostsView, Vec<Id>, &mut Window, &mut Context<HostsView>)| {
            let w = w.clone();
            let targets = targets.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut gpui::App| {
                if let Some(v) = w.upgrade() {
                    let targets = targets.clone();
                    v.update(cx, |v, cx| f(v, targets, window, cx));
                }
            }
        };
        let current_group = rec.data.group_id;
        let move_menu = |menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>| {
            if groups.is_empty() || !all_writable || !same_place {
                return menu;
            }
            let w = view.clone();
            let ids = move_ids.clone();
            let groups = groups.clone();
            menu.submenu_with_icon(
                Some(ui::icon(IconName::FolderInput)),
                if n > 1 {
                    tn!("hosts.bulk.move", n)
                } else {
                    t!("hosts.menu.move_to_group")
                },
                window,
                cx,
                move |mut sub, _, _| {
                    let mut entries: Vec<(Option<Id>, SharedString)> =
                        vec![(None, t!("hosts.menu.no_group"))];
                    entries.extend(
                        groups
                            .iter()
                            .map(|(g, name)| (Some(*g), name.clone().into())),
                    );
                    for (group, name) in entries {
                        let w = w.clone();
                        let ids = ids.clone();
                        sub = sub.item(
                            PopupMenuItem::new(name)
                                .checked(n == 1 && group == current_group)
                                .on_click(move |_, window, cx| {
                                    if let Some(v) = w.upgrade() {
                                        let ids = ids.clone();
                                        v.update(cx, |v, cx| {
                                            v.move_to_group(ids, group, window, cx)
                                        });
                                    }
                                }),
                        );
                    }
                    sub
                },
            )
        };

        let transfer_items = |menu: PopupMenu, move_out: bool, copy_out: bool| {
            if !same_place {
                return menu;
            }
            menu.when(move_out, |menu| {
                menu.item(
                    PopupMenuItem::new(t!("hosts.menu.move_to"))
                        .icon(ui::icon(IconName::ArrowRightLeft))
                        .on_click(act(|v, ids, window, cx| {
                            v.transfer(ids, TransferMode::Move, window, cx)
                        })),
                )
            })
            .when(copy_out, |menu| {
                menu.item(
                    PopupMenuItem::new(t!("hosts.menu.copy_to"))
                        .icon(ui::icon(IconName::CopyPlus))
                        .on_click(act(|v, ids, window, cx| {
                            v.transfer(ids, TransferMode::Copy, window, cx)
                        })),
                )
            })
        };

        if n > 1 {
            let menu = menu
                .label(tn!("hosts.bulk.selected", n))
                .item(
                    PopupMenuItem::new(tn!("hosts.bulk.connect", n))
                        .icon(ui::icon(IconName::SquareTerminal))
                        .on_click(act(|v, ids, _, cx| v.connect_many(&ids, cx))),
                )
                .item(
                    PopupMenuItem::new(t!("hosts.bulk.split"))
                        .icon(ui::icon(IconName::LayoutGrid))
                        .on_click(act(|v, ids, _, cx| v.open_split(ids, false, cx))),
                )
                .item(
                    PopupMenuItem::new(tn!("hosts.ask_ai", n))
                        .icon(ui::icon(IconName::Sparkles))
                        .on_click(act(|v, ids, _, cx| v.ask_ai(ids, cx))),
                );
            let menu = Self::workspace_submenu(menu, view.clone(), ws_ids, window, cx);
            let menu = menu.when(status_on, |menu| {
                menu.item(
                    PopupMenuItem::new(t!("host_status.check_now"))
                        .icon(ui::icon(IconName::RefreshCw))
                        .on_click(act(|v, ids, _, cx| v.check_now(ids, cx))),
                )
            });
            let menu = move_menu(menu.separator(), window, cx);
            let menu = transfer_items(menu, all_writable, all_writable);
            return menu.when(all_writable, |menu| {
                menu.separator().item(
                    PopupMenuItem::new(tn!("hosts.bulk.delete", n))
                        .icon(ui::icon(IconName::Trash))
                        .on_click(act(|v, _, window, cx| v.delete_selected(window, cx))),
                )
            });
        }

        let favorite = rec.data.favorite;
        // Telnet: no server sessions or SFTP.
        let ssh = rec.data.protocol.is_ssh();
        let menu = menu
            .item(
                PopupMenuItem::new(t!("hosts.menu.connect"))
                    .icon(ui::icon(IconName::SquareTerminal))
                    .on_click(act(|v, ids, _, cx| v.connect_many(&ids, cx))),
            )
            .item(
                PopupMenuItem::new(t!("hosts.menu.connect_split"))
                    .icon(ui::icon(IconName::LayoutGrid))
                    .on_click(act(|v, ids, _, cx| v.open_split(ids, true, cx))),
            )
            .when(server && ssh, |menu| {
                menu.item(
                    PopupMenuItem::new(t!("hosts.menu.connect_server"))
                        .icon(ui::icon(IconName::Cloud))
                        .on_click(act(|v, ids, _, cx| {
                            for host_id in ids {
                                v.connect(host_id, true, cx);
                            }
                        })),
                )
            })
            .when(ssh, |menu| {
                menu.item(
                    PopupMenuItem::new(t!("hosts.menu.open_sftp"))
                        .icon(ui::icon(IconName::FolderOpen))
                        .on_click(act(|_, ids, _, cx| {
                            for host_id in ids {
                                cx.emit(OpenRequest::Sftp {
                                    host_id,
                                    conn: None,
                                });
                            }
                        })),
                )
            });
        let menu = Self::workspace_submenu(menu, view.clone(), ws_ids, window, cx);
        let menu = menu
            .when(status_on && !status_off, |menu| {
                menu.item(
                    PopupMenuItem::new(t!("host_status.check_now"))
                        .icon(ui::icon(IconName::RefreshCw))
                        .on_click(act(|v, ids, _, cx| v.check_now(ids, cx))),
                )
            })
            .when(status_on, |menu| {
                menu.item(
                    PopupMenuItem::new(if status_off {
                        t!("host_status.turn_on")
                    } else {
                        t!("host_status.turn_off")
                    })
                    .icon(ui::icon(IconName::Activity))
                    .on_click(act(|v, ids, _, cx| {
                        if let Some(id) = ids.first() {
                            v.toggle_status_check(*id, cx);
                        }
                    })),
                )
            })
            .separator()
            .item(
                PopupMenuItem::new(if caps.edit {
                    t!("hosts.menu.edit")
                } else {
                    t!("hosts.menu.view")
                })
                .icon(ui::icon(if caps.edit {
                    IconName::Pencil
                } else {
                    IconName::Eye
                }))
                .on_click(act(|v, ids, window, cx| {
                    let rec = ids
                        .first()
                        .and_then(|id| v.model.read(cx).host_record(*id).cloned());
                    if rec.is_some() {
                        v.edit(rec, window, cx);
                    }
                })),
            )
            .when(caps.duplicate, |menu| {
                menu.item(
                    PopupMenuItem::new(t!("hosts.menu.duplicate"))
                        .icon(ui::icon(IconName::CopyPlus))
                        .on_click(act(|v, ids, window, cx| {
                            if let Some(id) = ids.first() {
                                v.duplicate_host(*id, window, cx);
                            }
                        })),
                )
            })
            .item(
                PopupMenuItem::new(t!("hosts.menu.copy_address"))
                    .icon(ui::icon(IconName::Copy))
                    .on_click(act(|v, ids, window, cx| {
                        if let Some(id) = ids.first() {
                            v.copy_address(*id, window, cx);
                        }
                    })),
            )
            .when(caps.reveal && rec.meta.has_secret, |menu| {
                menu.item(
                    PopupMenuItem::new(t!("hosts.menu.copy_password"))
                        .icon(ui::icon(IconName::KeyRound))
                        .on_click(act(|v, ids, window, cx| {
                            if let Some(id) = ids.first() {
                                v.copy_password(*id, window, cx);
                            }
                        })),
                )
            })
            .when(caps.edit, |menu| {
                menu.item(
                    PopupMenuItem::new(if favorite {
                        t!("hosts.menu.unfavorite")
                    } else {
                        t!("hosts.menu.favorite")
                    })
                    .icon(ui::icon(IconName::Star))
                    .on_click(act(|v, ids, window, cx| {
                        let rec = ids
                            .first()
                            .and_then(|id| v.model.read(cx).host_record(*id).cloned());
                        if let Some(rec) = rec {
                            v.toggle_favorite(&rec, window, cx);
                        }
                    })),
                )
            });
        let menu = move_menu(menu, window, cx);
        let menu = transfer_items(menu, caps.move_out, caps.copy_out);
        menu.when(caps.delete, |menu| {
            menu.separator().item(
                PopupMenuItem::new(t!("hosts.menu.delete"))
                    .icon(ui::icon(IconName::Trash))
                    .on_click(act(|v, ids, window, cx| {
                        let rec = ids
                            .first()
                            .and_then(|id| v.model.read(cx).host_record(*id).cloned());
                        if let Some(rec) = rec {
                            v.delete_host(&rec, window, cx);
                        }
                    })),
            )
        })
    }

    /// Menu of a group header: connect to all its hosts, in tabs or in a
    /// split view, and edit or delete the group.
    fn group_menu(
        menu: PopupMenu,
        view: WeakEntity<Self>,
        group: Option<Group>,
        hosts: Vec<Id>,
    ) -> PopupMenu {
        let (w1, w2, w3, w4, w5) = (view.clone(), view.clone(), view.clone(), view.clone(), view);
        let (h1, h2, h3) = (hosts.clone(), hosts.clone(), hosts.clone());
        let empty = hosts.is_empty();
        let menu = menu
            .item(
                PopupMenuItem::new(tn!("hosts.group.menu.connect_all", hosts.len()))
                    .icon(ui::icon(IconName::SquareTerminal))
                    .disabled(empty)
                    .on_click(move |_, _, cx| {
                        if let Some(v) = w1.upgrade() {
                            v.update(cx, |v, cx| v.connect_many(&h1, cx));
                        }
                    }),
            )
            .item(
                PopupMenuItem::new(t!("hosts.group.menu.split_all"))
                    .icon(ui::icon(IconName::LayoutGrid))
                    .disabled(empty)
                    .on_click(move |_, _, cx| {
                        if let Some(v) = w2.upgrade() {
                            let ids = h2.clone();
                            v.update(cx, |v, cx| v.open_split(ids, false, cx));
                        }
                    }),
            )
            .item(
                PopupMenuItem::new(tn!("hosts.ask_ai", hosts.len()))
                    .icon(ui::icon(IconName::Sparkles))
                    .disabled(empty)
                    .on_click(move |_, _, cx| {
                        if let Some(v) = w5.upgrade() {
                            let ids = h3.clone();
                            v.update(cx, |v, cx| v.ask_ai(ids, cx));
                        }
                    }),
            );
        let Some(g) = group else {
            return menu;
        };
        let g2 = g.clone();
        menu.separator()
            .item(
                PopupMenuItem::new(t!("hosts.group.menu.edit"))
                    .icon(ui::icon(IconName::Pencil))
                    .on_click(move |_, window, cx| {
                        if let Some(v) = w3.upgrade() {
                            let g = g.clone();
                            v.update(cx, |v, cx| v.edit_group(Some(g), window, cx));
                        }
                    }),
            )
            .item(
                PopupMenuItem::new(t!("hosts.group.menu.delete"))
                    .icon(ui::icon(IconName::Trash))
                    .on_click(move |_, window, cx| {
                        if let Some(v) = w4.upgrade() {
                            let g = g2.clone();
                            v.update(cx, |v, cx| v.delete_group(g, window, cx));
                        }
                    }),
            )
    }

    // ----- Keyboard -----

    fn on_open_menu(&mut self, _: &OpenHostMenu, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.cursor.or_else(|| self.order.first().copied()) else {
            return;
        };
        self.cursor = Some(id);
        let view = cx.entity().downgrade();
        let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
            Self::host_menu(menu, view.clone(), id, window, cx)
        });
        let sub = cx.subscribe_in(
            &menu,
            window,
            |this, _, _: &gpui::DismissEvent, window, cx| {
                this.kbd_menu = None;
                this.focus.focus(window, cx);
                cx.notify();
            },
        );
        let handle = menu.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        self.kbd_menu = Some((id, menu, sub));
        cx.notify();
    }

    fn on_select_all(&mut self, _: &SelectAllHosts, _: &mut Window, cx: &mut Context<Self>) {
        let order = self.order.clone();
        self.selection.select_all(&order);
        cx.notify();
    }

    fn on_clear_selection(&mut self, _: &ClearSelection, _: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            cx.propagate();
            return;
        }
        self.selection.clear();
        cx.notify();
    }

    fn on_connect_focused(&mut self, _: &ConnectFocused, _: &mut Window, cx: &mut Context<Self>) {
        if self.selection.len() > 1 {
            let ids = self.selection.ids().to_vec();
            self.connect_many(&ids, cx);
        } else if let Some(id) = self.cursor {
            self.connect(id, false, cx);
        }
    }

    fn on_delete_selected(
        &mut self,
        _: &DeleteSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_selected(window, cx);
    }

    fn step_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.order.is_empty() {
            return;
        }
        let n = self.order.len() as isize;
        let next = match self
            .cursor
            .and_then(|c| self.order.iter().position(|x| *x == c))
        {
            Some(ix) => (ix as isize + delta).rem_euclid(n),
            None if delta < 0 => n - 1,
            None => 0,
        };
        self.cursor = Some(self.order[next as usize]);
        cx.notify();
    }

    fn on_prev(&mut self, _: &FocusPrevHost, _: &mut Window, cx: &mut Context<Self>) {
        self.step_cursor(-1, cx);
    }

    fn on_next(&mut self, _: &FocusNextHost, _: &mut Window, cx: &mut Context<Self>) {
        self.step_cursor(1, cx);
    }

    /// Bar shown while several hosts are selected.
    fn render_selection_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let n = self.selection.len();
        let theme = cx.theme();
        let has_groups = !self.model.read(cx).groups.is_empty();
        let view = cx.entity().downgrade();
        h_flex()
            .mx_6()
            .mt_3()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .flex_wrap()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.primary)
            .bg(theme.secondary)
            .child(
                ui::icon(IconName::SquareCheck)
                    .size(px(16.))
                    .text_color(theme.primary),
            )
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child(tn!("hosts.bulk.selected", n)),
            )
            .child(div().flex_1())
            .child(
                Button::new("bulk-connect")
                    .small()
                    .primary()
                    .icon(ui::icon(IconName::SquareTerminal))
                    .label(tn!("hosts.bulk.connect", n))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        let ids = this.selection.ids().to_vec();
                        this.connect_many(&ids, cx);
                    })),
            )
            .child(
                Button::new("bulk-split")
                    .small()
                    .icon(ui::icon(IconName::LayoutGrid))
                    .label(t!("hosts.bulk.split"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        let ids = this.selection.ids().to_vec();
                        this.open_split(ids, false, cx);
                    })),
            )
            .child(
                Button::new("bulk-ask-ai")
                    .small()
                    .icon(ui::icon(IconName::Sparkles))
                    .label(t!("hosts.ask_ai_short"))
                    .tooltip(tn!("hosts.ask_ai", n))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        let ids = this.selection.ids().to_vec();
                        this.ask_ai(ids, cx);
                    })),
            )
            .when(has_groups, |this| {
                this.child(
                    Button::new("bulk-move")
                        .small()
                        .icon(ui::icon(IconName::FolderInput))
                        .label(t!("hosts.bulk.move_short"))
                        .dropdown_menu(move |mut menu, _, cx| {
                            let Some(v) = view.upgrade() else {
                                return menu;
                            };
                            let groups: Vec<(Option<Id>, SharedString)> =
                                std::iter::once((None, t!("hosts.menu.no_group")))
                                    .chain(v.read(cx).model.read(cx).groups.iter().map(|g| {
                                        (Some(g.data.id), SharedString::from(g.data.name.clone()))
                                    }))
                                    .collect();
                            for (group, name) in groups {
                                let w = view.clone();
                                menu = menu.item(PopupMenuItem::new(name).on_click(
                                    move |_, window, cx| {
                                        if let Some(v) = w.upgrade() {
                                            v.update(cx, |v, cx| {
                                                let ids = v.selection.ids().to_vec();
                                                v.move_to_group(ids, group, window, cx)
                                            });
                                        }
                                    },
                                ));
                            }
                            menu
                        }),
                )
            })
            .child(
                Button::new("bulk-delete")
                    .small()
                    .danger()
                    .icon(ui::icon(IconName::Trash))
                    .label(t!("hosts.menu.delete"))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.delete_selected(window, cx)
                    })),
            )
            .child(
                Button::new("bulk-all")
                    .small()
                    .ghost()
                    .label(t!("hosts.bulk.select_all"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        let order = this.order.clone();
                        this.selection.select_all(&order);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("bulk-clear")
                    .small()
                    .ghost()
                    .icon(ui::icon(IconName::X))
                    .tooltip(t!("hosts.bulk.clear"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.selection.clear();
                        cx.notify();
                    })),
            )
    }

    fn matches(query: &str, rec: &Item<Host>, group_name: Option<&str>) -> bool {
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
        rec: &Item<Host>,
        ix: usize,
        window: &mut Window,
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
        // Where it lives: vault chip (with more than one place in sight),
        // account badge (with more than one account) and "Use only".
        let (vault_chip, account_badge, use_only) = {
            let m = self.model.read(cx);
            let in_view = m.accounts_in_view().len();
            let chips = vm::show_vault_picker(m.vaults_in_view().len(), m.has_device_items());
            let chip = chips.then(|| m.vault_entry_of(rec).cloned()).flatten();
            let device = chips && rec.scope == Scope::Device;
            let badge = vm::show_account_badges(in_view)
                .then(|| {
                    rec.account()
                        .and_then(|a| m.account(a))
                        .map(|a| AccountRow::new(&a.info))
                })
                .flatten();
            (
                chip.map(|c| super::vaults::chip(&c, cx).into_any_element())
                    .or_else(|| {
                        device.then(|| {
                            ui::pill(t!("accounts.switcher.device"), cx.theme().muted_foreground)
                                .into_any_element()
                        })
                    }),
                badge,
                m.caps_of(rec).use_only_badge,
            )
        };
        let status = self
            .shown
            .get(&host.id)
            .map(|s| (*s, self.status.read(cx).is_running(host.id)));
        let theme = cx.theme();
        let selected = self.editor.as_ref().and_then(|e| e.read(cx).host_id()) == Some(host.id);
        let telnet = host.protocol.is_telnet();
        // Without a detected system an SSH host says "SSH" (Telnet ones get
        // their own badge).
        let os_known = host.os.is_some();
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
        let port = host.settings.port;
        let subtitle = format!(
            "{}{}{}",
            user.map(|u| format!("{u}@")).unwrap_or_default(),
            host.address,
            port.filter(|p| *p != host.protocol.default_port())
                .map(|p| format!(":{p}"))
                .unwrap_or_default()
        );
        let id = host.id;
        let weak = cx.entity().downgrade();
        let weak_menu = weak.clone();
        let multi = self.selection.contains(id);
        let cursor = self.cursor == Some(id) && self.focus.contains_focused(window, cx);
        let kbd_menu = self
            .kbd_menu
            .as_ref()
            .filter(|(m, _, _)| *m == id)
            .map(|(_, menu, _)| menu.clone());

        v_flex()
            .id(("host-card", ix))
            .relative()
            .w(px(290.))
            .p_3()
            .gap_2()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(if selected || multi || cursor {
                theme.primary
            } else {
                theme.border
            })
            .when(multi, |this| this.bg(theme.list_active))
            .when(!multi, |this| {
                this.bg(theme.secondary)
                    .hover(|s| s.bg(theme.secondary_hover))
            })
            .cursor_pointer()
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                this.focus.focus(window, cx);
                this.cursor = Some(id);
                let mods = ev.modifiers();
                if mods.secondary() {
                    // Cmd/Ctrl+click: add or remove it from the selection.
                    this.pending_click = None;
                    this.selection.toggle(id);
                    cx.notify();
                    return;
                }
                if mods.shift {
                    let order = this.order.clone();
                    this.pending_click = None;
                    this.selection.extend_to(&order, id);
                    cx.notify();
                    return;
                }
                this.selection.click(id);
                cx.notify();
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
                    .child(crate::logos::avatar(host, 40., theme.radius))
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
                    .when(multi, |this| {
                        this.child(
                            ui::icon(IconName::SquareCheck)
                                .size(px(16.))
                                .text_color(theme.primary),
                        )
                    })
                    .when_some(status, |this, (shown, running)| {
                        let color = match shown {
                            Shown::Up { .. } => theme.success,
                            Shown::Down { .. } => theme.danger,
                            Shown::Pending | Shown::Skipped(_) => theme.muted_foreground,
                        };
                        let tip = if running && shown == Shown::Pending {
                            t!("host_status.checking")
                        } else {
                            host_status::tooltip(shown)
                        };
                        this.child(
                            h_flex()
                                .id(("host-status", ix))
                                .flex_shrink_0()
                                .gap_1()
                                .items_center()
                                .child(
                                    div()
                                        .size(px(8.))
                                        .rounded_full()
                                        .bg(color)
                                        .when(matches!(shown, Shown::Skipped(_)), |d| {
                                            d.opacity(0.5)
                                        }),
                                )
                                .when_some(
                                    match shown {
                                        Shown::Up { rtt, .. } => {
                                            Some(crate::terminal::latency::format(Some(rtt)))
                                        }
                                        _ => None,
                                    },
                                    |this, ms| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .whitespace_nowrap()
                                                .child(ms),
                                        )
                                    },
                                )
                                .tooltip(move |window, cx| {
                                    Tooltip::new(tip.clone()).build(window, cx)
                                }),
                        )
                    })
                    .child(
                        Button::new(("card-menu", ix))
                            .xsmall()
                            .ghost()
                            .icon(ui::icon(IconName::EllipsisVertical))
                            .dropdown_menu(move |menu, window, cx| {
                                Self::host_menu(menu, weak.clone(), id, window, cx)
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
                        None if telnet && !os_known => this,
                        None => this.child(ui::pill(os_label, os_color)),
                    })
                    .when(telnet, |this| {
                        this.child(
                            h_flex()
                                .id(("host-telnet", ix))
                                .child(ui::pill("Telnet", theme.warning))
                                .tooltip(|window, cx| {
                                    Tooltip::new(t!("hosts.telnet_tooltip")).build(window, cx)
                                }),
                        )
                    })
                    .children(
                        host.tags
                            .iter()
                            .take(4)
                            .map(|t| ui::pill(t.clone(), theme.muted_foreground)),
                    ),
            )
            .when(
                vault_chip.is_some() || account_badge.is_some() || use_only,
                |this| {
                    this.child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .flex_wrap()
                            .when_some(account_badge, |this, row| {
                                this.child(
                                    h_flex()
                                        .id(("host-account", ix))
                                        .child(super::accounts::avatar(&row, 16.))
                                        .tooltip({
                                            let label = row.label();
                                            move |window, cx| {
                                                Tooltip::new(label.clone()).build(window, cx)
                                            }
                                        }),
                                )
                            })
                            .children(vault_chip)
                            .when(use_only, |this| {
                                this.child(
                                    h_flex()
                                        .id(("host-use-only", ix))
                                        .gap_1()
                                        .items_center()
                                        .child(ui::icon(IconName::Lock).size(px(11.)))
                                        .child(ui::pill(t!("vaults.use_only_badge"), theme.warning))
                                        .tooltip(|window, cx| {
                                            Tooltip::new(t!("vaults.use_only_tooltip"))
                                                .build(window, cx)
                                        }),
                                )
                            }),
                    )
                },
            )
            // Menu opened from the keyboard (Shift+F10, the menu key).
            .when_some(kbd_menu, |this, menu| {
                this.child(
                    div().absolute().top(px(40.)).left(px(24.)).child(
                        deferred(anchored().snap_to_window_with_margin(px(8.)).child(menu))
                            .with_priority(1),
                    ),
                )
            })
            .context_menu(move |menu, window, cx| {
                Self::host_menu(menu, weak_menu.clone(), id, window, cx)
            })
    }

    fn render_group_header(
        &self,
        group: Option<&Group>,
        hosts: Vec<Id>,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let count = hosts.len();
        let theme = cx.theme();
        let name = group
            .map(|g| g.name.clone())
            .unwrap_or_else(|| t!("hosts.ungrouped").to_string());
        let color = group
            .and_then(|g| g.color.as_deref().and_then(theme::parse_color))
            .unwrap_or_else(|| theme::color_for(&name));
        let weak = cx.entity().downgrade();
        let group_owned = group.cloned();
        let (menu_view, menu_group, menu_hosts) =
            (weak.clone(), group_owned.clone(), hosts.clone());
        h_flex()
            .id(("group-header", ix))
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
            .child(
                Button::new(("group-menu", ix))
                    .xsmall()
                    .ghost()
                    .icon(ui::icon(IconName::Ellipsis))
                    .dropdown_menu(move |menu, _, _| {
                        Self::group_menu(menu, weak.clone(), group_owned.clone(), hosts.clone())
                    }),
            )
            .context_menu(move |menu, _, _| {
                Self::group_menu(
                    menu,
                    menu_view.clone(),
                    menu_group.clone(),
                    menu_hosts.clone(),
                )
            })
    }
}

impl Focusable for HostsView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for HostsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let query = self.search.read(cx).value().trim().to_string();
        // Only what the vault picker lets through.
        let group_items: Vec<Item<Group>> = model
            .groups
            .iter()
            .filter(|g| model.in_filter(g))
            .cloned()
            .collect();
        let groups: Vec<Group> = group_items.iter().map(|g| g.data.clone()).collect();
        let hosts: Vec<Item<Host>> = model
            .hosts
            .iter()
            .filter(|h| model.in_filter(h))
            .cloned()
            .collect();
        let loaded = model.loaded;
        // Place (account → vault, This device last) of hosts and groups.
        let infos = model.account_infos();
        let all_vaults = model.all_vaults();
        let place_of =
            |scope: Scope, vault: Option<Id>| vm::place_of(&infos, &all_vaults, scope, vault);
        let host_place: Vec<(Place, Scope, Option<Id>)> = hosts
            .iter()
            .map(|h| {
                let v = model.vault_of(h);
                (place_of(h.scope, v), h.scope, v)
            })
            .collect();
        let group_place: Vec<Place> = group_items
            .iter()
            .map(|g| place_of(g.scope, model.vault_of(g)))
            .collect();
        let group_exists = |id: Option<Id>| id.is_some_and(|g| groups.iter().any(|x| x.id == g));

        // Search filter.
        let visible: Vec<(&Item<Host>, Place)> = hosts
            .iter()
            .zip(host_place.iter())
            .filter(|(h, _)| {
                let gname = h
                    .data
                    .group_id
                    .and_then(|g| groups.iter().find(|x| x.id == g))
                    .map(|g| g.name.as_str());
                Self::matches(&query, h, gname)
            })
            .map(|(h, p)| (h, p.0))
            .collect();
        let fav_count = visible.iter().filter(|(h, _)| h.data.favorite).count();
        let ungrouped_count = visible
            .iter()
            .filter(|(h, _)| !group_exists(h.data.group_id))
            .count();
        // Places in sight, in order, with their scope and vault.
        let mut places: Vec<(Place, Scope, Option<Id>)> = host_place.clone();
        for (g, p) in group_items.iter().zip(&group_place) {
            places.push((*p, g.scope, model.vault_of(g)));
        }
        places.sort_by_key(|p| p.0);
        places.dedup_by_key(|p| p.0);
        let place_labels: Vec<(
            String,
            Option<AccountRow>,
            Option<crate::accounts::VaultEntry>,
        )> = places
            .iter()
            .map(|(_, scope, vault)| {
                let badge = vm::show_account_badges(model.accounts_in_view().len())
                    .then(|| {
                        scope
                            .account()
                            .and_then(|a| model.account(a))
                            .map(|a| AccountRow::new(&a.info))
                    })
                    .flatten();
                let entry = scope
                    .account()
                    .zip(*vault)
                    .and_then(|(a, v)| model.vault_entry(a, v))
                    .cloned();
                (model.place_label(*scope, *vault), badge, entry)
            })
            .collect();

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
                .filter(|(h, _)| h.data.group_id == Some(g.id))
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

        // Sections by group according to the filter; with several places in
        // sight (accounts, vaults, This device), by place first.
        let mut sections: Vec<(Option<usize>, Option<Group>, Vec<&Item<Host>>)> = Vec::new();
        match self.filter {
            GroupFilter::Favorites => {
                sections.push((
                    None,
                    None,
                    visible
                        .iter()
                        .map(|(h, _)| *h)
                        .filter(|h| h.data.favorite)
                        .collect(),
                ));
            }
            GroupFilter::Group(gid) => {
                let g = groups.iter().find(|g| g.id == gid).cloned();
                sections.push((
                    None,
                    g,
                    visible
                        .iter()
                        .map(|(h, _)| *h)
                        .filter(|h| h.data.group_id == Some(gid))
                        .collect(),
                ));
            }
            GroupFilter::Ungrouped => {
                sections.push((
                    None,
                    None,
                    visible
                        .iter()
                        .map(|(h, _)| *h)
                        .filter(|h| !group_exists(h.data.group_id))
                        .collect(),
                ));
            }
            GroupFilter::All => {
                let by_place = places.len() > 1;
                for (pi, (place, _, _)) in places.iter().enumerate() {
                    let header = by_place.then_some(pi);
                    let start = sections.len();
                    for (g, gp) in groups.iter().zip(&group_place) {
                        if gp != place {
                            continue;
                        }
                        let list: Vec<&Item<Host>> = visible
                            .iter()
                            .filter(|(h, p)| p == place && h.data.group_id == Some(g.id))
                            .map(|(h, _)| *h)
                            .collect();
                        if !list.is_empty() || query.is_empty() {
                            sections.push((None, Some(g.clone()), list));
                        }
                    }
                    let rest: Vec<&Item<Host>> = visible
                        .iter()
                        .filter(|(h, p)| {
                            p == place
                                && !h.data.group_id.is_some_and(|g| {
                                    groups
                                        .iter()
                                        .zip(&group_place)
                                        .any(|(x, xp)| x.id == g && xp == place)
                                })
                        })
                        .map(|(h, _)| *h)
                        .collect();
                    if !rest.is_empty() {
                        sections.push((None, None, rest));
                    }
                    // The place header goes on its first section.
                    if let Some(first) = sections.get_mut(start) {
                        first.0 = header;
                    }
                }
            }
        }

        // Order on screen, for Shift ranges and the keyboard; what is no
        // longer shown leaves the selection.
        self.order = sections
            .iter()
            .flat_map(|(_, _, list)| list.iter().map(|h| h.data.id))
            .collect();
        let order = self.order.clone();
        self.selection.retain(&order);
        if self.cursor.is_some_and(|c| !order.contains(&c)) {
            self.cursor = None;
        }

        // Whether the hosts on screen answer: what each card shows, and
        // the ones that are due are checked (only while this renders).
        self.shown.clear();
        let status_on = self.model.read(cx).settings.host_status;
        if status_on {
            let checks: Vec<(Id, Result<Probe, Skip>)> = {
                let m = self.model.read(cx);
                order
                    .iter()
                    .filter_map(|id| m.host_record(*id))
                    .map(|rec| (rec.data.id, self.probe_of(rec, cx)))
                    .collect()
            };
            let status = self.status.read(cx);
            for (id, check) in &checks {
                let target = check.as_ref().map(|p| &p.target).map_err(|s| *s);
                self.shown.insert(*id, status.shown(*id, target));
            }
            let probes: Vec<Probe> = checks.into_iter().filter_map(|(_, c)| c.ok()).collect();
            let status = self.status.clone();
            cx.defer(move |cx| status.update(cx, |s, cx| s.tick(probes, cx)));
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
        for (si, (place, group, list)) in sections.iter().enumerate() {
            if let Some(pi) = place {
                let (label, badge, entry) = place_labels[*pi].clone();
                let theme = cx.theme();
                body = body.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .pb_1()
                        .border_b_1()
                        .border_color(theme.border)
                        .when_some(badge, |this, row| {
                            this.child(super::accounts::avatar(&row, 20.))
                        })
                        .map(|this| match &entry {
                            Some(v) => this.child(
                                ui::icon(super::vaults::vault_icon(&v.vault))
                                    .size(px(16.))
                                    .text_color(super::vaults::vault_color(&v.vault)),
                            ),
                            None if places[*pi].1 == Scope::Device => this.child(
                                ui::icon(IconName::Laptop)
                                    .size(px(16.))
                                    .text_color(theme.muted_foreground),
                            ),
                            None => this,
                        })
                        .child(div().text_lg().font_semibold().child(label))
                        .when_some(entry.filter(|v| !v.can_write()), |this, v| {
                            this.child(ui::pill(
                                if v.strict() {
                                    t!("vaults.use_only_strict_badge")
                                } else {
                                    t!("vaults.use_only_badge")
                                },
                                theme.warning,
                            ))
                        }),
                );
            }
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
                self.render_group_header(
                    title_group,
                    list.iter().map(|h| h.data.id).collect(),
                    si,
                    cx,
                )
                .into_any_element()
            };
            let mut grid = h_flex().flex_wrap().gap_3();
            for rec in list {
                grid = grid.child(self.render_card(rec, card_ix, window, cx));
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
                    .when(status_on, |this| {
                        this.child(
                            Button::new("check-status")
                                .icon(ui::icon(IconName::RefreshCw))
                                .tooltip(t!("host_status.check_now_tooltip"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    let ids = this.order.clone();
                                    this.check_now(ids, cx);
                                })),
                        )
                    })
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
                    .child({
                        let model = self.model.clone();
                        Button::new("import-export")
                            .icon(ui::icon(IconName::ArrowUpDown))
                            .label(t!("hosts.import_export"))
                            .dropdown_menu(move |menu, _, _| {
                                super::import_export::menu(menu, model.clone())
                            })
                    })
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
            .when(!self.selection.is_empty(), |this| {
                this.child(self.render_selection_bar(cx))
            })
            .child(
                div()
                    .id("host-list")
                    .key_context(CONTEXT)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::on_open_menu))
                    .on_action(cx.listener(Self::on_select_all))
                    .on_action(cx.listener(Self::on_clear_selection))
                    .on_action(cx.listener(Self::on_connect_focused))
                    .on_action(cx.listener(Self::on_delete_selected))
                    .on_action(cx.listener(Self::on_prev))
                    .on_action(cx.listener(Self::on_next))
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
