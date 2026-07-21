use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
#[cfg(not(target_os = "macos"))]
use tauri::window::{Color, WindowBuilder};
use tauri::{AppHandle, Emitter, Manager, Position, Size, WebviewWindow};

// Default time between breaks. The live value lives in `AppState.interval_secs`
// (settable from the frontend); this is only the startup default.
#[cfg(debug_assertions)]
const TIMER_INTERVAL_SECS: u64 = 15; // 15 seconds in dev for testing
#[cfg(not(debug_assertions))]
const TIMER_INTERVAL_SECS: u64 = 1800; // 30 minutes in production

// Floor for a user-chosen interval, so Settings can't set breaks so frequently
// they're unusable. Lower in dev to keep testing fast.
#[cfg(debug_assertions)]
const MIN_INTERVAL_SECS: u64 = 5;
#[cfg(not(debug_assertions))]
const MIN_INTERVAL_SECS: u64 = 60;

// Cumulative snooze allowed since the last completed stretch. Once the user has
// deferred breaks for this long without moving, snooze is refused and the break
// becomes mandatory. Shorter in dev so the cap is reachable during testing.
#[cfg(debug_assertions)]
const SNOOZE_CAP_SECS: u64 = 30 * 60;
#[cfg(not(debug_assertions))]
const SNOOZE_CAP_SECS: u64 = 2 * 60 * 60; // 2 hours

/// Countdown snapshot handed to the frontend so its idle ring matches the real
/// (backend) timer instead of running an independent clock.
#[derive(Clone, serde::Serialize)]
struct Countdown {
    remaining: u64, // seconds until the next break
    cycle: u64,     // length of the current wait cycle (interval or snooze)
    paused: bool,
}

/// Managed state to track whether screen is currently locked
struct AppState {
    is_locked: AtomicBool,
    timer_paused: AtomicBool,
    /// When > 0, the next break fires after this many seconds instead of the
    /// full interval (set by a snooze). Consumed once, then reset to 0.
    snooze_secs: AtomicU64,
    /// Live time between breaks, in seconds. Seeded from `TIMER_INTERVAL_SECS`
    /// and updated by `set_timer_interval` when the user changes it in Settings.
    interval_secs: AtomicU64,
    /// Seconds until the next break, republished by the timer each tick.
    remaining_secs: AtomicU64,
    /// Length of the current wait cycle (interval, or a snooze if pending).
    cycle_secs: AtomicU64,
    /// Cumulative snooze taken since the last completed stretch. Reset on
    /// unlock_screen (a real stretch), grown by snooze_break, capped at
    /// SNOOZE_CAP_SECS.
    snooze_used_secs: AtomicU64,
}

const BLOCKER_LABEL_PREFIX: &str = "monitor-blocker-";

#[cfg(target_os = "macos")]
static MACOS_BLOCKERS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

#[cfg(target_os = "macos")]
fn close_macos_blockers_on_main_thread() {
    use objc2::rc::Retained;
    use objc2_app_kit::NSPanel;

    let handles = MACOS_BLOCKERS
        .lock()
        .map(|mut blockers| std::mem::take(&mut *blockers))
        .unwrap_or_default();

    for address in handles {
        // Each pointer came from Retained::into_raw when the window was made.
        if let Some(window) = unsafe { Retained::<NSPanel>::from_raw(address as *mut NSPanel) } {
            window.orderOut(None);
            window.close();
        }
    }
}

/// Tauri's `always_on_top` maps to macOS's floating-window level. That is not
/// high enough to stay above every application (and can disappear behind a
/// full-screen Space), so apply the stronger AppKit policy on macOS.
#[cfg(target_os = "macos")]
fn configure_macos_blocker(ns_window: *mut std::ffi::c_void) {
    use objc2_app_kit::{NSScreenSaverWindowLevel, NSWindow, NSWindowCollectionBehavior};

    if ns_window.is_null() {
        return;
    }

    let collection_behavior = NSWindowCollectionBehavior::CanJoinAllSpaces
        | NSWindowCollectionBehavior::Stationary
        | NSWindowCollectionBehavior::IgnoresCycle
        | NSWindowCollectionBehavior::FullScreenAuxiliary;

    // AppKit window mutations must run on the main thread. Callers guarantee
    // that with Tauri's run_on_main_thread before passing the native pointer.
    unsafe {
        let window = &*ns_window.cast::<NSWindow>();
        window.setLevel(NSScreenSaverWindowLevel);
        window.setCollectionBehavior(collection_behavior);
        window.setHidesOnDeactivate(false);
        window.setIgnoresMouseEvents(false);
        window.orderFrontRegardless();
    }
}

