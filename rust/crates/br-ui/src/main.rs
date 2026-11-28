// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

#![windows_subsystem = "windows"]

mod args;
mod motion;
mod platform;
mod theme;
mod views;

use gpui::{
    App, AppContext as _, Bounds, QuitMode, WindowBackgroundAppearance, WindowBounds,
    WindowDecorations, WindowOptions, px, size,
};
use gpui_component::{Root, TitleBar};

use crate::args::{LaunchArgs, resolve_demo_assets_dir};
use crate::platform::events::{AppSignal, EventListener, acquire_single_instance};
use crate::views::launcher::LauncherView;
use crate::views::shell::MainShell;

fn main() {
    let args = LaunchArgs::parse_from_env();
    if !args.mock {
        std::process::exit(2);
    }

    let _instance_guard = match acquire_single_instance() {
        Some(guard) => guard,
        None => std::process::exit(0),
    };

    let (sig_tx, sig_rx) = async_channel::bounded::<AppSignal>(8);
    let _event_listener = EventListener::start(sig_tx).ok();

    gpui_platform::application()
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            theme::apply_system_theme(cx);
            let _theme_watcher = theme::start_theme_watcher(cx);
            motion::init(args.motion.into(), cx);

            let cursor_display = platform::display_for_cursor(cx);
            let assets_dir = resolve_demo_assets_dir(args.assets_dir.as_ref());
            let capturable = args.mock_capturable;

            // 1. Main window (1400x900, min 640x480, MicaBackdrop, transparent title bar).
            let show_main = !args.hidden && args.mock_query.is_none();
            let main_bounds = Bounds::centered(cursor_display, size(px(1400.0), px(900.0)), cx);
            let mut main_opts = TitleBar::window_options();
            main_opts.window_bounds = Some(WindowBounds::Windowed(main_bounds));
            main_opts.window_min_size = Some(size(px(640.0), px(480.0)));
            main_opts.window_background = WindowBackgroundAppearance::MicaBackdrop;
            main_opts.display_id = cursor_display;
            main_opts.show = show_main;
            main_opts.focus = show_main;
            if let Some(ref mut tb) = main_opts.titlebar {
                tb.title = Some("BetterRecall".into());
            }

            let args_for_main = args.clone();
            let _main_window = cx.open_window(main_opts, move |window, cx| {
                if show_main
                    && let Some(hwnd) = platform::hwnd_from_window(window)
                {
                    platform::apply_display_affinity(hwnd, capturable);
                }
                window.on_window_should_close(cx, |_window, cx| {
                    cx.quit();
                    true
                });
                let shell = cx.new(|cx| MainShell::new(&args_for_main, window, cx));
                cx.new(|cx| Root::new(shell, window, cx))
            });

            // 2. Launcher window (warm, borderless resizable ~1200x720, hidden unless --mock-query).
            let show_launcher = args.mock_query.is_some();
            let launcher_bounds =
                Bounds::centered(cursor_display, size(px(1200.0), px(720.0)), cx);
            let launcher_opts = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(launcher_bounds)),
                window_min_size: Some(size(px(720.0), px(480.0))),
                titlebar: None,
                window_decorations: Some(WindowDecorations::Client),
                window_background: WindowBackgroundAppearance::Opaque,
                display_id: cursor_display,
                is_resizable: true,
                show: show_launcher,
                focus: show_launcher,
                ..Default::default()
            };

            let initial_query = args.mock_query.clone();
            let launcher_view_slot: std::rc::Rc<
                std::cell::RefCell<Option<gpui::Entity<LauncherView>>>,
            > = std::rc::Rc::new(std::cell::RefCell::new(None));
            let launcher_view_slot_init = launcher_view_slot.clone();

            let launcher_window = cx
                .open_window(launcher_opts, move |window, cx| {
                    if let Some(hwnd) = platform::hwnd_from_window(window) {
                        platform::configure_launcher_hwnd(hwnd);
                        if show_launcher {
                            platform::set_launcher_topmost(hwnd);
                            platform::apply_display_affinity(hwnd, capturable);
                        }
                    }
                    let view =
                        cx.new(|cx| LauncherView::new(initial_query, assets_dir, window, cx));
                    if show_launcher {
                        view.read(cx).focus_search(window, cx);
                    }
                    *launcher_view_slot_init.borrow_mut() = Some(view.clone());
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .ok();

            if show_main || show_launcher {
                cx.activate(true);
            }

            // Keep theme watcher alive inside the background signal task.
            cx.spawn(async move |cx| {
                let _keep_watcher = _theme_watcher;
                while let Ok(sig) = sig_rx.recv().await {
                    match sig {
                        AppSignal::ShowLauncher => {
                            if let Some(handle) = launcher_window {
                                let view_opt = launcher_view_slot.borrow().clone();
                                let _ = cx.update_window(handle.into(), |_, window, cx| {
                                    window.set_visible(true);
                                    if let Some(hwnd) = platform::hwnd_from_window(window) {
                                        platform::configure_launcher_hwnd(hwnd);
                                        platform::set_launcher_topmost(hwnd);
                                        platform::apply_display_affinity(hwnd, capturable);
                                    }
                                    window.activate_window();
                                    if let Some(view) = view_opt {
                                        view.update(cx, |v, cx| {
                                            v.show_with_query(Some("invoice"), window, cx);
                                        });
                                    }
                                });
                            }
                        }
                        AppSignal::Quit => {
                            let _ = cx.update(|cx| cx.quit());
                            break;
                        }
                    }
                }
            })
            .detach();
        });
}
