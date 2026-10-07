//! "Import/Export" menu of the hosts header and its dialogs.
//!
//! Import: pick a file (or, on Windows, PuTTY's registry) → preview: the
//! hosts that will be created, duplicates of the target vault (same
//! address, port and user) with Skip / Update / Create copy, the target
//! vault and an optional group, the column mapping of tables, the
//! passphrase of a Termoak export with secrets → import → summary.
//!
//! Export: Termoak JSON or CSV, a vault (or This device) and optionally a
//! group, and for JSON the passwords and private keys sealed with a
//! passphrase → save the file.

use std::path::{Path, PathBuf};

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    PathPromptOptions, Render, SharedString, StatefulInteractiveElement, Styled, Subscription,
    Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use gpui_component::select::{Select, SelectEvent};
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use termoak_client::Scope;
use termoak_core::Id;

use crate::accounts::{self, Destination};
use crate::importers::apply::{self, ExportRequest, Place, PlaceData, Summary};
use crate::importers::csv::{self, CsvTable, Field, Mapping};
use crate::importers::termoak::{self, ExportFile};
use crate::importers::{
    self, Action, DupPolicy, Duplicate, ExistingHost, ImportSet, Source, mobaxterm, putty,
    securecrt, termius, text, zoc,
};
use crate::runtime;
use crate::state::AppModel;
use crate::ui::{self, Choice, ChoiceState, IconName};

/// Icon of each source in the menu.
pub fn source_icon(source: Source) -> IconName {
    match source {
        Source::Termoak => IconName::Braces,
        Source::Csv => IconName::FileSpreadsheet,
        Source::SshConfig => IconName::FileCode,
        Source::Termius => IconName::SquareTerminal,
        Source::Putty => IconName::Computer,
        Source::MobaXterm => IconName::AppWindow,
        Source::SecureCrt => IconName::ShieldCheck,
        Source::Zoc => IconName::NotebookTabs,
    }
}

/// Items of the "Import/Export" menu.
pub fn menu(mut menu: PopupMenu, model: Entity<AppModel>) -> PopupMenu {
    let m = model.clone();
    menu = menu
        .item(
            PopupMenuItem::new(t!("import_export.menu.import"))
                .icon(ui::icon(IconName::Import))
                .on_click(move |_, window, cx| start_import(m.clone(), None, window, cx)),
        )
        .item({
            let m = model.clone();
            PopupMenuItem::new(t!("import_export.menu.export"))
                .icon(ui::icon(IconName::FileOutput))
                .on_click(move |_, window, cx| open_export(m.clone(), window, cx))
        })
        .separator();
    for source in Source::MENU {
        let m = model.clone();
        menu = menu.item(
            PopupMenuItem::new(t!("import_export.menu.from", name = source.name()))
                .icon(ui::icon(source_icon(source)))
                .on_click(move |_, window, cx| start_import(m.clone(), Some(source), window, cx)),
        );
    }
    menu
}

fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
}

// ---------------------------------------------------------------------------
// Reading the file
// ---------------------------------------------------------------------------

/// What was read, before the preview.
#[derive(Clone)]
enum Content {
    Set(ImportSet),
    /// A table whose columns can be mapped.
    Table {
        table: CsvTable,
        mapping: Mapping,
    },
    /// A Termoak export (its secrets may still be sealed).
    Termoak(Box<ExportFile>),
}

#[derive(Clone)]
struct Loaded {
    source: Source,
    /// File name, folder or "registry".
    origin: String,
    content: Content,
}

fn table_or_set(
    text: &str,
    fallback: impl FnOnce() -> Result<ImportSet, String>,
) -> Result<Content, String> {
    let table = csv::read(text);
    let mapping = csv::guess_mapping(&table);
    if mapping.is_usable() {
        return Ok(Content::Table { table, mapping });
    }
    fallback().map(Content::Set)
}

/// Reads a file of a source (or of the source it looks like).
fn read_file(path: &Path, source: Option<Source>) -> Result<Loaded, String> {
    let bytes =
        std::fs::read(path).map_err(|e| t!("import_export.error.read", error = e).to_string())?;
    let text = text::decode(&bytes);
    let source = source.unwrap_or_else(|| importers::detect(path, &text));
    let content = match source {
        Source::Termoak => {
            Content::Termoak(Box::new(termoak::parse(&text).map_err(|e| e.to_string())?))
        }
        Source::Csv => {
            let table = csv::read(&text);
            if table.rows.is_empty() {
                return Err(t!("import_export.error.no_hosts_found").to_string());
            }
            let mapping = csv::guess_mapping(&table);
            Content::Table { table, mapping }
        }
        Source::Termius => {
            let t = text.trim_start_matches('\u{feff}').trim_start();
            if t.starts_with('{') || t.starts_with('[') {
                Content::Set(termius::parse(&text)?)
            } else {
                table_or_set(&text, || termius::parse(&text))?
            }
        }
        Source::Putty => {
            let sessions = putty::parse_reg(&text);
            if sessions.is_empty() {
                return Err(t!("import_export.error.no_putty_sessions").to_string());
            }
            Content::Set(putty::to_set(&sessions))
        }
        Source::MobaXterm => Content::Set(mobaxterm::parse(&text)),
        Source::SecureCrt => {
            if text.trim_start().starts_with('<') {
                Content::Set(securecrt::parse_xml(&text)?)
            } else {
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                Content::Set(securecrt::from_ini_files(&[(name, text.clone())]))
            }
        }
        Source::Zoc => {
            let t = text.trim_start_matches('\u{feff}').trim_start();
            let first = t.lines().next().unwrap_or("");
            if !t.starts_with('<') && !first.contains('=') {
                table_or_set(&text, || zoc::parse(&text))?
            } else {
                Content::Set(zoc::parse(&text)?)
            }
        }
        Source::SshConfig => return Err(String::new()),
    };
    Ok(Loaded {
        source,
        origin: path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string()),
        content,
    })
}

