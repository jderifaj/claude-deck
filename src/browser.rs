//! Open a session's local web page (its dev server, else index.html) in an
//! installed browser or Xcode's iOS Simulator, and load installed apps' icons
//! for buttons.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Browser {
    Chrome,
    Safari,
    Firefox,
    /// Safari on a device in Xcode's iOS Simulator, shown in DeviceHub.app
    /// (Simulator.app before Xcode 27). The device is picked from a menu.
    Simulator,
}

impl Browser {
    pub const ALL: [Browser; 4] = [Browser::Chrome, Browser::Safari, Browser::Firefox, Browser::Simulator];

    pub fn app_name(self) -> &'static str {
        match self {
            Browser::Chrome => "Google Chrome",
            Browser::Safari => "Safari",
            Browser::Firefox => "Firefox",
            Browser::Simulator => "Simulator",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Browser::Chrome => "Chrome",
            Browser::Safari => "Safari",
            Browser::Firefox => "Firefox",
            Browser::Simulator => "Device Hub",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Browser::Chrome => "act-chrome",
            Browser::Safari => "act-safari",
            Browser::Firefox => "act-firefox",
            Browser::Simulator => "act-simulator",
        }
    }

    /// Line icon shown when the app's own icon can't be read.
    pub fn fallback_icon(self) -> &'static str {
        match self {
            Browser::Simulator => "phone",
            _ => "globe",
        }
    }
}

/// An installed browser: its app bundle and (if it could be read) its icon.
#[derive(Clone)]
pub struct InstalledBrowser {
    pub browser: Browser,
    pub app: PathBuf,
    pub icon: Option<PathBuf>,
}

/// Browsers installed on this Mac, in button order.
pub fn installed() -> Vec<InstalledBrowser> {
    Browser::ALL
        .into_iter()
        .filter_map(|browser| {
            let app = match browser {
                Browser::Simulator => simulator_app()?,
                _ => app_bundle(browser.app_name())?,
            };
            let icon = app_icon(&app, &format!("{}-icon", browser.app_name().to_lowercase().replace(' ', "-")));
            Some(InstalledBrowser { browser, app, icon })
        })
        .collect()
}

/// The app that shows Xcode's simulators, found in the selected developer
/// folder or in an installed Xcode (the selected folder is often just the
/// Command Line Tools). Up to Xcode 26 that's Simulator.app in the developer
/// folder; Xcode 27 replaced it with DeviceHub.app in Xcode's own Applications.
fn simulator_app() -> Option<PathBuf> {
    let selected = Command::new("xcode-select")
        .arg("-p")
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    selected
        .into_iter()
        .chain(["Xcode", "Xcode-beta"].into_iter().filter_map(app_bundle).map(|x| x.join("Contents/Developer")))
        .flat_map(|dev| {
            let devicehub = dev.parent().map(|contents| contents.join("Applications/DeviceHub.app"));
            [Some(dev.join("Applications/Simulator.app")), devicehub]
        })
        .flatten()
        .find(|p| p.is_dir())
}

/// The developer folder of the Xcode that holds `app` (Simulator.app or
/// DeviceHub.app): `<Xcode.app>/Contents/Developer`.
fn developer_dir(app: &Path) -> Option<PathBuf> {
    app.ancestors().find(|a| a.file_name().is_some_and(|n| n == "Contents")).map(|c| c.join("Developer"))
}

/// Find `<name>.app` in the usual application folders.
pub fn app_bundle(name: &str) -> Option<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Volumes/Preboot/Cryptexes/App/System/Applications"),
    ];
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join("Applications"));
    }
    dirs.into_iter().map(|d| d.join(format!("{name}.app"))).find(|p| p.is_dir())
}

/// An app's icon, converted once to a PNG under ~/.claude-deck/cache, traced
/// into an SVG with VTracer, and refreshed when the app updates. Falls back to
/// the PNG if tracing fails; None if the icon can't be read.
pub fn app_icon(app: &Path, cache_name: &str) -> Option<PathBuf> {
    let plist = app.join("Contents/Info.plist");
    let output = Command::new("plutil")
        .args(["-extract", "CFBundleIconFile", "raw", "-o", "-"])
        .arg(&plist)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let mut file = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if !output.status.success() || file.is_empty() {
        return None;
    }
    if !file.ends_with(".icns") {
        file.push_str(".icns");
    }
    let source = app.join("Contents/Resources").join(file);
    let cache = crate::catalog::deck_dir().join("cache");
    let out = cache.join(format!("{cache_name}-256.png"));
    let svg = cache.join(format!("{cache_name}.svg"));
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let fresh = |p: &Path| modified(p).is_some_and(|o| modified(&source).is_none_or(|s| o >= s));
    if fresh(&svg) {
        return Some(svg);
    }
    if fresh(&out) {
        return Some(trace(&out, &svg));
    }
    if !source.exists() {
        return None;
    }
    std::fs::create_dir_all(out.parent()?).ok()?;
    let status = Command::new("sips")
        .args(["-s", "format", "png", "-Z", "256"])
        .arg(&source)
        .arg("--out")
        .arg(&out)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    status.success().then(|| trace(&out, &svg))
}