#[cfg(target_os = "macos")]
fn elevate_exercise_window(window: &WebviewWindow) {
    if let Ok(ns_window) = window.ns_window() {
        let address = ns_window as usize;
        let _ = window.run_on_main_thread(move || {
            configure_macos_blocker(address as *mut std::ffi::c_void);
        });
    }
}

#[cfg(target_os = "macos")]
fn restore_exercise_window(window: &WebviewWindow) {
    use objc2_app_kit::{NSNormalWindowLevel, NSWindow, NSWindowCollectionBehavior};

    if let Ok(ns_window) = window.ns_window() {
        let address = ns_window as usize;
        let _ = window.run_on_main_thread(move || {
            let ns_window = address as *mut std::ffi::c_void;
            if ns_window.is_null() {
                return;
            }
            unsafe {
                let native_window = &*ns_window.cast::<NSWindow>();
                native_window.setLevel(NSNormalWindowLevel);
                native_window.setCollectionBehavior(NSWindowCollectionBehavior::Default);
            }
        });
    }
}

/// Remove the lightweight cover windows used on secondary monitors.
fn remove_monitor_blockers(app: &AppHandle) {
    for (label, window) in app.windows() {
        if label.starts_with(BLOCKER_LABEL_PREFIX) {
            let _ = window.destroy();
        }
    }

    #[cfg(target_os = "macos")]
    {
        let _ = app.run_on_main_thread(close_macos_blockers_on_main_thread);
    }
}

/// Cover every monitor except the one hosting the main exercise UI. Each
/// blocker is its own undecorated native window so mixed monitor positions, sizes,
/// and scale factors are handled by the OS instead of one giant virtual window.
#[cfg(not(target_os = "macos"))]
fn create_monitor_blockers(app: &AppHandle, main: &WebviewWindow) -> Result<(), String> {
    remove_monitor_blockers(app);

    let monitors = main.available_monitors().map_err(|e| e.to_string())?;
    let active_monitor = main.current_monitor().map_err(|e| e.to_string())?;
    for (index, monitor) in monitors.iter().enumerate() {
        let is_active = active_monitor.as_ref().is_some_and(|active| {
            active.position() == monitor.position() && active.size() == monitor.size()
        });
        if is_active {
            continue;
        }

        let label = format!("{BLOCKER_LABEL_PREFIX}{index}");
        let target_scale = monitor.scale_factor();
        let logical_x = monitor.position().x as f64 / target_scale;
        let logical_y = monitor.position().y as f64 / target_scale;
        let logical_width = monitor.size().width as f64 / target_scale;
        let logical_height = monitor.size().height as f64 / target_scale;
        let blocker = WindowBuilder::new(app, label)
            .title("Moov — Break in progress")
            // The initial screen determines the macOS Space assignment. A
            // window created on the primary display and moved later can remain
            // invisible on a secondary display's active Space.
            .position(logical_x, logical_y)
            .inner_size(logical_width, logical_height)
            .visible(false)
            .focused(false)
            .focusable(false)
            .decorations(false)
            .resizable(false)
            .maximizable(false)
            .minimizable(false)
            .closable(false)
            .always_on_top(true)
            .visible_on_all_workspaces(true)
            .skip_taskbar(true)
            .background_color(Color(9, 10, 16, 255))
            .build()
            .map_err(|e| format!("monitor {index}: could not create blocker: {e}"))?;

        // Reapply these after creation as macOS may initially construct a
        // hidden native window with standard title-bar/Space behavior.
        let _ = blocker.set_decorations(false);
        let _ = blocker.set_visible_on_all_workspaces(true);
        blocker.show().map_err(|e| e.to_string())?;
        let _ = blocker.set_always_on_top(true);
        let _ = blocker.set_visible_on_all_workspaces(true);
        eprintln!(
            "[moov] covered monitor {index}: {:?} at {:?}, size {:?}",
            monitor.name(),
            monitor.position(),
            monitor.size()
        );
    }

    // Creating non-focused secondary windows must not steal keyboard focus
    // from the exercise window.
    let _ = main.set_focus();
    Ok(())
}

