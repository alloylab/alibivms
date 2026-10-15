// Release builds on Windows are GUI apps: no console window behind the main one.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod audit;
mod bundle;
mod cloud_media;
mod config;
mod download;
mod motion;
mod rtsp_proxy;
mod sites;
mod thumbnails;
mod ui;
mod update;
mod video;

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use adw::prelude::*;
use tracing_subscriber::EnvFilter;

fn main() -> gtk::glib::ExitCode {
    bundle::setup();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    quiet_harmless_gtk_warnings();
    if let Err(e) = gst::init() {
        eprintln!("GStreamer failed to start: {e}");
        return gtk::glib::ExitCode::FAILURE;
    }
    if let Err(e) = gstgtk4::plugin_register_static() {
        eprintln!("could not register the GTK video sink: {e}");
        return gtk::glib::ExitCode::FAILURE;
    }
    video::tune_decoders();
    app::rt();

    let application = adw::Application::builder().application_id(ui::window::APP_ID).build();
    let holder: Rc<RefCell<Option<Rc<app::App>>>> = Default::default();
    let failed = Rc::new(Cell::new(false));
    let f = failed.clone();
    application.connect_activate(move |application| {
        if let Some(a) = holder.borrow().as_ref() {
            a.window.present();
            return;
        }
        match ui::window::build(application) {
            Ok(a) => *holder.borrow_mut() = Some(a),
            Err(e) => {
                eprintln!("startup failed: {e:#}");
                f.set(true);
                application.quit();
            }
        }
    });
    let code = application.run();
    if failed.get() { gtk::glib::ExitCode::FAILURE } else { code }
}

/// GTK's Vulkan renderer warns on every resize that the swapchain is "suboptimal"
/// (`VK_SUBOPTIMAL_KHR`); it still presents correctly, so that one message is dropped.
fn quiet_harmless_gtk_warnings() {
    use gtk::glib;
    glib::log_set_writer_func(|level, fields| {
        let harmless = fields.iter().any(|f| f.key() == "MESSAGE" && f.value_str().is_some_and(|m| m.contains("VK_SUBOPTIMAL_KHR")));
        if harmless {
            return glib::LogWriterOutput::Handled;
        }
        glib::log_writer_default(level, fields)
    });
}
