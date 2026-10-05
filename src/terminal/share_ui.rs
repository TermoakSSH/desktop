//! Live sharing inside a terminal tab: who is in the session and who has
//! the keyboard (participants button and popover), the banners to ask for
//! the keyboard, give it back or take it back, the owner's requests to
//! answer (letting people in, handing over the keyboard, for a while or
//! until taken back), the time left of a timed hand-over, the waiting room
//! and the screen shown when the server sends you away.
//!
//! The same view works for a server session (as owner or guest) and for a
//! terminal of this computer shared through the server (relay: always the
//! owner).

use std::collections::HashSet;
use std::time::Duration;

use gpui::{
    Anchor, AnyElement, App, ClickEvent, Context, Div, Hsla, IntoElement, ParentElement,
    SharedString, Styled, WeakEntity, div, hsla, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::popover::Popover;
use gpui_component::spinner::Spinner;
use gpui_component::{ActiveTheme, Sizable, StyledExt, h_flex, v_flex};
use termoak_client::relay::{RelayEvent, RelayShare};
use termoak_client::remote::Participant;
use termoak_core::Id;

use super::backend::{Cmd, RemoteSeat, ShareAction};
use super::{PaneAction, TermState, TerminalEvent, TerminalView};
use crate::runtime;
use crate::sharing::{self, AVATAR_COLORS, ParticipantRow, Role};
use crate::state::ToastKind;
use crate::ui::{self, IconName};

/// What a request from a participant is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestKind {
    /// Wants to be let in (waiting room).
    Join,
    /// Asks for the keyboard.
    Control,
}

/// A request the owner has to answer (also shown as a toast by the window
/// when this tab is not in view).
#[derive(Debug, Clone)]
pub struct ShareRequest {
    pub kind: RequestKind,
    pub participant: Id,
    pub name: String,
    /// Name of the session (tab).
    pub title: String,
}

impl ShareRequest {
    pub fn text(&self) -> SharedString {
        match self.kind {
            RequestKind::Join => t!("share.request.join", name = self.name),
            RequestKind::Control => t!("share.request.control", name = self.name),
        }
    }
}

/// Live sharing state of a terminal.
#[derive(Default)]
pub(super) struct ShareState {
    /// Known (after `hello`, or sharing a terminal of this computer).
    pub known: bool,
    pub owner: bool,
    /// `owner`, `control` (can ask for the keyboard) or `view`.
    pub access: String,
    pub participant: Option<Id>,
    pub participants: Vec<Participant>,
    /// Participant with the keyboard (`None`: the owner).
    pub driver: Option<Id>,
    pub driver_name: Option<String>,
    /// End of the driver's timed grant (ms; `None`: not timed).
    pub until: Option<i64>,
    /// Repaints every second while a grant is timed (the countdown).
    pub countdown: Option<gpui::Task<()>>,
    pub owner_name: Option<String>,
    /// In the waiting room: (owner, session title).
    pub waiting: Option<(String, String)>,
    /// Sent away for good, with this code.
    pub ended: Option<String>,
    /// Requests already announced to the window (to tell it when they are
    /// answered).
    pub announced: HashSet<(RequestKind, Id)>,
    /// The relay lost its connection and is reconnecting.
    pub relay_offline: bool,
    /// Reads the relay's events.
    pub relay_reader: Option<gpui::Task<()>>,
}

impl ShareState {
    /// The people inside and those waiting, as shown.
    pub fn rows(&self) -> (Vec<ParticipantRow>, Vec<ParticipantRow>) {
        let (mut inside, waiting) = sharing::participant_rows(&self.participants, self.driver);
        // Mark yourself even if the list came before the `hello`.
        if let Some(me) = self.participant {
            for r in &mut inside {
                r.you |= r.id == me;
            }
        }
        (inside, waiting)
    }

    /// You have the keyboard.
    pub fn is_driver(&self) -> bool {
        match self.driver {
            Some(d) => self.participant == Some(d),
            None => self.owner,
        }
    }

    /// Asked for the keyboard and waiting for the owner.
    pub fn requested(&self) -> bool {
        self.participant.is_some_and(|me| {
            self.participants
                .iter()
                .any(|p| p.id == me && p.requested_control)
        })
    }

