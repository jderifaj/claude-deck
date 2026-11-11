//! GitHub identities: the name/email a window's commits use, the SSH key
//! its pushes authenticate with, and which `gh` CLI login it sees. Applied
//! per window through environment variables; git config is never changed.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use portable_pty::CommandBuilder;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitIdentity {
    /// GitHub username, e.g. "octocat".
    pub github_user: String,
    /// Commit author/committer.
    pub name: String,
    pub email: String,
    /// Key used for every SSH git remote in the window, whatever host alias
    /// the remote uses.
    pub ssh_key: Option<PathBuf>,
    /// A separate `gh` config dir with its own login; `None` uses the normal
    /// `~/.config/gh`.
    pub gh_config_dir: Option<PathBuf>,
}

const VARS: [&str; 7] = [
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_SSH_COMMAND",
    "GH_CONFIG_DIR",
    "CLAUDE_DECK_GITHUB",
];

pub fn expand_tilde(path: &PathBuf) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        Err(_) => path.clone(),
    }
}

impl GitIdentity {
    /// The `ssh` command git should use: only this key, and not the agent,
    /// which would otherwise offer whichever key it holds first.
    pub fn ssh_command(&self) -> Option<String> {
        let key = expand_tilde(self.ssh_key.as_ref()?);
        Some(format!("ssh -i '{}' -o IdentitiesOnly=yes -o IdentityAgent=none", key.display()))
    }

    pub fn apply_env(&self, cmd: &mut CommandBuilder) {
        clear_env(cmd);
        cmd.env("GIT_AUTHOR_NAME", &self.name);
        cmd.env("GIT_AUTHOR_EMAIL", &self.email);
        cmd.env("GIT_COMMITTER_NAME", &self.name);
        cmd.env("GIT_COMMITTER_EMAIL", &self.email);
        cmd.env("CLAUDE_DECK_GITHUB", &self.github_user);
        if let Some(ssh) = self.ssh_command() {
            cmd.env("GIT_SSH_COMMAND", ssh);
        }
        if let Some(dir) = &self.gh_config_dir {
            let dir = expand_tilde(dir);
            let _ = std::fs::create_dir_all(&dir);
            cmd.env("GH_CONFIG_DIR", dir);
        }
    }
}

/// Key in a repo's local `.git/config` (never committed) naming the GitHub
/// login its windows use. Shared with the user's Claude Code hook.
const FOLDER_KEY: &str = "claude.githubUser";

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

/// Owner in a GitHub remote URL: `https://github.com/<owner>/...`,
/// `git@<host-or-alias>:<owner>/...` or `ssh://git@<host>/<owner>/...`.
pub fn remote_owner(url: &str) -> Option<String> {
    let rest = if let Some(r) = url.strip_prefix("https://") {
        let r = r.split_once('@').map_or(r, |(_, r)| r);
        r.strip_prefix("github.com/")?
    } else if let Some(r) = url.strip_prefix("ssh://") {
        r.split_once('/')?.1
    } else {
        url.split_once('@')?.1.split_once(':')?.1
    };
    let owner = rest.split('/').next()?;
    (!owner.is_empty() && rest.contains('/')).then(|| owner.to_string())
}

/// Web page of a GitHub remote: `https://github.com/<owner>/<repo>`. SSH
/// remotes count when their host (or `~/.ssh/config` alias, such as
/// `github-personal`) names GitHub.
pub fn remote_web_url(url: &str) -> Option<String> {
    let (host, path) = if let Some(r) = url.strip_prefix("https://") {
        let r = r.split_once('@').map_or(r, |(_, r)| r);
        ("github.com", r.strip_prefix("github.com/")?)
    } else if let Some(r) = url.strip_prefix("ssh://") {
        let (host, path) = r.split_once('/')?;
        (host.rsplit('@').next()?, path)
    } else {
        let (host, path) = url.split_once('@')?.1.split_once(':')?;
        (host, path)
    };
    if !host.split(':').next()?.contains("github") {
        return None;
    }
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    (!owner.is_empty() && !repo.is_empty() && parts.next().is_none()).then(|| format!("https://github.com/{owner}/{repo}"))
}

/// Page for `branch` in the repo at `repo_web`: the branch's tree once it has
/// been pushed to `origin`, else the repo's front page (an unpushed `deck/N`
/// branch would 404). Runs `git`, so call it off the main thread.
pub fn branch_web_url(dir: &Path, repo_web: &str, branch: Option<&str>) -> String {
    match branch {
        Some(b) if git(dir, &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/origin/{b}")]).is_some() => {
            format!("{repo_web}/tree/{b}")
        }
        _ => repo_web.to_string(),
    }
}

