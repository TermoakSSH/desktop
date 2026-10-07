//! Command palette (Cmd+K on macOS, Ctrl+K or Ctrl+Shift+P elsewhere; also
//! View → Command palette): one search over the open tabs, the hosts, the
//! server sessions, the snippets and the actions of the app, run from the
//! keyboard. Matching and order are `crate::palette`; what each entry does
//! is what the rest of the window already does (the menus' actions, `open`,
//! the sections...).
//!
//! Keys: ↑/↓ choose, Enter runs, Cmd/Ctrl+Enter the second action (a host
//! in split view, a snippet pasted without running it), Alt+Enter SFTP (a
//! host), Shift+Enter edits (a host or a snippet), Esc closes.

use gpui::{
    Action, App, AppContext, ClickEvent, Context, Entity, FontWeight, HighlightStyle,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, StyledText, Subscription, WeakEntity, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::input::{
    Input, InputEvent, InputState, MoveDown, MovePageDown, MovePageUp, MoveUp,
};
use gpui_component::kbd::Kbd;
use gpui_component::{ActiveTheme, Sizable, WindowExt, h_flex, v_flex};
use termoak_core::Id;

use super::{AppView, CommandPalette, Section, TabContent};
use crate::accounts::{self, SwitcherEntry, VaultFilter, ViewMode};
use crate::menus;
use crate::palette::{self, Entry, Kind};
use crate::state::ToastKind;
use crate::ui::{self, IconName};
use crate::views::OpenRequest;
use crate::views::settings::SettingsPage;

/// Key context of the palette.
const CONTEXT: &str = "CommandPalette";
/// Rows shown at most (the best ones).
const MAX_ROWS: usize = 150;
/// Rows a page up or down moves.
const PAGE: usize = 8;

gpui::actions!(
    palette,
    [
        /// The third action of the chosen entry (SFTP for a host).
        Alternate
    ]
);

pub(super) fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("alt-enter", Alternate, Some(CONTEXT))]);
}

/// How an entry is used (Enter, Cmd/Ctrl+Enter, Alt+Enter, Shift+Enter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Use {
    Primary,
    Secondary,
    Alternate,
    Edit,
}

impl Use {
    fn keys(self) -> &'static str {
        let mac = cfg!(target_os = "macos");
        match (self, mac) {
            (Use::Primary, true) => "↵",
            (Use::Primary, false) => "Enter",
            (Use::Secondary, true) => "⌘↵",
            (Use::Secondary, false) => "Ctrl+Enter",
            (Use::Alternate, true) => "⌥↵",
            (Use::Alternate, false) => "Alt+Enter",
            (Use::Edit, true) => "⇧↵",
            (Use::Edit, false) => "Shift+Enter",
        }
    }
}

/// What an entry acts on.
pub(super) enum Target {
    /// A tab (by id).
    Tab(usize),
    Host(Id),
    Session {
        session_id: Id,
        title: String,
        account: Id,
    },
    Snippet(Id),
    /// An action of the menus.
    Action(Box<dyn Action>),
    Section(Section),
    SettingsPage(SettingsPage),
    Theme {
        dark: bool,
    },
    View(ViewMode),
    Vault(VaultFilter),
    AddAccount,
    JoinLink,
}

impl Clone for Target {
    fn clone(&self) -> Self {
        match self {
            Target::Tab(id) => Target::Tab(*id),
            Target::Host(id) => Target::Host(*id),
            Target::Session {
                session_id,
                title,
                account,
            } => Target::Session {
                session_id: *session_id,
                title: title.clone(),
                account: *account,
            },
            Target::Snippet(id) => Target::Snippet(*id),
            Target::Action(a) => Target::Action(a.boxed_clone()),
            Target::Section(s) => Target::Section(*s),
            Target::SettingsPage(p) => Target::SettingsPage(*p),
            Target::Theme { dark } => Target::Theme { dark: *dark },
            Target::View(v) => Target::View(*v),
            Target::Vault(f) => Target::Vault(*f),
            Target::AddAccount => Target::AddAccount,
            Target::JoinLink => Target::JoinLink,
        }
    }
}