    /// Pending requests (owner): who waits to come in and who asks for the
    /// keyboard.
    pub fn pending(&self) -> Vec<(RequestKind, ParticipantRow)> {
        if !self.owner {
            return Vec::new();
        }
        let (inside, waiting) = self.rows();
        waiting
            .into_iter()
            .map(|r| (RequestKind::Join, r))
            .chain(
                inside
                    .into_iter()
                    .filter(|r| r.requested_control && r.role != Role::Owner)
                    .map(|r| (RequestKind::Control, r)),
            )
            .collect()
    }

    /// Time left of the driver's timed grant: `12:34 left`.
    pub fn time_left(&self) -> Option<String> {
        let until = self.until?;
        let left = sharing::time_left(until, termoak_core::time::now_ms());
        Some(t!("share.time_left", time = left).to_string())
    }

    /// Name of a participant (for the owner's notices).
    fn name_of(&self, id: Id) -> Option<String> {
        let (inside, _) = self.rows();
        inside
            .into_iter()
            .find(|r| r.id == id)
            .map(|r| r.name)
            .filter(|n| !n.is_empty())
    }

    /// Name of the owner, for guests.
    pub fn owner_label(&self) -> String {
        let (inside, _) = self.rows();
        sharing::owner_name(&inside)
            .map(str::to_string)
            .or_else(|| self.owner_name.clone())
            .unwrap_or_else(|| t!("share.role.owner").to_string())
    }
}

/// Colors of the avatars.
const AVATAR_HUES: [f32; AVATAR_COLORS] = [0.58, 0.36, 0.07, 0.8, 0.95, 0.5, 0.12, 0.68];

fn avatar_color(ix: usize) -> Hsla {
    hsla(AVATAR_HUES[ix % AVATAR_COLORS], 0.55, 0.45, 1.)
}

/// Round avatar with initials.
fn avatar(row: &ParticipantRow, size: f32) -> Div {
    div()
        .size(px(size))
        .flex_shrink_0()
        .rounded_full()
        .bg(avatar_color(row.color))
        .flex()
        .items_center()
        .justify_center()
        .text_color(gpui::white())
        .text_size(px(size * 0.42))
        .font_semibold()
        .child(row.initials.clone())
}

impl TerminalView {
    // ----- State -----

    /// Forgets everything about sharing (a new connection).
    pub(super) fn reset_share(&mut self) {
        let relay_reader = self.share.relay_reader.take();
        let owner = self.relay.is_some();
        self.share = ShareState {
            relay_reader,
            known: owner,
            owner,
            access: if owner { "owner".into() } else { String::new() },
            ..Default::default()
        };
    }

    /// You are the owner of what this tab shows (or of its relay): the
    /// share buttons are yours.
    pub fn is_share_owner(&self) -> bool {
        self.share.known && self.share.owner
    }

    pub(super) fn on_remote_seat(&mut self, seat: RemoteSeat, cx: &mut Context<Self>) {
        self.share.known = true;
        self.share.owner = seat.owner;
        self.share.access = seat.access;
        self.share.participant = seat.participant;
        self.share.owner_name = seat.owner_name;
        self.share.waiting = None;
        self.can_write = seat.can_write;
        self.share.until = seat.until.filter(|_| seat.driver.is_some());
        self.sync_countdown(cx);
        self.on_participants(seat.participants, seat.driver, cx);
    }

