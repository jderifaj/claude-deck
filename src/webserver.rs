//! Start a web server for a session's folder: detect what the project is
//! (a framework with a dev script, a Hugo/Jekyll site, or plain static HTML),
//! pick a free port, and run it as a child process, or serve the static
//! files from a small built-in server.

use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::process::CommandExt,
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Framework {
    Astro,
    Next,
    Nuxt,
    SvelteKit,
    Remix,
    Gatsby,
    Vite,
    Eleventy,
    Node,
    Hugo,
    Jekyll,
    Static,
}

impl Framework {
    pub fn label(self) -> &'static str {
        match self {
            Framework::Astro => "Astro",
            Framework::Next => "Next.js",
            Framework::Nuxt => "Nuxt",
            Framework::SvelteKit => "SvelteKit",
            Framework::Remix => "Remix",
            Framework::Gatsby => "Gatsby",
            Framework::Vite => "Vite",
            Framework::Eleventy => "Eleventy",
            Framework::Node => "Node",
            Framework::Hugo => "Hugo",
            Framework::Jekyll => "Jekyll",
            Framework::Static => "Static HTML",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Framework::Astro => 4321,
            Framework::Next | Framework::Remix | Framework::Node => 3000,
            Framework::Nuxt => 3000,
            Framework::SvelteKit | Framework::Vite => 5173,
            Framework::Gatsby => 8000,
            Framework::Eleventy => 8080,
            Framework::Hugo => 1313,
            Framework::Jekyll => 4000,
            Framework::Static => 8080,
        }
    }

    /// Arguments that make the dev server use `port` and, where the tool
    /// allows it, listen on 127.0.0.1 only.
    fn port_args(self, port: u16) -> Vec<String> {
        let p = port.to_string();
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match self {
            Framework::Astro | Framework::Nuxt => v(&["--port", &p, "--host", "127.0.0.1"]),
            Framework::Vite | Framework::SvelteKit | Framework::Remix => {
                v(&["--port", &p, "--strictPort", "--host", "127.0.0.1"])
            }
            Framework::Next | Framework::Gatsby => v(&["-p", &p, "-H", "127.0.0.1"]),
            Framework::Eleventy => vec![format!("--port={p}")],
            Framework::Hugo => v(&["--port", &p, "--bind", "127.0.0.1"]),
            Framework::Jekyll => v(&["--port", &p, "--host", "127.0.0.1"]),
            // Node: PORT in the environment. Static: served in-process.
            Framework::Node | Framework::Static => Vec::new(),
        }
    }

    /// Whether the server is told to listen on 127.0.0.1 (else it picks its
    /// own address, which may be IPv6 only).
    fn binds_ipv4_loopback(self) -> bool {
        !matches!(self, Framework::Eleventy | Framework::Node)
    }
}

#[derive(Clone, Debug)]
pub struct Project {
    pub framework: Framework,
    /// Folder to run the command in, or (static) to serve.
    pub root: PathBuf,
    /// npm, pnpm, yarn or bun, for package.json projects.
    pub package_manager: Option<&'static str>,
    /// The package.json script that starts the dev server.
    pub script: Option<String>,
    /// package.json project without node_modules.
    pub needs_install: bool,
}

impl Project {
    pub fn is_static(&self) -> bool {
        self.framework == Framework::Static
    }

    /// "Astro · pnpm"
    pub fn tech_line(&self) -> String {
        match self.package_manager {
            Some(pm) => format!("{} · {pm}", self.framework.label()),
            None => self.framework.label().to_string(),
        }
    }

    pub fn install_command(&self) -> Option<Vec<String>> {
        if self.needs_install {
            return Some(vec![self.package_manager?.to_string(), "install".into()]);
        }
        None
    }

    /// The start command without port arguments, for showing and for
    /// remembering what the user approved.
    pub fn base_command(&self) -> Vec<String> {
        match (self.framework, self.package_manager, &self.script) {
            (Framework::Hugo, ..) => vec!["hugo".into(), "server".into()],
            (Framework::Jekyll, ..) => {
                ["bundle", "exec", "jekyll", "serve"].iter().map(|s| s.to_string()).collect()
            }
            (_, Some(pm), Some(script)) => vec![pm.to_string(), "run".into(), script.clone()],
            _ => Vec::new(),
        }
    }

    pub fn start_command(&self, port: u16) -> Vec<String> {
        let mut cmd = self.base_command();
        let args = self.framework.port_args(port);
        if !args.is_empty() {
            // npm needs `--` to pass arguments through to the script; pnpm,
            // yarn and bun pass them as-is.
            if self.package_manager == Some("npm") {
                cmd.push("--".into());
            }
            cmd.extend(args);
        }
        cmd
    }

    pub fn url(&self, port: u16) -> String {
        if self.framework.binds_ipv4_loopback() {
            format!("http://127.0.0.1:{port}")
        } else {
            format!("http://localhost:{port}")
        }
    }
}