/// An entry of the palette.
pub(super) struct Item {
    entry: Entry,
    target: Target,
    icon: IconName,
}

impl Item {
    fn new(kind: Kind, key: String, title: impl Into<String>, target: Target) -> Self {
        Self {
            entry: Entry {
                key,
                kind,
                title: title.into(),
                detail: String::new(),
                keywords: Vec::new(),
            },
            target,
            icon: match kind {
                Kind::Tab => IconName::Terminal,
                Kind::Host => IconName::Server,
                Kind::Session => IconName::Cloud,
                Kind::Snippet => IconName::SquareTerminal,
                Kind::Command => IconName::ChevronRight,
            },
        }
    }

    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.entry.detail = detail.into();
        self
    }

    fn keywords(mut self, words: impl IntoIterator<Item = String>) -> Self {
        self.entry.keywords.extend(words);
        self
    }

    fn icon(mut self, icon: IconName) -> Self {
        self.icon = icon;
        self
    }

    /// What it can do, with the label of each use (for the hints).
    fn uses(&self) -> Vec<(Use, SharedString)> {
        match self.entry.kind {
            Kind::Tab => vec![(Use::Primary, t!("palette.action.switch"))],
            Kind::Host => vec![
                (Use::Primary, t!("palette.action.connect")),
                (Use::Secondary, t!("palette.action.split")),
                (Use::Alternate, t!("palette.action.sftp")),
                (Use::Edit, t!("palette.action.edit")),
            ],
            Kind::Session => vec![(Use::Primary, t!("palette.action.attach"))],
            Kind::Snippet => vec![
                (Use::Primary, t!("palette.action.run")),
                (Use::Secondary, t!("palette.action.paste")),
                (Use::Edit, t!("palette.action.edit")),
            ],
            Kind::Command => vec![(Use::Primary, t!("palette.action.run_command"))],
        }
    }

    fn kind_label(&self) -> SharedString {
        match self.entry.kind {
            Kind::Tab => t!("palette.kind.tab"),
            Kind::Host => t!("palette.kind.host"),
            Kind::Session => t!("palette.kind.session"),
            Kind::Snippet => t!("palette.kind.snippet"),
            Kind::Command => t!("palette.kind.command"),
        }
    }
}

/// A menu label without its trailing ellipsis ("Find…" → "Find").
fn plain(label: &str) -> String {
    label
        .trim_end_matches('…')
        .trim_end_matches("...")
        .trim()
        .to_string()
}

/// The same words in English, so they are found in any language.
fn english(key: &str) -> Vec<String> {
    if crate::i18n::current() == crate::i18n::DEFAULT {
        return Vec::new();
    }
    vec![plain(&rust_i18n::t!(key, locale = "en"))]
}

impl AppView {
    /// Opens the palette (or closes it, from inside it: the same shortcut
    /// toggles it, see `PaletteView`).
    pub(super) fn on_command_palette(
        &mut self,
        _: &CommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Sessions as they are now (the list follows when they load).
        self.server_sessions
            .update(cx, |v, cx| v.refresh(window, cx));
        let items = self.palette_items(cx);
        let recent = self.model.read(cx).settings.palette_recent.clone();
        let app = cx.entity().downgrade();
        let sessions = self.server_sessions.clone();
        let view = cx.new(|cx| {
            let sub = cx.observe(&sessions, |p: &mut PaletteView, _, cx| p.reload(cx));
            PaletteView::new(app, items, recent, sub, window, cx)
        });
        window.open_dialog(cx, move |d, _, _| {
            let palette = view.clone();
            d.w(px(640.))
                .margin_top(px(72.))
                .close_button(false)
                // Enter in the search runs the chosen entry (the dialog gets
                // it as "OK"; running it closes the dialog).
                .on_ok(move |_, window, cx| {
                    palette.update(cx, |p, cx| p.confirm(Use::Primary, window, cx));
                    false
                })
                .child(view.clone())
        });
    }

