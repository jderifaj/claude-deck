//! The optional Asana panel under the charge codes: the user's open tasks,
//! grouped by due date.
//!
//! - The Personal Access Token lives in the macOS Keychain (service
//!   `claude-deck.asana`), never in a file the sessions can read.
//! - ~/.claude-deck/asana.json keeps only two numeric IDs (workspace and
//!   user), owner-only (0600). Names are fetched again on each load.
//! - Tasks and projects live only in the app's memory: never written to
//!   disk, logged, or passed to a Claude session.
//! - Requests go through the system `curl`, with the token passed on stdin
//!   so it never shows in the process list. Responses come back over a pipe.
//! - Nothing reloads on a timer: the first time the panel shows, then only
//!   when the user clicks Reload.

use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog::deck_dir;

/// The Asana mark (three dots, Asana's gradient), shown in full color.
pub const LOGO_SVG: &[u8] = include_bytes!("../assets/icons/asana.svg");

/// Where the user makes a Personal Access Token.
pub const TOKEN_URL: &str = "https://app.asana.com/0/my-apps";

const API: &str = "https://app.asana.com/api/1.0";
const KEYCHAIN_SERVICE: &str = "claude-deck.asana";
const KEYCHAIN_ACCOUNT: &str = "personal-access-token";
/// Most tasks listed (pages of 100).
const MAX_PAGES: usize = 5;

// ---- settings ---------------------------------------------------------------

/// What's kept on disk: IDs only, no names and nothing from the tasks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub workspace_gid: String,
    /// Filled in on the first load for configs saved before it was kept.
    #[serde(default)]
    pub user_gid: String,
}

fn config_path() -> PathBuf {
    deck_dir().join("asana.json")
}

pub fn load_config() -> Option<Config> {
    let text = fs::read_to_string(config_path()).ok()?;
    let config: Config = serde_json::from_str(&text).ok()?;
    // Older files also held the user's and workspace's names: rewrite without them.
    if text.contains("_name") {
        save_config(&config);
    }
    Some(config)
}

pub fn save_config(config: &Config) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::create_dir_all(deck_dir());
    if let Ok(json) = serde_json::to_string_pretty(config) {
        let path = config_path();
        if fs::write(&path, json).is_ok() {
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
    }
}

// ---- keychain ---------------------------------------------------------------

#[cfg(target_os = "macos")]
pub fn load_token() -> Option<String> {
    let bytes = security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).ok()?;
    String::from_utf8(bytes).ok().filter(|t| !t.is_empty())
}

#[cfg(target_os = "macos")]
pub fn save_token(token: &str) -> Result<()> {
    security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, token.as_bytes())
        .context("couldn't save the token in the Keychain")
}

#[cfg(target_os = "macos")]
fn delete_token() {
    let _ = security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
}

#[cfg(not(target_os = "macos"))]
pub fn load_token() -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn save_token(_: &str) -> Result<()> {
    bail!("saving the Asana token needs the macOS Keychain")
}

#[cfg(not(target_os = "macos"))]
fn delete_token() {}

/// Forget the token and the workspace.
pub fn disconnect() {
    delete_token();
    let _ = fs::remove_file(config_path());
}

// ---- API --------------------------------------------------------------------

/// Asana turned the token down (expired, revoked or mistyped).
#[derive(Debug)]
pub struct Unauthorized;

impl std::fmt::Display for Unauthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("Asana didn't accept the token")
    }
}

impl std::error::Error for Unauthorized {}

pub fn is_unauthorized(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Unauthorized>().is_some()
}

