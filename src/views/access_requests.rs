//! Temporary access (docs-next/TEAMS-SECURITY.md §5): the "Request access"
//! dialog (a duration up to the policy's maximum and a reason), and the
//! Access section: requests waiting for your decision (approve with an
//! optional shorter window and a note, deny with a reason), the windows you
//! can end early, and your own requests (cancel). Kept current by the
//! events socket (`access_request`).

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState};
use gpui_component::select::Select;
use gpui_component::{ActiveTheme, Sizable, StyledExt, h_flex, v_flex};
use serde_json::{Value, json};
use termoak_core::Id;
use termoak_core::access::{AccessRequestStatus, AccessTargetKind};

use crate::access::{self, AccessBook, AccessNeed, Request};
use crate::runtime;
use crate::state::{AppModel, api_error};
use crate::ui::{self, Choice, IconName};

/// After a request changed: the book and the host answers are read again.
fn refresh(model: &Entity<AppModel>, account: Id, cx: &mut App) {
    let Some(api) = model.read(cx).api_of(Some(account)) else {
        return;
    };
    let book = AccessBook::global(cx);
    book.update(cx, |b, cx| b.on_event(account, api, cx));
}

/// Asks for access to a host (or what its policy covers).
pub fn open_request_dialog(
    model: Entity<AppModel>,
    account: Id,
    host_label: String,
    need: AccessNeed,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(pending) = need.pending {
        ui::confirm(
            window,
            cx,
            t!("access.request.pending_title"),
            t!("access.request.pending_message", host = host_label),
            t!("access.request.cancel_request"),
            true,
            move |window, cx| cancel(model.clone(), account, pending, window, cx),
        );
        return;
    }
    if !need.can_request {
        ui::error(window, cx, t!("access.request.cannot"));
        return;
    }
    let choices = access::duration_choices(need.max_minutes);
    let default = choices
        .iter()
        .copied()
        .find(|m| *m >= 60)
        .unwrap_or(need.max_minutes)
        .min(need.max_minutes);
    let duration = ui::choice_state(
        choices
            .iter()
            .map(|m| Choice::new(access::duration_label(*m), *m))
            .collect(),
        Some(&default),
        window,
        cx,
    );
    // A policy of a group or the vault: the request may cover all of it.
    let wide = match (need.target_kind, need.target_id) {
        (AccessTargetKind::Group | AccessTargetKind::Vault, Some(id)) => {
            Some((need.target_kind, id))
        }
        _ => None,
    };
    let scope = wide.map(|(kind, _)| {
        ui::choice_state(
            vec![
                Choice::new(
                    t!("access.request.scope_host", host = host_label.clone()),
                    false,
                ),
                Choice::new(
                    match kind {
                        AccessTargetKind::Vault => t!("access.request.scope_vault"),
                        _ => t!("access.request.scope_group"),
                    },
                    true,
                ),
            ],
            Some(&false),
            window,
            cx,
        )
    });
    let reason = cx
        .new(|cx| InputState::new(window, cx).placeholder(t!("access.request.reason_placeholder")));
    ui::focus_later(&reason, window, cx);
    let (d2, r2, s2) = (duration.clone(), reason.clone(), scope.clone());
    let max = need.max_minutes;
    let host = need.host_id;
    let label = host_label.clone();
    ui::open_form_dialog(
        window,
        cx,
        t!("access.request.title", host = host_label.clone()),
        t!("access.request.send"),
        480.,
        move |_, cx| {
            v_flex()
                .gap_3()
                .child(div().text_sm().child(t!(
                    "access.request.message",
                    max = access::duration_label(max)
                )))
                .child(ui::field(
                    t!("access.request.duration"),
                    Select::new(&duration),
                    cx,
                ))
                .when_some(scope.clone(), |this, s| {
                    this.child(ui::field(t!("access.request.scope"), Select::new(&s), cx))
                })
                .child(ui::field(
                    t!("access.request.reason"),
                    Input::new(&reason),
                    cx,
                ))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(t!("access.request.after")),
                )
                .into_any_element()
        },
        move |window, cx| {
            let minutes = ui::chosen(&d2, cx).unwrap_or(0);
            let text = r2.read(cx).value().to_string();
            if let Err(key) = access::request_valid(minutes, max, &text) {
                ui::error(window, cx, t!(key));
                return false;
            }
            let all = s2.as_ref().and_then(|s| ui::chosen(s, cx)).unwrap_or(false);
            let (kind, target) = match (all, wide, host) {
                (true, Some((k, id)), _) => (k, id),
                (_, _, Some(h)) => (AccessTargetKind::Host, h),
                (_, Some((k, id)), None) => (k, id),
                _ => {
                    ui::error(window, cx, t!("access.request.no_target"));
                    return false;
                }
            };
            let Some(api) = model.read(cx).api_of(Some(account)) else {
                ui::error(window, cx, t!("vaults.not_signed_in"));
                return false;
            };
            let body = access::request_body(kind, target, minutes, &text);
            let model = model.clone();
            let label = label.clone();
            let task = runtime::spawn(cx, async move {
                api.post::<Value>("/api/v1/access-requests", &body)
                    .await
                    .map_err(api_error)
            });
            window
                .spawn(cx, async move |cx| {
                    let res = task.await;
                    let _ = cx.update(|window, cx| match res {
                        Ok(_) => {
                            ui::success(window, cx, t!("access.request.sent", host = label));
                            refresh(&model, account, cx);
                        }
                        Err(e) => ui::error(window, cx, e),
                    });
                })
                .detach();
            true
        },
    );
}

