//! What the AI interface shows about tasks, hosts and tools, without
//! drawing anything: the phase of a task with its icon and color, the
//! filters and search of the task list, the per-host table's sorting,
//! relative times, durations and costs, and the kind of each tool.

use gpui::SharedString;
use serde_json::Value;
use termoak_ai::HostRun;

use crate::ui::IconName;

/// The semantic color of something (mapped to the theme when drawing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Warning,
    Success,
    Danger,
    Muted,
}

/// Where a task (or a host of a multi-host task) is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Queued,
    Running,
    /// Waiting for an approval (or has some pending).
    NeedsApproval,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}

impl Phase {
    /// From a task's status and its pending approvals.
    pub fn of(status: &str, pending: usize) -> Self {
        match status {
            "waiting_approval" => Phase::NeedsApproval,
            "queued" | "running" if pending > 0 => Phase::NeedsApproval,
            "queued" => Phase::Queued,
            "running" => Phase::Running,
            "completed" => Phase::Completed,
            "failed" => Phase::Failed,
            "cancelled" => Phase::Cancelled,
            _ => Phase::Unknown,
        }
    }

    /// Still going (also while it waits for you).
    pub fn active(self) -> bool {
        matches!(self, Phase::Queued | Phase::Running | Phase::NeedsApproval)
    }

    pub fn finished(self) -> bool {
        matches!(self, Phase::Completed | Phase::Failed | Phase::Cancelled)
    }

    pub fn tone(self) -> Tone {
        match self {
            Phase::Queued | Phase::Running => Tone::Info,
            Phase::NeedsApproval => Tone::Warning,
            Phase::Completed => Tone::Success,
            Phase::Failed => Tone::Danger,
            Phase::Cancelled | Phase::Unknown => Tone::Muted,
        }
    }

    /// The icon of a finished or waiting phase (a running one shows a
    /// spinner instead).
    pub fn icon(self) -> IconName {
        match self {
            Phase::Queued => IconName::Clock,
            Phase::Running => IconName::LoaderCircle,
            Phase::NeedsApproval => IconName::ShieldAlert,
            Phase::Completed => IconName::CircleCheck,
            Phase::Failed => IconName::CircleX,
            Phase::Cancelled => IconName::CircleSlash,
            Phase::Unknown => IconName::CircleDashed,
        }
    }

    /// Order in the per-host table (what needs you first).
    pub fn rank(self) -> u8 {
        match self {
            Phase::NeedsApproval => 0,
            Phase::Running => 1,
            Phase::Queued => 2,
            Phase::Failed => 3,
            Phase::Cancelled => 4,
            Phase::Completed => 5,
            Phase::Unknown => 6,
        }
    }

    pub fn label(self) -> SharedString {
        match self {
            Phase::Queued => t!("ai_chat.status.queued"),
            Phase::Running => t!("ai_chat.status.running"),
            Phase::NeedsApproval => t!("ai_chat.status.waiting_approval"),
            Phase::Completed => t!("ai_chat.status.completed"),
            Phase::Failed => t!("ai_chat.status.failed"),
            Phase::Cancelled => t!("ai_chat.status.cancelled"),
            Phase::Unknown => "—".into(),
        }
    }
}

/// The filters above the task list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskFilter {
    #[default]
    All,
    Running,
    NeedsApproval,
    Done,
}

impl TaskFilter {
    pub const ALL: [TaskFilter; 4] = [
        TaskFilter::All,
        TaskFilter::Running,
        TaskFilter::NeedsApproval,
        TaskFilter::Done,
    ];

    pub fn matches(self, phase: Phase) -> bool {
        match self {
            TaskFilter::All => true,
            TaskFilter::Running => matches!(phase, Phase::Queued | Phase::Running),
            TaskFilter::NeedsApproval => phase == Phase::NeedsApproval,
            TaskFilter::Done => phase.finished(),
        }
    }

    pub fn label(self) -> SharedString {
        match self {
            TaskFilter::All => t!("ai_ui.filter.all"),
            TaskFilter::Running => t!("ai_ui.filter.running"),
            TaskFilter::NeedsApproval => t!("ai_ui.filter.needs_approval"),
            TaskFilter::Done => t!("ai_ui.filter.done"),
        }
    }
}

/// Does every word of `query` appear in one of the `fields` (ignoring
/// case)? An empty query matches everything.
pub fn matches_search(query: &str, fields: &[&str]) -> bool {
    let fields: Vec<String> = fields.iter().map(|f| f.to_lowercase()).collect();
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .all(|word| fields.iter().any(|f| f.contains(&word)))
}

