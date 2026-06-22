// Copyright (c) 2026 Nyx Software, LLC
// SPDX-License-Identifier: Apache-2.0
// Nyx Backup Recovery - https://nyxbackup.com

//! Tauri GUI entry point for the Recovery Tool.

// Hide the Windows console window in release builds (the GUI process has
// no use for a stdio terminal; without this attribute Windows attaches a
// blank cmd.exe alongside the app).  Debug builds keep the console for
// developer logging.  Matches bkp-gui/src/main.rs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use bkp_recover::{commands, session};
use tauri::Manager as _;
#[cfg(target_os = "macos")]
use tauri::{
    Emitter as _,
    menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder},
};

/// WSLg's emulated GPU (the d3d12 / zink Mesa path) emits noisy
/// `libEGL`/`MESA: ZINK` warnings and can leave WebKitGTK as a blank window.
/// When running under WSL, steer GTK/WebKit to the software path BEFORE any
/// GUI init so the launch is clean and renders reliably.  Each var is only set
/// when the user hasn't already chosen one, so an explicit override still wins.
/// No-op off WSL.
fn quiet_wsl_gpu() {
    let is_wsl = std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/version")
            .map(|v| v.to_ascii_lowercase().contains("microsoft"))
            .unwrap_or(false);
    if !is_wsl {
        return;
    }
    for (k, v) in [
        ("WEBKIT_DISABLE_DMABUF_RENDERER", "1"),
        ("LIBGL_ALWAYS_SOFTWARE", "1"),
    ] {
        if std::env::var_os(k).is_none() {
            // SAFETY: called first thing in main(), before any threads are
            // spawned or any GUI/Mesa code reads the environment.
            unsafe { std::env::set_var(k, v) };
        }
    }
}

