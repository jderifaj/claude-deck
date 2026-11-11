//! Opening GitHub links in the Chrome profile that belongs to a GitHub
//! account. The app can't see which account a browser is signed in to, so
//! each GitHub login gets a Chrome profile the user picks once.

use std::{
    path::PathBuf,
    process::{Command, Stdio},
};

use anyhow::{Result, bail};

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// Folder name under Chrome's data dir ("Default", "Profile 1").
    pub dir: String,
    /// Name shown in Chrome's profile menu.
    pub name: String,
    /// Google account the profile syncs with, if any.
    pub email: String,
}

fn local_state() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Google/Chrome/Local State"))
}

/// Chrome is installed and has been run at least once.
pub fn installed() -> bool {
    local_state().is_some_and(|p| p.exists())
}

/// Chrome's profiles, from its `Local State` file (only names and emails
/// are read).
pub fn profiles() -> Vec<Profile> {
    let Some(text) = local_state().and_then(|p| std::fs::read_to_string(p).ok()) else { return Vec::new() };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };
    let Some(cache) = json.pointer("/profile/info_cache").and_then(|c| c.as_object()) else { return Vec::new() };
    let mut list: Vec<Profile> = cache
        .iter()
        .map(|(dir, info)| Profile {
            dir: dir.clone(),
            name: info.get("name").and_then(|v| v.as_str()).unwrap_or(dir).to_string(),
            email: info.get("user_name").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        })
        .collect();
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}

/// A profile folder name Chrome isn't using yet; Chrome creates the profile
/// the first time it's opened with it.
pub fn new_profile_dir(existing: &[Profile], taken: &[&str]) -> String {
    (1..)
        .map(|n| format!("Profile {n}"))
        .find(|d| !existing.iter().any(|p| &p.dir == d) && !taken.contains(&d.as_str()))
        .expect("an unused profile number")
}

/// Open `url` in Chrome profile `dir` (a running Chrome opens it there).
pub fn open(dir: &str, url: &str) -> Result<()> {
    let status = Command::new("open")
        .args(["-na", "Google Chrome", "--args"])
        .arg(format!("--profile-directory={dir}"))
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        bail!("Chrome didn't open ({status})");
    }
    Ok(())
}

/// Whether a GitHub repo page loads for someone who isn't signed in: 200
/// for a public repo, 404 for a private one (or one that doesn't exist).
/// `None` if GitHub couldn't be reached. Blocks on the network, so call it
/// off the main thread.
pub fn is_public(repo_web: &str) -> Option<bool> {
    let out = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-I", "-L", "--max-time", "5", "-w", "%{http_code}"])
        .arg(repo_web)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "200" => Some(true),
        "404" => Some(false),
        _ => None,
    }
}

/// GitHub's sign-in page. Without a `return_to`, a profile signed in as
/// someone else lands on their dashboard (showing who it is) rather than on
/// a 404.
pub const SIGN_IN_URL: &str = "https://github.com/login";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_profile_skips_used_names() {
        let p = |d: &str| Profile { dir: d.into(), name: d.into(), email: String::new() };
        assert_eq!(new_profile_dir(&[p("Default"), p("Profile 1")], &["Profile 2"]), "Profile 3");
    }
}
