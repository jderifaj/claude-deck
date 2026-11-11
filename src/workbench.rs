//! The VS Code-style shell: title bar, activity bar, side bar, session
//! grid/tabs and status bar.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use gpui::{
    AnchoredPositionMode, AnyElement, AnyView, App, ClickEvent, ClipboardItem, Context, Entity, EntityId, FocusHandle,
    Focusable, Hsla, MouseButton, PathPromptOptions, PromptLevel, SharedString, StyleRefinement, Subscription, Task,
    Animation, AnimationExt, Transformation, Window, anchored, deferred, div, ease_in_out, ease_out_quint, img, point, prelude::*, px,
    radians, svg,
};

use crate::{
    CloseSession, NewSession, NewTerminal, NextSession, OpenFolder, PreviousSession, ShowSessions, ShowThemes,
    ToggleAsanaPanel, ToggleChargeCodes, ToggleLayout, ToggleRightPanel, ToggleSidebar,
    accounts::{self, Account},
    asana::{self, Bucket},
    browser::{self, Browser, InstalledBrowser, SimDevice},
    charge_codes::{self, ChargeSection},
    chrome,
    git_identity::{self, FolderLogins, GitIdentity},
    glass,
    catalog::{self, ThemeChoice},
    usage::{self, Usage},
    settings::{Settings, ThemeSetting},
    terminal::{SessionEvent, TerminalSession},
    text_field::{TextField, TextFieldEvent},
    theme::{self, Theme},
    webserver::{self, Log, Project, ServerProcess, StaticServer},
    worktree::{self, Cleanup, Workspace},
};

#[derive(Clone, Copy, PartialEq)]
enum SidebarView {
    Sessions,
    Themes,
}

#[derive(Clone, Copy, PartialEq)]
enum Layout {
    Grid,
    Tabs,
}

#[derive(Clone, Copy)]
enum NoticeKind {
    Info,
    Warning,
    Error,
}

/// What a text field in the right panel is editing.
#[derive(Clone, Copy, PartialEq)]
enum EditTarget {
    NewSection,
    SectionTitle(usize),
    NewCode(usize),
    Code(usize, usize),
    /// The Asana Personal Access Token, while connecting.
    AsanaToken,
}

/// The Asana panel's connection and what it last loaded.
#[derive(Default)]
struct AsanaPanel {
    config: Option<asana::Config>,
    /// Read from the Keychain once, then kept for the next reloads.
    token: Option<String>,
    tickets: Option<Vec<asana::Ticket>>,
    /// Shown under the logo; fetched on each load, never saved.
    user_name: String,
    workspace_name: String,
    projects: Option<Vec<asana::Project>>,
    /// Accordions: both start closed.
    tasks_open: bool,
    projects_open: bool,
    /// The project in My projects showing its tasks.
    open_project: Option<String>,
    /// A checked token whose account has several workspaces to pick from.
    choosing: Option<(String, asana::Me)>,
    /// The saved token is gone or Asana turned it down: ask for a new one.
    needs_token: bool,
    /// What's happening now ("Loading tasks…").
    busy: Option<&'static str>,
    error: Option<SharedString>,
    /// Something worth knowing that isn't a failure (shown muted).
    notice: Option<SharedString>,
    /// Removing the old token from the Keychain; a new one is saved after it.
    keychain: Option<Task<()>>,
    loaded_at: Option<Instant>,
    task: Option<Task<()>>,
}

impl AsanaPanel {
    fn setting_up(&self) -> bool {
        self.config.is_none() || self.needs_token || self.choosing.is_some()
    }
}

struct Editing {
    target: EditTarget,
    field: Entity<TextField>,
    _events: Subscription,
}

struct Notice {
    kind: NoticeKind,
    message: SharedString,
    /// Held back until the welcome screen's load animation has finished.
    shows_at: Instant,
}

/// How long a notice stays up once shown, unless the pointer is over it.
const NOTICE_TIME: Duration = Duration::from_secs(5);
/// How long a notice stays after the pointer leaves it.
const NOTICE_AFTER_HOVER: Duration = Duration::from_millis(1500);

/// Where a session's web server is (see `Workbench::toggle_webserver`).
#[derive(Clone, PartialEq)]
enum ServerPhase {
    /// Detecting the framework and a free port.
    Checking,
    /// Waiting for the user to tick the dialog's checkboxes and press Start.
    NeedsApproval,
    Installing,
    Starting,
    Ready,
    Failed(SharedString),
}

enum ServerHandle {
    Process(ServerProcess),
    /// Held for its Drop, which stops the server.
    Static(#[allow(dead_code)] StaticServer),
}

/// A web server for one session's folder, started (or found) with the
/// activity bar's webserver button.
struct ServerRun {
    dir: PathBuf,
    /// None for a server found already running that isn't a known project.
    project: Option<Project>,
    port: u16,
    url: String,
    /// The framework's usual port and who holds it, when we had to move.
    port_owner: Option<(u16, String)>,
    pages: Vec<String>,
    phase: ServerPhase,
    /// Found already running (not started by Claude Deck): no Stop button.
    external: bool,
    /// Dialog checkboxes.
    install: bool,
    allow_run: bool,
    remember: bool,
    /// Whether dependencies were installed as part of this start.
    install_ran: bool,
    log: Log,
    log_seen: u64,
    show_log: bool,
    handle: Option<ServerHandle>,
    started_at: Instant,
    /// Polls the process while it installs, starts and runs.
    _watch: Option<Task<()>>,
}

impl ServerRun {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            project: None,
            port: 0,
            url: String::new(),
            port_owner: None,
            pages: Vec::new(),
            phase: ServerPhase::Checking,
            external: false,
            install: true,
            allow_run: true,
            remember: true,
            install_ran: false,
            log: Log::default(),
            log_seen: 0,
            show_log: false,
            handle: None,
            started_at: Instant::now(),
            _watch: None,
        }
    }

    fn tech(&self) -> &'static str {
        self.project.as_ref().map_or("web", |p| p.framework.label())
    }

    fn is_busy(&self) -> bool {
        matches!(self.phase, ServerPhase::Checking | ServerPhase::Installing | ServerPhase::Starting)
    }

    fn spawn_dev(&mut self) -> std::io::Result<()> {
        let project = self.project.as_ref().ok_or_else(|| std::io::Error::other("no project"))?;
        let process = ServerProcess::spawn(&project.start_command(self.port), &project.root, Some(self.port), &self.log)?;
        self.handle = Some(ServerHandle::Process(process));
        self.phase = ServerPhase::Starting;
        self.started_at = Instant::now();
        Ok(())
    }

    fn process_exited(&mut self) -> Option<bool> {
        match &mut self.handle {
            Some(ServerHandle::Process(p)) => p.exited(),
            _ => None,
        }
    }
}

/// What looking at a session's folder found.
enum ServerFound {
    /// A server for the session is already running.
    Running { url: String, project: Option<Project>, pages: Vec<String> },
    Project { project: Project, port: u16, port_owner: Option<String>, pages: Vec<String> },
    Nothing(String),
}

impl ServerFound {
    /// Runs in the background: lsof, file reads, maybe `docker ps`.
    fn look(pid: Option<u32>, dir: &Path) -> Self {
        let project = webserver::detect(dir);
        let pages = project.as_ref().map(webserver::pages).unwrap_or_else(|_| vec!["/".into()]);
        if let Some(url) = browser::find_local_page(pid, dir).filter(|u| u.starts_with("http")) {
            return ServerFound::Running { url, project: project.ok(), pages };
        }
        match project {
            Err(why) => ServerFound::Nothing(why),
            Ok(project) => {
                let (port, port_owner) = webserver::pick_port(project.framework.default_port());
                ServerFound::Project { project, port, port_owner, pages }
            }
        }
    }
}

/// How often a starting or running server is checked.
const SERVER_TICK: Duration = Duration::from_millis(400);
/// How long a dev server gets to start answering.
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(120);
const SERVER_GREEN: u32 = 0x3FB950;
/// Longest an account's login status goes without `claude auth status`.
const ACCOUNT_RECHECK: Duration = Duration::from_secs(120);

pub struct Workbench {
    sessions: Vec<Entity<TerminalSession>>,
    session_subscriptions: HashMap<EntityId, Subscription>,
    active: Option<EntityId>,
    sidebar: SidebarView,
    sidebar_visible: bool,
    right_panel_visible: bool,
    charge_codes: Vec<ChargeSection>,
    editing: Option<Editing>,
    /// Charge code just copied (section, code), shown with a check mark.
    copied: Option<(usize, usize)>,
    copied_timer: Option<Task<()>>,
    layout: Layout,
    themes: Vec<ThemeChoice>,
    usage: HashMap<String, Usage>,
    settings: Settings,
    notice: Option<Notice>,
    /// Shows and then dismisses the notice; dropped (cancelled) while the
    /// pointer is over it.
    notice_timer: Option<Task<()>>,
    /// When this window opened, for holding notices back until the welcome
    /// animation is done.
    opened_at: Instant,
    next_id: usize,
    creating: usize,
    accounts: Vec<Account>,
    /// Session whose account menu is open.
    account_menu: Option<EntityId>,
    /// Session whose theme menu is open.
    theme_menu: Option<EntityId>,
    /// Session whose GitHub menu is open.
    git_menu: Option<EntityId>,
    /// Whether the Device Hub button's device menu is open.
    device_menu: bool,
    /// The branch badge's GitHub sign-in panel, while it's open.
    github_sign_in: Option<GithubSignIn>,
    /// Simulators for the device menu; None while they load.
    sim_devices: Option<Vec<SimDevice>>,
    menu_closed_at: Option<Instant>,
    /// Logins (emails) already warned about being shared by several profiles.
    warned_shared_logins: Vec<String>,
    refresh_accounts_tx: smol::channel::Sender<()>,
    /// GitHub logins each window's folder points at, by its cwd. Looked up in
    /// the background when the window opens, so menus never wait on `git`.
    folder_logins: HashMap<PathBuf, FolderLogins>,
    /// Ghostty's app icon (traced SVG), if Ghostty is installed.
    ghostty_icon: Option<PathBuf>,
    /// Browsers installed on this Mac, shown as buttons in the activity bar.
    browsers: Vec<InstalledBrowser>,
    /// Web servers started (or found) per session with the webserver button.
    servers: HashMap<EntityId, ServerRun>,
    /// Session whose web server dialog is showing.
    server_panel: Option<EntityId>,
    /// Side-bar sections folded shut: "project:<path>", "recent", "themes".
    collapsed: HashSet<String>,
    /// Projects already checked for leftover worktrees this run.
    swept: HashSet<PathBuf>,
    focus_handle: FocusHandle,
    _quit: Subscription,
    _account_refresh: Task<()>,
    _usage_refresh: Task<()>,
    asana: AsanaPanel,
    _asana_refresh: Task<()>,
}

const WARNING: Hsla = Hsla { h: 0.11, s: 0.9, l: 0.55, a: 1. };

fn icon(name: &str, size: f32, color: Hsla) -> gpui::Svg {
    svg().path(SharedString::from(format!("icons/{name}.svg"))).size(px(size)).flex_none().text_color(color)
}

/// The activity bar's own icons: VTracer traces (see src/vector.rs).
static CLAUDE_ICON: LazyLock<Arc<gpui::Image>> =
    LazyLock::new(|| Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Svg, crate::vector::CLAUDE_SVG.to_vec())));
static COLOR_BARS_ICON: LazyLock<Arc<gpui::Image>> =
    LazyLock::new(|| Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Svg, crate::vector::COLOR_BARS_SVG.to_vec())));
/// The Asana mark at the top of the Asana panel, in its own colors.
static ASANA_LOGO: LazyLock<Arc<gpui::Image>> =
    LazyLock::new(|| Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Svg, asana::LOGO_SVG.to_vec())));

/// The welcome screen's app icon: a 320px WebP (4 KB) made from the 4096px
/// master, enough for a sharp 2x render at `WELCOME_ICON_SIZE`. Decoding the
/// full master would cost ~64 MB of RGBA for a 144px image.
static WELCOME_ICON: LazyLock<Arc<gpui::Image>> = LazyLock::new(|| {
    Arc::new(gpui::Image::from_bytes(
        gpui::ImageFormat::Webp,
        include_bytes!("../assets/brand/claude-deck-icon-320.webp").to_vec(),
    ))
});
const WELCOME_ICON_SIZE: f32 = 144.;

/// The brand card used by the welcome screen and the notice toast: black,
/// white text, a 1px burnt-orange (Claude) border, 16px corners.
const CARD_RADIUS: f32 = 16.;
const CARD_BORDER: f32 = 1.;
fn brand_orange() -> Hsla {
    gpui::rgb(0xD97757).into()
}
fn card(d: gpui::Div) -> gpui::Div {
    d.bg(gpui::black())
        .text_color(gpui::white())
        .rounded(px(CARD_RADIUS))
        .border(px(CARD_BORDER))
        .border_color(brand_orange())
}

/// Welcome card load timing: the glow waits for the icon, then takes one
/// lap; the card's lines fade in top to bottom as it sets off.
const SPARKLE_DELAY_MS: u64 = 450;
/// The last row of text has finished fading in this long after the window
/// opens, and the glow finishes its lap at the same moment.
const TEXT_DONE_MS: u64 = 1700;
const SPARKLE_DELAY: Duration = Duration::from_millis(SPARKLE_DELAY_MS);
const SPARKLE_LAP: Duration = Duration::from_millis(TEXT_DONE_MS - SPARKLE_DELAY_MS);
/// When the whole load animation (icon, text, glow) has finished.
const WELCOME_ANIMATION_DONE: Duration = Duration::from_millis(TEXT_DONE_MS);
const LINE_FADE: Duration = Duration::from_millis(300);
/// Rows of text in the welcome card: the title, tagline, the Start and
/// Shortcuts rows side by side, then the footer.
const WELCOME_ROWS: u32 = 9;
const TEXT_DONE: Duration = Duration::from_millis(TEXT_DONE_MS);

/// Fade a welcome-card line in once: row 0 as the glow's lap begins, the
/// rest evenly spaced so the last row is fully in at `TEXT_DONE`.
fn fade_line<E: Styled + IntoElement + 'static>(el: E, id: usize, row: u32) -> gpui::AnimationElement<E> {
    let gap = (TEXT_DONE - SPARKLE_DELAY - LINE_FADE) / (WELCOME_ROWS - 1);
    let wait = SPARKLE_DELAY + gap * row;
    el.opacity(0.).with_animations(
        ("welcome-line", id),
        vec![Animation::new(wait), Animation::new(LINE_FADE).with_easing(ease_in_out)],
        |el, ix, t| el.opacity(if ix == 0 { 0. } else { t }),
    )
}

/// The point `s` px clockwise along the edge of a rounded rectangle, from
/// the left end of its top edge.
fn point_on_rounded_rect(b: gpui::Bounds<gpui::Pixels>, r: f32, s: f32) -> (f32, f32) {
    use std::f32::consts::FRAC_PI_2;
    let (x0, y0) = (f32::from(b.origin.x), f32::from(b.origin.y));
    let (x1, y1) = (x0 + f32::from(b.size.width), y0 + f32::from(b.size.height));
    let (w, h) = (x1 - x0 - 2. * r, y1 - y0 - 2. * r);
    let arc = FRAC_PI_2 * r;
    let total = 2. * (w + h) + 4. * arc;
    let mut s = s.rem_euclid(total);
    let at_arc = |cx: f32, cy: f32, start: f32, s: f32| {
        let a = start + s / r;
        (cx + r * a.cos(), cy + r * a.sin())
    };
    // (length, point at distance d into the segment), clockwise.
    let segments: [(f32, &dyn Fn(f32) -> (f32, f32)); 8] = [
        (w, &|d| (x0 + r + d, y0)),
        (arc, &|d| at_arc(x1 - r, y0 + r, -FRAC_PI_2, d)),
        (h, &|d| (x1, y0 + r + d)),
        (arc, &|d| at_arc(x1 - r, y1 - r, 0., d)),
        (w, &|d| (x1 - r - d, y1)),
        (arc, &|d| at_arc(x0 + r, y1 - r, FRAC_PI_2, d)),
        (h, &|d| (x0, y1 - r - d)),
        (arc, &|d| at_arc(x0 + r, y0 + r, 2. * FRAC_PI_2, d)),
    ];
    for (len, at) in segments {
        if s <= len {
            return at(s);
        }
        s -= len;
    }
    (x0 + r, y0)
}

/// One frame of the welcome card's load sparkle: a soft 100px glow along
/// the border, centred `t` (0..1) of the way clockwise around it. `bounds`
/// is the card's padding box (inside the border).
fn paint_sparkle(bounds: gpui::Bounds<gpui::Pixels>, t: f32, window: &mut Window) {
    /// Length of the glow along the border, in px.
    const GLOW_LEN: f32 = 100.;
    /// Spacing of the overlapping soft dots that make up the glow.
    const STEP: f32 = 1.;
    // Fade in over the first 10% of the lap, out over the last 25%.
    let envelope = (t / 0.1).min(1.) * ((1. - t) / 0.25).min(1.);
    if envelope <= 0. {
        return;
    }
    // Follow the centre line of the border.
    let half = CARD_BORDER / 2.;
    let edge = gpui::Bounds {
        origin: gpui::point(bounds.origin.x - px(half), bounds.origin.y - px(half)),
        size: gpui::size(bounds.size.width + px(CARD_BORDER), bounds.size.height + px(CARD_BORDER)),
    };
    let r = CARD_RADIUS - half;
    let perimeter = 2. * (f32::from(edge.size.width) + f32::from(edge.size.height) - 4. * r)
        + 2. * std::f32::consts::PI * r;
    let centre = t * perimeter;
    let dot = |window: &mut Window, (x, y): (f32, f32), size: f32, color: Hsla| {
        let b = gpui::Bounds { origin: gpui::point(px(x - size / 2.), px(y - size / 2.)), size: gpui::size(px(size), px(size)) };
        window.paint_quad(gpui::fill(b, color).corner_radii(px(size / 2.)));
    };
    let orange = brand_orange();
    let warm = gpui::Hsla { l: 0.85, ..orange };
    // Raised-cosine falloff: 1 at the centre, 0 at either end of the glow.
    // Each spot is overlapped by several neighbours, so per-dot alpha is low.
    let steps = (GLOW_LEN / STEP) as i32;
    for i in -steps / 2..=steps / 2 {
        let d = i as f32 * STEP;
        let k = 0.5 + 0.5 * (std::f32::consts::PI * d / (GLOW_LEN / 2.)).cos();
        let a = k * envelope;
        let p = point_on_rounded_rect(edge, r, centre + d);
        dot(window, p, 10., orange.opacity(0.018 * a)); // wide halo
        dot(window, p, 3., warm.opacity(0.15 * a)); // brighter line on the border
    }
}

/// Shown under a window's branch badge when its repo is private and its
/// GitHub account hasn't been confirmed signed in to GitHub in Chrome, so the
/// link would 404. Stays open (outside clicks don't close it) while the user
/// signs in in Chrome.
struct GithubSignIn {
    session: EntityId,
    user: String,
    repo: String,
    profiles: Vec<chrome::Profile>,
    /// Profile folder picked; `None` means a new profile.
    chosen: Option<String>,
    /// GitHub's sign-in page was opened in the chosen profile.
    opened: bool,
}

/// A window about to open, with what it needs from `git` already looked up.
struct Opening {
    workspace: Workspace,
    branch: Option<String>,
    logins: FolderLogins,
}

impl Opening {
    /// Runs `git`, so call it on the background executor: the first `git`
    /// after a reboot or an Xcode update can take seconds (it's an xcrun shim),
    /// which would beachball the app if it ran on the main thread.
    fn load(workspace: Workspace) -> Self {
        let branch = workspace.branch();
        let logins = git_identity::folder_logins(&workspace.cwd);
        Self { workspace, branch, logins }
    }
}

/// Ghostty's app icon, traced to an SVG. None if Ghostty isn't installed.
fn ghostty_icon() -> Option<PathBuf> {
    browser::app_icon(&browser::app_bundle("Ghostty")?, "ghostty-icon")
}

fn home_relative(path: &std::path::Path) -> String {
    let home = dirs::home_dir().unwrap_or_default();
    match path.strip_prefix(&home) {
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_) => path.display().to_string(),
    }
}

fn builtin_theme(setting: Option<&ThemeSetting>) -> Option<Theme> {
    match setting {
        None => Some(Theme::dark_modern()),
        Some(s) if s.path.as_os_str().is_empty() && s.label == "Light Modern" => Some(Theme::light_modern()),
        Some(s) if s.path.as_os_str().is_empty() => Some(Theme::dark_modern()),
        _ => None,
    }
}

impl Workbench {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = Settings::load();
        let theme = builtin_theme(settings.color_theme.as_ref())
            .or_else(|| {
                let s = settings.color_theme.as_ref()?;
                Theme::load_vscode(&s.path, &s.label, &s.ui_theme).ok()
            })
            .unwrap_or_else(Theme::dark_modern);
        cx.set_global(theme);

        cx.spawn(async move |this, cx| {
            let found = cx.background_executor().spawn(async { catalog::scan() }).await;
            let _ = this.update(cx, |this, cx| {
                this.themes = found;
                cx.notify();
            });
        })
        .detach();

