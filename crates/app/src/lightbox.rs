//! The lightbox — port of content.ts's lightbox (the web did it with
//! `:target` CSS): a dark full-screen viewer, fit-to-screen, prev/next,
//! Esc/arrow keys. Images come from the media cache — tiles and full view
//! share one URL, so opening is instant.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::bridge::drain;
use crate::ctx::Ctx;

pub fn open(ctx: &Ctx, urls: Vec<String>, start: usize) {
    if urls.is_empty() {
        return;
    }
    let win = gtk::Window::builder()
        .title("Media")
        .modal(true)
        .default_width(1100)
        .default_height(820)
        .css_classes(["lightbox"])
        .build();
    if let Some(parent) = ctx
        .nav()
        .root()
        .map(|r| r.upcast::<gtk::Widget>())
        .and_downcast::<adw::ApplicationWindow>()
    {
        win.set_transient_for(Some(&parent));
        win.set_application(parent.application().as_ref());
    }

    let overlay = gtk::Overlay::builder().build();
    let pic = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .hexpand(true)
        .vexpand(true)
        .build();
    overlay.set_child(Some(&pic));

    let idx = Rc::new(Cell::new(start.min(urls.len() - 1)));

    // Nav arrows (hidden when a single image).
    let nav_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::End)
        .spacing(14)
        .margin_bottom(20)
        .visible(urls.len() > 1)
        .build();
    let counter = gtk::Label::builder()
        .css_classes(["lightbox-counter"])
        .valign(gtk::Align::Center)
        .build();
    nav_box.append(&counter);
    overlay.add_overlay(&nav_box);

    let ctx2 = ctx.clone();
    let load = {
        let pic = pic.clone();
        let urls = urls.clone();
        let idx = idx.clone();
        let counter = counter.clone();
        let ctx2 = ctx2.clone();
        move |i: usize| {
            idx.set(i.min(urls.len() - 1));
            let u = &urls[idx.get()];
            counter.set_text(&format!("{} / {}", idx.get() + 1, urls.len()));
            if let Some(tex) = crate::media_cache::mem_get(u) {
                pic.set_paintable(Some(&tex));
                return;
            }
            let pic2 = pic.clone();
            let u2 = u.clone();
            drain(ctx2.core.fetch_image(u2.clone()), move |img| {
                if let Some(decoded) = img {
                    let tex = crate::media_cache::from_decoded(&u2, &decoded);
                    pic2.set_paintable(Some(&tex));
                }
            });
        }
    };

    // Click zones: left third = prev, right third = next, middle = close.
    let click = gtk::GestureClick::new();
    {
        let win = win.clone();
        let load = load.clone();
        let urls_len = urls.len();
        let idx2 = idx.clone();
        let width = pic.clone();
        click.set_button(1);
        click.connect_released(move |g, _n, x, _y| {
            let _ = g;
            if urls_len == 1 {
                win.close();
                return;
            }
            let w = width.width() as f64;
            let (next, closed) = if x < w * 0.33 {
                (Some((idx2.get() + urls_len - 1) % urls_len), false)
            } else if x > w * 0.67 {
                (Some((idx2.get() + 1) % urls_len), false)
            } else {
                (None, true)
            };
            if closed {
                win.close();
            } else if let Some(n) = next {
                load(n);
            }
        });
    }
    pic.add_controller(click);

    // Keyboard: arrows + Escape.
    {
        let win_recv = win.clone();
        let load = load.clone();
        let urls_len = urls.len();
        let idx2 = idx.clone();
        let key = gtk::EventControllerKey::new();
        key.connect_key_pressed(move |_, keyval, _code, _mods| {
            match keyval {
                gtk::gdk::Key::Escape => {
                    win_recv.close();
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Right | gtk::gdk::Key::Left if urls_len > 1 => {
                    let cur = idx2.get();
                    let next = if keyval == gtk::gdk::Key::Right {
                        (cur + 1) % urls_len
                    } else {
                        (cur + urls_len - 1) % urls_len
                    };
                    load(next);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        win.add_controller(key);
    }

    // Close button.
    let close = gtk::Button::builder()
        .child(&crate::icons::icon("close"))
        .css_classes(["flat", "lightbox-close"])
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .margin_top(16)
        .margin_end(16)
        .build();
    {
        let win = win.clone();
        close.connect_clicked(move |_| win.close());
    }
    overlay.add_overlay(&close);

    // Dark backdrop.
    overlay.add_css_class("lightbox-overlay");
    win.set_child(Some(&overlay));

    load(idx.get());
    win.present();
}
