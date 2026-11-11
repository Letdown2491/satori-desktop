//! A minimal multi-line text input, GPUI-native.
//!
//! TODO(port): crates/app used AdwTextView; gpui 0.2.2 ships no editor, so this
//! adapts the gpui `input.rs` example (IME path, actions, mouse selection) and
//! extends it to wrapped multi-line: one shaped block per paragraph, cursor and
//! selection mapped through WrappedLineLayout::position_for_index. The
//! composer's article body drove the full editor pass: vertical motion
//! (goal-column memory over wrapped rows), line-wise home/end with
//! ctrl-variants for the doc extremes, word-wise motion and deletion,
//! and a caret-reveal that scrolls the host scroller (scroll_reveal) to
//! keep the caret in view.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, Global, GlobalElementId, KeyBinding,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels,
    Point as GpPoint, Render, SharedString, Style, TextAlign, TextRun, UTF16Selection, Window,
    WrappedLine, actions, div, fill, hsla, prelude::*, px, relative, rgb, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme;

actions!(
    input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Paste,
        Copy,
        Cut,
        // Vertical motion (the article body's need): goal-column moves.
        Up,
        Down,
        SelectUp,
        SelectDown,
        // Line-wise home/end + the doc-absolute extremes (ctrl variants).
        LineStart,
        LineEnd,
        SelectLineStart,
        SelectLineEnd,
        DocStart,
        DocEnd,
        // Word-wise motion (ctrl+arrows) + word deletion (ctrl+backspace/delete).
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        DeleteWordLeft,
        DeleteWordRight,
    ]
);

pub fn bind_input_keys(cx: &mut App) {
    // Scoped to the "Input" key context so these never hijack global keys.
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("Input")),
        KeyBinding::new("delete", Delete, Some("Input")),
        KeyBinding::new("left", Left, Some("Input")),
        KeyBinding::new("right", Right, Some("Input")),
        KeyBinding::new("up", Up, Some("Input")),
        KeyBinding::new("down", Down, Some("Input")),
        KeyBinding::new("shift-left", SelectLeft, Some("Input")),
        KeyBinding::new("shift-right", SelectRight, Some("Input")),
        KeyBinding::new("shift-up", SelectUp, Some("Input")),
        KeyBinding::new("shift-down", SelectDown, Some("Input")),
        KeyBinding::new("ctrl-left", WordLeft, Some("Input")),
        KeyBinding::new("ctrl-right", WordRight, Some("Input")),
        KeyBinding::new("shift-ctrl-left", SelectWordLeft, Some("Input")),
        KeyBinding::new("shift-ctrl-right", SelectWordRight, Some("Input")),
        KeyBinding::new("ctrl-a", SelectAll, Some("Input")),
        KeyBinding::new("cmd-a", SelectAll, Some("Input")),
        KeyBinding::new("ctrl-v", Paste, Some("Input")),
        KeyBinding::new("cmd-v", Paste, Some("Input")),
        KeyBinding::new("ctrl-c", Copy, Some("Input")),
        KeyBinding::new("cmd-c", Copy, Some("Input")),
        KeyBinding::new("ctrl-x", Cut, Some("Input")),
        KeyBinding::new("cmd-x", Cut, Some("Input")),
        // Home/end are LINE-wise (the standard in editors); the doc
        // extremes take ctrl.
        KeyBinding::new("home", LineStart, Some("Input")),
        KeyBinding::new("end", LineEnd, Some("Input")),
        KeyBinding::new("shift-home", SelectLineStart, Some("Input")),
        KeyBinding::new("shift-end", SelectLineEnd, Some("Input")),
        KeyBinding::new("ctrl-home", DocStart, Some("Input")),
        KeyBinding::new("ctrl-end", DocEnd, Some("Input")),
        KeyBinding::new("ctrl-backspace", DeleteWordLeft, Some("Input")),
        KeyBinding::new("ctrl-delete", DeleteWordRight, Some("Input")),
    ]);
}