        // Keep every account's login status current, so window labels follow
        // /login and /logout. Every 10s (or on demand) look at when each
        // account's login files last changed; only run `claude auth status`
        // (a Node start, ~0.1s of CPU) for an account whose files changed, a
        // new one, or every ACCOUNT_RECHECK in case a login only touched the
        // Keychain.
        let (refresh_accounts_tx, refresh_rx) = smol::channel::unbounded::<()>();
        let command = settings.claude_command.clone();
        let account_refresh = cx.spawn(async move |this, cx| {
            let bin = cx.background_executor().spawn(async move { accounts::resolve_claude_bin(&command) }).await;
            let mut known = accounts::KnownStatuses::new();
            let mut checked_all_at: Option<Instant> = None;
            loop {
                let bin = bin.clone();
                let recheck_all = checked_all_at.is_none_or(|t| t.elapsed() >= ACCOUNT_RECHECK);
                if recheck_all {
                    checked_all_at = Some(Instant::now());
                }
                let mut seen = std::mem::take(&mut known);
                let (found, seen) = cx
                    .background_executor()
                    .spawn(async move {
                        let mut list = accounts::discover();
                        if let Some(bin) = &bin {
                            accounts::refresh_statuses(&mut list, &mut seen, recheck_all, |a| accounts::query_status(bin, a));
                        }
                        (list, seen)
                    })
                    .await;
                known = seen;
                let updated = this.update(cx, |this, cx| {
                    if this.accounts != found {
                        this.accounts = found;
                        this.warn_shared_logins(cx);
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    break;
                }
                let timer = cx.background_executor().timer(Duration::from_secs(10));
                smol::future::or(timer, async {
                    let _ = refresh_rx.recv().await;
                })
                .await;
            }
        });

        cx.spawn(async move |this, cx| {
            let (ghostty, browsers) =
                cx.background_executor().spawn(async { (ghostty_icon(), browser::installed()) }).await;
            let _ = this.update(cx, |this, cx| {
                this.ghostty_icon = ghostty;
                this.browsers = browsers;
                cx.notify();
            });
        })
        .detach();

        // Usage files are written by each session's status line; re-read them
        // and redraw when they change, or when a reset countdown ("in 2h 13m")
        // reaches its next minute.
        let usage_refresh = cx.spawn(async move |this, cx| {
            let mut drawn = Vec::new();
            loop {
                let found = cx.background_executor().spawn(async { usage::load_all() }).await;
                let countdowns = usage::countdown_minutes(&found, usage::now());
                let alive = this.update(cx, |this, cx| {
                    if this.usage != found || countdowns != drawn {
                        this.usage = found;
                        drawn = countdowns;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
            }
        });

        // Load the Asana tasks the first time the panel shows (also when it
        // was left open at launch). After that only the Reload button does.
        let asana_refresh = cx.spawn(async move |this, cx| loop {
            let alive = this.update(cx, |this, cx| {
                if this.asana_visible() && this.asana.loaded_at.is_none() {
                    this.reload_asana(cx);
                }
            });
            if alive.is_err() {
                break;
            }
            cx.background_executor().timer(Duration::from_secs(30)).await;
        });

        Self {
            sessions: Vec::new(),
            session_subscriptions: HashMap::new(),
            active: None,
            sidebar: SidebarView::Sessions,
            sidebar_visible: true,
            right_panel_visible: settings.right_panel_open,
            warned_shared_logins: Vec::new(),
            charge_codes: charge_codes::load(),
            editing: None,
            copied: None,
            copied_timer: None,
            layout: Layout::Grid,
            themes: catalog::builtin_choices(),
            usage: usage::load_all(),
            settings,
            notice: None,
            notice_timer: None,
            opened_at: Instant::now(),
            next_id: 0,
            creating: 0,
            accounts: accounts::discover(),
            account_menu: None,
            theme_menu: None,
            git_menu: None,
            device_menu: false,
            github_sign_in: None,
            sim_devices: None,
            menu_closed_at: None,
            refresh_accounts_tx,
            _account_refresh: account_refresh,
            _usage_refresh: usage_refresh,
            asana: AsanaPanel { config: asana::load_config(), ..Default::default() },
            _asana_refresh: asana_refresh,
            folder_logins: HashMap::new(),
            ghostty_icon: None,
            browsers: Vec::new(),
            servers: HashMap::new(),
            server_panel: None,
            collapsed: HashSet::new(),
            swept: HashSet::new(),
            focus_handle: cx.focus_handle(),
            _quit: cx.on_app_quit(|this, cx| {
                this.shutdown(cx);
                async {}
            }),
        }
    }

    // ---- sessions ------------------------------------------------------

    fn active_session(&self) -> Option<&Entity<TerminalSession>> {
        let id = self.active?;
        self.sessions.iter().find(|s| s.entity_id() == id)
    }

    fn new_session(&mut self, _: &NewSession, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.active_session().map(|s| s.read(cx).workspace.project.clone());
        match dir {
            Some(dir) => self.open_session(dir, window, cx),
            None => self.prompt_for_folder(window, cx),
        }
    }

    /// Open a Ghostty window in the active session's folder (else home).
    fn new_terminal(&mut self, _: &NewTerminal, _: &mut Window, cx: &mut Context<Self>) {
        let dir = self
            .active_session()
            .map(|s| s.read(cx).workspace.cwd.clone())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    std::process::Command::new("open")
                        .args(["-na", "Ghostty", "--args"])
                        .arg(format!("--working-directory={}", dir.display()))
                        .status()
                })
                .await;
            let error = match result {
                Ok(status) if status.success() => return,
                Ok(_) => "Couldn't open Ghostty. Is it installed in /Applications?".to_string(),
                Err(e) => format!("Couldn't open Ghostty: {e}"),
            };
            let _ = this.update(cx, |this, cx| this.notify(NoticeKind::Error, error, cx));
        })
        .detach();
    }

    /// Open the active session's local page (its dev server, else its
    /// index.html) in a Mac browser.
    fn open_in_browser(&mut self, browser: Browser, cx: &mut Context<Self>) {
        let Some(app) = self.browsers.iter().find(|b| b.browser == browser).map(|b| b.app.clone()) else { return };
        self.open_local_page(browser.label().to_string(), cx, move |url| browser::open(&app, url));
    }

    /// Open the active session's local page in Safari on simulator `device`.
    fn open_in_simulator(&mut self, device: SimDevice, cx: &mut Context<Self>) {
        self.close_menus(cx);
        let Some(app) = self.simulator_app() else { return };
        if self.active_session().is_some() && !device.booted {
            self.notify(NoticeKind::Info, format!("Starting the {} simulator…", device.name), cx);
        }
        let label = format!("Safari on the {} simulator", device.name);
        self.open_local_page(label, cx, move |url| browser::open_in_simulator(&app, &device.udid, url));
    }