/// GET `path` (with its query) from the Asana API and return the JSON.
fn get(token: &str, path: &str) -> Result<Value> {
    let mut child = Command::new("/usr/bin/curl")
        .args(["-sS", "--max-time", "20", "-H", "@-", "-H", "Accept: application/json", "-w", "\n%{http_code}"])
        .arg(format!("{API}{path}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("couldn't run curl")?;
    child.stdin.take().context("no curl stdin")?.write_all(format!("Authorization: Bearer {token}\n").as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("couldn't reach Asana: {}", err.trim().trim_start_matches("curl: "));
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let (json, code) = body.rsplit_once('\n').unwrap_or(("", &body));
    match code.trim() {
        "401" | "403" => Err(Unauthorized.into()),
        "200" => serde_json::from_str(json).context("Asana sent something unexpected"),
        code => {
            let message = serde_json::from_str::<Value>(json)
                .ok()
                .and_then(|v| v["errors"][0]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| format!("HTTP {code}"));
            Err(anyhow!("Asana: {message}"))
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Workspace {
    pub gid: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Me {
    pub gid: String,
    pub name: String,
    pub workspaces: Vec<Workspace>,
}

/// Check the token and list the workspaces it can see.
pub fn me(token: &str) -> Result<Me> {
    let v = get(token, "/users/me?opt_fields=name,workspaces.name")?;
    let data = &v["data"];
    let workspaces = data["workspaces"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| Some(Workspace { gid: w["gid"].as_str()?.to_string(), name: w["name"].as_str()?.to_string() }))
        .collect();
    Ok(Me { gid: data["gid"].as_str().unwrap_or("").to_string(), name: data["name"].as_str().unwrap_or("").to_string(), workspaces })
}

/// What kind of task it is, when that changes what the user does with it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Task,
    Approval,
    Milestone,
}

impl Kind {
    pub fn label(self) -> Option<&'static str> {
        match self {
            Kind::Task => None,
            Kind::Approval => Some("Approval"),
            Kind::Milestone => Some("Milestone"),
        }
    }
}

/// The groups the panel lists, most urgent first.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Bucket {
    Overdue,
    Today,
    Upcoming,
    NoDate,
}

impl Bucket {
    pub const ALL: [Bucket; 4] = [Bucket::Overdue, Bucket::Today, Bucket::Upcoming, Bucket::NoDate];

    pub fn title(self) -> &'static str {
        match self {
            Bucket::Overdue => "Overdue",
            Bucket::Today => "Due today",
            Bucket::Upcoming => "Upcoming",
            Bucket::NoDate => "No due date",
        }
    }

    /// Where a task due on `due` (YYYY-MM-DD) goes when it's `today`.
    fn of(due: Option<&str>, today: &str) -> Bucket {
        match due {
            None => Bucket::NoDate,
            Some(d) if d < today => Bucket::Overdue,
            Some(d) if d == today => Bucket::Today,
            Some(_) => Bucket::Upcoming,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ticket {
    pub title: String,
    /// First project's name, or empty.
    pub project: String,
    /// Every project the task is in.
    pub project_gids: Vec<String>,
    /// YYYY-MM-DD, for sorting.
    pub due_on: Option<String>,
    /// As shown: "Nov 4", "Today", or empty.
    pub due: String,
    pub bucket: Bucket,
    pub kind: Kind,
    pub url: String,
}

/// The user's incomplete tasks in `workspace`, most urgent first.
pub fn my_tasks(token: &str, workspace: &str) -> Result<Vec<Ticket>> {
    let today = local_today();
    let mut tickets = Vec::new();
    let mut offset: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut path = format!(
            "/tasks?assignee=me&workspace={workspace}&completed_since=now&limit=100\
             &opt_fields=name,due_on,due_at,resource_subtype,permalink_url,projects.name"
        );
        if let Some(o) = &offset {
            path.push_str("&offset=");
            path.push_str(o);
        }
        let v = get(token, &path)?;
        tickets.extend(v["data"].as_array().into_iter().flatten().filter_map(|t| parse_task(t, &today)));
        offset = v["next_page"]["offset"].as_str().map(str::to_string);
        if offset.is_none() {
            break;
        }
    }
    sort(&mut tickets);
    Ok(tickets)
}

fn parse_task(t: &Value, today: &str) -> Option<Ticket> {
    let kind = match t["resource_subtype"].as_str() {
        Some("section") => return None,
        Some("approval") => Kind::Approval,
        Some("milestone") => Kind::Milestone,
        _ => Kind::Task,
    };
    let due_on = t["due_on"].as_str().or_else(|| t["due_at"].as_str().and_then(|s| s.get(..10))).map(str::to_string);
    let bucket = Bucket::of(due_on.as_deref(), today);
    let due = match (&due_on, bucket) {
        (_, Bucket::Today) => "Today".to_string(),
        (Some(d), _) => short_date(d, today),
        (None, _) => String::new(),
    };
    Some(Ticket {
        title: t["name"].as_str().filter(|n| !n.is_empty()).unwrap_or("Untitled task").to_string(),
        project: t["projects"][0]["name"].as_str().unwrap_or("").to_string(),
        project_gids: t["projects"].as_array().into_iter().flatten().filter_map(|p| p["gid"].as_str().map(str::to_string)).collect(),
        due_on,
        due,
        bucket,
        kind,
        url: t["permalink_url"].as_str().unwrap_or("").to_string(),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    pub gid: String,
    pub name: String,
    pub url: String,
}

/// Everything the panel shows, from one reload.
pub struct Loaded {
    pub tickets: Vec<Ticket>,
    /// Projects the user is a member of, plus any their tasks are in, by name.
    pub projects: Vec<Project>,
    pub user_gid: String,
    pub user_name: String,
    pub workspace_name: String,
}

/// The user's tasks and projects, and who and where they are.
pub fn load(token: &str, workspace: &str) -> Result<Loaded> {
    let me = me(token)?;
    let workspace_name = me.workspaces.iter().find(|w| w.gid == workspace).map(|w| w.name.clone()).unwrap_or_default();
    let tickets = my_tasks(token, workspace)?;
    let user_gid = me.gid;
    let mut projects = match member_projects(token, workspace, &user_gid) {
        Ok(p) => p,
        Err(e) if is_unauthorized(&e) => return Err(e),
        // Membership listing is a nice-to-have; the tasks' projects still show.
        Err(_) => Vec::new(),
    };
    for t in &tickets {
        let Some(gid) = t.project_gids.first() else { continue };
        if !projects.iter().any(|p| &p.gid == gid) {
            projects.push(Project { gid: gid.clone(), name: t.project.clone(), url: project_url(gid) });
        }
    }
    projects.sort_by_key(|p| p.name.to_lowercase());
    Ok(Loaded { tickets, projects, user_gid, user_name: me.name, workspace_name })
}

fn project_url(gid: &str) -> String {
    format!("https://app.asana.com/0/{gid}/list")
}

/// Projects the user is a member of in `workspace`, archived ones left out.
fn member_projects(token: &str, workspace: &str, user_gid: &str) -> Result<Vec<Project>> {
    let mut projects = Vec::new();
    let mut offset: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut path = format!(
            "/memberships?member={user_gid}&resource_subtype=project_membership&limit=100\
             &opt_fields=parent.name,parent.permalink_url,parent.archived,parent.workspace"
        );
        if let Some(o) = &offset {
            path.push_str("&offset=");
            path.push_str(o);
        }
        let v = get(token, &path)?;
        projects.extend(v["data"].as_array().into_iter().flatten().filter_map(|m| parse_member_project(&m["parent"], workspace)));
        offset = v["next_page"]["offset"].as_str().map(str::to_string);
        if offset.is_none() {
            break;
        }
    }
    Ok(projects)
}

fn parse_member_project(p: &Value, workspace: &str) -> Option<Project> {
    if p["archived"].as_bool() == Some(true) {
        return None;
    }
    if p["workspace"]["gid"].as_str().is_some_and(|w| w != workspace) {
        return None;
    }
    let gid = p["gid"].as_str()?.to_string();
    let url = p["permalink_url"].as_str().map(str::to_string).unwrap_or_else(|| project_url(&gid));
    Some(Project { name: p["name"].as_str().unwrap_or("Untitled project").to_string(), gid, url })
}

fn sort(tickets: &mut [Ticket]) {
    let rank = |b: Bucket| Bucket::ALL.iter().position(|x| *x == b).unwrap_or(0);
    tickets.sort_by(|a, b| rank(a.bucket).cmp(&rank(b.bucket)).then_with(|| a.due_on.cmp(&b.due_on)));
}

/// "Nov 4", with the year when it isn't this year's.
fn short_date(date: &str, today: &str) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let mut parts = date.splitn(3, '-');
    let (Some(y), Some(m), Some(d)) = (parts.next(), parts.next(), parts.next()) else { return date.to_string() };
    let (Ok(m), Ok(d)) = (m.parse::<usize>(), d.parse::<u32>()) else { return date.to_string() };
    let Some(month) = m.checked_sub(1).and_then(|i| MONTHS.get(i)) else { return date.to_string() };
    if today.starts_with(y) { format!("{month} {d}") } else { format!("{month} {d}, {y}") }
}

/// Today's date on this Mac, as YYYY-MM-DD.
fn local_today() -> String {
    // SAFETY: localtime_r only writes the `tm` we hand it.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn buckets_follow_the_due_date() {
        let today = "2026-11-06";
        assert_eq!(Bucket::of(Some("2026-11-05"), today), Bucket::Overdue);
        assert_eq!(Bucket::of(Some("2026-11-06"), today), Bucket::Today);
        assert_eq!(Bucket::of(Some("2026-12-01"), today), Bucket::Upcoming);
        assert_eq!(Bucket::of(None, today), Bucket::NoDate);
    }

    #[test]
    fn parses_tasks_and_skips_sections() {
        let today = "2026-11-06";
        let task = json!({
            "name": "Review hero banner copy", "due_on": "2026-11-06", "resource_subtype": "approval",
            "permalink_url": "https://app.asana.com/0/1/2", "projects": [{ "name": "Winter Campaign" }]
        });
        let t = parse_task(&task, today).unwrap();
        assert_eq!((t.due.as_str(), t.bucket, t.kind, t.project.as_str()), ("Today", Bucket::Today, Kind::Approval, "Winter Campaign"));
        let timed = parse_task(&json!({ "name": "x", "due_at": "2027-01-02T15:00:00.000Z" }), today).unwrap();
        assert_eq!(timed.due, "Jan 2, 2027");
        assert!(parse_task(&json!({ "name": "Backlog:", "resource_subtype": "section" }), today).is_none());
    }

    #[test]
    fn member_projects_skip_archived_and_other_workspaces() {
        let p = |archived: bool, ws: &str| json!({ "gid": "9", "name": "Site", "archived": archived, "workspace": { "gid": ws } });
        assert_eq!(parse_member_project(&p(false, "1"), "1").map(|p| p.url), Some("https://app.asana.com/0/9/list".into()));
        assert!(parse_member_project(&p(true, "1"), "1").is_none());
        assert!(parse_member_project(&p(false, "2"), "1").is_none());
    }

    #[test]
    fn sorts_overdue_first_then_by_date() {
        let today = "2026-11-06";
        let mut tickets: Vec<_> = ["2026-11-20", "", "2026-11-01", "2026-11-06", "2026-11-03"]
            .iter()
            .map(|d| parse_task(&json!({ "name": "t", "due_on": if d.is_empty() { Value::Null } else { json!(d) } }), today).unwrap())
            .collect();
        sort(&mut tickets);
        let order: Vec<_> = tickets.iter().map(|t| t.due.as_str()).collect();
        assert_eq!(order, ["Nov 1", "Nov 3", "Today", "Nov 20", ""]);
    }
}