/// One shaped paragraph ('\n'-separated) plus its byte offset in the content.
#[derive(Clone)]
struct Para {
    // P5 (D35): WrappedLine dropped Clone in the snapshot; every use here
    // is a shared read, so the Arc carries the clonability.
    line: std::sync::Arc<WrappedLine>,
    /// Byte offset of the paragraph's first byte in the full content.
    start: usize,
    /// Byte length of the paragraph text (excluding the trailing '\n').
    len: usize,
    /// Top of this paragraph's block within the editor, in px.
    y: Pixels,
}

/// Snapshot of the last layout, kept for hit-testing between frames.
struct Layout {
    paras: Vec<Para>,
    line_height: Pixels,
}

impl Layout {
    /// (paragraph ix, byte offset within the paragraph) for a content offset.
    fn locate(&self, offset: usize) -> (usize, usize) {
        for (ix, para) in self.paras.iter().enumerate() {
            if offset <= para.start + para.len {
                return (ix, (offset - para.start).min(para.len));
            }
        }
        match self.paras.last() {
            Some(last) => (self.paras.len() - 1, last.len),
            None => (0, 0),
        }
    }
}

pub struct Input {
    focus_handle: FocusHandle,
    pub content: String,
    placeholder: SharedString,
    pub multiline: bool,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<Layout>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    /// Single-line horizontal scroll: how far the line is shifted left so
    /// the caret stays visible ( bunker URIs run long). 0 for multiline.
    scroll_x: Pixels,
    /// The blink: each activity bumps the epoch (a newer task owns the
    /// caret, the old loop dies on its next tick) and the loop toggles
    /// `caret_on` while it still holds the epoch. The loop self-terminates
    /// when render sees the field lost focus (render bumps the epoch —
    /// render runs on the loop's own notify, so this catches within a tick).
    blink_epoch: u64,
    caret_on: bool,
    /// WS4 nsec tripwire (D17): when on, a paste that looks like a private
    /// key (nsec1…/ncryptsec1…) is INTERCEPTED — nothing is inserted, the
    /// value is never logged — and the shared [`NsecWarn`] global lights a
    /// dismissible strip in the hosting pane. Off for fields that
    /// legitimately take a key (the login door).
    pub tripwire: bool,
    /// The composer's `:`-emoji typeahead owns Enter while its strip is
    /// open (it inserts the shortcode); this field must not add a newline
    /// under it. The composer sets it from detect_emoji every frame.
    pub suppress_newline: bool,
    /// Multi-line wrapping but ONE paragraph: Enter inserts nothing (a
    /// title/summary never wants a newline; pasted newlines still fold to
    /// spaces like a single-line field). The article fields' shape.
    pub single_paragraph: bool,
    /// The remembered x for vertical motion (up/down): the column the
    /// caret returns to across short lines. None until an up/down runs;
    /// every horizontal move or click clears it.
    goal_x: Option<Pixels>,
    /// The host scroller for the caret-reveal (the composer's body box,
    /// set once at construction): multiline motion scrolls it so the
    /// caret stays visible — the "End jumped somewhere invisible" fix.
    /// None where no scroller wraps the field (single-line panels).
    scroll_reveal: Option<gpui::ScrollHandle>,
}

/// The tripwire's shared alert (true = show the strip). A bool global, not
/// per-entity state: the paste happens inside the Input entity, the strip
/// renders in the pane around it. Carries NO pasted value — the key never
/// leaves the clipboard un-inspected, and nothing logs it.
pub struct NsecWarn(pub bool);
impl Global for NsecWarn {}

/// Looks like a bech32 private key (NIP-06 nsec / NIP-49 ncryptsec)? The
/// prefix is the tell; no parsing, no logging of the value.
fn looks_like_nsec(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("nsec1") || t.starts_with("ncryptsec1")
}

/// The classic caret duty cycle.
const BLINK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(530);

