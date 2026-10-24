//! Snippet variables (docs/SSH-PRO.md §7 in the core): the fields of the
//! run dialogs, the issues the editor shows and how a snippet is rendered
//! for one or several hosts.
//!
//! - Prompts (`{{name}}`, `{{prompt:Label|default}}`) are text fields with
//!   their default; selects (`{{select:Label|a|b}}`) a dropdown; secrets
//!   (`{{secret:Label}}`) a masked field, never remembered nor stored.
//! - Built-ins (`{{host}}`, `{{user}}`, `{{date}}`...) are filled per host;
//!   without a host (a local terminal) the host ones are asked.
//! - Values are remembered on this computer per snippet and host (the
//!   core's `snippet_values`, never secrets).

use std::collections::BTreeMap;

use gpui::{
    AppContext, Entity, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::FluentBuilder,
};
use gpui_component::input::{Input, InputState};
use gpui_component::select::Select;
use gpui_component::{ActiveTheme, StyledExt, h_flex, v_flex};
use termoak_core::template::{self, Issue, Template, TemplateContext, VarKind, Variable};

use crate::ui::{self, Choice, ChoiceState};

/// The field of one variable.
enum Field {
    Text(Entity<InputState>),
    Secret(Entity<InputState>),
    Select(ChoiceState<String>),
}

/// The fields of the variables to ask.
pub struct SnippetVars {
    fields: Vec<(Variable, Field)>,
}

/// The variables to ask for `script`: prompts, selects and secrets, and
/// the host built-ins when some target has no host.
pub fn prompts(script: &str, every_target_has_host: bool) -> Vec<Variable> {
    let t = template::parse(script);
    let ctx = TemplateContext {
        host: every_target_has_host.then(String::new),
        ..Default::default()
    };
    t.prompts(&ctx).into_iter().cloned().collect()
}