    /// Starts or stops the second-by-second repaint of the countdown.
    fn sync_countdown(&mut self, cx: &mut Context<Self>) {
        if self.share.until.is_none() {
            self.share.countdown = None;
            return;
        }
        if self.share.countdown.is_some() {
            return;
        }
        self.share.countdown = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));
    }

    /// New list of participants: tells the window which requests were
    /// answered (to remove their toasts).
    pub(super) fn on_participants(
        &mut self,
        list: Vec<Participant>,
        driver: Option<Id>,
        cx: &mut Context<Self>,
    ) {
        self.share.participants = list;
        self.share.driver = driver;
        if driver.is_none() {
            self.share.until = None;
            self.sync_countdown(cx);
        }
        let pending: HashSet<(RequestKind, Id)> = self
            .share
            .pending()
            .into_iter()
            .map(|(k, r)| (k, r.id))
            .collect();
        let done: Vec<(RequestKind, Id)> = self
            .share
            .announced
            .iter()
            .filter(|k| !pending.contains(k))
            .copied()
            .collect();
        for (kind, participant) in done {
            self.share.announced.remove(&(kind, participant));
            cx.emit(TerminalEvent::ShareRequestDone { kind, participant });
        }
        // Requests that arrived only in the list (e.g. a reconnect).
        let title = self.label(cx);
        for (kind, row) in self.share.pending() {
            if self.share.announced.insert((kind, row.id)) {
                cx.emit(TerminalEvent::ShareRequest(ShareRequest {
                    kind,
                    participant: row.id,
                    name: row.name.clone(),
                    title: title.clone(),
                }));
            }
        }
    }

    pub(super) fn on_control(
        &mut self,
        driver: Option<Id>,
        driver_name: Option<String>,
        can_write: Option<bool>,
        until: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        let was_driver = self.share.is_driver();
        let old_until = self.share.until;
        self.share.driver = driver;
        self.share.driver_name = driver_name.clone();
        self.share.until = until.filter(|_| driver.is_some());
        self.sync_countdown(cx);
        if let Some(w) = can_write {
            self.can_write = w;
        }
        // The list carries `is_driver` too: keep it in step.
        for p in &mut self.share.participants {
            p.is_driver = driver == Some(p.id) || (driver.is_none() && p.kind == "owner");
            if p.is_driver {
                p.requested_control = false;
            }
        }
        let is_driver = self.share.is_driver();
        if self.share.owner || was_driver == is_driver {
            return;
        }
        // The server takes a timed grant back by itself when the time is up
        // (`control` first, then `control_expired`).
        let time_up = driver.is_none()
            && old_until.is_some_and(|u| termoak_core::time::now_ms() >= u - 5_000);
        let text = if is_driver {
            match self.share.time_left() {
                Some(left) => t!("share.toast.you_drive_timed", time = left),
                None => t!("share.toast.you_drive"),
            }
        } else if time_up {
            t!("share.toast.your_time_up")
        } else {
            match driver_name.filter(|n| !n.is_empty()) {
                Some(name) if driver.is_some() => t!("share.toast.control_to", name = name),
                _ => t!("share.toast.control_back"),
            }
        };
        self.app
            .update(cx, |m, cx| m.toast(ToastKind::Info, text.to_string(), cx));
        cx.emit(TerminalEvent::KeyboardChanged {
            you_drive: is_driver,
            text,
        });
    }

    /// A timed grant ended. The driver already heard it with the `control`
    /// that came first; the owner is told here.
    pub(super) fn on_control_expired(&mut self, participant: Option<Id>, cx: &mut Context<Self>) {
        if !self.share.owner {
            return;
        }
        let text = match participant.and_then(|p| self.share.name_of(p)) {
            Some(name) => t!("share.toast.time_up", name = name),
            None => t!("share.toast.time_up_someone"),
        };
        self.app
            .update(cx, |m, cx| m.toast(ToastKind::Info, text.to_string(), cx));
        cx.notify();
    }

    /// Someone waits or asks for the keyboard (owner).
    pub(super) fn on_share_request(
        &mut self,
        kind: RequestKind,
        p: Participant,
        cx: &mut Context<Self>,
    ) {
        if kind == RequestKind::Join {
            if !self.share.participants.iter().any(|x| x.id == p.id) {
                let mut p = p.clone();
                p.waiting = true;
                self.share.participants.push(p);
            }
        } else if let Some(x) = self.share.participants.iter_mut().find(|x| x.id == p.id) {
            x.requested_control = true;
        } else {
            let mut p = p.clone();
            p.requested_control = true;
            self.share.participants.push(p);
        }
        // Already announced (e.g. it was in the list that came first).
        if !self.share.announced.insert((kind, p.id)) {
            return;
        }
        let row = ParticipantRow::new(&p, self.share.driver);
        cx.emit(TerminalEvent::ShareRequest(ShareRequest {
            kind,
            participant: p.id,
            name: row.name,
            title: self.label(cx),
        }));
    }

    pub(super) fn on_ended(&mut self, code: String) {
        self.share.waiting = None;
        self.can_write = false;
        let (title, _) = sharing::end_message(&code);
        self.share.ended = Some(code);
        self.state = TermState::Closed(Some(title.to_string()));
    }

    /// Sends an action of the shared session (through the relay for a
    /// terminal of this computer).
    pub fn share_action(&mut self, action: ShareAction, cx: &mut Context<Self>) {
        // Shown at once; the server confirms with the next list.
        match action {
            ShareAction::AllowJoin(p) | ShareAction::DenyJoin(p) => {
                if matches!(action, ShareAction::DenyJoin(_)) {
                    self.share.participants.retain(|x| x.id != p);
                } else if let Some(x) = self.share.participants.iter_mut().find(|x| x.id == p) {
                    x.waiting = false;
                }
            }
            ShareAction::DenyControl(p) => {
                if let Some(x) = self.share.participants.iter_mut().find(|x| x.id == p) {
                    x.requested_control = false;
                }
            }
            ShareAction::Kick(p, _) => self.share.participants.retain(|x| x.id != p),
            ShareAction::RequestControl => {
                if let Some(me) = self.share.participant
                    && let Some(x) = self.share.participants.iter_mut().find(|x| x.id == me)
                {
                    x.requested_control = true;
                }
            }
            _ => {}
        }
        let list = std::mem::take(&mut self.share.participants);
        let driver = self.share.driver;
        self.on_participants(list, driver, cx);
        if action == ShareAction::StopSharing && self.relay.is_some() {
            // A terminal of this computer: everyone leaves and it is no
            // longer shared.
            self.stop_relay(true, cx);
        } else if let Some(relay) = self.relay.clone() {
            runtime::handle(cx).spawn(async move {
                let guard = relay.lock().await;
                let Some(r) = guard.as_ref() else { return };
                match action {
                    ShareAction::GrantControl(p, minutes) => r.grant_control(p, minutes).await,
                    ShareAction::DenyControl(p) => r.deny_control(p).await,
                    ShareAction::TakeControl => r.take_control().await,
                    ShareAction::AllowJoin(p) => r.allow_join(p).await,
                    ShareAction::DenyJoin(p) => r.deny_join(p).await,
                    ShareAction::Kick(p, revoke) => r.kick(p, revoke).await,
                    ShareAction::StopSharing => r.stop_guests().await,
                    // The host is the owner: it never asks.
                    ShareAction::RequestControl | ShareAction::ReleaseControl => {}
                }
            });
        } else if let Some(b) = &self.backend {
            b.send(Cmd::Share(action));
        }
        cx.notify();
    }

    /// Answer to a request (from the tab or from a toast).
    pub fn answer_request(
        &mut self,
        kind: RequestKind,
        participant: Id,
        yes: bool,
        cx: &mut Context<Self>,
    ) {
        let action = match (kind, yes) {
            (RequestKind::Join, true) => ShareAction::AllowJoin(participant),
            (RequestKind::Join, false) => ShareAction::DenyJoin(participant),
            (RequestKind::Control, true) => ShareAction::GrantControl(participant, None),
            (RequestKind::Control, false) => ShareAction::DenyControl(participant),
        };
        self.share_action(action, cx);
    }

    // ----- Relay (a terminal of this computer, shared) -----

    /// Listens to what the server says about the shared terminal.
    pub(super) fn watch_relay(&mut self, share: &RelayShare, cx: &mut Context<Self>) {
        let mut rx = share.subscribe();
        self.share.known = true;
        self.share.owner = true;
        self.share.access = "owner".into();
        self.share.relay_offline = false;
        self.share.relay_reader = Some(cx.spawn(async move |this, cx| {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if this.update(cx, |v, cx| v.on_relay_event(ev, cx)).is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }));
    }

    fn on_relay_event(&mut self, ev: RelayEvent, cx: &mut Context<Self>) {
        match ev {
            RelayEvent::Participants {
                participants,
                driver,
            } => self.on_participants(participants, driver, cx),
            RelayEvent::Control {
                driver,
                driver_name,
                until,
            } => self.on_control(driver, driver_name, None, until, cx),
            RelayEvent::ControlExpired { participant } => self.on_control_expired(participant, cx),
            RelayEvent::JoinRequest(p) => self.on_share_request(RequestKind::Join, p, cx),
            RelayEvent::ControlRequest(p) => self.on_share_request(RequestKind::Control, p, cx),
            // The terminal is here: its size is the window's.
            RelayEvent::ResizeRequest { .. } => {}
            RelayEvent::Reconnecting => self.share.relay_offline = true,
            RelayEvent::Reconnected => self.share.relay_offline = false,
            RelayEvent::Ended { code } => {
                let was_shared = self.relay.is_some();
                // The relay task is over: forget it without stopping it again.
                self.relay = None;
                if !matches!(self.kind, super::TermKind::Server { .. }) {
                    self.share_session = None;
                }
                self.copilot_shared = false;
                if let Some((_, closed)) = self.local_feed.take() {
                    let _ = closed.send(true);
                }
                self.share.participants.clear();
                self.share.driver = None;
                self.share.until = None;
                self.sync_countdown(cx);
                self.share.relay_offline = false;
                let done: Vec<_> = self.share.announced.drain().collect();
                for (kind, participant) in done {
                    cx.emit(TerminalEvent::ShareRequestDone { kind, participant });
                }
                if was_shared && code.is_some() && matches!(self.state, TermState::Running) {
                    self.app.update(cx, |m, cx| {
                        m.toast(
                            ToastKind::Info,
                            t!("share.toast.relay_ended").to_string(),
                            cx,
                        )
                    });
                }
            }
        }
        cx.notify();
    }

    // ----- Painting -----

    /// Participants button of the toolbar (avatars and count), with the
    /// list and the owner's actions in a popover.
    pub(super) fn render_participants(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.share_session.is_none() || !self.share.known {
            return None;
        }
        let (inside, waiting) = self.share.rows();
        if inside.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let count = inside.len();
        let attention = self.share.owner && !self.share.pending().is_empty();
        let stack = h_flex()
            .items_center()
            .children(inside.iter().take(3).enumerate().map(|(i, r)| {
                avatar(r, 20.)
                    .border_2()
                    .border_color(theme.tab_bar)
                    .when(i > 0, |a| a.ml(px(-6.)))
            }))
            .child(
                div()
                    .ml_1()
                    .text_xs()
                    .font_medium()
                    .child(count.to_string()),
            )
            .when(attention, |this| {
                this.child(div().ml_1().size(px(8.)).rounded_full().bg(theme.warning))
            });
        let weak = cx.entity().downgrade();
        let tooltip = tn!("share.participants.count", count);
        let _ = waiting;
        Some(
            Popover::new("participants")
                .anchor(Anchor::TopRight)
                .trigger(
                    Button::new("participants-button")
                        .small()
                        .ghost()
                        .tooltip(tooltip)
                        .child(stack),
                )
                .content(move |_, _, cx| participants_popover(&weak, cx))
                .into_any_element(),
        )
    }

    /// Banners under the toolbar: read-only / keyboard for participants,
    /// "X is driving" and the pending requests for the owner.
    pub(super) fn render_share_banners(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.share.known
            || self.share_session.is_none()
            || !matches!(self.state, TermState::Running)
        {
            return None;
        }
        let theme = cx.theme();
        let (inside, _) = self.share.rows();
        let mut rows: Vec<AnyElement> = Vec::new();
        let banner = |color: Hsla| {
            let mut bg = color;
            bg.a = 0.12;
            h_flex()
                .w_full()
                .px_3()
                .py_1()
                .gap_2()
                .items_center()
                .text_xs()
                .bg(bg)
                .border_b_1()
                .border_color(theme.border)
        };
        let time_left = self.share.time_left();
        if self.share.owner {
            if let Some(d) = sharing::other_driver(&inside) {
                rows.push(
                    banner(theme.info)
                        .child(
                            ui::icon(IconName::Keyboard)
                                .size(px(14.))
                                .text_color(theme.info),
                        )
                        .child(
                            div()
                                .flex_1()
                                .child(t!("share.banner.driving", name = d.name.clone())),
                        )
                        .when_some(time_left.clone(), |this, left| {
                            this.child(time_pill(left, theme.info))
                        })
                        .child(
                            Button::new("take-control")
                                .xsmall()
                                .primary()
                                .label(t!("share.take_back"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.share_action(ShareAction::TakeControl, cx)
                                })),
                        )
                        .into_any_element(),
                );
            }
            for (i, (kind, r)) in self.share.pending().into_iter().enumerate() {
                let id = r.id;
                let (text, yes, no) = match kind {
                    RequestKind::Join => (
                        t!("share.request.join", name = r.name.clone()),
                        t!("share.allow"),
                        t!("share.deny"),
                    ),
                    RequestKind::Control => (
                        t!("share.request.control", name = r.name.clone()),
                        t!("share.give"),
                        t!("share.deny"),
                    ),
                };
                let weak = cx.entity().downgrade();
                rows.push(
                    banner(theme.warning)
                        .child(avatar(&r, 18.))
                        .child(div().flex_1().font_medium().child(text))
                        .child(
                            Button::new(("request-yes", i))
                                .xsmall()
                                .primary()
                                .label(yes)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.answer_request(kind, id, true, cx)
                                })),
                        )
                        // Give the keyboard for a while.
                        .when(kind == RequestKind::Control, |this| {
                            this.child(
                                Button::new(("request-yes-for", i))
                                    .xsmall()
                                    .primary()
                                    .icon(ui::icon(IconName::ChevronDown))
                                    .tooltip(t!("share.give_for"))
                                    .dropdown_menu(move |menu, _, _| {
                                        grant_menu(menu, weak.clone(), id)
                                    }),
                            )
                        })
                        .child(
                            Button::new(("request-no", i))
                                .xsmall()
                                .ghost()
                                .label(no)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.answer_request(kind, id, false, cx)
                                })),
                        )
                        .into_any_element(),
                );
            }
        } else {
            let owner = self.share.owner_label();
            // Servers before live sharing let `control` guests always type
            // and know nothing about asking for the keyboard.
            let live = !self.share.participants.is_empty();
            let driving = self.share.is_driver() || self.can_write;
            let can_ask = live && self.share.access == "control";
            let requested = self.share.requested();
            let other = sharing::other_driver(&inside).filter(|d| d.role != Role::Owner);
            let row = if driving {
                banner(theme.success)
                    .child(
                        ui::icon(IconName::Keyboard)
                            .size(px(14.))
                            .text_color(theme.success),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(t!("share.banner.you_drive", owner = owner)),
                    )
                    .when_some(time_left.clone(), |this, left| {
                        this.child(time_pill(left, theme.success))
                    })
                    .child(
                        Button::new("release-control")
                            .xsmall()
                            .label(t!("share.release"))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.share_action(ShareAction::ReleaseControl, cx)
                            })),
                    )
            } else {
                banner(theme.muted_foreground)
                    .child(
                        ui::icon(IconName::Eye)
                            .size(px(14.))
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(t!("share.banner.read_only", owner = owner)),
                    )
                    .when_some(other, |this, d| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .text_color(theme.info)
                                .child(ui::icon(IconName::Keyboard).size(px(14.)))
                                .child(t!("share.banner.driving", name = d.name.clone()))
                                .when_some(time_left.clone(), |this, left| {
                                    this.child(format!("· {left}"))
                                }),
                        )
                    })
                    .when(can_ask && !requested, |this| {
                        this.child(
                            Button::new("request-control")
                                .xsmall()
                                .primary()
                                .icon(ui::icon(IconName::Hand))
                                .label(t!("share.request_control"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.share_action(ShareAction::RequestControl, cx)
                                })),
                        )
                    })
                    .when(can_ask && requested, |this| {
                        this.child(div().text_color(theme.warning).child(t!("share.requested")))
                            .child(
                                Button::new("cancel-request")
                                    .xsmall()
                                    .ghost()
                                    .label(t!("common.cancel"))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.share_action(ShareAction::ReleaseControl, cx)
                                    })),
                            )
                    })
            };
            rows.push(row.into_any_element());
        }
        if rows.is_empty() {
            return None;
        }
        Some(v_flex().w_full().children(rows).into_any_element())
    }

    /// Waiting room: until the owner lets you in.
    pub(super) fn render_waiting(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (owner, title) = self.share.waiting.clone()?;
        let theme = cx.theme();
        let owner = if owner.is_empty() {
            t!("share.role.owner").to_string()
        } else {
            owner
        };
        Some(
            v_flex()
                .absolute()
                .inset_0()
                .items_center()
                .justify_center()
                .bg(theme.background)
                .child(
                    ui::card(cx)
                        .p_6()
                        .gap_3()
                        .max_w(px(460.))
                        .items_center()
                        .child(Spinner::new().large())
                        .child(
                            div()
                                .text_lg()
                                .font_semibold()
                                .text_center()
                                .child(t!("share.waiting.title", owner = owner)),
                        )
                        .when(!title.is_empty(), |this| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(title),
                            )
                        })
                        .child(
                            div()
                                .text_sm()
                                .text_center()
                                .text_color(theme.muted_foreground)
                                .child(t!("share.waiting.detail")),
                        )
                        .child(
                            Button::new("waiting-cancel")
                                .label(t!("common.cancel"))
                                .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                    cx.emit(TerminalEvent::Pane(PaneAction::Close))
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Sent away for good: why, and a button to close the tab.
    pub(super) fn render_ended(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let code = self.share.ended.as_deref()?;
        // The owner sees the usual "closed" state.
        if self.share.owner {
            return None;
        }
        let theme = cx.theme();
        let (title, detail) = sharing::end_message(code);
        let icon = match code {
            "kicked" | "join_denied" | "forbidden" => IconName::Ban,
            "expired" => IconName::Timer,
            "revoked" => IconName::Lock,
            _ => IconName::CircleStop,
        };
        Some(
            v_flex()
                .absolute()
                .inset_0()
                .items_center()
                .justify_center()
                .child(
                    ui::card(cx)
                        .p_6()
                        .gap_3()
                        .max_w(px(460.))
                        .items_center()
                        .child(
                            ui::icon(icon)
                                .size(px(32.))
                                .text_color(theme.muted_foreground),
                        )
                        .child(div().text_lg().font_semibold().text_center().child(title))
                        .child(
                            div()
                                .text_sm()
                                .text_center()
                                .text_color(theme.muted_foreground)
                                .child(detail),
                        )
                        .child(
                            Button::new("ended-close")
                                .primary()
                                .label(t!("share.end.close_tab"))
                                .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                    cx.emit(TerminalEvent::Pane(PaneAction::Close))
                                })),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// The participants popover, read from the terminal each time it paints.
fn participants_popover(weak: &WeakEntity<TerminalView>, cx: &mut App) -> AnyElement {
    let Some(view) = weak.upgrade() else {
        return div().into_any_element();
    };
    let (inside, waiting, owner, time_left) = {
        let v = view.read(cx);
        let (inside, waiting) = v.share.rows();
        (inside, waiting, v.share.owner, v.share.time_left())
    };
    let theme = cx.theme().clone();
    let action_button =
        |id: (&'static str, usize), label: SharedString, icon: IconName, action: ShareAction| {
            let weak = weak.clone();
            Button::new(id)
                .xsmall()
                .ghost()
                .icon(ui::icon(icon))
                .label(label)
                .on_click(move |_: &ClickEvent, _, cx| {
                    if let Some(v) = weak.upgrade() {
                        v.update(cx, |v, cx| v.share_action(action, cx));
                    }
                })
        };
    let row = |i: usize, r: &ParticipantRow, waiting: bool| {
        let mut details: Vec<String> = vec![r.role.label().to_string()];
        if r.devices > 1 {
            details.push(tn!("share.participants.devices", r.devices).to_string());
        } else if r.devices == 0 && !waiting {
            details.push(t!("share.participants.reconnecting").to_string());
        }
        if r.is_driver
            && r.role != Role::Owner
            && let Some(left) = &time_left
        {
            details.push(left.clone());
        }
        let id = r.id;
        let name = if r.you {
            t!("share.participants.you", name = r.name.clone()).to_string()
        } else {
            r.name.clone()
        };
        let kick_weak = weak.clone();
        let kick_name = r.name.clone();
        v_flex()
            .gap_1()
            .py_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(avatar(r, 26.))
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
                                            .text_sm()
                                            .font_medium()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .child(name),
                                    )
                                    .when(r.is_driver, |this| {
                                        this.child(
                                            ui::icon(IconName::Keyboard)
                                                .size(px(14.))
                                                .text_color(theme.success),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(details.join(" · ")),
                            ),
                    )
                    .when(r.requested_control, |this| {
                        this.child(ui::pill(t!("share.participants.asked"), theme.warning))
                    })
                    .when(waiting, |this| {
                        this.child(ui::pill(t!("share.participants.waiting"), theme.warning))
                    }),
            )
            .when(owner && r.role != Role::Owner, |this| {
                let mut actions = h_flex().gap_1().pl(px(34.)).flex_wrap();
                if waiting {
                    actions = actions
                        .child(action_button(
                            ("p-allow", i),
                            t!("share.allow"),
                            IconName::UserCheck,
                            ShareAction::AllowJoin(id),
                        ))
                        .child(action_button(
                            ("p-deny", i),
                            t!("share.deny"),
                            IconName::X,
                            ShareAction::DenyJoin(id),
                        ));
                } else {
                    if r.is_driver {
                        actions = actions.child(action_button(
                            ("p-take", i),
                            t!("share.take_back"),
                            IconName::Undo2,
                            ShareAction::TakeControl,
                        ));
                    } else if r.can_control {
                        actions = actions.child(action_button(
                            ("p-give", i),
                            t!("share.give_control"),
                            IconName::Keyboard,
                            ShareAction::GrantControl(id, None),
                        ));
                    }
                    // Already driving: the same buttons change the time.
                    if r.can_control {
                        actions = actions.child(
                            h_flex()
                                .gap_0p5()
                                .items_center()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(t!("share.give_for_short")),
                                )
                                .children(sharing::CONTROL_MINUTES.iter().map(|&m| {
                                    let weak = weak.clone();
                                    Button::new(SharedString::from(format!("p-give-{i}-{m}")))
                                        .xsmall()
                                        .ghost()
                                        .label(tn!("share.minutes_short", m))
                                        .tooltip(tn!("share.give_control_minutes", m))
                                        .on_click(move |_: &ClickEvent, _, cx| {
                                            if let Some(v) = weak.upgrade() {
                                                v.update(cx, |v, cx| {
                                                    v.share_action(
                                                        ShareAction::GrantControl(id, Some(m)),
                                                        cx,
                                                    )
                                                });
                                            }
                                        })
                                })),
                        );
                    }
                    if r.requested_control && !r.is_driver {
                        actions = actions.child(action_button(
                            ("p-deny-control", i),
                            t!("share.deny"),
                            IconName::X,
                            ShareAction::DenyControl(id),
                        ));
                    }
                    actions = actions
                        .child(action_button(
                            ("p-kick", i),
                            t!("share.kick"),
                            IconName::LogOut,
                            ShareAction::Kick(id, false),
                        ))
                        .child(
                            Button::new(("p-block", i))
                                .xsmall()
                                .ghost()
                                .icon(ui::icon(IconName::Ban))
                                .label(t!("share.kick_block"))
                                .on_click(move |_: &ClickEvent, window, cx| {
                                    let weak = kick_weak.clone();
                                    ui::confirm(
                                        window,
                                        cx,
                                        t!("share.kick_block_title"),
                                        t!("share.kick_block_message", name = kick_name.clone()),
                                        t!("share.kick_block"),
                                        true,
                                        move |_, cx| {
                                            if let Some(v) = weak.upgrade() {
                                                v.update(cx, |v, cx| {
                                                    v.share_action(ShareAction::Kick(id, true), cx)
                                                });
                                            }
                                        },
                                    );
                                }),
                        );
                }
                this.child(actions)
            })
    };
    v_flex()
        .w(px(340.))
        .gap_1()
        .child(
            div()
                .text_xs()
                .font_semibold()
                .text_color(theme.muted_foreground)
                .child(tn!("share.participants.count", inside.len())),
        )
        .children(inside.iter().enumerate().map(|(i, r)| row(i, r, false)))
        .when(!waiting.is_empty(), |this| {
            this.child(
                div()
                    .pt_2()
                    .text_xs()
                    .font_semibold()
                    .text_color(theme.muted_foreground)
                    .child(t!("share.participants.waiting_room")),
            )
            .children(
                waiting
                    .iter()
                    .enumerate()
                    .map(|(i, r)| row(1000 + i, r, true)),
            )
        })
        .when(owner, |this| {
            let weak = weak.clone();
            this.child(
                h_flex().pt_2().justify_end().child(
                    Button::new("stop-sharing-all")
                        .xsmall()
                        .danger()
                        .label(t!("share.stop_all"))
                        .on_click(move |_: &ClickEvent, window, cx| {
                            let weak = weak.clone();
                            ui::confirm(
                                window,
                                cx,
                                t!("share.stop_all_title"),
                                t!("share.stop_all_message"),
                                t!("share.stop_all"),
                                true,
                                move |_, cx| {
                                    if let Some(v) = weak.upgrade() {
                                        v.update(cx, |v, cx| {
                                            v.share_action(ShareAction::StopSharing, cx)
                                        });
                                    }
                                },
                            );
                        }),
                ),
            )
        })
        .into_any_element()
}

/// Time left of a timed hand-over, next to the driver.
fn time_pill(text: String, color: Hsla) -> Div {
    h_flex()
        .gap_1()
        .items_center()
        .child(ui::icon(IconName::Timer).size(px(12.)))
        .child(ui::pill(text, color))
        .text_color(color)
}

/// Menu of the "Give" split button: until taken back, or for a while.
fn grant_menu(
    mut menu: gpui_component::menu::PopupMenu,
    weak: WeakEntity<TerminalView>,
    participant: Id,
) -> gpui_component::menu::PopupMenu {
    let choices = std::iter::once(None).chain(sharing::CONTROL_MINUTES.iter().map(|&m| Some(m)));
    for minutes in choices {
        let weak = weak.clone();
        menu = menu.item(
            PopupMenuItem::new(sharing::control_for_label(minutes)).on_click(move |_, _, cx| {
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |v, cx| {
                        v.share_action(ShareAction::GrantControl(participant, minutes), cx)
                    });
                }
            }),
        );
    }
    menu
}
