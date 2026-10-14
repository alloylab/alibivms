//! Local camera names: the rename dialog and applying a new name everywhere it is shown.

use std::rc::Rc;

use adw::prelude::*;

use crate::{
    app::{App, Cam},
    audit::Entry,
};

/// Asks for a local name for `cam`, prefilled with the current one; empty means the NVR's name.
pub fn rename(app: &Rc<App>, cam: &Cam) {
    let nvr = app.sites.get(&cam.site).and_then(|s| s.channel(cam.channel)).map(|c| c.name).unwrap_or_else(|| format!("Channel {}", cam.channel));
    let current = app.cfg.borrow().camera_name(&cam.site, cam.channel).map(str::to_string).unwrap_or_default();
    let entry = adw::EntryRow::builder().title("Local name").text(&current).activates_default(true).build();
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    list.append(&entry);

    let d = adw::AlertDialog::new(
        Some("Rename camera"),
        Some(&format!(
            "{} · channel {}\nNVR name: {nvr}\n\nShown as “{:02} - Name”. Leave empty to use the NVR's name.",
            app.site_name(&cam.site),
            cam.channel,
            cam.channel
        )),
    );
    d.set_extra_child(Some(&list));
    d.add_response("cancel", "Cancel");
    d.add_response("save", "Save");
    d.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    d.set_default_response(Some("save"));
    d.set_close_response("cancel");
    let (a, cam2, e) = (app.clone(), cam.clone(), entry.clone());
    d.connect_response(Some("save"), move |_, _| set(&a, &cam2, Some(e.text().as_str())));
    d.present(Some(&app.window));
    // Ready to type over the old name.
    entry.grab_focus();
}

/// Sets (or with `None`/blank, removes) a camera's local name, saves, and refreshes the views.
pub fn set(app: &Rc<App>, cam: &Cam, name: Option<&str>) {
    let name = name.map(str::trim).filter(|n| !n.is_empty());
    let old = app.cfg.borrow().camera_name(&cam.site, cam.channel).map(str::to_string);
    if old.as_deref() == name {
        return;
    }
    app.cfg.borrow_mut().set_camera_name(&cam.site, cam.channel, name);
    app.save_config();
    app.audit.record(Entry {
        action: "camera.rename",
        site: Some(cam.site.clone()),
        channels: vec![cam.channel],
        detail: Some(match name {
            Some(n) => format!("{} → {n}", old.unwrap_or_else(|| "(NVR name)".into())),
            None => format!("{} → (NVR name)", old.unwrap_or_default()),
        }),
        ..Default::default()
    });
    app.cameras_renamed();
}
