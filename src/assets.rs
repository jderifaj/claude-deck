//! Embedded line icons (tinted by GPUI via text color).

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

const S: &str = r##"fill="none" stroke="#000" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round""##;

fn icon(name: &str) -> Option<String> {
    let body = match name {
        "claude" => r#"<path stroke-width="2" d="M12.3 9.8 13.2 2.6M13.3 10.2 16.5 6.0M14.0 11.1 20.7 8.3M14.2 12.3 19.4 12.9M13.8 13.3 19.6 17.7M12.9 14.0 14.9 18.9M11.7 14.2 10.8 21.4M10.7 13.8 7.5 18.0M10.0 12.9 3.3 15.7M9.8 11.7 4.6 11.1M10.2 10.7 4.4 6.3M11.1 10.0 9.1 5.1"/>"#,
        "terminal" => r#"<rect x="3" y="4" width="18" height="16" rx="2"/><path d="m7 9 3 3-3 3"/><path d="M12.5 15H17"/>"#,
        "github" => r#"<path d="M15 22v-4a4.8 4.8 0 0 0-1-3.5c3 0 6-2 6-5.5.08-1.25-.27-2.48-1-3.5.28-1.15.28-2.35 0-3.5 0 0-1 0-3 1.5-2.64-.5-5.36-.5-8 0C6 2 5 2 5 2c-.3 1.15-.3 2.35 0 3.5A5.403 5.403 0 0 0 4 9c0 3.5 3 5.5 6 5.5-.39.49-.68 1.05-.85 1.65-.17.6-.22 1.23-.15 1.85v4"/><path d="M9 18c-4.51 2-5-2-7-2"/>"#,
        "plus" => r#"<path d="M12 5v14M5 12h14"/>"#,
        "close" => r#"<path d="m7 7 10 10M17 7 7 17"/>"#,
        "folder" => r#"<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>"#,
        "folder-open" => r#"<path d="M3 7a2 2 0 0 1 2-2h4l2 2h6a2 2 0 0 1 2 2v1"/><path d="M3 18 5.5 11h16L19 18z"/>"#,
        "branch" => r#"<circle cx="6" cy="6" r="2"/><circle cx="6" cy="18" r="2"/><circle cx="18" cy="8" r="2"/><path d="M6 8v8"/><path d="M18 10c0 4-6 3-11.5 6.5"/>"#,
        "grid" => r#"<rect x="3.5" y="3.5" width="7" height="7" rx="1"/><rect x="13.5" y="3.5" width="7" height="7" rx="1"/><rect x="3.5" y="13.5" width="7" height="7" rx="1"/><rect x="13.5" y="13.5" width="7" height="7" rx="1"/>"#,
        "tabs" => r#"<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M3 9h18M9 4v5"/>"#,
        "sidebar" => r#"<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M9 4v16"/>"#,
        "sidebar-right" => r#"<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M15 4v16"/>"#,
        "copy" => r#"<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2"/>"#,
        "chevron-down" => r#"<path d="m6 9 6 6 6-6"/>"#,
        "chevron-right" => r#"<path d="m9 6 6 6-6 6"/>"#,
        "check" => r#"<path d="m5 12 5 5 9-10"/>"#,
        "task" => r#"<circle cx="12" cy="12" r="8.5"/><path d="m8.5 12.2 2.4 2.4 4.6-5"/>"#,
        "logout" => r#"<path d="M9 4H6a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h3"/><path d="m16 17 5-5-5-5M21 12H9"/>"#,
        "external" => r#"<path d="M14 4h6v6M20 4l-9 9"/><path d="M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5"/>"#,
        "receipt" => r#"<path d="M6 3h12v18l-3-2-3 2-3-2-3 2z"/><path d="M9 8h6M9 12h6M9 16h3"/>"#,
        "restart" => r#"<path d="M4 12a8 8 0 1 0 2.5-5.8"/><path d="M4 4v4h4"/>"#,
        "paint" => r#"<circle cx="12" cy="12" r="9"/><circle cx="8" cy="10" r="1.2"/><circle cx="12" cy="7.5" r="1.2"/><circle cx="16" cy="10" r="1.2"/><path d="M12 21a3 3 0 0 1 0-6h1.5a2 2 0 0 0 0-4"/>"#,
        "package" => r#"<path d="M12 3 4 7v10l8 4 8-4V7z"/><path d="m4 7 8 4 8-4M12 11v10"/>"#,
        "trash" => r#"<path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"/>"#,
        "shield" => r#"<path d="M12 3 5 6v6c0 4.5 3 7.5 7 9 4-1.5 7-4.5 7-9V6z"/><path d="m9 12 2 2 4-4"/>"#,
        "info" => r#"<circle cx="12" cy="12" r="9"/><path d="M12 11v5M12 8h.01"/>"#,
        "warning" => r#"<path d="M12 3 2 20h20z"/><path d="M12 10v4M12 17h.01"/>"#,
        "globe" => r#"<circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3c2.5 2.5 3.8 5.5 3.8 9s-1.3 6.5-3.8 9c-2.5-2.5-3.8-5.5-3.8-9S9.5 5.5 12 3z"/>"#,
        "phone" => r#"<rect x="6.5" y="2.5" width="11" height="19" rx="2.5"/><path d="M10.5 18.5h3"/>"#,
        "tablet" => r#"<rect x="4" y="2.5" width="16" height="19" rx="2.5"/><path d="M10.5 18.5h3"/>"#,
        "spark" => r#"<path d="M12 3v4M12 17v4M3 12h4M17 12h4M5.6 5.6l2.8 2.8M15.6 15.6l2.8 2.8M5.6 18.4l2.8-2.8M15.6 8.4l2.8-2.8"/>"#,
        "web-server" => concat!(
            // A wireframe globe behind a two-unit server; the mask keeps a gap around the server.
            r##"<defs><mask id="ws-gap" stroke="none"><rect width="24" height="24" fill="#fff"/><rect x="5.3" y="11.3" width="13.4" height="13" rx="1.5" fill="#000"/></mask>"##,
            r##"<mask id="ws-slots" stroke="none"><rect width="24" height="24" fill="#fff"/><g fill="#000">"##,
            r##"<rect x="8" y="13.3" width="0.9" height="1.9"/><rect x="9.9" y="13.3" width="0.9" height="1.9"/><rect x="11.8" y="13.3" width="0.9" height="1.9"/><circle cx="15.4" cy="14.25" r="0.85"/>"##,
            r##"<rect x="8" y="17.8" width="0.9" height="1.9"/><rect x="9.9" y="17.8" width="0.9" height="1.9"/><rect x="11.8" y="17.8" width="0.9" height="1.9"/>"##,
            r##"</g></mask></defs>"##,
            r##"<g mask="url(#ws-gap)" stroke-width="1.2"><circle cx="12" cy="10" r="7.6"/><ellipse cx="12" cy="10" rx="3.6" ry="7.6"/><path d="M12 2.4v15.2M4.4 10h15.2M5.6 6h12.8"/></g>"##,
            r##"<g mask="url(#ws-slots)" fill="#000" stroke="none"><rect x="6.5" y="12.5" width="11" height="3.5" rx="0.8"/><rect x="6.5" y="17" width="11" height="3.5" rx="0.8"/>"##,
            r##"<rect x="8.2" y="20.5" width="1.4" height="2.5"/><rect x="14.4" y="20.5" width="1.4" height="2.5"/></g>"##
        ),
        _ => return None,
    };
    Some(format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" {S}>{body}</svg>"#))
}

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let name = path.trim_start_matches("icons/").trim_end_matches(".svg");
        Ok(icon(name).map(|s| Cow::Owned(s.into_bytes())))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}
