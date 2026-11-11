//! Plan usage per account.
//!
//! Claude Code hands its status-line command the subscription's rate-limit
//! windows (`rate_limits.five_hour` / `seven_day`: used % and reset time).
//! Sessions started by Claude Deck use this binary as their status line
//! (`claude-deck --statusline <account>`), which records the numbers in
//! ~/.claude-deck/usage/<account>.json for the usage bar. No credentials
//! are read.

use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog::deck_dir;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Limit {
    pub used_percentage: f32,
    pub resets_at: i64,
}

impl Limit {
    /// Usage now: a window whose reset time has passed is back to zero.
    pub fn current(&self, now: i64) -> f32 {
        if self.resets_at <= now { 0. } else { self.used_percentage }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct Usage {
    pub five_hour: Option<Limit>,
    pub seven_day: Option<Limit>,
    pub updated_at: i64,
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn usage_dir() -> PathBuf {
    deck_dir().join("usage")
}

fn safe_id(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect()
}

pub fn load_all() -> HashMap<String, Usage> {
    let mut out = HashMap::new();
    let Ok(entries) = fs::read_dir(usage_dir()) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") && !path.file_name().unwrap_or_default().to_string_lossy().starts_with("settings-") {
            if let Some(usage) = fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()) {
                out.insert(path.file_stem().unwrap_or_default().to_string_lossy().into_owned(), usage);
            }
        }
    }
    out
}

/// The `statusLine` setting that points Claude's status line at this binary.
pub fn status_line(account_id: &str) -> Option<Value> {
    let exe = std::env::current_exe().ok()?;
    let command = format!("\"{}\" --statusline {}", exe.display(), safe_id(account_id));
    fs::create_dir_all(usage_dir()).ok()?;
    Some(serde_json::json!({ "type": "command", "command": command, "padding": 0 }))
}

fn parse_limit(v: Option<&Value>) -> Option<Limit> {
    let v = v?;
    Some(Limit { used_percentage: v.get("used_percentage")?.as_f64()? as f32, resets_at: v.get("resets_at")?.as_i64()? })
}

/// Status-line entry point: record rate limits, print a compact summary.
pub fn run_statusline(account_id: &str) -> i32 {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let Ok(json) = serde_json::from_str::<Value>(&input) else { return 0 };

    let mut parts = Vec::new();
    if let Some(email) = signed_in_email() {
        parts.push(email);
    }
    if let Ok(github) = std::env::var("CLAUDE_DECK_GITHUB") {
        parts.push(format!("GitHub {github}"));
    }
    if let Some(model) = json.pointer("/model/display_name").and_then(|v| v.as_str()) {
        parts.push(model.to_string());
    }
    if let Some(limits) = json.get("rate_limits") {
        let usage = Usage {
            five_hour: parse_limit(limits.get("five_hour")),
            seven_day: parse_limit(limits.get("seven_day")),
            updated_at: now(),
        };
        if let Some(l) = &usage.five_hour {
            parts.push(format!("5h {:.0}%", l.used_percentage));
        }
        if let Some(l) = &usage.seven_day {
            parts.push(format!("7d {:.0}%", l.used_percentage));
        }
        if usage.five_hour.is_some() || usage.seven_day.is_some() {
            let dir = usage_dir();
            let id = safe_id(account_id);
            let tmp = dir.join(format!(".{id}.tmp"));
            if fs::create_dir_all(&dir).is_ok() && fs::write(&tmp, serde_json::to_vec(&usage).unwrap_or_default()).is_ok() {
                let _ = fs::rename(tmp, dir.join(format!("{id}.json")));
            }
        }
    }
    print!("{}", parts.join(" · "));
    0
}

/// The account this Claude process is signed in to, from its profile's
/// `.claude.json` (`$CLAUDE_CONFIG_DIR`, else `~/.claude.json`).
fn signed_in_email() -> Option<String> {
    let path = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir).join(".claude.json"),
        None => dirs::home_dir()?.join(".claude.json"),
    };
    let json: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    json.pointer("/oauthAccount/emailAddress")?.as_str().map(str::to_string)
}

/// "in 2h 10m", "in 3d 4h", "now".
/// Whole minutes left on every reset countdown, -1 once a window has reset.
/// It changes exactly when `until` or `Limit::current` would show something
/// new, so the usage bar is redrawn only then.
pub fn countdown_minutes(all: &HashMap<String, Usage>, now: i64) -> Vec<(String, i64)> {
    let mut minutes: Vec<(String, i64)> = all
        .iter()
        .flat_map(|(id, u)| {
            [&u.five_hour, &u.seven_day].into_iter().flatten().map(move |l| {
                let secs = l.resets_at - now;
                (id.clone(), if secs <= 0 { -1 } else { secs / 60 })
            })
        })
        .collect();
    minutes.sort();
    minutes
}

pub fn until(resets_at: i64, now: i64) -> String {
    let secs = resets_at - now;
    if secs <= 0 {
        return "now".into();
    }
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("in {d}d {h}h")
    } else if h > 0 {
        format!("in {h}h {m}m")
    } else {
        format!("in {}m", m.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_window_reads_zero() {
        let l = Limit { used_percentage: 80., resets_at: 100 };
        assert_eq!(l.current(50), 80.);
        assert_eq!(l.current(100), 0.);
    }

    #[test]
    fn countdown() {
        assert_eq!(until(10_000, 10_000), "now");
        assert_eq!(until(10_000 + 90, 10_000), "in 1m");
        assert_eq!(until(10_000 + 2 * 3600 + 600, 10_000), "in 2h 10m");
        assert_eq!(until(10_000 + 3 * 86_400 + 4 * 3600, 10_000), "in 3d 4h");
    }

    #[test]
    fn parses_status_line_limits() {
        let v: Value = serde_json::from_str(r#"{"five_hour":{"used_percentage":23.5,"resets_at":1790000000}}"#).unwrap();
        assert_eq!(parse_limit(v.get("five_hour")), Some(Limit { used_percentage: 23.5, resets_at: 1_790_000_000 }));
        assert_eq!(parse_limit(v.get("seven_day")), None);
    }

    #[test]
    fn countdowns_change_only_when_the_shown_minute_does() {
        let limit = |resets_at| Some(Limit { used_percentage: 10., resets_at });
        let all = HashMap::from([("a".to_string(), Usage { five_hour: limit(1_000), seven_day: None, updated_at: 0 })]);
        let at = |now| countdown_minutes(&all, now);
        // 2m 0s and 2m 59s left both show "in 2m".
        assert_eq!(at(880), at(821));
        assert_eq!(until(1_000, 880), until(1_000, 821));
        // Crossing to 1m 59s does change it.
        assert_ne!(at(821), at(881));
        assert_ne!(until(1_000, 821), until(1_000, 881));
        // Reaching the reset time ("now", usage back to 0) changes it too.
        assert_ne!(at(999), at(1_000));
        assert_eq!(at(1_000), at(5_000));
    }
}
