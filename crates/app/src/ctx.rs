//! App context — the handle every view/widget gets. The main area is a
//! Stack of sections (feed / notifications / search), each with its own
//! NavigationView: sidebar switches sections; threads and profiles push
//! onto the section you're in.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use satori_core::manager::Core;

#[derive(Clone)]
pub struct Ctx {
    pub core: Arc<Core>,
    pub overlay: adw::ToastOverlay,
    sections: Rc<RefCell<Option<gtk::Stack>>>,
    navs: Rc<RefCell<HashMap<&'static str, adw::NavigationView>>>,
    side_rows: Rc<RefCell<HashMap<&'static str, gtk::Button>>>,
    active: Rc<Cell<&'static str>>,
    /// Per-section hooks fired on switch (the notifications page fetches
    /// when SHOWN, not at construction — it's built before any session).
    pub show_hooks: Rc<RefCell<HashMap<&'static str, Rc<dyn Fn()>>>>,
}

impl Ctx {
    pub fn new(core: Arc<Core>, overlay: adw::ToastOverlay) -> Self {
        Self {
            core,
            overlay,
            sections: Rc::new(RefCell::new(None)),
            navs: Rc::new(RefCell::new(HashMap::new())),
            side_rows: Rc::new(RefCell::new(HashMap::new())),
            active: Rc::new(Cell::new("feed")),
            show_hooks: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    /// Wire the section machinery after the shell builds the parts.
    pub fn attach(
        &self,
        sections: &gtk::Stack,
        navs: HashMap<&'static str, adw::NavigationView>,
        side_rows: HashMap<&'static str, gtk::Button>,
    ) {
        *self.sections.borrow_mut() = Some(sections.clone());
        *self.navs.borrow_mut() = navs;
        *self.side_rows.borrow_mut() = side_rows;
    }

    /// The nav view of the section you're in (threads push here).
    pub fn nav(&self) -> adw::NavigationView {
        self.navs
            .borrow()
            .get(self.active.get())
            .cloned()
            .expect("section nav")
    }

    /// Switch sections (+ sidebar active styling).
    pub fn show(&self, section: &'static str) {
        self.active.set(section);
        if let Some(stack) = self.sections.borrow().as_ref() {
            stack.set_visible_child_name(section);
        }
        if let Some(hook) = self.show_hooks.borrow().get(section) {
            hook();
        }
        for (name, btn) in self.side_rows.borrow().iter() {
            if *name == section {
                btn.add_css_class("side-active");
            } else {
                btn.remove_css_class("side-active");
            }
        }
    }

    pub fn toast(&self, title: &str) {
        let t = adw::Toast::new(title);
        self.overlay.add_toast(t);
    }
}
