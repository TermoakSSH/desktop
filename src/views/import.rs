//! Import `~/.ssh/config`: first a preview (which hosts are created, which
//! are skipped and why, jump hosts, keys, tunnels and warnings) and then the
//! real import. It can be repeated: hosts that already exist are skipped.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Task, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme, Disableable, StyledExt, WindowExt, h_flex, v_flex};
use termoak_client::import::{ImportOptions, ImportReport};

use crate::runtime;
use crate::state::{AppModel, api_error};
use crate::ui::{self, IconName};

/// Delay after changing an option before repeating the preview.
const PREVIEW_DELAY: Duration = Duration::from_millis(400);

/// Opens the import dialog, with a file (by default `~/.ssh/config`).
pub fn open(model: Entity<AppModel>, path: Option<PathBuf>, window: &mut Window, cx: &mut App) {
    let dialog = cx.new(|cx| ImportDialog::new(model, path, window, cx));
    window.open_dialog(cx, move |d, _, _| {
        d.title(t!("import.title"))
            .w(px(680.))
            .overlay_closable(false)
            .child(dialog.clone())
    });
}

/// Path typed by the user, with a leading `~` as the home folder.
pub fn expand_path(text: &str, home: Option<PathBuf>) -> PathBuf {
    let text = text.trim();
    match (text.strip_prefix("~"), home) {
        (Some(rest), Some(home))
            if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') =>
        {
            home.join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(text),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Options as they are in the form (to know whether the preview is still
/// valid).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Form {
    path: String,
    group: String,
    device_only: bool,
}

impl Form {
    fn options(&self, dry_run: bool) -> ImportOptions {
        ImportOptions {
            dry_run,
            group: Some(self.group.trim().to_string()).filter(|g| !g.is_empty()),
            device_only: self.device_only,
        }
    }
}

/// Is there anything to import?
pub fn has_changes(r: &ImportReport) -> bool {
    !r.hosts_created.is_empty()
        || !r.jump_hosts_created.is_empty()
        || !r.keys_imported.is_empty()
        || r.forwards_created > 0
}

/// Summary for the final notification.
pub fn summary(r: &ImportReport) -> String {
    let mut parts = vec![tn!("import.summary.hosts", r.hosts_created.len()).to_string()];
    if !r.jump_hosts_created.is_empty() {
        parts.push(tn!("import.summary.jumps", r.jump_hosts_created.len()).to_string());
    }
    if !r.keys_imported.is_empty() {
        parts.push(tn!("import.summary.keys", r.keys_imported.len()).to_string());
    }
    if r.forwards_created > 0 {
        parts.push(tn!("import.summary.forwards", r.forwards_created).to_string());
    }
    parts.join(", ")
}

struct ImportDialog {
    model: Entity<AppModel>,
    path: Entity<InputState>,
    group: Entity<InputState>,
    device_only: bool,
    /// Preview and the options it was made with.
    preview: Option<(Form, ImportReport)>,
    error: Option<String>,
    previewing: bool,
    importing: bool,
    _preview_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl ImportDialog {
    fn new(
        model: Entity<AppModel>,
        path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let default = path
            .unwrap_or_else(termoak_ssh::sshconfig::default_path)
            .display()
            .to_string();
        let path = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("~/.ssh/config")
                .default_value(default)
        });
        let group =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("import.group_placeholder")));
        let subs = vec![
            cx.subscribe_in(&path, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::Change = ev {
                    this.schedule_preview(window, cx);
                }
            }),
            cx.subscribe_in(&group, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::Change = ev {
                    this.schedule_preview(window, cx);
                }
            }),
        ];
        let mut dialog = Self {
            model,
            path,
            group,
            device_only: false,
            preview: None,
            error: None,
            previewing: false,
            importing: false,
            _preview_task: None,
            _subs: subs,
        };
        dialog.run_preview(window, cx);
        dialog
    }

    fn form(&self, cx: &App) -> Form {
        Form {
            path: self.path.read(cx).value().trim().to_string(),
            group: self.group.read(cx).value().trim().to_string(),
            device_only: self.device_only,
        }
    }

    fn schedule_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._preview_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(PREVIEW_DELAY).await;
            let _ = this.update_in(cx, |this, window, cx| this.run_preview(window, cx));
        }));
    }

    /// Preview (dry-run import).
    fn run_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = self.form(cx);
        if form.path.is_empty() {
            self.preview = None;
            self.error = Some(t!("import.path_required").to_string());
            cx.notify();
            return;
        }
        let ws = self.model.read(cx).ws.clone();
        let path = expand_path(&form.path, home_dir());
        let opts = form.options(true);
        self.previewing = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                if !path.is_file() {
                    return Err(t!("import.file_missing", path = path.display()).to_string());
                }
                ws.import_ssh_config(&path, &opts)
                    .await
                    .map_err(|e| t!("import.read_failed", error = api_error(e)).to_string())
            },
            move |this, res, _, cx| {
                this.previewing = false;
                // If the options changed meanwhile, another one is on its way.
                if this.form(cx) != form {
                    return;
                }
                match res {
                    Ok(report) => {
                        this.preview = Some((form, report));
                        this.error = None;
                    }
                    Err(e) => {
                        this.preview = None;
                        this.error = Some(e);
                    }
                }
                cx.notify();
            },
        );
    }

    fn import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = self.form(cx);
        if self.preview.as_ref().map(|(f, _)| f) != Some(&form) {
            self.run_preview(window, cx);
            return;
        }
        let ws = self.model.read(cx).ws.clone();
        let path = expand_path(&form.path, home_dir());
        let opts = form.options(false);
        self.importing = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { ws.import_ssh_config(&path, &opts).await.map_err(api_error) },
            |this, res, window, cx| {
                this.importing = false;
                match res {
                    Ok(report) => {
                        this.model.update(cx, |m, cx| m.data_changed(cx));
                        window.close_dialog(cx);
                        ui::success(window, cx, t!("import.done", summary = summary(&report)));
                        if !report.warnings.is_empty() {
                            ui::notify(
                                window,
                                cx,
                                crate::state::ToastKind::Warning,
                                tn!("import.done_warnings", report.warnings.len()),
                            );
                        }
                    }
                    Err(e) => ui::error(window, cx, t!("import.failed", error = e)),
                }
                cx.notify();
            },
        );
    }

    fn render_report(&self, r: &ImportReport, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let mono = ui::mono_family(cx);
        let section =
            |title: SharedString, color: gpui::Hsla, icon: IconName, items: Vec<SharedString>| {
                v_flex()
                    .gap_1p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(ui::icon(icon).size(px(14.)).text_color(color))
                            .child(div().text_sm().font_semibold().child(title)),
                    )
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_1()
                            .children(items.into_iter().map(|t| {
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded(px(4.))
                                    .bg(theme.muted)
                                    .font_family(mono.clone())
                                    .text_xs()
                                    .child(t)
                            })),
                    )
            };
        let mut body = v_flex().gap_4();
        body = body.child(
            h_flex()
                .gap_1()
                .flex_wrap()
                .child(ui::pill(
                    tn!("import.pill.new_hosts", r.hosts_created.len()),
                    theme.success,
                ))
                .child(ui::pill(
                    tn!("import.pill.jumps", r.jump_hosts_created.len()),
                    theme.info,
                ))
                .child(ui::pill(
                    tn!("import.pill.new_keys", r.keys_imported.len()),
                    theme.primary,
                ))
                .child(ui::pill(
                    tn!("import.pill.reused_keys", r.keys_reused.len()),
                    theme.muted_foreground,
                ))
                .child(ui::pill(
                    tn!("import.pill.forwards", r.forwards_created),
                    theme.primary,
                ))
                .when(!r.hosts_skipped.is_empty(), |this| {
                    this.child(ui::pill(
                        tn!("import.pill.skipped", r.hosts_skipped.len()),
                        theme.warning,
                    ))
                })
                .when(!r.warnings.is_empty(), |this| {
                    this.child(ui::pill(
                        tn!("import.pill.warnings", r.warnings.len()),
                        theme.danger,
                    ))
                }),
        );
        if !has_changes(r) && r.hosts_skipped.is_empty() {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("import.nothing")),
            );
        }
        if !r.hosts_created.is_empty() {
            body = body.child(section(
                t!("import.section.hosts"),
                theme.success,
                IconName::Server,
                r.hosts_created.iter().map(|h| h.clone().into()).collect(),
            ));
        }
        if !r.jump_hosts_created.is_empty() {
            body = body.child(section(
                t!("import.section.jumps"),
                theme.info,
                IconName::ArrowLeftRight,
                r.jump_hosts_created
                    .iter()
                    .map(|h| h.clone().into())
                    .collect(),
            ));
        }
        if !r.keys_imported.is_empty() {
            body = body.child(section(
                t!("import.section.keys"),
                theme.primary,
                IconName::KeyRound,
                r.keys_imported.iter().map(|k| k.clone().into()).collect(),
            ));
        }
        if !r.keys_reused.is_empty() {
            body = body.child(section(
                t!("import.section.reused_keys"),
                theme.muted_foreground,
                IconName::KeyRound,
                r.keys_reused.iter().map(|k| k.clone().into()).collect(),
            ));
        }
        if !r.hosts_skipped.is_empty() {
            body = body.child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                ui::icon(IconName::CircleSlash)
                                    .size(px(14.))
                                    .text_color(theme.warning),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child(t!("import.section.skipped")),
                            ),
                    )
                    .children(r.hosts_skipped.iter().map(|s| {
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .child(
                                div()
                                    .font_family(mono.clone())
                                    .font_medium()
                                    .child(s.alias.clone()),
                            )
                            .child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(s.reason.clone()),
                            )
                    })),
            );
        }
        if !r.warnings.is_empty() {
            body = body.child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                ui::icon(IconName::TriangleAlert)
                                    .size(px(14.))
                                    .text_color(theme.danger),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child(t!("import.section.warnings")),
                            ),
                    )
                    .children(r.warnings.iter().map(|w| {
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("• {w}"))
                    })),
            );
        }
        body.into_any_element()
    }
}

