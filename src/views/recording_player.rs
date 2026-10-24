//! Recordings of a vault's sessions (vault Admins, docs-next/
//! TEAMS-SECURITY.md §6.2): the list (`GET /vaults/{id}/recordings`),
//! downloading one as an asciicast file and a simple player that replays it
//! into a terminal model (play/pause, restart, speed) and shows the screen
//! as text.

use std::time::Duration;

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, IntoElement, ParentElement, Render, Styled, Task,
    Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, Sizable, WindowExt, h_flex, v_flex};
use serde::Deserialize;
use termoak_core::Id;

use crate::runtime;
use crate::state::{AppModel, api_error};
use crate::terminal::model::TermModel;
use crate::ui::{self, IconName};

/// A recorded session of a vault.
#[derive(Debug, Clone, Deserialize)]
pub struct VaultRecording {
    pub session_id: Id,
    #[serde(default)]
    pub owner_email: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub kind: Option<String>,
    pub created_at: i64,
    #[serde(default)]
    pub ended_at: Option<i64>,
}

/// An asciicast v2 recording: its size and its output events.
#[derive(Debug, Clone, PartialEq)]
pub struct Cast {
    pub cols: u16,
    pub rows: u16,
    /// `(seconds since the start, output)`.
    pub events: Vec<(f64, String)>,
}

impl Cast {
    /// Reads an asciicast v2 file (other event kinds are skipped).
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let header: serde_json::Value = lines
            .next()
            .and_then(|l| serde_json::from_str(l).ok())
            .ok_or_else(|| t!("recordings.error.format").to_string())?;
        if header["version"].as_u64() != Some(2) {
            return Err(t!("recordings.error.format").to_string());
        }
        let size = |k: &str, d: u16| {
            header[k]
                .as_u64()
                .and_then(|v| u16::try_from(v).ok())
                .filter(|v| *v > 0)
                .unwrap_or(d)
        };
        let mut events = Vec::new();
        for l in lines {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(l) else {
                continue;
            };
            if v[1] == "o"
                && let (Some(t), Some(s)) = (v[0].as_f64(), v[2].as_str())
            {
                events.push((t, s.to_string()));
            }
        }
        Ok(Self {
            cols: size("width", 80),
            rows: size("height", 24),
            events,
        })
    }

    /// Length in seconds.
    pub fn duration(&self) -> f64 {
        self.events.last().map_or(0.0, |e| e.0)
    }
}

/// "03:45".
pub fn clock(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{:02}:{:02}", s / 60, s % 60)
}

pub struct RecordingPlayer {
    cast: Cast,
    model: TermModel,
    /// Events already fed.
    next: usize,
    /// Playback position (s).
    position: f64,
    playing: bool,
    speed: f64,
    /// Wall clock of the last play/speed change and the position then.
    clock_base: (std::time::Instant, f64),
    _timer: Option<Task<()>>,
}

impl RecordingPlayer {
    pub fn new(cast: Cast, cx: &mut Context<Self>) -> Self {
        let model = TermModel::new(cast.cols, cast.rows, 0);
        let mut this = Self {
            cast,
            model,
            next: 0,
            position: 0.0,
            playing: false,
            speed: 1.0,
            clock_base: (std::time::Instant::now(), 0.0),
            _timer: None,
        };
        this.play(cx);
        this
    }

    fn restart(&mut self) {
        self.model = TermModel::new(self.cast.cols, self.cast.rows, 0);
        self.next = 0;
        self.position = 0.0;
    }

    /// Feeds every event up to the position.
    fn advance(&mut self) {
        while let Some((t, out)) = self.cast.events.get(self.next) {
            if *t > self.position {
                break;
            }
            self.model.feed(out.as_bytes());
            self.next += 1;
        }
    }

