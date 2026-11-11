//! Claude accounts. Each account is a Claude Code profile: the default one
//! (~/.claude) or a separate `CLAUDE_CONFIG_DIR` under
//! ~/.claude-deck/accounts/<id>, which keeps its own login and conversations.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, SystemTime},
};

use gpui::{Hsla, Rgba};
use serde::Deserialize;

use crate::catalog::deck_dir;

pub const DEFAULT_ID: &str = "default";

/// Set by a running Claude Code for the processes it starts. When the app is
/// launched from inside a Claude session they'd otherwise reach our windows,
/// which then treat themselves as that session's child (e.g. transcript
/// saving turns off). The user's own `CLAUDE_CODE_*` settings are kept.
const PARENT_SESSION_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "AI_AGENT",
];

#[derive(Clone, Debug, PartialEq)]
pub struct Account {
    pub id: String,
    /// `None` for the default profile.
    pub config_dir: Option<PathBuf>,
    pub status: Option<AuthStatus>,
    /// Custom avatar image, looked up whenever the status is refreshed.
    pub avatar: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    #[serde(default)]
    pub logged_in: bool,
    pub email: Option<String>,
    pub org_name: Option<String>,
    pub subscription_type: Option<String>,
    pub auth_method: Option<String>,
}

impl Account {
    pub fn default_account() -> Self {
        Self { id: DEFAULT_ID.into(), config_dir: None, status: None, avatar: None }
    }

    fn email(&self) -> Option<&str> {
        self.status.as_ref().filter(|s| s.logged_in).and_then(|s| s.email.as_deref())
    }

    /// Email when signed in, otherwise a description of the profile.
    pub fn label(&self) -> String {
        match (&self.status, self.email()) {
            (_, Some(email)) => email.to_string(),
            (Some(s), None) if s.logged_in => format!("Signed in ({})", s.auth_method.as_deref().unwrap_or("API")),
            (Some(_), None) => format!("{} · signed out", self.profile_name()),
            (None, None) => self.profile_name(),
        }
    }

    pub fn profile_name(&self) -> String {
        if self.id == DEFAULT_ID { "Default account".into() } else { capitalize(&self.id.replace('-', " ")) }
    }

    /// e.g. "Pro · Acme Inc" — plan and organization if known.
    pub fn detail(&self) -> String {
        let Some(s) = self.status.as_ref().filter(|s| s.logged_in) else {
            return if self.status.is_some() { "Not signed in. Run /login in the session".into() } else { "Checking…".into() };
        };
        let plan = s.subscription_type.as_deref().map(capitalize);
        let org = s.org_name.as_deref().filter(|o| !o.ends_with("'s Organization"));
        match (plan, org) {
            (Some(p), Some(o)) => format!("{p} · {o}"),
            (Some(p), None) => p,
            (None, Some(o)) => o.to_string(),
            (None, None) => s.auth_method.clone().unwrap_or_default(),
        }
    }

    pub fn initial(&self) -> String {
        self.email()
            .and_then(|e| e.chars().next())
            .unwrap_or(if self.id == DEFAULT_ID { 'D' } else { '?' })
            .to_uppercase()
            .to_string()
    }

    /// Custom avatar at ~/.claude-deck/avatars/<email>.png, shown instead of the initial.
    pub fn find_avatar(&self) -> Option<PathBuf> {
        let path = deck_dir().join("avatars").join(format!("{}.png", self.email()?));
        path.is_file().then_some(path)
    }

