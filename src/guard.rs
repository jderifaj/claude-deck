//! Safety gates for sessions working on Claude Deck itself (its repo or one of
//! its worktrees), passed as the session's `--settings` file. Sessions in any
//! other folder get the same file without the gates (just the status line).
//! That level sits above user and project settings, and deny/ask rules
//! from any level beat allow rules, so a repo's own settings can't loosen them.
//!
//! - Bypass-permissions mode is disabled, even if `claudeCommand` asks for it.
//! - Shell commands run in Claude Code's OS sandbox (macOS Seatbelt): they can
//!   write only inside the session's folder (and temp), and can't read the
//!   secrets below. Running a command outside the sandbox always asks first.
//! - Claude's file tools can't read secrets or private app data, can't edit
//!   system folders, shell startup files or these gates, and ask before
//!   reading personal folders (Documents, Desktop, Downloads, ...).
//! - A few system-level commands are refused or always ask.

use std::{fs, path::{Path, PathBuf}};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::{catalog::deck_dir, worktree::Workspace};

/// Secrets and private app data (folders end in `/`): never readable by the file tools or by
/// anything the shell sandbox runs.
const SECRETS: &[&str] = &[
    "~/.ssh/",
    "~/.gnupg/",
    "~/.aws/",
    "~/.azure/",
    "~/.config/gcloud/",
    "~/.kube/",
    "~/.docker/config.json",
    "~/.netrc",
    "~/.npmrc",
    "~/.pypirc",
    "~/.git-credentials",
    "~/.config/gh/",
    "~/.claude-deck/github/",
    "~/.claude-deck/asana.json",
    "~/.claude/.credentials.json",
    "~/Library/Keychains/",
    "~/Library/Cookies/",
    "~/Library/Messages/",
    "~/Library/Mail/",
    "~/Library/Safari/",
    "~/Library/Calendars/",
    "~/Library/Application Support/AddressBook/",
    "~/Library/Application Support/com.apple.TCC/",
    "~/Library/Application Support/Google/Chrome/",
    "~/Library/Application Support/Firefox/",
    "~/Pictures/Photos Library.photoslibrary/",
];

/// Folders and files the file tools may never change.
const NO_EDIT: &[&str] = &[
    "//System/**",
    "//usr/**",
    "//bin/**",
    "//sbin/**",
    "//etc/**",
    "//private/etc/**",
    "//Library/**",
    "//Applications/**",
    "~/Library/**",
    "~/Applications/**",
    "~/.ssh/**",
    "~/.gnupg/**",
    "~/.aws/**",
    "~/.zshrc",
    "~/.zshenv",
    "~/.zprofile",
    "~/.zlogin",
    "~/.bashrc",
    "~/.bash_profile",
    "~/.profile",
    "~/.gitconfig",
    "~/.config/git/**",
    "~/.claude.json",
    "~/.claude/settings.json",
    "~/.claude/settings.local.json",
    // Claude Deck's own settings, these gates, and account settings.
    "~/.claude-deck/*.json",
    "~/.claude-deck/sessions/**",
    "~/.claude-deck/github/**",
    "~/.claude-deck/accounts/*/settings.json",
    "~/.claude-deck/accounts/*/settings.local.json",
    "~/.claude-deck/accounts/*/.claude.json",
];

/// Places the shell sandbox may never write, even when the session's folder
/// contains them (e.g. a session opened on the home folder).
const NO_SHELL_WRITE: &[&str] = &[
    "~/Library",
    "~/.ssh",
    "~/.gnupg",
    "~/.aws",
    "~/.zshrc",
    "~/.zshenv",
    "~/.zprofile",
    "~/.zlogin",
    "~/.bashrc",
    "~/.bash_profile",
    "~/.profile",
    "~/.gitconfig",
    "~/.config/git",
    "~/.claude.json",
    "~/.claude",
    "~/.claude-deck",
];

/// Personal folders: reading them asks first, unless the session works there.
const PERSONAL: &[&str] =
    &["Documents", "Desktop", "Downloads", "Pictures", "Movies", "Music", "Library/Mobile Documents"];

const DENY_BASH: &[&str] = &[
    "Bash(sudo)",
    "Bash(sudo *)",
    "Bash(su)",
    "Bash(su *)",
    "Bash(security *)",
    "Bash(csrutil *)",
    "Bash(rm -rf /)",
    "Bash(rm -rf /*)",
    "Bash(rm -rf ~)",
    "Bash(rm -rf ~/)",
    "Bash(rm -rf ~/*)",
];

const ASK_BASH: &[&str] = &[
    // Running a command outside the sandbox, in every permission mode.
    "Bash(dangerouslyDisableSandbox:true)",
    "Bash(osascript *)",
    "Bash(launchctl *)",
    "Bash(crontab *)",
    "Bash(defaults write *)",
    "Bash(defaults delete *)",
    "Bash(diskutil *)",
    "Bash(chmod -R *)",
    "Bash(chown *)",
];

