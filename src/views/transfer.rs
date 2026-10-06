//! "Move to…" and "Copy to…": items go to another vault, another account
//! or This device. A dry run first shows what will happen (what moves, what
//! is copied along because other items still use it, which references are
//! cleared), then the user confirms.
//!
//! Inside one account the server does it (online); between This device and
//! an account, or between accounts, the app does it with the same planner.

use std::rc::Rc;

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Subscription, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::select::{Select, SelectEvent};
use gpui_component::{ActiveTheme, Disableable, StyledExt, WindowExt, h_flex, v_flex};
use termoak_client::{ItemRef, LocalTransfer, Scope};
use termoak_core::Id;
use termoak_core::model::EntityKind;
use termoak_core::transfer::{Dependencies, TransferMode, TransferResult};

use crate::accounts::{self, Destination};
use crate::state::{AppModel, TransferError};
use crate::ui::{self, Choice, ChoiceState, IconName};

/// Called after a transfer was made (e.g. to close an editor of the items,
/// which now live elsewhere).
pub type OnDone = Rc<dyn Fn(&mut Window, &mut App)>;

/// Opens "Move to…" or "Copy to…" for items of one place.
pub fn open(
    model: Entity<AppModel>,
    items: Vec<ItemRef>,
    mode: TransferMode,
    on_done: Option<OnDone>,
    window: &mut Window,
    cx: &mut App,
) {
    if items.is_empty() {
        return;
    }
    let view = cx.new(|cx| TransferDialog::new(model, items, mode, on_done, window, cx));
    window.open_dialog(cx, move |d, _, _| {
        d.title(match mode {
            TransferMode::Move => t!("transfer.move_title"),
            TransferMode::Copy => t!("transfer.copy_title"),
        })
        .w(px(520.))
        .overlay_closable(false)
        .child(view.clone())
    });
}

/// Where items can go from `from` (with vault `vault`): every writable
/// place except their own.
pub fn targets(model: &AppModel, from: Scope, vault: Option<Id>) -> Vec<Destination> {
    let infos = model.account_infos();
    accounts::destinations(&infos, &model.all_vaults(), from != Scope::Device)
        .into_iter()
        .filter(|d| {
            !(d.scope == from
                && match from {
                    Scope::Device => true,
                    Scope::Account(_) => d.vault.is_none() || d.vault == vault,
                })
        })
        .collect()
}

/// Label of an item of the lists, by kind and id.
pub fn item_name(model: &AppModel, kind: EntityKind, id: Id) -> String {
    let found = match kind {
        EntityKind::Host => model.host(id).map(|h| h.label.clone()),
        EntityKind::Group => model
            .groups
            .iter()
            .find(|g| g.data.id == id)
            .map(|g| g.data.name.clone()),
        EntityKind::Key => model
            .keys
            .iter()
            .find(|k| k.data.id == id)
            .map(|k| k.data.label.clone()),
        EntityKind::Identity => model
            .identities
            .iter()
            .find(|k| k.data.id == id)
            .map(|k| k.data.label.clone()),
        EntityKind::Snippet => model
            .snippets
            .iter()
            .find(|k| k.data.id == id)
            .map(|k| k.data.name.clone()),
        EntityKind::Forward => model
            .forwards
            .iter()
            .find(|k| k.data.id == id)
            .map(|k| k.data.label.clone()),
        EntityKind::KnownHost => model
            .known_hosts
            .iter()
            .find(|k| k.data.id == id)
            .map(|k| k.data.host.clone()),
        EntityKind::Memory => None,
    };
    found.unwrap_or_else(|| id.to_string()[..8].to_string())
}

enum Stage {
    Choosing,
    Planning,
    /// The dry run, waiting for confirmation.
    Review(TransferResult),
    /// Refused because items that stay use what moves.
    StillReferenced(String),
    Running,
}

struct TransferDialog {
    model: Entity<AppModel>,
    items: Vec<ItemRef>,
    mode: TransferMode,
    targets: Vec<Destination>,
    target: ChoiceState<usize>,
    stage: Stage,
    force: bool,
    on_done: Option<OnDone>,
    _subs: Vec<Subscription>,
}

impl TransferDialog {
    fn new(
        model: Entity<AppModel>,
        items: Vec<ItemRef>,
        mode: TransferMode,
        on_done: Option<OnDone>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let m = model.read(cx);
        let from = items[0].scope;
        let vault = m.locate_loaded(items[0].id).and_then(|(_, _, v)| v);
        let targets = targets(m, from, vault);
        let choices: Vec<Choice<usize>> = targets
            .iter()
            .enumerate()
            .map(|(i, d)| Choice::new(d.label.clone(), i))
            .collect();
        let target = ui::choice_state(choices, Some(&0), window, cx);
        let subs = vec![cx.subscribe_in(
            &target,
            window,
            |this, _, _: &SelectEvent<Vec<Choice<usize>>>, _, cx| {
                this.stage = Stage::Choosing;
                this.force = false;
                cx.notify();
            },
        )];
        Self {
            model,
            items,
            mode,
            targets,
            target,
            stage: Stage::Choosing,
            force: false,
            on_done,
            _subs: subs,
        }
    }

    fn destination(&self, cx: &App) -> Option<Destination> {
        ui::chosen(&self.target, cx).and_then(|i| self.targets.get(i).cloned())
    }