/// Cancels a request of yours (or ends your window).
pub fn cancel(model: Entity<AppModel>, account: Id, id: Id, window: &mut Window, cx: &mut App) {
    decide(
        model,
        account,
        id,
        "cancel",
        json!({}),
        t!("access.cancelled"),
        window,
        cx,
    );
}

/// Sends a decision (`approve`, `deny`, `revoke`, `cancel`).
#[allow(clippy::too_many_arguments)]
fn decide(
    model: Entity<AppModel>,
    account: Id,
    id: Id,
    action: &'static str,
    body: Value,
    ok: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(api) = model.read(cx).api_of(Some(account)) else {
        ui::error(window, cx, t!("vaults.not_signed_in"));
        return;
    };
    let task = runtime::spawn(cx, async move {
        api.post::<Value>(&format!("/api/v1/access-requests/{id}/{action}"), &body)
            .await
            .map_err(api_error)
    });
    window
        .spawn(cx, async move |cx| {
            let res = task.await;
            let _ = cx.update(|window, cx| {
                match res {
                    Ok(_) => ui::success(window, cx, ok),
                    Err(e) => ui::error(window, cx, e),
                }
                refresh(&model, account, cx);
            });
        })
        .detach();
}

/// Approve: an optional shorter window and a note.
pub fn open_approve(
    model: Entity<AppModel>,
    account: Id,
    r: Request,
    window: &mut Window,
    cx: &mut App,
) {
    let choices = access::duration_choices(r.minutes);
    let minutes = ui::choice_state(
        choices
            .iter()
            .map(|m| Choice::new(access::duration_label(*m), *m))
            .collect(),
        Some(&r.minutes),
        window,
        cx,
    );
    let note =
        cx.new(|cx| InputState::new(window, cx).placeholder(t!("access.approve.note_placeholder")));
    let (m2, n2) = (minutes.clone(), note.clone());
    let summary = request_summary(&r);
    let full = r.minutes;
    let id = r.id;
    ui::open_form_dialog(
        window,
        cx,
        t!("access.approve.title"),
        t!("access.approve.ok"),
        480.,
        move |_, cx| {
            v_flex()
                .gap_3()
                .child(div().text_sm().child(summary.clone()))
                .child(ui::field_with_hint(
                    t!("access.approve.window"),
                    Select::new(&minutes),
                    t!("access.approve.window_hint"),
                    cx,
                ))
                .child(ui::field(t!("access.approve.note"), Input::new(&note), cx))
                .into_any_element()
        },
        move |window, cx| {
            let chosen = ui::chosen(&m2, cx).unwrap_or(full);
            let note = n2.read(cx).value().trim().to_string();
            let mut body = json!({});
            if chosen < full {
                body["minutes"] = json!(chosen);
            }
            if !note.is_empty() {
                body["reason"] = json!(note);
            }
            decide(
                model.clone(),
                account,
                id,
                "approve",
                body,
                t!("access.approved"),
                window,
                cx,
            );
            true
        },
    );
}