fn main() {
    // Silence WSLg's emulated-GPU warnings / blank-window risk before the
    // webview initializes.
    quiet_wsl_gpu();

    // Best-effort log init.  We don't fail the GUI if the log dir is
    // unwriteable - a recovery user with a read-only home should still get
    // a working UI.
    let _ = init_logging();

    // tauri::Builder::on_menu_event is the documented way to register
    // a global menu-event handler in Tauri 2.  Registering it at App
    // setup time (via `app.on_menu_event(...)`) is supported in some
    // 2.x revisions but has been observed to silently drop events on
    // arm64 - the Builder-level registration goes through a different
    // code path inside the Wry runtime and reliably fires.  We log
    // every fire at INFO so the recovery log shows the menu wiring
    // is reaching the renderer; the actual modal-toggle work happens
    // in App.svelte's Tauri-event listeners.
    let builder = tauri::Builder::default();

    #[cfg(target_os = "macos")]
    let builder = builder.on_menu_event(|app, event| {
        let id = event.id().as_ref().to_string();
        tracing::info!(target: "bkp_recover::menu",
            "menu event: id={}", id);
        match id.as_str() {
            "about" => {
                let _ = app.emit("menu://show-about", ());
            }
            "preferences" => {
                let _ = app.emit("menu://show-settings", ());
            }
            _ => {}
        }
    });

    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .manage(session::new_shared())
        .invoke_handler(tauri::generate_handler![
            commands::rec_test_connection,
            commands::rec_connect,
            commands::rec_disconnect,
            commands::rec_unlock,
            commands::rec_list_snapshots,
            commands::rec_list_snapshot_files,
            commands::rec_start_restore,
            commands::rec_get_progress,
            commands::rec_pause_restore,
            commands::rec_resume_restore,
            commands::rec_cancel_restore,
            commands::rec_open_folder,
            commands::rec_local_desktop,
            commands::rec_get_free_space,
            commands::rec_list_checkpoints,
            commands::rec_discard_checkpoint,
            commands::rec_get_recent,
            commands::rec_remove_recent,
            commands::rec_get_settings,
            commands::rec_save_settings,
            commands::rec_read_key_file,
            commands::rec_dropbox_oauth,
            commands::rec_google_oauth,
            commands::rec_dropbox_oauth_url,
            commands::rec_dropbox_oauth_exchange,
            commands::rec_google_oauth_url,
            commands::rec_google_oauth_exchange,
            commands::rec_onedrive_oauth,
            commands::rec_onedrive_oauth_url,
            commands::rec_onedrive_oauth_exchange,
            commands::rec_app_info,
        ])
        .setup(|app| {
            if let Some(win) = app.get_webview_window("main") {
                // Same Linux double-frame rationale as the main GUI: suppress
                // native chrome everywhere; the custom TitleBar in the Svelte
                // layer is the only frame. See REQUIREMENTS.md.
                #[cfg(target_os = "linux")]
                {
                    let _ = win.set_decorations(false);
                }
                let _ = win.show();
            }

            // macOS: build a native NSMenu instead of the in-window ⚙ + ?
            // buttons.  About goes at the top of the app menu (next to the
            // "Nyx Backup Recovery" submenu title), Preferences gets ⌘,
            // (Mac standard), and we include the Hide / Hide Others / Show
            // All / Quit predefined items so the app menu doesn't look
            // half-finished compared to other Mac apps.  Selecting About or
            // Preferences emits a Tauri event the Svelte layer listens for
            // (`menu://show-about`, `menu://show-settings`); the modals
            // themselves still live in the renderer.
            #[cfg(target_os = "macos")]
            {
                let handle = app.handle();
                // Resolve the user's locale once and translate every
                // user-visible menu string through bkp_recover::menu_i18n.
                // "auto" routes to `defaults read -g AppleLocale`; any
                // unknown locale falls through to English silently.
                // Predefined items (Hide / Cut / Quit / etc.) are pulled
                // by Tauri straight from the system localisation, so they
                // already match the user's macOS language without our
                // table.
                let cfg_locale = bkp_recover::settings::Settings::load().locale;
                let loc = bkp_recover::menu_i18n::resolve_locale(&cfg_locale);
                let t = |k: &str| bkp_recover::menu_i18n::lookup(&loc, k);

                // App menu: About, Preferences, Hide/Quit cluster.
                let about =
                    MenuItemBuilder::with_id("about", t("gui.recover.menu.about")).build(handle)?;
                let prefs =
                    MenuItemBuilder::with_id("preferences", t("gui.recover.menu.preferences"))
                        .accelerator("CmdOrCtrl+,")
                        .build(handle)?;
                let app_sep1 = PredefinedMenuItem::separator(handle)?;
                let app_sep2 = PredefinedMenuItem::separator(handle)?;
                let app_sep3 = PredefinedMenuItem::separator(handle)?;
                let hide = PredefinedMenuItem::hide(handle, None)?;
                let hide_others = PredefinedMenuItem::hide_others(handle, None)?;
                let show_all = PredefinedMenuItem::show_all(handle, None)?;
                let quit = PredefinedMenuItem::quit(handle, None)?;
                let app_submenu = SubmenuBuilder::new(handle, t("gui.recover.menu.app_title"))
                    .items(&[
                        &about,
                        &app_sep1,
                        &prefs,
                        &app_sep2,
                        &hide,
                        &hide_others,
                        &show_all,
                        &app_sep3,
                        &quit,
                    ])
                    .build()?;

                // Edit menu: standard Mac text-editing commands.  Tauri 2's
                // PredefinedMenuItem wires each one to the first-responder
                // chain so Cmd+C/V/X/A/Z/Shift+Z work in the webview's
                // input fields without any handler code on our side.
                let undo = PredefinedMenuItem::undo(handle, None)?;
                let redo = PredefinedMenuItem::redo(handle, None)?;
                let edit_sep = PredefinedMenuItem::separator(handle)?;
                let cut = PredefinedMenuItem::cut(handle, None)?;
                let copy = PredefinedMenuItem::copy(handle, None)?;
                let paste = PredefinedMenuItem::paste(handle, None)?;
                let select_all = PredefinedMenuItem::select_all(handle, None)?;
                let edit_submenu = SubmenuBuilder::new(handle, t("gui.recover.menu.edit_menu"))
                    .items(&[&undo, &redo, &edit_sep, &cut, &copy, &paste, &select_all])
                    .build()?;

                let menu = MenuBuilder::new(handle)
                    .items(&[&app_submenu, &edit_submenu])
                    .build()?;
                app.set_menu(menu)?;
                tracing::info!(target: "bkp_recover::menu",
                    "macOS NSMenu attached: 'Nyx Backup Recovery' + 'Edit' submenus.");

                // Menu-event handler is registered at the Builder level
                // (above) - registering it here on the App at setup-time
                // has been observed to silently drop events on some Tauri
                // 2.x patches.
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Failed to start Nyx Backup Recovery");
}

/// Initialise tracing to BOTH stderr (visible under `cargo run` / a console
/// build) AND a rolling file at `<data_root>/logs/recovery.log`.  GUI builds
/// on Windows have no visible stderr (windows_subsystem = "windows" detaches
/// the console), so the file sink is the only way to diagnose problems
/// post-mortem.  Path printed via `println!` is captured by Tauri's stdout
/// only when run from a terminal; on shipping installs the path is
/// deterministic per-platform - see crate::paths::log_dir().
fn init_logging() -> Result<(), Box<dyn std::error::Error>> {
    use tracing_subscriber::fmt;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let settings = bkp_recover::settings::Settings::load();
    let level = match settings.log_level.as_str() {
        "error" => tracing::Level::ERROR,
        "warn" => tracing::Level::WARN,
        "info" => tracing::Level::INFO,
        "debug" => tracing::Level::DEBUG,
        "trace" => tracing::Level::TRACE,
        _ => tracing::Level::INFO,
    };

    let log_dir = bkp_recover::paths::log_dir();
    let _ = std::fs::create_dir_all(&log_dir);

    // Same SizeRollingAppender (6 MiB rotate, keep 2 .gz archives) the
    // main GUI + daemon use, so the recovery log doesn't grow unbounded
    // and post-mortem inspection on shipping installs is identical
    // across all three binaries.
    let log_path = log_dir.join("recovery.log");
    let file_appender =
        bkp_log::SizeRollingAppender::new(&log_dir, "recovery.log", 6 * 1024 * 1024, 2).ok();

    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(false);

    let registry = tracing_subscriber::registry()
        .with(tracing::level_filters::LevelFilter::from_level(level))
        .with(stderr_layer);

    if let Some(appender) = file_appender {
        let file_layer = fmt::layer()
            .with_writer(std::sync::Mutex::new(appender))
            .with_target(true)
            .with_ansi(false);
        registry.with(file_layer).try_init().ok();
        tracing::info!("Recovery Tool log file: {}", log_path.display());
    } else {
        registry.try_init().ok();
        tracing::warn!(
            "Could not open log file at {} - logging to stderr only.",
            log_path.display()
        );
    }

    Ok(())
}