impl Render for ImportDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let form = self.form(cx);
        let fresh = self
            .preview
            .as_ref()
            .filter(|(f, _)| *f == form)
            .map(|(_, r)| r.clone());
        let can_import = fresh.as_ref().is_some_and(has_changes);
        let report = self.preview.as_ref().map(|(_, r)| r.clone());
        let report_el = report.map(|r| self.render_report(&r, cx));
        let theme = cx.theme();
        v_flex()
            .gap_4()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("import.intro")),
            )
            .child(ui::field(t!("import.file"), Input::new(&self.path), cx))
            .child(
                h_flex()
                    .gap_4()
                    .items_end()
                    .child(div().flex_1().child(ui::field(
                        t!("import.group"),
                        Input::new(&self.group),
                        cx,
                    )))
                    .child(
                        div().pb_2().child(
                            Switch::new("import-device-only")
                                .label(t!("import.device_only"))
                                .checked(self.device_only)
                                .on_click(cx.listener(|this, v: &bool, window, cx| {
                                    this.device_only = *v;
                                    this.run_preview(window, cx);
                                })),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .id("import-preview")
                    .max_h(px(340.))
                    .overflow_y_scroll()
                    .p_3()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.secondary)
                    .child(
                        h_flex()
                            .gap_2()
                            .pb_2()
                            .items_center()
                            .child(div().text_sm().font_semibold().child(t!("import.preview")))
                            .when(self.previewing, |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(t!("import.calculating")),
                                )
                            }),
                    )
                    .when_some(self.error.clone(), |this, e| {
                        this.child(div().text_sm().text_color(theme.danger).child(e))
                    })
                    .children(report_el),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("import-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("import-preview-btn")
                            .icon(ui::icon(IconName::RefreshCw))
                            .label(t!("import.refresh_preview"))
                            .loading(self.previewing)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.run_preview(window, cx)
                            })),
                    )
                    .child(
                        Button::new("import-run")
                            .primary()
                            .icon(ui::icon(IconName::Import))
                            .label(t!("import.run"))
                            .loading(self.importing)
                            .disabled(!can_import || self.previewing)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.import(window, cx)
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_is_home() {
        let home = Some(PathBuf::from("/home/ana"));
        assert_eq!(
            expand_path("~/.ssh/config", home.clone()),
            PathBuf::from("/home/ana/.ssh/config")
        );
        assert_eq!(expand_path("~", home.clone()), PathBuf::from("/home/ana"));
        assert_eq!(
            expand_path(" /etc/ssh/ssh_config ", home.clone()),
            PathBuf::from("/etc/ssh/ssh_config")
        );
        // `~other` is not the home folder.
        assert_eq!(expand_path("~other/x", home), PathBuf::from("~other/x"));
    }

    #[test]
    fn report_summary() {
        let mut r = ImportReport::default();
        assert!(!has_changes(&r));
        r.hosts_created = vec!["web".into(), "db".into()];
        r.keys_imported = vec!["id_ed25519".into()];
        r.forwards_created = 1;
        assert!(has_changes(&r));
        assert_eq!(summary(&r), "2 hosts, 1 key, 1 tunnel");
        let only_forward = ImportReport {
            forwards_created: 3,
            ..Default::default()
        };
        assert!(has_changes(&only_forward));
    }
}