impl Input {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            placeholder: String::new().into(),
            multiline: true,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            scroll_x: px(0.),
            blink_epoch: 0,
            caret_on: true,
            tripwire: false,
            suppress_newline: false,
            single_paragraph: false,
            goal_x: None,
            scroll_reveal: None,
        }
    }

    /// Hand the Input its host scroller (the composer's body box): every
    /// caret move then reveals the caret in it. Call once at build.
    pub fn set_scroll_reveal(&mut self, handle: gpui::ScrollHandle) {
        self.scroll_reveal = Some(handle);
    }

    pub fn placeholder(mut self, placeholder: &str) -> Self {
        self.placeholder = placeholder.to_string().into();
        self
    }

    /// Replace the whole buffer, caret at the end (draft restore).
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.content = text.to_string();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.select_to(self.next_boundary(self.selected_range.end), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn doc_start(&mut self, _: &DocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.move_to(0, cx);
    }

    fn doc_end(&mut self, _: &DocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.move_to(self.content.len(), cx);
    }

    /// The caret's visual (para, byte-col) position against the last
    /// layout — the precondition for every line/vertical motion.
    fn caret_layout(&self) -> Option<(usize, usize)> {
        self.last_layout
            .as_ref()
            .map(|l| l.locate(self.cursor_offset()))
    }

    fn line_start(&mut self, _: &LineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        if let Some((ix, _)) = self.caret_layout() {
            let start = self.last_layout.as_ref().unwrap().paras[ix].start;
            self.move_to(start, cx);
        } else {
            self.move_to(0, cx);
        }
    }

    fn line_end(&mut self, _: &LineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        if let Some((ix, _)) = self.caret_layout() {
            let layout = self.last_layout.as_ref().unwrap();
            let para = &layout.paras[ix];
            self.move_to(para.start + para.len, cx);
        } else {
            self.move_to(self.content.len(), cx);
        }
    }

    fn select_line_start(&mut self, _: &SelectLineStart, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((ix, _)) = self.caret_layout() {
            let start = self.last_layout.as_ref().unwrap().paras[ix].start;
            self.select_to(start, cx);
        } else {
            self.select_to(0, cx);
        }
    }

    fn select_line_end(&mut self, _: &SelectLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((ix, _)) = self.caret_layout() {
            let layout = self.last_layout.as_ref().unwrap();
            let para = &layout.paras[ix];
            self.select_to(para.start + para.len, cx);
        } else {
            self.select_to(self.content.len(), cx);
        }
    }

    /// Whitespace-delimited word start backwards from `off` (skip the
    /// space run, then the word run).
    fn previous_word(&self, off: usize) -> usize {
        let chars: Vec<(usize, char)> = self.content.char_indices().collect();
        let mut skipped_word = 0;
        for (idx, c) in chars[..chars.partition_point(|(i, _)| *i < off)]
            .iter()
            .rev()
        {
            if c.is_whitespace() {
                if skipped_word > 0 {
                    return idx + c.len_utf8();
                }
            } else {
                skipped_word += 1;
            }
        }
        0
    }

    /// Whitespace-delimited word end forwards from `off`.
    fn next_word(&self, off: usize) -> usize {
        let mut seen_word = false;
        for (i, c) in self.content.char_indices().skip_while(|(i, _)| *i < off) {
            if c.is_whitespace() && seen_word {
                return i;
            }
            seen_word |= !c.is_whitespace();
        }
        self.content.len()
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.move_to(self.previous_word(self.cursor_offset()), cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.move_to(self.next_word(self.cursor_offset()), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.select_to(self.previous_word(self.cursor_offset()), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.select_to(self.next_word(self.cursor_offset()), cx);
    }

    fn delete_word_left(
        &mut self,
        _: &DeleteWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            let target = self.previous_word(self.cursor_offset());
            if target < self.cursor_offset() {
                self.select_to(target, cx);
            }
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete_word_right(
        &mut self,
        _: &DeleteWordRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            let target = self.next_word(self.cursor_offset());
            if target > self.cursor_offset() {
                self.select_to(target, cx);
            }
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    /// Vertical motion (the P2 scope the article body needed): one visual
    /// WRAPPED row per press, the goal column remembered across short
    /// lines. Empty paragraphs shape as a space, so their rows position.
    fn vertical(&mut self, dir: i32, select: bool, cx: &mut Context<Self>) {
        if !self.multiline {
            return;
        }
        // The emoji strip owns the arrows while it's open (the strip's
        // selection moves; the caret must not wander underneath).
        if self.suppress_newline {
            return;
        }
        let Some(layout) = self.last_layout.as_ref() else {
            return;
        };
        let lh = layout.line_height;
        if layout.paras.is_empty() {
            return;
        }
        let cursor = self.cursor_offset();
        let (pix, pcol) = layout.locate(cursor);
        let para = &layout.paras[pix];
        let pos = para.line.position_for_index(pcol, lh).unwrap_or_default();
        let goal = self.goal_x.unwrap_or(pos.x);
        let row = (pos.y / lh).round() as i32;
        let rows_in =
            |p: &Para, lh: Pixels| -> i32 { ((p.line.size(lh).height / lh).round() as i32).max(1) };

        // The target row: same paragraph one row over, else across the
        // paragraph boundary (last row of the previous / first row of the
        // next). At the very top/bottom: clamp — no move, the goal column
        // is forgotten (a fresh down starts from the caret's own column).
        let target: Option<(usize, i32)> = if dir < 0 {
            if row > 0 {
                Some((pix, row - 1))
            } else if pix > 0 {
                let prev = &layout.paras[pix - 1];
                Some((pix - 1, rows_in(prev, lh) - 1))
            } else {
                None
            }
        } else if row + 1 < rows_in(para, lh) {
            Some((pix, row + 1))
        } else if pix + 1 < layout.paras.len() {
            Some((pix + 1, 0))
        } else {
            None
        };
        let Some((tpix, trow)) = target else {
            self.goal_x = None;
            return;
        };
        let tpara = &layout.paras[tpix];
        let idx = tpara
            .line
            .closest_index_for_position(GpPoint::new(goal, trow as f32 * lh), lh)
            .unwrap_or_else(|clamped| clamped);
        let offset = (tpara.start + idx).min(tpara.start + tpara.len);
        self.goal_x = Some(goal);
        if select {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1, false, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1, false, cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1, true, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1, true, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.selected_range.end), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            // D17 tripwire: a key pasted into a non-secret field is the
            // accident that ends accounts. Intercept, warn, insert NOTHING,
            // log NOTHING (not even a prefix — the value stays in the
            // clipboard where the user put it).
            if self.tripwire && looks_like_nsec(&text) {
                cx.update_global::<NsecWarn, _>(|w, _| w.0 = true);
                cx.notify();
                return;
            }
            let text = if self.multiline {
                text
            } else {
                text.replace('\n', " ")
            };
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // IME composition in progress — let it commit on its own.
        if self.marked_range.is_some() {
            return;
        }
        if ev.keystroke.key == "enter"
            && self.multiline
            && !self.single_paragraph
            && !ev.keystroke.modifiers.platform
            && !self.suppress_newline
        {
            self.insert("\n", cx);
        }
    }

    fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        let range = self.selected_range.clone();
        self.content = self.content[0..range.start].to_string() + text + &self.content[range.end..];
        self.selected_range = range.start + text.len()..range.start + text.len();
        self.restart_blink(cx);
        self.reveal_caret();
        cx.notify();
    }

    /// Empty the field and reset the cursor (an add-row after Add).
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.content.clear();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.scroll_x = px(0.);
        cx.notify();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Clicking a field hands IT the caret — every input (note, question,
        // options) needs no parent to do this for it.
        window.focus(&self.focus_handle, cx);
        self.goal_x = None;
        self.is_selecting = true;
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.restart_blink(cx);
        self.reveal_caret();
        cx.notify();
    }

    /// Multiline: the shaped content's full height (the host scroller's
    /// overflow comes from the ELEMENT being content-sized — see
    /// InputElement::request_layout). One frame behind an edit; exact for
    /// pure caret motion.
    pub fn content_height(&self) -> Option<Pixels> {
        let layout = self.last_layout.as_ref()?;
        let last = layout.paras.last()?;
        Some(last.y + last.line.size(layout.line_height).height)
    }

    /// Scroll the host scroller (scroll_reveal) so the caret's row is
    /// visible — the fix for "End jumped somewhere invisible". Offset
    /// convention (gpui): NEGATIVE going down, content paints at
    /// parent_top + offset. A no-op without a handle or layout.
    fn reveal_caret(&self) {
        let Some(handle) = &self.scroll_reveal else {
            return;
        };
        let Some(layout) = self.last_layout.as_ref() else {
            return;
        };
        let lh = layout.line_height;
        let (pix, pcol) = layout.locate(self.cursor_offset());
        let para = &layout.paras[pix];
        let Some(pos) = para.line.position_for_index(pcol, lh) else {
            return;
        };
        let caret_y = para.y + pos.y; // content-local (the input IS the content)
        let viewport_h = handle.bounds().size.height;
        let offset = handle.offset().y;
        let margin = lh; // one line of context above/below the caret
        let mut new_offset = offset;
        if caret_y < -offset + margin {
            new_offset = -(caret_y - margin).max(px(0.));
        } else if caret_y + lh > -offset + viewport_h - margin {
            let max_scroll = (self.content_height().unwrap_or(px(0.)) - viewport_h).max(px(0.));
            new_offset = -(caret_y + lh + margin - viewport_h)
                .min(max_scroll)
                .max(px(0.));
        }
        if new_offset != offset {
            handle.set_offset(GpPoint::new(px(0.), new_offset));
        }
    }

    /// Keep the caret blinking from NOW: any caret move or edit calls this.
    /// Reduced-motion desktops get a steady caret — visibility, not motion.
    fn restart_blink(&mut self, cx: &mut Context<Self>) {
        self.blink_epoch += 1;
        self.caret_on = true;
        if crate::desktop::Desktop::get(cx).reduced_motion {
            return;
        }
        let epoch = self.blink_epoch;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK_INTERVAL).await;
                let alive = this
                    .update(cx, |this, cx| {
                        if this.blink_epoch != epoch {
                            return false; // a newer activity owns the caret
                        }
                        this.caret_on = !this.caret_on;
                        cx.notify();
                        true
                    })
                    .is_ok_and(|alive| alive);
                if !alive {
                    return;
                }
            }
        })
        .detach();
    }

    /// The caret's byte offset in `content` (pub: the composer's
    /// @-mention detection reads it).
    pub fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: GpPoint<Pixels>) -> usize {
        let (Some(layout), Some(bounds)) = (self.last_layout.as_ref(), self.last_bounds.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        let local_y = position.y - bounds.top();
        let scroll_x = self.scroll_x;
        for para in &layout.paras {
            let para_bottom = para.y + para.line.size(layout.line_height).height;
            let is_last =
                para.start + para.len >= self.content.len() && !self.content.ends_with('\n');
            if local_y <= para_bottom || is_last {
                let local = GpPoint::new(position.x - bounds.left() + scroll_x, local_y - para.y);
                let col = para
                    .line
                    .closest_index_for_position(local, layout.line_height)
                    .unwrap_or_else(|clamped| clamped);
                return (para.start + col).min(para.start + para.len);
            }
        }
        self.content.len()
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.restart_blink(cx);
        self.reveal_caret();
        cx.notify();
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }
}

