//! One opacity for every window — the ones open now and the ones opened later.
//!
//! [`crate::transparency`] fades a window when asked; this keeps a whole desktop
//! faded. It owns a system-wide WinEvent hook, so it is the fourth thing the engine
//! has to give up explicitly on the way out (after the rotation timer, the mouse
//! hook and the video host windows): [`WindowWatch::stop`] unhooks *and* puts every
//! window it faded back the way it found it.
//!
//! ## What survives a restart, and what cannot
//!
//! Windows does not remember a layered window's opacity. It dies with the window, and
//! no window survives a reboot. So "persistent" means the setting lives in
//! `settings.toml` and `Engine::spawn` turns this back on — which, with *Start with
//! Windows* on, happens at every login. Nothing here writes to disk.
//!
//! ## Threads
//!
//! The same split as [`crate::scroll`]:
//!
//! - the **hook thread** installs `SetWinEventHook(WINEVENT_OUTOFCONTEXT)` — which
//!   only delivers to a thread that pumps messages — and forwards window handles down
//!   a channel. It opens no process handle and touches no window.
//! - the **worker thread** decides and applies: it reads the window's facts, resolves
//!   the alpha, fades it, and remembers what it changed so it can undo it.
//!
//! Three events are watched. `SHOW` is a window appearing. `UNCLOAKED` is a Store app
//! appearing: those are created cloaked and shown by DWM, never by `ShowWindow`, so
//! `SHOW` alone would miss every one of them. `DESTROY` lets a handle go, so a new
//! window that is handed a recycled value is not mistaken for one already faded.
//!
//! ## Windows that are already layered
//!
//! A window can be layered for its own reasons. Two kinds have to be told apart:
//!
//! - one that set an **alpha** with `SetLayeredWindowAttributes`. It can be faded,
//!   and on the way out it gets *its own* alpha back — not 255, and not its
//!   `WS_EX_LAYERED` stripped, which is what [`crate::transparency::set_opacity`] at
//!   255 would do to it.
//! - one drawn with `UpdateLayeredWindow` (or keyed on a colour). Calling
//!   `SetLayeredWindowAttributes` on that switches it to a different rendering model
//!   and breaks it, often into a black rectangle. `GetLayeredWindowAttributes` fails
//!   for these, and that failure is the signal: they are left alone.
//!
//! ## The global value wins
//!
//! While this is on, every window takes the one alpha — including apps that have a
//! per-app opacity in `transparency.json`. That file fills up with near-opaque values
//! from the scroll wheel (`Code.exe: 250`), and letting them win made the slider look
//! broken: on a desktop where every open app had one, moving it changed nothing. The
//! per-app values are not lost: a window already faded to its own alpha is put back
//! to exactly that when this is turned off, and the scroll hook and the per-window
//! slider still work on top of it until the next sweep.
//!
//! ## What cannot be faded
//!
//! A window belonging to an elevated process (Task Manager, an admin terminal) refuses
//! a non-elevated caller — UIPI — and the call fails without the window changing. It is
//! simply not tracked. The app's own window is never touched: the hook skips our
//! process, and so does the sweep.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

/// The slider's floor, shared with the per-window slider and the scroll hook.
pub const MIN_ALPHA: i64 = 20;

/// What a fresh install fades to when the switch is first turned on: 80%.
pub const DEFAULT_ALPHA: i64 = 204;

/// Window classes that belong to the shell rather than to an application. Fading
/// the desktop, the taskbar or the Start surfaces is never what "all windows" means.
const SHELL_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Windows.UI.Core.CoreWindow",
    "MSCTFIME UI",
];

/// How a window was layered before we touched it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layering {
    /// Not layered: the ordinary rendering path.
    None,
    /// Layered with an alpha of its own, from `SetLayeredWindowAttributes`.
    Alpha(u8),
    /// Layered some other way — `UpdateLayeredWindow`, or a colour key.
    Other,
}

/// Everything the fade decision needs about one window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowFacts {
    pub visible: bool,
    pub cloaked: bool,
    /// `WS_CHILD`. Only top-level windows can be layered on Windows 7 rules, and a
    /// child fades with its parent anyway.
    pub child: bool,
    /// Has a `GW_OWNER`: dialogs, menus, tooltips. They follow their owner's
    /// lifetime and would otherwise stack their fade on top of it.
    pub owned: bool,
    pub tool_window: bool,
    pub own_process: bool,
    pub class: String,
    pub process: String,
    pub layering: Layering,
}

