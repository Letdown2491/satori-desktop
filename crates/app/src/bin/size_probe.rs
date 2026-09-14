use adw::prelude::*;

fn main() {
    let app = adw::Application::builder()
        .application_id("io.github.Letdown2491.Satori.probe")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(|_app| {
        gtk::Window::set_default_icon_name("satori-logo");
        let core = std::sync::Arc::new(satori_core::manager::Core::new());
        let overlay = adw::ToastOverlay::new();
        let ctx = satori::ctx::Ctx::new(core, overlay.clone());

        // EXACTLY what the sidebar account row builds:
        let placeholder_pk =
            nostr::prelude::PublicKey::from_hex(&"0".repeat(64)).unwrap();
        let avatar = satori::note_card::avatar_widget(&ctx, placeholder_pk, None);
        avatar.set_width_request(28);
        avatar.set_height_request(28);

        let label = gtk::Label::builder()
            .label("npub1000000")
            .halign(gtk::Align::Start)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .build();
        inner.append(&avatar);
        inner.append(&label);

        let win = gtk::Window::builder().default_width(300).build();
        win.set_child(Some(&inner));
        win.present();

        // Measure after allocation.
        glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
            for (name, w) in [("avatar-stack", avatar.upcast_ref()), ("inner", inner.upcast_ref())] {
                let nat_w = w.natural_size(); // convenience? use measure
                let _ = nat_w;
                let (min_w, _nw) = w.measure(gtk::Orientation::Horizontal, -1);
                let (min_h, _nh) = w.measure(gtk::Orientation::Vertical, -1);
                println!("{name}: allocated {}x{}, min {min_w}x{min_h}",
                    w.width(), w.height());
            }
            std::process::exit(0);
        });
    });
    app.run();
}