    /// Everything the palette offers now.
    fn palette_items(&self, cx: &App) -> Vec<Item> {
        let mut items = Vec::new();
        let m = self.model.read(cx);

        // Open tabs.
        for (ix, tab) in self.tabs.iter().enumerate() {
            let detail = match &tab.content {
                TabContent::Terminal(p) if p.is_split() => {
                    tn!("palette.tab_split", p.items.len()).to_string()
                }
                TabContent::Terminal(_) => t!("palette.tab_terminal").to_string(),
                TabContent::Sftp(_) => t!("palette.tab_sftp").to_string(),
                TabContent::Dormant { .. } => t!("palette.tab_dormant").to_string(),
            };
            let detail = if self.active == Some(ix) {
                format!("{detail} · {}", t!("palette.tab_current"))
            } else {
                detail
            };
            let icon = match &tab.content {
                TabContent::Sftp(_) => IconName::FolderOpen,
                TabContent::Dormant { .. } => IconName::Cloud,
                TabContent::Terminal(p) if p.is_split() => IconName::LayoutGrid,
                TabContent::Terminal(_) => IconName::Terminal,
            };
            items.push(
                Item::new(
                    Kind::Tab,
                    format!("tab:{}", tab.id),
                    tab.title(cx).to_string(),
                    Target::Tab(tab.id),
                )
                .detail(detail)
                .icon(icon),
            );
        }

        // Hosts (those the vault picker shows).
        for h in m.hosts.iter().filter(|h| m.in_filter(h)) {
            let host = &h.data;
            let user = m.effective_user(host);
            let port = m.effective_port(host);
            let address = format!(
                "{}{}{}",
                user.map(|u| format!("{u}@")).unwrap_or_default(),
                host.address,
                if port == 22 {
                    String::new()
                } else {
                    format!(":{port}")
                }
            );
            items.push(
                Item::new(
                    Kind::Host,
                    format!("host:{}", host.id),
                    host.label.clone(),
                    Target::Host(host.id),
                )
                .detail(address)
                .keywords(host.tags.iter().cloned()),
            );
        }

        // Server sessions to attach to (not already in a tab).
        for s in self.server_sessions.read(cx).attachable() {
            if self
                .tabs
                .iter()
                .any(|t| t.server_sessions(cx).contains(&s.id))
            {
                continue;
            }
            items.push(
                Item::new(
                    Kind::Session,
                    format!("session:{}", s.id),
                    s.title.clone(),
                    Target::Session {
                        session_id: s.id,
                        title: s.title,
                        account: s.account,
                    },
                )
                .detail(s.detail),
            );
        }

        // Snippets.
        for s in m.snippets.iter().filter(|s| m.in_filter(s)) {
            let first = s
                .data
                .script
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            items.push(
                Item::new(
                    Kind::Snippet,
                    format!("snippet:{}", s.data.id),
                    s.data.name.clone(),
                    Target::Snippet(s.data.id),
                )
                .detail(first)
                .keywords(s.data.tags.iter().cloned()),
            );
        }

        // Actions of the menus that can be done now.
        let state = self.menu_state(cx);
        for spec in menus::spec(cfg!(target_os = "macos")) {
            let menu = menus::title(&spec).to_string();
            for e in spec.entries {
                let menus::Entry::Item {
                    label,
                    action,
                    need,
                } = e
                else {
                    continue;
                };
                if label == "menu.command_palette" || !state.allows(need) {
                    continue;
                }
                items.push(
                    Item::new(
                        Kind::Command,
                        format!("cmd:{label}"),
                        plain(&t!(label)),
                        Target::Action(action),
                    )
                    .detail(menu.clone())
                    .keywords(english(label)),
                );
            }
        }

        // Sections and settings pages.
        let mut sections = vec![
            Section::Hosts,
            Section::Keychain,
            Section::Snippets,
            Section::Forwards,
            Section::KnownHosts,
            Section::Ai,
            Section::ServerSessions,
            Section::Teams,
        ];
        if m.is_admin() {
            sections.push(Section::Admin);
        }
        for s in sections {
            items.push(
                Item::new(
                    Kind::Command,
                    format!("section:{s:?}"),
                    t!("palette.go_to", place = s.label()).to_string(),
                    Target::Section(s),
                )
                .detail(t!("palette.detail.sections").to_string())
                .icon(s.icon()),
            );
        }
        for (page, key) in [
            (SettingsPage::General, "settings.page.general"),
            (SettingsPage::Accounts, "settings.page.accounts"),
            (SettingsPage::Ai, "settings.page.ai"),
        ] {
            items.push(
                Item::new(
                    Kind::Command,
                    format!("settings:{page:?}"),
                    t!("palette.settings_page", page = t!(key)).to_string(),
                    Target::SettingsPage(page),
                )
                .detail(t!("app.section.settings").to_string())
                .keywords(english("app.section.settings"))
                .icon(IconName::Settings),
            );
        }

        // Theme.
        let dark = cx.theme().is_dark();
        items.push(
            Item::new(
                Kind::Command,
                "cmd:theme".into(),
                if dark {
                    t!("palette.theme_light")
                } else {
                    t!("palette.theme_dark")
                },
                Target::Theme { dark: !dark },
            )
            .detail(t!("settings.appearance.theme").to_string())
            .keywords([t!("palette.theme_keywords").to_string()])
            .icon(if dark { IconName::Sun } else { IconName::Moon }),
        );

        // Accounts (the switcher) and vaults (the vault picker).
        let infos = m.account_infos();
        for entry in accounts::switcher_entries(&infos, m.view) {
            let item = match entry {
                SwitcherEntry::Account { row, selected } if !selected => Item::new(
                    Kind::Command,
                    format!("view:{}", row.id),
                    t!("palette.switch_account", account = row.email.clone()),
                    Target::View(ViewMode::Account(row.id)),
                )
                .detail(row.server.clone().unwrap_or_default())
                .icon(IconName::User),
                SwitcherEntry::All { selected } if !selected => Item::new(
                    Kind::Command,
                    "view:all".into(),
                    t!("palette.show_all_accounts"),
                    Target::View(ViewMode::All),
                )
                .icon(IconName::Users),
                SwitcherEntry::Device { selected } if !selected => Item::new(
                    Kind::Command,
                    "view:device".into(),
                    t!("palette.show_device"),
                    Target::View(ViewMode::Device),
                )
                .icon(IconName::Laptop),
                SwitcherEntry::Add => Item::new(
                    Kind::Command,
                    "cmd:add_account".into(),
                    plain(&t!("accounts.add_menu")),
                    Target::AddAccount,
                )
                .icon(IconName::UserPlus),
                SwitcherEntry::Manage => Item::new(
                    Kind::Command,
                    "settings:Accounts".into(),
                    plain(&t!("accounts.manage_menu")),
                    Target::SettingsPage(SettingsPage::Accounts),
                )
                .icon(IconName::Settings),
                _ => continue,
            };
            // "Manage accounts" is the Accounts page, already listed.
            if items.iter().any(|i| i.entry.key == item.entry.key) {
                continue;
            }
            items.push(item.detail_if_empty(t!("palette.detail.accounts").to_string()));
        }
        let vaults = m.vaults_in_view();
        if accounts::show_vault_picker(vaults.len(), m.has_device_items()) {
            let filter = m.vault_filter;
            if filter != VaultFilter::All {
                items.push(
                    Item::new(
                        Kind::Command,
                        "vault:all".into(),
                        t!("palette.show_vault", vault = t!("vaults.picker.all")),
                        Target::Vault(VaultFilter::All),
                    )
                    .icon(IconName::Layers),
                );
            }
            for v in &vaults {
                let f = VaultFilter::Vault {
                    account: v.account,
                    vault: v.id(),
                };
                if f == filter {
                    continue;
                }
                let label = m.place_label(termoak_client::Scope::Account(v.account), Some(v.id()));
                items.push(
                    Item::new(
                        Kind::Command,
                        format!("vault:{}:{}", v.account, v.id()),
                        t!("palette.show_vault", vault = label),
                        Target::Vault(f),
                    )
                    .icon(crate::views::vaults::vault_icon(&v.vault)),
                );
            }
            if m.has_device_items() && filter != VaultFilter::Device {
                items.push(
                    Item::new(
                        Kind::Command,
                        "vault:device".into(),
                        t!("palette.show_vault", vault = t!("accounts.switcher.device")),
                        Target::Vault(VaultFilter::Device),
                    )
                    .icon(IconName::Laptop),
                );
            }
        }
        for item in items
            .iter_mut()
            .filter(|i| i.entry.key.starts_with("vault:"))
        {
            item.entry.detail = t!("palette.detail.vaults").to_string();
        }

        items.push(
            Item::new(
                Kind::Command,
                "cmd:join_link".into(),
                plain(&t!("join.title")),
                Target::JoinLink,
            )
            .icon(IconName::Link),
        );
        items
    }