impl EntityInputHandler for Input {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Single-line mode strips newlines (IME included) — so does a
        // single-paragraph field (a pasted multi-line title folds flat).
        let owned;
        let new_text = if self.multiline && !self.single_paragraph {
            new_text
        } else {
            owned = new_text.replace('\n', " ");
            &owned
        };
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            self.content[0..range.start].to_string() + new_text + &self.content[range.end..];
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        self.restart_blink(cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            self.content[0..range.start].to_string() + new_text + &self.content[range.end..];
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|new_range| new_range.start + range.start..new_range.end + range.end)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let (para_ix, col) = layout.locate(range.start);
        let para = layout.paras.get(para_ix)?;
        let pos = para.line.position_for_index(col, layout.line_height)?;
        Some(Bounds::from_corners(
            GpPoint::new(bounds.left() + pos.x, bounds.top() + para.y + pos.y),
            GpPoint::new(
                bounds.left() + pos.x + px(2.),
                bounds.top() + para.y + pos.y + layout.line_height,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: GpPoint<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let bounds = self.last_bounds?;
        let local = point - bounds.origin;
        let layout = self.last_layout.as_ref()?;
        for para in &layout.paras {
            let para_bottom = para.y + para.line.size(layout.line_height).height;
            if local.y <= para_bottom {
                let ix = para
                    .line
                    .closest_index_for_position(
                        GpPoint::new(local.x, local.y - para.y),
                        layout.line_height,
                    )
                    .unwrap_or_else(|clamped| clamped);
                return Some(self.offset_to_utf16(para.start + ix));
            }
        }
        None
    }
}

/// The shaped-text element painted inside the input's container.
struct InputElement {
    input: Entity<Input>,
}

struct PrepaintState {
    paras: Vec<Para>,
    line_height: Pixels,
    selection: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
}

impl IntoElement for InputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for InputElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        // Multiline: the element IS the scroll CONTENT — its height is the
        // shaped text's height, so the host scroller (the composer's body
        // box) actually overflows and scrolls. (Filling the parent meant
        // the scroller's child never overflowed: text clipped, wheel dead
        // — "no scrolling in the body".) Measured from the last prepaint
        // (one frame behind an edit; the first frame fills the parent).
        let content_h = if self.input.read(cx).multiline {
            self.input.read(cx).content_height()
        } else {
            None
        };
        match content_h {
            Some(h) => style.size.height = h.into(),
            // One definite line: the fallback for auto-height parents (the
            // composer's title/summary rows) — a relative height collapses
            // to zero against an auto parent, and it matches an empty
            // field's real height, so the first frame is already right.
            None if self.input.read(cx).multiline => {
                style.size.height = window.line_height().into();
            }
            None => style.size.height = relative(1.).into(),
        }
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        // One content clone per prepaint (the audit's note): the shaped
        // text needs an owned String, nothing else does.
        let empty = input.content.is_empty();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let multiline = input.multiline;
        let mut scroll_x = input.scroll_x;
        let caret_visible = input.caret_on && input.focus_handle.is_focused(window);
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();

        let text_color = if empty {
            // Placeholder wash: the field's own ink, rested — visible on
            // either paper (the fixed gray was invisible at night).
            let c = style.color;
            hsla(c.h, c.s, c.l, 0.45)
        } else {
            style.color
        };
        let display_text: SharedString = if empty {
            input.placeholder.clone()
        } else {
            input.content.clone().into()
        };

        // One shaped block per paragraph ('\n'-separated).
        let mut paras: Vec<Para> = Vec::new();
        let mut y = px(0.);
        let mut byte_start = 0usize;
        for seg in display_text.split('\n') {
            let run = TextRun {
                len: seg.len().max(1),
                font: style.font(),
                color: text_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            // Empty paragraphs shape as a single space so the cursor has a
            // position to live at; their mapping length stays 0.
            let shape_text: SharedString = if seg.is_empty() {
                " ".to_string().into()
            } else {
                seg.to_string().into()
            };
            let shaped = window
                .text_system()
                .shape_text(
                    shape_text,
                    font_size,
                    std::slice::from_ref(&run),
                    // Single-line never wraps — it scrolls horizontally.
                    if multiline {
                        Some(bounds.size.width)
                    } else {
                        None
                    },
                    None,
                )
                .ok()
                .and_then(|mut lines| lines.pop());
            if let Some(line) = shaped {
                paras.push(Para {
                    line: std::sync::Arc::new(line),
                    start: byte_start,
                    len: seg.len(),
                    y,
                });
                y += paras.last().unwrap().line.size(line_height).height;
            }
            byte_start += seg.len() + 1;
        }

        // Single-line: keep the caret inside the visible window by shifting
        // the line left (multiline wraps instead — para.y grows downward).
        if !multiline && let Some(para) = paras.first() {
            let vis = bounds.size.width;
            let caret = para
                .line
                .position_for_index(cursor - para.start, line_height)
                .map(|p| p.x)
                .unwrap_or(px(0.));
            let scroll_max = (para.line.width() - vis).max(px(0.));
            if caret < scroll_x {
                scroll_x = (caret - px(24.)).max(px(0.));
            } else if caret > scroll_x + vis - px(24.) {
                scroll_x = (caret - vis + px(24.)).min(scroll_max).max(px(0.));
            }
            scroll_x = scroll_x.min(scroll_max);
            let scrolled = scroll_x;
            self.input.update(cx, |input, _| input.scroll_x = scrolled);
        }

        // Selection quads, one per visual row the selection touches.
        let mut selection = Vec::new();
        if !empty && !selected_range.is_empty() {
            for para in &paras {
                let para_end = para.start + para.len;
                let sel = selected_range.start.max(para.start)..selected_range.end.min(para_end);
                if sel.start >= sel.end {
                    continue;
                }
                for (row_start, row_end) in rows_of(para) {
                    let rs = sel.start.max(para.start + row_start);
                    let re = sel.end.min(para.start + row_end);
                    if rs >= re {
                        continue;
                    }
                    let lh = line_height;
                    let p1 = para
                        .line
                        .position_for_index(rs - para.start, lh)
                        .unwrap_or_else(|| GpPoint::new(px(0.), px(0.)));
                    let p2 = para
                        .line
                        .position_for_index(re - para.start, lh)
                        .unwrap_or_else(|| GpPoint::new(para.line.width(), p1.y));
                    let row_y = para.y + p1.y;
                    selection.push(fill(
                        Bounds::from_corners(
                            GpPoint::new(bounds.left() - scroll_x + p1.x, bounds.top() + row_y),
                            GpPoint::new(
                                bounds.left() - scroll_x + p2.x.max(p1.x),
                                bounds.top() + row_y + lh,
                            ),
                        ),
                        hsla(0.08, 0.5, 0.5, 0.18), // faint vermilion wash
                    ));
                }
            }
        }

        // Cursor quad. Focused-only (paint re-gates), blinking, and — the
        // click-to-type affordance — present on an EMPTY field too: the
        // gate used to be `!empty`, so a focused empty field gave no
        // signal at all until the first keystroke.
        let mut cursor_quad = None;
        if caret_visible {
            for para in &paras {
                if cursor >= para.start && cursor <= para.start + para.len {
                    let lh = line_height;
                    if let Some(pos) = para.line.position_for_index(cursor - para.start, lh) {
                        cursor_quad = Some(fill(
                            Bounds::new(
                                GpPoint::new(
                                    bounds.left() - scroll_x + pos.x,
                                    bounds.top() + para.y + pos.y,
                                ),
                                size(px(2.), lh),
                            ),
                            rgb(theme::vermilion()),
                        ));
                    }
                    break;
                }
            }
        }

        PrepaintState {
            paras,
            line_height,
            selection,
            cursor: cursor_quad,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        for quad in prepaint.selection.drain(..) {
            window.paint_quad(quad);
        }
        for para in &prepaint.paras {
            para.line
                .paint(
                    GpPoint::new(
                        bounds.origin.x - self.input.read(cx).scroll_x,
                        bounds.origin.y + para.y,
                    ),
                    prepaint.line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                )
                .ok();
        }
        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        let paras = prepaint.paras.clone();
        let line_height = prepaint.line_height;
        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(Layout { paras, line_height });
            input.last_bounds = Some(bounds);
        });
    }
}