/// Starts an import: the file dialog (or the registry), then the preview.
pub fn start_import(
    model: Entity<AppModel>,
    source: Option<Source>,
    window: &mut Window,
    cx: &mut App,
) {
    if source == Some(Source::SshConfig) {
        super::import::open(model, None, window, cx);
        return;
    }
    #[cfg(windows)]
    if source == Some(Source::Putty) {
        let task = runtime::spawn(cx, async {
            tokio::task::spawn_blocking(putty::read_registry)
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())
        });
        window
            .spawn(cx, async move |cx| {
                let res = task.await;
                let _ = cx.update(|window, cx| match res {
                    Ok(sessions) if !sessions.is_empty() => {
                        let loaded = Loaded {
                            source: Source::Putty,
                            origin: t!("import_export.registry").to_string(),
                            content: Content::Set(putty::to_set(&sessions)),
                        };
                        open_preview(model.clone(), loaded, window, cx);
                    }
                    _ => pick_file(model.clone(), source, window, cx),
                });
            })
            .detach();
        return;
    }
    pick_file(model, source, window, cx);
}

fn pick_file(model: Entity<AppModel>, source: Option<Source>, window: &mut Window, cx: &mut App) {
    let rx = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some(t!("import_export.open")),
    });
    window
        .spawn(cx, async move |cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            load_and_preview(model, path, source, cx);
        })
        .detach();
}

fn load_and_preview(
    model: Entity<AppModel>,
    path: PathBuf,
    source: Option<Source>,
    cx: &mut gpui::AsyncWindowContext,
) {
    let _ = cx.update(|window, cx| {
        let p = path.clone();
        let task = runtime::spawn(cx, async move {
            tokio::task::spawn_blocking(move || read_file(&p, source))
                .await
                .map_err(|e| e.to_string())?
        });
        window
            .spawn(cx, async move |cx| {
                let res = task.await;
                let _ = cx.update(|window, cx| match res {
                    Ok(loaded) => open_preview(model, loaded, window, cx),
                    // An ssh_config chosen with "Import…".
                    Err(e) if e.is_empty() => {
                        super::import::open(model, Some(path.clone()), window, cx)
                    }
                    Err(e) => ui::error(window, cx, e),
                });
            })
            .detach();
    });
}

fn open_preview(model: Entity<AppModel>, loaded: Loaded, window: &mut Window, cx: &mut App) {
    let title = t!("import_export.import_title", name = loaded.source.name());
    let dialog = cx.new(|cx| ImportDialog::new(model, loaded, window, cx));
    window.open_dialog(cx, move |d, _, _| {
        d.title(title.clone())
            .w(px(820.))
            .overlay_closable(false)
            .child(dialog.clone())
    });
}

// ---------------------------------------------------------------------------
// Import preview
// ---------------------------------------------------------------------------

enum Stage {
    Preview,
    Running,
    Done(Summary),
}

struct ImportDialog {
    model: Entity<AppModel>,
    loaded: Loaded,
    set: ImportSet,
    mapping: Vec<(Field, ChoiceState<Option<usize>>)>,
    passphrase: Entity<InputState>,
    unlocked: bool,
    unlock_error: Option<String>,
    targets: Vec<Destination>,
    target: ChoiceState<usize>,
    policy: ChoiceState<DupPolicy>,
    group: Entity<InputState>,
    included: Vec<bool>,
    /// The target's items (for duplicates and to reuse groups and keys).
    place: Option<(Place, PlaceData)>,
    loading_place: bool,
    existing: Vec<ExistingHost>,
    dups: Vec<Option<Duplicate>>,
    stage: Stage,
    _subs: Vec<Subscription>,
}