/// GitHub logins a folder points at.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FolderLogins {
    /// Saved for the repo (`claude.githubUser`).
    pub saved: Option<String>,
    /// Owner of its `origin` remote.
    pub owner: Option<String>,
    /// Web page of its `origin` remote, when that's on GitHub.
    pub web: Option<String>,
}

impl FolderLogins {
    /// The saved login, then the remote's owner.
    pub fn best_first(&self) -> impl Iterator<Item = &str> {
        self.saved.iter().chain(&self.owner).map(String::as_str)
    }
}

/// Look up a folder's logins. Runs `git`, so call it off the main thread:
/// the first `git` after a reboot or an Xcode update can take seconds.
pub fn folder_logins(dir: &Path) -> FolderLogins {
    let origin = git(dir, &["remote", "get-url", "origin"]);
    FolderLogins {
        saved: git(dir, &["config", "--get", FOLDER_KEY]),
        owner: origin.as_deref().and_then(remote_owner),
        web: origin.as_deref().and_then(remote_web_url),
    }
}

/// Remember `login` for the folder's repo (or forget it with `None`). Runs
/// `git`, so call it off the main thread.
pub fn save_folder_login(dir: &Path, login: Option<&str>) {
    let _ = match login {
        Some(l) => git(dir, &["config", "--local", FOLDER_KEY, l]),
        None => git(dir, &["config", "--local", "--unset", FOLDER_KEY]),
    };
}

/// Don't let a window inherit another identity from the app's environment.
pub fn clear_env(cmd: &mut CommandBuilder) {
    for var in VARS {
        cmd.env_remove(var);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_owners() {
        assert_eq!(remote_owner("git@github-personal:jderifaj/claude-deck.git").as_deref(), Some("jderifaj"));
        assert_eq!(remote_owner("https://github.com/jderifaj/example.git").as_deref(), Some("jderifaj"));
        assert_eq!(remote_owner("https://x@github.com/org/repo").as_deref(), Some("org"));
        assert_eq!(remote_owner("ssh://git@github.com/org/repo.git").as_deref(), Some("org"));
        assert_eq!(remote_owner("https://gitlab.com/org/repo"), None);
        assert_eq!(remote_owner("/local/path"), None);
    }

    #[test]
    fn remote_web_urls() {
        let web = |u| remote_web_url(u);
        assert_eq!(web("git@github-personal:jderifaj/claude-deck.git").as_deref(), Some("https://github.com/jderifaj/claude-deck"));
        assert_eq!(web("https://github.com/jderifaj/example.git").as_deref(), Some("https://github.com/jderifaj/example"));
        assert_eq!(web("https://x@github.com/org/repo/").as_deref(), Some("https://github.com/org/repo"));
        assert_eq!(web("ssh://git@github.com:22/org/repo.git").as_deref(), Some("https://github.com/org/repo"));
        assert_eq!(web("git@gitlab.com:org/repo.git"), None);
        assert_eq!(web("https://gitlab.com/org/repo"), None);
        assert_eq!(web("/local/path"), None);
    }

    #[test]
    fn env_sets_author_key_and_gh_dir() {
        let id = GitIdentity {
            github_user: "jderifaj".into(),
            name: "My Name".into(),
            email: "me@example.com".into(),
            ssh_key: Some(PathBuf::from("~/.ssh/id_ed25519_personal")),
            gh_config_dir: None,
        };
        let mut cmd = CommandBuilder::new("true");
        cmd.env("GH_CONFIG_DIR", "/elsewhere");
        id.apply_env(&mut cmd);
        let get = |k: &str| cmd.get_env(k).map(|v| v.to_string_lossy().into_owned());
        assert_eq!(get("GIT_AUTHOR_NAME").as_deref(), Some("My Name"));
        assert_eq!(get("GIT_COMMITTER_EMAIL").as_deref(), Some("me@example.com"));
        let ssh = get("GIT_SSH_COMMAND").unwrap();
        assert!(ssh.contains("id_ed25519_personal") && ssh.contains("IdentityAgent=none") && !ssh.contains('~'));
        assert_eq!(get("GH_CONFIG_DIR"), None, "personal identity uses the normal gh config");
    }
}