/// Whether this window should take the all-windows fade.
pub fn is_fadeable(facts: &WindowFacts) -> bool {
    facts.visible
        && !facts.cloaked
        && !facts.child
        && !facts.owned
        && !facts.tool_window
        && !facts.own_process
        && !facts.process.is_empty()
        && !SHELL_CLASSES.contains(&facts.class.as_str())
        && facts.layering != Layering::Other
}

/// The alpha `[transparency]` asks for, or `None` when the feature is off.
pub fn configured_alpha(cfg: &Value) -> Option<u8> {
    let enabled = cfg
        .pointer("/transparency/all_windows")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    enabled.then(|| {
        cfg.pointer("/transparency/all_windows_alpha")
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
            .unwrap_or(DEFAULT_ALPHA)
            .clamp(MIN_ALPHA, 255) as u8
    })
}

// ── the Win32 surface, behind a trait ────────────────────────────────────────

/// What the worker needs from the desktop. Faked in tests, so the fade and restore
/// rules are checked without a single real window changing.
pub trait Windows: Send + Sync {
    /// Every top-level window, visible or not.
    fn top_level(&self) -> Vec<isize>;
    /// `None` when the handle no longer names a window.
    fn facts(&self, hwnd: isize) -> Option<WindowFacts>;
    /// Layer the window if needed and set its alpha. `false` if Windows refused.
    fn fade(&self, hwnd: isize, alpha: u8) -> bool;
    /// Put the window back the way [`Layering`] says it was.
    fn restore(&self, hwnd: isize, was: Layering);
}

/// The real desktop.
pub struct RealWindows;

impl RealWindows {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RealWindows {
    fn default() -> Self {
        Self::new()
    }
}

impl Windows for RealWindows {
    fn top_level(&self) -> Vec<isize> {
        platform::top_level()
    }
    fn facts(&self, hwnd: isize) -> Option<WindowFacts> {
        platform::facts(hwnd)
    }
    fn fade(&self, hwnd: isize, alpha: u8) -> bool {
        platform::fade(hwnd, alpha)
    }
    fn restore(&self, hwnd: isize, was: Layering) {
        platform::restore(hwnd, was)
    }
}

// ── the worker ───────────────────────────────────────────────────────────────

/// What reaches the worker, from the hook or from the caller.
enum Message {
    /// A window was shown or uncloaked.
    Shown(isize),
    /// A window was destroyed.
    Destroyed(isize),
    /// A new global alpha: fade every window again.
    Sweep(u8),
}

struct Worker {
    windows: Arc<dyn Windows>,
    global: u8,
    /// Every window we changed, with how it was before.
    faded: HashMap<isize, Layering>,
    /// Published for the status, which cannot ask the worker directly.
    count: Arc<AtomicUsize>,
}

impl Worker {
    fn new(windows: Arc<dyn Windows>, count: Arc<AtomicUsize>) -> Self {
        Self {
            windows,
            global: 255,
            faded: HashMap::new(),
            count,
        }
    }

    fn handle(&mut self, message: Message) {
        match message {
            Message::Shown(hwnd) => self.consider(hwnd),
            Message::Destroyed(hwnd) => {
                self.faded.remove(&hwnd);
            }
            Message::Sweep(alpha) => {
                self.global = alpha;
                // Handles we faded that are gone now; nothing to restore on those.
                let alive: Vec<isize> = self.windows.top_level();
                self.faded.retain(|hwnd, _| alive.contains(hwnd));
                for hwnd in alive {
                    self.consider(hwnd);
                }
            }
        }
        self.count.store(self.faded.len(), Ordering::Relaxed);
    }

    /// Fade one window to what it should be at, or leave it.
    fn consider(&mut self, hwnd: isize) {
        let Some(facts) = self.windows.facts(hwnd) else {
            self.faded.remove(&hwnd);
            return;
        };
        if !is_fadeable(&facts) {
            return;
        }
        let alpha = self.global;
        // How it was before *we* touched it. Once faded, what the window reports is
        // our own alpha, so the first reading is the one that is kept.
        let was = self.faded.get(&hwnd).copied().unwrap_or(facts.layering);

        if alpha == 255 {
            // Opaque is not "layered at 255": put it back as it was, if it was ours.
            if self.faded.remove(&hwnd).is_some() {
                self.windows.restore(hwnd, was);
            }
            return;
        }
        if self.windows.fade(hwnd, alpha) {
            self.faded.insert(hwnd, was);
        }
    }