    /// Find the active session's local page and hand its URL to `open`
    /// (run in the background), reporting the result as `label`.
    fn open_local_page(
        &mut self,
        label: String,
        cx: &mut Context<Self>,
        open: impl FnOnce(&str) -> anyhow::Result<()> + Send + 'static,
    ) {
        let Some(session) = self.active_session() else {
            self.notify(NoticeKind::Info, "Open a Claude session first; its local page opens in the browser.", cx);
            return;
        };
        // A server from the webserver button wins: its URL is exact (and
        // Claude Deck's own static server isn't found by process lookup).
        let known = self
            .servers
            .get(&session.entity_id())
            .filter(|r| r.phase == ServerPhase::Ready)
            .map(|r| r.url.clone());
        let (pid, dir) = {
            let s = session.read(cx);
            (s.pid(), s.workspace.cwd.clone())
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let label = label.clone();
                    async move {
                        let url = match known {
                            Some(url) => url,
                            None => browser::find_local_page(pid, &dir).ok_or_else(|| {
                                format!(
                                    "No local server is running for {}, and it has no index.html. The webserver button can start one.",
                                    home_relative(&dir)
                                )
                            })?,
                        };
                        open(&url).map_err(|e| format!("Couldn't open {label}: {e}"))?;
                        Ok::<_, String>(url)
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(url) => this.notify(NoticeKind::Info, format!("Opened {url} in {label}"), cx),
                Err(e) => this.notify(NoticeKind::Warning, e, cx),
            });
        })
        .detach();
    }

    fn simulator_app(&self) -> Option<PathBuf> {
        self.browsers.iter().find(|b| b.browser == Browser::Simulator).map(|b| b.app.clone())
    }

    /// Open or close the Device Hub button's menu, reloading its simulators
    /// on open so devices added in DeviceHub show up.
    fn toggle_device_menu(&mut self, cx: &mut Context<Self>) {
        let was_open = self.device_menu || self.recently_closed_menu();
        self.close_menus(cx);
        if was_open {
            return;
        }
        let Some(app) = self.simulator_app() else { return };
        self.device_menu = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { browser::simulator_devices(&app) }).await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(devices) => {
                    this.sim_devices = Some(devices);
                    cx.notify();
                }
                Err(e) => {
                    this.close_menus(cx);
                    this.notify(NoticeKind::Warning, format!("Couldn't list simulators: {e}"), cx);
                }
            });
        })
        .detach();
    }

    /// The Device Hub button's menu: one row per simulator.
    fn render_device_menu(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let hover = t.list_hover_bg;
        let mut list = div().id("device-menu-list").max_h(px(420.)).overflow_y_scroll().flex().flex_col();
        match &self.sim_devices {
            None => {
                list = list.child(div().px(px(12.)).py(px(4.)).text_color(t.fg_muted).child("Loading simulators…"));
            }
            Some(devices) => {
                for (i, device) in devices.iter().enumerate() {
                    if i > 0 && device.is_ipad() && !devices[i - 1].is_ipad() {
                        list = list.child(div().my(px(4.)).h(px(1.)).bg(t.border));
                    }
                    let detail = if device.booted { format!("{} · Running", device.os) } else { device.os.clone() };
                    let picked = device.clone();
                    list = list.child(
                        div()
                            .id(("device", i))
                            .h(px(24.))
                            .pl(px(12.))
                            .pr(px(12.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover))
                            .child(icon(
                                if device.is_ipad() { "tablet" } else { "phone" },
                                14.,
                                if device.booted { t.focus_border } else { t.fg_muted },
                            ))
                            .child(div().flex_1().truncate().child(device.name.clone()))
                            .child(div().flex_none().text_size(px(12.)).text_color(t.fg_muted).child(detail))
                            .on_click(cx.listener(move |this, _, _, cx| this.open_in_simulator(picked.clone(), cx))),
                    );
                }
            }
        }
        div()
            .id("device-menu")
            .occlude()
            .w(px(300.))
            .py(px(6.))
            .rounded(px(6.))
            .bg(t.notification_bg)
            .border_1()
            .border_color(t.border)
            .shadow_lg()
            .text_color(t.fg)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menus(cx)))
            .child(
                div()
                    .px(px(12.))
                    .pb(px(4.))
                    .text_size(px(12.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(t.fg_muted)
                    .child("OPEN IN DEVICE HUB"),
            )
            .child(list)
    }

    fn open_folder(&mut self, _: &OpenFolder, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_for_folder(window, cx);
    }

    fn prompt_for_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Start Claude Here".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                if let Some(dir) = paths.into_iter().next() {
                    let _ = this.update_in(cx, |this, window, cx| this.open_session(dir, window, cx));
                }
            }
        })
        .detach();
    }

    /// Clear out worktrees a crash or force quit left behind in `project`,
    /// once per run. Done when a project is opened rather than at launch, so
    /// starting the app never reads project folders (one in Desktop or
    /// Documents would raise a macOS permission prompt out of context).
    fn sweep_orphans(&mut self, project: PathBuf, cx: &mut Context<Self>) {
        if !self.swept.insert(project.clone()) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let removed = cx.background_executor().spawn(async move { worktree::sweep_orphans(&[project]) }).await;
            if !removed.is_empty() {
                let names: Vec<String> = removed.iter().map(|d| home_relative(d)).collect();
                let _ = this.update(cx, |this, cx| {
                    this.notify(NoticeKind::Info, format!("Removed unused worktrees left from a previous run: {}", names.join(", ")), cx)
                });
            }
        })
        .detach();
    }

    /// A Recent Projects entry was clicked. Its folder is only checked now,
    /// not while drawing the list, so launching never touches protected
    /// folders; one that's gone is dropped from the list.
    fn open_recent(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if dir.is_dir() {
            return self.open_session(dir, window, cx);
        }
        self.settings.recent_projects.retain(|p| p != &dir);
        self.settings.save();
        self.notify(NoticeKind::Info, format!("{} no longer exists, so it was removed from Recent Projects.", home_relative(&dir)), cx);
    }

    /// Start a session in `dir`. The first session in a project uses the
    /// checkout directly; any additional one gets its own worktree.
    /// Every `git` call happens in the background (see `Opening::load`).
    pub fn open_session(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.creating += 1;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let shared = cx.background_executor().spawn({
                let dir = dir.clone();
                async move { Opening::load(worktree::shared(&dir)) }
            });
            let shared = shared.await;
            this.update(cx, |this, cx| this.sweep_orphans(shared.workspace.project.clone(), cx)).ok();
            // Checked and added in one update, so two windows opened at once
            // can't both take the main checkout.
            let isolate = this.update_in(cx, |this, window, cx| {
                let main_in_use = this.sessions.iter().any(|s| {
                    let ws = &s.read(cx).workspace;
                    ws.project == shared.workspace.project && ws.isolation.is_none()
                });
                if main_in_use || this.settings.always_isolate {
                    return true;
                }
                this.creating -= 1;
                this.add_session(shared, window, cx);
                false
            });
            if !matches!(isolate, Ok(true)) {
                return;
            }
            let result = cx.background_executor().spawn(async move { worktree::isolated(&dir).map(Opening::load) }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.creating -= 1;
                match result {
                    Ok(opening) => {
                        if let Some(iso) = &opening.workspace.isolation {
                            let msg = format!(
                                "Started in its own worktree on branch {} ({}) so it can't overwrite files used by other sessions.",
                                iso.branch,
                                home_relative(&iso.dir)
                            );
                            this.notify(NoticeKind::Info, msg, cx);
                        }
                        this.add_session(opening, window, cx);
                    }
                    Err(e) => this.notify(NoticeKind::Error, e.to_string(), cx),
                }
            });
        })
        .detach();
    }

    fn add_session(&mut self, opening: Opening, window: &mut Window, cx: &mut Context<Self>) -> Entity<TerminalSession> {
        // New windows start with the configured default account, else the
        // active window's account.
        let account = self
            .settings
            .default_account_email
            .as_ref()
            .and_then(|email| self.accounts.iter().find(|a| self.email_of(a).as_ref() == Some(email)).cloned())
            .or_else(|| self.active_session().map(|s| s.read(cx).account.clone()))
            .unwrap_or_else(Account::default_account);
        self.add_session_as(opening, account, window, cx)
    }

    /// The account's email: from its live status, else its profile file.
    fn email_of(&self, account: &Account) -> Option<String> {
        account
            .status
            .as_ref()
            .filter(|s| s.logged_in)
            .and_then(|s| s.email.clone())
            .or_else(|| accounts::configured_email(account))
    }

    /// GitHub identity a window uses unless overridden: the one its folder
    /// points at, else its Claude account's default.
    fn git_for(&self, account: &Account, cwd: &Path) -> Option<GitIdentity> {
        self.folder_git(cwd).or_else(|| self.settings.git_for_email(&self.email_of(account)?)).cloned()
    }

    /// The identity a window's folder points at, from the logins looked up
    /// when it opened.
    fn folder_git(&self, cwd: &Path) -> Option<&GitIdentity> {
        self.settings.git_for_folder(self.folder_logins.get(cwd)?)
    }

    fn add_session_as(&mut self, opening: Opening, account: Account, window: &mut Window, cx: &mut Context<Self>) -> Entity<TerminalSession> {
        let Opening { workspace, branch, logins } = opening;
        self.folder_logins.insert(workspace.cwd.clone(), logins);
        self.next_id += 1;
        let id = self.next_id;
        self.settings.add_recent(workspace.project.clone());
        self.settings.save();
        let git = self.git_for(&account, &workspace.cwd);
        let s = &self.settings;
        let (command, family, size) = (s.claude_command.clone(), s.font_family.clone(), s.font_size);
        let session = cx.new(|cx| TerminalSession::new(id, workspace, branch, account, git, command, family, size, window, cx));
        let subscription = cx.subscribe_in(&session, window, |this, session, event, _, cx| {
            match event {
                SessionEvent::Focused => {
                    this.active = Some(session.entity_id());
                    this.refresh_accounts();
                }
                SessionEvent::Exited => this.refresh_accounts(),
                SessionEvent::TitleChanged => {}
            }
            cx.notify();
        });
        self.session_subscriptions.insert(session.entity_id(), subscription);
        self.sessions.push(session.clone());
        self.activate(&session, window, cx);
        session
    }

    /// The branch shown on a window's badge.
    fn badge_branch(s: &TerminalSession) -> String {
        match &s.workspace.isolation {
            Some(iso) => iso.branch.clone(),
            None => s.branch.clone().unwrap_or_else(|| "main".into()),
        }
    }

    /// The branch badge was clicked: open the branch on GitHub. A public
    /// repo just opens. A private one 404s unless the browser is signed in as
    /// the window's GitHub account, so it opens in that account's Chrome
    /// profile once the user has confirmed it's signed in there, and asks
    /// them to sign in (`GithubSignIn`) before that.
    fn open_branch_link(&mut self, id: EntityId, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.iter().find(|s| s.entity_id() == id).cloned() else { return };
        let s = session.read(cx);
        let Some(web) = self.folder_logins.get(&s.workspace.cwd).and_then(|l| l.web.clone()) else { return };
        let (cwd, branch) = (s.workspace.cwd.clone(), Self::badge_branch(s));
        let user = s.git.as_ref().map(|g| g.github_user.clone()).filter(|_| chrome::installed());
        let profile = user.as_ref().and_then(|u| self.settings.chrome_profile_for_github.get(u).cloned());
        let confirmed = user.as_ref().is_some_and(|u| self.settings.github_web_confirmed.contains(u) && profile.is_some());
        let repo = web.rsplit('/').next().unwrap_or_default().to_string();
        cx.spawn(async move |this, cx| {
            let (url, public) = cx
                .background_executor()
                .spawn(async move { (git_identity::branch_web_url(&cwd, &web, Some(&branch)), chrome::is_public(&web)) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match (user, public) {
                    // Confirmed signed in: its profile, public or not.
                    (Some(_), _) if confirmed => this.open_in_profile(profile.unwrap_or_default(), url, cx),
                    // Public, or no account to sign in as: the default
                    // browser (or the account's profile, if it has one).
                    (_, Some(true)) | (None, _) => match profile {
                        Some(dir) => this.open_in_profile(dir, url, cx),
                        None => cx.open_url(&url),
                    },
                    // Private (or GitHub unreachable): sign in first.
                    (Some(user), _) => {
                        this.close_menus(cx);
                        this.github_sign_in = Some(GithubSignIn {
                            session: id,
                            user,
                            repo,
                            profiles: chrome::profiles(),
                            chosen: profile,
                            opened: false,
                        });
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn open_in_profile(&mut self, dir: String, url: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let opened = cx.background_executor().spawn(async move { chrome::open(&dir, &url) }).await;
            if let Err(e) = opened {
                let _ = this.update(cx, |this, cx| this.notify(NoticeKind::Error, format!("Couldn't open GitHub in Chrome: {e}"), cx));
            }
        })
        .detach();
    }

    /// Sign-in panel, step 2: save the picked profile for the account and
    /// open GitHub's sign-in page in it.
    fn open_github_sign_in(&mut self, cx: &mut Context<Self>) {
        let taken: Vec<String> = self.settings.chrome_profile_for_github.values().cloned().collect();
        let Some(panel) = self.github_sign_in.as_mut() else { return };
        let dir = panel.chosen.clone().unwrap_or_else(|| {
            let taken: Vec<&str> = taken.iter().map(String::as_str).collect();
            chrome::new_profile_dir(&panel.profiles, &taken)
        });
        panel.chosen = Some(dir.clone());
        panel.opened = true;
        let user = panel.user.clone();
        self.settings.github_web_confirmed.retain(|u| u != &user);
        self.settings.chrome_profile_for_github.insert(user, dir.clone());
        self.settings.save();
        self.open_in_profile(dir, chrome::SIGN_IN_URL.to_string(), cx);
        cx.notify();
    }

    /// Sign-in panel, step 3: the user says Chrome is signed in as the
    /// account; remember that and open the branch.
    fn confirm_github_sign_in(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.github_sign_in.take() else { return };
        if !self.settings.github_web_confirmed.contains(&panel.user) {
            self.settings.github_web_confirmed.push(panel.user);
            self.settings.save();
        }
        self.open_branch_link(panel.session, cx);
    }

    /// `claude-deck --smoke DIR`: drive the real account-switch path and
    /// check what each window's Claude reports. Prints results, then quits.
    pub fn run_smoke(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let accounts = accounts::discover();
        if accounts.len() < 2 {
            eprintln!("SMOKE: need two accounts, found {}", accounts.len());
            return cx.quit();
        }
        let (a, b) = (accounts[0].clone(), accounts[1].clone());
        // Both windows start as A, like a new window inheriting the active one.
        // The smoke test may block on `git`; the app itself never does.
        let w1 = self.add_session_as(Opening::load(worktree::shared(&dir)), a.clone(), window, cx);
        let w2 = self.add_session_as(Opening::load(worktree::shared(&dir)), a.clone(), window, cx);
        // A window opened the normal way (⌘T) should get the default account.
        let w3 = self.add_session(Opening::load(worktree::shared(&dir)), window, cx);
        cx.spawn_in(window, async move |this, cx| {
            let executor = cx.background_executor().clone();
            let wait = |secs| executor.timer(Duration::from_secs(secs));
            wait(4).await;
            // Menu picks: window 1 -> B, window 2 -> B, then window 2 back -> A.
            let _ = this.update(cx, |this, cx| {
                this.perform_switch(&w1, b.clone(), false, cx);
                this.perform_switch(&w2, b.clone(), false, cx);
            });
            wait(4).await;
            let _ = this.update(cx, |this, cx| this.perform_switch(&w2, a.clone(), false, cx));
            wait(16).await;
            let question = "Without using any tools, reply with exactly one line in the form ACCOUNT=<the user's email address from your context>.";
            let _ = this.update(cx, |_, cx| {
                for w in [&w1, &w2] {
                    w.update(cx, |s, _| s.type_text(question));
                }
            });
            wait(1).await;
            let _ = this.update(cx, |_, cx| {
                for w in [&w1, &w2] {
                    w.update(cx, |s, _| s.type_text("\r"));
                }
            });
            wait(35).await;
            let _ = this.update(cx, |this, cx| {
                let mut ok = true;
                for (n, w) in [&w1, &w2].into_iter().enumerate() {
                    let s = w.read(cx);
                    let chip = this.account_of(s).label();
                    let screen = s.screen_text();
                    let answered: Vec<String> = screen
                        .split("ACCOUNT=")
                        .skip(1)
                        .filter_map(|rest| rest.split_whitespace().next())
                        .filter(|e| e.contains('@') && !e.contains('<'))
                        .map(|e| e.trim_end_matches(['.', ',', ')']).to_string())
                        .collect();
                    let github = s.git.as_ref().map(|g| g.github_user.clone());
                    let expected_github = this.git_for(&s.account, &s.workspace.cwd).map(|g| g.github_user);
                    let pass = !answered.is_empty() && answered.iter().all(|e| *e == chip) && github == expected_github;
                    ok &= pass;
                    eprintln!(
                        "SMOKE window {}: profile={} chip={} claude_answered={:?} github={:?} expected_github={:?} -> {}",
                        n + 1,
                        s.account.id,
                        chip,
                        answered,
                        github,
                        expected_github,
                        if pass { "PASS" } else { "FAIL" }
                    );
                }
                let s3 = w3.read(cx);
                let new_email = this.email_of(&s3.account);
                let new_github = s3.git.as_ref().map(|g| g.github_user.clone());
                let expected_email = this.settings.default_account_email.clone();
                let expected_github = expected_email.as_deref().and_then(|e| this.settings.git_for_email(e)).map(|g| g.github_user.clone());
                let pass = new_email == expected_email && new_github == expected_github;
                ok &= pass;
                eprintln!(
                    "SMOKE new window: account={new_email:?} expected={expected_email:?} github={new_github:?} expected_github={expected_github:?} -> {}",
                    if pass { "PASS" } else { "FAIL" }
                );
                eprintln!("SMOKE RESULT: {}", if ok { "PASS" } else { "FAIL" });
                cx.quit();
            });
        })
        .detach();
    }

    fn activate(&mut self, session: &Entity<TerminalSession>, window: &mut Window, cx: &mut Context<Self>) {
        self.active = Some(session.entity_id());
        window.focus(&session.focus_handle(cx));
        cx.notify();
    }

    fn activate_id(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.sessions.iter().find(|s| s.entity_id() == id).cloned() {
            self.activate(&session, window, cx);
        }
    }

    fn cycle(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.sessions.is_empty() {
            return;
        }
        let len = self.sessions.len() as isize;
        let current = self
            .active
            .and_then(|id| self.sessions.iter().position(|s| s.entity_id() == id))
            .unwrap_or(0) as isize;
        let next = self.sessions[((current + delta).rem_euclid(len)) as usize].clone();
        self.activate(&next, window, cx);
    }

    fn close_active(&mut self, _: &CloseSession, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active {
            self.request_close(id, window, cx);
        }
    }

    fn request_close(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.iter().find(|s| s.entity_id() == id) else { return };
        let session = session.read(cx);
        if !session.is_running() {
            self.close_session(id, window, cx);
            return;
        }
        let detail = match &session.workspace.isolation {
            Some(iso) => format!(
                "The Claude process will be stopped. Its worktree on {} is removed only if it has no changes or commits.",
                iso.branch
            ),
            None => "The Claude process will be stopped. You can pick the conversation back up later with `claude --resume`.".into(),
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Close {}?", session.label()),
            Some(&detail),
            &["Close Session", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update_in(cx, |this, window, cx| this.close_session(id, window, cx));
            }
        })
        .detach();
    }

    fn close_session(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.sessions.iter().position(|s| s.entity_id() == id) else { return };
        let session = self.sessions.remove(ix);
        self.session_subscriptions.remove(&id);
        // Dropping the run stops a server Claude Deck started.
        self.servers.remove(&id);
        if self.server_panel == Some(id) {
            self.server_panel = None;
        }
        let workspace = session.update(cx, |s, _| {
            s.kill();
            s.workspace.clone()
        });

        if self.active == Some(id) {
            match self.sessions.get(ix.min(self.sessions.len().saturating_sub(1))).cloned() {
                Some(next) => self.activate(&next, window, cx),
                None => {
                    self.active = None;
                    window.focus(&self.focus_handle);
                }
            }
        }

        if workspace.isolation.is_some() {
            let task = cx.background_executor().spawn(async move { worktree::cleanup(&workspace) });
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |this, cx| match result {
                    Cleanup::Removed { branch } => {
                        this.notify(NoticeKind::Info, format!("Removed the unused worktree for {branch}."), cx)
                    }
                    Cleanup::Kept { branch, dir, reason } => this.notify(
                        NoticeKind::Warning,
                        format!("Kept worktree {} (branch {branch}) because {reason}. Merge or delete it when you're done.", home_relative(&dir)),
                        cx,
                    ),
                    Cleanup::NotIsolated => {}
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn restart(&mut self, id: EntityId, cx: &mut Context<Self>) {
        if let Some(session) = self.sessions.iter().find(|s| s.entity_id() == id) {
            session.update(cx, |s, cx| s.restart(cx));
        }
    }

    /// Stop every Claude process and remove worktrees that hold no work.
    fn shutdown(&mut self, cx: &mut Context<Self>) {
        self.servers.clear();
        for session in self.sessions.drain(..) {
            let workspace = session.update(cx, |s, _| {
                s.kill();
                s.workspace.clone()
            });
            worktree::cleanup(&workspace);
        }
    }

    // ---- accounts ---------------------------------------------------------

    fn refresh_accounts(&self) {
        let _ = self.refresh_accounts_tx.try_send(());
    }

    /// The session's account with its latest login status.
    fn account_of(&self, session: &TerminalSession) -> Account {
        self.accounts.iter().find(|a| a.id == session.account.id).cloned().unwrap_or_else(|| session.account.clone())
    }

    fn toggle_account_menu(&mut self, id: EntityId, cx: &mut Context<Self>) {
        let was_open = self.account_menu == Some(id) || self.recently_closed_menu();
        self.theme_menu = None;
        self.git_menu = None;
        self.device_menu = false;
        self.account_menu = if was_open { None } else { Some(id) };
        if self.account_menu.is_some() {
            self.refresh_accounts();
        }
        cx.notify();
    }

    /// The click that closed a menu (mouse-down outside) shouldn't reopen it.
    fn recently_closed_menu(&self) -> bool {
        self.menu_closed_at.is_some_and(|t| t.elapsed() < Duration::from_millis(300))
    }

    fn close_menus(&mut self, cx: &mut Context<Self>) {
        let had_menu = self.account_menu.take().is_some()
            | self.theme_menu.take().is_some()
            | self.git_menu.take().is_some()
            | std::mem::take(&mut self.device_menu);
        if had_menu {
            self.menu_closed_at = Some(Instant::now());
            cx.notify();
        }
    }

    fn toggle_theme_menu(&mut self, id: EntityId, cx: &mut Context<Self>) {
        let was_open = self.theme_menu == Some(id) || self.recently_closed_menu();
        self.account_menu = None;
        self.git_menu = None;
        self.device_menu = false;
        self.theme_menu = if was_open { None } else { Some(id) };
        cx.notify();
    }

    fn toggle_git_menu(&mut self, id: EntityId, cx: &mut Context<Self>) {
        let was_open = self.git_menu == Some(id) || self.recently_closed_menu();
        self.account_menu = None;
        self.theme_menu = None;
        self.device_menu = false;
        self.git_menu = if was_open { None } else { Some(id) };
        cx.notify();
    }

    fn request_switch(&mut self, id: EntityId, account_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.close_menus(cx);
        let Some(session) = self.sessions.iter().find(|s| s.entity_id() == id).cloned() else { return };
        let Some(target) = self.accounts.iter().find(|a| a.id == account_id).cloned() else { return };
        let (from, cwd) = {
            let s = session.read(cx);
            if s.account.id == account_id {
                return;
            }
            (s.account.clone(), s.workspace.cwd.clone())
        };
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("Switch this window to {}?", target.label()),
            Some("Claude restarts in this window under that account. You can bring the current conversation along, or start a new one."),
            &["Switch & Continue Conversation", "Switch & Start New", "Cancel"],
            cx,
        );
        let _ = (from, cwd);
        cx.spawn_in(window, async move |this, cx| {
            let carry = match answer.await {
                Ok(0) => true,
                Ok(1) => false,
                _ => return,
            };
            let _ = this.update(cx, |this, cx| this.perform_switch(&session, target, carry, cx));
        })
        .detach();
    }

    /// Restart `session` under `target`. Shared by the account menu and the smoke test.
    fn perform_switch(&mut self, session: &Entity<TerminalSession>, target: Account, carry: bool, cx: &mut Context<Self>) {
        let (from, cwd) = {
            let s = session.read(cx);
            (s.account.clone(), s.workspace.cwd.clone())
        };
        let resume = if carry { accounts::carry_conversation(&from, &target, &cwd) } else { None };
        let label = target.label();
        let found = resume.is_some();
        // The window's GitHub identity follows its folder, else its Claude account.
        let git = self.git_for(&target, &cwd);
        session.update(cx, |s, cx| s.restart_with(target, git, resume, cx));
        let msg = match (carry, found) {
            (true, true) => format!("Now using {label}. Your conversation was carried over."),
            (true, false) => format!("Now using {label}. No earlier conversation was found here, so a new one started."),
            _ => format!("Now using {label}."),
        };
        self.notify(NoticeKind::Info, msg, cx);
        self.refresh_accounts();
    }

    fn add_account(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        self.close_menus(cx);
        let Some(session) = self.sessions.iter().find(|s| s.entity_id() == id).cloned() else { return };
        let answer = window.prompt(
            PromptLevel::Info,
            "Add another Claude account?",
            Some("Claude restarts in this window with a new, empty profile. Sign in with the account you want when Claude asks (or type /login). Your other windows keep their accounts."),
            &["Add Account", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| match accounts::create_profile() {
                Ok(account) => {
                    this.accounts.push(account.clone());
                    session.update(cx, |s, cx| s.restart_with(account, None, None, cx));
                    this.notify(NoticeKind::Info, "New account profile created. Sign in inside the window.", cx);
                }
                Err(e) => this.notify(NoticeKind::Error, format!("Couldn't create account profile: {e}"), cx),
            });
        })
        .detach();
    }

    // ---- GitHub identity ----------------------------------------------------

    fn request_git_switch(&mut self, id: EntityId, github_user: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.close_menus(cx);
        let Some(session) = self.sessions.iter().find(|s| s.entity_id() == id).cloned() else { return };
        let git = github_user.and_then(|u| self.settings.git_identity(&u).cloned());
        if session.read(cx).git == git {
            return;
        }
        let label = git.as_ref().map(|g| g.github_user.clone()).unwrap_or_else(|| "your normal git settings".into());
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("Use {label} for GitHub in this window?"),
            Some("Claude restarts in this window and continues the same conversation, with commits, pushes and the gh CLI using this GitHub account."),
            &["Switch", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update(cx, |this, cx| {
                    let (account, cwd, project) = {
                        let s = session.read(cx);
                        (s.account.clone(), s.workspace.cwd.clone(), s.workspace.project.clone())
                    };
                    // Remember the choice for the folder, so its next windows
                    // use it too: in the repo's git config (in the background)
                    // and right away for every open window on the same repo.
                    let login = git.as_ref().map(|g| g.github_user.clone());
                    for s in &this.sessions {
                        let ws = &s.read(cx).workspace;
                        if ws.project == project {
                            if let Some(logins) = this.folder_logins.get_mut(&ws.cwd) {
                                logins.saved = login.clone();
                            }
                        }
                    }
                    let dir = cwd.clone();
                    cx.background_executor()
                        .spawn(async move { git_identity::save_folder_login(&dir, login.as_deref()) })
                        .detach();
                    let resume = accounts::latest_conversation(&account, &cwd);
                    session.update(cx, |s, cx| s.restart_with(account, git, resume, cx));
                    this.notify(NoticeKind::Info, format!("GitHub in this window: {label}, saved for this folder."), cx);
                });
            }
        })
        .detach();
    }

    // ---- themes -------------------------------------------------------------

    /// Set the default theme (app chrome, and every window without its own).
    fn apply_theme(&mut self, setting: ThemeSetting, cx: &mut Context<Self>) {
        match Self::load_theme(&setting) {
            Ok(theme) => {
                glass::match_theme(theme.is_dark, cx);
                cx.set_global(theme);
                self.settings.color_theme = Some(setting);
                self.settings.save();
                cx.refresh_windows();
            }
            Err(e) => self.notify(NoticeKind::Error, format!("Couldn't load theme: {e:#}"), cx),
        }
    }

    fn load_theme(setting: &ThemeSetting) -> anyhow::Result<Theme> {
        match builtin_theme(Some(setting)) {
            Some(t) => Ok(t),
            None => Theme::load_vscode(&setting.path, &setting.label, &setting.ui_theme),
        }
    }

    /// Give one window its own theme, or `None` to follow the default.
    fn set_window_theme(&mut self, id: EntityId, setting: Option<ThemeSetting>, cx: &mut Context<Self>) {
        self.close_menus(cx);
        let Some(session) = self.sessions.iter().find(|s| s.entity_id() == id).cloned() else { return };
        let theme = match setting.map(|s| Self::load_theme(&s).map(|t| (s, t))).transpose() {
            Ok(theme) => theme,
            Err(e) => return self.notify(NoticeKind::Error, format!("Couldn't load theme: {e:#}"), cx),
        };
        session.update(cx, |s, cx| {
            s.theme_override = theme;
            cx.notify();
        });
        cx.notify();
    }

    /// Warn (once) when two profiles are signed in to the same Claude
    /// account, which usually means the wrong account was picked at sign-in.
    fn warn_shared_logins(&mut self, cx: &mut Context<Self>) {
        let shared: Vec<(String, Vec<String>)> = accounts::shared_logins(&self.accounts)
            .into_iter()
            .map(|(email, list)| (email, list.iter().map(|a| a.profile_name()).collect()))
            .collect();
        self.warned_shared_logins.retain(|e| shared.iter().any(|(s, _)| s == e));
        for (email, names) in shared {
            if self.warned_shared_logins.contains(&email) {
                continue;
            }
            self.warned_shared_logins.push(email.clone());
            self.notify(
                NoticeKind::Warning,
                format!(
                    "{} are both signed in as {email}. To use another Claude account in one of them, run /logout and then /login in a window using it.",
                    names.join(" and ")
                ),
                cx,
            );
        }
    }

    fn notify(&mut self, kind: NoticeKind, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.notify_for(kind, message, NOTICE_TIME, cx);
    }

    /// A notice that stays up for `stay` (for ones with more to read).
    fn notify_for(&mut self, kind: NoticeKind, message: impl Into<SharedString>, stay: Duration, cx: &mut Context<Self>) {
        // Don't cover the welcome screen while it is still animating in.
        let hold = if self.sessions.is_empty() {
            (self.opened_at + WELCOME_ANIMATION_DONE).saturating_duration_since(Instant::now())
        } else {
            Duration::ZERO
        };
        self.notice = Some(Notice { kind, message: message.into(), shows_at: Instant::now() + hold });
        self.dismiss_notice_after(hold, stay, cx);
        cx.notify();
    }

    /// Show the notice after `hold`, then dismiss it `stay` later.
    fn dismiss_notice_after(&mut self, hold: Duration, stay: Duration, cx: &mut Context<Self>) {
        self.notice_timer = Some(cx.spawn(async move |this, cx| {
            if !hold.is_zero() {
                cx.background_executor().timer(hold).await;
                let _ = this.update(cx, |_, cx| cx.notify());
            }
            cx.background_executor().timer(stay).await;
            let _ = this.update(cx, |this, cx| {
                this.notice = None;
                cx.notify();
            });
        }));
    }

    // ---- view toggles -------------------------------------------------------

    fn toggle_right_panel(&mut self, cx: &mut Context<Self>) {
        self.right_panel_visible = !self.right_panel_visible;
        self.settings.right_panel_open = self.right_panel_visible;
        self.settings.save();
        cx.notify();
    }

    /// Add or remove the charge codes panel. Adding it also opens the right panel.
    fn toggle_charge_codes(&mut self, cx: &mut Context<Self>) {
        let open = !(self.settings.charge_codes_open && self.right_panel_visible);
        self.settings.charge_codes_open = open;
        if open && !self.right_panel_visible {
            self.right_panel_visible = true;
            self.settings.right_panel_open = true;
        }
        // Drop a half-typed charge code or section title.
        if !open && self.editing.as_ref().is_some_and(|e| e.target != EditTarget::AsanaToken) {
            self.editing = None;
        }
        self.settings.save();
        cx.notify();
    }

    /// Show or hide the Asana panel. Showing it also opens the right panel,
    /// and asks for a token the first time.
    fn toggle_asana_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = !self.asana_visible();
        self.settings.asana_panel_open = open;
        if open && !self.right_panel_visible {
            self.right_panel_visible = true;
            self.settings.right_panel_open = true;
        }
        self.settings.save();
        if open {
            if self.asana.config.is_none() && self.asana.choosing.is_none() && self.asana.busy.is_none() {
                self.start_asana_token(window, cx);
            } else if self.asana.loaded_at.is_none() {
                self.reload_asana(cx);
            }
        } else if self.editing(EditTarget::AsanaToken).is_some() {
            self.editing = None;
        }
        cx.notify();
    }

    fn asana_visible(&self) -> bool {
        self.settings.asana_panel_open && self.right_panel_visible
    }

    // ---- Asana ----------------------------------------------------------------

    /// Open the token field, with what's typed hidden.
    fn start_asana_token(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_edit(EditTarget::AsanaToken, "", "Paste your token", window, cx);
        if let Some(field) = self.editing(EditTarget::AsanaToken) {
            field.update(cx, |f, _| f.set_secret(true));
        }
    }

    /// Check a pasted token, then save it (or ask which workspace to show).
    fn connect_asana(&mut self, token: String, cx: &mut Context<Self>) {
        self.asana.busy = Some("Checking the token…");
        self.asana.error = None;
        self.asana.choosing = None;
        cx.notify();
        let check = token.clone();
        self.asana.task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { asana::me(&check) }).await;
            let _ = this.update(cx, |this, cx| {
                this.asana.busy = None;
                match result {
                    Ok(me) if me.workspaces.is_empty() => {
                        this.asana.error = Some("That account has no workspaces to show.".into());
                    }
                    Ok(me) if me.workspaces.len() == 1 => {
                        let workspace = me.workspaces[0].clone();
                        this.finish_asana_connect(token, me.name, me.gid, workspace, cx);
                    }
                    Ok(me) => this.asana.choosing = Some((token, me)),
                    Err(e) if asana::is_unauthorized(&e) => {
                        this.asana.error = Some("Asana didn't accept that token. Check it and paste it again.".into());
                    }
                    Err(e) => this.asana.error = Some(e.to_string().into()),
                }
                cx.notify();
            });
        }));
    }

    /// Keep the token in the Keychain and the workspace in asana.json, then load.
    fn finish_asana_connect(&mut self, token: String, user_name: String, user_gid: String, workspace: asana::Workspace, cx: &mut Context<Self>) {
        self.asana.choosing = None;
        self.asana.busy = Some("Saving to the Keychain…");
        cx.notify();
        let removing_old = self.asana.keychain.take();
        self.asana.task = Some(cx.spawn(async move |this, cx| {
            if let Some(removing) = removing_old {
                removing.await;
            }
            let saving = token.clone();
            let saved = cx.background_executor().spawn(async move { asana::save_token(&saving) }).await;
            let _ = this.update(cx, |this, cx| {
                this.asana.busy = None;
                // Carry on with the token anyway; it just won't survive a relaunch.
                if saved.is_err() {
                    this.asana.notice = Some(
                        "Connected, but the Keychain didn't keep the token (access may have been denied). \
                         You'll be asked for it again next launch."
                            .into(),
                    );
                }
                let config = asana::Config { workspace_gid: workspace.gid, user_gid };
                asana::save_config(&config);
                this.asana.user_name = user_name;
                this.asana.workspace_name = workspace.name;
                this.asana.config = Some(config);
                this.asana.token = Some(token);
                this.asana.needs_token = false;
                this.reload_asana(cx);
            });
        }));
    }

    /// Fetch the user's open tasks again.
    fn reload_asana(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.asana.config.clone() else { return };
        if self.asana.busy.is_some() || self.asana.needs_token {
            return;
        }
        self.asana.busy = Some(if self.asana.tickets.is_some() { "Refreshing…" } else { "Loading tasks…" });
        self.asana.error = None;
        cx.notify();
        let token = self.asana.token.clone();
        self.asana.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let token = token.or_else(asana::load_token)?;
                    let loaded = asana::load(&token, &config.workspace_gid);
                    Some((token, loaded))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.asana.busy = None;
                this.asana.loaded_at = Some(Instant::now());
                match result {
                    Some((token, Ok(loaded))) => {
                        this.asana.token = Some(token);
                        this.asana.tickets = Some(loaded.tickets);
                        this.asana.projects = Some(loaded.projects);
                        this.asana.user_name = loaded.user_name;
                        this.asana.workspace_name = loaded.workspace_name;
                        if let Some(config) = this.asana.config.as_mut().filter(|c| c.user_gid != loaded.user_gid) {
                            config.user_gid = loaded.user_gid;
                            asana::save_config(config);
                        }
                    }
                    Some((_, Err(e))) if asana::is_unauthorized(&e) => {
                        this.asana.token = None;
                        this.asana.needs_token = true;
                        this.asana.error = Some("Asana didn't accept the saved token. Paste a new one.".into());
                    }
                    Some((_, Err(e))) => this.asana.error = Some(format!("{e:#}").into()),
                    None => {
                        this.asana.needs_token = true;
                        this.asana.error = Some("The token isn't in the Keychain any more. Paste it again.".into());
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Forget the token, workspace and any alerts, and ask for a token again.
    fn restart_asana(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing(EditTarget::AsanaToken).is_some() {
            self.editing = None;
        }
        let keychain = cx.background_executor().spawn(async { asana::disconnect() });
        self.asana = AsanaPanel { keychain: Some(keychain), ..Default::default() };
        self.start_asana_token(window, cx);
        cx.notify();
    }

    /// Close the panel's error and notice lines.
    fn dismiss_asana_alerts(&mut self, cx: &mut Context<Self>) {
        self.asana.error = None;
        self.asana.notice = None;
        cx.notify();
    }

    // ---- charge codes ---------------------------------------------------------

    /// Open a text field for `target`, first saving any field already open.
    fn start_edit(&mut self, target: EditTarget, text: &str, placeholder: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(old) = self.editing.take() {
            let text = old.field.update(cx, |f, _| {
                f.close();
                f.text()
            });
            self.take_edit(old.target, text, cx);
        }
        let field = cx.new(|cx| TextField::new(text, placeholder.to_string(), window, cx));
        let events = cx.subscribe(&field, move |this, _, event: &TextFieldEvent, cx| {
            this.editing = None;
            if let TextFieldEvent::Submit(text) = event {
                this.take_edit(target, text.clone(), cx);
            }
            cx.notify();
        });
        self.editing = Some(Editing { target, field, _events: events });
        cx.notify();
    }

    /// Use the text a field was closed with.
    fn take_edit(&mut self, target: EditTarget, text: String, cx: &mut Context<Self>) {
        match target {
            EditTarget::AsanaToken if !text.is_empty() => self.connect_asana(text, cx),
            EditTarget::AsanaToken => {}
            _ => self.apply_edit(target, text),
        }
    }

    fn apply_edit(&mut self, target: EditTarget, text: String) {
        if text.is_empty() {
            return;
        }
        let sections = &mut self.charge_codes;
        match target {
            EditTarget::NewSection => sections.push(ChargeSection { title: text, codes: Vec::new() }),
            EditTarget::SectionTitle(i) => {
                if let Some(s) = sections.get_mut(i) {
                    s.title = text;
                }
            }
            EditTarget::NewCode(i) => {
                if let Some(s) = sections.get_mut(i) {
                    s.codes.push(text);
                }
            }
            EditTarget::Code(i, j) => {
                if let Some(code) = sections.get_mut(i).and_then(|s| s.codes.get_mut(j)) {
                    *code = text;
                }
            }
            EditTarget::AsanaToken => return,
        }
        charge_codes::save(&self.charge_codes);
    }

    fn editing(&self, target: EditTarget) -> Option<Entity<TextField>> {
        self.editing.as_ref().filter(|e| e.target == target).map(|e| e.field.clone())
    }

    fn copy_code(&mut self, i: usize, j: usize, cx: &mut Context<Self>) {
        let Some(code) = self.charge_codes.get(i).and_then(|s| s.codes.get(j)) else { return };
        cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
        self.copied = Some((i, j));
        self.copied_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(1500)).await;
            let _ = this.update(cx, |this, cx| {
                this.copied = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn delete_code(&mut self, i: usize, j: usize, cx: &mut Context<Self>) {
        self.editing = None;
        self.copied = None;
        if let Some(s) = self.charge_codes.get_mut(i) {
            if j < s.codes.len() {
                s.codes.remove(j);
                charge_codes::save(&self.charge_codes);
            }
        }
        cx.notify();
    }

    fn request_delete_section(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(section) = self.charge_codes.get(i) else { return };
        if section.codes.is_empty() {
            return self.delete_section(i, cx);
        }
        let n = section.codes.len();
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete \"{}\"?", section.title),
            Some(&format!("Its {n} charge code{} will be deleted too.", if n == 1 { "" } else { "s" })),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update(cx, |this, cx| this.delete_section(i, cx));
            }
        })
        .detach();
    }

    fn delete_section(&mut self, i: usize, cx: &mut Context<Self>) {
        self.editing = None;
        self.copied = None;
        if i < self.charge_codes.len() {
            self.charge_codes.remove(i);
            charge_codes::save(&self.charge_codes);
        }
        cx.notify();
    }

    fn show_view(&mut self, view: SidebarView, cx: &mut Context<Self>) {
        // Like VS Code: clicking the active view's icon collapses the side bar.
        if self.sidebar_visible && self.sidebar == view {
            self.sidebar_visible = false;
        } else {
            self.sidebar = view;
            self.sidebar_visible = true;
        }
        cx.notify();
    }
}