    /// A stable, distinct color per account.
    pub fn color(&self) -> Hsla {
        const PALETTE: [u32; 6] = [0xD97757, 0x3B8EEA, 0x2EA043, 0xA371F7, 0xDB61A2, 0x1BA9B5];
        let index = if self.id == DEFAULT_ID {
            0
        } else {
            self.id.bytes().fold(7usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize)) % (PALETTE.len() - 1) + 1
        };
        let c = PALETTE[index];
        Rgba {
            r: ((c >> 16) & 0xFF) as f32 / 255.,
            g: ((c >> 8) & 0xFF) as f32 / 255.,
            b: (c & 0xFF) as f32 / 255.,
            a: 1.,
        }
        .into()
    }

    /// Modification times of the files `/login` and `/logout` rewrite, so
    /// the status is asked for again only when one changes. Only metadata is
    /// read, never the files.
    pub fn login_stamp(&self) -> Vec<Option<SystemTime>> {
        let home = dirs::home_dir().unwrap_or_default();
        let files = match &self.config_dir {
            Some(dir) => vec![dir.join(".claude.json"), dir.join(".credentials.json")],
            None => vec![home.join(".claude.json"), home.join(".claude/.claude.json"), home.join(".claude/.credentials.json")],
        };
        files.iter().map(|f| fs::metadata(f).and_then(|m| m.modified()).ok()).collect()
    }

    /// Where this profile keeps its data (`projects/` holds conversations).
    pub fn data_dir(&self) -> PathBuf {
        self.config_dir.clone().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"))
    }

    pub fn apply_env(&self, cmd: &mut portable_pty::CommandBuilder) {
        for var in PARENT_SESSION_VARS {
            cmd.env_remove(var);
        }
        match &self.config_dir {
            Some(dir) => cmd.env("CLAUDE_CONFIG_DIR", dir),
            None => cmd.env_remove("CLAUDE_CONFIG_DIR"),
        }
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

fn profiles_dir() -> PathBuf {
    deck_dir().join("accounts")
}

/// Whether a folder under the profiles directory is one `create_profile`
/// made (`account-<n>`). Other folders aren't profiles. Claude Code locks a
/// profile by creating `account-3.lock` next to it for a moment, and listing
/// that as an account would run `claude` inside it and leave it behind.
fn is_profile_name(name: &str) -> bool {
    name.strip_prefix("account-").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The default account plus every profile directory, without statuses.
pub fn discover() -> Vec<Account> {
    let mut accounts = vec![Account::default_account()];
    if let Ok(entries) = fs::read_dir(profiles_dir()) {
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()).is_some_and(is_profile_name))
            .collect();
        dirs.sort();
        for dir in dirs {
            let id = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
            accounts.push(Account { id, config_dir: Some(dir), status: None, avatar: None });
        }
    }
    accounts
}

pub fn create_profile() -> std::io::Result<Account> {
    let mut n = 2;
    let dir = loop {
        let dir = profiles_dir().join(format!("account-{n}"));
        if !dir.exists() {
            break dir;
        }
        n += 1;
    };
    fs::create_dir_all(&dir)?;
    Ok(Account { id: format!("account-{n}"), config_dir: Some(dir), status: None, avatar: None })
}

/// Find the `claude` executable. Apps launched from Finder don't get the
/// shell's PATH, so ask a login shell, then try the usual install spots.
pub fn resolve_claude_bin(command: &str) -> Option<PathBuf> {
    let program = command.split_whitespace().next().unwrap_or("claude");
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let script = format!("command -v {program}");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let out = Command::new(shell).args(["-l", "-i", "-c", &script]).stdin(std::process::Stdio::null()).output();
        let _ = tx.send(out.ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()));
    });
    if let Ok(Some(out)) = rx.recv_timeout(Duration::from_secs(5)) {
        if let Some(line) = out.lines().rev().find(|l| l.starts_with('/')) {
            return Some(PathBuf::from(line.trim()));
        }
    }
    let home = dirs::home_dir().unwrap_or_default();
    [home.join(".local/bin"), PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin")]
        .into_iter()
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

/// What an account's login files looked like when its status was last asked for.
pub type KnownStatuses = std::collections::HashMap<String, (Vec<Option<SystemTime>>, Option<AuthStatus>)>;

/// Fill in each account's status and avatar. `query` (`claude auth status`)
/// runs only for an account that's new, whose login files changed since the
/// last ask, or for all when `recheck_all`. The rest reuse `known`.
pub fn refresh_statuses(
    list: &mut [Account],
    known: &mut KnownStatuses,
    recheck_all: bool,
    mut query: impl FnMut(&Account) -> Option<AuthStatus>,
) {
    known.retain(|id, _| list.iter().any(|a| &a.id == id));
    for account in list {
        let stamp = account.login_stamp();
        account.status = match known.get(&account.id) {
            Some((s, status)) if !recheck_all && *s == stamp => status.clone(),
            _ => {
                let status = query(account);
                known.insert(account.id.clone(), (stamp, status.clone()));
                status
            }
        };
        account.avatar = account.find_avatar();
    }
}

/// `claude` set up to run as `account` (its config dir, no parent-session
/// variables), with no stdin.
pub fn claude_command(claude: &Path, account: &Account) -> Command {
    let mut cmd = Command::new(claude);
    cmd.stdin(std::process::Stdio::null());
    for var in PARENT_SESSION_VARS {
        cmd.env_remove(var);
    }
    match &account.config_dir {
        Some(dir) => cmd.env("CLAUDE_CONFIG_DIR", dir),
        None => cmd.env_remove("CLAUDE_CONFIG_DIR"),
    };
    cmd
}

/// The account's `.claude.json`, where its login and user-scope MCP servers live.
pub fn claude_json(account: &Account) -> Option<PathBuf> {
    match &account.config_dir {
        Some(dir) => Some(dir.join(".claude.json")),
        None => Some(dirs::home_dir()?.join(".claude.json")),
    }
}

/// `claude auth status --json` for one profile.
pub fn query_status(claude: &Path, account: &Account) -> Option<AuthStatus> {
    let (tx, rx) = mpsc::channel();
    let mut cmd = claude_command(claude, account);
    cmd.args(["auth", "status", "--json"]);
    thread::spawn(move || {
        let parsed = cmd.output().ok().and_then(|o| serde_json::from_slice::<AuthStatus>(&o.stdout).ok());
        let _ = tx.send(parsed);
    });
    rx.recv_timeout(Duration::from_secs(10)).ok().flatten()
}

/// Profiles to offer in an account picker: one per Claude login, so profiles
/// signed in to the same email show once. Of those, the one `current` names
/// is kept, else the first. Signed-out profiles are all kept.
pub fn one_per_login<'a>(all: &'a [Account], current: &str) -> Vec<&'a Account> {
    all.iter()
        .filter(|account| {
            let Some(email) = account.email() else { return true };
            let shared: Vec<&Account> = all.iter().filter(|a| a.email() == Some(email)).collect();
            let keep = shared.iter().find(|a| a.id == current).unwrap_or(&shared[0]);
            keep.id == account.id
        })
        .collect()
}