    /// Runs an entry of the palette (already closed), and remembers it.
    fn run_palette(
        &mut self,
        key: String,
        target: Target,
        how: Use,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Tabs come and go: they are not remembered.
        if !key.starts_with("tab:") {
            self.model.update(cx, |m, cx| {
                let mut s = m.settings.clone();
                palette::remember(&mut s.palette_recent, &key);
                m.save_settings(s, cx);
            });
        }
        match (target, how) {
            (Target::Tab(id), _) => {
                if let Some(ix) = self.tab_index(id) {
                    self.activate(Some(ix), window, cx);
                }
            }
            (Target::Host(host_id), Use::Primary) => {
                self.open(OpenRequest::Local { host_id }, window, cx)
            }
            (Target::Host(host_id), Use::Secondary) => self.open(
                OpenRequest::Split {
                    hosts: vec![host_id],
                    current: true,
                },
                window,
                cx,
            ),
            (Target::Host(host_id), Use::Alternate) => self.open(
                OpenRequest::Sftp {
                    host_id,
                    conn: None,
                },
                window,
                cx,
            ),
            (Target::Host(host_id), Use::Edit) => {
                let rec = self.model.read(cx).host_record(host_id).cloned();
                if rec.is_some() {
                    self.select_section(Section::Hosts, window, cx);
                    self.hosts.update(cx, |h, cx| h.edit(rec, window, cx));
                }
            }
            (
                Target::Session {
                    session_id,
                    title,
                    account,
                },
                _,
            ) => self.open(
                OpenRequest::Attach {
                    session_id,
                    title,
                    account: Some(account),
                },
                window,
                cx,
            ),
            (Target::Snippet(id), Use::Edit) => {
                let rec = self
                    .model
                    .read(cx)
                    .snippets
                    .iter()
                    .find(|s| s.data.id == id)
                    .cloned();
                if rec.is_some() {
                    self.select_section(Section::Snippets, window, cx);
                    self.snippets.update(cx, |s, cx| s.edit(rec, window, cx));
                }
            }
            (Target::Snippet(id), how) => self.palette_snippet(id, how == Use::Primary, window, cx),
            (Target::Action(action), _) => window.dispatch_action(action, cx),
            (Target::Section(s), _) => self.select_section(s, window, cx),
            (Target::SettingsPage(page), _) => {
                self.select_section(Section::Settings, window, cx);
                self.settings
                    .update(cx, |s, cx| s.show_page(page, window, cx));
            }
            (Target::Theme { dark }, _) => {
                self.settings
                    .update(cx, |s, cx| s.set_dark(dark, window, cx));
            }
            (Target::View(view), _) => self.model.update(cx, |m, cx| m.set_view(view, cx)),
            (Target::Vault(filter), _) => self
                .model
                .update(cx, |m, cx| m.set_vault_filter(filter, cx)),
            (Target::AddAccount, _) => crate::views::add_account::open(
                self.model.clone(),
                crate::views::add_account::Start::Choose,
                window,
                cx,
            ),
            (Target::JoinLink, _) => self.open_join_dialog(None, window, cx),
        }
    }

