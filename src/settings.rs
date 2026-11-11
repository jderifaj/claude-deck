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
    /// GitHub username -> Chrome profile folder its GitHub links open in.
    pub chrome_profile_for_github: BTreeMap<String, String>,
    /// GitHub usernames the user confirmed are signed in to GitHub in their
    /// Chrome profile (Claude Deck can't see browser logins itself).
    pub github_web_confirmed: Vec<String>,
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
            chrome_profile_for_github: BTreeMap::new(),
            github_web_confirmed: Vec::new(),
        }
    }
}

fn path() -> PathBuf {
    deck_dir().join("settings.json")
}

impl Settings {
    pub fn load() -> Self {
        fs::read_to_string(path()).map(|s| Self::parse(&s)).unwrap_or_default()
    }

    /// Settings from the file's text. Missing fields take their defaults,
    /// and a file that doesn't parse gives the defaults.
    fn parse(text: &str) -> Self {
        json5::from_str(text).unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_identity::FolderLogins;

    fn identity(user: &str) -> GitIdentity {
        GitIdentity {
            github_user: user.into(),
            name: user.into(),
            email: format!("{user}@example.com"),
            ssh_key: None,
            gh_config_dir: None,
        }
    }

    #[test]
    fn missing_fields_take_their_defaults() {
        let s = Settings::parse(r#"{ fontSize: 15, alwaysIsolate: true }"#);
        assert_eq!(s.font_size, 15.);
        assert!(s.always_isolate);
        assert_eq!(s.claude_command, "claude");
        assert_eq!(s.font_family, "Menlo");
        assert!(s.liquid_glass && s.charge_codes_open);
        assert!(s.recent_projects.is_empty());
    }

    #[test]
    fn reads_json5_comments_and_trailing_commas() {
        let s = Settings::parse("{\n  // hand edited\n  claudeCommand: 'claude --verbose',\n}");
        assert_eq!(s.claude_command, "claude --verbose");
    }

    #[test]
    fn unreadable_file_gives_defaults() {
        let s = Settings::parse("{ not json");
        assert_eq!(s.claude_command, "claude");
        assert_eq!(s.font_size, 13.);
    }

    #[test]
    fn saved_json_round_trips_with_camel_case_keys() {
        let mut s = Settings::default();
        s.font_size = 16.;
        s.github_for_account.insert("me@example.com".into(), "octocat".into());
        s.chrome_profile_for_github.insert("octocat".into(), "Profile 1".into());
        s.approved_servers.insert("/tmp/site".into(), "npm run dev".into());
        let json = serde_json::to_string_pretty(&s).unwrap();
        assert!(json.contains("\"githubForAccount\""), "{json}");
        assert!(json.contains("\"chromeProfileForGithub\""), "{json}");

        let back = Settings::parse(&json);
        assert_eq!(back.font_size, 16.);
        assert_eq!(back.github_for_account, s.github_for_account);
        assert_eq!(back.chrome_profile_for_github, s.chrome_profile_for_github);
        assert_eq!(back.approved_servers, s.approved_servers);
    }

    #[test]
    fn add_recent_moves_to_front_without_duplicates_and_keeps_ten() {
        let mut s = Settings::default();
        for i in 0..12 {
            s.add_recent(PathBuf::from(format!("/p/{i}")));
        }
        assert_eq!(s.recent_projects.len(), 10);
        assert_eq!(s.recent_projects[0], PathBuf::from("/p/11"));
        assert_eq!(s.recent_projects[9], PathBuf::from("/p/2"));

        s.add_recent(PathBuf::from("/p/5"));
        assert_eq!(s.recent_projects.len(), 10);
        assert_eq!(s.recent_projects[0], PathBuf::from("/p/5"));
        assert_eq!(s.recent_projects.iter().filter(|p| p.ends_with("5")).count(), 1);
    }

    #[test]
    fn finds_the_identity_for_an_account() {
        let mut s = Settings::default();
        s.git_identities = vec![identity("work"), identity("personal")];
        s.github_for_account.insert("me@example.com".into(), "personal".into());
        s.github_for_account.insert("old@example.com".into(), "removed".into());

        assert_eq!(s.git_for_email("me@example.com").unwrap().github_user, "personal");
        assert!(s.git_for_email("old@example.com").is_none(), "login no longer saved");
        assert!(s.git_for_email("nobody@example.com").is_none());
    }

    #[test]
    fn folder_prefers_the_saved_login_then_the_remote_owner() {
        let mut s = Settings::default();
        s.git_identities = vec![identity("work"), identity("personal")];
        let both = FolderLogins { saved: Some("personal".into()), owner: Some("work".into()), web: None };
        assert_eq!(s.git_for_folder(&both).unwrap().github_user, "personal");

        let unknown_saved = FolderLogins { saved: Some("someone".into()), owner: Some("work".into()), web: None };
        assert_eq!(s.git_for_folder(&unknown_saved).unwrap().github_user, "work");

        let neither = FolderLogins { saved: None, owner: Some("someone".into()), web: None };
        assert!(s.git_for_folder(&neither).is_none());
    }
}