/// Deny or revoke, with a reason.
fn open_with_reason(
    model: Entity<AppModel>,
    account: Id,
    r: Request,
    revoke: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let reason =
        cx.new(|cx| InputState::new(window, cx).placeholder(t!("access.deny.reason_placeholder")));
    let r2 = reason.clone();
    let summary = request_summary(&r);
    let id = r.id;
    ui::open_form_dialog(
        window,
        cx,
        if revoke {
            t!("access.revoke.title")
        } else {
            t!("access.deny.title")
        },
        if revoke {
            t!("access.revoke.ok")
        } else {
            t!("access.deny.ok")
        },
        460.,
        move |_, cx| {
            v_flex()
                .gap_3()
                .child(div().text_sm().child(summary.clone()))
                .when(revoke, |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t!("access.revoke.hint")),
                    )
                })
                .child(ui::field(t!("access.deny.reason"), Input::new(&reason), cx))
                .into_any_element()
        },
        move |window, cx| {
            let text = r2.read(cx).value().trim().to_string();
            let body = if text.is_empty() {
                json!({})
            } else {
                json!({"reason": text})
            };
            let (action, ok) = if revoke {
                ("revoke", t!("access.revoked"))
            } else {
                ("deny", t!("access.denied"))
            };
            decide(model.clone(), account, id, action, body, ok, window, cx);
            true
        },
    );
}

/// "Ana asks for db-1 for 1 h: deploy the fix".
pub fn request_summary(r: &Request) -> String {
    let who = if r.requester_name.trim().is_empty() {
        r.requester_email.clone()
    } else {
        format!("{} ({})", r.requester_name, r.requester_email)
    };
    t!(
        "access.summary",
        name = who,
        target = r.target_label.clone(),
        time = access::duration_label(r.minutes),
        reason = r.reason.clone()
    )
    .to_string()
}

/// The Access section.
pub struct AccessView {
    model: Entity<AppModel>,
    book: Entity<AccessBook>,
    _subs: Vec<Subscription>,
    _tick: gpui::Task<()>,
}

