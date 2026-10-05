//! Joining a shared session with a link: `https://<server>/join/<token>`
//! (the web page), or `termoak://join?server=…&token=…` (the app link).
//!
//! The link is read first (`GET /api/v1/join/{token}`, no account needed):
//! whose session it is, its title, whether you can ask for the keyboard and
//! whether the owner has to let you in. Signed in to that server, you join
//! with your account; otherwise you choose the name the others see.

use std::rc::Rc;

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Subscription, Task, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::spinner::Spinner;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use serde_json::Value;
use termoak_client::{ApiClient, ClientError};

use super::OpenRequest;
use crate::runtime;
use crate::sharing::{self, JoinInfo, JoinLink};
use crate::state::{AppModel, api_error};
use crate::terminal::backend::LinkJoin;
use crate::ui::{self, IconName};

/// What to do with the chosen session (open a tab).
pub type OnJoin = Rc<dyn Fn(OpenRequest, &mut Window, &mut App)>;

/// Reading the link.
#[derive(Clone)]
enum Check {
    /// Nothing (or not a link yet).
    Empty,
    Invalid,
    Loading,
    Ready(JoinLink, JoinInfo),
    Failed(String),
}

/// Content of the "Join with link" dialog.
pub struct JoinDialog {
    model: Entity<AppModel>,
    link: Entity<InputState>,
    name: Entity<InputState>,
    check: Check,
    /// Link being read (to ignore older answers).
    reading: Option<JoinLink>,
    _task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

/// Opens the dialog, with the link already filled in if there is one.
pub fn open(
    model: Entity<AppModel>,
    link: Option<String>,
    on_join: OnJoin,
    window: &mut Window,
    cx: &mut App,
) {
    let dialog = cx.new(|cx| JoinDialog::new(model, link, window, cx));
    window.open_dialog(cx, move |d, _, cx| {
        let ready = dialog.read(cx).ready(cx);
        let join = {
            let dialog = dialog.clone();
            let on_join = on_join.clone();
            move |window: &mut Window, cx: &mut App| {
                if let Some(req) = dialog.read(cx).request(cx) {
                    window.close_dialog(cx);
                    on_join(req, window, cx);
                }
            }
        };
        let join_ok = join.clone();
        d.title(t!("join.title"))
            .w(px(520.))
            .on_ok(move |_, window, cx| {
                join_ok(window, cx);
                false
            })
            .child(dialog.clone())
            .footer(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("join-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("join-go")
                            .primary()
                            .icon(ui::icon(IconName::LogIn))
                            .label(t!("join.join"))
                            .disabled(!ready)
                            .on_click(move |_: &ClickEvent, window, cx| join(window, cx)),
                    ),
            )
    });
}

impl JoinDialog {
    fn new(
        model: Entity<AppModel>,
        link: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let link_input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder(t!("join.link_placeholder"));
            if let Some(l) = &link {
                s.set_value(l.clone(), window, cx);
            }
            s
        });
        // Your name on your own server is a good guess for others.
        let my_name = model
            .read(cx)
            .me
            .as_ref()
            .map(|u| u.name.clone())
            .filter(|n| !n.trim().is_empty());
        let name = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder(t!("join.name_placeholder"));
            if let Some(n) = my_name {
                s.set_value(n, window, cx);
            }
            s
        });
        let subs = vec![
            cx.subscribe_in(&link_input, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    this.read_link(cx);
                }
            }),
            cx.subscribe_in(&name, window, |_, _, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    cx.notify();
                }
            }),
        ];
        let mut this = Self {
            model,
            link: link_input,
            name,
            check: Check::Empty,
            reading: None,
            _task: None,
            _subs: subs,
        };
        this.read_link(cx);
        ui::focus_later(
            if link.is_some() {
                &this.name
            } else {
                &this.link
            },
            window,
            cx,
        );
        this
    }

    /// You are signed in to the server of the link: you join with your
    /// account.
    fn signed_in_to(&self, link: &JoinLink, cx: &App) -> bool {
        let m = self.model.read(cx);
        m.api.is_some() && m.server_url.as_deref().is_some_and(|u| link.same_server(u))
    }

    fn guest_name(&self, cx: &App) -> String {
        sharing::clean_guest_name(&self.name.read(cx).value())
    }

    /// Ready to join.
    fn ready(&self, cx: &App) -> bool {
        match &self.check {
            Check::Ready(link, _) => self.signed_in_to(link, cx) || !self.guest_name(cx).is_empty(),
            _ => false,
        }
    }

    /// The tab to open.
    fn request(&self, cx: &App) -> Option<OpenRequest> {
        let Check::Ready(link, info) = &self.check else {
            return None;
        };
        if !self.ready(cx) {
            return None;
        }
        let (api, guest_name) = if self.signed_in_to(link, cx) {
            (self.model.read(cx).api.clone()?, None)
        } else {
            (
                ApiClient::new(&link.server).ok()?,
                Some(self.guest_name(cx)),
            )
        };
        let title = if info.owner.is_empty() {
            info.title.clone()
        } else {
            format!("{} · {}", info.title, info.owner)
        };
        Some(OpenRequest::JoinLink {
            session_id: info.session_id,
            title,
            link: LinkJoin {
                api,
                ws_path: info.ws_path.clone(),
                guest_name,
            },
        })
    }

    /// Reads the link in the field (if it changed).
    fn read_link(&mut self, cx: &mut Context<Self>) {
        let text = self.link.read(cx).value().to_string();
        let Some(link) = sharing::parse_join_link(&text) else {
            self.reading = None;
            self._task = None;
            self.check = if text.trim().is_empty() {
                Check::Empty
            } else {
                Check::Invalid
            };
            cx.notify();
            return;
        };
        if self.reading.as_ref() == Some(&link) {
            return;
        }
        self.reading = Some(link.clone());
        self.check = Check::Loading;
        cx.notify();
        let fetch = runtime::spawn(cx, join_info(link.clone()));
        self._task = Some(cx.spawn(async move |this, cx| {
            let res = fetch.await;
            let _ = this.update(cx, |this, cx| {
                if this.reading.as_ref() != Some(&link) {
                    return;
                }
                this.check = match res {
                    Ok(info) => Check::Ready(link, info),
                    Err(e) => Check::Failed(e),
                };
                cx.notify();
            });
        }));
    }
}

