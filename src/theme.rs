//! Workbench colors, using VS Code color-theme keys so VS Code theme
//! extensions can be loaded directly.

use std::{collections::HashMap, path::Path};

use anyhow::{Context as _, Result, bail};
use gpui::{Global, Hsla, Rgba};

#[derive(Clone)]
pub struct Theme {
    pub name: String,
    pub is_dark: bool,
    /// Drawn over the window's glass backdrop (see `glassy`).
    pub glass: bool,
    pub fg: Hsla,
    pub fg_muted: Hsla,
    pub editor_bg: Hsla,
    pub titlebar_bg: Hsla,
    pub titlebar_fg: Hsla,
    pub activity_bg: Hsla,
    pub activity_fg: Hsla,
    pub activity_inactive_fg: Hsla,
    pub activity_active_border: Hsla,
    pub sidebar_bg: Hsla,
    pub sidebar_fg: Hsla,
    pub tabs_bg: Hsla,
    pub tab_active_bg: Hsla,
    pub tab_inactive_bg: Hsla,
    pub tab_active_fg: Hsla,
    pub tab_inactive_fg: Hsla,
    pub tab_active_border_top: Hsla,
    pub border: Hsla,
    pub status_bg: Hsla,
    pub status_fg: Hsla,
    pub focus_border: Hsla,
    pub list_hover_bg: Hsla,
    pub list_active_bg: Hsla,
    pub button_bg: Hsla,
    pub button_fg: Hsla,
    pub button_hover_bg: Hsla,
    pub badge_bg: Hsla,
    pub badge_fg: Hsla,
    pub notification_bg: Hsla,
    pub error_fg: Hsla,
    pub success_fg: Hsla,
    pub term_bg: Hsla,
    pub term_fg: Hsla,
    pub term_cursor: Hsla,
    pub term_selection: Hsla,
    pub ansi: [Hsla; 16],
}

impl Global for Theme {}

pub fn hex(s: &str) -> Option<Hsla> {
    let s = s.trim().strip_prefix('#')?;
    let expanded: String = match s.len() {
        3 | 4 => s.chars().flat_map(|c| [c, c]).collect(),
        6 | 8 => s.to_string(),
        _ => return None,
    };
    let n = |i: usize| u8::from_str_radix(&expanded[i..i + 2], 16).ok().map(|v| v as f32 / 255.);
    let a = if expanded.len() == 8 { n(6)? } else { 1. };
    Some(Rgba { r: n(0)?, g: n(2)?, b: n(4)?, a }.into())
}

fn c(s: &str) -> Hsla {
    hex(s).expect("valid builtin color")
}

fn shade(color: Hsla, amount: f32) -> Hsla {
    Hsla { l: (color.l + amount).clamp(0., 1.), ..color }
}

impl Theme {
    /// VS Code "Dark Modern".
    pub fn dark_modern() -> Self {
        Self {
            name: "Dark Modern".into(),
            is_dark: true,
            glass: false,
            fg: c("#CCCCCC"),
            fg_muted: c("#9D9D9D"),
            editor_bg: c("#1F1F1F"),
            titlebar_bg: c("#181818"),
            titlebar_fg: c("#CCCCCC"),
            activity_bg: c("#181818"),
            activity_fg: c("#D7D7D7"),
            activity_inactive_fg: c("#868686"),
            activity_active_border: c("#0078D4"),
            sidebar_bg: c("#181818"),
            sidebar_fg: c("#CCCCCC"),
            tabs_bg: c("#181818"),
            tab_active_bg: c("#1F1F1F"),
            tab_inactive_bg: c("#181818"),
            tab_active_fg: c("#FFFFFF"),
            tab_inactive_fg: c("#9D9D9D"),
            tab_active_border_top: c("#0078D4"),
            border: c("#2B2B2B"),
            status_bg: c("#181818"),
            status_fg: c("#CCCCCC"),
            focus_border: c("#0078D4"),
            list_hover_bg: c("#2A2D2E"),
            list_active_bg: c("#37373D"),
            button_bg: c("#0078D4"),
            button_fg: c("#FFFFFF"),
            button_hover_bg: c("#026EC1"),
            badge_bg: c("#616161"),
            badge_fg: c("#F8F8F8"),
            notification_bg: c("#1F1F1F"),
            error_fg: c("#F85149"),
            success_fg: c("#2EA043"),
            term_bg: c("#1F1F1F"),
            term_fg: c("#CCCCCC"),
            term_cursor: c("#AEAFAD"),
            term_selection: c("#264F78"),
            ansi: [
                "#000000", "#CD3131", "#0DBC79", "#E5E510", "#2472C8", "#BC3FBC", "#11A8CD",
                "#E5E5E5", "#666666", "#F14C4C", "#23D18B", "#F5F543", "#3B8EEA", "#D670D6",
                "#29B8DB", "#E5E5E5",
            ]
            .map(c),
        }
    }

