//! Saved charge codes, grouped under titled sections:
//! ~/.claude-deck/charge-codes.json

use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::catalog::deck_dir;

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Debug)]
pub struct ChargeSection {
    pub title: String,
    pub codes: Vec<String>,
}

fn path() -> PathBuf {
    deck_dir().join("charge-codes.json")
}

pub fn load() -> Vec<ChargeSection> {
    fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save(sections: &[ChargeSection]) {
    let _ = fs::create_dir_all(deck_dir());
    if let Ok(json) = serde_json::to_string_pretty(sections) {
        let _ = fs::write(path(), json);
    }
}
