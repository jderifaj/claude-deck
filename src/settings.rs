//! ~/.claude-deck/settings.json

use std::{collections::BTreeMap, fs, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{catalog::deck_dir, git_identity::GitIdentity};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Command started in each session (run through a login shell).
    pub claude_command: String,
    /// Give every session its own worktree, even the first one in a project.
    pub always_isolate: bool,
    pub font_family: String,
    pub font_size: f32,
    pub color_theme: Option<ThemeSetting>,
    /// Claude account (by email) that new windows start with.
    pub default_account_email: Option<String>,
    /// GitHub identities a window can use.
    pub git_identities: Vec<GitIdentity>,
    /// Claude account email -> GitHub username its windows use by default.
    pub github_for_account: BTreeMap<String, String>,
    pub recent_projects: Vec<PathBuf>,
    /// Side-bar tips the user closed.
    pub dismissed_tips: Vec<String>,
    /// Draw the window over a Liquid Glass backdrop (applies on restart).
    pub liquid_glass: bool,
    /// The right panel (charge codes) was open when last used.
    pub right_panel_open: bool,
    /// The charge codes panel is shown in the right panel.
    pub charge_codes_open: bool,
    /// The optional Asana panel under the charge codes is shown.
    pub asana_panel_open: bool,
    /// Folder -> dev server command the user approved in the webserver
    /// dialog (without port arguments), so later starts skip the question.
    pub approved_servers: BTreeMap<PathBuf, String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThemeSetting {
    pub label: String,
    pub path: PathBuf,
    pub ui_theme: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            claude_command: "claude".into(),
            always_isolate: false,
            font_family: "Menlo".into(),
            font_size: 13.,
            color_theme: None,
            default_account_email: None,
            git_identities: Vec::new(),
            github_for_account: BTreeMap::new(),
            recent_projects: Vec::new(),
            dismissed_tips: Vec::new(),
            liquid_glass: true,
            right_panel_open: false,
            charge_codes_open: true,
            asana_panel_open: false,
            approved_servers: BTreeMap::new(),
        }
    }
}

fn path() -> PathBuf {
    deck_dir().join("settings.json")
}

impl Settings {
    pub fn load() -> Self {
        fs::read_to_string(path()).ok().and_then(|s| json5::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let _ = fs::create_dir_all(deck_dir());
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path(), json);
        }
    }

    pub fn git_identity(&self, github_user: &str) -> Option<&GitIdentity> {
        self.git_identities.iter().find(|g| g.github_user == github_user)
    }

    /// The GitHub identity a Claude account uses unless a window overrides it.
    pub fn git_for_email(&self, email: &str) -> Option<&GitIdentity> {
        self.git_identity(self.github_for_account.get(email)?)
    }

    /// The GitHub identity a folder points at (saved login, else remote
    /// owner), if it's one of ours.
    pub fn git_for_folder(&self, logins: &crate::git_identity::FolderLogins) -> Option<&GitIdentity> {
        logins.best_first().find_map(|l| self.git_identity(l))
    }

    pub fn add_recent(&mut self, dir: PathBuf) {
        self.recent_projects.retain(|p| p != &dir);
        self.recent_projects.insert(0, dir);
        self.recent_projects.truncate(10);
    }
}