    /// A snippet from the palette, typed into the focused terminal: run
    /// (Enter) or pasted without running it. With variables to fill in,
    /// the "Send snippet" dialog opens with it chosen.
    fn palette_snippet(&mut self, id: Id, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let writable = self
            .focused_terminal()
            .is_some_and(|t| t.read(cx).writable());
        if !writable {
            ui::notify(window, cx, ToastKind::Info, t!("palette.no_terminal"));
            return;
        }
        let Some(snippet) = self
            .model
            .read(cx)
            .snippets
            .iter()
            .find(|s| s.data.id == id)
            .map(|s| s.data.clone())
        else {
            return;
        };
        if !snippet.variables().is_empty() {
            self.open_send_snippet(Some(id), window, cx);
            return;
        }
        let text = crate::views::snippets::snippet_input(&snippet.script, run);
        self.send_snippet(&text, false, window, cx);
    }
}

impl Item {
    fn detail_if_empty(mut self, detail: String) -> Self {
        if self.entry.detail.is_empty() {
            self.entry.detail = detail;
        }
        self
    }
}

/// The palette: a search and the entries that match.
pub(super) struct PaletteView {
    app: WeakEntity<AppView>,
    items: Vec<Item>,
    recent: Vec<String>,
    search: Entity<InputState>,
    ranked: Vec<palette::Ranked>,
    selected: usize,
    scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

impl PaletteView {
    fn new(
        app: WeakEntity<AppView>,
        items: Vec<Item>,
        recent: Vec<String>,
        sessions: Subscription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("palette.placeholder")));
        ui::focus_later(&search, window, cx);
        let sub = cx.subscribe_in(
            &search,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    this.rank(cx);
                    this.select(0, cx);
                }
                // Plain Enter is the dialog's OK (see `on_command_palette`).
                InputEvent::PressEnter {
                    secondary: true, ..
                } => this.confirm(Use::Secondary, window, cx),
                InputEvent::PressEnter { shift: true, .. } => this.confirm(Use::Edit, window, cx),
                _ => {}
            },
        );
        let mut view = Self {
            app,
            items,
            recent,
            search,
            ranked: Vec::new(),
            selected: 0,
            scroll: ScrollHandle::new(),
            _subs: vec![sub, sessions],
        };
        view.rank(cx);
        view
    }

    /// The entries again (server sessions loaded), keeping the chosen one.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let chosen = self.chosen().map(|i| i.entry.key.clone());
        self.items = app.read(cx).palette_items(cx);
        self.rank(cx);
        let ix = chosen
            .and_then(|k| {
                self.ranked
                    .iter()
                    .position(|r| self.items[r.index].entry.key == k)
            })
            .unwrap_or(0);
        self.selected = ix;
        cx.notify();
    }

    fn rank(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).value().to_string();
        let entries: Vec<Entry> = self.items.iter().map(|i| i.entry.clone()).collect();
        self.ranked = palette::rank(&query, &entries, &self.recent);
        self.ranked.truncate(MAX_ROWS);
        cx.notify();
    }

    fn chosen(&self) -> Option<&Item> {
        self.ranked
            .get(self.selected)
            .and_then(|r| self.items.get(r.index))
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = ix.min(self.ranked.len().saturating_sub(1));
        self.scroll.scroll_to_item(self.selected);
        cx.notify();
    }

    /// Moves the choice (wrapping around with single steps).
    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.ranked.len();
        if len == 0 {
            return;
        }
        let next = if delta.unsigned_abs() == 1 {
            (self.selected as isize + delta).rem_euclid(len as isize) as usize
        } else {
            (self.selected as isize + delta).clamp(0, len as isize - 1) as usize
        };
        self.select(next, cx);
    }

    /// Runs the chosen entry (if it can be used that way).
    fn confirm(&mut self, how: Use, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.chosen() else {
            return;
        };
        if !item.uses().iter().any(|(u, _)| *u == how) {
            return;
        }
        let key = item.entry.key.clone();
        let target = item.target.clone();
        window.close_dialog(cx);
        if let Some(app) = self.app.upgrade() {
            app.update(cx, |app, cx| app.run_palette(key, target, how, window, cx));
        }
    }

    fn click(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = ix;
        self.confirm(Use::Primary, window, cx);
    }

    /// Shortcut of a menu action, wherever it is bound.
    fn shortcut(action: &dyn Action, window: &Window) -> Option<Kbd> {
        ["Workspace", "Terminal"]
            .into_iter()
            .find_map(|c| Kbd::binding_for_action(action, Some(c), window))
            .or_else(|| Kbd::global_binding_for_action(action, window))
    }
}