    /// VS Code "Light Modern".
    pub fn light_modern() -> Self {
        Self {
            name: "Light Modern".into(),
            is_dark: false,
            glass: false,
            fg: c("#3B3B3B"),
            fg_muted: c("#767676"),
            editor_bg: c("#FFFFFF"),
            titlebar_bg: c("#F8F8F8"),
            titlebar_fg: c("#1E1E1E"),
            activity_bg: c("#F8F8F8"),
            activity_fg: c("#1F1F1F"),
            activity_inactive_fg: c("#616161"),
            activity_active_border: c("#005FB8"),
            sidebar_bg: c("#F8F8F8"),
            sidebar_fg: c("#3B3B3B"),
            tabs_bg: c("#F8F8F8"),
            tab_active_bg: c("#FFFFFF"),
            tab_inactive_bg: c("#F8F8F8"),
            tab_active_fg: c("#3B3B3B"),
            tab_inactive_fg: c("#868686"),
            tab_active_border_top: c("#005FB8"),
            border: c("#E5E5E5"),
            status_bg: c("#F8F8F8"),
            status_fg: c("#3B3B3B"),
            focus_border: c("#005FB8"),
            list_hover_bg: c("#F2F2F2"),
            list_active_bg: c("#E4E6F1"),
            button_bg: c("#005FB8"),
            button_fg: c("#FFFFFF"),
            button_hover_bg: c("#0258A8"),
            badge_bg: c("#CCCCCC"),
            badge_fg: c("#3B3B3B"),
            notification_bg: c("#FFFFFF"),
            error_fg: c("#E51400"),
            success_fg: c("#388A34"),
            term_bg: c("#FFFFFF"),
            term_fg: c("#3B3B3B"),
            term_cursor: c("#005FB8"),
            term_selection: c("#ADD6FF"),
            ansi: [
                "#000000", "#CD3131", "#107C10", "#949800", "#0451A5", "#BC05BC", "#0598BC",
                "#555555", "#666666", "#CD3131", "#14CE14", "#B5BA00", "#0451A5", "#BC05BC",
                "#0598BC", "#A5A5A5",
            ]
            .map(c),
        }
    }

    /// Load a VS Code color theme (JSON with comments, following `include`).
    /// The app's default theme, as drawn in this window (see-through over glass).
    pub fn current(cx: &gpui::App) -> Self {
        let theme = cx.global::<Theme>().clone();
        if crate::glass::enabled(cx) { theme.glassy() } else { theme }
    }

    /// This theme with its surfaces made see-through, for drawing over the
    /// window's glass backdrop.
    pub fn glassy(mut self) -> Self {
        // One tint over the whole window so it reads as a single sheet of
        // glass, dense enough that text stays legible over a bright desktop.
        let tint = |c: Hsla, alpha: f32| Hsla { a: c.a * alpha, ..c };
        for bg in [
            &mut self.titlebar_bg,
            &mut self.activity_bg,
            &mut self.sidebar_bg,
            &mut self.tabs_bg,
            &mut self.tab_inactive_bg,
            &mut self.status_bg,
            &mut self.editor_bg,
            &mut self.term_bg,
        ] {
            *bg = tint(*bg, 0.74);
        }
        self.tab_active_bg = tint(self.tab_active_bg, 0.86);
        // Dim text gets lost against a busy backdrop: lift it toward the
        // main text color.
        self.fg_muted = Hsla { a: 0.85, ..self.fg };
        self.tab_inactive_fg = Hsla { a: 0.8, ..self.tab_active_fg };
        self.activity_inactive_fg = Hsla { a: 0.75, ..self.activity_fg };
        self.glass = true;
        self
    }

