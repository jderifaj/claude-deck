//! A minimal single-line text field: typing, ←/→ (⌘ for line start/end,
//! ⇧ to select), Home/End, Backspace/Delete, ⌘A select all, ⌘C/⌘X/⌘V.
//! Enter or clicking away submits; Escape cancels.

use std::ops::Range;

use gpui::{
    ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, SharedString, Subscription, Window,
    div, prelude::*, px,
};

use crate::theme::Theme;

pub enum TextFieldEvent {
    Submit(String),
    Cancel,
}

/// The text being edited, with a caret and an optional selection. Offsets
/// are bytes, always on char boundaries.
#[derive(Default)]
struct Buffer {
    text: String,
    cursor: usize,
    /// Where the selection started; it runs from here to `cursor`.
    anchor: Option<usize>,
}

impl Buffer {
    fn new(text: &str) -> Self {
        Self { text: text.to_string(), cursor: text.len(), anchor: None }
    }

    fn selection(&self) -> Option<Range<usize>> {
        let anchor = self.anchor.filter(|a| *a != self.cursor)?;
        Some(anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    fn selected_text(&self) -> Option<&str> {
        self.selection().map(|r| &self.text[r])
    }

    fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }

    /// Move the caret to `to`, extending the selection if `extend`.
    fn move_to(&mut self, to: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = to;
    }

    /// ← / →: with a selection and no ⇧, land on its edge.
    fn step(&mut self, forward: bool, to_end: bool, extend: bool) {
        if let (Some(r), false) = (self.selection(), extend) {
            return self.move_to(if forward { r.end } else { r.start }, false);
        }
        let to = match (forward, to_end) {
            (false, true) => 0,
            (true, true) => self.text.len(),
            (false, false) => prev_boundary(&self.text, self.cursor),
            (true, false) => next_boundary(&self.text, self.cursor),
        };
        self.move_to(to, extend);
    }

    /// Remove the selection, if any. Returns whether there was one.
    fn delete_selection(&mut self) -> bool {
        let Some(r) = self.selection() else {
            self.anchor = None;
            return false;
        };
        self.text.drain(r.clone());
        self.cursor = r.start;
        self.anchor = None;
        true
    }

    fn insert(&mut self, s: &str) {
        self.delete_selection();
        let s: String = s.chars().map(|c| if c == '\n' || c == '\t' { ' ' } else { c }).filter(|c| !c.is_control()).collect();
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    /// Backspace (⌘ deletes to the line start).
    fn backspace(&mut self, to_start: bool) {
        if self.delete_selection() {
            return;
        }
        let from = if to_start { 0 } else { prev_boundary(&self.text, self.cursor) };
        self.text.drain(from..self.cursor);
        self.cursor = from;
    }

    fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let to = next_boundary(&self.text, self.cursor);
        self.text.drain(self.cursor..to);
    }
}

pub struct TextField {
    buffer: Buffer,
    placeholder: SharedString,
    focus_handle: FocusHandle,
    done: bool,
    /// Shows dots instead of the text and won't copy it (for tokens).
    secret: bool,
    _blur: Subscription,
}

impl EventEmitter<TextFieldEvent> for TextField {}

impl Focusable for TextField {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TextField {
    /// A field holding `text`, focused and ready for typing.
    pub fn new(text: &str, placeholder: impl Into<SharedString>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let blur = cx.on_blur(&focus_handle, window, |this, _, cx| this.finish(true, cx));
        window.focus(&focus_handle);
        Self { buffer: Buffer::new(text), placeholder: placeholder.into(), focus_handle, done: false, secret: false, _blur: blur }
    }

    pub fn text(&self) -> String {
        self.buffer.text.trim().to_string()
    }

    /// Hide what's typed behind dots, and don't let it be copied.
    pub fn set_secret(&mut self, secret: bool) {
        self.secret = secret;
    }

    /// How `text` is drawn: itself, or a dot per character when secret.
    fn shown(&self, text: &str) -> String {
        if self.secret { "•".repeat(text.chars().count()) } else { text.to_string() }
    }

    /// Stop emitting events (the owner has taken the text itself).
    pub fn close(&mut self) {
        self.done = true;
    }

