use adw::prelude::*;
use gtk::glib;


fn main() {
    gtk::init().unwrap();
    adw::init().ok();
    {
        satori_lib::theme::load_current();
        satori_lib::icons::install();
        let core = std::sync::Arc::new(satori_core::manager::Core::new());
        let overlay = adw::ToastOverlay::new();
        let ctx = satori_lib::ctx::Ctx::new(core, overlay.clone());

        let feed = satori_lib::feed::FeedView::new(&ctx);
        let (shell_w, _handles) = satori_lib::shell::build_main(&ctx, std::rc::Rc::clone(&feed));

        let win = adw::Window::builder()
            .title("probe")
            .default_width(1100)
            .default_height(760)
            .content(&shell_w)
            .build();
        win.present();
        eprintln!("[probe] window presented");

        glib::timeout_add_local(std::time::Duration::from_millis(600), move || {
            // Walk the tree; print Boxes wider than 260px.
            fn walk(w: &gtk::Widget, depth: usize, out: &mut Vec<(String, i32, i32, Vec<String>)>) {
                let name = w.css_classes().iter().map(|s| s.as_str()).collect::<Vec<_>>().join(".");
                let is_box = w.downcast_ref::<gtk::Box>().is_some();
                let (min_w, nat_w, _, _) = w.measure(gtk::Orientation::Horizontal, -1);
                if is_box && (nat_w > 260 || min_w > 260) {
                    out.push((w.type_().name().to_string(), min_w, nat_w, vec![name]));
                }
                if let Some(b) = w.downcast_ref::<gtk::Box>() {
                    let mut c = b.first_child();
                    while let Some(k) = c {
                        walk(&k, depth + 1, out);
                        c = k.next_sibling();
                    }
                } else if let Some(sw) = w.downcast_ref::<gtk::ScrolledWindow>() {
                    if let Some(k) = sw.child() { walk(&k, depth + 1, out); }
                } else if let Some(o) = w.downcast_ref::<gtk::Overlay>() {
                    if let Some(k) = o.child() { walk(&k, depth + 1, out); }
                } else if let Some(v) = w.downcast_ref::<gtk::Stack>() {
                    if let Some(k) = v.visible_child() { walk(&k, depth + 1, out); }
                } else if let Some(nv) = w.downcast_ref::<adw::NavigationView>() {
                    // visible_page is the accessor on NavigationView.
                    if let Some(k) = nv.visible_page() { walk(k.upcast_ref(), depth + 1, out); }
                } else if let Some(t) = w.downcast_ref::<adw::ToolbarView>() {
                    if let Some(k) = t.content() { walk(&k, depth + 1, out); }
                } else if let Some(cl) = w.downcast_ref::<adw::Clamp>() {
                    if let Some(k) = cl.child() { walk(&k, depth + 1, out); }
                }
            }
            eprintln!("[probe] walking...");
            let mut out = Vec::new();
            walk(win.upcast_ref(), 0, &mut out);
            eprintln!("[probe] found {} wide boxes", out.len());
            for (t, min_w, nat_w, cls) in out {
                println!("WIDE {t} min={min_w} nat={nat_w} classes={:?}", cls);
            }
            println!("window: {}x{}", win.width(), win.height());
            std::process::exit(0);
        });
        eprintln!("[probe] timeout attached");
    }
    eprintln!("[probe] running main loop");
    let loop_ = glib::MainLoop::new(None, false);
    {
        let loop2 = loop_.clone();
        glib::timeout_add_seconds_local(3, move || {
            loop2.quit();
            glib::ControlFlow::Break
        });
    }
    loop_.run();
    eprintln!("[probe] main loop ended");
}