/// The traced SVG, or the PNG itself if VTracer can't trace it.
fn trace(png: &Path, svg: &Path) -> PathBuf {
    match crate::vector::trace_file(png, svg) {
        Ok(()) => svg.to_path_buf(),
        Err(_) => png.to_path_buf(),
    }
}

/// The page to show for a session: a local server started from the session
/// (a process under `root_pid`) or running in its folder, else the folder's
/// index.html.
pub fn find_local_page(root_pid: Option<u32>, dir: &Path) -> Option<String> {
    if let Some(port) = find_server_port(root_pid, dir) {
        return Some(format!("http://localhost:{port}"));
    }
    let index = dir.join("index.html");
    index.is_file().then(|| format!("file://{}", index.display()))
}

fn find_server_port(root_pid: Option<u32>, dir: &Path) -> Option<u16> {
    let listening = listening_ports();
    if listening.is_empty() {
        return None;
    }
    let descendants = root_pid.map(descendants_of).unwrap_or_default();
    let pids: Vec<u32> = listening.keys().copied().collect();
    let cwds = process_cwds(&pids);

    // Prefer servers the session started, then the lowest port.
    listening
        .iter()
        .filter(|(pid, (command, _))| Some(**pid) != root_pid && !command.to_lowercase().contains("claude"))
        .filter_map(|(pid, (_, ports))| {
            let ours = descendants.contains(pid);
            let in_dir = cwds.get(pid).is_some_and(|cwd| cwd.starts_with(dir));
            (ours || in_dir).then(|| ports.iter().map(move |&port| (ours, port)))
        })
        .flatten()
        .max_by_key(|&(ours, port)| (ours, std::cmp::Reverse(port)))
        .map(|(_, port)| port)
}

/// pid -> (command name, TCP ports it listens on).
pub(crate) fn listening_ports() -> HashMap<u32, (String, Vec<u16>)> {
    let mut map: HashMap<u32, (String, Vec<u16>)> = HashMap::new();
    let Ok(output) = Command::new("lsof").args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"]).stderr(Stdio::null()).output()
    else {
        return map;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut pid = None;
    for line in text.lines() {
        let (tag, value) = line.split_at(line.len().min(1));
        match tag {
            "p" => {
                pid = value.parse::<u32>().ok();
                if let Some(pid) = pid {
                    map.entry(pid).or_default();
                }
            }
            "c" => {
                if let Some(entry) = pid.and_then(|p| map.get_mut(&p)) {
                    entry.0 = value.to_string();
                }
            }
            "n" => {
                let port = value.rsplit(':').next().and_then(|p| p.parse::<u16>().ok());
                if let (Some(entry), Some(port)) = (pid.and_then(|p| map.get_mut(&p)), port) {
                    if !entry.1.contains(&port) {
                        entry.1.push(port);
                    }
                }
            }
            _ => {}
        }
    }
    map
}

/// Every process below `root` in the process tree.
fn descendants_of(root: u32) -> HashSet<u32> {
    let mut found = HashSet::new();
    let Ok(output) = Command::new("ps").args(["-axo", "pid=,ppid="]).stderr(Stdio::null()).output() else {
        return found;
    };
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.split_whitespace().filter_map(|p| p.parse::<u32>().ok());
        if let (Some(pid), Some(ppid)) = (parts.next(), parts.next()) {
            children.entry(ppid).or_default().push(pid);
        }
    }
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        for &child in children.get(&pid).into_iter().flatten() {
            if found.insert(child) {
                stack.push(child);
            }
        }
    }
    found
}

/// Working directories of the given processes.
pub(crate) fn process_cwds(pids: &[u32]) -> HashMap<u32, PathBuf> {
    let mut map = HashMap::new();
    if pids.is_empty() {
        return map;
    }
    let list = pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
    let Ok(output) = Command::new("lsof").args(["-a", "-d", "cwd", "-Fn", "-p", &list]).stderr(Stdio::null()).output()
    else {
        return map;
    };
    let mut pid = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(v) = line.strip_prefix('p') {
            pid = v.parse::<u32>().ok();
        } else if let (Some(v), Some(p)) = (line.strip_prefix('n'), pid) {
            map.insert(p, PathBuf::from(v));
        }
    }
    map
}

/// Open `url` in a Mac browser.
pub fn open(app: &Path, url: &str) -> Result<()> {
    let status = Command::new("open").arg("-a").arg(app).arg(url).status()?;
    if !status.success() {
        bail!("`open` exited with {status}");
    }
    Ok(())
}

/// An iOS simulator device that can open pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimDevice {
    pub udid: String,
    pub name: String,
    /// Its iOS version, e.g. "iOS 27.2".
    pub os: String,
    pub booted: bool,
}

impl SimDevice {
    pub fn is_ipad(&self) -> bool {
        self.name.starts_with("iPad")
    }
}

