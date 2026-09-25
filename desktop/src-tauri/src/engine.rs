//! The webview's route into the engine.
//!
//! [`Engine::call`] hands every request to [`wallpaper_core::Core`], which answers all
//! of them in-process. This was the seam the Rust port grew through: for eight phases
//! it asked the core first and forwarded whatever came back as `NotPorted` to a Python
//! sidecar over stdio. The sidecar is gone, and that arm is now the allowlist gate —
//! a method the core does not know reaches the front end as `unknown_method`.
//!
//! It remains the single choke point: `lib.rs`, `tray.rs`, `hotkeys.rs` and the
//! `engine_call` command all funnel through `call`.
//!
//! What went with the sidecar: the child process and its `%PATH%` discovery, the
//! newline-JSON framing, the id/`oneshot` correlation table, the two reader threads,
//! a 120-second call timeout, a 5-second shutdown grace period followed by a kill, and
//! the `Bridge` that let the core call *back* into Python while ownership straddled
//! the two. None of it has an equivalent here, because there is no longer a second
//! process to lose, wedge, or race.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use wallpaper_core::{Core, Dispatch, EventSink, Notifier};

/// Event name the webview listens on for unsolicited engine events.
pub const ENGINE_EVENT: &str = "engine-event";

/// A call at least this long is written to the log with its time.
const SLOW_CALL: Duration = Duration::from_millis(100);

/// Forwards core events to the webview.
///
/// The `{"event": ..., "data": ...}` envelope is the one the sidecar's `dispatch_line`
/// used to re-emit verbatim. It is kept because the front end parses it, and because
/// nothing about the protocol needed to change when the process behind it did.
struct WebviewSink {
    app: AppHandle,
}

impl EventSink for WebviewSink {
    fn emit(&self, event: &str, data: Value) {
        let _ = self
            .app
            .emit(ENGINE_EVENT, json!({ "event": event, "data": data }));
    }
}

/// Shows a real Windows toast, through the plugin the shell already carries.
///
/// The core cannot do this itself — it has no `tauri` dependency by design — so it
/// takes a [`Notifier`] the same way it takes a `WallpaperSetter`. This replaces
/// `notifications.py`, which built a PowerShell script and spawned
/// `powershell.exe -EncodedCommand` once per toast.
struct TauriNotifier {
    app: AppHandle,
}

impl Notifier for TauriNotifier {
    fn notify(&self, title: &str, message: &str) -> Result<(), wallpaper_core::CoreError> {
        use tauri_plugin_notification::NotificationExt;
        self.app
            .notification()
            .builder()
            .title(title)
            .body(message)
            .show()
            .map_err(|e| wallpaper_core::CoreError::error(format!("Could not notify: {e}")))
    }
}

pub struct Engine {
    core: Arc<Core>,
}

impl Engine {
    /// Build the core and bring back whatever was running when the app last closed.
    pub fn spawn(app: &AppHandle) -> Result<Self, String> {
        // Where the bundled libmpv landed. The core has no `tauri` dependency and so
        // cannot resolve a resource path itself, and in a packaged build this is the
        // only copy of the DLL. Must happen before anything tries to play.
        if let Ok(dir) = app
            .path()
            .resolve("libmpv", tauri::path::BaseDirectory::Resource)
        {
            wallpaper_core::video::set_search_dir(dir);
        }

        let core = Arc::new(Core::new(
            Arc::new(WebviewSink { app: app.clone() }),
            Arc::new(TauriNotifier { app: app.clone() }),
        ));

        // The sidecar used to do this on a daemon thread at start-up — never through
        // `dispatch`, so the port's seam never saw it, which is why each half had to
        // move the moment its unit was ported or there would have been two rotation
        // timers and two video players. On a task rather than inline because starting
        // the video wallpaper spins up mpv and reparents windows, and that must not
        // delay the first request the shell sends.
        let restoring = Arc::clone(&core);
        tauri::async_runtime::spawn(async move {
            let restored = restoring.session().restore_session().await;
            log::info!(
                "session restored: rotation={} video={}",
                restored["rotation"],
                restored["video"]
            );
        });

        // Same story, and inline rather than spawned: the hook is what a user who left
        // the switch on expects to be working the moment the app is up.
        match core.session().sync_scroll_transparency() {
            Ok(status) if status["running"] == true => {
                log::info!("scroll transparency active on {}+wheel", status["modifier"]);
            }
            Ok(_) => {}
            // Never fatal: everything else still works without it.
            Err(e) => log::warn!("could not start scroll transparency: {e}"),
        }

        // The all-windows fade has nothing of its own that survives a reboot — Windows
        // forgets a window's opacity with the window — so this is what brings it back.
        // With autostart on, that is every login.
        match core.session().sync_all_windows_opacity(None) {
            Ok(status) if status["running"] == true => {
                log::info!("all-windows opacity active at {}", status["alpha"]);
            }
            Ok(_) => {}
            Err(e) => log::warn!("could not start the all-windows opacity: {e}"),
        }

        Ok(Self { core })
    }

    /// Answer a request.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let started = Instant::now();
        let outcome = self.dispatch(method, &params).await;
        // Every call worth waiting for is logged with its time, so "it is slow" can be
        // answered from the log file instead of a guess. Metadata calls answer in a
        // millisecond or two and stay out of it.
        let elapsed = started.elapsed();
        if elapsed >= SLOW_CALL {
            log::info!(
                "{method} took {} ms{}",
                elapsed.as_millis(),
                if outcome.is_ok() { "" } else { " (failed)" }
            );
        }
        outcome
    }

    async fn dispatch(&self, method: &str, params: &Value) -> Result<Value, String> {
        match self.core.dispatch(method, params).await {
            // The front end reads the "{kind}: {message}" prefix to tell a `busy` from
            // a `not_found`, so the shape of this string is part of the contract.
            Dispatch::Handled(result) => result.map_err(|e| format!("{}: {}", e.kind(), e)),
            // Nothing is behind the core any more, so this is the allowlist gate.
            Dispatch::NotPorted => Err(format!("unknown_method: Unknown method: {method}")),
        }
    }

    /// Release everything that outlives the process if it is not let go of.
    ///
    /// The first three used to belong to the sidecar and died with it; the window
    /// fade is newer, and the same kind of thing. In-process they have to be given up
    /// explicitly, and this runs from `RunEvent::Exit`.
    pub fn shutdown(&self) {
        // A tick during teardown would composite and call `SystemParametersInfoW` on a
        // process on its way out. This does not clear `rotation_active`, so a rotation
        // left running still comes back on the next launch.
        self.core.session().stop_rotation_for_exit();
        // A WH_MOUSE_LL hook is system-wide: leaving it installed would route every
        // wheel event on the machine through a process that is going away.
        self.core.session().stop_scroll_for_exit();
        // The same for the WinEvent hook — and every window it faded goes back to how
        // it was, since nothing would be left to manage them.
        self.core.session().stop_windows_for_exit();
        // The video host windows are children of WORKERW, so they sit on the desktop
        // until Explorer restarts if they are not destroyed here.
        self.core.session().stop_video_for_exit();
    }
}
