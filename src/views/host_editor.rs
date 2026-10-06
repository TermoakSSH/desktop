//! Host editor (side panel, in the style of Termius): the address first and
//! big, then the label, group, tags and color; the SSH credentials with a
//! "Connect" button at the top; and a collapsible "Advanced" section with
//! jumps, proxy, agent forwarding, keep-alive, startup snippet, environment,
//! recording, terminal type and theme. Errors are shown under each field.
//!
//! Keyboard: Enter saves, Ctrl+Enter (⌘↩ on macOS) saves and connects and
//! Escape closes the panel.
//!
//! The "Vault" field says where the host lives: chosen when creating it
//! (default: the vault of the picker, the last one used or the personal
//! one), read-only afterwards with "Move to…" and "Copy to…". Groups, keys,
//! identities, jumps and the startup snippet are offered from the same
//! vault (and This device). A Use-only host opens read-only, without its
//! secrets.

use std::collections::BTreeMap;

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::select::{Select, SelectEvent};
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme, Sizable, StyledExt, h_flex, v_flex};
use termoak_client::{SaveTarget, Scope};
use termoak_core::Id;
use termoak_core::model::{Host, HostSettings, ProxyKind, ProxySettings, SyncMode};
use termoak_core::transfer::TransferMode;

use super::OpenRequest;
use crate::accounts::{self as vm, Destination};
use crate::state::{AppModel, Item};
use crate::theme;
use crate::ui::{self, Choice, ChoiceState, IconName};

pub enum EditorEvent {
    Close,
    Open(OpenRequest),
}

/// Colors offered for a host (the same ones used for hosts without a color).
const HOST_COLORS: [&str; 8] = [
    "#4f7cff", "#30a46c", "#f5a524", "#e5484d", "#8e4ec6", "#0ea5e9", "#d6409f", "#12a594",
];

/// Form field that can show an error under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Field {
    Address,
    Port,
    Keepalive,
    Env,
    ProxyAddress,
    ProxyPort,
}

impl Field {
    /// Is the field inside the "Advanced" section?
    fn advanced(self) -> bool {
        matches!(
            self,
            Field::Keepalive | Field::Env | Field::ProxyAddress | Field::ProxyPort
        )
    }
}

/// Why a field is not valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FieldError {
    AddressMissing,
    AddressSpaces,
    Port,
    Keepalive,
    /// Line that is not `KEY=value`.
    Env(String),
    ProxyAddress,
    ProxyPort,
}

impl FieldError {
    pub(crate) fn field(&self) -> Field {
        match self {
            FieldError::AddressMissing | FieldError::AddressSpaces => Field::Address,
            FieldError::Port => Field::Port,
            FieldError::Keepalive => Field::Keepalive,
            FieldError::Env(_) => Field::Env,
            FieldError::ProxyAddress => Field::ProxyAddress,
            FieldError::ProxyPort => Field::ProxyPort,
        }
    }

    fn message(&self) -> SharedString {
        match self {
            FieldError::AddressMissing => t!("host_editor.error.address"),
            FieldError::AddressSpaces => t!("host_editor.error.address_spaces"),
            FieldError::Port => t!("host_editor.error.port"),
            FieldError::Keepalive => t!("host_editor.error.keepalive"),
            FieldError::Env(line) => t!("host_editor.error.env", line = line),
            FieldError::ProxyAddress => t!("host_editor.error.proxy_address"),
            FieldError::ProxyPort => t!("host_editor.error.proxy_port"),
        }
    }
}

/// Text of the form fields that need checking.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct FormText<'a> {
    pub label: &'a str,
    pub address: &'a str,
    pub port: &'a str,
    pub keepalive: &'a str,
    pub env: &'a str,
    pub tags: &'a str,
    /// Proxy address and port, if a proxy type is chosen.
    pub proxy: Option<(&'a str, &'a str)>,
}

/// Checked values of the form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FormValues {
    pub label: String,
    pub address: String,
    pub port: Option<u16>,
    pub keepalive: Option<u32>,
    pub env: BTreeMap<String, String>,
    pub tags: Vec<String>,
    pub proxy: Option<(String, u16)>,
}

/// A TCP port (1-65535).
fn parse_port(text: &str) -> Option<u16> {
    text.trim().parse::<u16>().ok().filter(|p| *p > 0)
}

/// `KEY=value` lines (blank lines are ignored); the first wrong line on error.
pub(crate) fn parse_env(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut env = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() && !k.trim().contains(char::is_whitespace) => {
                env.insert(k.trim().to_string(), v.trim().to_string());
            }
            _ => return Err(line.to_string()),
        }
    }
    Ok(env)
}

/// Comma-separated tags, without empty ones or repetitions.
pub(crate) fn parse_tags(text: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for tag in text.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        if !tags.iter().any(|t| t == tag) {
            tags.push(tag.to_string());
        }
    }
    tags
}

/// Optional text: `None` when empty.
fn optional(text: &str) -> Option<String> {
    Some(text.trim().to_string()).filter(|t| !t.is_empty())
}

/// Checks the form. Returns every error found, so all of them can be shown
/// at once under their fields.
pub(crate) fn parse_form(f: &FormText) -> Result<FormValues, Vec<FieldError>> {
    let mut errors = Vec::new();
    let address = f.address.trim().to_string();
    if address.is_empty() {
        errors.push(FieldError::AddressMissing);
    } else if address.contains(char::is_whitespace) {
        errors.push(FieldError::AddressSpaces);
    }
    // Without a label, the host is called by its address.
    let label = optional(f.label).unwrap_or_else(|| address.clone());
    let port = match f.port.trim() {
        "" => None,
        p => {
            let parsed = parse_port(p);
            if parsed.is_none() {
                errors.push(FieldError::Port);
            }
            parsed
        }
    };
    let keepalive = match f.keepalive.trim() {
        "" => None,
        k => match k.parse::<u32>() {
            Ok(k) => Some(k),
            Err(_) => {
                errors.push(FieldError::Keepalive);
                None
            }
        },
    };
    let env = parse_env(f.env).unwrap_or_else(|line| {
        errors.push(FieldError::Env(line));
        BTreeMap::new()
    });
    let proxy = match f.proxy {
        None => None,
        Some((host, port)) => {
            let host = host.trim().to_string();
            if host.is_empty() {
                errors.push(FieldError::ProxyAddress);
            }
            let port = parse_port(port);
            if port.is_none() {
                errors.push(FieldError::ProxyPort);
            }
            port.filter(|_| !host.is_empty()).map(|p| (host, p))
        }
    };
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(FormValues {
        label,
        address,
        port,
        keepalive,
        env,
        tags: parse_tags(f.tags),
        proxy,
    })
}