/// Run `xcrun simctl` with the Xcode that holds `app` (Simulator.app or
/// DeviceHub.app), since xcode-select may point at the Command Line Tools,
/// which have no simctl.
fn simctl(app: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let developer_dir = developer_dir(app).context("The simulator app isn't inside Xcode")?;
    let output = Command::new("xcrun")
        .env("DEVELOPER_DIR", &developer_dir)
        .arg("simctl")
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        bail!("simctl {} failed: {}", args[0], String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output.stdout)
}

/// The iPhone and iPad simulators installed with `app`'s Xcode, for the
/// device menu.
pub fn simulator_devices(app: &Path) -> Result<Vec<SimDevice>> {
    let devices = parse_devices(&simctl(app, &["list", "devices", "available", "-j"])?);
    if devices.is_empty() {
        bail!("No iPhone or iPad simulator is installed. In Xcode, open Settings → Components and download the iOS platform.");
    }
    Ok(devices)
}

/// iPhones then iPads from `simctl list devices -j` output. A name on several
/// runtimes (or added twice) is listed once: the running one, else the one
/// on the newest iOS. Device-specific runtimes (ids like
/// `iOS-27-2-iPhone19-4`, for one new model) count by their version.
fn parse_devices(json: &[u8]) -> Vec<SimDevice> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(json) else { return Vec::new() };
    let Some(runtimes) = value["devices"].as_object() else { return Vec::new() };
    let mut found: Vec<(Vec<u32>, usize, SimDevice)> = Vec::new();
    for (runtime, list) in runtimes {
        let Some((_, rest)) = runtime.rsplit_once(".iOS-") else { continue };
        let version: Vec<u32> = rest.split('-').map_while(|n| n.parse().ok()).collect();
        let os = format!("iOS {}", version.iter().map(u32::to_string).collect::<Vec<_>>().join("."));
        for d in list.as_array().into_iter().flatten() {
            let (Some(udid), Some(name)) = (d["udid"].as_str(), d["name"].as_str()) else { continue };
            if !(name.starts_with("iPhone") || name.starts_with("iPad")) {
                continue;
            }
            let device =
                SimDevice { udid: udid.into(), name: name.into(), os: os.clone(), booted: d["state"] == "Booted" };
            found.push((version.clone(), found.len(), device));
        }
    }
    // Best copy of each name first, then keep the first of each name.
    found.sort_by(|(va, ia, a), (vb, ib, b)| b.booted.cmp(&a.booted).then(vb.cmp(va)).then(ia.cmp(ib)));
    let mut seen = HashSet::new();
    found.retain(|(_, _, d)| seen.insert(d.name.clone()));
    // Menu order: iPhones then iPads, each in simctl's order.
    found.sort_by_key(|(_, i, d)| (d.is_ipad(), *i));
    found.into_iter().map(|(_, _, d)| d).collect()
}

/// Open `url` in Safari on simulator `udid`, booting it if needed and showing
/// it in `app`. The simulator shares the Mac's network, so localhost URLs
/// work as-is.
pub fn open_in_simulator(app: &Path, udid: &str, url: &str) -> Result<()> {
    simctl(app, &["bootstatus", udid, "-b"])?;
    let status = Command::new("open").arg("-a").arg(app).status()?;
    if !status.success() {
        bail!("`open` exited with {status}");
    }
    simctl(app, &["openurl", udid, url])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_devices;

    fn device(udid: &str, name: &str, state: &str) -> serde_json::Value {
        serde_json::json!({ "udid": udid, "name": name, "state": state })
    }

    fn list(booted_old_duo: bool) -> Vec<u8> {
        let old_duo = if booted_old_duo { "Booted" } else { "Shutdown" };
        serde_json::to_vec(&serde_json::json!({ "devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-27-1": [device("duo-27-1", "iPhone Duo", old_duo)],
            "com.apple.CoreSimulator.SimRuntime.iOS-27-2": [
                device("pro", "iPhone 18 Pro", "Shutdown"),
                device("pro-2", "iPhone 18 Pro", "Shutdown"),
                device("ipad", "iPad mini (A20 Pro)", "Shutdown"),
                device("air", "iPhone Air", "Shutdown"),
            ],
            "com.apple.CoreSimulator.SimRuntime.iOS-27-2-iPhone19-4": [device("duo-27-2", "iPhone Duo", "Shutdown")],
            "com.apple.CoreSimulator.SimRuntime.watchOS-12-0": [device("watch", "Apple Watch", "Shutdown")],
        }}))
        .unwrap()
    }

    fn udids(json: &[u8]) -> Vec<String> {
        parse_devices(json).into_iter().map(|d| d.udid).collect()
    }

    #[test]
    fn lists_iphones_then_ipads_once_each_on_newest_ios() {
        assert_eq!(udids(&list(false)), ["pro", "air", "duo-27-2", "ipad"]);
        assert_eq!(parse_devices(&list(false))[2].os, "iOS 27.2");
    }

    #[test]
    fn prefers_the_running_copy_of_a_device() {
        assert_eq!(udids(&list(true)), ["duo-27-1", "pro", "air", "ipad"]);
    }
}