    /// `color` lifted (or darkened, on a light theme) just enough to read
    /// against the terminal background, like iTerm's "minimum contrast".
    /// Colors that already contrast enough are left alone.
    pub fn legible(&self, color: Hsla) -> Hsla {
        let luminance = |c: Hsla| {
            let rgb = c.to_rgb();
            let lin = |v: f32| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
            0.2126 * lin(rgb.r) + 0.7152 * lin(rgb.g) + 0.0722 * lin(rgb.b)
        };
        let mut c = Hsla { a: color.a.max(0.92), ..color };
        for _ in 0..40 {
            let ok = if self.is_dark { luminance(c) >= 0.36 } else { luminance(c) <= 0.16 };
            if ok {
                break;
            }
            c.l = (c.l + if self.is_dark { 0.015 } else { -0.015 }).clamp(0., 1.);
        }
        c
    }

    pub fn load_vscode(path: &Path, label: &str, ui_theme: &str) -> Result<Self> {
        if path.extension().is_some_and(|e| e == "tmTheme") {
            bail!("TextMate (.tmTheme) themes are not supported");
        }
        let mut colors = HashMap::new();
        let mut kind = None;
        collect_colors(path, 0, &mut colors, &mut kind)?;

        let is_dark = match kind.as_deref() {
            Some("light") | Some("hcLight") => false,
            Some(_) => true,
            None => ui_theme != "vs" && ui_theme != "hc-light",
        };
        let mut t = if is_dark { Self::dark_modern() } else { Self::light_modern() };
        t.name = label.to_string();
        t.is_dark = is_dark;

        let get = |k: &str| colors.get(k).and_then(|v| hex(v));
        let has_bg = get("editor.background").is_some();
        let bg = get("editor.background").unwrap_or(t.editor_bg);
        // Chrome a step darker than the editor when a theme only sets the editor colors.
        let derive = |key: &str, fallback: Hsla, step: f32| {
            get(key).unwrap_or(if has_bg { shade(bg, if is_dark { -step } else { -step / 2. }) } else { fallback })
        };

        t.editor_bg = bg;
        t.fg = get("foreground").or(get("editor.foreground")).unwrap_or(t.fg);
        t.fg_muted = get("descriptionForeground").unwrap_or(Hsla { a: 0.7, ..t.fg });
        t.sidebar_bg = derive("sideBar.background", t.sidebar_bg, 0.03);
        t.sidebar_fg = get("sideBar.foreground").unwrap_or(t.fg);
        t.activity_bg = derive("activityBar.background", t.activity_bg, 0.04);
        t.activity_fg = get("activityBar.foreground").unwrap_or(t.activity_fg);
        t.activity_inactive_fg = get("activityBar.inactiveForeground").unwrap_or(Hsla { a: 0.6, ..t.activity_fg });
        t.titlebar_bg = get("titleBar.activeBackground").unwrap_or(t.activity_bg);
        t.titlebar_fg = get("titleBar.activeForeground").unwrap_or(t.fg);
        t.tabs_bg = get("editorGroupHeader.tabsBackground").unwrap_or(t.sidebar_bg);
        t.tab_active_bg = get("tab.activeBackground").unwrap_or(bg);
        t.tab_inactive_bg = get("tab.inactiveBackground").unwrap_or(t.tabs_bg);
        t.tab_active_fg = get("tab.activeForeground").unwrap_or(t.fg);
        t.tab_inactive_fg = get("tab.inactiveForeground").unwrap_or(Hsla { a: 0.65, ..t.fg });
        t.focus_border = get("focusBorder").unwrap_or(t.focus_border);
        t.tab_active_border_top = get("tab.activeBorderTop").unwrap_or(t.focus_border);
        t.activity_active_border = get("activityBar.activeBorder").unwrap_or(t.activity_fg);
        t.border = get("tab.border")
            .or(get("sideBar.border"))
            .or(get("editorGroup.border"))
            .unwrap_or(if has_bg { shade(bg, if is_dark { 0.06 } else { -0.1 }) } else { t.border });
        t.status_bg = get("statusBar.background").unwrap_or(t.activity_bg);
        t.status_fg = get("statusBar.foreground").unwrap_or(t.fg);
        t.list_hover_bg = get("list.hoverBackground").unwrap_or(t.list_hover_bg);
        t.list_active_bg = get("list.inactiveSelectionBackground")
            .or(get("list.activeSelectionBackground"))
            .unwrap_or(t.list_active_bg);
        t.button_bg = get("button.background").unwrap_or(t.button_bg);
        t.button_fg = get("button.foreground").unwrap_or(t.button_fg);
        t.button_hover_bg = get("button.hoverBackground").unwrap_or(t.button_bg);
        t.badge_bg = get("badge.background").unwrap_or(t.badge_bg);
        t.badge_fg = get("badge.foreground").unwrap_or(t.badge_fg);
        t.notification_bg = get("notifications.background").unwrap_or(t.sidebar_bg);
        t.error_fg = get("errorForeground").unwrap_or(t.error_fg);
        t.term_bg = get("terminal.background").unwrap_or(bg);
        t.term_fg = get("terminal.foreground").unwrap_or(t.fg);
        t.term_cursor = get("terminalCursor.foreground").or(get("editorCursor.foreground")).unwrap_or(t.term_cursor);
        t.term_selection = get("terminal.selectionBackground")
            .or(get("editor.selectionBackground"))
            .unwrap_or(t.term_selection);
        const ANSI: [&str; 16] = [
            "Black", "Red", "Green", "Yellow", "Blue", "Magenta", "Cyan", "White", "BrightBlack",
            "BrightRed", "BrightGreen", "BrightYellow", "BrightBlue", "BrightMagenta", "BrightCyan",
            "BrightWhite",
        ];
        for (i, name) in ANSI.iter().enumerate() {
            if let Some(color) = get(&format!("terminal.ansi{name}")) {
                t.ansi[i] = color;
            }
        }
        Ok(t)
    }
}