    fn play(&mut self, cx: &mut Context<Self>) {
        if self.next >= self.cast.events.len() {
            self.restart();
        }
        self.playing = true;
        self.clock_base = (std::time::Instant::now(), self.position);
        const STEP: Duration = Duration::from_millis(50);
        self._timer = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(STEP).await;
                let go = this
                    .update(cx, |p, cx| {
                        if !p.playing {
                            return false;
                        }
                        // The wall clock, not the ticks (slow repaints do not
                        // slow the replay down).
                        let (at, from) = p.clock_base;
                        p.position = from + at.elapsed().as_secs_f64() * p.speed;
                        p.advance();
                        if p.next >= p.cast.events.len() {
                            p.playing = false;
                        }
                        cx.notify();
                        p.playing
                    })
                    .unwrap_or(false);
                if !go {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn pause(&mut self, cx: &mut Context<Self>) {
        self.playing = false;
        self._timer = None;
        cx.notify();
    }
}

impl Render for RecordingPlayer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let speed = self.speed;
        // Every row of the recorded size (the screen text leaves out the
        // empty ones at the end).
        let rows = usize::from(self.cast.rows.clamp(5, 40));
        let mut lines: Vec<String> = self
            .model
            .screen_text()
            .lines()
            .map(str::to_string)
            .collect();
        lines.resize(rows, String::new());
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("rec-play")
                            .small()
                            .primary()
                            .icon(ui::icon(if self.playing {
                                IconName::Pause
                            } else {
                                IconName::Play
                            }))
                            .label(if self.playing {
                                t!("recordings.pause")
                            } else {
                                t!("recordings.play")
                            })
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                if this.playing {
                                    this.pause(cx)
                                } else {
                                    this.play(cx)
                                }
                            })),
                    )
                    .child(
                        Button::new("rec-restart")
                            .small()
                            .ghost()
                            .icon(ui::icon(IconName::RotateCcw))
                            .tooltip(t!("recordings.restart"))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.restart();
                                this.play(cx);
                            })),
                    )
                    .children(
                        [1.0f64, 2.0, 4.0, 8.0]
                            .into_iter()
                            .enumerate()
                            .map(|(i, s)| {
                                Button::new(("rec-speed", i))
                                    .xsmall()
                                    .label(format!("{s}×"))
                                    .map(|b| {
                                        if (speed - s).abs() < f64::EPSILON {
                                            b.primary()
                                        } else {
                                            b.ghost()
                                        }
                                    })
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.clock_base =
                                            (std::time::Instant::now(), this.position);
                                        this.speed = s;
                                        cx.notify();
                                    }))
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_right()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "{} / {}",
                                clock(self.position.min(self.cast.duration())),
                                clock(self.cast.duration())
                            )),
                    ),
            )
            .child(
                div()
                    .p_2()
                    .rounded(theme.radius)
                    .bg(gpui::black())
                    .text_color(gpui::white())
                    .text_xs()
                    .font_family(ui::mono_family(cx))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .children(lines.into_iter().map(|l| div().h(px(15.)).child(l))),
            )
    }
}

/// Downloads a recording (`play`: into the player; else saved where the
/// user says).
pub fn open_recording(
    model: Entity<AppModel>,
    account: Id,
    vault: Id,
    rec: VaultRecording,
    play: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(api) = model.read(cx).api_of(Some(account)) else {
        ui::error(window, cx, t!("vaults.not_signed_in"));
        return;
    };
    let path = format!("/api/v1/vaults/{vault}/recordings/{}", rec.session_id);
    let task = runtime::spawn(
        cx,
        async move { api.get_bytes(&path).await.map_err(api_error) },
    );
    let title = if rec.title.trim().is_empty() {
        rec.session_id.to_string()
    } else {
        rec.title.clone()
    };
    window
        .spawn(cx, async move |cx| {
            let res = task.await;
            let _ = cx.update(|window, cx| match res {
                Err(e) => ui::error(window, cx, e),
                Ok(bytes) if play => match Cast::parse(&String::from_utf8_lossy(&bytes)) {
                    Ok(cast) => {
                        let view = cx.new(|cx| RecordingPlayer::new(cast, cx));
                        let title = t!("recordings.player_title", title = title.clone());
                        window.open_dialog(cx, move |d, _, _| {
                            d.title(title.clone()).w(px(900.)).child(view.clone())
                        });
                    }
                    Err(e) => ui::error(window, cx, e),
                },
                Ok(bytes) => {
                    let dir = directories::UserDirs::new()
                        .and_then(|d| d.download_dir().map(std::path::Path::to_path_buf))
                        .unwrap_or_else(|| std::path::PathBuf::from("."));
                    let name = format!("{}.cast", rec.session_id);
                    let rx = cx.prompt_for_new_path(&dir, Some(&name));
                    window
                        .spawn(cx, async move |cx| {
                            let Ok(Ok(Some(path))) = rx.await else {
                                return;
                            };
                            let res = std::fs::write(&path, &bytes);
                            let _ = cx.update(|window, cx| match res {
                                Ok(()) => ui::success(
                                    window,
                                    cx,
                                    t!("recordings.saved", path = path.display().to_string()),
                                ),
                                Err(e) => ui::error(window, cx, e.to_string()),
                            });
                        })
                        .detach();
                }
            });
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_asciicast_v2() {
        let text = "{\"version\": 2, \"width\": 100, \"height\": 30}\n[0.5, \"o\", \"hello\\r\\n\"]\n[0.7, \"i\", \"x\"]\n[1.25, \"o\", \"bye\"]\n";
        let c = Cast::parse(text).unwrap();
        assert_eq!((c.cols, c.rows), (100, 30));
        assert_eq!(c.events.len(), 2);
        assert_eq!(c.duration(), 1.25);
        assert!(Cast::parse("{\"version\": 1}").is_err());
        assert!(Cast::parse("").is_err());
        assert_eq!(clock(225.9), "03:45");
    }

    #[test]
    fn the_player_model_shows_the_output() {
        let c = Cast::parse(
            "{\"version\": 2, \"width\": 20, \"height\": 3}\n[0.1, \"o\", \"ls -la\"]\n",
        )
        .unwrap();
        let mut m = TermModel::new(c.cols, c.rows, 0);
        m.feed(c.events[0].1.as_bytes());
        assert!(m.screen_text().contains("ls -la"));
    }
}