    /// Everything back the way it was found.
    fn restore_all(&mut self) {
        for (hwnd, was) in self.faded.drain() {
            // A handle that is no longer a window has nothing left to restore — and
            // a recycled one belongs to someone else now.
            if self.windows.facts(hwnd).is_some() {
                self.windows.restore(hwnd, was);
            }
        }
        self.count.store(0, Ordering::Relaxed);
    }
}

/// Handle messages until every sender is gone, then restore.
fn run_worker(rx: Receiver<Message>, windows: Arc<dyn Windows>, count: Arc<AtomicUsize>) {
    let mut worker = Worker::new(windows, count);
    for message in rx {
        worker.handle(message);
    }
    worker.restore_all();
}

fn log_warn(message: &str) {
    log::warn!("all-windows opacity: {message}");
}

// ── the watch itself ─────────────────────────────────────────────────────────

/// Owns the hook thread and the worker. One per session.
pub struct WindowWatch {
    windows: Arc<dyn Windows>,
    running: Mutex<Option<Running>>,
    faded: Arc<AtomicUsize>,
}

struct Running {
    alpha: u8,
    thread_id: u32,
    /// The caller's line to the worker. Dropped before joining: the worker only stops
    /// once *every* sender is gone.
    control: Sender<Message>,
    hook: std::thread::JoinHandle<()>,
    worker: std::thread::JoinHandle<()>,
}

impl WindowWatch {
    pub fn new() -> Self {
        Self::with_windows(Arc::new(RealWindows::new()))
    }

    pub fn with_windows(windows: Arc<dyn Windows>) -> Self {
        Self {
            windows,
            running: Mutex::new(None),
            faded: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// The alpha being applied, if running.
    pub fn alpha(&self) -> Option<u8> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|r| r.alpha)
    }

    /// Fade everything to *alpha*, or turn the watch off and restore with `None`.
    ///
    /// Always re-sweeps when on, even at an unchanged alpha: this is called after a
    /// save, and a sweep also catches windows that something else has changed since.
    /// Reports whether the watch is running afterwards.
    pub fn set(&self, alpha: Option<u8>) -> bool {
        let Some(alpha) = alpha else {
            self.stop();
            return false;
        };
        {
            let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(running) = running.as_mut() {
                running.alpha = alpha;
                let _ = running.control.send(Message::Sweep(alpha));
                return true;
            }
        }
        self.start(alpha)
    }

    fn start(&self, alpha: u8) -> bool {
        let (tx, rx) = channel::<Message>();
        let (ready_tx, ready_rx) = channel::<Result<u32, String>>();

        let windows = Arc::clone(&self.windows);
        let count = Arc::clone(&self.faded);
        let worker = std::thread::spawn(move || run_worker(rx, windows, count));
        let hook_tx = tx.clone();
        let hook = std::thread::spawn(move || platform::run_hook(hook_tx, ready_tx));

        match ready_rx.recv() {
            Ok(Ok(thread_id)) => {
                // Hook first, sweep second: a window that opens between the two is
                // caught by the hook rather than falling through the gap.
                let _ = tx.send(Message::Sweep(alpha));
                *self.running.lock().unwrap_or_else(|e| e.into_inner()) = Some(Running {
                    alpha,
                    thread_id,
                    control: tx,
                    hook,
                    worker,
                });
                true
            }
            outcome => {
                match outcome {
                    Ok(Err(e)) => log_warn(&format!("could not install the window hook: {e}")),
                    _ => log_warn("the hook thread stopped before reporting"),
                }
                drop(tx);
                let _ = hook.join();
                let _ = worker.join();
                false
            }
        }
    }

    /// Unhook, and put every faded window back. Safe when nothing was started.
    pub fn stop(&self) {
        let running = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(running) = running else { return };

        platform::post_quit(running.thread_id);
        let _ = running.hook.join();
        drop(running.control);
        // Joining is the promise: when this returns, every window is restored.
        let _ = running.worker.join();
    }

