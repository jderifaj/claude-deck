//! Finds color themes: the built-in defaults, VS Code's bundled themes and
//! theme extensions installed in VS Code.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::settings::ThemeSetting;

#[derive(Clone)]
pub struct ThemeChoice {
    pub setting: ThemeSetting,
    /// Where it came from, e.g. "Default" or the extension's name.
    pub source: String,
}

/// Read a file, giving up after `timeout`. Some machines have security
/// agents that stall `open()` on certain files indefinitely; a stuck read
/// must never block the app.
pub fn read_with_timeout(path: &Path, timeout: Duration) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    let path = path.to_path_buf();
    thread::spawn(move || {
        let _ = tx.send(fs::read_to_string(path).ok());
    });
    rx.recv_timeout(timeout).ok().flatten()
}

pub fn deck_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude-deck")
}

pub fn builtin_choices() -> Vec<ThemeChoice> {
    ["Dark Modern", "Light Modern"]
        .into_iter()
        .map(|label| ThemeChoice {
            setting: ThemeSetting {
                label: label.into(),
                path: PathBuf::new(),
                ui_theme: if label == "Dark Modern" { "vs-dark" } else { "vs" }.into(),
            },
            source: "Default".into(),
        })
        .collect()
}

/// Every available theme. Extension folders are parsed in parallel and any
/// that don't finish in time are skipped.
pub fn scan() -> Vec<ThemeChoice> {
    let home = dirs::home_dir().unwrap_or_default();
    let roots = [
        PathBuf::from("/Applications/Visual Studio Code.app/Contents/Resources/app/extensions"),
        home.join(".vscode/extensions"),
    ];
    let (tx, rx) = mpsc::channel();
    let mut pending = 0;
    for root in roots {
        let Ok(entries) = fs::read_dir(&root) else { continue };
        for entry in entries.flatten() {
            let (dir, tx) = (entry.path(), tx.clone());
            pending += 1;
            thread::spawn(move || {
                let _ = tx.send(themes_in(&dir));
            });
        }
    }
    drop(tx);

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut found = Vec::new();
    while pending > 0 {
        let Ok(themes) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) else { break };
        pending -= 1;
        found.extend(themes);
    }
    found.sort_by(|a, b| a.setting.label.to_lowercase().cmp(&b.setting.label.to_lowercase()));
    found.dedup_by(|a, b| a.setting.label == b.setting.label && a.source == b.source);

    let mut all = builtin_choices();
    all.extend(found);
    all
}

fn themes_in(dir: &Path) -> Vec<ThemeChoice> {
    let Some(text) = read_with_timeout(&dir.join("package.json"), Duration::from_secs(1)) else { return Vec::new() };
    let Ok(pkg) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
    let Some(themes) = pkg.pointer("/contributes/themes").and_then(|t| t.as_array()) else { return Vec::new() };

    let nls: HashMap<String, Value> = read_with_timeout(&dir.join("package.nls.json"), Duration::from_secs(1))
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let localize = |s: &str| -> String {
        let key = s.strip_prefix('%').and_then(|s| s.strip_suffix('%'));
        match key.and_then(|k| nls.get(k)) {
            Some(Value::String(v)) => v.clone(),
            Some(Value::Object(o)) => o.get("message").and_then(|m| m.as_str()).unwrap_or(s).to_string(),
            _ => s.to_string(),
        }
    };
    let field = |v: &Value, key: &str| v.get(key).and_then(|v| v.as_str()).map(&localize).unwrap_or_default();

    let name = field(&pkg, "displayName");
    let source = if name.is_empty() { field(&pkg, "name") } else { name };
    themes
        .iter()
        .filter_map(|t| {
            let path = dir.join(t.get("path")?.as_str()?);
            if path.extension().is_none_or(|e| e != "json") {
                return None; // .tmTheme files aren't supported
            }
            let label = Some(field(t, "label")).filter(|l| !l.is_empty()).unwrap_or_else(|| field(t, "id"));
            Some(ThemeChoice {
                setting: ThemeSetting { label, path, ui_theme: field(t, "uiTheme") },
                source: source.clone(),
            })
        })
        .collect()
}
