//! The all-windows fade against the real desktop.
//!
//! **Ignored by default** — this fades every window on the machine running it for
//! the length of the test, and opens and closes Notepad. Run it deliberately:
//!
//! ```text
//! cargo test -p wallpaper-core --test window_opacity -- --ignored --nocapture
//! ```
//!
//! What the unit tests cannot show is that Windows really delivers the hook's events
//! and really accepts the fade: they run against a fake desktop by design.

#![cfg(windows)]

use std::process::Command;
use std::time::{Duration, Instant};

use wallpaper_core::transparency;
use wallpaper_core::window_watch::{Layering, RealWindows, WindowWatch, Windows};

/// Notepad's window, once it exists. On Windows 11 it is a packaged app, so the
/// process that shows the window is not the one `Command` started — find it by name.
fn notepad_window(timeout: Duration) -> Option<isize> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(window) = transparency::visible_windows()
            .into_iter()
            .find(|w| w.process.eq_ignore_ascii_case("notepad.exe"))
        {
            return Some(window.hwnd);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn wait_for(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
#[ignore = "fades every window on this desktop and opens Notepad"]
fn a_window_opened_after_the_fade_is_faded_and_restored_on_stop() {
    let desktop = RealWindows::new();
    let watch = WindowWatch::new();
    assert!(watch.set(Some(170)), "the window hook did not install");

    let mut launcher = Command::new("notepad.exe")
        .spawn()
        .expect("could not start Notepad");
    let hwnd = notepad_window(Duration::from_secs(10)).expect("Notepad never showed a window");

    // The global value wins over any per-app opacity Notepad may have saved.
    let faded = wait_for(Duration::from_secs(5), || {
        matches!(
            desktop.facts(hwnd).map(|f| f.layering),
            Some(Layering::Alpha(170))
        )
    });

    watch.stop();
    let restored = desktop.facts(hwnd).map(|f| f.layering);
    let _ = Command::new("taskkill")
        .args(["/IM", "notepad.exe", "/F"])
        .output();
    let _ = launcher.wait();

    assert!(faded, "the new window was not faded to 170");
    assert_eq!(
        restored,
        Some(Layering::None),
        "stop() did not restore the window"
    );
}