impl ImportDialog {
    fn new(
        model: Entity<AppModel>,
        loaded: Loaded,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (targets, default_place) = {
            let m = model.read(cx);
            let targets = accounts::destinations(&m.account_infos(), &m.all_vaults(), true);
            (targets, m.place_of_target(m.new_item_target()))
        };
        let default_ix = targets
            .iter()
            .position(|d| (d.scope, d.vault) == default_place)
            .unwrap_or(0);
        let target = ui::choice_state(
            targets
                .iter()
                .enumerate()
                .map(|(i, d)| Choice::new(d.label.clone(), i))
                .collect(),
            Some(&default_ix),
            window,
            cx,
        );
        let policy = ui::choice_state(
            vec![
                Choice::new(t!("import_export.dup.skip"), DupPolicy::Skip),
                Choice::new(t!("import_export.dup.update"), DupPolicy::Update),
                Choice::new(t!("import_export.dup.copy"), DupPolicy::Copy),
            ],
            Some(&DupPolicy::Skip),
            window,
            cx,
        );
        let group = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("import_export.group_placeholder"))
        });
        let passphrase = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("import_export.passphrase"))
        });
        let mut subs = vec![
            cx.subscribe_in(
                &target,
                window,
                |this, _, _: &SelectEvent<Vec<Choice<usize>>>, window, cx| {
                    this.load_place(window, cx)
                },
            ),
            cx.subscribe_in(
                &policy,
                window,
                |_, _, _: &SelectEvent<Vec<Choice<DupPolicy>>>, _, cx| cx.notify(),
            ),
            cx.subscribe_in(
                &passphrase,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.unlock(window, cx);
                    }
                },
            ),
        ];
        let mut mapping = Vec::new();
        if let Content::Table { table, mapping: m } = &loaded.content {
            let names = csv::column_names(table, m.has_header);
            for field in Field::ALL {
                let mut choices = vec![Choice::new(t!("import_export.column_none"), None)];
                choices.extend(
                    names
                        .iter()
                        .enumerate()
                        .map(|(i, n)| Choice::new(n.clone(), Some(i))),
                );
                let state = ui::choice_state(choices, Some(&m.column(field)), window, cx);
                subs.push(cx.subscribe_in(
                    &state,
                    window,
                    |this, _, _: &SelectEvent<Vec<Choice<Option<usize>>>>, _, cx| this.remap(cx),
                ));
                mapping.push((field, state));
            }
        }
        let mut this = Self {
            model,
            loaded,
            set: ImportSet::default(),
            mapping,
            passphrase,
            unlocked: false,
            unlock_error: None,
            targets,
            target,
            policy,
            group,
            included: Vec::new(),
            place: None,
            loading_place: false,
            existing: Vec::new(),
            dups: Vec::new(),
            stage: Stage::Preview,
            _subs: subs,
        };
        this.rebuild_set(None, cx);
        this.load_place(window, cx);
        this
    }

    fn has_header(&self) -> bool {
        match &self.loaded.content {
            Content::Table { mapping, .. } => mapping.has_header,
            _ => false,
        }
    }

    /// The set from the content (and the opened secrets).
    fn rebuild_set(&mut self, secrets: Option<&termoak::Secrets>, cx: &mut Context<Self>) {
        self.set = match &self.loaded.content {
            Content::Set(s) => s.clone(),
            Content::Table { table, mapping } => csv::to_set(table, mapping),
            Content::Termoak(file) => termoak::to_set(file, secrets),
        };
        self.included = vec![true; self.set.hosts.len()];
        self.refresh_dups();
        cx.notify();
    }

    /// The mapping changed in the selects.
    fn remap(&mut self, cx: &mut Context<Self>) {
        let columns: Vec<(Field, Option<usize>)> = self
            .mapping
            .iter()
            .map(|(f, s)| (*f, ui::chosen(s, cx).flatten()))
            .collect();
        if let Content::Table { mapping, .. } = &mut self.loaded.content {
            for (f, c) in columns {
                mapping.set(f, c);
            }
        }
        self.rebuild_set(None, cx);
    }

    fn set_has_header(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        let names = match &mut self.loaded.content {
            Content::Table { table, mapping } => {
                mapping.has_header = on;
                csv::column_names(table, on)
            }
            _ => return,
        };
        // The names of the columns change in every select.
        for (_, state) in &self.mapping {
            let mut choices = vec![Choice::new(t!("import_export.column_none"), None)];
            choices.extend(
                names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| Choice::new(n.clone(), Some(i))),
            );
            let selected = ui::chosen(state, cx).flatten();
            state.update(cx, |s, cx| {
                s.set_items(choices, window, cx);
                s.set_selected_value(&selected, window, cx);
            });
        }
        self.rebuild_set(None, cx);
    }

    fn unlock(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Content::Termoak(file) = &self.loaded.content else {
            return;
        };
        let Some(sealed) = file.secrets.clone() else {
            return;
        };
        let pass = self.passphrase.read(cx).value().to_string();
        if pass.is_empty() {
            return;
        }
        let task = runtime::spawn(cx, async move {
            tokio::task::spawn_blocking(move || termoak::open(&sealed, &pass))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let res = task.await;
            let _ = this.update(cx, |this, cx| match res {
                Ok(secrets) => {
                    this.unlocked = true;
                    this.unlock_error = None;
                    this.rebuild_set(Some(&secrets), cx);
                }
                Err(e) => {
                    this.unlock_error = Some(e);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn place_of_choice(&self, cx: &App) -> Option<Place> {
        let ix = ui::chosen(&self.target, cx)?;
        let d = self.targets.get(ix)?;
        Some(Place {
            scope: d.scope,
            vault: d.vault,
        })
    }

    fn load_place(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(place) = self.place_of_choice(cx) else {
            return;
        };
        let ws = self.model.read(cx).ws.clone();
        self.loading_place = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { apply::load_place(&ws, place).await },
            move |this, res, window, cx| {
                this.loading_place = false;
                if this.place_of_choice(cx) != Some(place) {
                    return;
                }
                match res {
                    Ok(data) => {
                        this.existing = data.existing_hosts();
                        this.place = Some((place, data));
                    }
                    Err(e) => {
                        this.place = None;
                        this.existing.clear();
                        ui::error(window, cx, e);
                    }
                }
                this.refresh_dups();
                cx.notify();
            },
        );
    }

    fn refresh_dups(&mut self) {
        self.dups = importers::find_duplicates(&self.set, &self.existing);
    }

    fn actions(&self, cx: &App) -> Vec<Action> {
        let policy = ui::chosen(&self.policy, cx).unwrap_or_default();
        importers::plan(&self.dups, &self.included, &self.existing, policy)
    }

    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((place, data)) = self.place.clone() else {
            return;
        };
        let actions = self.actions(cx);
        if actions.iter().all(|a| *a == Action::Skip) {
            return;
        }
        let ws = self.model.read(cx).ws.clone();
        let base = self.group.read(cx).value().trim().to_string();
        let req = apply::Request {
            set: self.set.clone(),
            actions,
            place,
            base_group: Some(base).filter(|b| !b.is_empty()),
            existing: data,
            home: home_dir(),
        };
        self.stage = Stage::Running;
        cx.notify();
        runtime::run_in(cx, window, apply::run(ws, req), |this, res, window, cx| {
            this.model.update(cx, |m, cx| m.data_changed(cx));
            match res {
                Ok(summary) => {
                    ui::success(window, cx, summary_text(&summary));
                    this.stage = Stage::Done(summary);
                }
                Err(e) => {
                    ui::error(window, cx, t!("import_export.import_failed", error = e));
                    this.stage = Stage::Preview;
                    // Part of it may be saved: duplicates change.
                    this.load_place(window, cx);
                }
            }
            cx.notify();
        });
    }

    fn render_mapping(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.mapping.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let usable = match &self.loaded.content {
            Content::Table { mapping, .. } => mapping.is_usable(),
            _ => true,
        };
        Some(
            v_flex()
                .gap_2()
                .p_3()
                .rounded(theme.radius)
                .border_1()
                .border_color(if usable { theme.border } else { theme.warning })
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .child(
                            div()
                                .text_sm()
                                .font_semibold()
                                .child(t!("import_export.mapping")),
                        )
                        .child(div().flex_1())
                        .child(
                            Switch::new("import-has-header")
                                .label(t!("import_export.has_header"))
                                .checked(self.has_header())
                                .on_click(cx.listener(|this, v: &bool, window, cx| {
                                    this.set_has_header(*v, window, cx)
                                })),
                        ),
                )
                .when(!usable, |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(theme.warning)
                            .child(t!("import_export.mapping_needs_address")),
                    )
                })
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_2()
                        .children(self.mapping.iter().map(|(f, s)| {
                            div()
                                .w(px(180.))
                                .child(ui::field(f.name(), Select::new(s).small(), cx))
                        })),
                )
                .into_any_element(),
        )
    }

    fn render_secrets(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let Content::Termoak(file) = &self.loaded.content else {
            return None;
        };
        file.secrets.as_ref()?;
        let theme = cx.theme();
        if self.unlocked {
            return Some(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        ui::icon(IconName::LockOpen)
                            .size(px(14.))
                            .text_color(theme.success),
                    )
                    .child(div().text_sm().child(t!("import_export.secrets_unlocked")))
                    .into_any_element(),
            );
        }
        Some(
            v_flex()
                .gap_2()
                .p_3()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(ui::icon(IconName::Lock).size(px(14.)))
                        .child(div().text_sm().child(t!("import_export.secrets_locked"))),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&self.passphrase).small()))
                        .child(
                            Button::new("import-unlock")
                                .small()
                                .label(t!("import_export.unlock"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.unlock(window, cx)
                                })),
                        ),
                )
                .when_some(self.unlock_error.clone(), |this, e| {
                    this.child(div().text_xs().text_color(theme.danger).child(e))
                })
                .into_any_element(),
        )
    }

    fn render_rows(&self, actions: &[Action], cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let all = self.included.iter().all(|i| *i);
        let header = h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .text_xs()
            .font_semibold()
            .text_color(theme.muted_foreground)
            .border_b_1()
            .border_color(theme.border)
            .child(
                Checkbox::new("import-all")
                    .checked(all)
                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                        this.included.iter_mut().for_each(|i| *i = *v);
                        cx.notify();
                    })),
            )
            .child(div().w(px(190.)).child(t!("import_export.col.name")))
            .child(div().w(px(220.)).child(t!("import_export.col.address")))
            .child(div().w(px(150.)).child(t!("import_export.col.group")))
            .child(div().flex_1().child(t!("import_export.col.status")));
        let policy = ui::chosen(&self.policy, cx).unwrap_or_default();
        let rows = self.set.hosts.iter().enumerate().take(2000).map(|(i, h)| {
            let (status, color): (SharedString, gpui::Hsla) =
                match (self.dups.get(i).cloned().flatten(), actions.get(i)) {
                    (_, Some(Action::Skip)) if !self.included[i] => {
                        (t!("import_export.status.unchecked"), theme.muted_foreground)
                    }
                    (None, _) => (t!("import_export.status.new"), theme.success),
                    (Some(Duplicate::Existing(ix)), _) => {
                        let name = self
                            .existing
                            .get(ix)
                            .map(|e| e.label.clone())
                            .unwrap_or_default();
                        let text = match policy {
                            DupPolicy::Skip => t!("import_export.status.dup_skip", name = name),
                            DupPolicy::Update => {
                                t!("import_export.status.dup_update", name = name)
                            }
                            DupPolicy::Copy => t!("import_export.status.dup_copy", name = name),
                        };
                        (text, theme.warning)
                    }
                    (Some(Duplicate::InFile(_)), _) => (
                        if policy == DupPolicy::Copy {
                            t!("import_export.status.repeat_copy")
                        } else {
                            t!("import_export.status.repeat")
                        },
                        theme.warning,
                    ),
                };
            let cell = |w: f32, text: String| {
                div()
                    .w(px(w))
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(text)
            };
            h_flex()
                .id(("import-row", i))
                .px_2()
                .py_1()
                .gap_2()
                .text_sm()
                .items_center()
                .when(i % 2 == 1, |this| this.bg(theme.muted.opacity(0.3)))
                .child(
                    Checkbox::new(("import-row-check", i))
                        .checked(self.included[i])
                        .on_click(cx.listener(move |this, v: &bool, _, cx| {
                            if let Some(x) = this.included.get_mut(i) {
                                *x = *v;
                            }
                            cx.notify();
                        })),
                )
                .child(cell(190., h.label.clone()))
                .child(
                    cell(220., h.target())
                        .font_family(ui::mono_family(cx))
                        .text_xs(),
                )
                .child(cell(
                    150.,
                    h.group
                        .as_deref()
                        .map(|g| self.set.group_label(g))
                        .unwrap_or_default(),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(color)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(status),
                )
        });
        v_flex()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(header)
            .child(
                v_flex()
                    .id("import-rows")
                    .max_h(px(300.))
                    .overflow_y_scroll()
                    .children(rows)
                    .when(self.set.hosts.is_empty(), |this| {
                        this.child(
                            div()
                                .p_4()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("import_export.nothing")),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_warnings(warnings: &[String], cx: &App) -> Option<gpui::AnyElement> {
        if warnings.is_empty() {
            return None;
        }
        let theme = cx.theme();
        Some(
            v_flex()
                .id("import-warnings")
                .max_h(px(120.))
                .overflow_y_scroll()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            ui::icon(IconName::TriangleAlert)
                                .size(px(14.))
                                .text_color(theme.warning),
                        )
                        .child(
                            div()
                                .text_sm()
                                .font_semibold()
                                .child(tn!("import_export.warnings", warnings.len())),
                        ),
                )
                .children(warnings.iter().take(200).map(|w| {
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("• {w}"))
                }))
                .into_any_element(),
        )
    }

    fn render_done(&self, s: &Summary, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let pills = [
            (
                tn!("import_export.done.created", s.created),
                theme.success,
                s.created,
            ),
            (
                tn!("import_export.done.updated", s.updated),
                theme.info,
                s.updated,
            ),
            (
                tn!("import_export.done.skipped", s.skipped),
                theme.muted_foreground,
                s.skipped,
            ),
            (
                tn!("import_export.done.groups", s.groups),
                theme.primary,
                s.groups,
            ),
            (
                tn!("import_export.done.keys", s.keys),
                theme.primary,
                s.keys,
            ),
            (
                tn!("import_export.done.keys_reused", s.keys_reused),
                theme.muted_foreground,
                s.keys_reused,
            ),
            (
                tn!("import_export.done.identities", s.identities),
                theme.primary,
                s.identities,
            ),
            (
                tn!("import_export.done.snippets", s.snippets),
                theme.primary,
                s.snippets,
            ),
        ];
        v_flex()
            .gap_4()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        ui::icon(IconName::CircleCheck)
                            .size(px(20.))
                            .text_color(theme.success),
                    )
                    .child(div().font_semibold().child(t!("import_export.done.title"))),
            )
            .child(
                h_flex().flex_wrap().gap_1().children(
                    pills
                        .into_iter()
                        .filter(|(_, _, n)| *n > 0)
                        .map(|(t, c, _)| ui::pill(t, c)),
                ),
            )
            .children(Self::render_warnings(&s.warnings, cx))
            .child(
                h_flex().justify_end().child(
                    Button::new("import-close")
                        .primary()
                        .label(t!("common.close"))
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
            .into_any_element()
    }
}