/// Does the host use anything of the "Advanced" section? (Then it starts open.)
pub(crate) fn has_advanced(s: &HostSettings) -> bool {
    s.jump_host_ids.as_ref().is_some_and(|j| !j.is_empty())
        || s.proxy.is_some()
        || s.agent_forwarding == Some(true)
        || s.keepalive_secs.is_some()
        || s.startup_snippet_id.is_some()
        || !s.env.is_empty()
        || s.record_sessions == Some(true)
        || s.term.as_deref().is_some_and(|t| !t.trim().is_empty())
        || s.theme.is_some()
}

/// Same color, written in any case and with or without `#`.
fn same_color(a: &str, b: &str) -> bool {
    a.trim()
        .trim_start_matches('#')
        .eq_ignore_ascii_case(b.trim().trim_start_matches('#'))
}

/// Where a new host goes, as a save target.
fn save_target(d: &Destination) -> SaveTarget {
    match d.scope {
        Scope::Device => SaveTarget::Device,
        Scope::Account(account) => SaveTarget::Account {
            account,
            vault: d.vault,
        },
    }
}

/// Index of the destination matching a save target.
fn destination_index(list: &[Destination], target: SaveTarget) -> usize {
    list.iter()
        .position(|d| match target {
            SaveTarget::Device => d.scope == Scope::Device,
            SaveTarget::Account { account, vault } => {
                d.scope == Scope::Account(account) && (vault.is_none() || d.vault == vault)
            }
            SaveTarget::Auto => false,
        })
        .unwrap_or(0)
}

/// Choices of the references of a host in a place: items of the same vault
/// and of This device (`None` first).
struct RefChoices {
    groups: Vec<Choice<Option<Id>>>,
    identities: Vec<Choice<Option<Id>>>,
    keys: Vec<Choice<Option<Id>>>,
    snippets: Vec<Choice<Option<Id>>>,
}

fn ref_choices(m: &AppModel, scope: Scope, vault: Option<Id>) -> RefChoices {
    let fits = |s: Scope, v: Option<Id>| s == Scope::Device || (s == scope && v == vault);
    let mut groups = vec![Choice::new(t!("host_editor.no_group"), None)];
    groups.extend(
        m.groups
            .iter()
            // A group must be in the host's own vault.
            .filter(|g| g.scope == scope && m.vault_of(g) == vault)
            .map(|g| Choice::new(g.data.name.clone(), Some(g.data.id))),
    );
    let mut identities = vec![Choice::new(t!("host_editor.no_identity"), None)];
    identities.extend(
        m.identities
            .iter()
            .filter(|i| fits(i.scope, m.vault_of(i)))
            .map(|i| {
                Choice::new(
                    format!("{} ({})", i.data.label, i.data.username),
                    Some(i.data.id),
                )
            }),
    );
    let mut keys = vec![Choice::new(t!("host_editor.no_key"), None)];
    keys.extend(
        m.keys
            .iter()
            .filter(|k| fits(k.scope, m.vault_of(k)))
            .map(|k| Choice::new(k.data.label.clone(), Some(k.data.id))),
    );
    let mut snippets = vec![Choice::new(t!("common.none"), None)];
    snippets.extend(
        m.snippets
            .iter()
            .filter(|s| fits(s.scope, m.vault_of(s)))
            .map(|sn| Choice::new(sn.data.name.clone(), Some(sn.data.id))),
    );
    RefChoices {
        groups,
        identities,
        keys,
        snippets,
    }
}

pub struct HostEditor {
    model: Entity<AppModel>,
    original: Option<Item<Host>>,
    /// New host: where it goes (`targets[i]`).
    targets: Vec<Destination>,
    target: Option<ChoiceState<usize>>,
    /// Where the host is (or will be): its scope and vault.
    place: (Scope, Option<Id>),
    /// Use-only host: it can be used but not seen or changed.
    read_only: bool,
    focus: FocusHandle,
    label: Entity<InputState>,
    address: Entity<InputState>,
    port: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    clear_password: bool,
    group: ChoiceState<Option<Id>>,
    identity: ChoiceState<Option<Id>>,
    key: ChoiceState<Option<Id>>,
    snippet: ChoiceState<Option<Id>>,
    /// Terminal theme of the host: `None` follows the app, `"dark"`, `"light"`.
    theme: ChoiceState<Option<String>>,
    term: Entity<InputState>,
    color: Option<String>,
    jumps: Vec<Id>,
    proxy_kind: ChoiceState<Option<ProxyKind>>,
    proxy_host: Entity<InputState>,
    proxy_port: Entity<InputState>,
    proxy_user: Entity<InputState>,
    proxy_password: Entity<InputState>,
    clear_proxy_password: bool,
    env: Entity<TextareaState>,
    keepalive: Entity<InputState>,
    tags: Entity<InputState>,
    notes: Entity<TextareaState>,
    device_only: bool,
    favorite: bool,
    agent_forwarding: bool,
    record: bool,
    /// The "Advanced" section is open.
    advanced_open: bool,
    /// Errors of the last save attempt, shown under their fields.
    errors: Vec<FieldError>,
    saving: bool,
    _subs: Vec<Subscription>,
}

impl EventEmitter<EditorEvent> for HostEditor {}

fn input(
    window: &mut Window,
    cx: &mut Context<HostEditor>,
    placeholder: SharedString,
    value: String,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(value)
    })
}

