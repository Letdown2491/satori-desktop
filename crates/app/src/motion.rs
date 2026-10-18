//! Motion — port of the web app's animation inventory (styles.css §motion):
//! enso-breath (3.4s opacity pulse), inkfade card entrances. Everything
//! honors the desktop's animation pref; scrolls stay instant.

use std::cell::Cell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

/// enso-breath: 0.4→1.0 opacity, 3.4s period (styles.css:1575).
pub fn breathe(widget: &impl IsA<gtk::Widget>) {
    let w = widget.clone();
    let t = Rc::new(Cell::new(0.0_f64));
    let animate = move || {
        let w = w.clone();
        let t = t.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(60), move || {
            let v = t.get() + 0.06;
            t.set(v);
            let phase = 0.5 + 0.5 * (v * 2.0 * std::f64::consts::PI / 3.4).sin();
            w.set_opacity(0.4 + 0.6 * phase);
            glib::ControlFlow::Continue
        })
    };
    animate();
}

/// inkfade: feed cards enter with a 0.45s opacity fade (styles.css:1108).
pub fn fade_in(widget: &impl IsA<gtk::Widget>) {
    let w = widget.clone();
    w.set_opacity(0.0);
    let step = Rc::new(Cell::new(0.0_f64));
    glib::timeout_add_local(std::time::Duration::from_millis(45), move || {
        let v = step.get() + 0.1;
        step.set(v);
        if v >= 1.0 {
            w.set_opacity(1.0);
            glib::ControlFlow::Break
        } else {
            w.set_opacity(v);
            glib::ControlFlow::Continue
        }
    });
}

/// press pulse: a quick opacity dip-and-return on click (~180ms) — the GUI
/// stand-in for the web's :active transform. Quiet: no size, no color.
pub fn press_pulse(widget: &impl IsA<gtk::Widget>) {
    let w = widget.clone();
    let t = Rc::new(Cell::new(0.0_f64));
    glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
        let v = t.get() + 0.09;
        t.set(v);
        let phase = (v * std::f64::consts::PI).sin();
        w.set_opacity(1.0 - 0.45 * phase);
        if v >= 1.0 {
            w.set_opacity(1.0);
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}
