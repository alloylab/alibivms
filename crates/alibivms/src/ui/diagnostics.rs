//! Diagnostics for a camera or an NVR: what the app knows plus raw answers to a few probe
//! requests, as plain text to copy into a bug report. Never includes passwords.

use std::{rc::Rc, sync::Arc};

use adw::prelude::*;
use gtk::gdk;
use serde_json::{Value, json};

use crate::{
    app::{App, Cam, spawn_bg},
    sites::Site,
};

/// A Star4Live probe: API path, request body, and which part of the answer to show.
type CloudProbe = (&'static str, Value, Box<dyn Fn(&Value) -> Value + Send>);

/// Runs each LAPI `GET` (and, for cloud sites, each Star4Live call) and formats the answers.
async fn probe(site: Arc<Site>, lapi: Vec<String>, cloud: Vec<CloudProbe>) -> String {
    let mut out = String::new();
    for uri in lapi {
        let r = site.client.raw("GET", &uri).await;
        out.push_str(&format!("\nGET {uri}\n{}\n", show(r.map_err(|e| e.to_string()))));
    }
    if let Some((account, _)) = site.client.cloud_link() {
        for (path, body, pick) in cloud {
            let r = account.call_raw(path, &body).await.map(|v| pick(&v));
            out.push_str(&format!("\nStar4Live {path}\n{}\n", show(r.map_err(|e| e.to_string()))));
        }
    }
    out
}

fn show(r: Result<Value, String>) -> String {
    match r {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_default(),
        Err(e) => format!("error: {e}"),
    }
}

pub fn camera(app: &Rc<App>, cam: &Cam) {
    let Some(site) = app.sites.get(&cam.site) else { return app.toast("Site not connected") };
    let ch = site.channel(cam.channel);
    let info = site.camera(cam.channel);
    let (ptz, why) = app.ptz(cam);
    let mut text = format!(
        "Camera: {}\nSite: {} ({}) · {}\nChannel: {}\nNVR channel name: {}\nOnline: {}\nCamera type reported: {}\nCamera IP: {}\nCamera model: {}\nCamera firmware: {}\nCamera serial: {}\nPTZ controls: {} ({why})\n",
        app.cam_label(cam),
        site.cfg.name,
        site.cfg.id,
        site.cfg.location(),
        cam.channel,
        ch.as_ref().map(|c| c.name.as_str()).unwrap_or("?"),
        ch.as_ref().map(|c| c.online.to_string()).unwrap_or("?".into()),
        ch.as_ref().map(|c| format!("{:?}", c.camera_type)).unwrap_or("?".into()),
        ch.as_ref().map(|c| c.ip.as_str()).filter(|s| !s.is_empty()).unwrap_or("?"),
        info.as_ref().map(|i| i.device_model.as_str()).unwrap_or("?"),
        info.as_ref().map(|i| i.firmware_version.as_str()).unwrap_or("?"),
        info.as_ref().map(|i| i.serial_number.as_str()).unwrap_or("?"),
        if ptz { "shown" } else { "hidden" },
    );
    text.push_str(&format!("Thumbnail: {}\n", app.thumbnails.describe(cam)));
    text.push_str("\n--- Probes ---\n");
    let n = cam.channel;
    let lapi = vec![
        format!("/LAPI/V1.0/Channels/{n}/Media/Video/Streams/0/LiveStreamURL?TransProtocol=tcp"),
        format!("/LAPI/V1.0/Channels/{n}/Media/Video/Streams/1/LiveStreamURL?TransProtocol=tcp"),
        format!("/LAPI/V1.0/Channels/{n}/Media/Video/Streams"),
        format!("/LAPI/V1.0/Channels/{n}/PTZ/Capabilities"),
        format!("/LAPI/V1.0/Channels/{n}/PTZ/Presets"),
    ];
    let serial = site.cfg.cloud.as_ref().map(|c| c.serial.clone()).unwrap_or_default();
    let cloud: Vec<CloudProbe> = vec![(
        "/openapi/inner/device/channel/list",
        json!({ "deviceSerial": serial, "queryType": 0, "pageStart": 0, "pageSize": 100 }),
        Box::new(move |v: &Value| {
            v["channelList"].as_array().and_then(|l| l.iter().find(|c| c["channelNo"].as_u64() == Some(n as u64)).cloned()).unwrap_or(Value::Null)
        }),
    )];

    // Detection override.
    let choice = gtk::DropDown::from_strings(&["Detect automatically", "PTZ camera", "Not PTZ"]);
    let set = app.cfg.borrow().ptz.get(&cam.site).and_then(|m| m.get(&cam.channel)).copied();
    choice.set_selected(match set {
        None => 0,
        Some(true) => 1,
        Some(false) => 2,
    });
    let row = adw::ActionRow::builder().title("PTZ controls").subtitle("Set by hand when detection gets it wrong").build();
    choice.set_valign(gtk::Align::Center);
    row.add_suffix(&choice);
    let (a, c) = (app.clone(), cam.clone());
    choice.connect_selected_notify(move |d| {
        a.set_ptz(&c, [None, Some(true), Some(false)][d.selected() as usize]);
    });

    present(app, &format!("Diagnostics · {}", app.cam_label(cam)), text, Some(row.upcast()), probe(site, lapi, cloud));
}

pub fn site(app: &Rc<App>, site_id: &str) {
    let Some(site) = app.sites.get(site_id) else { return app.toast("Site not connected") };
    let st = site.status();
    let mut text = format!(
        "Site: {} ({})\nReached via: {}\nOnline: {}\nError: {}\nTime zone: {}\n",
        site.cfg.name,
        site.cfg.id,
        site.cfg.location(),
        st.online,
        st.error.as_deref().unwrap_or("none"),
        st.time_zone.as_deref().unwrap_or("?"),
    );
    if let Some(d) = &st.device {
        text.push_str(&format!("NVR: {} · model {} · firmware {} · serial {}\n", d.device_name, d.device_model, d.firmware_version, d.serial_number));
    }
    text.push_str("\nCh  Online  Type      PTZ  Model                 Firmware                 IP               Name\n");
    for c in site.channels() {
        let info = site.camera(c.id);
        let cam = Cam { site: site.cfg.id.clone(), channel: c.id };
        text.push_str(&format!(
            "{:<3} {:<7} {:<9} {:<4} {:<21} {:<24} {:<16} {}\n",
            c.id,
            c.online,
            format!("{:?}", c.camera_type),
            if app.ptz(&cam).0 { "yes" } else { "no" },
            info.as_ref().map(|i| i.device_model.as_str()).unwrap_or("?"),
            info.as_ref().map(|i| i.firmware_version.as_str()).unwrap_or("?"),
            c.ip,
            c.name
        ));
    }
    text.push_str("\n--- Probes ---\n");
    let lapi = vec!["/LAPI/V1.0/System/DeviceInfo".to_string(), "/LAPI/V1.0/Channels/System/DeviceInfos".to_string(), "/LAPI/V1.0/Network/WebSocketServer/Capabilities".to_string()];
    let serial = site.cfg.cloud.as_ref().map(|c| c.serial.clone()).unwrap_or_default();
    let cloud: Vec<CloudProbe> = vec![(
        "/openapi/device/list",
        json!({ "pageNo": 1, "pageSize": 50 }),
        Box::new(move |v: &Value| v["deviceList"].as_array().and_then(|l| l.iter().find(|d| d["deviceSerial"].as_str() == Some(&serial)).cloned()).unwrap_or(Value::Null)),
    )];
    present(app, &format!("Diagnostics · {}", site.cfg.name), text, None, probe(site, lapi, cloud));
}

fn present(app: &Rc<App>, title: &str, text: String, extra: Option<gtk::Widget>, probes: impl std::future::Future<Output = String> + Send + 'static) {
    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(&format!("{text}(running…)\n"));
    let view = gtk::TextView::builder().buffer(&buffer).editable(false).monospace(true).wrap_mode(gtk::WrapMode::WordChar).top_margin(8).bottom_margin(8).left_margin(8).right_margin(8).build();
    let scroll = gtk::ScrolledWindow::builder().child(&view).vexpand(true).min_content_height(360).css_classes(["card"]).build();
    let copy = gtk::Button::builder().label("Copy").css_classes(["suggested-action"]).halign(gtk::Align::End).build();
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_top(12).margin_bottom(18).margin_start(18).margin_end(18).build();
    if let Some(w) = extra {
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
        list.append(&w);
        body.append(&list);
    }
    body.append(&scroll);
    let note = gtk::Label::builder()
        .label("The text names the Star4Live account's e-mail, the NVR's and cameras' serial numbers and the cameras' IP addresses, but never a password.")
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label", "caption"])
        .build();
    body.append(&note);
    body.append(&copy);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&body));
    let dialog = adw::Dialog::builder().title(title).content_width(760).content_height(620).child(&toolbar).build();
    let (b, a) = (buffer.clone(), app.clone());
    copy.connect_clicked(move |_| {
        let t = b.text(&b.start_iter(), &b.end_iter(), false);
        if let Some(d) = gdk::Display::default() {
            d.clipboard().set_text(&t);
        }
        a.toast("Diagnostics copied");
    });
    spawn_bg(probes, move |more| buffer.set_text(&format!("{text}{more}")));
    dialog.present(Some(&app.window));
}