    fn finish(&mut self, submit: bool, cx: &mut Context<Self>) {
        if self.done {
            return;
        }
        self.done = true;
        cx.emit(if submit { TextFieldEvent::Submit(self.text()) } else { TextFieldEvent::Cancel });
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let (cmd, shift) = (keystroke.modifiers.platform, keystroke.modifiers.shift);
        let b = &mut self.buffer;
        match keystroke.key.as_str() {
            "enter" => self.finish(true, cx),
            "escape" => self.finish(false, cx),
            "backspace" => b.backspace(cmd),
            "delete" => b.delete_forward(),
            "left" => b.step(false, cmd, shift),
            "right" => b.step(true, cmd, shift),
            "home" | "up" => b.move_to(0, shift),
            "end" | "down" => b.move_to(b.text.len(), shift),
            "a" if cmd => b.select_all(),
            "c" | "x" if cmd && self.secret => return,
            "c" | "x" if cmd => {
                if let Some(text) = b.selected_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                    if keystroke.key == "x" {
                        b.delete_selection();
                    }
                }
            }
            "v" if cmd => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    b.insert(text.trim());
                }
            }
            _ if !cmd && !keystroke.modifiers.control => match &keystroke.key_char {
                Some(ch) => b.insert(ch),
                None => return,
            },
            // Leave other shortcuts to the app.
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }
}

/// Start of the character before byte offset `at`.
fn prev_boundary(text: &str, at: usize) -> usize {
    text[..at].char_indices().next_back().map_or(0, |(i, _)| i)
}

/// End of the character after byte offset `at`.
fn next_boundary(text: &str, at: usize) -> usize {
    text[at..].chars().next().map_or(at, |c| at + c.len_utf8())
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::current(cx);
        let focused = self.focus_handle.is_focused(window);
        let caret = || div().flex_none().w(px(1.)).h(px(15.)).when(focused, |d| d.bg(t.fg));
        let b = &self.buffer;
        div()
            .track_focus(&self.focus_handle)
            .key_context("TextField")
            .on_key_down(cx.listener(Self::key_down))
            .h(px(26.))
            .px(px(6.))
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .rounded(px(3.))
            .bg(t.editor_bg)
            .border_1()
            .border_color(if focused { t.focus_border } else { t.border })
            .text_color(t.fg)
            .when(b.text.is_empty(), |d| {
                d.child(caret()).child(div().text_color(t.fg_muted).child(self.placeholder.clone()))
            })
            .when(!b.text.is_empty(), |d| match b.selection() {
                Some(r) => d
                    .child(self.shown(&b.text[..r.start]))
                    .child(div().bg(t.term_selection).child(self.shown(&b.text[r.clone()])))
                    .child(self.shown(&b.text[r.end..])),
                None => d
                    .child(self.shown(&b.text[..b.cursor]))
                    .child(caret())
                    .child(self.shown(&b.text[b.cursor..])),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_all_then_type_replaces() {
        let mut b = Buffer::new("ABC-123");
        b.select_all();
        assert_eq!(b.selected_text(), Some("ABC-123"));
        b.insert("X");
        assert_eq!((b.text.as_str(), b.cursor, b.selection()), ("X", 1, None));
    }

    #[test]
    fn select_all_then_backspace_clears() {
        let mut b = Buffer::new("abc");
        b.select_all();
        b.backspace(false);
        assert_eq!((b.text.as_str(), b.cursor), ("", 0));
    }

    #[test]
    fn arrows_collapse_or_extend_selection() {
        let mut b = Buffer::new("abcd");
        b.select_all();
        b.step(false, false, false);
        assert_eq!((b.cursor, b.selection()), (0, None));
        b.step(true, false, true);
        b.step(true, false, true);
        assert_eq!(b.selected_text(), Some("ab"));
        b.delete_forward();
        assert_eq!(b.text, "cd");
    }

    #[test]
    fn caret_moves_by_whole_characters() {
        let text = "aé😀b";
        assert_eq!(&text[1..next_boundary(text, 1)], "é");
        let end = text.len() - 1;
        assert_eq!(&text[prev_boundary(text, end)..end], "😀");
        assert_eq!(prev_boundary(text, 0), 0);
        assert_eq!(next_boundary(text, text.len()), text.len());
    }
}
