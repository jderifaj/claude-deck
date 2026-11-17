//! GitHub identities: the name/email a window's commits use, the SSH key
//! its pushes authenticate with, the login its HTTPS pushes use, and which
//! `gh` CLI login it sees. Applied per window through environment variables;
//! git config is never changed (apart from the folder's saved login).

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

const VARS: [&str; 12] = [
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_SSH_COMMAND",
    "GH_CONFIG_DIR",
    "CLAUDE_DECK_GITHUB",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_KEY_0",
    "GIT_CONFIG_VALUE_0",
    "GIT_CONFIG_KEY_1",
    "GIT_CONFIG_VALUE_1",
];

/// Git config key for the credential helpers used with GitHub over HTTPS.
const HTTPS_HELPER_KEY: &str = "credential.https://github.com.helper";

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

    /// This identity with the author the repo sets for itself (`user.name`
    /// and `user.email` in its own git config), when it sets one: the
    /// environment would otherwise override it without a word.
    pub fn for_repo(mut self, logins: &FolderLogins) -> Self {
        if let Some(name) = &logins.author_name {
            self.name = name.clone();
        }
        if let Some(email) = &logins.author_email {
            self.email = email.clone();
        }
        self
    }

    /// A git credential helper that signs HTTPS pushes to github.com in as
    /// this identity, the way `ssh_key` does for SSH remotes. Without it git
    /// uses whichever GitHub login the Mac's keychain holds, which can be
    /// another account ("Repository not found"). The token comes from the
    /// `gh` CLI's login for this user each time git asks, and goes straight
    /// to git: it's never stored, logged or put in the environment. If `gh`
    /// has no login for this user, git's usual helpers answer as before.
    /// None for a username that couldn't be a GitHub login.
    pub fn https_helper(&self) -> Option<String> {
        let user = &self.github_user;
        let valid = !user.is_empty() && user.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        valid.then(|| {
            format!(
                "!f() {{ test \"$1\" = get || exit 0; \
                 t=$(gh auth token --hostname github.com --user {user} 2>/dev/null); \
                 if test -n \"$t\"; then echo username={user}; echo \"password=$t\"; \
                 else gh auth git-credential get 2>/dev/null || git credential-osxkeychain get; fi; }}; f"
            )
        })
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
        if let Some(helper) = self.https_helper() {
            // An empty value first drops the helpers set in git config, so
            // ours is the one asked.
            cmd.env("GIT_CONFIG_COUNT", "2");
            cmd.env("GIT_CONFIG_KEY_0", HTTPS_HELPER_KEY);
            cmd.env("GIT_CONFIG_VALUE_0", "");
            cmd.env("GIT_CONFIG_KEY_1", HTTPS_HELPER_KEY);
            cmd.env("GIT_CONFIG_VALUE_1", helper);
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
    /// The commit author the repo sets in its own git config, if it does.
    pub author_name: Option<String>,
    pub author_email: Option<String>,
}

impl FolderLogins {
    /// The saved login, then the remote's owner.
    pub fn best_first(&self) -> impl Iterator<Item = &str> {
        self.saved.iter().chain(&self.owner).map(String::as_str)
    }

    /// "Name <email>" when the repo sets its own commit author.
    pub fn author(&self) -> Option<String> {
        match (&self.author_name, &self.author_email) {
            (Some(name), Some(email)) => Some(format!("{name} <{email}>")),
            (Some(name), None) => Some(name.clone()),
            (None, Some(email)) => Some(format!("<{email}>")),
            (None, None) => None,
        }
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
        author_name: git(dir, &["config", "--local", "--get", "user.name"]),
        author_email: git(dir, &["config", "--local", "--get", "user.email"]),
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
        fn get_from(cmd: &CommandBuilder, k: &str) -> Option<String> {
            cmd.get_env(k).map(|v| v.to_string_lossy().into_owned())
        }
        let get = |k: &str| get_from(&cmd, k);
        assert_eq!(get("GIT_AUTHOR_NAME").as_deref(), Some("My Name"));
        assert_eq!(get("GIT_COMMITTER_EMAIL").as_deref(), Some("me@example.com"));
        let ssh = get("GIT_SSH_COMMAND").unwrap();
        assert!(ssh.contains("id_ed25519_personal") && ssh.contains("IdentityAgent=none") && !ssh.contains('~'));
        assert_eq!(get("GH_CONFIG_DIR"), None, "personal identity uses the normal gh config");
        // HTTPS pushes: git's own helpers are dropped, then ours is added.
        assert_eq!(get("GIT_CONFIG_COUNT").as_deref(), Some("2"));
        assert_eq!(get("GIT_CONFIG_KEY_0").as_deref(), Some(HTTPS_HELPER_KEY));
        assert_eq!(get("GIT_CONFIG_VALUE_0").as_deref(), Some(""));
        assert_eq!(get("GIT_CONFIG_KEY_1").as_deref(), Some(HTTPS_HELPER_KEY));
        assert!(get("GIT_CONFIG_VALUE_1").unwrap().contains("gh auth token --hostname github.com --user jderifaj"));

        clear_env(&mut cmd);
        assert_eq!(get_from(&cmd, "GIT_AUTHOR_NAME"), None);
        assert_eq!(get_from(&cmd, "GIT_CONFIG_COUNT"), None);
    }

    fn identity(user: &str) -> GitIdentity {
        GitIdentity { github_user: user.into(), name: "Work Name".into(), email: "me@work.example".into(), ssh_key: None, gh_config_dir: None }
    }

    #[test]
    fn the_repos_own_author_wins() {
        let repo = FolderLogins {
            author_name: Some("me".into()),
            author_email: Some("12345+me@users.noreply.github.com".into()),
            ..Default::default()
        };
        let id = identity("work-user").for_repo(&repo);
        assert_eq!((id.name.as_str(), id.email.as_str()), ("me", "12345+me@users.noreply.github.com"));
        assert_eq!(id.github_user, "work-user", "pushes still use the chosen account");
        assert_eq!(repo.author().as_deref(), Some("me <12345+me@users.noreply.github.com>"));

        let unset = FolderLogins::default();
        assert_eq!(identity("work-user").for_repo(&unset), identity("work-user"));
        assert_eq!(unset.author(), None);
    }

    #[test]
    fn no_https_helper_for_an_odd_username() {
        assert!(identity("me; rm -rf ~").https_helper().is_none());
        assert!(identity("").https_helper().is_none());
    }

    /// Runs the helper the way git does, with stand-ins for `gh` and for
    /// git's keychain helper on PATH.
    #[test]
    fn https_helper_hands_git_the_identitys_token() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("claude-deck-helper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = |name: &str, body: &str| {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        // `gh` knows a token for "me" only, and has no active login.
        script("gh", r#"[ "$1 $2" = "auth token" ] && [ "$6" = me ] && echo tok-me && exit 0; exit 1"#);
        script("git-credential-osxkeychain", "echo username=keychain; echo password=tok-keychain");

        let ask = |user: &str| {
            let helper = identity(user).https_helper().unwrap();
            let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap_or_default());
            let mut child = Command::new("git")
                .args(["-c", "credential.helper=", "-c", &format!("{HTTPS_HELPER_KEY}={helper}"), "credential", "fill"])
                .env("PATH", path)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                // Helpers are looked up here first, so the stand-in is run,
                // never the real keychain helper.
                .env("GIT_EXEC_PATH", &dir)
                .env_remove("GIT_CONFIG_COUNT")
                .env_remove("GIT_ASKPASS")
                .current_dir(&dir)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            use std::io::Write;
            child.stdin.take().unwrap().write_all(b"protocol=https\nhost=github.com\n\n").unwrap();
            String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
        };
        let mine = ask("me");
        assert!(mine.contains("username=me") && mine.contains("password=tok-me"), "{mine}");
        // No `gh` login for this user: git's keychain helper answers instead.
        let other = ask("someone-else");
        assert!(other.contains("username=keychain") && other.contains("password=tok-keychain"), "{other}");
    }
}
