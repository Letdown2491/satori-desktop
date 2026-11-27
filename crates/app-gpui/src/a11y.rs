//! Screen-reader exposure (AccessKit, via gpui-pre's a11y tree).
//!
//! gpui reports an element to assistive technology only when it has an
//! id AND a role; nothing in the app set one, so Orca saw an empty
//! window (review, Nov 27 2026). These helpers are the one vocabulary
//! the shared controls wear. An element with a click handler already
//! answers AT-SPI's Click action (gpui wires it), so a role plus a name
//! is all a control needs to be operable from a screen reader.

use gpui::{Role, SharedString, StatefulInteractiveElement, Toggled};

pub trait A11y: StatefulInteractiveElement + Sized {
    /// A pressable control named `label` (icon-only controls especially:
    /// the glyph says nothing to a screen reader).
    fn a11y_button(self, label: impl Into<SharedString>) -> Self {
        self.role(Role::Button).aria_label(label)
    }

    /// A two-state control (like, bookmark, the columns switch): the
    /// state is announced with the name.
    fn a11y_toggle(self, label: impl Into<SharedString>, on: bool) -> Self {
        self.role(Role::Button)
            .aria_label(label)
            .aria_toggled(if on { Toggled::True } else { Toggled::False })
    }

    /// A navigation entry (the rail): selected when it is the current
    /// page.
    fn a11y_tab(self, label: impl Into<SharedString>, selected: bool) -> Self {
        self.role(Role::Tab)
            .aria_label(label)
            .aria_selected(selected)
    }
}

impl<E: StatefulInteractiveElement + Sized> A11y for E {}