// ---- rendering --------------------------------------------------------------

impl Workbench {
    fn icon_button(
        &self,
        id: &'static str,
        name: &str,
        theme: &Theme,
        on_click: impl Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let hover = theme.list_hover_bg;
        div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(24.))
            .rounded(px(4.))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .child(icon(name, 16., theme.titlebar_fg))
            .on_click(cx.listener(on_click))
    }

    fn render_titlebar(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let accounts = self.usage_accounts(cx);
        let mut cards = div().flex().items_center().gap(px(8.));
        for account in accounts {
            cards = cards.child(self.render_usage_card(account, t));
        }
        let layout_icon = if self.layout == Layout::Grid { "tabs" } else { "grid" };
        div()
            .h(px(40.))
            .flex_none()
            .flex()
            .items_center()
            .bg(t.titlebar_bg)
            .border_b_1()
            .border_color(t.border)
            .text_color(t.titlebar_fg)
            .on_mouse_down(MouseButton::Left, |e, window, _| {
                if e.click_count == 2 {
                    window.titlebar_double_click();
                }
            })
            // Room for the window's close / minimise / zoom buttons.
            .child(div().w(px(84.)).flex_none())
            // App mark: the same icon as the welcome screen, then the name.
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(img(WELCOME_ICON.clone()).size(px(20.)).flex_none())
                    .child(
                        div()
                            .text_color(gpui::white())
                            .text_size(px(13.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Claude Deck"),
                    ),
            )
            .child(div().flex_1())
            .child(cards)
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .gap(px(2.))
                    .pr(px(8.))
                    .child(self.icon_button("tb-new", "plus", t, |this, _, w, cx| this.new_session(&NewSession, w, cx), cx))
                    .child(self.icon_button("tb-layout", layout_icon, t, |this, _, _, cx| this.toggle_layout(cx), cx))
                    .child(self.icon_button("tb-sidebar", "sidebar", t, |this, _, _, cx| {
                        this.sidebar_visible = !this.sidebar_visible;
                        cx.notify();
                    }, cx))
                    .child(self.icon_button("tb-right-panel", "sidebar-right", t, |this, _, _, cx| this.toggle_right_panel(cx), cx)),
            )
    }

    fn toggle_layout(&mut self, cx: &mut Context<Self>) {
        self.layout = if self.layout == Layout::Grid { Layout::Tabs } else { Layout::Grid };
        cx.notify();
    }

    fn render_activity_bar(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let item = |id: &'static str, graphic: &Arc<gpui::Image>, view: SidebarView, badge: Option<usize>, cx: &mut Context<Self>| {
            let active = self.sidebar_visible && self.sidebar == view;
            div()
                .id(id)
                .relative()
                .size(px(48.))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .group(id)
                .child(
                    img(graphic.clone())
                        .size(px(24.))
                        .flex_none()
                        .opacity(if active { 1. } else { 0.7 })
                        .group_hover(id, |s| s.opacity(1.)),
                )
                .when(active, |d| {
                    d.child(div().absolute().left_0().top_0().bottom_0().w(px(2.)).bg(t.activity_active_border))
                })
                .when_some(badge.filter(|n| *n > 0), |d, n| {
                    d.child(
                        div()
                            .absolute()
                            .bottom(px(8.))
                            .right(px(6.))
                            .min_w(px(16.))
                            .h(px(16.))
                            .px(px(4.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(t.button_bg)
                            .text_color(t.button_fg)
                            .text_size(px(11.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child(n.to_string()),
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| this.show_view(view, cx)))
        };
        let terminal_hover_fg = t.activity_fg;
        div()
            .w(px(48.))
            .flex_none()
            .flex()
            .flex_col()
            .bg(t.activity_bg)
            .border_r_1()
            .border_color(t.border)
            .child(item("act-sessions", &CLAUDE_ICON, SidebarView::Sessions, Some(self.sessions.len()), cx))
            .child(item("act-themes", &COLOR_BARS_ICON, SidebarView::Themes, None, cx))
            .child(div().mx(px(12.)).my(px(4.)).h(px(1.)).bg(t.border))
            .child(
                div()
                    .id("act-terminal")
                    .size(px(48.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .group("act-terminal")
                    .child(match self.ghostty_icon.clone() {
                        Some(path) => img(path)
                            .size(px(26.))
                            .flex_none()
                            .opacity(0.85)
                            .group_hover("act-terminal", |s| s.opacity(1.))
                            .into_any_element(),
                        None => icon("terminal", 24., t.activity_inactive_fg)
                            .group_hover("act-terminal", move |s| s.text_color(terminal_hover_fg))
                            .into_any_element(),
                    })
                    .on_click(cx.listener(|this, _, w, cx| this.new_terminal(&NewTerminal, w, cx))),
            )
            .child(self.render_webserver_button(t, cx))
            .children(self.browsers.iter().map(|b| {
                let browser = b.browser;
                let graphic = match b.icon.clone() {
                    Some(path) => img(path)
                        .size(px(26.))
                        .flex_none()
                        .opacity(0.85)
                        .group_hover(browser.id(), |s| s.opacity(1.))
                        .into_any_element(),
                    None => icon(browser.fallback_icon(), 22., t.activity_inactive_fg)
                        .group_hover(browser.id(), move |s| s.text_color(terminal_hover_fg))
                        .into_any_element(),
                };
                let button = div()
                    .id(browser.id())
                    .size(px(48.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .group(browser.id())
                    .child(graphic);
                if browser != Browser::Simulator {
                    return button.on_click(cx.listener(move |this, _, _, cx| this.open_in_browser(browser, cx))).into_any_element();
                }
                div()
                    .relative()
                    .child(button.on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_device_menu(cx);
                    })))
                    .when(self.device_menu, |d| {
                        d.child(deferred(
                            anchored()
                                .position_mode(AnchoredPositionMode::Local)
                                .position(point(px(48.), px(4.)))
                                .snap_to_window_with_margin(px(8.))
                                .child(self.render_device_menu(t, cx)),
                        ))
                    })
                    .into_any_element()
            }))
    }

    /// A side-bar section header; clicking it folds the section under
    /// `key` open or shut.
    fn section_header(&self, key: &str, title: &str, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let collapsed = self.collapsed.contains(key);
        let hover = t.list_hover_bg;
        let key = key.to_string();
        div()
            .id(SharedString::from(format!("section-{key}")))
            .h(px(22.))
            .flex()
            .items_center()
            .gap(px(2.))
            .px(px(4.))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .text_size(px(12.))
            .font_weight(gpui::FontWeight::BOLD)
            .text_color(t.sidebar_fg)
            .child(icon(if collapsed { "chevron-right" } else { "chevron-down" }, 16., t.sidebar_fg))
            .child(title.to_uppercase())
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_section(&key, cx)))
    }

    fn toggle_section(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(key) {
            self.collapsed.insert(key.to_string());
        }
        cx.notify();
    }

    fn render_sidebar(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        // White text in the side bar on dark themes (light themes keep their
        // own dark text, which white would make unreadable).
        let mut white_text = t.clone();
        if t.is_dark {
            white_text.sidebar_fg = gpui::white();
            white_text.fg = gpui::white();
            white_text.fg_muted = gpui::white().opacity(0.75);
        }
        let t = &white_text;
        let (title, body): (&str, AnyElement) = match self.sidebar {
            SidebarView::Sessions => ("Claude Sessions", self.render_sessions_view(t, cx).into_any_element()),
            SidebarView::Themes => ("Themes", self.render_themes_view(t, cx).into_any_element()),
        };
        let actions = div().flex().gap(px(2.)).when(self.sidebar == SidebarView::Sessions, |d| {
            d.child(self.icon_button("sb-new", "plus", t, |this, _, w, cx| this.new_session(&NewSession, w, cx), cx))
                .child(self.icon_button("sb-open", "folder-open", t, |this, _, w, cx| this.prompt_for_folder(w, cx), cx))
        });
        div()
            .w(px(270.))
            .flex_none()
            .flex()
            .flex_col()
            .bg(t.sidebar_bg)
            .text_color(t.sidebar_fg)
            .border_r_1()
            .border_color(t.border)
            .child(
                div()
                    .h(px(35.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pl(px(20.))
                    .pr(px(8.))
                    .text_size(px(12.))
                    .child(title.to_uppercase())
                    .child(actions),
            )
            .child(body)
    }

    fn primary_button(
        id: &'static str,
        label: &'static str,
        t: &Theme,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        let hover = t.button_hover_bg;
        div()
            .id(id)
            .h(px(26.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.))
            .bg(t.button_bg)
            .text_color(t.button_fg)
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .child(label)
            .on_click(on_click)
    }

    fn render_sessions_view(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let mut groups: Vec<(PathBuf, Vec<Entity<TerminalSession>>)> = Vec::new();
        for session in &self.sessions {
            let project = session.read(cx).workspace.project.clone();
            match groups.iter_mut().find(|(p, _)| *p == project) {
                Some((_, list)) => list.push(session.clone()),
                None => groups.push((project, vec![session.clone()])),
            }
        }

        let mut list = div().id("sessions-scroll").flex_1().overflow_y_scroll().flex().flex_col().pb(px(8.));

        if self.sessions.is_empty() && self.creating == 0 {
            let open = cx.listener(|this, _: &ClickEvent, w, cx| this.prompt_for_folder(w, cx));
            list = list.child(
                div()
                    .px(px(20.))
                    .py(px(10.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(div().text_color(t.fg_muted).child("No Claude sessions are running."))
                    .child(Self::primary_button("open-folder", "Open Folder", t, open)),
            );
        }

        let has_groups = !groups.is_empty();
        for (project, sessions) in groups {
            let key = format!("project:{}", project.display());
            let collapsed = self.collapsed.contains(&key);
            let hover = t.list_hover_bg;
            let count = sessions.len();
            list = list.child(
                div()
                    .id(SharedString::from(format!("section-{key}")))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(4.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(icon(if collapsed { "chevron-right" } else { "chevron-down" }, 16., t.sidebar_fg))
                    .child(div().flex_none().font_weight(gpui::FontWeight::BOLD).text_size(px(12.)).child(
                        project.file_name().map(|n| n.to_string_lossy().to_uppercase()).unwrap_or_default(),
                    ))
                    .child(div().flex_1().min_w(px(0.)).text_size(px(12.)).text_color(t.fg_muted).truncate().child(home_relative(&project)))
                    // Folded shut: how many sessions it hides.
                    .when(collapsed, |d| d.child(div().flex_none().pr(px(4.)).text_size(px(11.)).text_color(t.fg_muted).child(count.to_string())))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_section(&key, cx))),
            );
            if !collapsed {
                for session in sessions {
                    list = list.child(self.render_session_row(&session, t, cx));
                }
            }
            // A rule under each project's sessions, then room before the next.
            list = list.child(div().mt(px(6.)).mx(px(8.)).h(px(1.)).flex_none().bg(t.border)).child(div().h(px(25.)).flex_none());
        }

        if self.creating > 0 {
            list = list.child(
                div()
                    .h(px(22.))
                    .pl(px(24.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_color(t.fg_muted)
                    .child(icon("branch", 14., t.fg_muted))
                    .child("Creating worktree…"),
            );
        }

        let open_projects: Vec<PathBuf> = self.sessions.iter().map(|s| s.read(cx).workspace.project.clone()).collect();
        let recents: Vec<PathBuf> = self
            .settings
            .recent_projects
            .iter()
            .filter(|p| !open_projects.contains(p))
            .cloned()
            .collect();
        if !recents.is_empty() {
            if !has_groups {
                list = list.child(div().h(px(12.)));
            }
            list = list.child(self.section_header("recent", "Recent Projects", t, cx));
            let recents = if self.collapsed.contains("recent") { Vec::new() } else { recents };
            for (i, dir) in recents.into_iter().enumerate() {
                let hover = t.list_hover_bg;
                let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let path = home_relative(&dir);
                list = list.child(
                    div()
                        .id(("recent", i))
                        .h(px(22.))
                        .pl(px(24.))
                        .pr(px(8.))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .child(icon("folder", 14., t.sidebar_fg))
                        .child(div().flex_none().child(name))
                        .child(div().text_size(px(12.)).text_color(t.fg_muted).truncate().child(path))
                        .on_click(cx.listener(move |this, _, w, cx| this.open_recent(dir.clone(), w, cx))),
                );
            }
        }

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(list)
            .children(self.render_tip(
                "tip-isolation",
                "shield",
                "Extra sessions in the same repo each get their own git worktree and branch (deck/N), so they never write to the same files.",
                t,
                cx,
            ))
    }

    fn render_session_row(&self, session: &Entity<TerminalSession>, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let id = session.entity_id();
        let s = session.read(cx);
        let active = self.active == Some(id);
        let running = s.is_running();
        let detail = match &s.workspace.isolation {
            Some(iso) => iso.branch.clone(),
            None => s.branch.clone().map(|b| format!("{b} · main checkout")).unwrap_or_else(|| "main checkout".into()),
        };
        let hover = t.list_hover_bg;
        let group = SharedString::from(format!("session-row-{}", s.id));
        let account = self.account_of(s);
        let who = account.label().split('@').next().unwrap_or_default().to_string();
        let detail = format!("{who} · {detail}");
        div()
            .id(("session-row", s.id))
            .group(group.clone())
            .h(px(22.))
            .pl(px(24.))
            .pr(px(6.))
            .flex()
            .items_center()
            .gap(px(6.))
            .cursor_pointer()
            .when(active, |d| d.bg(t.list_active_bg))
            .hover(move |st| st.bg(hover))
            .child(div().flex_none().when(!running, |d| d.opacity(0.4)).child(Self::avatar(&account, 14.)))
            .child(div().flex_none().max_w(px(120.)).truncate().child(s.label()))
            .child(div().flex_1().truncate().text_size(px(12.)).text_color(t.fg_muted).child(detail))
            .child(
                div()
                    .id(("session-close", s.id))
                    .flex_none()
                    .rounded(px(3.))
                    .p(px(1.))
                    .invisible()
                    .when(active, |d| d.visible())
                    .group_hover(group, |st| st.visible())
                    .hover(move |st| st.bg(hover))
                    .child(icon("close", 14., t.sidebar_fg))
                    .on_click(cx.listener(move |this, _, w, cx| {
                        cx.stop_propagation();
                        this.request_close(id, w, cx);
                    })),
            )
            .on_click(cx.listener(move |this, _, w, cx| this.activate_id(id, w, cx)))
    }

    fn theme_row(
        &self,
        id: impl Into<gpui::ElementId>,
        label: String,
        source: String,
        selected: bool,
        t: &Theme,
        on_click: impl Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let hover = t.list_hover_bg;
        div()
            .id(id)
            .h(px(24.))
            .pl(px(16.))
            .pr(px(10.))
            .flex()
            .items_center()
            .gap(px(8.))
            .cursor_pointer()
            .when(selected, |d| d.bg(t.list_active_bg))
            .hover(move |s| s.bg(hover))
            .child(icon(if selected { "check" } else { "paint" }, 14., if selected { t.focus_border } else { t.fg_muted }))
            .child(div().flex_none().max_w(px(160.)).truncate().child(label))
            .child(div().flex_1().truncate().text_size(px(12.)).text_color(t.fg_muted).child(source))
            .on_click(cx.listener(on_click))
    }

    fn render_themes_view(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.settings.color_theme.clone();
        let mut body = div()
            .id("themes-scroll")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .pb(px(12.))
            .children(self.render_tip(
                "tip-themes",
                "info",
                "New windows use the default theme. To tell windows apart, give one its own theme with the palette button in its header.",
                t,
                cx,
            ))
            .child(self.section_header("themes", &format!("Default Theme ({})", self.themes.len()), t, cx));
        let shown = if self.collapsed.contains("themes") { 0 } else { self.themes.len() };
        for (i, choice) in self.themes.iter().enumerate().take(shown) {
            let selected = match &current {
                Some(c) => *c == choice.setting,
                None => i == 0,
            };
            let setting = choice.setting.clone();
            body = body.child(self.theme_row(
                ("theme", i),
                choice.setting.label.clone(),
                choice.source.clone(),
                selected,
                t,
                move |this, _, _, cx| this.apply_theme(setting.clone(), cx),
                cx,
            ));
        }
        body
    }

    /// Chip naming the window's GitHub account; opens the GitHub menu.
    fn render_git_chip(&self, session: &Entity<TerminalSession>, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let id = session.entity_id();
        let s = session.read(cx);
        let open = self.git_menu == Some(id);
        let label = s.git.as_ref().map(|g| g.github_user.clone()).unwrap_or_else(|| "git default".into());
        let hover = t.list_hover_bg;
        div()
            .relative()
            .flex_none()
            .child(
                div()
                    .id(("git-chip", s.id))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .h(px(20.))
                    .px(px(6.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(t.border)
                    .cursor_pointer()
                    .when(open, |d| d.bg(hover))
                    .hover(move |st| st.bg(hover))
                    .child(icon("github", 12., t.fg))
                    .child(div().max_w(px(130.)).truncate().text_size(px(12.)).text_color(t.fg).child(label))
                    .child(icon("chevron-down", 12., t.fg_muted))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_git_menu(id, cx);
                    })),
            )
            .when(open, |d| {
                d.child(deferred(
                    anchored()
                        .position_mode(AnchoredPositionMode::Local)
                        .position(point(px(0.), px(24.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(self.render_git_menu(session, t, cx)),
                ))
            })
    }

    /// Private repo, GitHub account not confirmed signed in yet: walk the
    /// user through signing in before anything is sent to GitHub.
    fn render_github_sign_in(&self, panel: &GithubSignIn, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let GithubSignIn { user, repo, profiles, chosen, opened, .. } = panel;
        let opened = *opened;
        let hover = t.list_hover_bg;
        let used_by = |dir: &str| {
            self.settings.chrome_profile_for_github.iter().find(|(u, d)| d.as_str() == dir && *u != user).map(|(u, _)| u.clone())
        };
        let step = |n: &str, text: String| {
            div()
                .px(px(12.))
                .pt(px(10.))
                .pb(px(4.))
                .flex()
                .gap(px(6.))
                .text_size(px(12.))
                .font_weight(gpui::FontWeight::BOLD)
                .child(div().flex_none().text_color(t.fg_muted).child(n.to_string()))
                .child(div().flex_1().min_w(px(0.)).child(text))
        };
        let note = |text: String| div().px(px(12.)).pt(px(4.)).text_size(px(12.)).text_color(t.fg_muted).child(text);
        let button = |eid: &'static str, label: String, primary: bool, enabled: bool| {
            let hover_bg = if primary { t.button_hover_bg } else { hover };
            div()
                .id(eid)
                .mx(px(12.))
                .mt(px(4.))
                .h(px(26.))
                .px(px(10.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .text_size(px(12.))
                .when(primary, |d| d.bg(t.button_bg).text_color(t.button_fg))
                .when(!primary, |d| d.border_1().border_color(t.border))
                .when(!enabled, |d| d.opacity(0.4))
                .when(enabled, |d| d.cursor_pointer().hover(move |s| s.bg(hover_bg)))
                .child(label)
        };
        let mut menu = div()
            .id("github-sign-in")
            .occlude()
            .w(px(360.))
            .py(px(8.))
            .rounded(px(6.))
            .bg(t.notification_bg)
            .border_1()
            .border_color(t.border)
            .shadow_lg()
            .text_color(t.fg)
            // Clicks inside don't reach the pane header.
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(icon("github", 14., t.fg))
                    .child(div().flex_1().min_w(px(0.)).font_weight(gpui::FontWeight::BOLD).child(format!("Sign in to GitHub as {user}")))
                    .child(
                        div()
                            .id("github-sign-in-close")
                            .flex_none()
                            .p(px(2.))
                            .rounded(px(3.))
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover))
                            .child(icon("close", 12., t.fg_muted))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.github_sign_in = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(note(format!(
                "{repo} is private, so GitHub shows a 404 unless the browser is signed in as {user}. \
                 Claude Deck can't see your browser's login, so set it up once:"
            )))
            .child(step("1", format!("Chrome profile for {user}")));
        let options = profiles
            .iter()
            .map(|p| (Some(p.dir.clone()), p.name.clone(), if p.email.is_empty() { "Chrome profile".to_string() } else { p.email.clone() }))
            .chain(std::iter::once((None, "New Chrome profile".to_string(), format!("Recommended: a separate profile only for {user}"))));
        for (i, (dir, title, detail)) in options.enumerate() {
            let selected = *chosen == dir;
            let new = dir.is_none();
            let taken = dir.as_deref().and_then(used_by);
            let detail = match &taken {
                Some(other) => format!("{detail} · used for {other}"),
                None => detail,
            };
            menu = menu.child(
                div()
                    .id(("github-profile", i))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(12.))
                    .py(px(4.))
                    .when(selected, |d| d.bg(t.list_active_bg))
                    .when(taken.is_some(), |d| d.opacity(0.4))
                    .when(taken.is_none(), |d| {
                        d.cursor_pointer().hover(move |st| st.bg(hover)).on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(panel) = this.github_sign_in.as_mut() {
                                panel.chosen = dir.clone();
                                panel.opened = false;
                            }
                            cx.notify();
                        }))
                    })
                    .child(icon(if new { "plus" } else { "globe" }, 16., t.fg))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(div().truncate().child(title))
                            .child(div().text_size(px(12.)).text_color(t.fg_muted).child(detail)),
                    )
                    .when(selected, |d| d.child(icon("check", 14., t.focus_border))),
            );
        }
        let confirm = button("github-confirm-sign-in", format!("I'm signed in as {user}, open the branch"), opened, opened);
        menu.child(step("2", format!("Sign in as {user} in that profile")))
            .child(
                button("github-open-sign-in", "Open GitHub sign-in in Chrome".into(), !opened, true)
                    .on_click(cx.listener(|this, _, _, cx| this.open_github_sign_in(cx))),
            )
            .child(note(format!(
                "If GitHub shows a dashboard instead of a sign-in form, that profile is already signed in, maybe as \
                 someone else: check the avatar menu, and use Add account to sign in as {user}."
            )))
            .child(step("3", format!("Back here once you're signed in as {user}")))
            .child(confirm.when(opened, |b| b.on_click(cx.listener(|this, _, _, cx| this.confirm_github_sign_in(cx)))))
    }

    fn render_git_menu(&self, session: &Entity<TerminalSession>, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let id = session.entity_id();
        let s = session.read(cx);
        let current = s.git.as_ref().map(|g| g.github_user.clone());
        let from_folder = self.folder_git(&s.workspace.cwd).map(|g| g.github_user.clone());
        let natural = from_folder.clone().or_else(|| self.git_for(&self.account_of(s), &s.workspace.cwd).map(|g| g.github_user));
        let hover = t.list_hover_bg;
        let mut menu = div()
            .id("git-menu")
            .occlude()
            .w(px(330.))
            .py(px(6.))
            .rounded(px(6.))
            .bg(t.notification_bg)
            .border_1()
            .border_color(t.border)
            .shadow_lg()
            .text_color(t.fg)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menus(cx)))
            .child(
                div()
                    .px(px(12.))
                    .pb(px(4.))
                    .text_size(px(12.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(t.fg_muted)
                    .child("GITHUB ACCOUNT FOR THIS WINDOW"),
            );
        let options: Vec<(Option<String>, String, String)> = self
            .settings
            .git_identities
            .iter()
            .map(|g| {
                let mut detail = format!("{} <{}>", g.name, g.email);
                if natural.as_deref() == Some(g.github_user.as_str()) {
                    detail.push_str(if from_folder.is_some() { " · saved for this folder" } else { " · default for this Claude account" });
                }
                (Some(g.github_user.clone()), g.github_user.clone(), detail)
            })
            .chain(std::iter::once((None, "Git default".to_string(), "Use your normal git settings and SSH config".to_string())))
            .collect();
        for (i, (user, title, detail)) in options.into_iter().enumerate() {
            let selected = current == user;
            menu = menu.child(
                div()
                    .id(("git-option", i))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(12.))
                    .py(px(6.))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(t.list_active_bg))
                    .hover(move |st| st.bg(hover))
                    .child(div().size(px(26.)).flex_none().flex().items_center().justify_center().child(icon("github", 18., t.fg)))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(div().truncate().child(title))
                            .child(div().text_size(px(12.)).text_color(t.fg_muted).child(detail)),
                    )
                    .when(selected, |d| d.child(icon("check", 16., t.focus_border)))
                    .on_click(cx.listener(move |this, _, w, cx| this.request_git_switch(id, user.clone(), w, cx))),
            );
        }
        menu
    }

    /// A dismissible side-bar tip; closed tips stay closed.
    fn render_tip(&self, key: &'static str, icon_name: &'static str, text: &'static str, t: &Theme, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.settings.dismissed_tips.iter().any(|k| k == key) {
            return None;
        }
        let hover = t.list_hover_bg;
        Some(
            div()
                .flex_none()
                .mx(px(10.))
                .my(px(8.))
                .p(px(10.))
                .rounded(px(4.))
                .border_1()
                .border_color(t.border)
                .flex()
                .items_start()
                .gap(px(8.))
                .text_size(px(12.))
                .text_color(t.fg_muted)
                .child(icon(icon_name, 16., t.fg_muted))
                .child(div().flex_1().min_w(px(0.)).child(text))
                .child(
                    div()
                        .id(key)
                        .flex_none()
                        .p(px(2.))
                        .rounded(px(3.))
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .child(icon("close", 12., t.fg_muted))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.settings.dismissed_tips.push(key.to_string());
                            this.settings.save();
                            cx.notify();
                        })),
                ),
        )
    }

    /// Per-window theme picker, opened from the palette button.
    fn render_theme_menu(&self, session: &Entity<TerminalSession>, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let id = session.entity_id();
        let own = session.read(cx).theme_override.as_ref().map(|(s, _)| s.clone());
        let default_name = cx.global::<Theme>().name.clone();
        let mut list = div()
            .id("theme-menu-list")
            .max_h(px(360.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .child(self.theme_row(
                "theme-default",
                format!("Default ({default_name})"),
                String::new(),
                own.is_none(),
                t,
                move |this, _, _, cx| this.set_window_theme(id, None, cx),
                cx,
            ))
            .child(div().my(px(4.)).h(px(1.)).bg(t.border));
        for (i, choice) in self.themes.iter().enumerate() {
            let setting = choice.setting.clone();
            list = list.child(self.theme_row(
                ("window-theme", i),
                choice.setting.label.clone(),
                choice.source.clone(),
                own.as_ref() == Some(&choice.setting),
                t,
                move |this, _, _, cx| this.set_window_theme(id, Some(setting.clone()), cx),
                cx,
            ));
        }
        div()
            .id("theme-menu")
            .occlude()
            .w(px(320.))
            .py(px(6.))
            .rounded(px(6.))
            .bg(t.notification_bg)
            .border_1()
            .border_color(t.border)
            .shadow_lg()
            .text_color(t.fg)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menus(cx)))
            .child(
                div()
                    .px(px(12.))
                    .pb(px(4.))
                    .text_size(px(12.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(t.fg_muted)
                    .child("THEME FOR THIS WINDOW"),
            )
            .child(list)
    }

    /// Palette button in a window's header that opens its theme menu.
    fn render_theme_button(&self, session: &Entity<TerminalSession>, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let id = session.entity_id();
        let s = session.read(cx);
        let open = self.theme_menu == Some(id);
        let has_own = s.theme_override.is_some();
        let hover = t.list_hover_bg;
        div()
            .relative()
            .flex_none()
            .child(
                div()
                    .id(("theme-button", s.id))
                    .p(px(3.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .when(open || has_own, |d| d.bg(hover))
                    .hover(move |st| st.bg(hover))
                    .child(icon("paint", 14., if has_own { t.tab_active_border_top } else { t.tab_inactive_fg }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_theme_menu(id, cx);
                    })),
            )
            .when(open, |d| {
                d.child(deferred(
                    anchored()
                        .position_mode(AnchoredPositionMode::Local)
                        .position(point(px(0.), px(24.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(self.render_theme_menu(session, t, cx)),
                ))
            })
    }

    /// One account's usage card for the top bar.
    /// The accounts open windows run as, once per Claude login: profiles
    /// signed in to the same account share one card, showing whichever
    /// recorded usage most recently.
    fn usage_accounts(&self, cx: &App) -> Vec<&Account> {
        let in_use: Vec<String> = self.sessions.iter().map(|s| s.read(cx).account.id.clone()).collect();
        let updated = |a: &Account| self.usage.get(&a.id).map_or(i64::MIN, |u| u.updated_at);
        let mut shown: Vec<(String, &Account)> = Vec::new();
        for account in self.accounts.iter().filter(|a| in_use.contains(&a.id)) {
            let login = self.email_of(account).unwrap_or_else(|| account.id.clone());
            match shown.iter_mut().find(|(l, _)| *l == login) {
                Some((_, kept)) if updated(account) > updated(kept) => *kept = account,
                Some(_) => {}
                None => shown.push((login, account)),
            }
        }
        shown.into_iter().map(|(_, a)| a).collect()
    }

    fn render_usage_card(&self, account: &Account, t: &Theme) -> impl IntoElement {
        let now = usage::now();
        let usage = self.usage.get(&account.id);
        let name = account.label().split('@').next().unwrap_or_default().to_string();
        let meter = |label: &'static str, limit: Option<&usage::Limit>| {
            let (pct, reset) = match limit {
                Some(l) => (l.current(now), Some(usage::until(l.resets_at, now))),
                None => (0., None),
            };
            let color = if pct >= 85. {
                t.error_fg
            } else if pct >= 60. {
                gpui::hsla(0.11, 0.85, 0.55, 1.)
            } else {
                t.success_fg
            };
            div()
                .flex()
                .items_center()
                .gap(px(5.))
                .child(div().text_size(px(11.)).text_color(t.fg_muted).child(label))
                .child(
                    div()
                        .w(px(56.))
                        .h(px(5.))
                        .rounded_full()
                        .bg(t.fg.opacity(0.12))
                        .child(div().h_full().rounded_full().bg(color).w(px(56. * (pct / 100.).clamp(0., 1.)))),
                )
                .child(div().w(px(34.)).text_size(px(12.)).font_weight(gpui::FontWeight::BOLD).child(format!("{pct:.0}%")))
                .when_some(reset, |d, r| d.child(div().text_size(px(11.)).text_color(t.fg_muted).child(r)))
        };
        let card = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(28.))
            .pl(px(4.))
            .pr(px(10.))
            .rounded(px(14.))
            .border_1()
            .border_color(account.color().opacity(0.45))
            .bg(account.color().opacity(0.08))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(Self::avatar(account, 20.))
                    .child(div().text_size(px(13.)).font_weight(gpui::FontWeight::BOLD).max_w(px(110.)).truncate().child(name)),
            );
        match usage.filter(|u| u.five_hour.is_some() || u.seven_day.is_some()) {
            Some(u) => card.child(meter("5h", u.five_hour.as_ref())).child(meter("7d", u.seven_day.as_ref())),
            None => card.child(div().text_size(px(12.)).text_color(t.fg_muted).child("usage shows after Claude's next reply")),
        }
    }

    fn avatar(account: &Account, size: f32) -> AnyElement {
        if let Some(path) = account.avatar.clone() {
            return img(path).size(px(size)).flex_none().rounded_full().into_any_element();
        }
        div()
            .size(px(size))
            .flex_none()
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(account.color())
            .text_color(gpui::white())
            .text_size(px((size * 0.58).round()))
            .font_weight(gpui::FontWeight::BOLD)
            .child(account.initial())
            .into_any_element()
    }

    /// Colored chip naming the session's account; opens the switcher.
    fn render_account_chip(&self, session: &Entity<TerminalSession>, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let id = session.entity_id();
        let s = session.read(cx);
        let account = self.account_of(s);
        let color = account.color();
        let open = self.account_menu == Some(id);
        div()
            .relative()
            .flex_none()
            .child(
                div()
                    .id(("account-chip", s.id))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .h(px(20.))
                    .pl(px(2.))
                    .pr(px(6.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(color.opacity(if open { 1. } else { 0.55 }))
                    .bg(color.opacity(0.14))
                    .cursor_pointer()
                    .hover(move |st| st.bg(color.opacity(0.24)))
                    .child(Self::avatar(&account, 15.))
                    .child(div().max_w(px(200.)).truncate().text_size(px(12.)).text_color(t.fg).child(account.label()))
                    .child(icon("chevron-down", 12., t.fg_muted))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_account_menu(id, cx);
                    })),
            )
            .when(open, |d| {
                d.child(deferred(
                    anchored()
                        .position_mode(AnchoredPositionMode::Local)
                        .position(point(px(0.), px(24.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(self.render_account_menu(id, &account.id, t, cx)),
                ))
            })
    }

    fn render_account_menu(&self, session_id: EntityId, current: &str, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let hover = t.list_hover_bg;
        let mut menu = div()
            .id("account-menu")
            .occlude()
            .w(px(330.))
            .py(px(6.))
            .rounded(px(6.))
            .bg(t.notification_bg)
            .border_1()
            .border_color(t.border)
            .shadow_lg()
            .text_color(t.fg)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menus(cx)))
            .child(
                div()
                    .px(px(12.))
                    .pb(px(4.))
                    .text_size(px(12.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(t.fg_muted)
                    .child("ACCOUNT FOR THIS WINDOW"),
            );
        for (i, account) in accounts::one_per_login(&self.accounts, current).into_iter().enumerate() {
            let selected = account.id == current;
            let account_id = account.id.clone();
            menu = menu.child(
                div()
                    .id(("account-option", i))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(12.))
                    .py(px(6.))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(t.list_active_bg))
                    .hover(move |st| st.bg(hover))
                    .child(Self::avatar(account, 26.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(div().truncate().child(account.label()))
                            .child(div().truncate().text_size(px(12.)).text_color(t.fg_muted).child({
                                let is_default = self.settings.default_account_email.is_some()
                                    && self.email_of(account) == self.settings.default_account_email;
                                if is_default { format!("{} · default for new windows", account.detail()) } else { account.detail() }
                            })),
                    )
                    .when(selected, |d| d.child(icon("check", 16., t.focus_border)))
                    .on_click(cx.listener(move |this, _, w, cx| this.request_switch(session_id, account_id.clone(), w, cx))),
            );
        }
        menu.child(div().my(px(6.)).h(px(1.)).bg(t.border))
            .child(
                div()
                    .id("account-add")
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(12.))
                    .py(px(6.))
                    .cursor_pointer()
                    .hover(move |st| st.bg(hover))
                    .child(div().size(px(26.)).flex().items_center().justify_center().child(icon("plus", 16., t.fg)))
                    .child("Add another account…")
                    .on_click(cx.listener(move |this, _, w, cx| this.add_account(session_id, w, cx))),
            )
            .child(
                div()
                    .px(px(12.))
                    .pt(px(6.))
                    .text_size(px(12.))
                    .text_color(t.fg_muted)
                    .child("Each account is a separate Claude Code profile with its own login and history."),
            )
    }

    fn render_pane_header(&self, session: &Entity<TerminalSession>, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let id = session.entity_id();
        let s = session.read(cx);
        let active = self.active == Some(id);
        let hover = t.list_hover_bg;
        let badge = Self::badge_branch(s);
        let (sid, label) = (s.id, s.label());
        // With a GitHub `origin`, the badge links to the branch on github.com.
        let linked = self.folder_logins.get(&s.workspace.cwd).is_some_and(|l| l.web.is_some());
        let picker = self.github_sign_in.as_ref().filter(|p| p.session == id).map(|p| self.render_github_sign_in(p, t, cx));
        let chip = self.render_account_chip(session, t, cx);
        let theme_button = self.render_theme_button(session, t, cx);
        let git_chip = self.render_git_chip(session, t, cx);
        div()
            .id(("pane-header", sid))
            .relative()
            .h(px(30.))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(10.))
            .bg(if active { t.tab_active_bg } else { t.tab_inactive_bg })
            .text_color(if active { t.tab_active_fg } else { t.tab_inactive_fg })
            .border_b_1()
            .border_color(t.border)
            .when(active, |d| d.child(div().absolute().top_0().left_0().right_0().h(px(1.)).bg(t.tab_active_border_top)))
            .child(icon("claude", 14., if active { t.tab_active_fg } else { t.tab_inactive_fg }))
            .child(div().truncate().child(label))
            .child(
                div()
                    .relative()
                    .flex_none()
                    .child(
                        div()
                            .id(("pane-branch", sid))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(3.))
                            .px(px(6.))
                            .h(px(16.))
                            .rounded(px(8.))
                            .bg(t.badge_bg.opacity(0.5))
                            .text_size(px(11.))
                            .child(icon("branch", 10., t.badge_fg))
                            .child(badge)
                            .when(linked, |d| {
                                let hover = t.badge_bg;
                                d.cursor_pointer()
                                    .hover(move |st| st.bg(hover))
                                    .child(icon("external", 9., t.badge_fg))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.open_branch_link(id, cx);
                                    }))
                            }),
                    )
                    .when_some(picker, |d, picker| {
                        d.child(deferred(
                            anchored()
                                .position_mode(AnchoredPositionMode::Local)
                                .position(point(px(0.), px(20.)))
                                .snap_to_window_with_margin(px(8.))
                                .child(picker),
                        ))
                    }),
            )
            .child(div().flex_1())
            .child(theme_button)
            .child(git_chip)
            .child(chip)
            .child(
                div()
                    .id(("pane-close", sid))
                    .flex_none()
                    .p(px(2.))
                    .rounded(px(3.))
                    .cursor_pointer()
                    .hover(move |st| st.bg(hover))
                    .child(icon("close", 14., if active { t.tab_active_fg } else { t.tab_inactive_fg }))
                    .on_click(cx.listener(move |this, _, w, cx| {
                        cx.stop_propagation();
                        this.request_close(id, w, cx);
                    })),
            )
            .on_click(cx.listener(move |this, _, w, cx| this.activate_id(id, w, cx)))
    }

    fn render_exit_bar(&self, session: &Entity<TerminalSession>, t: &Theme, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let id = session.entity_id();
        let s = session.read(cx);
        let status = s.exit_status.clone()?;
        let link = |eid: (&'static str, usize), label: &'static str| {
            div().id(eid).cursor_pointer().text_color(t.focus_border).hover(|st| st.underline()).child(label)
        };
        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(12.))
                .h(px(30.))
                .bg(t.notification_bg)
                .border_b_1()
                .border_color(t.border)
                .text_size(px(13.))
                .child(icon("info", 14., t.fg_muted))
                .child(div().flex_1().truncate().child(status))
                .child(link(("pane-restart", s.id), "Restart").on_click(cx.listener(move |this, _, _, cx| this.restart(id, cx))))
                .child(link(("pane-dismiss", s.id), "Close").on_click(cx.listener(move |this, _, w, cx| this.close_session(id, w, cx)))),
        )
    }

    /// A session's terminal, redrawn only when that session changes rather
    /// than whenever anything else in the window does.
    fn terminal_view(session: &Entity<TerminalSession>) -> AnyView {
        AnyView::from(session.clone()).cached(StyleRefinement::default().size_full())
    }

    fn render_pane(&self, session: &Entity<TerminalSession>, _: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let id = session.entity_id();
        // Each window is drawn in its own theme so it's easy to spot.
        let window_theme = session.read(cx).theme(cx);
        let t = &window_theme;
        div()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .flex()
            .flex_col()
            .bg(t.term_bg)
            .child(self.render_pane_header(session, t, cx))
            .children(self.render_exit_bar(session, t, cx))
            .child(div().flex_1().min_h(px(0.)).child(Self::terminal_view(session)))
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                this.active = Some(id);
                cx.notify();
            }))
    }

    fn render_tabs(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let mut strip = div().id("tab-strip").h_full().flex_1().min_w(px(0.)).flex().overflow_x_scroll();
        for session in &self.sessions {
            let id = session.entity_id();
            let s = session.read(cx);
            let active = self.active == Some(id);
            let tt = s.theme(cx);
            let fg = if active { tt.tab_active_fg } else { tt.tab_inactive_fg };
            let group = SharedString::from(format!("tab-{}", s.id));
            let hover = t.list_hover_bg;
            strip = strip.child(
                div()
                    .id(("tab", s.id))
                    .group(group.clone())
                    .relative()
                    .h_full()
                    .min_w(px(120.))
                    .max_w(px(240.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .pl(px(12.))
                    .pr(px(6.))
                    .bg(if active { tt.tab_active_bg } else { tt.tab_inactive_bg })
                    .text_color(fg)
                    .cursor_pointer()
                    .when(active, |d| d.child(div().absolute().top_0().left_0().right_0().h(px(2.)).bg(tt.tab_active_border_top)))
                    .child(div().flex_none().when(!s.is_running(), |d| d.opacity(0.4)).child(Self::avatar(&self.account_of(s), 14.)))
                    .child(div().flex_1().truncate().child(s.label()))
                    .child(
                        div()
                            .id(("tab-close", s.id))
                            .p(px(2.))
                            .rounded(px(3.))
                            .invisible()
                            .when(active, |d| d.visible())
                            .group_hover(group, |st| st.visible())
                            .hover(move |st| st.bg(hover))
                            .child(icon("close", 14., fg))
                            .on_click(cx.listener(move |this, _, w, cx| {
                                cx.stop_propagation();
                                this.request_close(id, w, cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, _, w, cx| this.activate_id(id, w, cx))),
            );
            strip = strip.child(div().w(px(1.)).h_full().flex_none().bg(t.border));
        }

        let active = self.active_session().cloned().or_else(|| self.sessions.first().cloned());
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(35.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .bg(t.tabs_bg)
                    .border_b_1()
                    .border_color(t.border)
                    .child(strip)
                    .when_some(active.clone(), |d, session| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .px(px(8.))
                                .child(self.render_theme_button(&session, t, cx))
                                .child(self.render_git_chip(&session, t, cx))
                                .child(self.render_account_chip(&session, t, cx)),
                        )
                    }),
            )
            .when_some(active, |d, session| {
                let tt = session.read(cx).theme(cx);
                d.children(self.render_exit_bar(&session, &tt, cx))
                    .child(div().flex_1().min_h(px(0.)).bg(tt.term_bg).child(Self::terminal_view(&session)))
            })
    }

    fn render_grid(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let n = self.sessions.len();
        let cols = (n as f32).sqrt().ceil().max(1.) as usize;
        // Dividers are borders on the panes, not gaps over a filled
        // background, so see-through panes don't show the fill. They're
        // drawn in the neutral grey of Claude Code's own rules around the
        // prompt, thick enough to tell the panes apart at a glance.
        let divider = theme::hex(if t.is_dark { "#888888" } else { "#999999" }).unwrap_or(t.border);
        let mut grid = div().flex_1().min_h(px(0.)).flex().flex_col();
        for (i, row) in self.sessions.chunks(cols).enumerate() {
            let mut r = div().flex_1().min_h(px(0.)).flex().border_color(divider).when(i > 0, |d| d.border_t_2());
            for (j, session) in row.iter().enumerate() {
                r = r.child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .border_color(divider)
                        .when(j > 0, |d| d.border_l_2())
                        .child(self.render_pane(session, t, cx)),
                );
            }
            grid = grid.child(r);
        }
        grid
    }

    fn render_welcome(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        // The welcome card is black with white text in every theme; links use
        // the theme's button colour, the same as the side bar's Open Folder.
        let fg = gpui::white();
        let chip_bg = gpui::white().opacity(0.12);
        let link_color = t.button_bg;
        let link = |id: &'static str, icon_name: &'static str, label: &'static str, keys: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(8.))
                .py(px(3.))
                .cursor_pointer()
                .text_color(link_color)
                .hover(|s| s.underline())
                .child(icon(icon_name, 16., link_color))
                .child(label)
                .child(div().text_color(fg).text_size(px(13.)).child(keys))
        };
        let shortcut = |label: &'static str, keys: &'static str| {
            div()
                .flex()
                .justify_between()
                .gap(px(24.))
                .py(px(3.))
                .child(div().child(label))
                .child(div().px(px(6.)).rounded(px(3.)).bg(chip_bg).text_size(px(13.)).child(keys))
        };
        // The pane can be narrower than the content (side bar open, small
        // window). `min_w(0)` + a flex-sized column keep it inside the pane
        // instead of letting `justify_center` push its left edge off-screen.
        // (Not `w_full`: taffy then sizes the column from min-content text,
        // making it ~5x too tall and pushing the content above the window.)
        div()
            .flex_1()
            .min_w(px(0.))
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .px(px(32.))
            .bg(t.editor_bg)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(20.))
                    .flex_1()
                    .max_w(px(704.))
                    .child(
                        div().flex().justify_center().child(
                            img(WELCOME_ICON.clone())
                                .size(px(WELCOME_ICON_SIZE))
                                .flex_none()
                                .with_animation(
                                    "welcome-icon-fade",
                                    Animation::new(Duration::from_millis(700)).with_easing(ease_out_quint()),
                                    // Fade in while rising 8px; mt + mb stays 8px so
                                    // nothing below the icon moves.
                                    |el, delta| el.opacity(delta).mt(px(8. * (1. - delta))).mb(px(8. * delta)),
                                ),
                        ),
                    )
                    .child(
                        card(div())
                            .relative()
                            .flex()
                            .flex_col()
                            .gap(px(28.))
                            .p(px(32.))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .gap(px(4.))
                                    .child(fade_line(div().text_size(px(32.)).child("Claude Deck"), 0, 0))
                                    .child(fade_line(div().text_size(px(15.)).text_center().child("Run as many Claude Code sessions as you like, side by side."), 1, 1)),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap_x(px(64.))
                                    .gap_y(px(24.))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap(px(4.))
                                            .child(fade_line(div().text_size(px(16.)).pb(px(4.)).child("Start"), 2, 2))
                                            .child(fade_line(link("w-open", "folder-open", "Open Folder…", "⌘O").on_click(cx.listener(|this, _, w, cx| this.prompt_for_folder(w, cx))), 3, 3))
                                            .child(fade_line(link("w-themes", "paint", "Choose a Theme", "⇧⌘T").on_click(cx.listener(|this, _, _, cx| {
                                                this.sidebar = SidebarView::Themes;
                                                this.sidebar_visible = true;
                                                cx.notify();
                                            })), 4, 4)),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap(px(2.))
                                            // Same rows as the Start column beside it.
                                            .child(fade_line(div().text_size(px(16.)).pb(px(4.)).child("Shortcuts"), 5, 2))
                                            .child(fade_line(shortcut("New session in current project", "⌘T"), 6, 3))
                                            .child(fade_line(shortcut("Close session", "⌘W"), 7, 4))
                                            .child(fade_line(shortcut("Next / previous session", "⌥⌘→ / ⌥⌘←"), 8, 5))
                                            .child(fade_line(shortcut("Grid ↔ tabs", "⌘\\"), 9, 6))
                                            .child(fade_line(shortcut("Toggle side bar", "⌘B"), 10, 7)),
                                    ),
                            )
                            .child(fade_line(
                                div()
                                    .flex()
                                    .gap(px(8.))
                                    .text_size(px(13.))
                                    .child(icon("shield", 16., fg))
                                    // flex_1 + min_w(0) let the text shrink to the
                                    // card and wrap instead of running past it.
                                    .child(div().flex_1().min_w(px(0.)).child("Opening a second session in the same git repo puts it in its own worktree, so sessions never overwrite each other's files.")),
                                11,
                                8,
                            ))
                            // Load sparkle: after the icon has mostly faded in,
                            // a soft glow runs once clockwise around the border.
                            .child(div().absolute().top_0().left_0().size_full().with_animations(
                                "welcome-card-sparkle",
                                vec![Animation::new(SPARKLE_DELAY), Animation::new(SPARKLE_LAP).with_easing(ease_in_out)],
                                |el, ix, t| {
                                    if ix == 0 {
                                        return el;
                                    }
                                    el.child(
                                        gpui::canvas(|_, _, _| (), move |bounds, _, window, _| paint_sparkle(bounds, t, window))
                                            .size_full(),
                                    )
                                },
                            )),
                    ),
            )
    }

    fn render_right_panel(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(280.))
            .flex_none()
            .flex()
            .flex_col()
            .bg(t.sidebar_bg)
            .text_color(t.sidebar_fg)
            .border_l_1()
            .border_color(t.border)
            .when(self.settings.charge_codes_open, |d| {
                d.child(
                    div()
                        .h(px(35.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(2.))
                        .pl(px(20.))
                        .pr(px(8.))
                        .text_size(px(12.))
                        .child(div().flex_1().child("CHARGE CODES"))
                        .child(self.icon_button("cc-add-section", "plus", t, |this, _, w, cx| {
                            this.start_edit(EditTarget::NewSection, "", "Section title", w, cx)
                        }, cx))
                        .child(self.icon_button("cc-close", "close", t, |this, _, _, cx| this.toggle_charge_codes(cx), cx)),
                )
                .child(self.render_charge_codes(t, cx))
            })
            .when(self.settings.asana_panel_open, |d| d.child(self.render_asana_panel(t, cx)))
            .when(!self.settings.charge_codes_open && !self.settings.asana_panel_open, |d| {
                d.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .px(px(20.))
                        .text_size(px(11.))
                        .text_color(t.fg_muted)
                        .child(div().w_full().text_center().child("No panels showing. Add one below, or from View in the menu bar.")),
                )
            })
            .when(!self.settings.charge_codes_open, |d| {
                d.child(self.render_add_panel_row(
                    "cc-add",
                    icon("receipt", 14., t.fg_muted).into_any_element(),
                    "Add Charge Codes panel",
                    t,
                    cx.listener(|this, _, _, cx| this.toggle_charge_codes(cx)),
                ))
            })
            .when(!self.settings.asana_panel_open, |d| {
                d.child(self.render_add_panel_row(
                    "asana-add",
                    img(ASANA_LOGO.clone()).size(px(14.)).flex_none().into_any_element(),
                    "Add Asana panel",
                    t,
                    cx.listener(|this, _, w, cx| this.toggle_asana_panel(w, cx)),
                ))
            })
    }

    /// Bottom of the right panel: a way to add a panel that's turned off.
    fn render_add_panel_row(
        &self,
        id: &'static str,
        logo: AnyElement,
        label: &'static str,
        t: &Theme,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        let hover = t.list_hover_bg;
        div()
            .id(id)
            .flex_none()
            .h(px(30.))
            .pl(px(20.))
            .flex()
            .items_center()
            .gap(px(8.))
            .border_t_1()
            .border_color(t.border)
            .text_color(t.fg_muted)
            .cursor_pointer()
            .hover(move |st| st.bg(hover))
            .child(logo)
            .child(div().flex_1().min_w(px(0.)).truncate().child(label))
            .on_click(on_click)
    }

    /// The optional Asana panel: the user's open tasks, grouped by due date.
    /// Until it's connected, it walks the user through adding a token.
    fn render_asana_panel(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let a = &self.asana;
        let connected = !a.setting_up();

        let mut header = div()
            .h(px(35.))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.))
            .pl(px(20.))
            .pr(px(8.))
            .text_size(px(12.))
            .child("ASANA");
        header = header.child(div().flex_1());
        if connected {
            header = header
                .child(self.icon_button("asana-reload", "restart", t, |this, _, _, cx| this.reload_asana(cx), cx))
                .child(self.icon_button("asana-disconnect", "logout", t, |this, _, w, cx| this.restart_asana(w, cx), cx));
        }
        header = header.child(self.icon_button("asana-close", "close", t, |this, _, w, cx| this.toggle_asana_panel(w, cx), cx));

        let subtitle: SharedString = match connected {
            true if a.user_name.is_empty() => a.workspace_name.clone().into(),
            true => format!("{} · {}", a.user_name, a.workspace_name).into(),
            false => "See the tasks assigned to you, next to your charge codes.".into(),
        };
        let hero = div()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(6.))
            .px(px(20.))
            .pt(px(4.))
            .pb(px(12.))
            .child(img(ASANA_LOGO.clone()).size(px(40.)).flex_none())
            .child(div().font_weight(gpui::FontWeight::BOLD).child(if connected { "Your Asana" } else { "Connect Asana" }))
            .child(div().w_full().text_size(px(11.)).text_color(t.fg_muted).text_center().child(subtitle));

        // What's happening, or what went wrong (closable, with a way to start over).
        let status = match (a.busy, &a.error, &a.notice) {
            (Some(busy), _, _) => Some(div().flex_none().px(px(20.)).pb(px(8.)).text_size(px(11.)).text_color(t.fg_muted).child(busy)),
            (None, Some(text), _) => Some(self.asana_alert("asana-error", text.clone(), t.error_fg, true, t, cx)),
            (None, None, Some(text)) => Some(self.asana_alert("asana-notice", text.clone(), t.fg_muted, false, t, cx)),
            _ => None,
        };

        let body = if connected { self.render_asana_lists(t, cx).into_any_element() } else { self.render_asana_setup(t, cx).into_any_element() };

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(t.border)
            .child(header)
            .child(hero)
            .children(status)
            .child(body)
    }

    /// A small × that closes the Asana panel's alerts.
    fn small_close(&self, id: &'static str, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(id)
            .flex_none()
            .size(px(18.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.))
            .cursor_pointer()
            .hover(|st| st.bg(t.fg.opacity(0.15)))
            .child(icon("close", 12., t.fg_muted))
            .on_click(cx.listener(|this, _, _, cx| this.dismiss_asana_alerts(cx)))
    }

    /// An alert line in the Asana panel: the message, a ×, and for errors a
    /// "Start over" that clears the token and asks for a new one.
    fn asana_alert(&self, id: &'static str, text: SharedString, color: Hsla, start_over: bool, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .flex_none()
            .mx(px(12.))
            .mb(px(8.))
            .px(px(8.))
            .py(px(6.))
            .rounded(px(4.))
            .bg(color.opacity(0.1))
            .flex()
            .flex_col()
            .gap(px(4.))
            .text_size(px(11.))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(6.))
                    .child(div().flex_1().min_w(px(0.)).text_color(color).child(text))
                    .child(self.small_close(id, t, cx)),
            )
            .when(start_over, |d| {
                d.child(
                    div()
                        .id(SharedString::from(format!("{id}-restart")))
                        .text_color(t.focus_border)
                        .cursor_pointer()
                        .hover(|st| st.underline())
                        .child("Start over with a new token")
                        .on_click(cx.listener(|this, _, w, cx| this.restart_asana(w, cx))),
                )
            })
    }

    /// First run (or a rejected token): make a token, paste it, pick a workspace.
    fn render_asana_setup(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let hover = t.list_hover_bg;
        let step = |n: &'static str, text: &'static str| {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .flex_none()
                        .size(px(18.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(t.fg.opacity(0.1))
                        .text_size(px(10.))
                        .child(n),
                )
                .child(div().flex_1().min_w(px(0.)).child(text))
        };
        let mut col = div().id("asana-setup").flex_1().min_h(px(0.)).overflow_y_scroll().flex().flex_col().gap(px(10.)).px(px(20.)).pb(px(12.));

        if let Some((_, me)) = &self.asana.choosing {
            col = col.child(div().text_color(t.fg_muted).child("Which workspace should the panel show?"));
            for (i, ws) in me.workspaces.iter().enumerate() {
                col = col.child(
                    div()
                        .id(SharedString::from(format!("asana-ws-{i}")))
                        .px(px(10.))
                        .py(px(6.))
                        .rounded(px(4.))
                        .border_1()
                        .border_color(t.border)
                        .cursor_pointer()
                        .hover(move |st| st.bg(hover))
                        .child(ws.name.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let Some((token, me)) = this.asana.choosing.take() else { return };
                            let Some(ws) = me.workspaces.get(i).cloned() else { return };
                            this.finish_asana_connect(token, me.name, me.gid, ws, cx);
                        })),
                );
            }
            return col;
        }

        let link = div()
            .id("asana-token-link")
            .ml(px(26.))
            .text_color(t.focus_border)
            .cursor_pointer()
            .hover(|st| st.underline())
            .child("Open Asana's developer console ↗")
            .on_click(|_, _, _| {
                let _ = std::process::Command::new("open").arg(asana::TOKEN_URL).spawn();
            });
        let field = match self.editing(EditTarget::AsanaToken) {
            Some(field) => div().ml(px(26.)).child(field).into_any_element(),
            None => div()
                .id("asana-token-start")
                .ml(px(26.))
                .h(px(26.))
                .px(px(6.))
                .flex()
                .items_center()
                .rounded(px(3.))
                .border_1()
                .border_color(t.border)
                .bg(t.editor_bg)
                .text_color(t.fg_muted)
                .cursor_text()
                .child("Paste your token")
                .on_click(cx.listener(|this, _, w, cx| this.start_asana_token(w, cx)))
                .into_any_element(),
        };
        col.child(step("1", "Create a personal access token"))
            .child(div().ml(px(26.)).text_size(px(11.)).text_color(t.fg_muted).child("My apps → Personal access tokens → Create new token."))
            .child(link)
            .child(step("2", "Paste it here and press Return"))
            .child(field)
            .child(
                div()
                    .mt(px(4.))
                    .text_size(px(11.))
                    .text_color(t.fg_muted)
                    .child("The token stays in your macOS Keychain. The panel only reads your tasks."),
            )
    }

    /// A count in a circle, in the theme's badge colors.
    fn count_badge(n: usize, t: &Theme) -> impl IntoElement {
        div()
            .flex_none()
            .min_w(px(18.))
            .h(px(18.))
            .px(px(5.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(t.badge_bg)
            .text_color(t.badge_fg)
            .text_size(px(10.))
            .child(n.to_string())
    }

    /// An accordion heading: chevron, title, then the count of what's inside.
    fn asana_accordion(
        &self,
        id: &'static str,
        title: &'static str,
        count: Option<usize>,
        open: bool,
        t: &Theme,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let hover = t.list_hover_bg;
        div()
            .id(id)
            .flex_none()
            .h(px(28.))
            .pl(px(14.))
            .pr(px(12.))
            .flex()
            .items_center()
            .gap(px(4.))
            .cursor_pointer()
            .hover(move |st| st.bg(hover))
            .text_size(px(12.))
            .font_weight(gpui::FontWeight::BOLD)
            .child(icon(if open { "chevron-down" } else { "chevron-right" }, 16., t.sidebar_fg))
            .child(div().flex_1().child(title))
            .children(count.map(|n| Self::count_badge(n, t)))
            .on_click(cx.listener(move |this, _, _, cx| {
                on_click(this, cx);
                cx.notify();
            }))
    }

    /// My tasks and My projects, both closed until clicked.
    fn render_asana_lists(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let a = &self.asana;
        let hover = t.list_hover_bg;
        let mut list = div().id("asana-lists").flex_1().min_h(px(0.)).overflow_y_scroll().flex().flex_col().pb(px(8.));
        let empty = |text: &'static str| div().pl(px(34.)).pr(px(12.)).py(px(6.)).text_size(px(11.)).text_color(t.fg_muted).child(text);

        let tickets = a.tickets.as_deref();
        list = list.child(self.asana_accordion("asana-tasks-head", "My tasks", tickets.map(<[_]>::len), a.tasks_open, t, |this, _| {
            this.asana.tasks_open = !this.asana.tasks_open;
        }, cx));
        if a.tasks_open {
            match tickets {
                Some([]) => list = list.child(empty("Nothing is assigned to you right now.")),
                Some(all) => list = list.children(self.asana_task_rows(all.iter().collect(), "all", 34., t)),
                None => {}
            }
        }

        let projects = a.projects.as_deref();
        list = list.child(self.asana_accordion("asana-projects-head", "My projects", projects.map(<[_]>::len), a.projects_open, t, |this, _| {
            this.asana.projects_open = !this.asana.projects_open;
        }, cx));
        if a.projects_open {
            match projects {
                Some([]) => list = list.child(empty("You aren't in any projects.")),
                Some(all) => {
                    for (i, project) in all.iter().enumerate() {
                        let mine: Vec<&asana::Ticket> =
                            tickets.unwrap_or(&[]).iter().filter(|tk| tk.project_gids.contains(&project.gid)).collect();
                        let open = a.open_project.as_ref() == Some(&project.gid);
                        let (gid, url) = (project.gid.clone(), project.url.clone());
                        list = list.child(
                            div()
                                .id(SharedString::from(format!("asana-project-{i}")))
                                .h(px(28.))
                                .pl(px(30.))
                                .pr(px(12.))
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .cursor_pointer()
                                .hover(move |st| st.bg(hover))
                                .child(icon(if open { "chevron-down" } else { "chevron-right" }, 14., t.fg_muted))
                                .child(div().flex_1().min_w(px(0.)).truncate().child(project.name.clone()))
                                .child(
                                    div()
                                        .id(SharedString::from(format!("asana-project-open-{i}")))
                                        .flex_none()
                                        .size(px(20.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(3.))
                                        .hover(|st| st.bg(t.fg.opacity(0.15)))
                                        .child(icon("external", 12., t.fg_muted))
                                        .on_click(move |_, _, cx| {
                                            cx.stop_propagation();
                                            let _ = std::process::Command::new("open").arg(&url).spawn();
                                        }),
                                )
                                .child(Self::count_badge(mine.len(), t))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.asana.open_project = if this.asana.open_project.as_ref() == Some(&gid) { None } else { Some(gid.clone()) };
                                    cx.notify();
                                })),
                        );
                        if open {
                            if mine.is_empty() {
                                list = list.child(empty("None of your tasks are in this project.").pl(px(48.)));
                            } else {
                                list = list.children(self.asana_task_rows(mine, &format!("p{i}"), 48., t));
                            }
                        }
                    }
                }
                None => {}
            }
        }
        list
    }

    /// Tasks grouped by due date, indented `indent`. Clicking one opens it in Asana.
    fn asana_task_rows(&self, tickets: Vec<&asana::Ticket>, key: &str, indent: f32, t: &Theme) -> Vec<AnyElement> {
        let hover = t.list_hover_bg;
        let coral: Hsla = gpui::rgb(0xF06A6A).into();
        let mut rows = Vec::new();
        for bucket in Bucket::ALL {
            let in_bucket: Vec<_> = tickets.iter().filter(|tk| tk.bucket == bucket).collect();
            if in_bucket.is_empty() {
                continue;
            }
            let due_color = match bucket {
                Bucket::Overdue => t.error_fg,
                Bucket::Today => WARNING,
                Bucket::Upcoming | Bucket::NoDate => t.fg_muted,
            };
            rows.push(
                div()
                    .h(px(22.))
                    .mt(px(4.))
                    .pl(px(indent))
                    .pr(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(10.))
                    .text_color(t.fg_muted)
                    .child(div().size(px(6.)).rounded_full().bg(due_color))
                    .child(div().flex_1().font_weight(gpui::FontWeight::BOLD).child(bucket.title().to_uppercase()))
                    .child(in_bucket.len().to_string())
                    .into_any_element(),
            );
            for (j, ticket) in in_bucket.into_iter().enumerate() {
                let mut meta = div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(11.))
                    .text_color(t.fg_muted)
                    .child(div().flex_1().min_w(px(0.)).truncate().child(ticket.project.clone()));
                if !ticket.due.is_empty() {
                    meta = meta.child(div().flex_none().text_color(due_color).child(ticket.due.clone()));
                }
                let url = ticket.url.clone();
                rows.push(
                    div()
                        .id(SharedString::from(format!("asana-{key}-{bucket:?}-{j}")))
                        .pl(px(indent))
                        .pr(px(12.))
                        .py(px(6.))
                        .flex()
                        .items_start()
                        .gap(px(8.))
                        .cursor_pointer()
                        .hover(move |st| st.bg(hover))
                        .child(div().pt(px(1.)).child(icon("task", 15., t.fg_muted)))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .flex()
                                .flex_col()
                                .gap(px(3.))
                                .child(div().line_clamp(2).child(ticket.title.clone()))
                                .child(meta),
                        )
                        .children(ticket.kind.label().map(|label| {
                            div()
                                .flex_none()
                                .mt(px(1.))
                                .px(px(5.))
                                .rounded(px(3.))
                                .bg(t.fg.opacity(0.08))
                                .text_size(px(10.))
                                .text_color(if ticket.kind == asana::Kind::Approval { coral } else { t.fg_muted })
                                .child(label)
                        }))
                        .on_click(move |_, _, _| {
                            if !url.is_empty() {
                                let _ = std::process::Command::new("open").arg(&url).spawn();
                            }
                        })
                        .into_any_element(),
                );
            }
        }
        rows
    }

    fn render_charge_codes(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let hover = t.list_hover_bg;
        // Icon buttons sit on rows that already show the hover color.
        let hover_strong = t.fg.opacity(0.15);
        let field_row = |field: Entity<TextField>| div().px(px(12.)).py(px(3.)).child(field);
        let mut list = div().id("charge-codes").flex_1().min_h(px(0.)).overflow_y_scroll().flex().flex_col().pb(px(8.));

        if self.charge_codes.is_empty() && self.editing(EditTarget::NewSection).is_none() {
            let add = cx.listener(|this, _: &ClickEvent, w, cx| this.start_edit(EditTarget::NewSection, "", "Section title", w, cx));
            list = list.child(
                div()
                    .px(px(20.))
                    .py(px(10.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(div().text_color(t.fg_muted).child("Add a section, then the charge codes that go under it. Click a title or code to edit it."))
                    .child(Self::primary_button("cc-first-section", "Add Section", t, add)),
            );
        }

        for (i, section) in self.charge_codes.iter().enumerate() {
            // Sections are divided by a line.
            let mut block = div().flex().flex_col().py(px(6.)).when(i > 0, |d| d.border_t_1().border_color(t.border));

            block = match self.editing(EditTarget::SectionTitle(i)) {
                Some(field) => block.child(field_row(field)),
                None => {
                    let group = SharedString::from(format!("cc-section-{i}"));
                    let title = section.title.clone();
                    block.child(
                        div()
                            .id(group.clone())
                            .group(group.clone())
                            .h(px(26.))
                            .pl(px(20.))
                            .pr(px(8.))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(div().flex_1().truncate().font_weight(gpui::FontWeight::BOLD).child(section.title.clone()))
                            .child(
                                div()
                                    .id(SharedString::from(format!("cc-delete-section-{i}")))
                                    .flex_none()
                                    .rounded(px(3.))
                                    .p(px(2.))
                                    .cursor_pointer()
                                    .invisible()
                                    .group_hover(group, |st| st.visible())
                                    .hover(move |st| st.bg(hover_strong))
                                    .child(icon("trash", 14., t.sidebar_fg))
                                    .on_click(cx.listener(move |this, _, w, cx| {
                                        cx.stop_propagation();
                                        this.request_delete_section(i, w, cx);
                                    })),
                            )
                            .cursor_pointer()
                            .hover(move |st| st.bg(hover))
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.start_edit(EditTarget::SectionTitle(i), &title, "Section title", w, cx)
                            })),
                    )
                }
            };

            for (j, code) in section.codes.iter().enumerate() {
                if let Some(field) = self.editing(EditTarget::Code(i, j)) {
                    block = block.child(field_row(field));
                    continue;
                }
                let group = SharedString::from(format!("cc-code-{i}-{j}"));
                let copied = self.copied == Some((i, j));
                let value = code.clone();
                block = block.child(
                    div()
                        .id(group.clone())
                        .group(group.clone())
                        .h(px(24.))
                        .pl(px(20.))
                        .pr(px(8.))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .cursor_pointer()
                        .hover(move |st| st.bg(hover))
                        .child(div().flex_1().truncate().child(code.clone()))
                        .child(
                            div()
                                .id(SharedString::from(format!("cc-delete-{i}-{j}")))
                                .flex_none()
                                .rounded(px(3.))
                                .p(px(2.))
                                .invisible()
                                .group_hover(group, |st| st.visible())
                                .hover(move |st| st.bg(hover_strong))
                                .child(icon("close", 14., t.sidebar_fg))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.delete_code(i, j, cx);
                                })),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("cc-copy-{i}-{j}")))
                                .flex_none()
                                .rounded(px(3.))
                                .p(px(2.))
                                .hover(move |st| st.bg(hover_strong))
                                .child(if copied { icon("check", 14., t.success_fg) } else { icon("copy", 14., t.sidebar_fg) })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.copy_code(i, j, cx);
                                })),
                        )
                        .on_click(cx.listener(move |this, _, w, cx| {
                            this.start_edit(EditTarget::Code(i, j), &value, "Charge code", w, cx)
                        })),
                );
            }

            block = match self.editing(EditTarget::NewCode(i)) {
                Some(field) => block.child(field_row(field)),
                None => block.child(
                    div()
                        .id(SharedString::from(format!("cc-add-code-{i}")))
                        .h(px(24.))
                        .pl(px(20.))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .cursor_pointer()
                        .text_color(t.fg_muted)
                        .hover(move |st| st.bg(hover))
                        .child(icon("plus", 14., t.fg_muted))
                        .child("Add charge code")
                        .on_click(cx.listener(move |this, _, w, cx| {
                            this.start_edit(EditTarget::NewCode(i), "", "Charge code", w, cx)
                        })),
                ),
            };
            list = list.child(block);
        }

        if let Some(field) = self.editing(EditTarget::NewSection) {
            list = list.child(
                div()
                    .py(px(6.))
                    .when(!self.charge_codes.is_empty(), |d| d.border_t_1().border_color(t.border))
                    .child(field_row(field)),
            );
        }
        list
    }

    fn render_status_bar(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let item = |content: AnyElement| div().flex().items_center().gap(px(4.)).px(px(6.)).h_full().child(content);
        let mut left = div().flex().h_full();
        if let Some(session) = self.active_session() {
            let s = session.read(cx);
            let account = self.account_of(s);
            let session_id = session.entity_id();
            left = left.child(
                div()
                    .id("status-account")
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .px(px(6.))
                    .h_full()
                    .cursor_pointer()
                    .hover(|st| st.bg(gpui::white().opacity(0.08)))
                    .child(Self::avatar(&account, 13.))
                    .child(account.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_account_menu(session_id, cx);
                    })),
            );
            let (branch, isolated) = match &s.workspace.isolation {
                Some(iso) => (Some(iso.branch.clone()), true),
                None => (s.branch.clone(), false),
            };
            if let Some(branch) = branch {
                left = left.child(item(
                    div().flex().items_center().gap(px(4.)).child(icon("branch", 13., t.status_fg)).child(branch).into_any_element(),
                ));
            }
            left = left.child(item(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(icon(if isolated { "shield" } else { "folder" }, 13., t.status_fg))
                    .child(if isolated { format!("worktree {}", home_relative(&s.workspace.cwd)) } else { home_relative(&s.workspace.cwd) })
                    .into_any_element(),
            ));
        }
        let count = self.sessions.len();
        left = left.child(item(
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .child(icon("terminal", 13., t.status_fg))
                .child(format!("{count} session{}", if count == 1 { "" } else { "s" }))
                .into_any_element(),
        ));
        let theme_name = t.name.clone();
        div()
            .h(px(22.))
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .px(px(4.))
            .bg(t.status_bg)
            .text_color(t.status_fg)
            .border_t_1()
            .border_color(t.border)
            .text_size(px(13.))
            .child(left)
            .child(
                div()
                    .flex()
                    .h_full()
                    .child(item(div().flex().items_center().gap(px(4.)).child(icon("paint", 13., t.status_fg)).child(theme_name).into_any_element()))
                    .child(item(div().child("Claude Deck").into_any_element())),
            )
    }

    // ---- web server ------------------------------------------------------

    /// The webserver button: show or hide the active session's server
    /// dialog, or (first time) look at its folder and start a server.
    fn toggle_webserver(&mut self, cx: &mut Context<Self>) {
        self.close_menus(cx);
        let Some(session) = self.active_session().cloned() else {
            self.notify(NoticeKind::Info, "Open a Claude session first; its web server starts in the session's folder.", cx);
            return;
        };
        let id = session.entity_id();
        if self.servers.contains_key(&id) {
            self.server_panel = if self.server_panel == Some(id) { None } else { Some(id) };
            cx.notify();
            return;
        }
        let (pid, dir) = {
            let s = session.read(cx);
            (s.pid(), s.workspace.cwd.clone())
        };
        self.servers.insert(id, ServerRun::new(dir.clone()));
        self.server_panel = Some(id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let found = cx.background_executor().spawn(async move { ServerFound::look(pid, &dir) }).await;
            let _ = this.update(cx, |this, cx| this.server_found(id, found, cx));
        })
        .detach();
    }

    fn server_found(&mut self, id: EntityId, found: ServerFound, cx: &mut Context<Self>) {
        // The session may have closed (or the run been cancelled) meanwhile.
        let Some(run) = self.servers.get_mut(&id) else { return };
        match found {
            ServerFound::Nothing(why) => {
                let dir = run.dir.clone();
                self.servers.remove(&id);
                if self.server_panel == Some(id) {
                    self.server_panel = None;
                }
                self.notify(NoticeKind::Warning, format!("Nothing to serve in {}. {why}", home_relative(&dir)), cx);
                return;
            }
            ServerFound::Running { url, project, pages } => {
                run.port = url.rsplit(':').next().and_then(|p| p.trim_end_matches('/').parse().ok()).unwrap_or(0);
                run.url = url;
                run.project = project;
                run.pages = pages;
                run.external = true;
                run.phase = ServerPhase::Ready;
            }
            ServerFound::Project { project, port, port_owner, pages } => {
                run.port = port;
                run.url = project.url(port);
                run.port_owner = port_owner.map(|who| (project.framework.default_port(), who));
                run.pages = pages;
                run.install = project.needs_install;
                let approved = self.settings.approved_servers.get(&run.dir).is_some_and(|c| *c == project.base_command().join(" "));
                let ask = !project.is_static() && (project.needs_install || !approved);
                run.project = Some(project);
                if !ask {
                    return self.start_server(id, cx);
                }
                run.phase = ServerPhase::NeedsApproval;
            }
        }
        cx.notify();
    }

    /// Start the server for session `id`: serve static files right away, or
    /// install dependencies (if ticked) and then run the dev server.
    fn start_server(&mut self, id: EntityId, cx: &mut Context<Self>) {
        let Some(run) = self.servers.get_mut(&id) else { return };
        let Some(project) = run.project.clone() else { return };
        if run.phase == ServerPhase::NeedsApproval {
            if !run.allow_run {
                return;
            }
            if run.remember {
                self.settings.approved_servers.insert(run.dir.clone(), project.base_command().join(" "));
                self.settings.save();
            }
        }
        let result = if project.is_static() {
            StaticServer::start(&project.root, run.port, &run.log).map(|server| {
                run.handle = Some(ServerHandle::Static(server));
                run.phase = ServerPhase::Ready;
            })
        } else if let Some(install) = project.install_command().filter(|_| run.install) {
            ServerProcess::spawn(&install, &project.root, None, &run.log).map(|process| {
                run.handle = Some(ServerHandle::Process(process));
                run.install_ran = true;
                run.phase = ServerPhase::Installing;
            })
        } else {
            run.spawn_dev()
        };
        if let Err(e) = result {
            run.phase = ServerPhase::Failed(format!("Couldn't start it: {e}").into());
        }
        run.started_at = Instant::now();
        if run.handle.as_ref().is_some_and(|h| matches!(h, ServerHandle::Process(_))) {
            run._watch = Some(cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(SERVER_TICK).await;
                // While starting, probe the port off the UI thread (each try
                // can wait up to 150ms) and hand the answer to the check.
                let probe = this.read_with(cx, |this, _| this.ready_probe(id)).ok().flatten();
                let ready = match probe {
                    Some((port, any_loopback)) => {
                        cx.background_executor().spawn(async move { webserver::is_listening(port, any_loopback) }).await
                    }
                    None => false,
                };
                if !this.update(cx, |this, cx| this.watch_server(id, ready, cx)).unwrap_or(false) {
                    break;
                }
            }));
        }
        cx.notify();
    }

    /// The port to probe and how, while a session's dev server is starting.
    fn ready_probe(&self, id: EntityId) -> Option<(u16, bool)> {
        let run = self.servers.get(&id).filter(|r| r.phase == ServerPhase::Starting)?;
        Some((run.port, run.project.as_ref()?.ready_on_any_loopback()))
    }

    /// One check of a server process: install finished, dev server
    /// answering (`ready`, probed off the UI thread), or stopped. Returns
    /// whether to keep watching.
    fn watch_server(&mut self, id: EntityId, ready: bool, cx: &mut Context<Self>) -> bool {
        let Some(run) = self.servers.get_mut(&id) else { return false };
        let generation = run.log.generation();
        let new_output = generation != run.log_seen;
        run.log_seen = generation;
        let before = run.phase.clone();
        let exited = run.process_exited();
        match run.phase {
            ServerPhase::Installing => match exited {
                Some(true) => {
                    if let Some(project) = run.project.as_mut() {
                        project.needs_install = false;
                    }
                    if let Err(e) = run.spawn_dev() {
                        run.phase = ServerPhase::Failed(format!("Couldn't start the dev server: {e}").into());
                    }
                }
                Some(false) => {
                    run.phase = ServerPhase::Failed("Installing dependencies failed. The log below says why.".into());
                }
                None => {}
            },
            ServerPhase::Starting => {
                if exited.is_some() {
                    run.phase = ServerPhase::Failed("The dev server stopped before it was ready.".into());
                } else if ready {
                    run.phase = ServerPhase::Ready;
                } else if run.started_at.elapsed() > SERVER_START_TIMEOUT {
                    run.handle = None; // stops it
                    run.phase = ServerPhase::Failed(
                        format!("The dev server didn't answer on port {} within 2 minutes.", run.port).into(),
                    );
                }
            }
            ServerPhase::Ready => {
                if exited.is_some() {
                    run.phase = ServerPhase::Failed("The dev server stopped.".into());
                }
            }
            _ => return false,
        }
        let changed = run.phase != before;
        let keep = !matches!(run.phase, ServerPhase::Failed(_));
        // Show the links (or the failure) when it's this window's session.
        if changed && matches!(run.phase, ServerPhase::Ready | ServerPhase::Failed(_)) && self.active == Some(id) {
            self.server_panel = Some(id);
        }
        if changed || (new_output && self.server_panel == Some(id)) {
            cx.notify();
        }
        keep
    }

    fn stop_server(&mut self, id: EntityId, cx: &mut Context<Self>) {
        if let Some(run) = self.servers.remove(&id) {
            let message = format!("Stopped the {} server on port {}.", run.tech(), run.port);
            drop(run);
            self.notify(NoticeKind::Info, message, cx);
        }
        if self.server_panel == Some(id) {
            self.server_panel = None;
        }
        cx.notify();
    }

    fn retry_server(&mut self, id: EntityId, cx: &mut Context<Self>) {
        self.servers.remove(&id);
        if self.active == Some(id) {
            self.toggle_webserver(cx);
        } else {
            self.server_panel = None;
            cx.notify();
        }
    }

    /// Open one of the dialog's page links, in `browser` or the default one.
    fn open_server_page(&mut self, url: String, browser: Option<Browser>, cx: &mut Context<Self>) {
        let app = browser.and_then(|b| self.browsers.iter().find(|i| i.browser == b)).map(|i| i.app.clone());
        let label = browser.map_or_else(|| "your default browser".to_string(), |b| b.label().to_string());
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let url = url.clone();
                    async move {
                        match app {
                            Some(app) => browser::open(&app, &url),
                            None => {
                                let status = std::process::Command::new("open").arg(&url).status()?;
                                anyhow::ensure!(status.success(), "`open` exited with {status}");
                                Ok(())
                            }
                        }
                    }
                })
                .await;
            if let Err(e) = result {
                let _ = this.update(cx, |this, cx| this.notify(NoticeKind::Warning, format!("Couldn't open {url} in {label}: {e}"), cx));
            }
        })
        .detach();
    }

    /// The activity bar's webserver button: a globe over a server, pulsing
    /// while a server builds, with a status dot once it's up.
    fn render_webserver_button(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let run = self.active.and_then(|id| self.servers.get(&id));
        let building = run.is_some_and(ServerRun::is_busy);
        let dot = match run.map(|r| &r.phase) {
            Some(ServerPhase::Ready) => Some(Hsla::from(gpui::rgb(SERVER_GREEN))),
            Some(ServerPhase::Failed(_)) => Some(t.error_fg),
            Some(ServerPhase::NeedsApproval) => Some(brand_orange()),
            _ => None,
        };
        let open = self.server_panel.is_some() && self.server_panel == self.active;
        let fg = t.activity_fg;
        let glyph = icon("web-server", 24., if open { fg } else { t.activity_inactive_fg });
        let graphic = if building {
            glyph
                .with_animation("webserver-pulse", Animation::new(Duration::from_millis(1600)).repeat(), |el, d| {
                    el.opacity(0.55 + 0.45 * (0.5 + 0.5 * (d * std::f32::consts::TAU).cos()))
                })
                .into_any_element()
        } else {
            glyph.group_hover("act-webserver", move |s| s.text_color(fg)).into_any_element()
        };
        div()
            .id("act-webserver")
            .relative()
            .size(px(48.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .group("act-webserver")
            .child(graphic)
            .when(open, |d| d.child(div().absolute().left_0().top_0().bottom_0().w(px(2.)).bg(t.activity_active_border)))
            .when_some(dot, |d, color| {
                d.child(
                    div()
                        .absolute()
                        .bottom(px(9.))
                        .right(px(9.))
                        .size(px(8.))
                        .rounded_full()
                        .bg(color)
                        .border_1()
                        .border_color(t.activity_bg),
                )
            })
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_webserver(cx);
            }))
    }

    /// A checkbox row in the web server dialog; clicking toggles a field of
    /// the session's run.
    fn server_check_row(
        &self,
        id: EntityId,
        el_id: &'static str,
        checked: bool,
        label: &str,
        code: Option<String>,
        note: Option<String>,
        toggle: fn(&mut ServerRun),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let orange = brand_orange();
        let check = div()
            .flex_none()
            .mt(px(1.))
            .size(px(15.))
            .rounded(px(4.))
            .border_1()
            .flex()
            .items_center()
            .justify_center()
            .map(|d| {
                if checked {
                    d.bg(orange).border_color(orange).child(icon("check", 11., gpui::white()))
                } else {
                    d.border_color(gpui::white().opacity(0.4))
                }
            });
        div()
            .id(el_id)
            .flex()
            .items_start()
            .gap(px(8.))
            .px(px(6.))
            .py(px(4.))
            .rounded(px(6.))
            .cursor_pointer()
            .hover(|s| s.bg(gpui::white().opacity(0.06)))
            .child(check)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(label.to_string())
                    .when_some(code, |d, c| {
                        d.child(div().font_family("Menlo").text_size(px(11.)).text_color(gpui::white().opacity(0.8)).child(c))
                    })
                    .when_some(note, |d, n| d.child(div().text_size(px(11.)).text_color(gpui::white().opacity(0.55)).child(n))),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(run) = this.servers.get_mut(&id) {
                    toggle(run);
                    cx.notify();
                }
            }))
            .into_any_element()
    }

    /// The last `lines` lines the server printed, in a dim monospace box.
    fn server_log_box(run: &ServerRun, lines: usize) -> impl IntoElement {
        let tail = run.log.tail(lines);
        let empty = tail.is_empty();
        div()
            .flex()
            .flex_col()
            .px(px(10.))
            .py(px(8.))
            .rounded(px(8.))
            .bg(gpui::white().opacity(0.05))
            .border_1()
            .border_color(gpui::white().opacity(0.1))
            .font_family("Menlo")
            .text_size(px(11.))
            .line_height(px(16.))
            .text_color(gpui::white().opacity(0.7))
            .children(tail.into_iter().map(|line| div().truncate().child(line)))
            .when(empty, |d| d.child(div().text_color(gpui::white().opacity(0.4)).child("Waiting for output…")))
    }

    /// The web server dialog: progress while it builds, checkboxes to
    /// approve the commands, then links to the project's pages.
    fn render_server_panel(&self, _t: &Theme, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let id = self.server_panel?;
        let run = self.servers.get(&id)?;
        let orange = brand_orange();
        let muted = gpui::white().opacity(0.6);
        let faint = gpui::white().opacity(0.12);
        let hover = gpui::white().opacity(0.12);
        let green: Hsla = gpui::rgb(SERVER_GREEN).into();
        let tech = run.tech();
        let folder = run.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

        let title = match &run.phase {
            ServerPhase::Checking => format!("Looking at {folder}…"),
            ServerPhase::NeedsApproval => format!("Start the {tech} dev server?"),
            ServerPhase::Installing => format!("Installing {tech} dependencies…"),
            ServerPhase::Starting => format!("Building {tech}…"),
            ServerPhase::Ready if run.external => format!("{} is already running", if run.project.is_some() { tech } else { "A server" }),
            ServerPhase::Ready => format!("{tech} is running"),
            ServerPhase::Failed(_) => format!("The {tech} server isn't running"),
        };
        let chip = match &run.project {
            Some(p) if run.port > 0 => format!("{} · :{}", p.tech_line(), run.port),
            Some(p) => p.tech_line(),
            None if run.port > 0 => format!(":{}", run.port),
            None => String::new(),
        };
        let button = |el_id: &'static str, label: &str, primary: bool, enabled: bool| {
            div()
                .id(el_id)
                .px(px(12.))
                .py(px(5.))
                .rounded(px(7.))
                .text_size(px(12.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .map(|d| if primary { d.bg(orange).text_color(gpui::white()) } else { d.border_1().border_color(gpui::white().opacity(0.25)) })
                .map(|d| if enabled { d.cursor_pointer().hover(|s| s.opacity(0.85)) } else { d.opacity(0.4) })
                .child(label.to_string())
        };
        let spinner = |key: &'static str| {
            icon("restart", 13., orange).with_animation(key, Animation::new(Duration::from_millis(900)).repeat(), |s, d| {
                s.with_transformation(Transformation::rotate(radians(d * std::f32::consts::TAU)))
            })
        };
        let log_toggle = div()
            .id("srv-log-toggle")
            .text_size(px(11.))
            .text_color(orange)
            .cursor_pointer()
            .hover(|s| s.opacity(0.8))
            .child(if run.show_log { "Show less" } else { "Show full log" })
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(run) = this.servers.get_mut(&id) {
                    run.show_log = !run.show_log;
                    cx.notify();
                }
            }));
        let log_lines = if run.show_log { 20 } else { 4 };

        let header = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(icon("web-server", 24., gpui::white()))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .child(div().font_weight(gpui::FontWeight::BOLD).child(title))
                    .child(div().text_size(px(11.)).text_color(muted).truncate().child(home_relative(&run.dir))),
            )
            .when(!chip.is_empty(), |d| {
                d.child(
                    div()
                        .flex_none()
                        .px(px(8.))
                        .py(px(2.))
                        .rounded_full()
                        .border_1()
                        .border_color(orange.opacity(0.7))
                        .text_size(px(11.))
                        .text_color(orange)
                        .child(chip),
                )
            })
            .child(
                div()
                    .id("srv-close")
                    .flex_none()
                    .p(px(2.))
                    .rounded(px(3.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(icon("close", 14., gpui::white()))
                    // Hides the dialog; the server keeps running.
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.server_panel = None;
                        cx.notify();
                    })),
            );

        let body: AnyElement = match &run.phase {
            ServerPhase::Checking => div()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_color(muted)
                .child(spinner("srv-spin-check"))
                .child(div().flex_1().min_w(px(0.)).child("Finding the framework and a free port…"))
                .into_any_element(),

            ServerPhase::NeedsApproval => {
                let project = run.project.as_ref()?;
                let start = project.start_command(run.port).join(" ");
                let mut rows = div().flex().flex_col().gap(px(2.));
                if let Some(install) = project.install_command() {
                    rows = rows.child(self.server_check_row(
                        id,
                        "srv-install",
                        run.install,
                        "Install dependencies first",
                        Some(install.join(" ")),
                        Some("node_modules is missing".into()),
                        |r| r.install = !r.install,
                        cx,
                    ));
                }
                rows = rows
                    .child(self.server_check_row(
                        id,
                        "srv-allow",
                        run.allow_run,
                        &format!("Run the {tech} dev server"),
                        Some(start),
                        None,
                        |r| r.allow_run = !r.allow_run,
                        cx,
                    ))
                    .child(self.server_check_row(
                        id,
                        "srv-remember",
                        run.remember,
                        "Don't ask again for this folder",
                        None,
                        Some("Installing dependencies still asks.".into()),
                        |r| r.remember = !r.remember,
                        cx,
                    ));
                let enabled = run.allow_run;
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(div().text_size(px(12.)).text_color(muted).child("Claude Deck runs these in the project folder:"))
                    .child(rows)
                    .when_some(run.port_owner.clone(), |d, (default, who)| {
                        d.child(
                            div()
                                .flex()
                                .items_start()
                                .gap(px(8.))
                                .text_size(px(12.))
                                .text_color(muted)
                                .child(icon("warning", 14., WARNING))
                                .child(div().flex_1().min_w(px(0.)).child(format!("Port {default} is in use by {who}, so this uses {}.", run.port))),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(button("srv-cancel", "Cancel", false, true).on_click(cx.listener(move |this, _, _, cx| {
                                this.servers.remove(&id);
                                this.server_panel = None;
                                cx.notify();
                            })))
                            .child(button("srv-start", "Start", true, enabled).on_click(cx.listener(move |this, _, _, cx| {
                                this.start_server(id, cx);
                            }))),
                    )
                    .into_any_element()
            }

            ServerPhase::Installing | ServerPhase::Starting => {
                let installing = run.phase == ServerPhase::Installing;
                let mut steps: Vec<(String, Option<bool>)> = Vec::new(); // None = now, Some(done)
                if run.install_ran {
                    steps.push(("Install dependencies".into(), if installing { None } else { Some(true) }));
                }
                steps.push((
                    format!("Start the {tech} dev server on port {}", run.port),
                    if installing { Some(false) } else { None },
                ));
                steps.push(("Wait for the first page".into(), Some(false)));
                let step_rows = steps.into_iter().enumerate().map(|(i, (label, state))| {
                    let mark = match state {
                        None => spinner(if i == 0 { "srv-spin-0" } else { "srv-spin-1" }).into_any_element(),
                        Some(true) => icon("check", 13., green).into_any_element(),
                        Some(false) => div()
                            .size(px(13.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(div().size(px(7.)).rounded_full().border_1().border_color(muted))
                            .into_any_element(),
                    };
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(12.))
                        .when(state == Some(false), |d| d.text_color(muted))
                        .child(mark)
                        .child(div().flex_1().min_w(px(0.)).child(label))
                });
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(div().flex().flex_col().gap(px(5.)).children(step_rows))
                    .child(Self::server_log_box(run, log_lines))
                    .child(div().flex().justify_between().items_center().child(log_toggle).child(
                        button("srv-stop-build", "Stop", false, true)
                            .on_click(cx.listener(move |this, _, _, cx| this.stop_server(id, cx))),
                    ))
                    .into_any_element()
            }

            ServerPhase::Ready => {
                let base = run.url.trim_end_matches('/').to_string();
                let link_browsers: Vec<&InstalledBrowser> =
                    self.browsers.iter().filter(|b| b.browser != Browser::Simulator).collect();
                let links = run.pages.iter().enumerate().map(|(i, page)| {
                    let url = format!("{base}{page}");
                    let open_url = url.clone();
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .child(
                            div()
                                .id(("srv-page", i))
                                .flex_1()
                                .min_w(px(0.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .px(px(10.))
                                .py(px(6.))
                                .rounded(px(8.))
                                .border_1()
                                .border_color(faint)
                                .bg(gpui::white().opacity(0.03))
                                .cursor_pointer()
                                .hover(move |s| s.bg(gpui::white().opacity(0.09)).border_color(orange))
                                .child(div().size(px(6.)).flex_none().rounded_full().bg(orange.opacity(0.8)))
                                .child(div().flex_1().min_w(px(0.)).truncate().font_family("Menlo").text_size(px(12.)).child(page.clone()))
                                .child(div().flex_none().text_color(muted).child("↗"))
                                .on_click(cx.listener(move |this, _, _, cx| this.open_server_page(open_url.clone(), None, cx))),
                        )
                        .children(link_browsers.iter().enumerate().map(|(b, installed)| {
                            let browser = installed.browser;
                            let url = url.clone();
                            let graphic = match installed.icon.clone() {
                                Some(path) => img(path).size(px(16.)).flex_none().into_any_element(),
                                None => icon(browser.fallback_icon(), 14., muted).into_any_element(),
                            };
                            div()
                                .id(("srv-open", i * 8 + b))
                                .flex_none()
                                .size(px(28.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(7.))
                                .cursor_pointer()
                                .opacity(0.8)
                                .hover(move |s| s.bg(hover).opacity(1.))
                                .child(graphic)
                                .on_click(cx.listener(move |this, _, _, cx| this.open_server_page(url.clone(), Some(browser), cx)))
                        }))
                });
                let names: Vec<&str> = self.browsers.iter().map(|b| b.browser.label()).collect();
                let tools = match names.as_slice() {
                    [] => "The browser buttons".to_string(),
                    [one] => format!("The {one} button"),
                    [rest @ .., last] => format!("The {} and {last} buttons", rest.join(", ")),
                };
                let mut footer = format!("{tools} in the activity bar now open this server.");
                if self.browsers.iter().any(|b| b.browser == Browser::Simulator) {
                    footer.push_str(" Device Hub shows it in Safari on an iPhone or iPad simulator.");
                }
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().size(px(8.)).rounded_full().bg(green))
                            .child(div().font_family("Menlo").text_size(px(12.)).text_color(gpui::white().opacity(0.85)).child(base.clone())),
                    )
                    .child(div().id("srv-links").max_h(px(260.)).overflow_y_scroll().flex().flex_col().gap(px(6.)).children(links))
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(8.))
                            .px(px(10.))
                            .py(px(8.))
                            .rounded(px(8.))
                            .bg(orange.opacity(0.12))
                            .text_size(px(12.))
                            .child(icon("phone", 14., orange))
                            .child(div().flex_1().min_w(px(0.)).child(footer)),
                    )
                    .when(run.show_log, |d| d.child(Self::server_log_box(run, log_lines)))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .map(|d| {
                                if run.external {
                                    d.child(div().text_size(px(11.)).text_color(muted).child("Started outside Claude Deck."))
                                } else {
                                    d.child(log_toggle).child(
                                        button("srv-stop", "Stop server", false, true)
                                            .on_click(cx.listener(move |this, _, _, cx| this.stop_server(id, cx))),
                                    )
                                }
                            }),
                    )
                    .into_any_element()
            }

            ServerPhase::Failed(message) => div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(8.))
                        .child(icon("warning", 14., WARNING))
                        .child(div().flex_1().min_w(px(0.)).child(message.clone())),
                )
                .child(Self::server_log_box(run, if run.show_log { 20 } else { 8 }))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(log_toggle)
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(button("srv-copy-log", "Copy log", false, true).on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(run) = this.servers.get(&id) {
                                        cx.write_to_clipboard(ClipboardItem::new_string(run.log.text()));
                                        this.notify(NoticeKind::Info, "Copied the server log.", cx);
                                    }
                                })))
                                .child(button("srv-retry", "Retry", true, true)
                                    .on_click(cx.listener(move |this, _, _, cx| this.retry_server(id, cx)))),
                        ),
                )
                .into_any_element(),
        };

        Some(
            card(div())
                .id("server-panel")
                .occlude()
                .w(px(420.))
                .flex()
                .flex_col()
                .gap(px(12.))
                .px(px(16.))
                .py(px(14.))
                .shadow_lg()
                .text_size(px(13.))
                .child(header)
                .child(body),
        )
    }

    fn render_notice(&self, t: &Theme, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let notice = self.notice.as_ref().filter(|n| Instant::now() >= n.shows_at)?;
        let (name, color) = match notice.kind {
            NoticeKind::Info => ("info", t.focus_border),
            NoticeKind::Warning => ("warning", WARNING),
            NoticeKind::Error => ("warning", t.error_fg),
        };
        let hover = gpui::white().opacity(0.12);
        Some(
            card(div())
                .id("notice")
                .occlude()
                // Stay up while the pointer is over it; go shortly after it leaves.
                .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                    if *hovered {
                        this.notice_timer = None;
                    } else if this.notice.is_some() {
                        this.dismiss_notice_after(Duration::ZERO, NOTICE_AFTER_HOVER, cx);
                    }
                }))
                .w(px(420.))
                .overflow_hidden()
                .flex()
                .items_start()
                .gap(px(10.))
                .px(px(14.))
                .py(px(12.))
                .shadow_lg()
                .text_size(px(13.))
                .child(icon(name, 16., color))
                .child(div().flex_1().min_w(px(0.)).child(notice.message.clone()))
                .child(
                    div()
                        .id("notice-close")
                        .flex_none()
                        .p(px(2.))
                        .rounded(px(3.))
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .child(icon("close", 14., gpui::white()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.notice = None;
                            cx.notify();
                        })),
                ),
        )
    }
}