impl AccessView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let book = AccessBook::global(cx);
        let subs = vec![
            cx.observe(&book, |_, _, cx| cx.notify()),
            cx.observe_in(&model, window, |_, _, _, cx| cx.notify()),
        ];
        // Countdowns of active windows.
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(15))
                    .await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        let mut this = Self {
            model,
            book,
            _subs: subs,
            _tick: tick,
        };
        this.refresh(cx);
        this
    }

    /// Reads the requests of every account with teams security again.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let accounts: Vec<(Id, termoak_client::ApiClient)> = {
            let m = self.model.read(cx);
            m.access_accounts()
                .into_iter()
                .filter_map(|a| m.api_of(Some(a)).map(|api| (a, api)))
                .collect()
        };
        self.book.update(cx, |b, cx| {
            for (a, api) in accounts {
                b.refresh(a, api, cx);
            }
        });
    }

    fn row(
        &self,
        account: Id,
        r: &Request,
        multi: Option<String>,
        actions: Vec<gpui::AnyElement>,
        cx: &App,
    ) -> gpui::Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let now = termoak_core::time::now_ms();
        let _ = account;
        let status = access::status_label(r, now);
        let color = match r.status {
            AccessRequestStatus::Pending => theme.warning,
            AccessRequestStatus::Approved if r.is_active(now) => theme.success,
            AccessRequestStatus::Denied | AccessRequestStatus::Revoked => theme.danger,
            _ => muted,
        };
        let when = match (r.status, r.ends_at) {
            (AccessRequestStatus::Approved, Some(end)) if end > now => {
                access::window_label(end, now)
            }
            (AccessRequestStatus::Pending, _) => t!(
                "access.waits_until",
                time = if r.expires_at - now > 12 * 3_600_000 {
                    ui::format_ms(r.expires_at)
                } else {
                    access::clock(r.expires_at)
                }
            )
            .to_string(),
            _ => ui::format_ms(r.decided_at.unwrap_or(r.created_at)),
        };
        h_flex()
            .gap_3()
            .p_3()
            .items_center()
            .rounded(theme.radius)
            .bg(theme.secondary)
            .child(
                ui::icon(match r.target_kind {
                    AccessTargetKind::Group => IconName::Folder,
                    AccessTargetKind::Vault => IconName::Vault,
                    _ => IconName::Server,
                })
                .size(px(16.))
                .text_color(muted),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child(r.target_label.clone()),
                            )
                            .child(ui::pill(status, color))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(access::duration_label(r.minutes)),
                            ),
                    )
                    .child(div().text_xs().child(request_summary(r)))
                    .child(div().text_xs().text_color(muted).child(match multi {
                        Some(a) => format!("{when} · {a}"),
                        None => when,
                    }))
                    .when_some(
                        r.decision_reason.clone().filter(|s| !s.trim().is_empty()),
                        |this, d| {
                            this.child(div().text_xs().text_color(muted).child(t!(
                                "access.decision_note",
                                name = r.decided_by_name.clone().unwrap_or_default(),
                                note = d
                            )))
                        },
                    ),
            )
            .child(h_flex().gap_1().children(actions))
    }
}