    /// `{running, alpha, faded}` — what is installed, not what was asked for.
    pub fn status(&self) -> Value {
        let alpha = self.alpha();
        json!({
            "running": alpha.is_some(),
            "alpha": alpha,
            "faded": if alpha.is_some() { self.faded.load(Ordering::Relaxed) } else { 0 },
        })
    }
}

impl Default for WindowWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WindowWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

// ── Win32 ────────────────────────────────────────────────────────────────────

#[cfg(windows)]
mod platform {
    use std::cell::RefCell;
    use std::sync::mpsc::Sender;

    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, WPARAM};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
    use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetLayeredWindowAttributes, GetMessageW, GetWindow,
        GetWindowLongPtrW, GetWindowThreadProcessId, IsWindow, IsWindowVisible, PostThreadMessageW,
        SetLayeredWindowAttributes, SetWindowLongPtrW, CHILDID_SELF, EVENT_OBJECT_DESTROY,
        EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED, GWL_EXSTYLE, GWL_STYLE, GW_OWNER,
        LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA, LWA_COLORKEY, MSG, OBJID_WINDOW,
        WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_QUIT, WS_CHILD, WS_EX_LAYERED,
        WS_EX_TOOLWINDOW,
    };

    use super::{Layering, Message, WindowFacts};

    fn hwnd(value: isize) -> HWND {
        HWND(value as *mut _)
    }

    fn is_cloaked(hwnd: HWND) -> bool {
        let mut cloaked: i32 = 0;
        let ok = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut i32 as *mut _,
                std::mem::size_of::<i32>() as u32,
            )
        };
        ok.is_ok() && cloaked != 0
    }

    fn class_name(hwnd: HWND) -> String {
        let mut buffer = [0u16; 256];
        let written = unsafe { GetClassNameW(hwnd, &mut buffer) };
        if written <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..(written as usize).min(buffer.len())])
    }

    fn layering(hwnd: HWND, ex_style: isize) -> Layering {
        if ex_style & WS_EX_LAYERED.0 as isize == 0 {
            return Layering::None;
        }
        let mut alpha: u8 = 255;
        let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
        let read =
            unsafe { GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), Some(&mut flags)) };
        // Fails for `UpdateLayeredWindow` windows; a colour key would be lost by
        // setting an alpha alone. Either way: not ours to change.
        if read.is_err() || flags.0 & LWA_COLORKEY.0 != 0 {
            return Layering::Other;
        }
        if flags.0 & LWA_ALPHA.0 != 0 {
            Layering::Alpha(alpha)
        } else {
            Layering::Alpha(255)
        }
    }

    pub fn facts(value: isize) -> Option<WindowFacts> {
        let hwnd = hwnd(value);
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return None;
        }
        let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
        let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
        let mut pid: u32 = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        let own_process = pid == unsafe { GetCurrentProcessId() };
        let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|owner| !owner.is_invalid());
        let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
        let child = style & WS_CHILD.0 as isize != 0;
        let tool_window = ex_style & WS_EX_TOOLWINDOW.0 as isize != 0;
        let cloaked = is_cloaked(hwnd);

        // A sweep asks about every top-level window, and most are invisible helpers.
        // Naming the process opens a handle to it, so that is only paid for a window
        // that could still qualify.
        let candidate = visible && !cloaked && !child && !owned && !tool_window && !own_process;
        Some(WindowFacts {
            visible,
            cloaked,
            child,
            owned,
            tool_window,
            own_process,
            class: class_name(hwnd),
            process: if candidate {
                crate::transparency::process_name(value)
            } else {
                String::new()
            },
            layering: layering(hwnd, ex_style),
        })
    }

    /// # Safety
    /// `lparam` is the `Vec<isize>` `top_level` passed in. Must not panic.
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let found = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
        found.push(hwnd.0 as isize);
        true.into()
    }

    pub fn top_level() -> Vec<isize> {
        let mut found: Vec<isize> = Vec::new();
        let _ = unsafe {
            EnumWindows(
                Some(collect),
                LPARAM(&mut found as *mut Vec<isize> as isize),
            )
        };
        found
    }

    pub fn fade(value: isize, alpha: u8) -> bool {
        let hwnd = hwnd(value);
        let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
        let layered = WS_EX_LAYERED.0 as isize;
        if style & layered == 0 {
            unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | layered) };
        }
        // Refused for an elevated window (UIPI). If we added the style and cannot set
        // the alpha, take the style back off rather than leave it half-layered.
        let applied =
            unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA) }.is_ok();
        if !applied && style & layered == 0 {
            unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style) };
        }
        applied
    }

    pub fn restore(value: isize, was: Layering) {
        let hwnd = hwnd(value);
        match was {
            Layering::Alpha(alpha) => {
                let _ = unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA) };
            }
            Layering::None => {
                let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
                let layered = WS_EX_LAYERED.0 as isize;
                if style & layered != 0 {
                    unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style & !layered) };
                }
            }
            // Never faded in the first place.
            Layering::Other => {}
        }
    }

    thread_local! {
        static EVENTS: RefCell<Option<Sender<Message>>> = const { RefCell::new(None) };
    }

    /// **Must not panic** — it is called across FFI. Forwards and returns.
    unsafe extern "system" fn on_event(
        _hook: HWINEVENTHOOK,
        event: u32,
        hwnd: HWND,
        id_object: i32,
        id_child: i32,
        _thread: u32,
        _time: u32,
    ) {
        // The window itself, not a caret, a scrollbar or a child element within it.
        if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 || hwnd.is_invalid() {
            return;
        }
        let message = match event {
            EVENT_OBJECT_DESTROY => Message::Destroyed(hwnd.0 as isize),
            _ => Message::Shown(hwnd.0 as isize),
        };
        let _ = EVENTS.try_with(|cell| {
            if let Ok(sender) = cell.try_borrow() {
                if let Some(sender) = sender.as_ref() {
                    let _ = sender.send(message);
                }
            }
        });
    }

    /// Install the hooks and pump messages until `WM_QUIT`.
    pub fn run_hook(events: Sender<Message>, ready: Sender<Result<u32, String>>) {
        EVENTS.with(|cell| *cell.borrow_mut() = Some(events));

        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        // DESTROY and SHOW are adjacent (0x8001, 0x8002); UNCLOAKED is far off.
        let shown = unsafe {
            SetWinEventHook(
                EVENT_OBJECT_DESTROY,
                EVENT_OBJECT_SHOW,
                None,
                Some(on_event),
                0,
                0,
                flags,
            )
        };
        let uncloaked = unsafe {
            SetWinEventHook(
                EVENT_OBJECT_UNCLOAKED,
                EVENT_OBJECT_UNCLOAKED,
                None,
                Some(on_event),
                0,
                0,
                flags,
            )
        };
        if shown.is_invalid() || uncloaked.is_invalid() {
            for hook in [shown, uncloaked] {
                if !hook.is_invalid() {
                    let _ = unsafe { UnhookWinEvent(hook) };
                }
            }
            EVENTS.with(|cell| *cell.borrow_mut() = None);
            let _ = ready.send(Err(windows::core::Error::from_win32().message()));
            return;
        }

        let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));

        let mut msg = MSG::default();
        loop {
            let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if result.0 <= 0 {
                break;
            }
        }

        let _ = unsafe { UnhookWinEvent(shown) };
        let _ = unsafe { UnhookWinEvent(uncloaked) };
        EVENTS.with(|cell| *cell.borrow_mut() = None);
    }

    pub fn post_quit(thread_id: u32) {
        let _ = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
}