/// Public data of a link (no account needed).
async fn join_info(link: JoinLink) -> Result<JoinInfo, String> {
    let url = format!("{}{}", link.server, link.info_path());
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .user_agent(concat!("Termoak/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(&url).send().await.map_err(|e| {
        tracing::warn!(error = %e, "could not read the link");
        t!("error.network").to_string()
    })?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(api_error(ClientError::Api {
            status: status.as_u16(),
            code: v["error"]["code"]
                .as_str()
                .unwrap_or("invalid_link")
                .to_string(),
            message: v["error"]["message"].as_str().unwrap_or("").to_string(),
        }));
    }
    JoinInfo::from_json(&v).ok_or_else(|| t!("join.not_a_session").to_string())
}

impl Render for JoinDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let status: Option<gpui::AnyElement> = match &self.check {
            Check::Empty => Some(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("join.hint"))
                    .into_any_element(),
            ),
            Check::Invalid => Some(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(t!("join.invalid_link"))
                    .into_any_element(),
            ),
            Check::Loading => Some(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .text_color(muted)
                    .child(Spinner::new().small())
                    .child(t!("join.checking"))
                    .into_any_element(),
            ),
            Check::Failed(e) => Some(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(t!("join.failed", error = e.clone()))
                    .into_any_element(),
            ),
            Check::Ready(..) => None,
        };
        let info = match &self.check {
            Check::Ready(link, info) => Some((link.clone(), info.clone())),
            _ => None,
        };
        let signed_in = info
            .as_ref()
            .is_some_and(|(link, _)| self.signed_in_to(link, cx));
        let me = self.model.read(cx).server_user.clone();
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .child(ui::field(t!("join.link"), Input::new(&self.link), cx))
            .children(status)
            .when_some(info, |this, (link, info)| {
                let owner = if info.owner.is_empty() {
                    t!("share.role.owner").to_string()
                } else {
                    info.owner.clone()
                };
                let mut facts: Vec<(IconName, String)> = vec![(
                    if info.control {
                        IconName::Keyboard
                    } else {
                        IconName::Eye
                    },
                    if info.control {
                        t!("join.permission_control").to_string()
                    } else {
                        t!("join.permission_view").to_string()
                    },
                )];
                if info.require_approval {
                    facts.push((
                        IconName::DoorOpen,
                        t!("join.needs_approval", owner = owner.clone()).to_string(),
                    ));
                }
                if info.participants > 0 {
                    facts.push((
                        IconName::Users,
                        tn!("join.participants", info.participants).to_string(),
                    ));
                }
                if let Some(exp) = info.expires_at {
                    facts.push((
                        IconName::Timer,
                        t!("join.expires", date = ui::format_ms(exp)).to_string(),
                    ));
                }
                this.child(
                    ui::card(cx)
                        .p_4()
                        .gap_2()
                        .child(div().text_lg().font_semibold().child(info.title.clone()))
                        .child(div().text_sm().text_color(theme.muted_foreground).child(t!(
                            "join.shared_by",
                            owner = owner,
                            server = link.server
                        )))
                        .children(facts.into_iter().map(|(icon, text)| {
                            h_flex()
                                .gap_2()
                                .items_center()
                                .text_sm()
                                .child(
                                    ui::icon(icon)
                                        .size(px(14.))
                                        .text_color(theme.muted_foreground),
                                )
                                .child(text)
                        })),
                )
                .child(if signed_in {
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(match me {
                            Some(user) => t!("join.as_user", user = user),
                            None => t!("join.as_account"),
                        })
                        .into_any_element()
                } else {
                    ui::field_with_hint(
                        t!("join.name"),
                        Input::new(&self.name),
                        t!("join.name_hint"),
                        cx,
                    )
                    .into_any_element()
                })
            })
    }
}