/// What kind of project `dir` is, or why it can't be served.
pub fn detect(dir: &Path) -> Result<Project, String> {
    let package = dir.join("package.json");
    if let Some(json) = std::fs::read_to_string(&package).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
        let has_dep = |name: &str| {
            ["dependencies", "devDependencies"].iter().any(|k| json[k].get(name).is_some())
        };
        let script = ["dev", "start", "serve"].into_iter().find(|s| json["scripts"].get(*s).is_some_and(|v| v.is_string()));
        if let Some(script) = script {
            let framework = [
                ("astro", Framework::Astro),
                ("next", Framework::Next),
                ("nuxt", Framework::Nuxt),
                ("@sveltejs/kit", Framework::SvelteKit),
                ("@remix-run/dev", Framework::Remix),
                ("gatsby", Framework::Gatsby),
                ("vite", Framework::Vite),
                ("@11ty/eleventy", Framework::Eleventy),
            ]
            .into_iter()
            .find(|(dep, _)| has_dep(dep))
            .map_or(Framework::Node, |(_, f)| f);
            let package_manager = if dir.join("pnpm-lock.yaml").exists() {
                "pnpm"
            } else if dir.join("yarn.lock").exists() {
                "yarn"
            } else if dir.join("bun.lockb").exists() || dir.join("bun.lock").exists() {
                "bun"
            } else {
                "npm"
            };
            return Ok(Project {
                framework,
                root: dir.to_path_buf(),
                package_manager: Some(package_manager),
                script: Some(script.to_string()),
                needs_install: !dir.join("node_modules").is_dir(),
            });
        }
    }
    let hugo_config = ["hugo.toml", "hugo.yaml", "hugo.json", "config.toml"].iter().any(|f| dir.join(f).is_file());
    if hugo_config && dir.join("content").is_dir() && on_path("hugo") {
        return Ok(build_site(Framework::Hugo, dir));
    }
    if dir.join("_config.yml").is_file() && dir.join("Gemfile").is_file() && on_path("bundle") {
        return Ok(build_site(Framework::Jekyll, dir));
    }
    if let Some(root) = static_root(dir) {
        return Ok(Project { framework: Framework::Static, root, package_manager: None, script: None, needs_install: false });
    }
    if package.is_file() {
        return Err("Its package.json has no dev, start or serve script, and there's no index.html to serve.".into());
    }
    Err("It has no package.json with a dev script and no HTML files to serve.".into())
}

fn build_site(framework: Framework, dir: &Path) -> Project {
    Project { framework, root: dir.to_path_buf(), package_manager: None, script: None, needs_install: false }
}

/// The folder to serve: `dir` if it has HTML files, else a usual output
/// folder that has an index.html.
fn static_root(dir: &Path) -> Option<PathBuf> {
    if dir.join("index.html").is_file() {
        return Some(dir.to_path_buf());
    }
    for sub in ["dist", "public", "build", "out", "_site", "site", "docs"] {
        let candidate = dir.join(sub);
        if candidate.join("index.html").is_file() {
            return Some(candidate);
        }
    }
    (!html_files(dir, 1).is_empty()).then(|| dir.to_path_buf())
}

/// HTML files under `dir` (relative paths), at most `depth` folders down,
/// skipping hidden folders and node_modules.
fn html_files(dir: &Path, depth: usize) -> Vec<String> {
    let mut found = Vec::new();
    walk(dir, dir, depth, &mut |rel| {
        if rel.extension().is_some_and(|e| e == "html" || e == "htm") {
            found.push(rel.to_string_lossy().into_owned());
        }
    });
    found.sort();
    found
}

fn walk(root: &Path, dir: &Path, depth: usize, f: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                walk(root, &path, depth - 1, f);
            }
        } else if let Ok(rel) = path.strip_prefix(root) {
            f(rel);
        }
    }
}

fn on_path(program: &str) -> bool {
    find_program(program).is_some()
}