/// macOS assigns each window to a Space when it is constructed. Tauri creates
/// native windows on the primary display and moves them afterward, which leaves
/// secondary-display covers attached to an inactive Space. Construct these
/// simple color windows directly against each target NSScreen instead.
#[cfg(target_os = "macos")]
fn create_monitor_blockers(_app: &AppHandle, main: &WebviewWindow) -> Result<(), String> {
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSBackingStoreType, NSColor, NSPanel, NSScreen, NSScreenSaverWindowLevel, NSWindow,
        NSWindowCollectionBehavior, NSWindowStyleMask,
    };
    let main_window = main.ns_window().map_err(|e| e.to_string())? as usize;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);

    main.run_on_main_thread(move || {
        close_macos_blockers_on_main_thread();

        let result = (|| -> Result<Vec<usize>, String> {
            let mtm = MainThreadMarker::new()
                .ok_or_else(|| "macOS blocker creation was not on the main thread".to_string())?;
            let screens = NSScreen::screens(mtm);
            let native_main = unsafe { &*(main_window as *mut NSWindow) };
            let active_frame = native_main.screen().map(|screen| screen.frame());
            let behavior = NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle
                | NSWindowCollectionBehavior::FullScreenAuxiliary;
            let mut blocker_handles = Vec::new();

            for index in 0..screens.count() {
                let screen = screens.objectAtIndex(index);
                let screen_frame = screen.frame();
                if active_frame.is_some_and(|frame| frame == screen_frame) {
                    continue;
                }

                let blocker = NSPanel::initWithContentRect_styleMask_backing_defer_screen(
                    NSPanel::alloc(mtm),
                    screen_frame,
                    NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                    NSBackingStoreType::Buffered,
                    false,
                    Some(&screen),
                );
                unsafe { blocker.setReleasedWhenClosed(false) };
                // Constructing with the target's global frame attaches the
                // window to that display's active Space. Reapplying the same
                // frame after construction removes AppKit's creation-time
                // origin offset while preserving that Space assignment.
                blocker.setFrame_display(screen_frame, true);
                blocker.setBackgroundColor(Some(&NSColor::blackColor()));
                blocker.setOpaque(true);
                blocker.setHasShadow(false);
                blocker.setMovable(false);
                blocker.setCanHide(false);
                blocker.setFloatingPanel(true);
                blocker.setBecomesKeyOnlyIfNeeded(true);
                blocker.setWorksWhenModal(true);
                blocker.setExcludedFromWindowsMenu(true);
                blocker.setLevel(NSScreenSaverWindowLevel);
                blocker.setCollectionBehavior(behavior);
                blocker.setHidesOnDeactivate(false);
                blocker.setIgnoresMouseEvents(false);
                blocker.orderFrontRegardless();

                eprintln!(
                    "[moov] native blocker on screen {index}: {:?}",
                    screen_frame
                );
                blocker_handles.push(Retained::into_raw(blocker) as usize);
            }

            Ok(blocker_handles)
        })();

        match result {
            Ok(handles) => {
                if let Ok(mut blockers) = MACOS_BLOCKERS.lock() {
                    *blockers = handles;
                    let _ = sender.send(Ok(()));
                } else {
                    let _ = sender.send(Err("macOS blocker registry is unavailable".to_string()));
                }
            }
            Err(error) => {
                let _ = sender.send(Err(error));
            }
        }
    })
    .map_err(|e| e.to_string())?;

    receiver.recv().map_err(|e| e.to_string())?
}

/// Put the desktop into "break blocker" mode. We deliberately avoid native
/// macOS fullscreen because it creates a separate Space and can hide the
/// secondary blocker windows. A borderless monitor-sized window gives the same
/// visual result while every display remains in the current desktop Space.
fn engage_lock(app: &AppHandle, window: &WebviewWindow) {
    let active_monitor = window.current_monitor().ok().flatten();
    let _ = window.set_fullscreen(false);
    let _ = window.set_decorations(false);
    if let Some(monitor) = active_monitor {
        let scale = monitor.scale_factor();
        // Tauri's macOS position conversion uses the window's current scale.
        // Monitor origins are already in the global desktop coordinate space,
        // so compensate before passing a Physical position.
        let position = tauri::PhysicalPosition::new(
            (monitor.position().x as f64 * scale).round() as i32,
            (monitor.position().y as f64 * scale).round() as i32,
        );
        let _ = window.set_position(Position::Physical(position));
        let _ = window.set_size(Size::Physical(*monitor.size()));
    }
    let _ = window.set_always_on_top(true);
    let _ = window.set_visible_on_all_workspaces(true);
    let _ = window.set_closable(false);
    let _ = window.set_minimizable(false);
    let _ = window.show();
    #[cfg(target_os = "macos")]
    elevate_exercise_window(window);
    let _ = window.set_focus();

    // Create secondary covers after the exercise window is focused. On macOS,
    // activating the main window after auxiliary windows are created can move
    // those windows back to the main display's Space.
    if let Err(error) = create_monitor_blockers(app, window) {
        eprintln!("failed to cover secondary monitors: {error}");
    }
}