#[cfg(not(windows))]
mod platform {
    use std::sync::mpsc::Sender;

    use super::{Layering, Message, WindowFacts};

    pub fn facts(_hwnd: isize) -> Option<WindowFacts> {
        None
    }
    pub fn top_level() -> Vec<isize> {
        Vec::new()
    }
    pub fn fade(_hwnd: isize, _alpha: u8) -> bool {
        false
    }
    pub fn restore(_hwnd: isize, _was: Layering) {}
    pub fn run_hook(_events: Sender<Message>, ready: Sender<Result<u32, String>>) {
        let _ = ready.send(Err("window hooks are Windows-only".to_string()));
    }
    pub fn post_quit(_thread_id: u32) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(process: &str) -> WindowFacts {
        WindowFacts {
            visible: true,
            cloaked: false,
            child: false,
            owned: false,
            tool_window: false,
            own_process: false,
            class: "Notepad".to_string(),
            process: process.to_string(),
            layering: Layering::None,
        }
    }

    /// A desktop of made-up windows. Records every call, changes nothing real.
    #[derive(Default)]
    struct FakeWindows {
        windows: Mutex<HashMap<isize, WindowFacts>>,
        /// Handles `fade` refuses, like an elevated window.
        refused: Vec<isize>,
        faded: Mutex<Vec<(isize, u8)>>,
        restored: Mutex<Vec<(isize, Layering)>>,
    }