impl HostEditor {
    pub fn new(
        model: Entity<AppModel>,
        original: Option<Item<Host>>,
        default_group: Option<Id>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let h = original.as_ref().map(|r| r.data.clone());
        let s = h.as_ref().map(|h| h.settings.clone()).unwrap_or_default();
        let read_only = original.as_ref().is_some_and(|r| !r.access.can_write());
        let has_secret = original.as_ref().is_some_and(|r| r.meta.has_secret) && !read_only;
        let m = model.read(cx);

        // Where it is, or where a new one goes.
        let (targets, target_ix, place) = match &original {
            Some(r) => (Vec::new(), 0, (r.scope, m.vault_of(r))),
            None => {
                let accounts: Vec<_> = m
                    .accounts_in_view()
                    .into_iter()
                    .map(|a| a.info.clone())
                    .collect();
                let targets = vm::destinations(&accounts, &m.vaults_in_view(), true);
                let ix = destination_index(&targets, m.new_item_target());
                let place = targets
                    .get(ix)
                    .map(|d| {
                        let personal = d
                            .scope
                            .account()
                            .and_then(|a| m.account(a))
                            .and_then(|a| a.personal());
                        (d.scope, vm::effective_vault(d.scope, d.vault, personal))
                    })
                    .unwrap_or((Scope::Device, None));
                (targets, ix, place)
            }
        };
        let RefChoices {
            groups,
            identities,
            keys,
            snippets,
        } = ref_choices(m, place.0, place.1);
        let themes = vec![
            Choice::new(t!("host_editor.theme.follow"), None),
            Choice::new(t!("host_editor.theme.dark"), Some("dark".to_string())),
            Choice::new(t!("host_editor.theme.light"), Some("light".to_string())),
        ];

        let group_sel = h.as_ref().map(|h| h.group_id).unwrap_or(default_group);
        let env_text = s
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("\n");
        let notes_text = h.as_ref().map(|h| h.notes.clone()).unwrap_or_default();

        let label = input(
            window,
            cx,
            t!("host_editor.label_placeholder"),
            h.as_ref().map(|h| h.label.clone()).unwrap_or_default(),
        );
        let address = input(
            window,
            cx,
            t!("host_editor.address_placeholder"),
            h.as_ref().map(|h| h.address.clone()).unwrap_or_default(),
        );
        let port = input(
            window,
            cx,
            "22".into(),
            s.port.map(|p| p.to_string()).unwrap_or_default(),
        );
        let username = input(
            window,
            cx,
            t!("host_editor.username_placeholder"),
            s.username.clone().unwrap_or_default(),
        );
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(if has_secret {
                    t!("host_editor.password_saved")
                } else {
                    t!("host_editor.password_none")
                })
        });
        let keepalive = input(
            window,
            cx,
            "30".into(),
            s.keepalive_secs.map(|k| k.to_string()).unwrap_or_default(),
        );
        let tags = input(
            window,
            cx,
            t!("host_editor.tags_placeholder"),
            h.as_ref().map(|h| h.tags.join(", ")).unwrap_or_default(),
        );
        let term = input(
            window,
            cx,
            "xterm-256color".into(),
            s.term.clone().unwrap_or_default(),
        );
        let env = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 10)
                .placeholder(t!("host_editor.env_placeholder"))
                .default_value(env_text)
        });
        let notes = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 10)
                .placeholder(t!("host_editor.notes_placeholder"))
                .default_value(notes_text)
        });
        let proxy = s.proxy.clone();
        let proxy_kind = ui::choice_state(
            vec![
                Choice::new(t!("host_editor.proxy.none"), None),
                Choice::new("SOCKS5", Some(ProxyKind::Socks5)),
                Choice::new("SOCKS4", Some(ProxyKind::Socks4)),
                Choice::new("HTTP (CONNECT)", Some(ProxyKind::Http)),
            ],
            Some(&proxy.as_ref().map(|p| p.kind)),
            window,
            cx,
        );
        let proxy_host = input(
            window,
            cx,
            t!("host_editor.proxy.host_placeholder"),
            proxy.as_ref().map(|p| p.host.clone()).unwrap_or_default(),
        );
        let proxy_port = input(
            window,
            cx,
            "1080".into(),
            proxy
                .as_ref()
                .map(|p| p.port.to_string())
                .unwrap_or_default(),
        );
        let proxy_user = input(
            window,
            cx,
            t!("host_editor.optional"),
            proxy
                .as_ref()
                .and_then(|p| p.username.clone())
                .unwrap_or_default(),
        );
        let proxy_password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("host_editor.proxy.password_placeholder"))
        });
        // A default group of another vault does not apply.
        let group_sel = group_sel.filter(|g| groups.iter().any(|c| c.value == Some(*g)));
        let group = ui::choice_state(groups, Some(&group_sel), window, cx);
        let target = (targets.len() > 1).then(|| {
            let choices: Vec<Choice<usize>> = targets
                .iter()
                .enumerate()
                .map(|(i, d)| Choice::new(d.label.clone(), i))
                .collect();
            ui::choice_state(choices, Some(&target_ix), window, cx)
        });
        let identity = ui::choice_state(identities, Some(&s.identity_id), window, cx);
        let key = ui::choice_state(keys, Some(&s.key_id), window, cx);
        let snippet = ui::choice_state(snippets, Some(&s.startup_snippet_id), window, cx);
        let theme = ui::choice_state(themes, Some(&s.theme), window, cx);
        if original.is_none() {
            address.update(cx, |i, cx| i.focus(window, cx));
        }

        // Enter saves and Ctrl/Cmd+Enter saves and connects; editing a field
        // clears its error.
        let mut subs = Vec::new();
        let single_line: [(&Entity<InputState>, Option<Field>); 12] = [
            (&address, Some(Field::Address)),
            (&label, None),
            (&port, Some(Field::Port)),
            (&username, None),
            (&password, None),
            (&tags, None),
            (&keepalive, Some(Field::Keepalive)),
            (&term, None),
            (&proxy_host, Some(Field::ProxyAddress)),
            (&proxy_port, Some(Field::ProxyPort)),
            (&proxy_user, None),
            (&proxy_password, None),
        ];
        for (state, field) in single_line {
            subs.push(cx.subscribe_in(
                state,
                window,
                move |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter { secondary, .. } => this.save(*secondary, window, cx),
                    InputEvent::Change => {
                        if let Some(field) = field {
                            this.clear_error(field, cx);
                        }
                    }
                    _ => {}
                },
            ));
        }
        // In the text areas Enter is a new line: only Ctrl/Cmd+Enter acts.
        for (state, field) in [(&env, Some(Field::Env)), (&notes, None)] {
            subs.push(cx.subscribe_in(
                state,
                window,
                move |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => this.save(true, window, cx),
                    InputEvent::Change => {
                        if let Some(field) = field {
                            this.clear_error(field, cx);
                        }
                    }
                    _ => {}
                },
            ));
        }

        if let Some(target) = &target {
            subs.push(cx.subscribe_in(
                target,
                window,
                |this, _, _: &SelectEvent<Vec<Choice<usize>>>, window, cx| {
                    this.target_changed(window, cx)
                },
            ));
        }

        Self {
            targets,
            target,
            place,
            read_only,
            device_only: original
                .as_ref()
                .is_some_and(|r| r.meta.sync_mode == SyncMode::DeviceOnly),
            favorite: h.as_ref().is_some_and(|h| h.favorite),
            agent_forwarding: s.agent_forwarding.unwrap_or(false),
            record: s.record_sessions.unwrap_or(false),
            jumps: s.jump_host_ids.clone().unwrap_or_default(),
            advanced_open: has_advanced(&s),
            color: h.as_ref().and_then(|h| h.color.clone()),
            proxy_kind,
            proxy_host,
            proxy_port,
            proxy_user,
            proxy_password,
            clear_proxy_password: false,
            model,
            original,
            focus: cx.focus_handle(),
            label,
            address,
            port,
            username,
            password,
            clear_password: false,
            group,
            identity,
            key,
            snippet,
            theme,
            term,
            env,
            keepalive,
            tags,
            notes,
            errors: Vec::new(),
            saving: false,
            _subs: subs,
        }
    }

    pub fn host_id(&self) -> Option<Id> {
        self.original.as_ref().map(|r| r.data.id)
    }

    /// The chosen destination of a new host.
    fn destination(&self, cx: &App) -> Option<Destination> {
        match &self.target {
            Some(t) => ui::chosen(t, cx).and_then(|i| self.targets.get(i).cloned()),
            None => self.targets.first().cloned(),
        }
    }

    /// Another destination for a new host: its references come from there.
    fn target_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.destination(cx) else {
            return;
        };
        let place = {
            let m = self.model.read(cx);
            let personal = d
                .scope
                .account()
                .and_then(|a| m.account(a))
                .and_then(|a| a.personal());
            (d.scope, vm::effective_vault(d.scope, d.vault, personal))
        };
        if place == self.place {
            return;
        }
        self.place = place;
        let refs = ref_choices(self.model.read(cx), place.0, place.1);
        for (state, items) in [
            (&self.group, refs.groups),
            (&self.identity, refs.identities),
            (&self.key, refs.keys),
            (&self.snippet, refs.snippets),
        ] {
            state.update(cx, |s, cx| {
                s.set_items(items, window, cx);
                s.set_selected_value(&None, window, cx);
            });
        }
        // Jumps of another vault no longer apply.
        self.jumps.clear();
        cx.notify();
    }

    /// "Move to…" / "Copy to…" this host.
    fn transfer(&mut self, mode: TransferMode, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rec) = &self.original else {
            return;
        };
        // The host now lives elsewhere (maybe with another id): the editor
        // closes instead of saving over the old place.
        let me = cx.entity().downgrade();
        let done: super::transfer::OnDone = std::rc::Rc::new(move |_, cx| {
            if let Some(e) = me.upgrade() {
                e.update(cx, |_, cx| cx.emit(EditorEvent::Close));
            }
        });
        super::transfer::open(
            self.model.clone(),
            vec![rec.item_ref()],
            mode,
            Some(done),
            window,
            cx,
        );
    }

    fn clear_error(&mut self, field: Field, cx: &mut Context<Self>) {
        let before = self.errors.len();
        self.errors.retain(|e| e.field() != field);
        if self.errors.len() != before {
            cx.notify();
        }
    }

    fn error_for(&self, field: Field) -> Option<SharedString> {
        self.errors
            .iter()
            .find(|e| e.field() == field)
            .map(FieldError::message)
    }

    /// Builds the host from the form.
    #[allow(clippy::type_complexity)]
    fn build(
        &self,
        cx: &Context<Self>,
    ) -> Result<
        (
            Host,
            Option<Option<String>>,
            Option<Option<String>>,
            SyncMode,
        ),
        Vec<FieldError>,
    > {
        let text = |e: &Entity<InputState>| e.read(cx).value().to_string();
        let proxy_kind = ui::chosen(&self.proxy_kind, cx).flatten();
        let (label, address, port, keepalive, tags) = (
            text(&self.label),
            text(&self.address),
            text(&self.port),
            text(&self.keepalive),
            text(&self.tags),
        );
        let (proxy_host, proxy_port) = (text(&self.proxy_host), text(&self.proxy_port));
        let env_text = self.env.read(cx).value().to_string();
        let values = parse_form(&FormText {
            label: &label,
            address: &address,
            port: &port,
            keepalive: &keepalive,
            env: &env_text,
            tags: &tags,
            proxy: proxy_kind.map(|_| (proxy_host.as_str(), proxy_port.as_str())),
        })?;

        let mut host = self
            .original
            .as_ref()
            .map(|r| r.data.clone())
            .unwrap_or(Host {
                id: Id::nil(),
                label: String::new(),
                address: String::new(),
                group_id: None,
                tags: Vec::new(),
                settings: HostSettings::default(),
                notes: String::new(),
                color: None,
                os: None,
                os_version: None,
                favorite: false,
            });
        host.label = values.label;
        host.address = values.address;
        host.group_id = ui::chosen(&self.group, cx).flatten();
        host.tags = values.tags;
        host.notes = self.notes.read(cx).value().to_string();
        host.favorite = self.favorite;
        host.color = self.color.clone();
        let s = &mut host.settings;
        s.port = values.port;
        s.username = optional(&text(&self.username));
        s.identity_id = ui::chosen(&self.identity, cx).flatten();
        s.key_id = ui::chosen(&self.key, cx).flatten();
        s.startup_snippet_id = ui::chosen(&self.snippet, cx).flatten();
        s.jump_host_ids = (!self.jumps.is_empty()).then(|| self.jumps.clone());
        s.env = values.env;
        s.keepalive_secs = values.keepalive;
        s.agent_forwarding = self.agent_forwarding.then_some(true);
        s.record_sessions = self.record.then_some(true);
        s.term = optional(&text(&self.term));
        // A theme this version does not know (nothing selected) is kept.
        if let Some(theme) = ui::chosen(&self.theme, cx) {
            s.theme = theme;
        }
        s.proxy = match (proxy_kind, values.proxy) {
            (Some(kind), Some((host, port))) => Some(ProxySettings {
                kind,
                host,
                port,
                username: optional(&text(&self.proxy_user)),
            }),
            _ => None,
        };

        // `None`: keep; `Some(None)`: delete.
        let password = self.password.read(cx).value().to_string();
        let password = if !password.is_empty() {
            Some(Some(password))
        } else if self.clear_password {
            Some(None)
        } else {
            None
        };
        let proxy_password = self.proxy_password.read(cx).value().to_string();
        let proxy_password = if !proxy_password.is_empty() {
            Some(Some(proxy_password))
        } else if self.clear_proxy_password || s.proxy.is_none() {
            Some(None)
        } else {
            None
        };
        let mode = if self.device_only {
            SyncMode::DeviceOnly
        } else {
            SyncMode::Synced
        };
        Ok((host, password, proxy_password, mode))
    }

    fn save(&mut self, then_connect: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        if self.read_only {
            // Use only: nothing to save; it can still be used.
            if then_connect && let Some(id) = self.host_id() {
                cx.emit(EditorEvent::Open(OpenRequest::Local { host_id: id }));
            }
            return;
        }
        let (host, password, proxy_password, mode) = match self.build(cx) {
            Ok(v) => {
                self.errors.clear();
                v
            }
            Err(errors) => {
                // The field may be out of sight (or inside the closed
                // "Advanced" section): the first error also as a notice.
                if errors.iter().any(|e| e.field().advanced()) {
                    self.advanced_open = true;
                }
                if let Some(first) = errors.first() {
                    ui::error(window, cx, first.message());
                }
                self.errors = errors;
                cx.notify();
                return;
            }
        };
        self.saving = true;
        cx.notify();
        let target = if self.original.is_none() {
            self.destination(cx).as_ref().map(save_target)
        } else {
            None
        };
        // "This device only" applies to This device items; in an account
        // an item moves to This device with "Move to…".
        let mode = if self.place.0 == Scope::Device
            || self.original.is_none() && target == Some(SaveTarget::Device)
        {
            mode
        } else {
            SyncMode::Synced
        };
        let task = self.model.update(cx, |m, cx| {
            m.save_host(host, password, proxy_password, Some(mode), target, cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match res {
                    Ok(rec) => {
                        let id = rec.data.id;
                        let label = rec.data.label.clone();
                        let placeholder = if rec.meta.has_secret {
                            t!("host_editor.password_saved")
                        } else {
                            t!("host_editor.password_none")
                        };
                        this.original = Some(rec);
                        this.clear_password = false;
                        this.clear_proxy_password = false;
                        this.password.update(cx, |i, cx| {
                            i.set_value("", window, cx);
                            i.set_placeholder(placeholder, window, cx);
                        });
                        this.proxy_password
                            .update(cx, |i, cx| i.set_value("", window, cx));
                        ui::success(window, cx, t!("host_editor.saved", name = label));
                        if then_connect {
                            cx.emit(EditorEvent::Open(OpenRequest::Local { host_id: id }));
                        }
                    }
                    Err(e) => ui::error(window, cx, t!("host_editor.error.save", error = e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rec) = self.original.clone() else {
            return;
        };
        let model = self.model.clone();
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("host_editor.delete.title"),
            t!("host_editor.delete.message", name = rec.data.label),
            t!("host_editor.delete.ok"),
            true,
            move |window, cx| {
                let task = model.update(cx, |m, cx| m.delete::<Host>(rec.data.id, cx));
                let weak = weak.clone();
                window
                    .spawn(cx, async move |cx| {
                        let res = task.await;
                        let _ = cx.update(|window, cx| match res {
                            Ok(()) => {
                                if let Some(e) = weak.upgrade() {
                                    e.update(cx, |_, cx| cx.emit(EditorEvent::Close));
                                }
                            }
                            Err(e) => ui::error(window, cx, e),
                        });
                    })
                    .detach();
            },
        );
    }

    fn toggle_jump(&mut self, id: Id, cx: &mut Context<Self>) {
        if let Some(pos) = self.jumps.iter().position(|j| *j == id) {
            self.jumps.remove(pos);
        } else {
            self.jumps.push(id);
        }
        cx.notify();
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        if ev.keystroke.key == "escape" && !m.control && !m.alt && !m.shift && !m.platform {
            cx.stop_propagation();
            cx.emit(EditorEvent::Close);
        }
    }

    // ----- Rendering -----

    /// Field with its error (if any) under it.
    fn checked_field(
        &self,
        field: Field,
        label: SharedString,
        control: impl IntoElement,
        cx: &App,
    ) -> gpui::Div {
        let error = self.error_for(field);
        ui::field(label, control, cx).when_some(error, |this, e| this.child(error_text(e, cx)))
    }

    fn render_colors(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let selected = self.color.clone();
        let ring = theme.foreground;
        let border = theme.border;
        let muted = theme.muted_foreground;
        let none_selected = selected.is_none();
        h_flex()
            .gap_2()
            .flex_wrap()
            .items_center()
            .child(
                div()
                    .id("host-color-none")
                    .size(px(22.))
                    .rounded_full()
                    .border_2()
                    .border_color(if none_selected { ring } else { border })
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .child(ui::icon(IconName::Ban).size(px(12.)).text_color(muted))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.color = None;
                        cx.notify();
                    })),
            )
            .children(HOST_COLORS.iter().enumerate().map(|(i, hex)| {
                let color = theme::parse_color(hex).unwrap_or(muted);
                let active = selected.as_deref().is_some_and(|c| same_color(c, hex));
                div()
                    .id(("host-color", i))
                    .size(px(22.))
                    .rounded_full()
                    .border_2()
                    .border_color(if active { ring } else { color })
                    .bg(color)
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(active, |this| {
                        this.child(
                            ui::icon(IconName::Check)
                                .size(px(12.))
                                .text_color(gpui::white()),
                        )
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.color = Some(hex.to_string());
                        cx.notify();
                    }))
            }))
    }

    fn render_advanced(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let has_secret = self.original.as_ref().is_some_and(|r| r.meta.has_secret);
        let own_id = self.host_id();
        let (scope, vault) = self.place;
        let other_hosts: Vec<(Id, String)> = {
            let m = self.model.read(cx);
            m.hosts
                .iter()
                .filter(|h| Some(h.data.id) != own_id)
                // Jumps of the same vault, or of This device.
                .filter(|h| {
                    h.scope == Scope::Device || (h.scope == scope && m.vault_of(h) == vault)
                })
                .map(|h| (h.data.id, h.data.label.clone()))
                .collect()
        };
        let jumps: AnyElement = if other_hosts.is_empty() {
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("host_editor.jumps_none"))
                .into_any_element()
        } else {
            v_flex()
                .id("host-jumps")
                .gap_1()
                .max_h(px(160.))
                .overflow_y_scrollbar()
                .children(other_hosts.iter().enumerate().map(|(i, (id, label))| {
                    let id = *id;
                    let pos = self.jumps.iter().position(|j| *j == id);
                    Checkbox::new(("jump", i))
                        .label(match pos {
                            Some(p) => format!("{}. {label}", p + 1),
                            None => label.clone(),
                        })
                        .checked(pos.is_some())
                        .on_click(
                            cx.listener(move |this, _: &bool, _, cx| this.toggle_jump(id, cx)),
                        )
                }))
                .into_any_element()
        };
        let proxy_on = ui::chosen(&self.proxy_kind, cx).flatten().is_some();

        v_flex()
            .gap_4()
            // Connection: jumps and proxy.
            .child(sub_title(t!("host_editor.section.connection"), cx))
            .child(ui::field_with_hint(
                t!("host_editor.jumps"),
                jumps,
                t!("host_editor.jumps_hint"),
                cx,
            ))
            .child(ui::field_with_hint(
                t!("host_editor.proxy.kind"),
                Select::new(&self.proxy_kind),
                t!("host_editor.proxy.kind_hint"),
                cx,
            ))
            .when(proxy_on, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .items_start()
                        .child(div().flex_1().min_w_0().child(self.checked_field(
                            Field::ProxyAddress,
                            t!("host_editor.proxy.address"),
                            Input::new(&self.proxy_host),
                            cx,
                        )))
                        .child(div().w(px(96.)).flex_shrink_0().child(self.checked_field(
                            Field::ProxyPort,
                            t!("host_editor.port"),
                            Input::new(&self.proxy_port),
                            cx,
                        ))),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().min_w_0().child(ui::field(
                            t!("host_editor.username"),
                            Input::new(&self.proxy_user),
                            cx,
                        )))
                        .child(div().flex_1().min_w_0().child(ui::field(
                            t!("host_editor.password"),
                            Input::new(&self.proxy_password).mask_toggle(),
                            cx,
                        ))),
                )
                .when(has_secret, |this| {
                    this.child(
                        Checkbox::new("clear-proxy-password")
                            .label(t!("host_editor.proxy.clear_password"))
                            .checked(self.clear_proxy_password)
                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                this.clear_proxy_password = *v;
                                cx.notify();
                            })),
                    )
                })
            })
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Switch::new("agent-forwarding")
                            .label(t!("host_editor.agent_forwarding"))
                            .checked(self.agent_forwarding)
                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                this.agent_forwarding = *v;
                                cx.notify();
                            })),
                    )
                    .child(hint(t!("host_editor.agent_forwarding_hint"), cx)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(self.checked_field(
                        Field::Keepalive,
                        t!("host_editor.keepalive"),
                        div().w(px(120.)).child(Input::new(&self.keepalive)),
                        cx,
                    ))
                    .child(hint(t!("host_editor.keepalive_hint"), cx)),
            )
            // Terminal: startup, environment, recording, TERM and theme.
            .child(sub_title(t!("host_editor.section.terminal"), cx))
            .child(ui::field(
                t!("host_editor.startup_snippet"),
                Select::new(&self.snippet),
                cx,
            ))
            .child(self.checked_field(
                Field::Env,
                t!("host_editor.env"),
                Textarea::new(&self.env),
                cx,
            ))
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(div().flex_1().min_w_0().child(ui::field_with_hint(
                        t!("host_editor.term"),
                        Input::new(&self.term),
                        t!("host_editor.term_hint"),
                        cx,
                    )))
                    .child(div().flex_1().min_w_0().child(ui::field(
                        t!("host_editor.theme"),
                        Select::new(&self.theme),
                        cx,
                    ))),
            )
            .child(
                Switch::new("record")
                    .label(t!("host_editor.record"))
                    .checked(self.record)
                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                        this.record = *v;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }
}