/// Where `program` is: on PATH, or in the usual folders PATH lacks when
/// Claude Deck was opened from the Finder (Homebrew, Docker Desktop).
fn find_program(program: &str) -> Option<PathBuf> {
    let extra = ["/opt/homebrew/bin", "/usr/local/bin", "/Applications/Docker.app/Contents/Resources/bin"].map(PathBuf::from);
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .chain(extra)
        .chain(dirs::home_dir().map(|h| h.join(".docker/bin")))
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

/// `docker`, by its full path when it can be found.
fn docker() -> Command {
    Command::new(find_program("docker").unwrap_or_else(|| PathBuf::from("docker")))
}

// ---- pages ---------------------------------------------------------------

const MAX_PAGES: usize = 12;

/// Paths worth linking to (always starting with "/"), at most 12.
pub fn pages(project: &Project) -> Vec<String> {
    let root = &project.root;
    let mut pages: Vec<String> = match project.framework {
        Framework::Static => html_files(root, 1)
            .into_iter()
            .map(|f| match f.as_str() {
                "index.html" => "/".to_string(),
                _ => format!("/{}", f.strip_suffix("index.html").unwrap_or(&f)),
            })
            .collect(),
        Framework::Astro => routes(&root.join("src/pages"), &["astro", "md", "mdx", "html"], false),
        Framework::Next => {
            let app = ["app", "src/app"].iter().map(|d| root.join(d)).find(|d| d.is_dir());
            match app {
                Some(app) => routes(&app, &["tsx", "jsx", "ts", "js", "mdx"], true),
                None => {
                    let pages = ["pages", "src/pages"].iter().map(|d| root.join(d)).find(|d| d.is_dir());
                    pages.map(|p| routes(&p, &["tsx", "jsx", "ts", "js", "mdx"], false)).unwrap_or_default()
                }
            }
        }
        _ => Vec::new(),
    };
    if !pages.iter().any(|p| p == "/") {
        pages.insert(0, "/".into());
    }
    pages.sort_by_key(|p| (p != "/", p.matches('/').count(), p.clone()));
    pages.dedup();
    pages.truncate(MAX_PAGES);
    pages
}

/// Routes from a file-based router folder: every file is a page (Astro,
/// Next pages/), or with `app_router` only page.* files are (Next app/).
/// Dynamic routes (`[slug]`), private files (`_x`) and API routes are skipped.
fn routes(dir: &Path, exts: &[&str], app_router: bool) -> Vec<String> {
    let mut out = Vec::new();
    walk(dir, dir, 3, &mut |rel| {
        let Some(ext) = rel.extension().and_then(|e| e.to_str()) else { return };
        if !exts.contains(&ext) {
            return;
        }
        let parts: Vec<&str> = rel.iter().filter_map(|p| p.to_str()).collect();
        let Some((file, folders)) = parts.split_last() else { return };
        let stem = file.rsplit_once('.').map_or(*file, |(s, _)| s);
        if app_router && stem != "page" {
            return;
        }
        let skip = |s: &str| s.starts_with('[') || s.starts_with('_') || s == "api";
        if folders.iter().any(|f| skip(f)) || (!app_router && skip(stem)) {
            return;
        }
        // App router groups like (marketing) don't appear in the URL.
        let mut segments: Vec<&str> = folders.iter().copied().filter(|f| !(f.starts_with('(') && f.ends_with(')'))).collect();
        if !app_router && stem != "index" {
            segments.push(stem);
        }
        out.push(format!("/{}", segments.join("/")));
    });
    out
}

// ---- ports ---------------------------------------------------------------

/// A free port at or above `preferred`, and (if it moved) who holds
/// `preferred`. Listeners on any address count, as do Docker-published ports.
pub fn pick_port(preferred: u16) -> (u16, Option<String>) {
    let taken = taken_ports();
    let port = (preferred..preferred.saturating_add(100)).find(|p| !taken.iter().any(|(t, _)| t == p)).unwrap_or(preferred);
    let owner = (port != preferred).then(|| {
        taken.iter().find(|(t, _)| *t == preferred).map(|(_, who)| who.clone()).unwrap_or_else(|| "another process".into())
    });
    (port, owner)
}

/// (port, "command (folder)") for every TCP listener and Docker-published port.
fn taken_ports() -> Vec<(u16, String)> {
    let listening = crate::browser::listening_ports();
    let pids: Vec<u32> = listening.keys().copied().collect();
    let cwds = crate::browser::process_cwds(&pids);
    let mut taken = Vec::new();
    for (pid, listener) in &listening {
        let who = crate::browser::who(&listener.command, cwds.get(pid).map(PathBuf::as_path));
        taken.extend(listener.ports().map(|p| (p, who.clone())));
    }
    taken.extend(docker_ports());
    taken
}

/// Host ports published by running Docker containers, with the container
/// name. Empty if Docker isn't installed or running.
fn docker_ports() -> Vec<(u16, String)> {
    let Some(text) = output_within(docker().args(["ps", "--format", "{{.Names}}\t{{.Ports}}"]), Duration::from_secs(2)) else {
        return Vec::new();
    };
    parse_docker_ports(&text)
}

fn parse_docker_ports(text: &str) -> Vec<(u16, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((name, ports)) = line.split_once('\t') else { continue };
        out.extend(published(ports).into_iter().map(|p| (p.host_port, format!("Docker container {name}"))));
    }
    out
}

/// One published port from `docker ps`'s Ports column.
struct Published {
    /// "0.0.0.0", "127.0.0.1", "::", "::1" …
    host_ip: String,
    host_port: u16,
    container_port: u16,
    tcp: bool,
}