    impl FakeWindows {
        fn with(windows: &[(isize, WindowFacts)]) -> Self {
            Self {
                windows: Mutex::new(windows.iter().cloned().collect()),
                ..Default::default()
            }
        }
        fn faded(&self) -> Vec<(isize, u8)> {
            let mut v = self.faded.lock().unwrap().clone();
            v.sort();
            v
        }
        fn restored(&self) -> Vec<(isize, Layering)> {
            let mut v = self.restored.lock().unwrap().clone();
            v.sort_by_key(|(h, _)| *h);
            v
        }
    }

    impl Windows for FakeWindows {
        fn top_level(&self) -> Vec<isize> {
            let mut all: Vec<isize> = self.windows.lock().unwrap().keys().copied().collect();
            all.sort();
            all
        }
        fn facts(&self, hwnd: isize) -> Option<WindowFacts> {
            self.windows.lock().unwrap().get(&hwnd).cloned()
        }
        fn fade(&self, hwnd: isize, alpha: u8) -> bool {
            if self.refused.contains(&hwnd) {
                return false;
            }
            self.faded.lock().unwrap().push((hwnd, alpha));
            // What Windows would now report about it.
            if let Some(facts) = self.windows.lock().unwrap().get_mut(&hwnd) {
                facts.layering = Layering::Alpha(alpha);
            }
            true
        }
        fn restore(&self, hwnd: isize, was: Layering) {
            self.restored.lock().unwrap().push((hwnd, was));
        }
    }

    fn worker(fake: &Arc<FakeWindows>) -> Worker {
        Worker::new(
            Arc::clone(fake) as Arc<dyn Windows>,
            Arc::new(AtomicUsize::new(0)),
        )
    }

    #[test]
    fn the_config_turns_it_on_and_clamps_the_alpha() {
        assert_eq!(configured_alpha(&json!({})), None);
        assert_eq!(
            configured_alpha(
                &json!({ "transparency": { "all_windows": false, "all_windows_alpha": 100 } })
            ),
            None
        );
        assert_eq!(
            configured_alpha(&json!({ "transparency": { "all_windows": true } })),
            Some(DEFAULT_ALPHA as u8)
        );
        assert_eq!(
            configured_alpha(
                &json!({ "transparency": { "all_windows": true, "all_windows_alpha": 3 } })
            ),
            Some(MIN_ALPHA as u8)
        );
        assert_eq!(
            configured_alpha(
                &json!({ "transparency": { "all_windows": true, "all_windows_alpha": 900 } })
            ),
            Some(255)
        );
    }

    #[test]
    fn only_ordinary_application_windows_qualify() {
        assert!(is_fadeable(&app("notepad.exe")));
        let refused = [
            WindowFacts {
                visible: false,
                ..app("a.exe")
            },
            WindowFacts {
                cloaked: true,
                ..app("a.exe")
            },
            WindowFacts {
                child: true,
                ..app("a.exe")
            },
            WindowFacts {
                owned: true,
                ..app("a.exe")
            },
            WindowFacts {
                tool_window: true,
                ..app("a.exe")
            },
            WindowFacts {
                own_process: true,
                ..app("a.exe")
            },
            WindowFacts {
                class: "Shell_TrayWnd".into(),
                ..app("explorer.exe")
            },
            WindowFacts {
                class: "Progman".into(),
                ..app("explorer.exe")
            },
            WindowFacts {
                layering: Layering::Other,
                ..app("a.exe")
            },
            app(""),
        ];
        for facts in refused {
            assert!(
                !is_fadeable(&facts),
                "{facts:?} should have been left alone"
            );
        }
        // Layered with an alpha of its own is fine: that can be restored exactly.
        assert!(is_fadeable(&WindowFacts {
            layering: Layering::Alpha(230),
            ..app("a.exe")
        }));
    }

