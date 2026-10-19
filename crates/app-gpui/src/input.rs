//! A minimal multi-line text input, GPUI-native.
//!
//! TODO(port): crates/app used AdwTextView; gpui 0.2.2 ships no editor, so this
//! adapts the gpui `input.rs` example (IME path, actions, mouse selection) and
//! extends it to wrapped multi-line: one shaped block per paragraph, cursor and
//! selection mapped through WrappedLineLayout::position_for_index. P2 scope —
//! arrow-up/down and scroll-into-view land when a view needs them.

use std::ops::Range;

use gpui::{
    actions, div, fill, hsla, prelude::*, px, relative, rgb, size, App, Bounds, ClipboardItem,
    Context, CursorStyle, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    FocusHandle, Focusable, GlobalElementId, KeyBinding, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point as GpPoint, Render,
    SharedString, Style, TextAlign, TextRun, UTF16Selection, Window, WrappedLine,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme;

actions!(
    input,
    [Backspace, Delete, Left, Right, SelectLeft, SelectRight, SelectAll, Home, End, Paste, Copy, Cut]
);

pub fn bind_input_keys(cx: &mut App) {
    // Scoped to the "Input" key context so these never hijack global keys.
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("Input")),
        KeyBinding::new("delete", Delete, Some("Input")),
        KeyBinding::new("left", Left, Some("Input")),
        KeyBinding::new("right", Right, Some("Input")),
        KeyBinding::new("shift-left", SelectLeft, Some("Input")),
        KeyBinding::new("shift-right", SelectRight, Some("Input")),
        KeyBinding::new("ctrl-a", SelectAll, Some("Input")),
        KeyBinding::new("cmd-a", SelectAll, Some("Input")),
        KeyBinding::new("ctrl-v", Paste, Some("Input")),
        KeyBinding::new("cmd-v", Paste, Some("Input")),
        KeyBinding::new("ctrl-c", Copy, Some("Input")),
        KeyBinding::new("cmd-c", Copy, Some("Input")),
        KeyBinding::new("ctrl-x", Cut, Some("Input")),
        KeyBinding::new("cmd-x", Cut, Some("Input")),
        KeyBinding::new("home", Home, Some("Input")),
        KeyBinding::new("end", End, Some("Input")),
    ]);
}

/// One shaped paragraph ('\n'-separated) plus its byte offset in the content.
#[derive(Clone)]
struct Para {
    line: WrappedLine,
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
}

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
        }
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
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.selected_range.end), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
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
            let text = if self.multiline { text } else { text.replace('\n', " ") };
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

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // IME composition in progress — let it commit on its own.
        if self.marked_range.is_some() {
            return;
        }
        if ev.keystroke.key == "enter" && self.multiline && !ev.keystroke.modifiers.platform {
            self.insert("\n", cx);
        }
    }

    fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        let range = self.selected_range.clone();
        self.content = self.content[0..range.start].to_string() + text + &self.content[range.end..];
        self.selected_range = range.start + text.len()..range.start + text.len();
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

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
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
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
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
            let is_last = para.start + para.len >= self.content.len()
                && !self.content.ends_with('\n');
            if local_y <= para_bottom || is_last {
                let local = GpPoint::new(
                    position.x - bounds.left() + scroll_x,
                    local_y - para.y,
                );
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

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
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
        // Single-line mode strips newlines (IME included).
        let owned;
        let new_text = if self.multiline {
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
        style.size.height = relative(1.).into();
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
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let multiline = input.multiline;
        let mut scroll_x = input.scroll_x;
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();

        let empty = content.is_empty();
        let text_color = if empty {
            hsla(0., 0., 0.5, 0.35) // placeholder wash
        } else {
            style.color
        };
        let display_text: SharedString = if empty {
            input.placeholder.clone()
        } else {
            content.clone().into()
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
                    if multiline { Some(bounds.size.width) } else { None },
                    None,
                )
                .ok()
                .and_then(|mut lines| lines.pop());
            if let Some(line) = shaped {
                paras.push(Para { line, start: byte_start, len: seg.len(), y });
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
                            GpPoint::new(
                                bounds.left() - scroll_x + p1.x,
                                bounds.top() + row_y,
                            ),
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

        // Cursor quad.
        let mut cursor_quad = None;
        if !empty {
            for para in &paras {
                if cursor >= para.start && cursor <= para.start + para.len {
                    let lh = line_height;
                    if let Some(pos) =
                        para.line.position_for_index(cursor - para.start, lh)
                    {
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

        PrepaintState { paras, line_height, selection, cursor: cursor_quad }
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .key_context("Input")
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
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