impl SnippetVars {
    /// Fields for `vars`, prefilled with the remembered values (or the
    /// defaults).
    pub fn new(
        vars: Vec<Variable>,
        remembered: &BTreeMap<String, String>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> Self {
        let fields = vars
            .into_iter()
            .map(|v| {
                let value = remembered
                    .get(&v.key)
                    .filter(|_| v.rememberable())
                    .cloned()
                    .or_else(|| v.default.clone());
                let field = match v.kind {
                    VarKind::Select => {
                        let items = v
                            .options
                            .iter()
                            .map(|o| Choice::new(o.clone(), o.clone()))
                            .collect();
                        let chosen = value
                            .filter(|x| v.options.contains(x))
                            .or_else(|| v.options.first().cloned());
                        Field::Select(ui::choice_state(items, chosen.as_ref(), window, cx))
                    }
                    VarKind::Secret => Field::Secret(cx.new(|cx| {
                        InputState::new(window, cx)
                            .masked(true)
                            .placeholder(t!("snippets.vars.secret_placeholder"))
                    })),
                    VarKind::Prompt | VarKind::Builtin => {
                        let label = v.label.clone();
                        Field::Text(cx.new(|cx| {
                            InputState::new(window, cx)
                                .placeholder(label)
                                .default_value(value.unwrap_or_default())
                        }))
                    }
                };
                (v, field)
            })
            .collect();
        Self { fields }
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The first field (to focus it).
    pub fn first_input(&self) -> Option<Entity<InputState>> {
        self.fields.iter().find_map(|(_, f)| match f {
            Field::Text(i) | Field::Secret(i) => Some(i.clone()),
            Field::Select(_) => None,
        })
    }

    /// The values typed, or the label of the first one missing.
    pub fn values(&self, cx: &gpui::App) -> Result<BTreeMap<String, String>, String> {
        let mut out = BTreeMap::new();
        for (v, f) in &self.fields {
            let value = match f {
                Field::Text(i) | Field::Secret(i) => i.read(cx).value().to_string(),
                Field::Select(s) => ui::chosen(s, cx).unwrap_or_default(),
            };
            if value.trim().is_empty() {
                return Err(v.label.clone());
            }
            out.insert(v.key.clone(), value);
        }
        Ok(out)
    }

    /// The fields, labelled with what they are.
    pub fn render(&self, cx: &gpui::App) -> impl IntoElement {
        v_flex().gap_3().children(self.fields.iter().map(|(v, f)| {
            let control = match f {
                Field::Text(i) => Input::new(i).into_any_element(),
                Field::Secret(i) => Input::new(i).mask_toggle().into_any_element(),
                Field::Select(s) => Select::new(s).into_any_element(),
            };
            let label: SharedString = match v.kind {
                VarKind::Secret => t!("snippets.vars.secret_label", name = v.label),
                VarKind::Builtin => t!("snippets.vars.builtin_label", name = v.label),
                _ => v.label.clone().into(),
            };
            match v.kind {
                VarKind::Secret => {
                    ui::field_with_hint(label, control, t!("snippets.vars.secret_hint"), cx)
                }
                VarKind::Builtin => {
                    ui::field_with_hint(label, control, t!("snippets.vars.builtin_hint"), cx)
                }
                _ => ui::field(label, control, cx),
            }
        }))
    }
}

/// Renders a snippet for several targets with the same `values`: each
/// target's own built-ins. Values asked for host built-ins (because some
/// target has no host) only apply to the targets without one.
pub fn render_targets(
    script: &str,
    values: &BTreeMap<String, String>,
    contexts: &[TemplateContext],
) -> Result<Vec<String>, String> {
    let t = template::parse(script);
    contexts
        .iter()
        .map(|ctx| {
            let mut v = values.clone();
            if ctx.has_host() {
                for var in t.variables.iter().filter(|x| x.needs_host()) {
                    v.remove(&var.key);
                }
            }
            t.render(&v, ctx).map_err(|e| e.to_string())
        })
        .collect()
}

/// One issue of the editor: where it is (line and column, from 1), the
/// tag as written and what is wrong (translated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueView {
    pub line: usize,
    pub column: usize,
    pub tag: String,
    pub text: String,
}

/// The issues of a script, with their place and text.
pub fn issues(script: &str) -> Vec<IssueView> {
    let t = template::parse(script);
    t.issues.iter().map(|i| issue_view(script, i)).collect()
}

fn issue_view(script: &str, i: &Issue) -> IssueView {
    let before: String = script.chars().take(i.start).collect();
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .map(|l| l.chars().count())
        .unwrap_or(0)
        + 1;
    IssueView {
        line,
        column,
        tag: i.tag.clone(),
        text: issue_text(&i.message),
    }
}

/// The text of a core issue message, translated when it is a known one.
pub fn issue_text(message: &str) -> String {
    if message == "the variable has no label" {
        return t!("snippets.issue.no_label").to_string();
    }
    if message.starts_with("the label is longer than") {
        return t!("snippets.issue.label_too_long", max = template::MAX_LABEL).to_string();
    }
    if message.starts_with("a select needs at least one option") {
        return t!("snippets.issue.select_options").to_string();
    }
    if message == "a secret cannot have a default value" {
        return t!("snippets.issue.secret_default").to_string();
    }
    if message.starts_with("a snippet can have at most") {
        return t!("snippets.issue.too_many", max = template::MAX_VARIABLES).to_string();
    }
    // "\"Env\" is already a select variable"
    if let Some(rest) = message.strip_prefix('"')
        && let Some((name, tail)) = rest.split_once("\" is already a ")
        && let Some(kind) = tail.strip_suffix(" variable")
    {
        let kind = match kind {
            "built-in" => t!("snippets.kind.builtin"),
            "prompt" => t!("snippets.kind.prompt"),
            "select" => t!("snippets.kind.select"),
            "secret" => t!("snippets.kind.secret"),
            other => other.to_string().into(),
        };
        return t!("snippets.issue.kind_conflict", name = name, kind = kind).to_string();
    }
    message.to_string()
}

/// "Env · choice: dev, prod" of a variable, for the editor's list.
pub fn variable_text(v: &Variable) -> String {
    match v.kind {
        VarKind::Builtin => t!("snippets.vars.kind_builtin", name = v.label).to_string(),
        VarKind::Prompt => match v.default.as_deref().filter(|d| !d.is_empty()) {
            Some(d) => t!(
                "snippets.vars.kind_prompt_default",
                name = v.label,
                default = d
            )
            .to_string(),
            None => t!("snippets.vars.kind_prompt", name = v.label).to_string(),
        },
        VarKind::Select => t!(
            "snippets.vars.kind_select",
            name = v.label,
            options = v.options.join(", ")
        )
        .to_string(),
        VarKind::Secret => t!("snippets.vars.kind_secret", name = v.label).to_string(),
    }
}

/// The editor's live view of a script: the variables it has and the
/// issues to fix before saving.
pub fn editor_summary(script: &str, cx: &gpui::App) -> Option<gpui::AnyElement> {
    let t: Template = template::parse(script);
    if t.variables.is_empty() && t.issues.is_empty() {
        return None;
    }
    let theme = cx.theme();
    let issues: Vec<IssueView> = t.issues.iter().map(|i| issue_view(script, i)).collect();
    Some(
        v_flex()
            .gap_2()
            .children(issues_block(&issues, cx))
            .when(!t.variables.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .font_medium()
                                .text_color(theme.muted_foreground)
                                .child(t!("snippets.vars.title")),
                        )
                        .child(
                            h_flex()
                                .flex_wrap()
                                .gap_1()
                                .children(t.variables.iter().map(|v| {
                                    let color = match v.kind {
                                        VarKind::Builtin => theme.muted_foreground,
                                        VarKind::Secret => theme.warning,
                                        _ => theme.primary,
                                    };
                                    ui::pill(variable_text(v), color)
                                })),
                        ),
                )
            })
            .into_any_element(),
    )
}