/// The published mappings in a Ports column such as
/// "0.0.0.0:4321->4321/tcp, [::]:4321->4321/tcp, 5432/tcp". Unpublished
/// ports (no `->`) are skipped, and port ranges give one entry per port.
fn published(ports: &str) -> Vec<Published> {
    let range = |s: &str| -> Option<std::ops::RangeInclusive<u16>> {
        let (from, to) = s.split_once('-').unwrap_or((s, s));
        Some(from.parse().ok()?..=to.parse().ok()?)
    };
    let mut out = Vec::new();
    for mapping in ports.split(", ") {
        let Some((host, container)) = mapping.trim().split_once("->") else { continue };
        let (container_ports, protocol) = container.split_once('/').unwrap_or((container, "tcp"));
        let Some((ip, host_ports)) = host.rsplit_once(':') else { continue };
        let (Some(host_ports), Some(container_ports)) = (range(host_ports), range(container_ports)) else { continue };
        for (host_port, container_port) in host_ports.zip(container_ports) {
            out.push(Published {
                host_ip: ip.trim_matches(['[', ']']).to_string(),
                host_port,
                container_port,
                tcp: protocol == "tcp",
            });
        }
    }
    out
}

// ---- Docker Compose -----------------------------------------------------------

/// A port one of a folder's Docker Compose services publishes on this Mac.
#[derive(Clone, Debug, PartialEq)]
pub struct ComposePort {
    pub service: String,
    /// The address it's published on: "127.0.0.1", "0.0.0.0", "::1" …
    pub host_ip: String,
    pub host_port: u16,
    pub container_port: u16,
    /// It answers HTTP, so it can be opened in a browser.
    pub web: bool,
}

impl ComposePort {
    /// The loopback address that reaches it. Never `localhost`, which another
    /// project's `[::1]` listener on the same port would capture.
    fn host(&self) -> String {
        match self.host_ip.as_str() {
            "" | "0.0.0.0" | "127.0.0.1" => "127.0.0.1".into(),
            "::" | "::1" => "[::1]".into(),
            ip if ip.contains(':') => format!("[{ip}]"),
            ip => ip.into(),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}:{}", self.host(), self.host_port)
    }
}

/// Whether `dir` has a Docker Compose file.
pub fn has_compose_file(dir: &Path) -> bool {
    ["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"].iter().any(|f| dir.join(f).is_file())
}

/// The ports published by the running Compose services whose project folder
/// is `dir`, each probed to see whether it's a web page. Empty when there are
/// none, or when Docker is missing, stopped or slow. Runs `docker ps` and
/// waits up to a second for replies, so call it off the main thread.
pub fn compose_services(dir: &Path) -> Vec<ComposePort> {
    // Compose records the folder with symlinks resolved.
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let filter = format!("label=com.docker.compose.project.working_dir={}", dir.display());
    let format = "{{.Label \"com.docker.compose.service\"}}\t{{.Ports}}";
    let Some(text) = output_within(docker().args(["ps", "--filter", &filter, "--format", format]), Duration::from_secs(2)) else {
        return Vec::new();
    };
    let mut ports = parse_compose_ports(&text);
    std::thread::scope(|scope| {
        for port in &mut ports {
            scope.spawn(move || port.web = answers_http(&port.host(), port.host_port));
        }
    });
    sort_compose_ports(&mut ports);
    ports
}

/// Published TCP ports from `docker ps` lines of "service\tports". A port
/// published on both IPv4 and IPv6 is listed once (as IPv4).
fn parse_compose_ports(text: &str) -> Vec<ComposePort> {
    let mut out: Vec<ComposePort> = Vec::new();
    for line in text.lines() {
        let Some((service, ports)) = line.split_once('\t') else { continue };
        let service = service.trim();
        for p in published(ports).into_iter().filter(|p| p.tcp) {
            let ipv6 = p.host_ip.contains(':');
            match out.iter_mut().find(|o| o.service == service && o.host_port == p.host_port) {
                Some(seen) if seen.host_ip.contains(':') && !ipv6 => seen.host_ip = p.host_ip,
                Some(_) => {}
                None => out.push(ComposePort {
                    service: service.to_string(),
                    host_ip: p.host_ip,
                    host_port: p.host_port,
                    container_port: p.container_port,
                    web: false,
                }),
            }
        }
    }
    out
}

/// By service, web pages first, then by port.
fn sort_compose_ports(ports: &mut [ComposePort]) {
    ports.sort_by(|a, b| a.service.cmp(&b.service).then(b.web.cmp(&a.web)).then(a.host_port.cmp(&b.host_port)));
}

/// The port the browser buttons open: the lowest-numbered web one.
pub fn compose_primary(ports: &[ComposePort]) -> Option<&ComposePort> {
    ports.iter().filter(|p| p.web).min_by_key(|p| p.host_port)
}

/// Whether `host:port` answers an HTTP request within a second. Other
/// services say something else (an SMTP banner) or nothing at all.
fn answers_http(host: &str, port: u16) -> bool {
    let wait = Duration::from_secs(1);
    let Ok(addr) = format!("{host}:{port}").parse::<SocketAddr>() else { return false };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, wait) else { return false };
    let _ = stream.set_read_timeout(Some(wait));
    let _ = stream.set_write_timeout(Some(wait));
    if stream.write_all(b"HEAD / HTTP/1.0\r\n\r\n").is_err() {
        return false;
    }
    let mut reply = [0u8; 8];
    let mut got = 0;
    while got < 5 {
        match stream.read(&mut reply[got..]) {
            Ok(n) if n > 0 => got += n,
            _ => break,
        }
    }
    looks_like_http(&reply[..got])
}