/// One line for the final notification.
fn summary_text(s: &Summary) -> SharedString {
    let mut parts = vec![tn!("import_export.done.created", s.created).to_string()];
    if s.updated > 0 {
        parts.push(tn!("import_export.done.updated", s.updated).to_string());
    }
    if s.skipped > 0 {
        parts.push(tn!("import_export.done.skipped", s.skipped).to_string());
    }
    t!("import_export.done.toast", summary = parts.join(", "))
}

impl Render for ImportDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Stage::Done(s) = &self.stage {
            let s = s.clone();
            return self.render_done(&s, cx);
        }
        let actions = self.actions(cx);
        let counts = importers::count_actions(&actions);
        let n = |k: &str| counts.get(k).copied().unwrap_or(0);
        let to_import = n("create") + n("copy") + n("update");
        let mapping = self.render_mapping(cx);
        let secrets = self.render_secrets(cx);
        let rows = self.render_rows(&actions, cx);
        let warnings = Self::render_warnings(&self.set.warnings, cx);
        let running = matches!(self.stage, Stage::Running);
        let dups = self.dups.iter().filter(|d| d.is_some()).count();
        let source = self.loaded.source;
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(ui::icon(source_icon(source)).size(px(16.)))
                    .child(
                        div()
                            .text_sm()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(self.loaded.origin.clone()),
                    )
                    .child(div().flex_1())
                    .when(source == Source::SecureCrt, |this| {
                        this.child(
                            Button::new("import-folder")
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::FolderOpen))
                                .label(t!("import_export.choose_folder"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    pick_securecrt_folder(this.model.clone(), window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("import-other-file")
                            .small()
                            .ghost()
                            .icon(ui::icon(IconName::FileInput))
                            .label(t!("import_export.choose_other"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                let model = this.model.clone();
                                window.close_dialog(cx);
                                pick_file(model, Some(source), window, cx);
                            })),
                    ),
            )
            .children(mapping)
            .children(secrets)
            .child(
                h_flex()
                    .gap_3()
                    .items_end()
                    .child(div().flex_1().child(ui::field(
                        t!("import_export.target"),
                        Select::new(&self.target),
                        cx,
                    )))
                    .child(div().flex_1().child(ui::field(
                        t!("import_export.group"),
                        Input::new(&self.group),
                        cx,
                    )))
                    .child(div().w(px(200.)).child(ui::field(
                        t!("import_export.duplicates"),
                        Select::new(&self.policy),
                        cx,
                    ))),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .child(ui::pill(
                        tn!("import_export.pill.hosts", self.set.hosts.len()),
                        theme.primary,
                    ))
                    .when(dups > 0, |this| {
                        this.child(ui::pill(
                            tn!("import_export.pill.dups", dups),
                            theme.warning,
                        ))
                    })
                    .when(!self.set.groups.is_empty(), |this| {
                        this.child(ui::pill(
                            tn!("import_export.pill.groups", self.set.groups.len()),
                            theme.info,
                        ))
                    })
                    .when(!self.set.keys.is_empty(), |this| {
                        this.child(ui::pill(
                            tn!("import_export.pill.keys", self.set.keys.len()),
                            theme.info,
                        ))
                    })
                    .when(!self.set.identities.is_empty(), |this| {
                        this.child(ui::pill(
                            tn!("import_export.pill.identities", self.set.identities.len()),
                            theme.info,
                        ))
                    })
                    .when(!self.set.snippets.is_empty(), |this| {
                        this.child(ui::pill(
                            tn!("import_export.pill.snippets", self.set.snippets.len()),
                            theme.info,
                        ))
                    })
                    .when(self.loading_place, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(t!("import_export.checking_duplicates")),
                        )
                    }),
            )
            .child(rows)
            .children(warnings)
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("import-cancel")
                            .label(t!("common.cancel"))
                            .disabled(running)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("import-run")
                            .primary()
                            .icon(ui::icon(IconName::Import))
                            .label(tn!("import_export.run", to_import))
                            .loading(running)
                            .disabled(to_import == 0 || self.place.is_none() || self.loading_place)
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.run(window, cx)
                                }),
                            ),
                    ),
            )
            .into_any_element()
    }
}