/// The settings for one session: these gates (only when it works on Claude
/// Deck itself) plus `extra` (e.g. the status line), written to
/// ~/.claude-deck/sessions/settings-<id>.json.
pub fn session_settings(session_id: usize, workspace: &Workspace, extra: Option<(&str, Value)>) -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home folder")?;
    let mut json = if is_claude_deck(&workspace.project) { settings(&home, workspace) } else { json!({}) };
    if let Some((key, value)) = extra {
        json[key] = value;
    }
    let dir = deck_dir().join("sessions");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("settings-{session_id}.json"));
    fs::write(&path, serde_json::to_vec_pretty(&json)?).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Whether `project` is Claude Deck's own repo: its Cargo.toml names the
/// `claude-deck` package. A session's worktrees share the main checkout as
/// their `project`, so they count too.
fn is_claude_deck(project: &Path) -> bool {
    fs::read_to_string(project.join("Cargo.toml")).is_ok_and(|toml| is_claude_deck_manifest(&toml))
}

fn is_claude_deck_manifest(toml: &str) -> bool {
    let mut in_package = false;
    for line in toml.lines().map(str::trim) {
        if line.starts_with('[') {
            in_package = line == "[package]";
        } else if in_package {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim() == "name" {
                    return value.trim().trim_matches(['"', '\'']) == "claude-deck";
                }
            }
        }
    }
    false
}

fn settings(home: &Path, workspace: &Workspace) -> Value {
    let read_rule = |p: &str| match p.strip_suffix('/') {
        Some(dir) => format!("Read({dir}/**)"),
        None => format!("Read({p})"),
    };
    let works_in = |dir: &Path| workspace.cwd.starts_with(dir) || workspace.project.starts_with(dir);
    let ask_read = PERSONAL
        .iter()
        .filter(|f| !works_in(&home.join(f)))
        .map(|f| format!("Read(~/{f}/**)"));

    let deny: Vec<String> = SECRETS
        .iter()
        .map(|p| read_rule(p))
        .chain(NO_EDIT.iter().map(|p| format!("Edit({p})")))
        .chain(DENY_BASH.iter().map(|s| s.to_string()))
        .collect();
    let ask: Vec<String> = ask_read.chain(ASK_BASH.iter().map(|s| s.to_string())).collect();

    json!({
        "permissions": {
            "disableBypassPermissionsMode": "disable",
            "deny": deny,
            "ask": ask,
        },
        "sandbox": {
            "enabled": true,
            "failIfUnavailable": true,
            "filesystem": {
                "denyRead": SECRETS.iter().map(|p| p.trim_end_matches('/')).collect::<Vec<_>>(),
                "denyWrite": NO_SHELL_WRITE,
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(dir: &str) -> Workspace {
        Workspace { cwd: PathBuf::from(dir), project: PathBuf::from(dir), isolation: None }
    }

    fn list(v: &Value, key: &str) -> Vec<String> {
        v["permissions"][key].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn gates_are_on() {
        let v = settings(Path::new("/Users/me"), &workspace("/Users/me/Sites/app"));
        assert_eq!(v["permissions"]["disableBypassPermissionsMode"], "disable");
        assert_eq!(v["sandbox"]["enabled"], true);
        assert_eq!(v["sandbox"]["failIfUnavailable"], true);
        let deny = list(&v, "deny");
        assert!(deny.contains(&"Read(~/.ssh/**)".into()));
        assert!(deny.contains(&"Read(~/.netrc)".into()));
        assert!(deny.contains(&"Read(~/.docker/config.json)".into()));
        assert!(deny.contains(&"Read(~/.git-credentials)".into()));
        assert!(deny.contains(&"Read(~/Pictures/Photos Library.photoslibrary/**)".into()));
        assert_eq!(v["sandbox"]["filesystem"]["denyRead"][0], "~/.ssh");
        assert!(deny.contains(&"Edit(//System/**)".into()));
        assert!(deny.contains(&"Edit(~/.claude-deck/sessions/**)".into()));
        let ask = list(&v, "ask");
        assert!(ask.contains(&"Bash(dangerouslyDisableSandbox:true)".into()));
        assert!(ask.contains(&"Read(~/Documents/**)".into()));
    }

    #[test]
    fn gates_only_for_claude_deck() {
        assert!(is_claude_deck_manifest("[package]\nname = \"claude-deck\"\nversion = \"0.1.0\"\n"));
        assert!(!is_claude_deck_manifest("[package]\nname = \"other-app\"\n"));
        assert!(!is_claude_deck_manifest("[dependencies]\nname = \"claude-deck\"\n"));
        assert!(is_claude_deck(Path::new(env!("CARGO_MANIFEST_DIR"))));
        assert!(!is_claude_deck(Path::new("/nonexistent/folder")));
    }

    #[test]
    fn no_read_prompt_for_the_folder_the_session_works_in() {
        let v = settings(Path::new("/Users/me"), &workspace("/Users/me/Documents/app"));
        let ask = list(&v, "ask");
        assert!(!ask.contains(&"Read(~/Documents/**)".into()));
        assert!(ask.contains(&"Read(~/Desktop/**)".into()));
    }
}