fn rows_of(para: &Para) -> Vec<(usize, usize)> {
    let bounds = para
        .line
        .wrap_boundaries()
        .iter()
        .map(|b| {
            let run = &para.line.runs()[b.run_ix];
            run.glyphs[b.glyph_ix].index
        })
        .collect::<Vec<_>>();
    let mut rows = Vec::with_capacity(bounds.len() + 1);
    let mut start = 0usize;
    for end in bounds {
        rows.push((start, end));
        start = end;
    }
    rows.push((start, para.len));
    rows
}

impl Render for Input {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Focus bookkeeping for the blink. Programmatic focus (autofocus,
        // parent-driven) never passes a mouse-down, so the loop starts
        // here; a blur bumps the epoch, which stops the loop within one
        // tick (it notified to get here). Unfocused fields then rest with
        // no repaints at all.
        if self.focus_handle.is_focused(window) {
            if self.blink_epoch == 0 {
                self.restart_blink(cx);
            }
        } else if self.blink_epoch != 0 {
            self.blink_epoch += 1;
            self.caret_on = true;
        }
        div()
            .flex()
            .key_context("Input")
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::line_start))
            .on_action(cx.listener(Self::line_end))
            .on_action(cx.listener(Self::select_line_start))
            .on_action(cx.listener(Self::select_line_end))
            .on_action(cx.listener(Self::doc_start))
            .on_action(cx.listener(Self::doc_end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .w_full()
            .child(InputElement { input: cx.entity() })
    }
}

impl Focusable for Input {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