/// SecureCRT: the folder of session files instead of an XML export.
fn pick_securecrt_folder(model: Entity<AppModel>, window: &mut Window, cx: &mut App) {
    let rx = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some(t!("import_export.open")),
    });
    window
        .spawn(cx, async move |cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let _ = cx.update(|window, cx| {
                let d = dir.clone();
                let task = runtime::spawn(cx, async move {
                    tokio::task::spawn_blocking(move || securecrt::read_folder(&d))
                        .await
                        .map_err(|e| e.to_string())?
                        .map_err(|e| e.to_string())
                });
                window
                    .spawn(cx, async move |cx| {
                        let res = task.await;
                        let _ = cx.update(|window, cx| match res {
                            Ok(files) => {
                                window.close_dialog(cx);
                                let loaded = Loaded {
                                    source: Source::SecureCrt,
                                    origin: dir.display().to_string(),
                                    content: Content::Set(securecrt::from_ini_files(&files)),
                                };
                                open_preview(model, loaded, window, cx);
                            }
                            Err(e) => {
                                ui::error(window, cx, t!("import_export.error.read", error = e))
                            }
                        });
                    })
                    .detach();
            });
        })
        .detach();
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Termoak,
    Csv,
}

/// Every place with items: the vaults of the accounts (any role), the
/// accounts without vaults and This device.
fn export_places(model: &AppModel) -> Vec<Destination> {
    let infos = model.account_infos();
    let vaults = model.all_vaults();
    let multi = infos.len() > 1;
    let names = accounts::names();
    let mut out = Vec::new();
    for a in &infos {
        let mine: Vec<_> = vaults.iter().filter(|v| v.account == a.id).collect();
        if mine.is_empty() {
            out.push(Destination {
                scope: Scope::Account(a.id),
                vault: None,
                label: names.name(a.id, &a.email),
            });
        }
        for v in mine {
            out.push(Destination {
                scope: Scope::Account(a.id),
                vault: Some(v.id()),
                label: if multi {
                    format!("{} · {}", names.name(a.id, &a.email), v.label())
                } else {
                    v.label()
                },
            });
        }
    }
    out.push(Destination {
        scope: Scope::Device,
        vault: None,
        label: t!("accounts.switcher.device").to_string(),
    });
    out
}