fn looks_like_http(reply: &[u8]) -> bool {
    reply.starts_with(b"HTTP/")
}

/// A command's stdout, or None if it fails or takes longer than `limit`.
pub(crate) fn output_within(cmd: &mut Command, limit: Duration) -> Option<String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Whether something accepts connections on `port`: 127.0.0.1, plus ::1 when
/// `any_loopback` (for servers that pick their own address).
pub fn is_listening(port: u16, any_loopback: bool) -> bool {
    let try_addr = |addr: &str| {
        addr.parse::<SocketAddr>()
            .ok()
            .is_some_and(|a| TcpStream::connect_timeout(&a, Duration::from_millis(150)).is_ok())
    };
    try_addr(&format!("127.0.0.1:{port}")) || (any_loopback && try_addr(&format!("[::1]:{port}")))
}

/// The port a dev server started as process `pid` answers on, once it does.
/// That's `port` when the server (or anything it started) is what listens
/// there. If another process holds `port`, a server such as Astro quietly
/// moves to the next free one: then it's the port the server did take. None
/// while it isn't up yet, so a stranger on `port` is never taken for it.
pub fn ready_port(pid: u32, port: u16, any_loopback: bool, dir: &Path) -> Option<u16> {
    if !is_listening(port, any_loopback) {
        return None;
    }
    let listening = crate::browser::listening_ports();
    if listening.is_empty() {
        return Some(port); // lsof told us nothing, so there's nothing to check against
    }
    let mut tree = crate::browser::descendants_of(pid);
    tree.insert(pid);
    let ours: Vec<u16> = listening.iter().filter(|(p, _)| tree.contains(p)).flat_map(|(_, l)| l.ports()).collect();
    let holders: Vec<u32> = listening.iter().filter(|(_, l)| l.ports().any(|p| p == port)).map(|(p, _)| *p).collect();
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let cwds = crate::browser::process_cwds(&holders);
    moved_port(port, &ours, holders.iter().any(|p| cwds.get(p).is_some_and(|cwd| cwd.starts_with(&dir))))
}

/// See `ready_port`. `ours`: ports the server's process tree listens on;
/// `holder_in_folder`: whether a process holding `port` runs in the project
/// folder (a server that detached from the process tree).
fn moved_port(port: u16, ours: &[u16], holder_in_folder: bool) -> Option<u16> {
    if ours.contains(&port) || (ours.is_empty() && holder_in_folder) {
        return Some(port);
    }
    // The next port up, as dev servers pick it; else whatever it took.
    ours.iter().copied().filter(|p| *p > port).min().or_else(|| ours.iter().copied().min())
}

impl Project {
    #[cfg(test)]
    pub fn is_ready(&self, port: u16) -> bool {
        is_listening(port, self.ready_on_any_loopback())
    }

    /// The `any_loopback` argument `is_ready` passes to `is_listening`, so
    /// the check can run off the UI thread without the whole project.
    pub fn ready_on_any_loopback(&self) -> bool {
        !self.framework.binds_ipv4_loopback()
    }
}

// ---- output log -------------------------------------------------------------

const LOG_LINES: usize = 200;

/// The last lines a command printed (stdout and stderr), shared with the
/// threads reading them.
#[derive(Clone, Default)]
pub struct Log(Arc<Mutex<LogInner>>);

#[derive(Default)]
struct LogInner {
    lines: VecDeque<String>,
    /// Bumped on every new line, so the UI can tell when to redraw.
    generation: u64,
}

impl Log {
    pub fn push(&self, line: &str) {
        let line = strip_ansi(line);
        let line = line.trim_end();
        if line.trim().is_empty() {
            return;
        }
        let mut inner = self.0.lock().unwrap();
        if inner.lines.len() == LOG_LINES {
            inner.lines.pop_front();
        }
        inner.lines.push_back(line.to_string());
        inner.generation += 1;
    }

    pub fn tail(&self, n: usize) -> Vec<String> {
        let inner = self.0.lock().unwrap();
        inner.lines.iter().skip(inner.lines.len().saturating_sub(n)).cloned().collect()
    }

    pub fn generation(&self) -> u64 {
        self.0.lock().unwrap().generation
    }

    pub fn text(&self) -> String {
        self.tail(LOG_LINES).join("\n")
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // CSI: parameters, then one final byte in @..~
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
        } else if c == '\r' {
            out.clear(); // progress redraws: keep the last state of the line
        } else {
            out.push(c);
        }
    }
    out
}

// ---- processes ------------------------------------------------------------

/// A command running in its own process group, so stopping it also stops
/// whatever it started (npm -> node -> esbuild ...).
pub struct ServerProcess {
    child: Child,
    stopped: bool,
}

