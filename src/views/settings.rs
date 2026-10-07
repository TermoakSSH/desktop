//! Settings: the current account (with two-step verification), appearance,
//! language, terminal (with autocomplete), copy and paste, the hosts list
//! (status checks), notifications and updates. Accounts (sign in, sign out, the account of each server) and the
//! AI (where it runs, API keys, AI credit) have their own pages.

use gpui::{
    AppContext, ClickEvent, Context, Entity, EventEmitter, IntoElement, ParentElement, Render,
    Styled, Subscription, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::select::{Select, SelectEvent};
use gpui_component::switch::Switch;
use gpui_component::tab::{Tab, TabBar};
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};

use super::OpenRequest;
use super::accounts::AccountsView;
use super::add_account::{self, web_link};
use super::ai_settings::AiSettingsView;
use super::two_factor::TwoFactorPanel;
use crate::i18n;
use crate::runtime;
use crate::state::AppModel;
use crate::theme;
use crate::ui::{self, IconName};
use crate::update::{self, UpdateModel, UpdateStatus};

/// Pages of the settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    General,
    Accounts,
    Ai,
}

pub struct SettingsView {
    model: Entity<AppModel>,
    page: SettingsPage,
    /// Settings → AI.
    ai: Entity<AiSettingsView>,
    /// Settings → Accounts.
    accounts: Entity<AccountsView>,
    updates: Entity<UpdateModel>,
    font_family: Entity<InputState>,
    /// Interface language (`None` = the system language).
    language: ui::ChoiceState<Option<String>>,
    two_factor: Entity<TwoFactorPanel>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenRequest> for SettingsView {}

/// Choices of the language selector: the system language and every
/// available translation, by its own name.
fn language_items() -> Vec<ui::Choice<Option<String>>> {
    let system = i18n::system().unwrap_or(i18n::DEFAULT);
    let mut items = vec![ui::Choice::new(
        t!(
            "settings.language.system",
            language = i18n::language_name(system)
        ),
        None,
    )];
    items.extend(
        i18n::available()
            .into_iter()
            .map(|l| ui::Choice::new(i18n::language_name(l), Some(l.to_string()))),
    );
    items
}

impl SettingsView {
    pub fn new(
        model: Entity<AppModel>,
        updates: Entity<UpdateModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let m = model.read(cx);
        let family = m.settings.font_family.clone();
        let language_choice = m.settings.language.clone();
        let font_family = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("settings.placeholder.font_family"))
                .default_value(family)
        });
        let language = ui::choice_state(language_items(), Some(&language_choice), window, cx);
        let two_factor = cx.new(|cx| TwoFactorPanel::new(model.clone(), window, cx));
        let ai = cx.new(|cx| AiSettingsView::new(model.clone(), window, cx));
        let accounts = cx.new(|cx| AccountsView::new(model.clone(), window, cx));
        let subs = vec![
            cx.subscribe_in(
                &language,
                window,
                |this, _, ev: &SelectEvent<Vec<ui::Choice<Option<String>>>>, window, cx| {
                    let SelectEvent::Confirm(Some(choice)) = ev else {
                        return;
                    };
                    this.set_language(choice.clone(), window, cx);
                },
            ),
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.observe(&updates, |_, _, cx| cx.notify()),
            cx.observe(&two_factor, |_, _, cx| cx.notify()),
        ];
        Self {
            model,
            page: SettingsPage::General,
            ai,
            accounts,
            updates,
            font_family,
            language,
            two_factor,
            _subs: subs,
        }
    }

    /// Reloads what depends on the server (when the section is entered).
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page == SettingsPage::Ai {
            self.ai.update(cx, |v, cx| v.refresh(window, cx));
        }
    }

    /// Shows a page (the AI one reloads its data when it appears).
    pub fn show_page(&mut self, page: SettingsPage, window: &mut Window, cx: &mut Context<Self>) {
        if self.page == page {
            return;
        }
        self.page = page;
        self.refresh(window, cx);
        cx.notify();
    }

    /// A `termoak://invite?...` link opened from outside the app: "Add
    /// account" with the sign-up form filled in.
    pub fn open_invite_link(&mut self, link: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((server, token)) = add_account::parse_invite_link(link) else {
            return;
        };
        self.show_page(SettingsPage::Accounts, window, cx);
        add_account::open(
            self.model.clone(),
            add_account::Start::Invite { server, token },
            window,
            cx,
        );
    }

    fn set_dark(&mut self, dark: bool, window: &mut Window, cx: &mut Context<Self>) {
        theme::apply(dark, Some(window), cx);
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            s.dark = dark;
            m.save_settings(s, cx);
        });
    }

    fn set_language(
        &mut self,
        choice: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.model.read(cx).settings.language == choice {
            return;
        }
        self.model
            .update(cx, |m, cx| m.set_language(choice.clone(), cx));
        // "System default" names the system language in the new language.
        self.language.update(cx, |state, cx| {
            state.set_items(language_items(), window, cx);
            state.set_selected_value(&choice, window, cx);
        });
    }

    fn change_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            s.font_size = (s.font_size + delta).clamp(9., 28.);
            m.save_settings(s, cx);
        });
    }

    fn apply_font_family(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let family = self.font_family.read(cx).value().trim().to_string();
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            s.font_family = family;
            m.save_settings(s, cx);
        });
        ui::success(window, cx, t!("settings.font.updated"));
    }

    fn set_agent(&mut self, on: bool, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            s.use_agent = on;
            m.save_settings(s, cx);
        });
    }

    fn set_autocomplete(&mut self, on: bool, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            s.autocomplete = on;
            m.save_settings(s, cx);
        });
    }

    /// Changes the copy and paste preferences.
    fn set_paste(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut crate::state::Settings)) {
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            f(&mut s);
            m.save_settings(s, cx);
        });
    }

    fn render_paste(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let settings = self.model.read(cx).settings.clone();
        let muted = cx.theme().muted_foreground;
        let hint = |text: gpui::SharedString| div().text_xs().text_color(muted).child(text);
        let mac = cfg!(target_os = "macos");
        let right_click = settings.right_click;
        self.render_card(t!("settings.paste.title"), IconName::ClipboardPaste, cx)
            .child(hint(if mac {
                t!("settings.paste.shortcuts_macos")
            } else {
                t!("settings.paste.shortcuts")
            }))
            .when(!mac, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Switch::new("ctrl-v-pastes")
                                .label(t!("settings.paste.ctrl_v"))
                                .checked(settings.ctrl_v_pastes)
                                .on_click(cx.listener(|this, v: &bool, _, cx| {
                                    let v = *v;
                                    this.set_paste(cx, |s| s.ctrl_v_pastes = v)
                                })),
                        )
                        .child(hint(t!("settings.paste.ctrl_v_hint"))),
                )
            })
            .child(ui::field_with_hint(
                t!("settings.paste.right_click"),
                h_flex().gap_2().flex_wrap().children(
                    crate::terminal::paste::RightClick::ALL
                        .into_iter()
                        .enumerate()
                        .map(|(i, choice)| {
                            Button::new(("right-click", i))
                                .small()
                                .label(match choice {
                                    crate::terminal::paste::RightClick::Menu => {
                                        t!("settings.paste.right_click_menu")
                                    }
                                    crate::terminal::paste::RightClick::Paste => {
                                        t!("settings.paste.right_click_paste")
                                    }
                                    crate::terminal::paste::RightClick::CopyOrPaste => {
                                        t!("settings.paste.right_click_copy_paste")
                                    }
                                })
                                .when(right_click == choice, |b| b.primary())
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.set_paste(cx, |s| s.right_click = choice)
                                }))
                        }),
                ),
                t!("settings.paste.right_click_hint"),
                cx,
            ))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Switch::new("copy-on-select")
                            .label(t!("settings.paste.copy_on_select"))
                            .checked(settings.copy_on_select)
                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                let v = *v;
                                this.set_paste(cx, |s| s.copy_on_select = v)
                            })),
                    )
                    .child(hint(t!("settings.paste.copy_on_select_hint"))),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Switch::new("confirm-multiline")
                            .label(t!("settings.paste.confirm_multiline"))
                            .checked(settings.confirm_multiline_paste)
                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                let v = *v;
                                this.set_paste(cx, |s| s.confirm_multiline_paste = v)
                            })),
                    )
                    .child(hint(t!("settings.paste.confirm_multiline_hint"))),
            )
    }

    /// Changes the notification preferences. Turning them on asks macOS
    /// for permission (only the first time does the system ask).
    fn set_notifications(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut crate::notifications::NotificationPrefs),
    ) {
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            let was = s.notifications.enabled;
            f(&mut s.notifications);
            if s.notifications.enabled && !was {
                crate::notifications::request_authorization();
            }
            m.save_settings(s, cx);
        });
    }

    fn render_notifications(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let prefs = self.model.read(cx).settings.notifications;
        let muted = cx.theme().muted_foreground;
        let hint = |text: gpui::SharedString| div().text_xs().text_color(muted).child(text);
        let off = !prefs.enabled;
        self.render_card(t!("settings.notifications.title"), IconName::Bell, cx)
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Switch::new("notifications-enabled")
                            .label(t!("settings.notifications.enabled"))
                            .checked(prefs.enabled)
                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                let v = *v;
                                this.set_notifications(cx, |p| p.enabled = v)
                            })),
                    )
                    .child(hint(if cfg!(target_os = "macos") {
                        t!("settings.notifications.enabled_hint_macos")
                    } else {
                        t!("settings.notifications.enabled_hint")
                    })),
            )
            .child(
                v_flex()
                    .gap_3()
                    .pl_4()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                Switch::new("notifications-sharing")
                                    .label(t!("settings.notifications.sharing"))
                                    .checked(prefs.sharing)
                                    .disabled(off)
                                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                                        let v = *v;
                                        this.set_notifications(cx, |p| p.sharing = v)
                                    })),
                            )
                            .child(hint(t!("settings.notifications.sharing_hint"))),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                Switch::new("notifications-shared")
                                    .label(t!("settings.notifications.shared_with_me"))
                                    .checked(prefs.shared_with_me)
                                    .disabled(off)
                                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                                        let v = *v;
                                        this.set_notifications(cx, |p| p.shared_with_me = v)
                                    })),
                            )
                            .child(hint(t!("settings.notifications.shared_with_me_hint"))),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                Switch::new("notifications-ai")
                                    .label(t!("settings.notifications.ai"))
                                    .checked(prefs.ai)
                                    .disabled(off)
                                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                                        let v = *v;
                                        this.set_notifications(cx, |p| p.ai = v)
                                    })),
                            )
                            .child(hint(t!("settings.notifications.ai_hint"))),
                    ),
            )
    }

    /// Hosts list: the status check of the hosts.
    fn render_hosts_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let settings = self.model.read(cx).settings.clone();
        let muted = cx.theme().muted_foreground;
        let hint = |text: gpui::SharedString| div().text_xs().text_color(muted).child(text);
        self.render_card(
            t!("settings.hosts_tabs.title"),
            IconName::LayoutPanelLeft,
            cx,
        )
        .child(
            v_flex()
                .gap_1()
                .child(
                    Switch::new("host-status")
                        .label(t!("settings.hosts_tabs.host_status"))
                        .checked(settings.host_status)
                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                            let v = *v;
                            this.set_paste(cx, |s| s.host_status = v)
                        })),
                )
                .child(hint(t!("settings.hosts_tabs.host_status_hint"))),
        )
    }

    fn clear_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ws = self.model.read(cx).ws.clone();
        ui::confirm(
            window,
            cx,
            t!("settings.history.clear_title"),
            t!("settings.history.clear_message"),
            t!("common.delete"),
            true,
            move |window, cx| {
                let ws = ws.clone();
                let task = runtime::spawn(cx, async move { ws.clear_history(None).await });
                window
                    .spawn(cx, async move |cx| {
                        let res = task.await;
                        let _ = cx.update(|window, cx| match res {
                            Ok(()) => ui::success(window, cx, t!("settings.history.cleared")),
                            Err(e) => ui::error(
                                window,
                                cx,
                                t!("settings.history.clear_failed", error = e),
                            ),
                        });
                    })
                    .detach();
            },
        );
    }

    fn render_card(
        &self,
        title: impl Into<gpui::SharedString>,
        icon: IconName,
        cx: &Context<Self>,
    ) -> gpui::Div {
        ui::card(cx).p_5().gap_4().w_full().max_w(px(760.)).child(
            h_flex()
                .gap_2()
                .items_center()
                .child(ui::icon(icon).size(px(18.)).text_color(cx.theme().primary))
                .child(div().text_lg().font_semibold().child(title.into())),
        )
    }

    fn render_appearance(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let settings = self.model.read(cx).settings.clone();
        let dark = cx.theme().is_dark();
        self.render_card(t!("settings.appearance.title"), IconName::Palette, cx)
            .child(ui::field_with_hint(
                t!("settings.language.label"),
                div().w(px(280.)).child(Select::new(&self.language)),
                t!("settings.language.hint"),
                cx,
            ))
            .child(ui::field(
                t!("settings.appearance.theme"),
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("theme-dark")
                            .icon(ui::icon(IconName::Moon))
                            .label(t!("settings.appearance.dark"))
                            .when(dark, |b| b.primary())
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.set_dark(true, window, cx)
                            })),
                    )
                    .child(
                        Button::new("theme-light")
                            .icon(ui::icon(IconName::Sun))
                            .label(t!("settings.appearance.light"))
                            .when(!dark, |b| b.primary())
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.set_dark(false, window, cx)
                            })),
                    ),
                cx,
            ))
            .child(ui::field(
                t!("settings.font.size"),
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("font-smaller")
                            .small()
                            .icon(ui::icon(IconName::Minus))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.change_font_size(-1., cx)
                            })),
                    )
                    .child(
                        div()
                            .w(px(48.))
                            .text_center()
                            .child(format!("{:.0} px", settings.font_size)),
                    )
                    .child(
                        Button::new("font-bigger")
                            .small()
                            .icon(ui::icon(IconName::Plus))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.change_font_size(1., cx)
                            })),
                    ),
                cx,
            ))
            .child(ui::field_with_hint(
                t!("settings.font.family"),
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.font_family)))
                    .child(
                        Button::new("font-apply")
                            .label(t!("settings.font.apply"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.apply_font_family(window, cx)
                            })),
                    ),
                t!("settings.font.family_hint"),
                cx,
            ))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Switch::new("use-agent")
                            .label(t!("settings.agent.label"))
                            .checked(settings.use_agent)
                            .on_click(cx.listener(|this, v: &bool, _, cx| this.set_agent(*v, cx))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t!("settings.agent.hint")),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                Switch::new("autocomplete")
                                    .label(t!("settings.autocomplete.label"))
                                    .checked(settings.autocomplete)
                                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                                        this.set_autocomplete(*v, cx)
                                    })),
                            )
                            .child(
                                Button::new("clear-history")
                                    .xsmall()
                                    .ghost()
                                    .icon(ui::icon(IconName::Trash))
                                    .label(t!("settings.history.clear"))
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.clear_history(window, cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t!("settings.autocomplete.hint")),
                    ),
            )
    }

    /// The current account: who, which server, sync and two-step
    /// verification; the accounts themselves are managed in their page.
    fn render_account(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let m = self.model.read(cx);
        let logged_in = m.logged_in();
        let user = m.server_user.clone().unwrap_or_default();
        let display_name =
            m.me.as_ref()
                .map(|u| u.name.clone())
                .filter(|n| !n.is_empty());
        let is_admin = m.is_admin();
        let url = m.server_url.clone().unwrap_or_default();
        let syncing = m.syncing;
        let last = m.last_sync.clone();
        let online = m.events_online;
        let accounts = m.accounts.len();
        let pending = m.pending_verification.clone();
        let theme = cx.theme();
        let card = self.render_card(t!("settings.account.title"), IconName::Cloud, cx);
        let manage = Button::new("manage-accounts")
            .icon(ui::icon(IconName::Users))
            .label(tn!("settings.account.manage", accounts))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.show_page(SettingsPage::Accounts, window, cx)
            }));
        if !logged_in {
            return card
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(if accounts == 0 {
                            t!("settings.account.none")
                        } else {
                            t!("settings.account.current_signed_out")
                        }),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .when_some(pending, |this, p| {
                            let model = self.model.clone();
                            this.child(
                                Button::new("enter-code")
                                    .primary()
                                    .icon(ui::icon(IconName::Mail))
                                    .label(t!("accounts.enter_code"))
                                    .on_click(move |_: &ClickEvent, window, cx| {
                                        add_account::open(
                                            model.clone(),
                                            add_account::Start::Verify(p.account),
                                            window,
                                            cx,
                                        )
                                    }),
                            )
                        })
                        .child(
                            Button::new("add-account")
                                .primary()
                                .icon(ui::icon(IconName::UserPlus))
                                .label(t!("accounts.add"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    add_account::open(
                                        this.model.clone(),
                                        add_account::Start::Choose,
                                        window,
                                        cx,
                                    )
                                })),
                        )
                        .when(accounts > 0, |this| this.child(manage)),
                );
        }
        let (last_text, last_color) = match &last {
            Some(Ok(sync)) => (
                t!(
                    "settings.account.last_sync",
                    changes = t!(
                        "settings.account.sync_changes",
                        pushed = sync.pushed,
                        pulled = sync.pulled
                    ),
                    time = sync.at.format("%H:%M:%S")
                ),
                theme.muted_foreground,
            ),
            Some(Err(e)) => (t!("settings.account.sync_error", error = e), theme.danger),
            None => (t!("settings.account.never_synced"), theme.muted_foreground),
        };
        card.child(
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .when_some(display_name, |this, n| {
                            this.child(div().font_semibold().child(n))
                        })
                        .child(div().font_medium().child(user))
                        .child(ui::pill(
                            if online {
                                t!("settings.account.online")
                            } else {
                                t!("settings.account.offline")
                            },
                            if online { theme.success } else { theme.warning },
                        ))
                        .when(is_admin, |this| {
                            this.child(ui::pill(t!("settings.account.server_admin"), theme.primary))
                        }),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(url),
                )
                .child(div().text_sm().text_color(last_color).child(last_text)),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("settings.account.sync_hint")),
        )
        .child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .child(
                    Button::new("sync-now")
                        .primary()
                        .icon(ui::icon(IconName::RefreshCw))
                        .label(t!("settings.account.sync_now"))
                        .loading(syncing)
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.model.update(cx, |m, cx| m.sync_now(cx));
                        })),
                )
                .child(
                    Button::new("web-account")
                        .icon(ui::icon(IconName::ExternalLink))
                        .label(t!("settings.account.web_account"))
                        .tooltip(t!("settings.account.web_account_tooltip"))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            let url = this.model.read(cx).server_url.clone().unwrap_or_default();
                            if let Some(link) = web_link(&url, "/app/account") {
                                cx.open_url(&link);
                            }
                        })),
                )
                .child(manage),
        )
        .child(self.two_factor.clone())
    }

    fn render_updates(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let u = self.updates.read(cx);
        let status = u.status.clone();
        let text = u.status_text();
        let enabled = status != UpdateStatus::Disabled;
        let ready = u.ready_version().is_some();
        let busy = matches!(
            status,
            UpdateStatus::Checking | UpdateStatus::Downloading(_)
        );
        let available = matches!(status, UpdateStatus::Available(_));
        self.render_card(t!("settings.updates.title"), IconName::Download, cx)
            .child(
                v_flex()
                    .gap_1()
                    .child(div().font_medium().child(t!(
                        "settings.updates.installed",
                        version = update::current_version()
                    )))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(text),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("check-updates")
                            .icon(ui::icon(IconName::RefreshCw))
                            .label(t!("settings.updates.check"))
                            .loading(busy)
                            .disabled(!enabled || ready)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.updates.update(cx, |u, cx| u.check_now(cx));
                            })),
                    )
                    .when(ready, |this| {
                        this.child(
                            Button::new("restart-update")
                                .primary()
                                .label(t!("settings.updates.restart"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.updates.read(cx).restart_now();
                                })),
                        )
                    })
                    .when(available, |this| {
                        this.child(
                            Button::new("open-releases")
                                .icon(ui::icon(IconName::ExternalLink))
                                .label(t!("settings.updates.download"))
                                .on_click(|_: &ClickEvent, _, cx| {
                                    cx.open_url(update::RELEASES_PAGE)
                                }),
                        )
                    }),
            )
    }

    fn render_about(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let dir = self.model.read(cx).ws.dir.display().to_string();
        self.render_card(t!("settings.about.title"), IconName::Info, cx)
            .child(div().text_sm().child(t!("settings.about.description")))
            .child(ui::field(
                t!("settings.about.local_data"),
                div().text_sm().font_family(ui::mono_family(cx)).child(dir),
                cx,
            ))
            .child(
                Button::new("repo")
                    .small()
                    .ghost()
                    .icon(ui::icon(IconName::Github))
                    .label(t!("settings.about.source_code"))
                    .on_click(|_: &ClickEvent, _, cx| {
                        cx.open_url("https://github.com/TermoakSSH/desktop")
                    }),
            )
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pages = TabBar::new("settings-pages")
            .segmented()
            .selected_index(match self.page {
                SettingsPage::General => 0,
                SettingsPage::Accounts => 1,
                SettingsPage::Ai => 2,
            })
            .child(
                Tab::new()
                    .icon(ui::icon(IconName::Settings))
                    .label(t!("settings.page.general")),
            )
            .child(
                Tab::new()
                    .icon(ui::icon(IconName::Users))
                    .label(t!("settings.page.accounts")),
            )
            .child(
                Tab::new()
                    .icon(ui::icon(IconName::Sparkles))
                    .label(t!("settings.page.ai")),
            )
            .on_click(cx.listener(|this, ix: &usize, window, cx| {
                let page = match *ix {
                    1 => SettingsPage::Accounts,
                    2 => SettingsPage::Ai,
                    _ => SettingsPage::General,
                };
                this.show_page(page, window, cx);
            }));
        let header = ui::section_header(
            t!("settings.title"),
            match self.page {
                SettingsPage::General => t!("settings.subtitle"),
                SettingsPage::Accounts => t!("settings.subtitle_accounts"),
                SettingsPage::Ai => t!("settings.subtitle_ai"),
            },
            pages,
            cx,
        );
        match self.page {
            SettingsPage::Ai => {
                return v_flex()
                    .size_full()
                    .child(header)
                    .child(div().flex_1().min_h_0().child(self.ai.clone()));
            }
            SettingsPage::Accounts => {
                return v_flex()
                    .size_full()
                    .child(header)
                    .child(div().flex_1().min_h_0().child(self.accounts.clone()));
            }
            SettingsPage::General => {}
        }
        let account = self.render_account(cx);
        let appearance = self.render_appearance(cx);
        let paste = self.render_paste(cx);
        let notifications = self.render_notifications(cx);
        let hosts_tabs = self.render_hosts_tabs(cx);
        let updates = self.render_updates(cx);
        let about = self.render_about(cx);
        v_flex().size_full().child(header).child(
            div().flex_1().min_h_0().child(
                v_flex().size_full().overflow_y_scrollbar().child(
                    v_flex()
                        .p_6()
                        .gap_5()
                        .child(account)
                        .child(appearance)
                        .child(paste)
                        .child(hosts_tabs)
                        .child(notifications)
                        .child(updates)
                        .child(about),
                ),
            ),
        )
    }
}