/// Opens the export dialog.
pub fn open_export(model: Entity<AppModel>, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| ExportDialog::new(model, window, cx));
    window.open_dialog(cx, move |d, _, _| {
        d.title(t!("import_export.export_title"))
            .w(px(520.))
            .overlay_closable(false)
            .child(view.clone())
    });
}

struct ExportDialog {
    model: Entity<AppModel>,
    format: ChoiceState<Format>,
    places: Vec<Destination>,
    place: ChoiceState<usize>,
    group: ChoiceState<Option<Id>>,
    include_secrets: bool,
    pass1: Entity<InputState>,
    pass2: Entity<InputState>,
    busy: bool,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

impl ExportDialog {
    fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (places, default_place) = {
            let m = model.read(cx);
            (export_places(m), m.place_of_target(m.new_item_target()))
        };
        let default_ix = places
            .iter()
            .position(|d| (d.scope, d.vault) == default_place)
            .unwrap_or(0);
        let place = ui::choice_state(
            places
                .iter()
                .enumerate()
                .map(|(i, d)| Choice::new(d.label.clone(), i))
                .collect(),
            Some(&default_ix),
            window,
            cx,
        );
        let format = ui::choice_state(
            vec![
                Choice::new(t!("import_export.format.termoak"), Format::Termoak),
                Choice::new(t!("import_export.format.csv"), Format::Csv),
            ],
            Some(&Format::Termoak),
            window,
            cx,
        );
        let group = ui::choice_state(
            vec![Choice::new(t!("import_export.all_groups"), None)],
            Some(&None),
            window,
            cx,
        );
        let masked = |placeholder: SharedString, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(placeholder)
            })
        };
        let pass1 = masked(t!("import_export.passphrase"), window, cx);
        let pass2 = masked(t!("import_export.passphrase_repeat"), window, cx);
        let subs = vec![
            cx.subscribe_in(
                &place,
                window,
                |this, _, _: &SelectEvent<Vec<Choice<usize>>>, window, cx| {
                    this.load_groups(window, cx)
                },
            ),
            cx.subscribe_in(
                &format,
                window,
                |_, _, _: &SelectEvent<Vec<Choice<Format>>>, _, cx| cx.notify(),
            ),
        ];
        let mut this = Self {
            model,
            format,
            places,
            place,
            group,
            include_secrets: false,
            pass1,
            pass2,
            busy: false,
            error: None,
            _subs: subs,
        };
        this.load_groups(window, cx);
        this
    }

    fn place(&self, cx: &App) -> Option<Place> {
        let d = self.places.get(ui::chosen(&self.place, cx)?)?;
        Some(Place {
            scope: d.scope,
            vault: d.vault,
        })
    }

    fn load_groups(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(place) = self.place(cx) else {
            return;
        };
        let ws = self.model.read(cx).ws.clone();
        runtime::run_in(
            cx,
            window,
            async move { apply::load_place(&ws, place).await },
            move |this, res, window, cx| {
                let Ok(data) = res else { return };
                let mut groups: Vec<(String, Id)> = data
                    .groups
                    .iter()
                    .map(|g| (data.group_path(Some(g.id)), g.id))
                    .collect();
                groups.sort_by_key(|(p, _)| p.to_lowercase());
                let mut choices = vec![Choice::new(t!("import_export.all_groups"), None)];
                choices.extend(
                    groups
                        .into_iter()
                        .map(|(p, id)| Choice::new(p.replace('/', " / "), Some(id))),
                );
                this.group.update(cx, |s, cx| {
                    s.set_items(choices, window, cx);
                    s.set_selected_value(&None, window, cx);
                });
                cx.notify();
            },
        );
    }

    fn export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(place) = self.place(cx) else {
            return;
        };
        let format = ui::chosen(&self.format, cx).unwrap_or(Format::Termoak);
        let secrets = self.include_secrets && format == Format::Termoak;
        let pass = self.pass1.read(cx).value().to_string();
        if secrets {
            if pass.chars().count() < 8 {
                self.error = Some(t!("import_export.passphrase_short").to_string());
                cx.notify();
                return;
            }
            if pass != self.pass2.read(cx).value().as_ref() {
                self.error = Some(t!("import_export.passphrase_mismatch").to_string());
                cx.notify();
                return;
            }
        }
        self.error = None;
        let req = ExportRequest {
            place,
            group: ui::chosen(&self.group, cx).flatten(),
            include_secrets: secrets,
        };
        let ext = match format {
            Format::Termoak => "json",
            Format::Csv => "csv",
        };
        let name = format!(
            "termoak-hosts-{}.{ext}",
            chrono::Local::now().format("%Y-%m-%d")
        );
        let dir = directories::UserDirs::new()
            .and_then(|d| d.document_dir().map(Path::to_path_buf))
            .or_else(home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        let ws = self.model.read(cx).ws.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = true;
                cx.notify();
                let target = path.clone();
                runtime::run_in(
                    cx,
                    window,
                    async move {
                        let g = apply::gather(ws, req).await?;
                        let hosts = g.file.hosts.len();
                        let hidden = g.hidden_secrets;
                        let body = match format {
                            Format::Csv => csv::write(&g.rows),
                            Format::Termoak => {
                                let mut file = g.file;
                                if req.include_secrets {
                                    let secrets = g.secrets;
                                    let sealed = tokio::task::spawn_blocking(move || {
                                        termoak::seal(&secrets, &pass)
                                    })
                                    .await
                                    .map_err(|e| e.to_string())??;
                                    file.secrets = Some(sealed);
                                }
                                termoak::write(&file)
                            }
                        };
                        if req.include_secrets {
                            termoak_core::crypto::write_private_file(&target, body.as_bytes())
                                .map_err(|e| e.to_string())?;
                        } else {
                            tokio::fs::write(&target, body)
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                        Ok::<_, String>((hosts, hidden))
                    },
                    move |this, res, window, cx| {
                        this.busy = false;
                        match res {
                            Ok((hosts, hidden)) => {
                                window.close_dialog(cx);
                                ui::success(
                                    window,
                                    cx,
                                    tn!(
                                        "import_export.exported",
                                        hosts,
                                        path = path.display().to_string()
                                    ),
                                );
                                if hidden > 0 {
                                    ui::notify(
                                        window,
                                        cx,
                                        crate::state::ToastKind::Warning,
                                        tn!("import_export.hidden_secrets", hidden),
                                    );
                                }
                            }
                            Err(e) => {
                                this.error =
                                    Some(t!("import_export.export_failed", error = e).to_string());
                            }
                        }
                        cx.notify();
                    },
                );
            });
        })
        .detach();
    }
}

