//! Activity of a server session (owner only): who had the keyboard and
//! when, read from the author marks of its recording (`GET
//! /sessions/{id}/recording/authors`). Only who typed and when is kept,
//! not what.

use gpui::{
    App, AppContext, Context, IntoElement, ParentElement, Render, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::scroll::ScrollableElement;
use gpui_component::spinner::Spinner;
use gpui_component::{ActiveTheme, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use serde_json::Value;
use termoak_client::ApiClient;
use termoak_core::Id;

use crate::runtime;
use crate::sharing::{AuthorPeriod, author_periods};
use crate::state::api_error;
use crate::ui;

/// What the dialog shows.
enum Load {
    Loading,
    /// The session was not recorded.
    NoRecording,
    Failed(String),
    /// When the recording started (ms) and who typed.
    Done(Option<i64>, Vec<AuthorPeriod>),
}

/// Opens the activity of the session `session_id` (yours).
pub fn open(api: ApiClient, session_id: Id, title: String, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| ActivityView::new(api, session_id, window, cx));
    window.open_dialog(cx, move |d, _, _| {
        d.title(t!(
            "server_sessions.activity.title_of",
            title = title.clone()
        ))
        .w(px(520.))
        .child(view.clone())
    });
}

struct ActivityView {
    load: Load,
}

impl ActivityView {
    fn new(api: ApiClient, session_id: Id, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = format!("/api/v1/sessions/{session_id}/recording/authors");
        runtime::run_in(
            cx,
            window,
            async move {
                match api.get::<Value>(&path).await {
                    Ok(v) => Ok(Some(v)),
                    Err(e) if e.api_code() == Some("recording_not_found") => Ok(None),
                    Err(e) => Err(api_error(e)),
                }
            },
            |this, res, _, cx| {
                this.load = match res {
                    Ok(Some(v)) => {
                        let (start, periods) = author_periods(&v);
                        Load::Done(start, periods)
                    }
                    Ok(None) => Load::NoRecording,
                    Err(e) => Load::Failed(e),
                };
                cx.notify();
            },
        );
        Self {
            load: Load::Loading,
        }
    }
}

impl Render for ActivityView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = |text: gpui::SharedString| {
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        match &self.load {
            Load::Loading => h_flex()
                .gap_2()
                .items_center()
                .child(Spinner::new().small())
                .child(muted(t!("common.loading")))
                .into_any_element(),
            Load::NoRecording => {
                muted(t!("server_sessions.activity.no_recording")).into_any_element()
            }
            Load::Failed(e) => div()
                .text_sm()
                .text_color(theme.danger)
                .child(t!("server_sessions.activity.failed", error = e.clone()))
                .into_any_element(),
            Load::Done(start, periods) => v_flex()
                .gap_2()
                .child(muted(match start {
                    Some(ms) => t!(
                        "server_sessions.activity.started",
                        date = ui::format_ms(*ms)
                    ),
                    None => t!("server_sessions.activity.hint"),
                }))
                .when(periods.is_empty(), |this| {
                    this.child(muted(t!("server_sessions.activity.none")))
                })
                .child(
                    v_flex()
                        .max_h(px(360.))
                        .overflow_y_scrollbar()
                        .gap_1()
                        .children(periods.iter().map(|p| {
                            h_flex()
                                .gap_3()
                                .items_center()
                                .py_1()
                                .border_b_1()
                                .border_color(theme.border)
                                .child(
                                    div()
                                        .w(px(130.))
                                        .flex_shrink_0()
                                        .text_xs()
                                        .font_family(ui::mono_family(cx))
                                        .text_color(theme.muted_foreground)
                                        .child(p.span()),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_sm()
                                        .font_medium()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(if p.name.is_empty() {
                                            p.kind_label().to_string()
                                        } else {
                                            p.name.clone()
                                        }),
                                )
                                .child(ui::pill(
                                    p.kind_label(),
                                    if p.kind == "owner" {
                                        theme.muted_foreground
                                    } else {
                                        theme.info
                                    },
                                ))
                        })),
                )
                .into_any_element(),
        }
    }
}