/// "just now", "5 min ago", "3 h ago", "2 d ago", or the date.
pub fn relative_time(now_ms: i64, ms: i64) -> SharedString {
    if ms <= 0 {
        return SharedString::default();
    }
    let secs = (now_ms - ms).max(0) / 1000;
    match secs {
        s if s < 45 => t!("ai_ui.time.just_now"),
        s if s < 3600 => t!("ai_ui.time.minutes_ago", n = (s / 60).max(1)),
        s if s < 86_400 => t!("ai_ui.time.hours_ago", n = s / 3600),
        s if s < 7 * 86_400 => t!("ai_ui.time.days_ago", n = s / 86_400),
        _ => crate::ui::format_ms(ms).into(),
    }
}

/// `850 ms`, `12 s`, `3 min 4 s`, `1 h 2 min`.
pub fn format_duration(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1000 {
        return format!("{ms} ms");
    }
    let secs = ms / 1000;
    match secs {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min {} s", s / 60, s % 60),
        s => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

/// `$0.0042`, `$1.27` (`None` when it cost nothing).
pub fn format_cost(micros: i64) -> Option<String> {
    if micros <= 0 {
        return None;
    }
    let dollars = micros as f64 / 1_000_000.0;
    Some(if dollars < 0.01 {
        format!("${dollars:.4}")
    } else {
        format!("${dollars:.2}")
    })
}

/// The model of a provider string (`claude::claude-opus-5` →
/// `claude-opus-5`; a plain provider stays as it is).
pub fn model_name(provider: &str) -> &str {
    match provider.rsplit_once("::") {
        Some((_, model)) if !model.is_empty() => model,
        _ => provider,
    }
}

/// The tools the AI uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    RunCommand,
    ReadFile,
    WriteFile,
    ListDirectory,
    ReadTerminal,
    SendToTerminal,
    ListSessions,
    Remember,
    ListHosts,
    ListSnippets,
    Other,
}

impl ToolKind {
    pub fn of(name: &str) -> Self {
        match name {
            "run_command" => ToolKind::RunCommand,
            "read_file" => ToolKind::ReadFile,
            "write_file" => ToolKind::WriteFile,
            "list_directory" => ToolKind::ListDirectory,
            "read_terminal" => ToolKind::ReadTerminal,
            "send_to_terminal" => ToolKind::SendToTerminal,
            "list_sessions" => ToolKind::ListSessions,
            "remember" => ToolKind::Remember,
            "list_hosts" => ToolKind::ListHosts,
            "list_snippets" => ToolKind::ListSnippets,
            _ => ToolKind::Other,
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            ToolKind::RunCommand => IconName::SquareTerminal,
            ToolKind::ReadFile => IconName::FileText,
            ToolKind::WriteFile => IconName::FilePen,
            ToolKind::ListDirectory => IconName::FolderOpen,
            ToolKind::ReadTerminal => IconName::Eye,
            ToolKind::SendToTerminal => IconName::Keyboard,
            ToolKind::ListSessions => IconName::Layers,
            ToolKind::Remember => IconName::Brain,
            ToolKind::ListHosts => IconName::Server,
            ToolKind::ListSnippets => IconName::BookOpen,
            ToolKind::Other => IconName::Wrench,
        }
    }

    /// What it does, in the user's language (the tool's own name for an
    /// unknown one).
    pub fn label(self, name: &str) -> SharedString {
        match self {
            ToolKind::RunCommand => t!("ai_ui.tool.run_command"),
            ToolKind::ReadFile => t!("ai_ui.tool.read_file"),
            ToolKind::WriteFile => t!("ai_ui.tool.write_file"),
            ToolKind::ListDirectory => t!("ai_ui.tool.list_directory"),
            ToolKind::ReadTerminal => t!("ai_ui.tool.read_terminal"),
            ToolKind::SendToTerminal => t!("ai_ui.tool.send_to_terminal"),
            ToolKind::ListSessions => t!("ai_ui.tool.list_sessions"),
            ToolKind::Remember => t!("ai_ui.tool.remember"),
            ToolKind::ListHosts => t!("ai_ui.tool.list_hosts"),
            ToolKind::ListSnippets => t!("ai_ui.tool.list_snippets"),
            ToolKind::Other => SharedString::from(name.to_string()),
        }
    }

    /// The command's text is shell (highlighted).
    pub fn is_command(self) -> bool {
        matches!(self, ToolKind::RunCommand | ToolKind::SendToTerminal)
    }
}

/// What a tool call acts on, from its input: the command, the path, the
/// fact to remember, the search... (`None` if nothing to show).
pub fn tool_target(input: &Value) -> Option<String> {
    ["command", "input", "path", "content", "query"]
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The host of a tool call (`None` for the terminal tools and the ones
/// without a host).
pub fn tool_host(input: &Value) -> Option<String> {
    input
        .get("host")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A tool's output reduced to its first lines for the collapsed card, and
/// whether there was more.
pub fn output_preview(output: &str, max_lines: usize) -> (String, bool) {
    let trimmed = output.trim_end();
    let mut lines = trimmed.lines();
    let head: Vec<&str> = lines.by_ref().take(max_lines).collect();
    let more = lines.next().is_some();
    (head.join("\n"), more)
}

/// Where a tool call is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Failed,
    /// You did not approve it.
    Denied,
}

impl ToolState {
    /// From its result (`None`: no result yet). A denial comes back as an
    /// error whose text the engine writes for the model.
    pub fn of(result: Option<(bool, &str)>) -> Self {
        match result {
            None => ToolState::Running,
            Some((true, _)) => ToolState::Ok,
            Some((false, text)) => {
                let text = text.trim_start();
                if text.starts_with("The user did NOT approve")
                    || text.starts_with("The user did not approve")
                    || text.starts_with("Denied:")
                {
                    ToolState::Denied
                } else {
                    ToolState::Failed
                }
            }
        }
    }

    pub fn tone(self) -> Tone {
        match self {
            ToolState::Running => Tone::Info,
            ToolState::Ok => Tone::Success,
            ToolState::Failed => Tone::Danger,
            ToolState::Denied => Tone::Warning,
        }
    }

    pub fn label(self) -> SharedString {
        match self {
            ToolState::Running => t!("ai_ui.tool_state.running"),
            ToolState::Ok => t!("ai_ui.tool_state.ok"),
            ToolState::Failed => t!("ai_ui.tool_state.failed"),
            ToolState::Denied => t!("ai_ui.tool_state.denied"),
        }
    }
}

/// Columns of the per-host table that sort it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostSort {
    Host,
    Status,
    Duration,
}

/// Sorts the hosts of a multi-host task (ties keep the host's name order).
pub fn sort_hosts(hosts: &mut [HostRun], by: HostSort, ascending: bool) {
    hosts.sort_by(|a, b| {
        let order = match by {
            HostSort::Host => std::cmp::Ordering::Equal,
            HostSort::Status => host_phase(a).rank().cmp(&host_phase(b).rank()),
            // Without a duration yet (still running): at the end.
            HostSort::Duration => match (a.duration_ms, b.duration_ms) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            },
        };
        let order = if ascending { order } else { order.reverse() };
        order.then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
    });
    if by == HostSort::Host && !ascending {
        hosts.reverse();
    }
}

/// The phase of a host's conversation.
pub fn host_phase(h: &HostRun) -> Phase {
    Phase::of(h.status.as_str(), h.pending_approvals)
}

/// Finished hosts and all hosts of a multi-host task.
pub fn progress(hosts: &[HostRun]) -> (usize, usize) {
    let done = hosts.iter().filter(|h| host_phase(h).finished()).count();
    (done, hosts.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn host(label: &str, status: &str, ms: Option<i64>, pending: usize) -> HostRun {
        serde_json::from_value(json!({
            "host_id": termoak_core::new_id(),
            "label": label,
            "task_id": termoak_core::new_id(),
            "status": status,
            "duration_ms": ms,
            "cost_micros": 0,
            "pending_approvals": pending,
        }))
        .unwrap()
    }

    #[test]
    fn phases_of_statuses() {
        assert_eq!(Phase::of("running", 0), Phase::Running);
        assert_eq!(Phase::of("running", 2), Phase::NeedsApproval);
        assert_eq!(Phase::of("waiting_approval", 0), Phase::NeedsApproval);
        assert_eq!(Phase::of("completed", 0), Phase::Completed);
        assert_eq!(Phase::of("weird", 0), Phase::Unknown);
        assert!(Phase::of("queued", 0).active());
        assert!(Phase::of("cancelled", 0).finished());
        assert_eq!(Phase::Failed.tone(), Tone::Danger);
        assert_eq!(Phase::NeedsApproval.tone(), Tone::Warning);
    }

    #[test]
    fn filters() {
        let all = [
            Phase::Queued,
            Phase::Running,
            Phase::NeedsApproval,
            Phase::Completed,
            Phase::Failed,
            Phase::Cancelled,
        ];
        let count = |f: TaskFilter| all.iter().filter(|p| f.matches(**p)).count();
        assert_eq!(count(TaskFilter::All), 6);
        assert_eq!(count(TaskFilter::Running), 2);
        assert_eq!(count(TaskFilter::NeedsApproval), 1);
        assert_eq!(count(TaskFilter::Done), 3);
    }

    #[test]
    fn search_needs_every_word() {
        let fields = ["Restart nginx on web-1", "Production"];
        assert!(matches_search("", &fields));
        assert!(matches_search("NGINX", &fields));
        assert!(matches_search("nginx prod", &fields));
        assert!(!matches_search("nginx staging", &fields));
    }

    #[test]
    fn times_durations_and_costs() {
        let now = 1_000_000_000_000;
        assert_eq!(relative_time(now, now - 10_000), t!("ai_ui.time.just_now"));
        assert_eq!(
            relative_time(now, now - 5 * 60_000),
            t!("ai_ui.time.minutes_ago", n = 5)
        );
        assert_eq!(
            relative_time(now, now - 3 * 3_600_000),
            t!("ai_ui.time.hours_ago", n = 3)
        );
        assert_eq!(
            relative_time(now, now - 2 * 86_400_000),
            t!("ai_ui.time.days_ago", n = 2)
        );
        assert_eq!(relative_time(now, 0), "");
        assert_eq!(format_duration(850), "850 ms");
        assert_eq!(format_duration(12_400), "12 s");
        assert_eq!(format_duration(184_000), "3 min 4 s");
        assert_eq!(format_duration(3_720_000), "1 h 2 min");
        assert_eq!(format_cost(0), None);
        assert_eq!(format_cost(4_200).as_deref(), Some("$0.0042"));
        assert_eq!(format_cost(1_270_000).as_deref(), Some("$1.27"));
        assert_eq!(model_name("claude::claude-opus-5"), "claude-opus-5");
        assert_eq!(model_name("openrouter"), "openrouter");
    }

    #[test]
    fn tools() {
        assert_eq!(ToolKind::of("run_command"), ToolKind::RunCommand);
        assert_eq!(ToolKind::of("mcp_thing"), ToolKind::Other);
        assert_eq!(ToolKind::Other.label("mcp_thing"), "mcp_thing");
        assert!(ToolKind::SendToTerminal.is_command());
        let input = json!({"host": " web-1 ", "command": "df -h"});
        assert_eq!(tool_target(&input).as_deref(), Some("df -h"));
        assert_eq!(tool_host(&input).as_deref(), Some("web-1"));
        assert_eq!(
            tool_target(&json!({"session_id": "x", "input": "ls"})).as_deref(),
            Some("ls")
        );
        assert_eq!(tool_target(&json!({})), None);
        assert_eq!(output_preview("a\nb\nc\n", 2), ("a\nb".to_string(), true));
        assert_eq!(output_preview("a\nb\n\n", 2), ("a\nb".to_string(), false));
        assert_eq!(ToolState::of(None), ToolState::Running);
        assert_eq!(ToolState::of(Some((true, "x"))), ToolState::Ok);
        assert_eq!(ToolState::of(Some((false, "exit 1"))), ToolState::Failed);
        assert_eq!(
            ToolState::of(Some((false, "The user did NOT approve this action."))),
            ToolState::Denied
        );
        assert_eq!(
            ToolState::of(Some((false, "Denied: not now."))),
            ToolState::Denied
        );
    }

    #[test]
    fn host_table_sorting_and_progress() {
        let mut hosts = vec![
            host("web-2", "completed", Some(9_000), 0),
            host("db", "running", None, 1),
            host("web-1", "failed", Some(3_000), 0),
            host("cache", "running", None, 0),
        ];
        assert_eq!(progress(&hosts), (2, 4));
        let labels = |h: &[HostRun]| h.iter().map(|h| h.label.clone()).collect::<Vec<_>>();
        sort_hosts(&mut hosts, HostSort::Status, true);
        assert_eq!(labels(&hosts), ["db", "cache", "web-1", "web-2"]);
        sort_hosts(&mut hosts, HostSort::Duration, true);
        assert_eq!(labels(&hosts), ["web-1", "web-2", "cache", "db"]);
        sort_hosts(&mut hosts, HostSort::Duration, false);
        assert_eq!(labels(&hosts)[..2], ["cache", "db"]);
        sort_hosts(&mut hosts, HostSort::Host, true);
        assert_eq!(labels(&hosts), ["cache", "db", "web-1", "web-2"]);
        sort_hosts(&mut hosts, HostSort::Host, false);
        assert_eq!(labels(&hosts), ["web-2", "web-1", "db", "cache"]);
    }
}