impl Render for ExportDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let format = ui::chosen(&self.format, cx).unwrap_or(Format::Termoak);
        let json = format == Format::Termoak;
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(if json {
                        t!("import_export.export_intro_json")
                    } else {
                        t!("import_export.export_intro_csv")
                    }),
            )
            .child(ui::field(
                t!("import_export.format"),
                Select::new(&self.format),
                cx,
            ))
            .child(
                h_flex()
                    .gap_3()
                    .child(div().flex_1().child(ui::field(
                        t!("import_export.source_place"),
                        Select::new(&self.place),
                        cx,
                    )))
                    .child(div().flex_1().child(ui::field(
                        t!("import_export.group"),
                        Select::new(&self.group),
                        cx,
                    ))),
            )
            .when(json, |this| {
                this.child(
                    Switch::new("export-secrets")
                        .label(t!("import_export.include_secrets"))
                        .checked(self.include_secrets)
                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                            this.include_secrets = *v;
                            cx.notify();
                        })),
                )
                .when(self.include_secrets, |this| {
                    this.child(
                        v_flex()
                            .gap_2()
                            .child(Input::new(&self.pass1))
                            .child(Input::new(&self.pass2))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(t!("import_export.passphrase_hint")),
                            ),
                    )
                })
            })
            .when_some(self.error.clone(), |this, e| {
                this.child(div().text_sm().text_color(theme.danger).child(e))
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("export-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("export-run")
                            .primary()
                            .icon(ui::icon(IconName::FileOutput))
                            .label(t!("import_export.export_button"))
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.export(window, cx)
                            })),
                    ),
            )
    }
}
