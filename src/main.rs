mod accounts;
mod asana;
mod assets;
mod browser;
mod catalog;
mod charge_codes;
mod chrome;
mod git_identity;
mod guard;
mod glass;
mod settings;
mod terminal;
mod text_field;
mod theme;
mod usage;
mod vector;
mod webserver;
mod workbench;
mod worktree;

use gpui::{
    App, Application, Bounds, Focusable, KeyBinding, Menu, MenuItem, SystemMenuType, TitlebarOptions,
    WindowBounds, WindowOptions, actions, point, prelude::*, px, size,
};

actions!(
    deck,
    [
        NewSession,
        NewTerminal,
        OpenFolder,
        CloseSession,
        NextSession,
        PreviousSession,
        ToggleSidebar,
        ToggleRightPanel,
        ToggleChargeCodes,
        ToggleAsanaPanel,
        ToggleLayout,
        ShowSessions,
        ShowThemes,
        Copy,
        Paste,
        Quit,
    ]
);

fn main() {
    // Invoked by Claude Code as a session's status line.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--statusline") {
        std::process::exit(usage::run_statusline(args.get(2).map(String::as_str).unwrap_or("default")));
    }

    Application::new().with_assets(assets::Assets).run(|cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("cmd-t", NewSession, None),
            KeyBinding::new("ctrl-`", NewTerminal, None),
            KeyBinding::new("cmd-o", OpenFolder, None),
            KeyBinding::new("cmd-w", CloseSession, None),
            KeyBinding::new("cmd-alt-right", NextSession, None),
            KeyBinding::new("cmd-alt-left", PreviousSession, None),
            KeyBinding::new("cmd-}", NextSession, None),
            KeyBinding::new("cmd-{", PreviousSession, None),
            KeyBinding::new("cmd-b", ToggleSidebar, None),
            KeyBinding::new("cmd-alt-b", ToggleRightPanel, None),
            KeyBinding::new("cmd-alt-c", ToggleChargeCodes, None),
            KeyBinding::new("cmd-alt-a", ToggleAsanaPanel, None),
            KeyBinding::new("cmd-\\", ToggleLayout, None),
            KeyBinding::new("cmd-shift-e", ShowSessions, None),
            KeyBinding::new("cmd-shift-t", ShowThemes, None),
            KeyBinding::new("cmd-c", Copy, Some("Terminal")),
            KeyBinding::new("cmd-v", Paste, Some("Terminal")),
            KeyBinding::new("cmd-q", Quit, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus(vec![
            Menu {
                name: "Claude Deck".into(),
                items: vec![
                    MenuItem::os_submenu("Services", SystemMenuType::Services),
                    MenuItem::separator(),
                    MenuItem::action("Quit Claude Deck", Quit),
                ],
            },
            Menu {
                name: "File".into(),
                items: vec![
                    MenuItem::action("New Session", NewSession),
                    MenuItem::action("Open Ghostty", NewTerminal),
                    MenuItem::action("Open Folder…", OpenFolder),
                    MenuItem::separator(),
                    MenuItem::action("Close Session", CloseSession),
                ],
            },
            Menu {
                name: "Edit".into(),
                items: vec![MenuItem::action("Copy", Copy), MenuItem::action("Paste", Paste)],
            },
            Menu {
                name: "View".into(),
                items: vec![
                    MenuItem::action("Sessions", ShowSessions),
                    MenuItem::action("Themes", ShowThemes),
                    MenuItem::separator(),
                    MenuItem::action("Toggle Side Bar", ToggleSidebar),
                    MenuItem::action("Toggle Right Panel", ToggleRightPanel),
                    MenuItem::action("Toggle Grid / Tabs", ToggleLayout),
                    MenuItem::separator(),
                    // Panels in the right panel; each can be added or removed.
                    MenuItem::action("Charge Codes Panel", ToggleChargeCodes),
                    MenuItem::action("Asana Panel", ToggleAsanaPanel),
                    MenuItem::separator(),
                    MenuItem::action("Next Session", NextSession),
                    MenuItem::action("Previous Session", PreviousSession),
                ],
            },
        ]);

        let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Claude Deck".into()),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(12.), px(13.))),
                    }),
                    window_min_size: Some(size(px(720.), px(420.))),
                    ..Default::default()
                },
                |window, cx| {
                    if settings::Settings::load().liquid_glass {
                        if let Some(glass) = glass::install(window) {
                            cx.set_global(glass);
                        }
                    }
                    let workbench = cx.new(|cx| workbench::Workbench::new(window, cx));
                    glass::match_theme(cx.global::<theme::Theme>().is_dark, cx);
                    window.focus(&workbench.focus_handle(cx));
                    workbench
                },
            )
            .unwrap();
        // `claude-deck DIR [DIR...]` opens a session per folder;
        // `claude-deck --smoke DIR` runs the account-switch smoke test.
        let args: Vec<String> = std::env::args().skip(1).collect();
        let smoke = args.first().is_some_and(|a| a == "--smoke");
        let dirs: Vec<_> = args.iter().map(std::path::PathBuf::from).filter(|p| p.is_dir()).collect();
        let _ = window.update(cx, |workbench, window, cx| {
            if smoke {
                if let Some(dir) = dirs.first() {
                    workbench.run_smoke(dir.clone(), window, cx);
                }
                return;
            }
            for dir in dirs {
                workbench.open_session(dir.canonicalize().unwrap_or(dir), window, cx);
            }
        });
        cx.on_window_closed(|cx| cx.quit()).detach();

        // Quit cleanly (stopping sessions, removing unused worktrees) when
        // asked to terminate by a signal, not only via ⌘Q.
        let (tx, rx) = smol::channel::bounded::<()>(1);
        if let Ok(mut signals) =
            signal_hook::iterator::Signals::new([signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP, signal_hook::consts::SIGINT])
        {
            std::thread::spawn(move || {
                if signals.forever().next().is_some() {
                    let _ = tx.send_blocking(());
                }
            });
        }
        cx.spawn(async move |cx| {
            if rx.recv().await.is_ok() {
                let _ = cx.update(|cx| cx.quit());
            }
        })
        .detach();
        cx.activate(true);
    });
}