/// Restore the desktop to its normal, dismissable state.
fn release_lock(app: &AppHandle, window: &WebviewWindow) {
    remove_monitor_blockers(app);
    let _ = window.set_fullscreen(false);
    let _ = window.set_decorations(true);
    let _ = window.set_closable(true);
    let _ = window.set_minimizable(true);
    let _ = window.set_always_on_top(false);
    let _ = window.set_visible_on_all_workspaces(false);
    #[cfg(target_os = "macos")]
    restore_exercise_window(window);
    let _ = window.set_size(Size::Logical(tauri::LogicalSize::new(460.0, 640.0)));
    let _ = window.center();
}

/// Called from the frontend when face detection confirms the user has stretched,
/// or via the manual "I've stretched" escape hatch.
#[tauri::command]
fn unlock_screen(app: AppHandle, state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
    state.is_locked.store(false, Ordering::SeqCst);
    // Completing a real stretch clears the snooze debt.
    state.snooze_used_secs.store(0, Ordering::SeqCst);
    app.emit("screen-unlocked", ()).map_err(|e| e.to_string())?;
    if let Some(window) = app.get_webview_window("main") {
        release_lock(&app, &window);
    }
    Ok(())
}

/// Snooze the current break: unlock now, but schedule the next break to fire
/// after `secs` instead of the full interval. This is the escape valve for
/// meetings / incidents / "I genuinely can't stand right now".
///
/// Snooze is capped: the requested duration is clamped to the remaining budget
/// (SNOOZE_CAP_SECS minus what's already been snoozed since the last stretch),
/// and the call is refused once that budget is exhausted. Returns the seconds
/// actually snoozed.
#[tauri::command]
fn snooze_break(
    secs: u64,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<u64, String> {
    let used = state.snooze_used_secs.load(Ordering::SeqCst);
    let budget = SNOOZE_CAP_SECS.saturating_sub(used);
    let eff = secs.min(budget);
    if eff == 0 {
        return Err("snooze budget exhausted".into());
    }
    state.snooze_used_secs.store(used + eff, Ordering::SeqCst);
    state.snooze_secs.store(eff, Ordering::SeqCst);
    state.is_locked.store(false, Ordering::SeqCst);
    app.emit("screen-unlocked", ()).map_err(|e| e.to_string())?;
    if let Some(window) = app.get_webview_window("main") {
        release_lock(&app, &window);
    }
    Ok(eff)
}

/// Called by the timer (or "Stretch now") to lock the screen.
#[tauri::command]
fn lock_screen(app: AppHandle, state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
    if state.is_locked.load(Ordering::SeqCst) {
        return Ok(()); // Already locked
    }
    state.is_locked.store(true, Ordering::SeqCst);
    if let Some(window) = app.get_webview_window("main") {
        engage_lock(&app, &window);
    }
    app.emit("screen-locked", ()).map_err(|e| e.to_string())?;
    Ok(())
}

/// Returns whether the screen is currently locked
#[tauri::command]
fn is_locked(state: tauri::State<'_, Arc<AppState>>) -> bool {
    state.is_locked.load(Ordering::SeqCst)
}

/// Pause/resume the timer (from the tray menu)
#[tauri::command]
fn set_timer_paused(paused: bool, state: tauri::State<'_, Arc<AppState>>) {
    state.timer_paused.store(paused, Ordering::SeqCst);
}

/// Returns the current timer interval in seconds.
#[tauri::command]
fn get_timer_interval(state: tauri::State<'_, Arc<AppState>>) -> u64 {
    state.interval_secs.load(Ordering::SeqCst)
}

/// Set the time between breaks (from Settings). Clamped to `MIN_INTERVAL_SECS`.
/// Takes effect on the current countdown — no restart needed.
#[tauri::command]
fn set_timer_interval(secs: u64, state: tauri::State<'_, Arc<AppState>>) -> u64 {
    let clamped = secs.max(MIN_INTERVAL_SECS);
    state.interval_secs.store(clamped, Ordering::SeqCst);
    clamped
}

/// Authoritative countdown for the idle screen — reflects the real timer.
#[tauri::command]
fn get_countdown(state: tauri::State<'_, Arc<AppState>>) -> Countdown {
    Countdown {
        remaining: state.remaining_secs.load(Ordering::SeqCst),
        cycle: state.cycle_secs.load(Ordering::SeqCst),
        paused: state.timer_paused.load(Ordering::SeqCst),
    }
}

/// Seconds of snooze the user still has before the cap forces a break.
#[tauri::command]
fn get_snooze_budget(state: tauri::State<'_, Arc<AppState>>) -> u64 {
    SNOOZE_CAP_SECS.saturating_sub(state.snooze_used_secs.load(Ordering::SeqCst))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let state = Arc::new(AppState {
        is_locked: AtomicBool::new(false),
        timer_paused: AtomicBool::new(false),
        snooze_secs: AtomicU64::new(0),
        interval_secs: AtomicU64::new(TIMER_INTERVAL_SECS),
        remaining_secs: AtomicU64::new(TIMER_INTERVAL_SECS),
        cycle_secs: AtomicU64::new(TIMER_INTERVAL_SECS),
        snooze_used_secs: AtomicU64::new(0),
    });

    let timer_state = Arc::clone(&state);
    let tray_state = Arc::clone(&state);

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            lock_screen,
            unlock_screen,
            snooze_break,
            is_locked,
            set_timer_paused,
            get_timer_interval,
            set_timer_interval,
            get_countdown,
            get_snooze_budget,
        ])
        .setup(move |app| {
            // In dev, pop open the WebView DevTools so frontend console.log()
            // output (the `[moov]` debug logs) is visible — those go to the
            // WebView console, NOT the `tauri dev` terminal (which is Rust only).
            #[cfg(debug_assertions)]
            if let Some(window) = app.get_webview_window("main") {
                window.open_devtools();
            }

            // ---- Menu-bar (tray) icon: pause without quitting ----
            let pause_i = MenuItemBuilder::with_id("pause", "Pause Moov").build(app)?;
            let stretch_i = MenuItemBuilder::with_id("stretch", "Stretch now").build(app)?;
            let quit_i = MenuItemBuilder::with_id("quit", "Quit Moov").build(app)?;
            let menu = MenuBuilder::new(app)
                .items(&[&pause_i, &stretch_i, &quit_i])
                .build()?;

            let pause_item = pause_i.clone();
            let menu_state = Arc::clone(&tray_state);
            let _tray = TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Moov — stretch reminder")
                .menu(&menu)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "quit" => app.exit(0),
                    "stretch" => {
                        if !menu_state.is_locked.load(Ordering::SeqCst) {
                            menu_state.is_locked.store(true, Ordering::SeqCst);
                            // Native macOS window construction synchronizes
                            // with the UI thread, so do it outside this tray
                            // callback (which itself runs on that thread).
                            let app = app.clone();
                            std::thread::spawn(move || {
                                if let Some(window) = app.get_webview_window("main") {
                                    engage_lock(&app, &window);
                                }
                                let _ = app.emit("screen-locked", ());
                            });
                        }
                    }
                    "pause" => {
                        let now_paused = !menu_state.timer_paused.load(Ordering::SeqCst);
                        menu_state.timer_paused.store(now_paused, Ordering::SeqCst);
                        let _ = pause_item.set_text(if now_paused {
                            "Resume Moov"
                        } else {
                            "Pause Moov"
                        });
                    }
                    _ => {}
                })
                .build(app)?;

            // ---- Break timer ----
            let app_handle = app.handle().clone();
            let state = timer_state;

            std::thread::spawn(move || {
                // Poll once a second and count elapsed idle time toward the
                // target. Polling (vs. one long sleep) lets an interval change
                // from Settings take effect on the current countdown, and lets
                // pause freeze it, without waiting out a stale duration.
                let mut elapsed: u64 = 0;

                loop {
                    std::thread::sleep(Duration::from_secs(1));

                    // A break is on screen — wait it out, then start fresh.
                    if state.is_locked.load(Ordering::SeqCst) {
                        elapsed = 0;
                        continue;
                    }
                    // Frozen while paused (tray toggle).
                    if state.timer_paused.load(Ordering::SeqCst) {
                        continue;
                    }

                    elapsed += 1;

                    // A pending snooze shortens just this cycle; otherwise use
                    // the live, user-configurable interval.
                    let snooze = state.snooze_secs.load(Ordering::SeqCst);
                    let target = if snooze > 0 {
                        snooze
                    } else {
                        state.interval_secs.load(Ordering::SeqCst)
                    };

                    // Publish the countdown so the idle screen can mirror it.
                    state.cycle_secs.store(target, Ordering::SeqCst);
                    state
                        .remaining_secs
                        .store(target.saturating_sub(elapsed), Ordering::SeqCst);

                    if elapsed >= target {
                        state.snooze_secs.store(0, Ordering::SeqCst); // consume the snooze
                        elapsed = 0;
                        state.is_locked.store(true, Ordering::SeqCst);
                        if let Some(window) = app_handle.get_webview_window("main") {
                            engage_lock(&app_handle, &window);
                        }
                        let _ = app_handle.emit("screen-locked", ());
                    }
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