impl Render for HostEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editing = self.original.is_some();
        let read_only = self.read_only;
        let has_secret = self.original.as_ref().is_some_and(|r| r.meta.has_secret) && !read_only;
        // The "Vault" field: where it is (with Move/Copy) or where it goes.
        let (place_label, caps, strict) = {
            let m = self.model.read(cx);
            match &self.original {
                Some(r) => (
                    m.place_label(r.scope, m.vault_of(r)),
                    Some(m.caps_of(r)),
                    m.vault_entry_of(r).is_some_and(|v| v.strict()),
                ),
                None => (String::new(), None, false),
            }
        };
        let show_place = editing || self.target.is_some();
        let device_place = self.place.0 == Scope::Device;
        let title: SharedString = if editing {
            self.original
                .as_ref()
                .map(|r| r.data.label.clone())
                .unwrap_or_default()
                .into()
        } else {
            t!("host_editor.new_title")
        };
        let colors = self.render_colors(cx).into_any_element();
        let advanced = self.advanced_open.then(|| self.render_advanced(cx));
        let advanced_open = self.advanced_open;
        let shortcut = if cfg!(target_os = "macos") {
            SharedString::from("⌘↩")
        } else {
            t!("host_editor.shortcut.save_connect")
        };
        let theme = cx.theme();

        // Header: title, Connect and close.
        let header = h_flex()
            .px_4()
            .py_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(
                ui::icon(if editing {
                    IconName::Pencil
                } else {
                    IconName::ServerPlus
                })
                .size(px(16.))
                .flex_shrink_0(),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_semibold()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(title),
            )
            .child(
                Button::new("editor-connect")
                    .small()
                    .primary()
                    .icon(ui::icon(IconName::SquareTerminal))
                    .label(t!("host_editor.connect"))
                    .tooltip(t!(
                        "host_editor.connect_tooltip",
                        shortcut = shortcut.clone()
                    ))
                    .loading(self.saving)
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.save(true, window, cx)),
                    ),
            )
            .child(
                Button::new("editor-close")
                    .small()
                    .ghost()
                    .icon(ui::icon(IconName::X))
                    .tooltip(t!("host_editor.close_tooltip"))
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(EditorEvent::Close))),
            );

        // Address (big) and label.
        let address_error = self.error_for(Field::Address);
        let identity_block = v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .w_full()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(t!("host_editor.address")),
                    )
                    .child(
                        Input::new(&self.address)
                            .large()
                            .prefix(ui::icon(IconName::Server).size(px(16.))),
                    )
                    .when_some(address_error, |this, e| this.child(error_text(e, cx))),
            )
            .child(ui::field(
                t!("host_editor.label"),
                Input::new(&self.label),
                cx,
            ));

        let place_field = show_place.then(|| {
            let control: AnyElement = match (&self.target, editing) {
                (Some(target), false) => Select::new(target).into_any_element(),
                _ => h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(place_label.clone()),
                    )
                    .when(caps.is_some_and(|c| c.move_out), |this| {
                        this.child(
                            Button::new("host-move-to")
                                .xsmall()
                                .icon(ui::icon(IconName::ArrowRightLeft))
                                .label(t!("hosts.menu.move_to"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.transfer(TransferMode::Move, window, cx)
                                })),
                        )
                    })
                    .when(caps.is_some_and(|c| c.copy_out), |this| {
                        this.child(
                            Button::new("host-copy-to")
                                .xsmall()
                                .ghost()
                                .icon(ui::icon(IconName::CopyPlus))
                                .label(t!("hosts.menu.copy_to"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.transfer(TransferMode::Copy, window, cx)
                                })),
                        )
                    })
                    .into_any_element(),
            };
            ui::field(t!("host_editor.vault"), control, cx)
        });

        // Organization: vault, group, tags and color.
        let organize = section(
            t!("host_editor.section.general"),
            IconName::Folder,
            v_flex()
                .gap_3()
                .children(place_field)
                .child(
                    h_flex()
                        .gap_2()
                        .items_start()
                        .child(div().flex_1().min_w_0().child(ui::field(
                            t!("host_editor.group"),
                            Select::new(&self.group),
                            cx,
                        )))
                        .child(div().flex_1().min_w_0().child(ui::field(
                            t!("host_editor.tags"),
                            Input::new(&self.tags),
                            cx,
                        ))),
                )
                .child(ui::field(t!("host_editor.color"), colors, cx)),
            cx,
        );

        // SSH: port, user and credentials.
        let ssh = section(
            t!("host_editor.section.ssh"),
            IconName::KeyRound,
            v_flex()
                .gap_3()
                .child(
                    h_flex()
                        .gap_2()
                        .items_start()
                        .child(div().flex_1().min_w_0().child(ui::field(
                            t!("host_editor.username"),
                            Input::new(&self.username),
                            cx,
                        )))
                        .child(div().w(px(96.)).flex_shrink_0().child(self.checked_field(
                            Field::Port,
                            t!("host_editor.port"),
                            Input::new(&self.port),
                            cx,
                        ))),
                )
                .when(!read_only, |this| {
                    this.child(ui::field(
                        t!("host_editor.password"),
                        Input::new(&self.password).mask_toggle(),
                        cx,
                    ))
                })
                .when(has_secret, |this| {
                    this.child(
                        Checkbox::new("clear-password")
                            .label(t!("host_editor.clear_password"))
                            .checked(self.clear_password)
                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                this.clear_password = *v;
                                cx.notify();
                            })),
                    )
                })
                .child(ui::field_with_hint(
                    t!("host_editor.key"),
                    Select::new(&self.key),
                    t!("host_editor.key_hint"),
                    cx,
                ))
                .child(ui::field_with_hint(
                    t!("host_editor.identity"),
                    Select::new(&self.identity),
                    t!("host_editor.identity_hint"),
                    cx,
                )),
            cx,
        );

        // Advanced (collapsible).
        let advanced_errors = self.errors.iter().any(|e| e.field().advanced());
        let advanced_header = h_flex()
            .id("host-advanced-toggle")
            .gap_2()
            .items_center()
            .cursor_pointer()
            .text_color(theme.muted_foreground)
            .hover(|s| s.text_color(theme.foreground))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.advanced_open = !this.advanced_open;
                cx.notify();
            }))
            .child(
                ui::icon(if advanced_open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(px(14.)),
            )
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .child(t!("host_editor.section.advanced").to_uppercase()),
            )
            .when(advanced_errors && !advanced_open, |this| {
                this.child(
                    ui::icon(IconName::CircleAlert)
                        .size(px(14.))
                        .text_color(theme.danger),
                )
            })
            .when(!advanced_open, |this| {
                this.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_xs()
                        .child(t!("host_editor.advanced_summary")),
                )
            });
        let advanced_block = v_flex()
            .gap_2()
            .child(advanced_header)
            .when_some(advanced, |this, content| {
                this.child(card(cx).child(content))
            });

        // Notes, favorite and "this device only".
        let other = section(
            t!("host_editor.section.organization"),
            IconName::Star,
            v_flex()
                .gap_3()
                .child(ui::field(
                    t!("host_editor.notes"),
                    Textarea::new(&self.notes),
                    cx,
                ))
                .child(
                    Switch::new("favorite")
                        .label(t!("host_editor.favorite"))
                        .checked(self.favorite)
                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                            this.favorite = *v;
                            cx.notify();
                        })),
                )
                .when(device_place, |this| {
                    this.child(
                        v_flex()
                            .gap_1()
                            .child(
                                Switch::new("device-only")
                                    .label(t!("host_editor.device_only"))
                                    .checked(self.device_only)
                                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                                        this.device_only = *v;
                                        cx.notify();
                                    })),
                            )
                            .child(hint(t!("host_editor.device_only_hint"), cx)),
                    )
                }),
            cx,
        );

        let footer = h_flex()
            .p_3()
            .gap_2()
            .items_center()
            .border_t_1()
            .border_color(theme.border)
            .when(editing && !read_only, |this| {
                this.child(
                    Button::new("delete-host")
                        .ghost()
                        .icon(ui::icon(IconName::Trash))
                        .tooltip(t!("host_editor.delete.title"))
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| this.delete(window, cx)),
                        ),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("host_editor.keyboard_hint", connect = shortcut)),
            )
            .when(!read_only, |this| {
                this.child(
                    Button::new("save-host")
                        .primary()
                        .icon(ui::icon(IconName::Save))
                        .label(t!("common.save"))
                        .loading(self.saving)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.save(false, window, cx)
                        })),
                )
            });

        v_flex()
            .id("host-editor")
            .key_context("HostEditor")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .child(header)
            .child(
                div().flex_1().min_h_0().child(
                    v_flex()
                        .id("host-editor-scroll")
                        .size_full()
                        .overflow_y_scrollbar()
                        .child(
                            v_flex()
                                .p_4()
                                .gap_5()
                                .when(read_only, |this| {
                                    this.child(
                                        h_flex()
                                            .gap_2()
                                            .items_start()
                                            .p_3()
                                            .rounded(theme.radius_lg)
                                            .border_1()
                                            .border_color(theme.warning)
                                            .child(
                                                ui::icon(IconName::Lock)
                                                    .size(px(16.))
                                                    .text_color(theme.warning),
                                            )
                                            .child(div().text_sm().child(if strict {
                                                t!("host_editor.use_only_strict")
                                            } else {
                                                t!("host_editor.use_only")
                                            })),
                                    )
                                })
                                .child(identity_block)
                                .child(ssh)
                                .child(organize)
                                .child(advanced_block)
                                .child(other),
                        ),
                ),
            )
            .child(footer)
    }
}

