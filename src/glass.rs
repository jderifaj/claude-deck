//! Liquid Glass behind the window: a native `NSGlassEffectView` (macOS 26+)
//! under GPUI's drawing surface, with the older blurred backdrop elsewhere.
//! The UI then draws its surfaces see-through (`Theme::glassy`).

// objc 0.2's `msg_send!` checks a `cargo-clippy` cfg this crate doesn't declare.
#![allow(unexpected_cfgs)]

use gpui::{App, Global, Window, WindowBackgroundAppearance};

/// Present once the window has a see-through backdrop. Holds the view the
/// backdrop lives in, whose appearance (light or dark glass) follows the theme.
pub struct Glass(usize);

impl Global for Glass {}

pub fn enabled(cx: &App) -> bool {
    cx.has_global::<Glass>()
}

/// Put a glass backdrop behind the window's content, or return `None` if the
/// window couldn't be given one (it stays opaque).
pub fn install(window: &mut Window) -> Option<Glass> {
    #[cfg(target_os = "macos")]
    unsafe {
        use cocoa::{base::{NO, YES}, foundation::{NSPoint, NSRect, NSSize}};
        use objc::{msg_send, runtime::{Class, Object}, sel, sel_impl};
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let Ok(handle) = window.window_handle() else { return None };
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return None };
        let view = handle.ns_view.as_ptr() as *mut Object;
        let superview: *mut Object = msg_send![view, superview];
        if superview.is_null() {
            return None;
        }
        let Some(glass_class) = Class::get("NSGlassEffectView") else {
            window.set_background_appearance(WindowBackgroundAppearance::Blurred);
            return Some(Glass(superview as usize));
        };
        window.set_background_appearance(WindowBackgroundAppearance::Transparent);

        let glass: *mut Object = msg_send![glass_class, alloc];
        let glass: *mut Object = msg_send![glass, initWithFrame: NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.))];
        let _: () = msg_send![glass, setTranslatesAutoresizingMaskIntoConstraints: NO];
        // NSWindowBelow GPUI's view, so the UI draws on top of the glass.
        let _: () = msg_send![superview, addSubview: glass positioned: -1i64 relativeTo: view];
        let _: () = msg_send![glass, release];
        // Pin all four edges so the glass always fills the window.
        for edge in [sel!(leadingAnchor), sel!(trailingAnchor), sel!(topAnchor), sel!(bottomAnchor)] {
            let a: *mut Object = msg_send![glass, performSelector: edge];
            let b: *mut Object = msg_send![superview, performSelector: edge];
            let constraint: *mut Object = msg_send![a, constraintEqualToAnchor: b];
            let _: () = msg_send![constraint, setActive: YES];
        }
        Some(Glass(superview as usize))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
        None
    }
}

/// Use dark or light glass, to suit the theme drawn over it.
pub fn match_theme(is_dark: bool, cx: &App) {
    let Some(Glass(view)) = cx.try_global::<Glass>() else { return };
    #[cfg(target_os = "macos")]
    unsafe {
        use cocoa::foundation::NSString;
        use objc::{class, msg_send, runtime::Object, sel, sel_impl};

        let name = NSString::alloc(cocoa::base::nil).init_str(if is_dark { "NSAppearanceNameDarkAqua" } else { "NSAppearanceNameAqua" });
        let appearance: *mut Object = msg_send![class!(NSAppearance), appearanceNamed: name];
        let _: () = msg_send![name, release];
        let _: () = msg_send![*view as *mut Object, setAppearance: appearance];
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (view, is_dark);
}
