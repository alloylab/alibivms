//! Builds the main window: header with view switcher, camera sidebar, and the three views.

use std::{cell::RefCell, rc::Rc, time::Duration};

use adw::prelude::*;
use gtk::glib;

use crate::{
    app::{App, rt, spawn_bg},
    audit::Audit,
    config::{self, Config},
    download::Downloads,
    sites::Sites,
    thumbnails::Thumbnails,
    ui::{alerts::Alerts, downloads::DownloadsView, live::LiveView, playback::PlaybackView, sidebar::Sidebar, sites_dialog},
};

pub const APP_ID: &str = "io.github.alloylab.alibivms";

pub const DISCLAIMER: &str = "Alibi VMS is an independent project. It is not made, endorsed or supported by Alibi Security, Uniview or Star4Live, and is not affiliated with them. Alibi, Star4Live and Uniview are trademarks of their respective owners, named only to describe compatibility.\n\nThis software is provided as is, without warranty of any kind. Do not rely on it as your only means of security monitoring or evidence capture.";

/// Makes the app icon available by name: written once under the data directory, which is then
/// added to the icon theme's search path.
fn install_icon(display: &gtk::gdk::Display) {
    let root = config::data_dir().join("icons");
    let path = root.join("hicolor/scalable/apps").join(format!("{APP_ID}.svg"));
    let svg = include_bytes!("../../data/io.github.alloylab.alibivms.svg");
    if std::fs::read(&path).ok().as_deref() != Some(&svg[..]) {
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        if let Err(e) = std::fs::write(&path, svg) {
            tracing::warn!(error = %e, "could not install the app icon");
        }
    }
    gtk::IconTheme::for_display(display).add_search_path(&root);
    gtk::Window::set_default_icon_name(APP_ID);
}