/// Emails that more than one profile is signed in with, each with the
/// profiles using it.
pub fn shared_logins(all: &[Account]) -> Vec<(String, Vec<&Account>)> {
    let mut groups: Vec<(String, Vec<&Account>)> = Vec::new();
    for account in all {
        let Some(email) = account.email() else { continue };
        match groups.iter_mut().find(|(e, _)| e == email) {
            Some((_, list)) => list.push(account),
            None => groups.push((email.to_string(), vec![account])),
        }
    }
    groups.retain(|(_, list)| list.len() > 1);
    groups
}

/// Claude Code's folder name for a working directory under `projects/`.
pub fn project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// The email an account's profile is signed in with, read from its
/// `.claude.json` (no process needed, so usable before statuses load).
pub fn configured_email(account: &Account) -> Option<String> {
    let path = claude_json(account)?;
    let text = crate::catalog::read_with_timeout(&path, Duration::from_secs(1))?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.pointer("/oauthAccount/emailAddress")?.as_str().map(str::to_string)
}

fn newest_transcript(account: &Account, cwd: &Path) -> Option<PathBuf> {
    fs::read_dir(account.data_dir().join("projects").join(project_slug(cwd)))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .max_by_key(|p| p.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH))
}

/// Session id of the account's most recent conversation in `cwd`.
pub fn latest_conversation(account: &Account, cwd: &Path) -> Option<String> {
    Some(newest_transcript(account, cwd)?.file_stem()?.to_string_lossy().into_owned())
}