impl ServerProcess {
    /// Run `argv` in `dir` through a login shell (so PATH has nvm, Homebrew
    /// and the like, even when Claude Deck was opened from the Finder).
    pub fn spawn(argv: &[String], dir: &Path, port: Option<u16>, log: &Log) -> std::io::Result<Self> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let line = argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
        log.push(&format!("$ {line}"));
        let mut cmd = Command::new(shell);
        cmd.args(["-l", "-c", &format!("exec {line}")])
            .current_dir(dir)
            .env("FORCE_COLOR", "0")
            .env("NO_COLOR", "1")
            .env("BROWSER", "none")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(port) = port {
            cmd.env("PORT", port.to_string());
        }
        let mut child = cmd.spawn()?;
        for stream in [child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)]
            .into_iter()
            .flatten()
        {
            let log = log.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    log.push(&line);
                }
            });
        }
        Ok(Self { child, stopped: false })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Some(success) once the process has exited.
    pub fn exited(&mut self) -> Option<bool> {
        self.child.try_wait().ok().flatten().map(|s| s.success())
    }

    /// SIGTERM the whole group, then SIGKILL anything left after 3s.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let pgid = self.child.id() as i32;
        unsafe { libc::killpg(pgid, libc::SIGTERM) };
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
                // Reap the group leader so it doesn't linger as a zombie
                // (harmless ECHILD if `exited` already did).
                libc::waitpid(pgid, std::ptr::null_mut(), libc::WNOHANG);
            }
        });
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

// ---- static server ----------------------------------------------------------

/// A minimal HTTP server for a folder of static files, on 127.0.0.1.
pub struct StaticServer {
    port: u16,
    stop: Arc<AtomicBool>,
}

impl StaticServer {
    pub fn start(root: &Path, port: u16, log: &Log) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let root = root.canonicalize()?;
        let stop = Arc::new(AtomicBool::new(false));
        log.push(&format!("Serving {} on http://127.0.0.1:{port}", root.display()));
        let flag = stop.clone();
        let log = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let root = root.clone();
                let log = log.clone();
                std::thread::spawn(move || {
                    let _ = serve_one(stream, &root, &log);
                });
            }
        });
        Ok(Self { port, stop })
    }

    pub fn stop(&self) {
        if !self.stop.swap(true, Ordering::Relaxed) {
            // Wake the accept loop so it sees the flag and drops the listener.
            let _ = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], self.port)), Duration::from_millis(200));
        }
    }
}

impl Drop for StaticServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve_one(mut stream: TcpStream, root: &Path, log: &Log) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    // Skip the headers.
    let mut header = String::new();
    while reader.read_line(&mut header)? > 2 {
        header.clear();
    }
    let mut parts = request.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    if method != "GET" && method != "HEAD" {
        return respond(&mut stream, "405 Method Not Allowed", "text/plain", b"Method not allowed", method == "HEAD");
    }
    let head = method == "HEAD";
    match resolve(root, target) {
        Some(file) => {
            let body = std::fs::read(&file)?;
            log.push(&format!("GET {target} 200"));
            respond(&mut stream, "200 OK", mime(&file), &body, head)
        }
        None => {
            log.push(&format!("GET {target} 404"));
            let body = format!(
                "<!doctype html><meta charset=utf-8><title>Not found</title>\
                 <body style=\"font:15px -apple-system,sans-serif;padding:48px;color:#333\">\
                 <h1 style=\"font-weight:600\">404</h1><p>{} isn't in this folder.</p>",
                html_escape(target)
            );
            respond(&mut stream, "404 Not Found", "text/html; charset=utf-8", body.as_bytes(), head)
        }
    }
}

fn respond(stream: &mut TcpStream, status: &str, mime: &str, body: &[u8], head: bool) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    if !head {
        stream.write_all(body)?;
    }
    stream.flush()
}