impl Render for AccessView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = termoak_core::time::now_ms();
        let accounts = self.model.read(cx).access_accounts();
        let multi = accounts.len() > 1;
        let names: Vec<(Id, String)> = accounts
            .iter()
            .map(|a| {
                (
                    *a,
                    self.model
                        .read(cx)
                        .account(*a)
                        .map(|m| crate::accounts::shown_name(&m.info))
                        .unwrap_or_default(),
                )
            })
            .collect();
        let name_of = |a: Id| names.iter().find(|(x, _)| *x == a).map(|(_, n)| n.clone());
        let mut waiting = Vec::new();
        let mut active = Vec::new();
        let mut mine = Vec::new();
        let mut recent = Vec::new();
        {
            let book = self.book.read(cx);
            for a in &accounts {
                let Some(acc) = book.account(*a) else {
                    continue;
                };
                for r in &acc.approvals {
                    if r.status == AccessRequestStatus::Pending && r.can_decide {
                        waiting.push((*a, r.clone()));
                    } else if r.status == AccessRequestStatus::Approved
                        && r.is_active(now)
                        && r.can_decide
                    {
                        active.push((*a, r.clone()));
                    } else if r.status != AccessRequestStatus::Pending {
                        recent.push((*a, r.clone()));
                    }
                }
                for r in &acc.mine {
                    mine.push((*a, r.clone()));
                }
            }
        }
        recent.truncate(20);
        let model = self.model.clone();
        let button = |id: (&'static str, usize),
                      label: SharedString,
                      icon: IconName,
                      primary: bool,
                      danger: bool,
                      f: Box<dyn Fn(&mut Window, &mut App)>|
         -> gpui::AnyElement {
            Button::new(id)
                .small()
                .label(label)
                .icon(ui::icon(icon))
                .map(|b| {
                    if primary {
                        b.primary()
                    } else if danger {
                        b.danger()
                    } else {
                        b.ghost()
                    }
                })
                .on_click(move |_: &ClickEvent, window, cx| f(window, cx))
                .into_any_element()
        };
        let section = |title: SharedString, empty: SharedString, rows: Vec<gpui::Div>, cx: &App| {
            v_flex()
                .gap_2()
                .child(div().font_semibold().child(title))
                .when(rows.is_empty(), |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(empty),
                    )
                })
                .children(rows)
        };
        let waiting_rows: Vec<gpui::Div> = waiting
            .iter()
            .enumerate()
            .map(|(i, (a, r))| {
                let (m1, m2, r1, r2, a1) = (model.clone(), model.clone(), r.clone(), r.clone(), *a);
                self.row(
                    *a,
                    r,
                    multi.then(|| name_of(*a)).flatten(),
                    vec![
                        button(
                            ("acc-approve", i),
                            t!("access.approve.button"),
                            IconName::CircleCheck,
                            true,
                            false,
                            Box::new(move |w, cx| open_approve(m1.clone(), a1, r1.clone(), w, cx)),
                        ),
                        button(
                            ("acc-deny", i),
                            t!("access.deny.button"),
                            IconName::CircleX,
                            false,
                            false,
                            Box::new(move |w, cx| {
                                open_with_reason(m2.clone(), a1, r2.clone(), false, w, cx)
                            }),
                        ),
                    ],
                    cx,
                )
            })
            .collect();
        let active_rows: Vec<gpui::Div> = active
            .iter()
            .enumerate()
            .map(|(i, (a, r))| {
                let (m1, r1, a1) = (model.clone(), r.clone(), *a);
                self.row(
                    *a,
                    r,
                    multi.then(|| name_of(*a)).flatten(),
                    vec![button(
                        ("acc-revoke", i),
                        t!("access.revoke.button"),
                        IconName::Ban,
                        false,
                        true,
                        Box::new(move |w, cx| {
                            open_with_reason(m1.clone(), a1, r1.clone(), true, w, cx)
                        }),
                    )],
                    cx,
                )
            })
            .collect();
        let mine_rows: Vec<gpui::Div> = mine
            .iter()
            .enumerate()
            .map(|(i, (a, r))| {
                let open = r.status == AccessRequestStatus::Pending
                    || (r.status == AccessRequestStatus::Approved && r.is_active(now));
                let (m1, id, a1) = (model.clone(), r.id, *a);
                let actions = if open {
                    vec![button(
                        ("acc-cancel", i),
                        if r.status == AccessRequestStatus::Pending {
                            t!("access.cancel.button")
                        } else {
                            t!("access.end.button")
                        },
                        IconName::X,
                        false,
                        false,
                        Box::new(move |w, cx| cancel(m1.clone(), a1, id, w, cx)),
                    )]
                } else {
                    Vec::new()
                };
                self.row(*a, r, multi.then(|| name_of(*a)).flatten(), actions, cx)
            })
            .collect();
        let recent_rows: Vec<gpui::Div> = recent
            .iter()
            .map(|(a, r)| self.row(*a, r, multi.then(|| name_of(*a)).flatten(), Vec::new(), cx))
            .collect();
        let body = if accounts.is_empty() {
            ui::empty_state(
                IconName::ShieldCheck,
                t!("access.none_title"),
                t!("access.none_hint"),
                cx,
            )
            .into_any_element()
        } else {
            v_flex()
                .gap_6()
                .p_6()
                .child(section(
                    t!("access.waiting"),
                    t!("access.waiting_empty"),
                    waiting_rows,
                    cx,
                ))
                .child(section(
                    t!("access.active"),
                    t!("access.active_empty"),
                    active_rows,
                    cx,
                ))
                .child(section(
                    t!("access.mine"),
                    t!("access.mine_empty"),
                    mine_rows,
                    cx,
                ))
                .when(!recent_rows.is_empty(), |this| {
                    this.child(section(
                        t!("access.recent"),
                        SharedString::default(),
                        recent_rows,
                        cx,
                    ))
                })
                .into_any_element()
        };
        v_flex()
            .size_full()
            .child(ui::section_header(
                t!("access.title"),
                t!("access.subtitle"),
                Button::new("access-refresh")
                    .icon(ui::icon(IconName::RefreshCw))
                    .label(t!("common.refresh"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.refresh(cx))),
                cx,
            ))
            .child(
                div()
                    .id("access-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}