/// Copy the most recent conversation for `cwd` from one account to another
/// so it can be resumed there. Returns its session id.
pub fn carry_conversation(from: &Account, to: &Account, cwd: &Path) -> Option<String> {
    let slug = project_slug(cwd);
    let newest = newest_transcript(from, cwd)?;
    if from.data_dir() == to.data_dir() {
        return latest_conversation(from, cwd);
    }
    let target_dir = to.data_dir().join("projects").join(&slug);
    fs::create_dir_all(&target_dir).ok()?;
    fs::copy(&newest, target_dir.join(newest.file_name()?)).ok()?;
    Some(newest.file_stem()?.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_in(id: &str, email: &str) -> Account {
        Account {
            id: id.into(),
            config_dir: None,
            status: Some(AuthStatus { logged_in: true, email: Some(email.into()), ..Default::default() }),
            avatar: None,
        }
    }

    #[test]
    fn asks_for_a_status_only_when_login_files_change() {
        let base = std::env::temp_dir().join(format!("deck-status-{}", std::process::id()));
        let profile = |id: &str| {
            let dir = base.join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(".claude.json"), "{}").unwrap();
            Account { id: id.into(), config_dir: Some(dir), status: None, avatar: None }
        };
        let (a, b) = (profile("account-2"), profile("account-3"));
        let mut known = KnownStatuses::new();
        let mut asked: Vec<String> = Vec::new();
        let mut tick = |list: &mut Vec<Account>, recheck_all: bool, known: &mut KnownStatuses| {
            asked.clear();
            refresh_statuses(list, known, recheck_all, |acc| {
                asked.push(acc.id.clone());
                Some(AuthStatus { logged_in: true, auth_method: Some(acc.id.clone()), ..Default::default() })
            });
            asked.clone()
        };

        let mut list = vec![a.clone(), b.clone()];
        assert_eq!(tick(&mut list, false, &mut known), ["account-2", "account-3"], "first look asks for all");
        let mut list = vec![a.clone(), b.clone()];
        assert!(tick(&mut list, false, &mut known).is_empty(), "nothing changed, nothing asked");
        assert_eq!(list[1].status.as_ref().and_then(|s| s.auth_method.as_deref()), Some("account-3"), "status reused");

        // A /login rewrites the profile's .claude.json.
        let later = SystemTime::now() + Duration::from_secs(60);
        fs::File::options().write(true).open(b.config_dir.as_ref().unwrap().join(".claude.json")).unwrap().set_modified(later).unwrap();
        let mut list = vec![a.clone(), b.clone()];
        assert_eq!(tick(&mut list, false, &mut known), ["account-3"], "only the changed account");
        // A .credentials.json appearing counts too.
        fs::write(a.config_dir.as_ref().unwrap().join(".credentials.json"), "").unwrap();
        let mut list = vec![a.clone(), b.clone()];
        assert_eq!(tick(&mut list, false, &mut known), ["account-2"]);

        let mut list = vec![a.clone(), b.clone()];
        assert_eq!(tick(&mut list, true, &mut known), ["account-2", "account-3"], "periodic recheck asks for all");

        // A removed profile is forgotten, so it's asked for again if it returns.
        let mut list = vec![a.clone()];
        tick(&mut list, false, &mut known);
        assert!(!known.contains_key("account-3"));
        let mut list = vec![a.clone(), b.clone()];
        assert_eq!(tick(&mut list, false, &mut known), ["account-3"]);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn only_account_folders_are_profiles() {
        assert!(is_profile_name("account-2"));
        assert!(is_profile_name("account-13"));
        assert!(!is_profile_name("account-3.lock"));
        assert!(!is_profile_name("account-"));
        assert!(!is_profile_name("backup"));
    }

    #[test]
    fn finds_profiles_sharing_a_login() {
        let all = vec![signed_in("default", "work@x.com"), signed_in("account-2", "work@x.com"), signed_in("account-3", "me@y.com")];
        let ids = |current| one_per_login(&all, current).into_iter().map(|a| a.id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids("account-3"), ["default", "account-3"]);
        assert_eq!(ids("account-2"), ["account-2", "account-3"]);
        let shared = shared_logins(&all);
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].0, "work@x.com");
        assert_eq!(shared[0].1.len(), 2);
    }

    #[test]
    fn slug_matches_claude_code() {
        assert_eq!(project_slug(Path::new("/Users/me/Sites/MyProject")), "-Users-me-Sites-MyProject");
        assert_eq!(project_slug(Path::new("/Users/me/my_app.v2")), "-Users-me-my-app-v2");
    }

    #[test]
    fn carries_newest_conversation_between_profiles() {
        let base = std::env::temp_dir().join(format!("deck-acct-{}", std::process::id()));
        let (a, b) = (base.join("a"), base.join("b"));
        let cwd = Path::new("/work/proj");
        let src = a.join("projects").join(project_slug(cwd));
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("old.jsonl"), "{}").unwrap();
        thread::sleep(Duration::from_millis(20));
        fs::write(src.join("new.jsonl"), "{}").unwrap();
        let from = Account { id: "a".into(), config_dir: Some(a), status: None, avatar: None };
        let to = Account { id: "b".into(), config_dir: Some(b.clone()), status: None, avatar: None };
        assert_eq!(carry_conversation(&from, &to, cwd).as_deref(), Some("new"));
        assert!(b.join("projects").join(project_slug(cwd)).join("new.jsonl").exists());
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn windows_drop_parent_claude_session_env() {
        let mut cmd = portable_pty::CommandBuilder::new("true");
        cmd.env("CLAUDE_CODE_CHILD_SESSION", "1");
        cmd.env("CLAUDECODE", "1");
        cmd.env("CLAUDE_CODE_USE_BEDROCK", "1");
        Account::default_account().apply_env(&mut cmd);
        assert_eq!(cmd.get_env("CLAUDE_CODE_CHILD_SESSION"), None);
        assert_eq!(cmd.get_env("CLAUDECODE"), None);
        assert!(cmd.get_env("CLAUDE_CODE_USE_BEDROCK").is_some(), "user settings pass through");
    }

    #[test]
    fn labels() {
        let mut acct = Account { id: "account-2".into(), config_dir: None, status: None, avatar: None };
        assert_eq!(acct.label(), "Account 2");
        acct.status = Some(AuthStatus { logged_in: false, ..Default::default() });
        assert_eq!(acct.label(), "Account 2 · signed out");
        acct.status = Some(AuthStatus {
            logged_in: true,
            email: Some("me@x.com".into()),
            subscription_type: Some("max".into()),
            org_name: Some("Acme".into()),
            ..Default::default()
        });
        assert_eq!(acct.label(), "me@x.com");
        assert_eq!(acct.detail(), "Max · Acme");
        assert_eq!(acct.initial(), "M");
    }
}