pub fn build(application: &adw::Application) -> anyhow::Result<Rc<App>> {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    let display = gtk::gdk::Display::default().unwrap();
    gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    install_icon(&display);

    let cfg = Config::load();
    let sites = Sites::default();
    {
        let _rt = rt().enter();
        sites.sync(&cfg.sites, &cfg.cloud_accounts);
    }
    let downloads = {
        let _rt = rt().enter();
        Downloads::new(config::data_dir().join("tmp"), crate::download::ffmpeg_path())?
    };

    let stack = adw::ViewStack::new();
    let switcher = adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build();
    let header = adw::HeaderBar::builder().title_widget(&switcher).build();
    let toggle = gtk::ToggleButton::builder().icon_name("sidebar-show-symbolic").tooltip_text("Cameras").active(true).build();
    header.pack_start(&toggle);
    let menu = gtk::gio::Menu::new();
    menu.append(Some("Sites and settings…"), Some("app.sites"));
    menu.append(Some("About Alibi VMS"), Some("app.about"));
    header.pack_end(&gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).build());
    let fullscreen = gtk::Button::builder().icon_name("view-fullscreen-symbolic").tooltip_text("Full screen (F11)").action_name("win.fullscreen").build();
    header.pack_end(&fullscreen);
    let reload = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Reconnect all cameras (F5)").action_name("win.reload").build();
    header.pack_end(&reload);
    let update = gtk::Button::builder().icon_name("software-update-available-symbolic").action_name("app.update").css_classes(["accent"]).visible(false).build();
    header.pack_end(&update);

    let split = adw::OverlaySplitView::builder().content(&stack).min_sidebar_width(240.0).max_sidebar_width(320.0).build();
    toggle.bind_property("active", &split, "show-sidebar").bidirectional().sync_create().build();
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&split));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));

    let window = adw::ApplicationWindow::builder().application(application).title("Alibi VMS").default_width(1400).default_height(900).content(&toolbar).build();
    let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(adw::BreakpointConditionLengthType::MaxWidth, 800.0, adw::LengthUnit::Sp));
    breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(breakpoint);

    let thumbnails = Thumbnails::new(sites.clone(), cfg.thumbnails);
    let app = Rc::new(App {
        window,
        toasts,
        stack: stack.clone(),
        split: split.clone(),
        cfg: RefCell::new(cfg),
        sites,
        downloads,
        audit: Audit::new(config::data_dir().join("audit.log")),
        thumbnails,
        sidebar: RefCell::default(),
        live: RefCell::default(),
        playback: RefCell::default(),
        downloads_view: RefCell::default(),
        alerts: RefCell::default(),
    });

    let sidebar = Sidebar::new(&app);
    split.set_sidebar(Some(&sidebar.root));
    let live = LiveView::new(&app);
    let playback = PlaybackView::new(&app);
    let dl = DownloadsView::new(&app);
    stack.add_titled_with_icon(&live.root, Some("live"), "Live", "camera-video-symbolic");
    stack.add_titled_with_icon(&playback.root, Some("playback"), "Playback", "media-playback-start-symbolic");
    let dl_page = stack.add_titled_with_icon(&dl.root, Some("downloads"), "Downloads", "folder-download-symbolic");
    *app.sidebar.borrow_mut() = Some(sidebar);
    *app.live.borrow_mut() = Some(live.clone());
    *app.playback.borrow_mut() = Some(playback.clone());
    *app.downloads_view.borrow_mut() = Some(dl.clone());
    live.build();
    let alerts = Alerts::new(&app, application);
    header.pack_end(&alerts.button);
    *app.alerts.borrow_mut() = Some(alerts);

    // Only the visible view streams video.
    let (a, r) = (app.clone(), reload.clone());
    stack.connect_visible_child_name_notify(move |s| {
        let v = s.visible_child_name().map(|n| n.to_string()).unwrap_or_default();
        r.set_visible(v != "downloads");
        if let Some(l) = a.live.borrow().clone() {
            l.set_active(v == "live");
        }
        if let Some(p) = a.playback.borrow().as_ref() {
            p.set_active(v == "playback");
        }
        if let Some(sb) = a.sidebar.borrow().as_ref() {
            sb.mark_selection();
        }
    });

    // Badge on the Downloads tab while clips are being pulled.
    let a = app.clone();
    glib::timeout_add_local(Duration::from_secs(1), move || {
        let n = a.downloads_view.borrow().as_ref().map(|d| d.active_count()).unwrap_or(0);
        dl_page.set_needs_attention(n > 0);
        dl_page.set_badge_number(n as u32);
        glib::ControlFlow::Continue
    });

    // Reconnect every camera in the open view at once, instead of each tile's Retry.
    let act = gtk::gio::SimpleAction::new("reload", None);
    let a = app.clone();
    act.connect_activate(move |_, _| match a.current_view().as_str() {
        "live" => {
            if let Some(l) = a.live.borrow().clone() {
                l.reload_all();
            }
        }
        "playback" => {
            if let Some(p) = a.playback.borrow().clone() {
                p.reload_all();
            }
        }
        _ => {}
    });
    app.window.add_action(&act);
    application.set_accels_for_action("win.reload", &["F5"]);

    // Full screen hides the header and camera list; F11 or Esc brings them back.
    let fs = gtk::gio::SimpleAction::new("fullscreen", None);
    let w = app.window.clone();
    fs.connect_activate(move |_, _| {
        if w.is_fullscreen() { w.unfullscreen() } else { w.fullscreen() }
    });
    app.window.add_action(&fs);
    application.set_accels_for_action("win.fullscreen", &["F11"]);
    let (tb, sp, tg, lv) = (toolbar.clone(), split.clone(), toggle.clone(), live.clone());
    app.window.connect_fullscreened_notify(move |w| {
        let full = w.is_fullscreen();
        tb.set_reveal_top_bars(!full);
        lv.set_fullscreen(full);
        if full {
            sp.set_show_sidebar(false);
        } else {
            sp.set_show_sidebar(tg.is_active());
        }
    });
    let keys = gtk::EventControllerKey::new();
    let a = app.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key != gtk::gdk::Key::Escape {
            return glib::Propagation::Proceed;
        }
        // Esc steps back: an enlarged camera to the grid first, then out of full screen.
        let live = a.live.borrow().clone();
        if a.current_view() == "live" && live.is_some_and(|l| l.restore_grid()) {
            return glib::Propagation::Stop;
        }
        if a.window.is_fullscreen() {
            a.window.unfullscreen();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    app.window.add_controller(keys);

    let a = app.clone();
    let act = gtk::gio::SimpleAction::new("sites", None);
    act.connect_activate(move |_, _| sites_dialog::open(&a));
    application.add_action(&act);
    watch_for_updates(&app, application, &update);

    let a = app.clone();
    let act = gtk::gio::SimpleAction::new("about", None);
    act.connect_activate(move |_, _| {
        let about = adw::AboutDialog::builder()
            .application_name("Alibi VMS")
            .application_icon(APP_ID)
            .developer_name("Alloy Lab, LLC")
            .version(env!("CARGO_PKG_VERSION"))
            .copyright("© 2026 Alloy Lab, LLC")
            .license_type(gtk::License::Custom)
            .license("Licensed under the Elastic License 2.0 (ELv2).")
            .website("https://github.com/alloylab/alibivms")
            .comments("Live view, playback and clip download for Alibi NVRs, on site or through Star4Live.")
            .build();
        about.add_legal_section("Disclaimer", None, gtk::License::Custom, Some(DISCLAIMER));
        about.present(Some(&a.window));
    });
    application.add_action(&act);

    let a = app.clone();
    app.window.connect_close_request(move |_| {
        let sites = a.sites.clone();
        let _ = rt().block_on(async move { tokio::time::timeout(Duration::from_secs(3), sites.logout_all()).await });
        glib::Propagation::Proceed
    });

    app.window.present();
    dev_screenshot(&app);
    Ok(app)
}

