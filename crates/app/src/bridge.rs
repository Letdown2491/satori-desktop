//! Small glue for draining core channels on the GLib main context.

use gtk::glib;
use tokio::sync::mpsc::UnboundedReceiver;

/// Await core events on the GLib main context (DESIGN.md §3.2 bridge).
pub fn drain<E: 'static>(mut rx: UnboundedReceiver<E>, f: impl Fn(E) + 'static) {
    glib::MainContext::default().spawn_local(async move {
        while let Some(ev) = rx.recv().await {
            f(ev);
        }
    });
}
