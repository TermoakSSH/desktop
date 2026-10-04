//! Bridge between tokio (SSH engine, server API, sync) and GPUI.
//!
//! Everything that touches the network or the disk runs on a multi-threaded
//! tokio runtime; the interface only awaits the result (tokio `JoinHandle`s
//! can be awaited from the GPUI executor) and never blocks the main thread.

use std::fmt::Display;
use std::future::Future;
use std::sync::Arc;

use gpui::{App, Context, Global, Window};

/// Tokio runtime shared by the whole app.
pub struct Runtime(pub Arc<tokio::runtime::Runtime>);

impl Global for Runtime {}

/// Registers the runtime as a GPUI global.
pub fn init(cx: &mut App, rt: Arc<tokio::runtime::Runtime>) {
    cx.set_global(Runtime(rt));
}

/// Runtime handle (to spawn tasks from anywhere).
pub fn handle(cx: &App) -> tokio::runtime::Handle {
    cx.global::<Runtime>().0.handle().clone()
}

/// Spawns `fut` on tokio and returns a future GPUI can await. Errors
/// (including a panic of the task) are turned into text to show them in the
/// interface.
pub fn spawn<T, E, F>(cx: &App, fut: F) -> impl Future<Output = Result<T, String>> + 'static
where
    T: Send + 'static,
    E: Display + Send + 'static,
    F: Future<Output = Result<T, E>> + Send + 'static,
{
    let task = cx.global::<Runtime>().0.spawn(fut);
    async move {
        match task.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e.to_string()),
            Err(e) => Err(t!("common.task_interrupted", error = e).to_string()),
        }
    }
}

/// Runs `fut` on tokio and applies the result to the view `T` on the
/// interface thread (with access to the window, e.g. to show notifications).
pub fn run_in<T, R, E, F>(
    cx: &mut Context<T>,
    window: &Window,
    fut: F,
    done: impl FnOnce(&mut T, Result<R, String>, &mut Window, &mut Context<T>) + 'static,
) where
    T: 'static,
    R: Send + 'static,
    E: Display + Send + 'static,
    F: Future<Output = Result<R, E>> + Send + 'static,
{
    let task = spawn(cx, fut);
    cx.spawn_in(window, async move |this, cx| {
        let res = task.await;
        let _ = this.update_in(cx, |this, window, cx| done(this, res, window, cx));
    })
    .detach();
}

/// Like [`run_in`] but without a window (for models that draw nothing).
pub fn run<T, R, E, F>(
    cx: &mut Context<T>,
    fut: F,
    done: impl FnOnce(&mut T, Result<R, String>, &mut Context<T>) + 'static,
) where
    T: 'static,
    R: Send + 'static,
    E: Display + Send + 'static,
    F: Future<Output = Result<R, E>> + Send + 'static,
{
    let task = spawn(cx, fut);
    cx.spawn(async move |this, cx| {
        let res = task.await;
        let _ = this.update(cx, |this, cx| done(this, res, cx));
    })
    .detach();
}