/// Development aid: `ALIBI_SCREENSHOT=out.png [ALIBI_SCREENSHOT_VIEW=playback]` renders the window
/// to a PNG after a few seconds and quits. Used to review the UI headlessly (GDK_BACKEND=broadway).
fn dev_screenshot(app: &Rc<App>) {
    let Ok(path) = std::env::var("ALIBI_SCREENSHOT") else { return };
    match std::env::var("ALIBI_SCREENSHOT_VIEW").as_deref() {
        Ok("about") => {
            if let Some(a) = app.window.application() {
                a.activate_action("about", None);
            }
        }
        Ok(view) => app.stack.set_visible_child_name(view),
        Err(_) => {}
    }
    let w = app.window.clone();
    glib::timeout_add_seconds_local_once(4, move || {
        let (width, height) = (w.width(), w.height());
        let paintable = gtk::WidgetPaintable::new(Some(&w));
        let snap = gtk::Snapshot::new();
        paintable.snapshot(&snap, width as f64, height as f64);
        if let (Some(node), Some(renderer)) = (snap.to_node(), w.renderer()) {
            let tex = renderer.render_texture(node, None);
            if let Err(e) = tex.save_to_png(&path) {
                eprintln!("screenshot failed: {e}");
            }
        }
        w.application().map(|a| a.quit());
    });
}

/// Checks for a newer release shortly after start and then daily, while the setting is on. When
/// one is out, shows `button` in the header bar and a toast once; both open the release page.
fn watch_for_updates(app: &Rc<App>, application: &adw::Application, button: &gtk::Button) {
    let url: Rc<RefCell<Option<String>>> = Rc::default();
    let act = gtk::gio::SimpleAction::new("update", None);
    let (a, u) = (app.clone(), url.clone());
    act.connect_activate(move |_, _| {
        if let Some(url) = u.borrow().as_deref() {
            gtk::UriLauncher::new(url).launch(Some(&a.window), gtk::gio::Cancellable::NONE, |_| {});
        }
    });
    application.add_action(&act);

    let (a, b) = (Rc::downgrade(app), button.clone());
    glib::spawn_future_local(async move {
        glib::timeout_future(Duration::from_secs(5)).await;
        loop {
            let Some(app) = a.upgrade() else { return };
            if app.cfg.borrow().check_updates && url.borrow().is_none() {
                let (app2, b, url) = (app.clone(), b.clone(), url.clone());
                spawn_bg(crate::update::check(), move |r| match r {
                    Ok(Some(rel)) => {
                        b.set_tooltip_text(Some(&format!("Alibi VMS {} is available. {}", rel.version, crate::update::how_to_update())));
                        b.set_visible(true);
                        *url.borrow_mut() = Some(rel.url);
                        let t = adw::Toast::builder().title(format!("Alibi VMS {} is available", rel.version)).button_label("View").action_name("app.update").timeout(10).build();
                        app2.toasts.add_toast(t);
                    }
                    Ok(None) => {}
                    Err(e) => tracing::debug!(error = %e, "update check failed"),
                });
            }
            drop(app);
            glib::timeout_future(Duration::from_secs(24 * 60 * 60)).await;
        }
    });
}