fn collect_colors(
    path: &Path,
    depth: usize,
    colors: &mut HashMap<String, String>,
    kind: &mut Option<String>,
) -> Result<()> {
    if depth > 8 {
        bail!("theme include chain too deep");
    }
    let text = crate::catalog::read_with_timeout(path, std::time::Duration::from_secs(2))
        .with_context(|| format!("couldn't read {}", path.display()))?;
    let value: serde_json::Value =
        json5::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    // Included base theme first so this file's colors win.
    if let Some(include) = value.get("include").and_then(|v| v.as_str()) {
        if let Some(dir) = path.parent() {
            let _ = collect_colors(&dir.join(include), depth + 1, colors, kind);
        }
    }
    if let Some(k) = value.get("type").and_then(|v| v.as_str()) {
        *kind = Some(k.to_string());
    }
    if let Some(map) = value.get("colors").and_then(|v| v.as_object()) {
        for (key, v) in map {
            if let Some(s) = v.as_str() {
                colors.insert(key.clone(), s.to_string());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn loads_jsonc_theme_with_include() {
        let dir = std::env::temp_dir().join(format!("deck-theme-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("base.json"), r##"{ "colors": { "editor.background": "#101010", "terminal.ansiRed": "#ff0000" } }"##).unwrap();
        fs::write(
            dir.join("t.json"),
            r##"{
                // comment
                "include": "./base.json",
                "type": "dark",
                "colors": { "editor.background": "#202020", "statusBar.background": "#007acc80", },
            }"##,
        )
        .unwrap();
        let t = Theme::load_vscode(&dir.join("t.json"), "Test", "vs-dark").unwrap();
        assert_eq!(t.editor_bg, hex("#202020").unwrap());
        assert_eq!(t.ansi[1], hex("#ff0000").unwrap());
        assert!((t.status_bg.a - 0.5).abs() < 0.01);
        assert!(t.is_dark);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn legible_lifts_only_faint_text() {
        let t = Theme::dark_modern().glassy();
        let dim = hex("#666666").unwrap();
        assert!(t.legible(dim).l > dim.l);
        let bright = hex("#E0E0E0").unwrap();
        assert_eq!(t.legible(bright).l, bright.l);
        let light = Theme::light_modern().glassy();
        assert!(light.legible(hex("#BBBBBB").unwrap()).l < hex("#BBBBBB").unwrap().l);
    }
}