/// Section: small uppercase title with an icon over a card.
fn section(title: SharedString, icon: IconName, content: impl IntoElement, cx: &App) -> gpui::Div {
    v_flex()
        .gap_2()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .text_color(cx.theme().muted_foreground)
                .child(ui::icon(icon).size(px(14.)))
                .child(div().text_xs().font_semibold().child(title.to_uppercase())),
        )
        .child(card(cx).child(content))
}

/// Box of a section.
fn card(cx: &App) -> gpui::Div {
    v_flex()
        .w_full()
        .p_3()
        .gap_3()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
}

/// Title of a group of fields inside a card.
fn sub_title(text: SharedString, cx: &App) -> gpui::Div {
    div()
        .text_xs()
        .font_semibold()
        .text_color(cx.theme().muted_foreground)
        .child(text.to_uppercase())
}

fn hint(text: SharedString, cx: &App) -> gpui::Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

fn error_text(text: SharedString, cx: &App) -> gpui::Div {
    h_flex()
        .gap_1()
        .items_center()
        .text_xs()
        .text_color(cx.theme().danger)
        .child(
            ui::icon(IconName::CircleAlert)
                .size(px(12.))
                .flex_shrink_0(),
        )
        .child(div().min_w_0().child(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form<'a>(address: &'a str) -> FormText<'a> {
        FormText {
            address,
            ..FormText::default()
        }
    }

    #[test]
    fn the_label_defaults_to_the_address() {
        let v = parse_form(&form(" example.com ")).unwrap();
        assert_eq!(v.label, "example.com");
        assert_eq!(v.address, "example.com");
        let v = parse_form(&FormText {
            label: "  Web  ",
            ..form("10.0.0.1")
        })
        .unwrap();
        assert_eq!(v.label, "Web");
    }

    #[test]
    fn address_is_required_and_has_no_spaces() {
        assert_eq!(
            parse_form(&form("  ")).unwrap_err(),
            vec![FieldError::AddressMissing]
        );
        assert_eq!(
            parse_form(&form("my host")).unwrap_err(),
            vec![FieldError::AddressSpaces]
        );
    }

    #[test]
    fn ports() {
        assert_eq!(parse_form(&form("h")).unwrap().port, None);
        let with = |port| parse_form(&FormText { port, ..form("h") });
        assert_eq!(with("2222").unwrap().port, Some(2222));
        assert_eq!(with(" 65535 ").unwrap().port, Some(65535));
        for bad in ["0", "65536", "-1", "ssh"] {
            assert_eq!(with(bad).unwrap_err(), vec![FieldError::Port], "{bad}");
        }
    }

    #[test]
    fn keepalive() {
        let with = |keepalive| {
            parse_form(&FormText {
                keepalive,
                ..form("h")
            })
        };
        assert_eq!(with("").unwrap().keepalive, None);
        assert_eq!(with("0").unwrap().keepalive, Some(0));
        assert_eq!(with("abc").unwrap_err(), vec![FieldError::Keepalive]);
    }

    #[test]
    fn environment() {
        let env = parse_env("A=1\n\n  B = two words \nC=").unwrap();
        assert_eq!(env.get("A").map(String::as_str), Some("1"));
        assert_eq!(env.get("B").map(String::as_str), Some("two words"));
        assert_eq!(env.get("C").map(String::as_str), Some(""));
        assert_eq!(parse_env("A=1\nnot a pair").unwrap_err(), "not a pair");
        assert_eq!(parse_env("=x").unwrap_err(), "=x");
        assert_eq!(parse_env("MY VAR=x").unwrap_err(), "MY VAR=x");
    }

    #[test]
    fn tags_without_blanks_or_repeats() {
        assert_eq!(parse_tags(" web, ,db,web ,"), vec!["web", "db"]);
        assert!(parse_tags("").is_empty());
    }

    #[test]
    fn proxy_is_checked_only_when_chosen() {
        assert_eq!(parse_form(&form("h")).unwrap().proxy, None);
        let with = |host, port| {
            parse_form(&FormText {
                proxy: Some((host, port)),
                ..form("h")
            })
        };
        assert_eq!(
            with("proxy", "1080").unwrap().proxy,
            Some(("proxy".to_string(), 1080))
        );
        assert_eq!(
            with("", "0").unwrap_err(),
            vec![FieldError::ProxyAddress, FieldError::ProxyPort]
        );
    }

    #[test]
    fn every_error_is_reported_at_once() {
        let errors = parse_form(&FormText {
            address: "",
            port: "x",
            keepalive: "y",
            env: "bad",
            ..FormText::default()
        })
        .unwrap_err();
        let fields: Vec<Field> = errors.iter().map(FieldError::field).collect();
        assert_eq!(
            fields,
            vec![Field::Address, Field::Port, Field::Keepalive, Field::Env]
        );
        assert!(Field::Env.advanced() && !Field::Port.advanced());
    }

    #[test]
    fn advanced_section_opens_for_hosts_that_use_it() {
        let mut s = HostSettings::default();
        assert!(!has_advanced(&s));
        s.term = Some("  ".into());
        assert!(!has_advanced(&s));
        s.theme = Some("light".into());
        assert!(has_advanced(&s));
        let mut s = HostSettings::default();
        s.jump_host_ids = Some(Vec::new());
        assert!(!has_advanced(&s));
        s.keepalive_secs = Some(0);
        assert!(has_advanced(&s));
    }

    #[test]
    fn colors_compare_loosely() {
        assert!(same_color("#4F7CFF", "#4f7cff"));
        assert!(same_color("4f7cff", "#4f7cff"));
        assert!(!same_color("#4f7cff", "#30a46c"));
        assert!(HOST_COLORS.iter().all(|c| theme::parse_color(c).is_some()));
    }
}