impl Render for Workbench {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let glass = glass::enabled(cx);
        let t = Theme::current(cx);
        let editor: AnyElement = if self.sessions.is_empty() {
            self.render_welcome(&t, cx).into_any_element()
        } else if self.layout == Layout::Grid {
            self.render_grid(&t, cx).into_any_element()
        } else {
            self.render_tabs(&t, cx).into_any_element()
        };

        div()
            .id("workbench")
            .key_context("Workbench")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::new_session))
            .on_action(cx.listener(Self::new_terminal))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(Self::close_active))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_visible = !this.sidebar_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleRightPanel, _, cx| this.toggle_right_panel(cx)))
            .on_action(cx.listener(|this, _: &ToggleAsanaPanel, w, cx| this.toggle_asana_panel(w, cx)))
            .on_action(cx.listener(|this, _: &ToggleChargeCodes, _, cx| this.toggle_charge_codes(cx)))
            .on_action(cx.listener(|this, _: &ShowSessions, _, cx| this.show_view(SidebarView::Sessions, cx)))
            .on_action(cx.listener(|this, _: &ShowThemes, _, cx| this.show_view(SidebarView::Themes, cx)))
            .on_action(cx.listener(|this, _: &ToggleLayout, _, cx| this.toggle_layout(cx)))
            .on_action(cx.listener(|this, _: &NextSession, w, cx| this.cycle(1, w, cx)))
            .on_action(cx.listener(|this, _: &PreviousSession, w, cx| this.cycle(-1, w, cx)))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            // Over glass, each area paints its own see-through background.
            .when(!glass, |d| d.bg(t.editor_bg))
            .text_color(t.fg)
            .font_family(".SystemUIFont")
            // GPUI draws text without the thickening macOS gives small text,
            // so regular weight looks thin and fuzzy, worst on 1x displays.
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(px(13.))
            .child(self.render_titlebar(&t, cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(self.render_activity_bar(&t, cx))
                    .when(self.sidebar_visible, |d| d.child(self.render_sidebar(&t, cx)))
                    .child(div().flex_1().min_w(px(0.)).flex().flex_col().child(editor))
                    .when(self.right_panel_visible, |d| d.child(self.render_right_panel(&t, cx))),
            )
            .child(self.render_status_bar(&t, cx))
            // Lower right: the notice toast above the web server dialog.
            .child(
                div()
                    .absolute()
                    .bottom(px(32.))
                    .right(px(12.))
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(px(8.))
                    .children(self.render_notice(&t, cx))
                    .children(self.render_server_panel(&t, cx)),
            )
    }
}

impl Focusable for Workbench {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod sparkle_tests {
    use super::point_on_rounded_rect;
    use gpui::{Bounds, point, px, size};

    #[test]
    fn walks_clockwise_along_the_edge() {
        let b = Bounds { origin: point(px(0.), px(0.)), size: size(px(100.), px(50.)) };
        let r = 10.;
        let near = |(x, y): (f32, f32), (ex, ey): (f32, f32)| (x - ex).abs() < 0.01 && (y - ey).abs() < 0.01;
        assert!(near(point_on_rounded_rect(b, r, 0.), (10., 0.)));
        // End of the top edge, then down the right side after the corner arc.
        assert!(near(point_on_rounded_rect(b, r, 80.), (90., 0.)));
        let arc = std::f32::consts::FRAC_PI_2 * r;
        assert!(near(point_on_rounded_rect(b, r, 80. + arc), (100., 10.)));
        assert!(near(point_on_rounded_rect(b, r, 80. + arc + 30.), (100., 40.)));
        // A full lap returns to the start.
        let lap = 2. * (80. + 30.) + 4. * arc;
        assert!(near(point_on_rounded_rect(b, r, lap + 5.), (15., 0.)));
    }
}