/// The issues to fix, in red.
fn issues_block(issues: &[IssueView], cx: &gpui::App) -> Option<gpui::AnyElement> {
    if issues.is_empty() {
        return None;
    }
    let theme = cx.theme();
    Some(
        v_flex()
            .gap_1()
            .p_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.danger)
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .text_color(theme.danger)
                    .child(ui::icon(ui::IconName::TriangleAlert).size(gpui::px(13.)))
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .child(tn!("snippets.issue.title", issues.len())),
                    ),
            )
            .children(issues.iter().map(|i| {
                div().text_xs().child(t!(
                    "snippets.issue.line",
                    line = i.line,
                    column = i.column,
                    tag = i.tag,
                    text = i.text
                ))
            }))
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn host(addr: &str, user: &str) -> TemplateContext {
        TemplateContext {
            host: Some(addr.into()),
            label: Some(format!("{addr}-label")),
            user: Some(user.into()),
            port: Some(22),
            group: None,
            now: None,
        }
    }

    #[test]
    fn what_is_asked() {
        let script = "deploy {{host}} {{prompt:Branch|main}} {{select:Env|dev|prod}} {{secret:Token}} {{date}}";
        let with_host: Vec<String> = prompts(script, true).into_iter().map(|v| v.key).collect();
        assert_eq!(with_host, ["Branch", "Env", "Token"]);
        let local: Vec<String> = prompts(script, false).into_iter().map(|v| v.key).collect();
        assert_eq!(local, ["host", "Branch", "Env", "Token"]);
        assert!(prompts("uptime", true).is_empty());
    }

    #[test]
    fn each_target_gets_its_own_builtins() {
        let script = "ssh {{user}}@{{host}} -- echo {{prompt:Msg}}";
        let out = render_targets(
            script,
            &map(&[("Msg", "hi"), ("host", "typed"), ("user", "me")]),
            &[
                host("10.0.0.1", "root"),
                host("10.0.0.2", "deploy"),
                TemplateContext::default(),
            ],
        )
        .unwrap();
        assert_eq!(
            out,
            [
                "ssh root@10.0.0.1 -- echo hi",
                "ssh deploy@10.0.0.2 -- echo hi",
                // A local terminal: the values asked for host built-ins.
                "ssh me@typed -- echo hi",
            ]
        );
        // A missing prompt is an error (never a half-rendered command).
        assert!(render_targets(script, &map(&[]), &[host("h", "u")]).is_err());
    }

    #[test]
    fn issues_have_place_and_text() {
        let script = "echo ok\n  {{select:Env}} {{secret:Pass|x}}\n{{prompt:}}";
        let list = issues(script);
        assert_eq!(list.len(), 3);
        assert_eq!((list[0].line, list[0].column), (2, 3));
        assert_eq!(list[0].tag, "{{select:Env}}");
        assert_eq!(list[0].text, t!("snippets.issue.select_options"));
        assert_eq!(list[1].text, t!("snippets.issue.secret_default"));
        assert_eq!((list[2].line, list[2].column), (3, 1));
        assert_eq!(list[2].text, t!("snippets.issue.no_label"));
        let conflict = issues("{{select:Env|a}} {{prompt:Env}}");
        assert_eq!(
            conflict[0].text,
            t!(
                "snippets.issue.kind_conflict",
                name = "Env",
                kind = t!("snippets.kind.select")
            )
        );
        assert!(issues("{{ .Values.x }} {{#each}} \\{{host}}").is_empty());
        assert_eq!(issue_text("something new"), "something new");
    }
}