    #[test]
    fn a_sweep_fades_every_qualifying_window_and_nothing_else() {
        let fake = Arc::new(FakeWindows::with(&[
            (1, app("notepad.exe")),
            (
                2,
                WindowFacts {
                    tool_window: true,
                    ..app("widget.exe")
                },
            ),
            (3, app("code.exe")),
            (
                4,
                WindowFacts {
                    class: "Shell_TrayWnd".into(),
                    ..app("explorer.exe")
                },
            ),
        ]));
        let mut worker = worker(&fake);

        worker.handle(Message::Sweep(200));

        assert_eq!(fake.faded(), vec![(1, 200), (3, 200)]);
        assert_eq!(worker.count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_new_window_is_faded_when_it_appears() {
        let fake = Arc::new(FakeWindows::with(&[]));
        let mut worker = worker(&fake);
        worker.handle(Message::Sweep(190));

        fake.windows.lock().unwrap().insert(7, app("explorer.exe"));
        worker.handle(Message::Shown(7));

        assert_eq!(fake.faded(), vec![(7, 190)]);
    }

    #[test]
    fn stopping_restores_each_window_the_way_it_was_found() {
        let fake = Arc::new(FakeWindows::with(&[
            (1, app("notepad.exe")),
            (
                2,
                WindowFacts {
                    layering: Layering::Alpha(230),
                    ..app("glass.exe")
                },
            ),
        ]));
        let mut worker = worker(&fake);
        worker.handle(Message::Sweep(128));
        // A second sweep sees our own alpha on the windows; it must not forget what
        // they were before.
        worker.handle(Message::Sweep(100));

        worker.restore_all();

        assert_eq!(
            fake.restored(),
            vec![(1, Layering::None), (2, Layering::Alpha(230))]
        );
        assert_eq!(worker.count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_closed_window_is_forgotten_rather_than_restored() {
        let fake = Arc::new(FakeWindows::with(&[
            (1, app("notepad.exe")),
            (2, app("paint.exe")),
        ]));
        let mut worker = worker(&fake);
        worker.handle(Message::Sweep(128));

        fake.windows.lock().unwrap().remove(&1);
        worker.handle(Message::Destroyed(1));
        worker.restore_all();

        assert_eq!(fake.restored(), vec![(2, Layering::None)]);
    }

    #[test]
    fn a_window_windows_refuses_is_not_tracked() {
        let fake = Arc::new(FakeWindows {
            refused: vec![5],
            ..FakeWindows::with(&[(5, app("taskmgr.exe")), (6, app("notepad.exe"))])
        });
        let mut worker = worker(&fake);
        worker.handle(Message::Sweep(128));
        worker.restore_all();

        assert_eq!(fake.restored(), vec![(6, Layering::None)]);
    }

    /// Dragging the slider to 100% puts windows back rather than leaving them
    /// layered at 255.
    #[test]
    fn fully_opaque_restores_instead_of_fading() {
        let fake = Arc::new(FakeWindows::with(&[(1, app("game.exe"))]));
        let mut worker = worker(&fake);
        worker.handle(Message::Sweep(128));
        assert_eq!(fake.faded(), vec![(1, 128)]);

        worker.handle(Message::Sweep(255));

        assert_eq!(fake.restored(), vec![(1, Layering::None)]);
        assert_eq!(worker.count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn stopping_what_never_started_is_harmless() {
        let watch = WindowWatch::with_windows(Arc::new(FakeWindows::default()));
        watch.stop();
        assert!(!watch.set(None));
        assert_eq!(watch.status()["running"], false);
    }

    /// Installs the **real** WinEvent hooks and takes them down again. The desktop is
    /// faked, so windows that open or close during the test are forwarded to a
    /// worker that changes nothing real.
    #[test]
    #[cfg(windows)]
    fn the_hook_really_installs_and_really_comes_back_out() {
        let fake = Arc::new(FakeWindows::with(&[(1, app("notepad.exe"))]));
        let watch = WindowWatch::with_windows(Arc::clone(&fake) as Arc<dyn Windows>);

        assert!(watch.set(Some(150)), "the hook did not install");
        assert!(watch.is_running());
        assert_eq!(watch.alpha(), Some(150));
        assert!(watch.set(Some(90)));
        assert_eq!(watch.alpha(), Some(90));

        watch.stop();
        assert!(!watch.is_running(), "the hook thread outlived stop()");
        assert!(fake.faded().contains(&(1, 150)));
        assert_eq!(fake.restored(), vec![(1, Layering::None)]);
    }
}