/// The file a request path names under `root`: query and fragment dropped,
/// percent-decoded, `index.html` for folders, `.html` added when that
/// exists. None for anything outside `root` or missing.
fn resolve(root: &Path, target: &str) -> Option<PathBuf> {
    let path = target.split(['?', '#']).next().unwrap_or("/");
    let decoded = percent_decode(path)?;
    let mut file = root.to_path_buf();
    for component in Path::new(decoded.trim_start_matches('/')).components() {
        match component {
            Component::Normal(part) => file.push(part),
            Component::CurDir => {}
            _ => return None, // `..`, absolute paths
        }
    }
    if file.is_dir() {
        file.push("index.html");
    } else if !file.exists() && file.extension().is_none() {
        file.set_extension("html");
    }
    // Symlinks could still point outside the root.
    let file = file.canonicalize().ok()?;
    (file.starts_with(root) && file.is_file()).then_some(file)
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some("wasm") => "application/wasm",
        Some("txt" | "md") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("claude-deck-webserver-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn detects_astro_with_pnpm_needing_install() {
        let dir = temp_dir("astro");
        write(&dir, "package.json", r#"{"scripts":{"dev":"astro dev"},"dependencies":{"astro":"^5"}}"#);
        write(&dir, "pnpm-lock.yaml", "");
        let p = detect(&dir).unwrap();
        assert_eq!(p.framework, Framework::Astro);
        assert_eq!(p.package_manager, Some("pnpm"));
        assert!(p.needs_install);
        assert_eq!(p.install_command().unwrap(), ["pnpm", "install"]);
        assert_eq!(p.start_command(4322), ["pnpm", "run", "dev", "--port", "4322", "--host", "127.0.0.1"]);
        assert_eq!(p.url(4322), "http://127.0.0.1:4322");
    }

    #[test]
    fn detects_next_with_npm() {
        let dir = temp_dir("next");
        write(&dir, "package.json", r#"{"scripts":{"dev":"next dev"},"dependencies":{"next":"15","react":"19"}}"#);
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        let p = detect(&dir).unwrap();
        assert_eq!(p.framework, Framework::Next);
        assert!(!p.needs_install);
        assert_eq!(p.start_command(3001), ["npm", "run", "dev", "--", "-p", "3001", "-H", "127.0.0.1"]);
    }

    #[test]
    fn detects_static_sites_and_output_folders() {
        let dir = temp_dir("static");
        write(&dir, "index.html", "<h1>hi</h1>");
        write(&dir, "about.html", "");
        let p = detect(&dir).unwrap();
        assert!(p.is_static());
        assert_eq!(pages(&p), ["/", "/about.html"]);

        let dir = temp_dir("dist");
        write(&dir, "README.md", "");
        write(&dir, "dist/index.html", "");
        assert_eq!(detect(&dir).unwrap().root, dir.join("dist"));
    }

    #[test]
    fn empty_folder_cannot_be_served() {
        assert!(detect(&temp_dir("empty")).is_err());
    }

    #[test]
    fn lists_astro_pages_without_dynamic_routes() {
        let dir = temp_dir("astro-pages");
        for f in ["index.astro", "about.astro", "blog/index.md", "blog/[slug].astro", "_draft.astro", "api/x.ts"] {
            write(&dir, &format!("src/pages/{f}"), "");
        }
        let p = Project { framework: Framework::Astro, root: dir, package_manager: None, script: None, needs_install: false };
        assert_eq!(pages(&p), ["/", "/about", "/blog"]);
    }

    #[test]
    fn lists_next_app_router_pages() {
        let dir = temp_dir("next-pages");
        for f in ["page.tsx", "layout.tsx", "pricing/page.tsx", "(marketing)/team/page.tsx", "blog/[id]/page.tsx"] {
            write(&dir, &format!("app/{f}"), "");
        }
        let p = Project { framework: Framework::Next, root: dir, package_manager: None, script: None, needs_install: false };
        assert_eq!(pages(&p), ["/", "/pricing", "/team"]);
    }

    #[test]
    fn skips_a_taken_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = listener.local_addr().unwrap().port();
        let (port, _) = pick_port(taken);
        assert_ne!(port, taken);
    }

    #[test]
    fn resolves_files_and_blocks_traversal() {
        let dir = temp_dir("resolve");
        write(&dir, "index.html", "");
        write(&dir, "docs/index.html", "");
        write(&dir, "about.html", "");
        write(&dir, "my page.html", "");
        let root = dir.canonicalize().unwrap();
        assert_eq!(resolve(&root, "/"), Some(root.join("index.html")));
        assert_eq!(resolve(&root, "/docs/?x=1"), Some(root.join("docs/index.html")));
        assert_eq!(resolve(&root, "/about"), Some(root.join("about.html")));
        assert_eq!(resolve(&root, "/my%20page.html"), Some(root.join("my page.html")));
        assert_eq!(resolve(&root, "/../etc/passwd"), None);
        assert_eq!(resolve(&root, "/%2e%2e/secret"), None);
        assert_eq!(resolve(&root, "/missing.css"), None);
        assert_eq!(mime(Path::new("a.woff2")), "font/woff2");
    }

    #[test]
    fn serves_static_files_over_http() {
        let dir = temp_dir("http");
        write(&dir, "index.html", "<h1>hello</h1>");
        let (port, _) = pick_port(18_480);
        let log = Log::default();
        let server = StaticServer::start(&dir, port, &log).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("text/html"));
        assert!(response.ends_with("<h1>hello</h1>"));
        server.stop();
    }

    /// Runs `npm run dev` through the login shell; needs npm and python3.
    #[test]
    #[ignore]
    fn spawns_waits_for_and_stops_a_dev_server() {
        let dir = temp_dir("spawn");
        write(&dir, "package.json", r#"{"scripts":{"dev":"python3 -m http.server $PORT --bind 127.0.0.1"}}"#);
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        let project = detect(&dir).unwrap();
        assert_eq!(project.framework, Framework::Node);
        let (port, _) = pick_port(18_600);
        let log = Log::default();
        let mut process = ServerProcess::spawn(&project.start_command(port), &dir, Some(port), &log).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !project.is_ready(port) {
            assert!(process.exited().is_none(), "exited early: {}", log.text());
            assert!(Instant::now() < deadline, "never listened: {}", log.text());
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(log.text().contains("$ npm run dev"));
        process.stop();
        std::thread::sleep(Duration::from_millis(800));
        assert!(!project.is_ready(port), "still listening after stop");
    }

    #[test]
    fn parses_compose_services() {
        // The stack from ISSUES.md, plus a port on both IPv4 and IPv6, an
        // unpublished one, a UDP one and a range.
        let text = "app\t127.0.0.1:4321->4321/tcp\n\
                    assets\t127.0.0.1:8081->80/tcp\n\
                    mailpit\t127.0.0.1:1025->1025/tcp, 127.0.0.1:8025->8025/tcp\n\
                    web\t[::]:3000->3000/tcp, 0.0.0.0:3000->3000/tcp, :::9000->9000/tcp\n\
                    db\t5432/tcp, 0.0.0.0:5353->53/udp\n\
                    range\t0.0.0.0:7000-7001->8000-8001/tcp\n";
        let mut ports = parse_compose_ports(text);
        let row = |p: &ComposePort| (p.service.clone(), p.host_port, p.container_port, p.url());
        let rows: Vec<_> = ports.iter().map(row).collect();
        let expect = |service: &str, host: u16, container: u16, url: &str| (service.to_string(), host, container, url.to_string());
        assert_eq!(
            rows,
            [
                expect("app", 4321, 4321, "http://127.0.0.1:4321"),
                expect("assets", 8081, 80, "http://127.0.0.1:8081"),
                expect("mailpit", 1025, 1025, "http://127.0.0.1:1025"),
                expect("mailpit", 8025, 8025, "http://127.0.0.1:8025"),
                expect("web", 3000, 3000, "http://127.0.0.1:3000"),
                expect("web", 9000, 9000, "http://[::1]:9000"),
                expect("range", 7000, 8000, "http://127.0.0.1:7000"),
                expect("range", 7001, 8001, "http://127.0.0.1:7001"),
            ]
        );

        // Web pages first within a service; the lowest web port is primary.
        for p in &mut ports {
            p.web = !matches!(p.host_port, 1025 | 7000 | 7001);
        }
        sort_compose_ports(&mut ports);
        let order: Vec<_> = ports.iter().filter(|p| p.service == "mailpit").map(|p| p.host_port).collect();
        assert_eq!(order, [8025, 1025]);
        assert_eq!(compose_primary(&ports).unwrap().host_port, 3000);
        assert!(compose_primary(&ports[..0]).is_none());
    }

    #[test]
    fn tells_web_replies_from_other_services() {
        assert!(looks_like_http(b"HTTP/1.1 200 OK"));
        assert!(looks_like_http(b"HTTP/1.0 404"));
        assert!(!looks_like_http(b"220 mailpit ESMTP"));
        assert!(!looks_like_http(b""));
    }

    #[test]
    fn probes_ports_for_http() {
        let dir = temp_dir("probe");
        write(&dir, "index.html", "<h1>hello</h1>");
        let (port, _) = pick_port(18_520);
        let server = StaticServer::start(&dir, port, &Log::default()).unwrap();
        assert!(answers_http("127.0.0.1", port));
        server.stop();

        // A mail server greets first, and never speaks HTTP.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let smtp = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.write_all(b"220 mailpit ESMTP Service ready\r\n");
            }
        });
        assert!(!answers_http("127.0.0.1", smtp));
    }

    #[test]
    fn finds_compose_files() {
        let dir = temp_dir("compose");
        assert!(!has_compose_file(&dir));
        write(&dir, "docker-compose.yml", "services: {}");
        assert!(has_compose_file(&dir));
    }

    #[test]
    fn a_stranger_on_the_port_is_not_our_server() {
        // Ours listens where we asked.
        assert_eq!(moved_port(4322, &[4322], false), Some(4322));
        // Another project holds 4322 and ours isn't up yet.
        assert_eq!(moved_port(4322, &[], false), None);
        // Astro moved up to 4323 (and has a side port open).
        assert_eq!(moved_port(4322, &[24678, 4323], false), Some(4323));
        assert_eq!(moved_port(4322, &[3000], false), Some(3000));
        // A server that left our process tree but runs in the folder.
        assert_eq!(moved_port(4322, &[], true), Some(4322));
    }

    #[test]
    fn parses_docker_ports_and_strips_ansi() {
        let ports = parse_docker_ports("web\t0.0.0.0:4321->4321/tcp, [::]:4321->4321/tcp\ndb\t5432/tcp\n");
        assert_eq!(ports.iter().map(|(p, _)| *p).collect::<Vec<_>>(), [4321, 4321]);
        assert_eq!(strip_ansi("\u{1b}[32m✓\u{1b}[0m ready"), "✓ ready");
        assert_eq!(strip_ansi("10%\r50%\r100%"), "100%");
    }
}