/// The title with the matched characters stressed.
fn title_text(title: &str, hits: &[usize], color: gpui::Hsla) -> StyledText {
    let style = HighlightStyle {
        color: Some(color),
        font_weight: Some(FontWeight::BOLD),
        ..Default::default()
    };
    let mut ranges: Vec<(std::ops::Range<usize>, HighlightStyle)> = Vec::new();
    for (ci, (bi, c)) in title.char_indices().enumerate() {
        if hits.binary_search(&ci).is_ok() {
            let range = bi..bi + c.len_utf8();
            match ranges.last_mut() {
                Some((last, _)) if last.end == range.start => last.end = range.end,
                _ => ranges.push((range, style)),
            }
        }
    }
    StyledText::new(SharedString::from(title.to_string())).with_highlights(ranges)
}

impl Render for PaletteView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let empty_query = self.search.read(cx).value().trim().is_empty();
        let recent_count = if empty_query {
            self.ranked
                .iter()
                .take_while(|r| self.recent.contains(&self.items[r.index].entry.key))
                .count()
        } else {
            0
        };
        let rows: Vec<gpui::AnyElement> = self
            .ranked
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let item = &self.items[r.index];
                let selected = i == self.selected;
                let right: gpui::AnyElement = match &item.target {
                    Target::Action(action) => match Self::shortcut(action.as_ref(), window) {
                        Some(kbd) => kbd.into_any_element(),
                        None => div().into_any_element(),
                    },
                    _ => div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(item.kind_label())
                        .into_any_element(),
                };
                h_flex()
                    .id(("palette-row", i))
                    .px_3()
                    .py_1p5()
                    .gap_3()
                    .items_center()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .when(selected, |this| this.bg(theme.list_active))
                    .when(!selected, |this| {
                        this.hover(|s| s.bg(theme.secondary_hover))
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.click(i, window, cx)
                    }))
                    .child(
                        ui::icon(item.icon.clone())
                            .size(px(16.))
                            .text_color(if selected {
                                theme.primary
                            } else {
                                theme.muted_foreground
                            }),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(title_text(&item.entry.title, &r.hits, theme.primary)),
                            )
                            .when(!item.entry.detail.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(item.entry.detail.clone()),
                                )
                            }),
                    )
                    .child(div().flex_shrink_0().child(right))
                    .into_any_element()
            })
            .collect();
        let hints = self.chosen().map(|item| item.uses()).unwrap_or_default();
        let count = self.ranked.len();
        v_flex()
            .key_context(CONTEXT)
            .gap_2()
            // The arrows move through the list, not the cursor of the search.
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                this.step(-1, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                this.step(1, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &MovePageUp, _, cx| {
                this.step(-(PAGE as isize), cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &MovePageDown, _, cx| {
                this.step(PAGE as isize, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &Alternate, window, cx| {
                this.confirm(Use::Alternate, window, cx)
            }))
            // The same shortcut closes it.
            .on_action(cx.listener(|_, _: &CommandPalette, window, cx| {
                window.close_dialog(cx);
            }))
            .child(
                Input::new(&self.search)
                    .large()
                    .prefix(ui::icon(IconName::Search).size(px(16.))),
            )
            .child(
                v_flex()
                    .id("palette-list")
                    .max_h(px(420.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .when(count == 0, |this| {
                        this.child(
                            div()
                                .p_4()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("palette.no_matches")),
                        )
                    })
                    .children(rows),
            )
            .child(
                h_flex()
                    .pt_1()
                    .gap_3()
                    .flex_wrap()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .when(recent_count > 0, |this| {
                        this.child(div().child(tn!("palette.recent_count", recent_count)))
                    })
                    .children(hints.into_iter().map(|(how, label)| {
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                div()
                                    .px_1()
                                    .rounded(theme.radius)
                                    .bg(theme.muted)
                                    .font_family(ui::mono_family(cx))
                                    .child(how.keys()),
                            )
                            .child(label)
                    }))
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                div()
                                    .px_1()
                                    .rounded(theme.radius)
                                    .bg(theme.muted)
                                    .font_family(ui::mono_family(cx))
                                    .child("Esc"),
                            )
                            .child(t!("palette.action.close")),
                    ),
            )
    }
}