    fn request(&self, dest: &Destination, dry_run: bool) -> LocalTransfer {
        LocalTransfer {
            items: self.items.clone(),
            to: dest.scope,
            vault: dest.vault,
            mode: self.mode,
            dependencies: Dependencies::Auto,
            dry_run,
            force: self.force,
        }
    }

    fn plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dest) = self.destination(cx) else {
            return;
        };
        self.stage = Stage::Planning;
        cx.notify();
        let req = self.request(&dest, true);
        let task = self.model.update(cx, |m, cx| m.transfer(req, cx));
        cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.stage = match res {
                    Ok(plan) => Stage::Review(plan),
                    Err(TransferError {
                        text,
                        still_referenced: true,
                    }) => Stage::StillReferenced(text),
                    Err(e) => {
                        ui::error(window, cx, e.text);
                        Stage::Choosing
                    }
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dest) = self.destination(cx) else {
            return;
        };
        self.stage = Stage::Running;
        cx.notify();
        let req = self.request(&dest, false);
        let mode = self.mode;
        let n = self.items.len();
        let task = self.model.update(cx, |m, cx| m.transfer(req, cx));
        cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update_in(cx, |this, window, cx| match res {
                Ok(_) => {
                    ui::success(
                        window,
                        cx,
                        match mode {
                            TransferMode::Move => {
                                tn!("transfer.moved", n, target = dest.label.clone())
                            }
                            TransferMode::Copy => {
                                tn!("transfer.copied", n, target = dest.label.clone())
                            }
                        },
                    );
                    window.close_dialog(cx);
                    if let Some(done) = this.on_done.clone() {
                        done(window, cx);
                    }
                }
                Err(e) => {
                    ui::error(window, cx, e.text);
                    this.stage = Stage::Choosing;
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

impl Render for TransferDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        if self.targets.is_empty() {
            return v_flex()
                .gap_3()
                .child(div().text_sm().child(t!("transfer.no_targets")))
                .into_any_element();
        }
        let dest = self.destination(cx);
        let n = self.items.len();
        let names = {
            let m = self.model.read(cx);
            self.items
                .iter()
                .take(5)
                .map(|i| {
                    let kind = m
                        .locate_loaded(i.id)
                        .map_or(EntityKind::Host, |(kind, _, _)| kind);
                    item_name(m, kind, i.id)
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let lines: Vec<String> = match &self.stage {
            Stage::Review(plan) => {
                let m = self.model.read(cx);
                let requested: Vec<Id> = self.items.iter().map(|i| i.id).collect();
                accounts::transfer_lines(plan, self.mode, &requested, |kind, id| {
                    item_name(m, kind, id)
                })
            }
            _ => Vec::new(),
        };
        let headline = dest
            .as_ref()
            .map(|d| accounts::transfer_headline(self.mode, n, &d.label))
            .unwrap_or_default();
        let busy = matches!(self.stage, Stage::Planning | Stage::Running);
        let reviewing = matches!(self.stage, Stage::Review(_));
        let still = match &self.stage {
            Stage::StillReferenced(t) => Some(t.clone()),
            _ => None,
        };
        let device_target = dest.as_ref().is_some_and(|d| d.scope == Scope::Device);
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(tn!("transfer.items", n, names = names)),
            )
            .child(ui::field(
                t!("transfer.target"),
                Select::new(&self.target),
                cx,
            ))
            .when(device_target, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(t!("transfer.device_hint")),
                )
            })
            .when(reviewing, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .p_3()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.secondary)
                        .child(div().font_semibold().text_sm().child(headline.clone()))
                        .children(lines.iter().map(|l| {
                            h_flex()
                                .gap_2()
                                .items_start()
                                .text_sm()
                                .child(
                                    ui::icon(IconName::ChevronRight)
                                        .size(px(14.))
                                        .text_color(theme.muted_foreground),
                                )
                                .child(div().min_w_0().child(l.clone()))
                        })),
                )
            })
            .when_some(still, |this, text| {
                this.child(
                    v_flex()
                        .gap_2()
                        .p_3()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.warning)
                        .child(div().text_sm().child(text))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(t!("transfer.force_hint")),
                        ),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("transfer-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_: &ClickEvent, window, cx| window.close_dialog(cx)),
                    )
                    .map(|this| match &self.stage {
                        Stage::Review(_) | Stage::Running => this.child(
                            Button::new("transfer-run")
                                .primary()
                                .icon(ui::icon(IconName::ArrowRightLeft))
                                .label(match self.mode {
                                    TransferMode::Move => t!("transfer.move"),
                                    TransferMode::Copy => t!("transfer.copy"),
                                })
                                .loading(busy)
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.run(window, cx)
                                })),
                        ),
                        Stage::StillReferenced(_) => this.child(
                            Button::new("transfer-force")
                                .warning()
                                .label(t!("transfer.force"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.force = true;
                                    this.plan(window, cx)
                                })),
                        ),
                        _ => this.child(
                            Button::new("transfer-plan")
                                .primary()
                                .label(t!("transfer.review"))
                                .loading(busy)
                                .disabled(dest.is_none())
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.plan(window, cx)
                                })),
                        ),
                    }),
            )
            .into_any_element()
    }
}
