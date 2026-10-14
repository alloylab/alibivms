//! "Download clip" dialog, opened from Playback.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::{
    app::{self, App},
    audit::Entry,
    download::Request,
    sites::now,
    ui::playback::PlaybackView,
};

const MAX_MINUTES: i64 = 240;

pub fn open(app: &Rc<App>, pb: &Rc<PlaybackView>) {
    let sel = pb.selection.borrow().clone();
    let Some(first) = sel.first() else { return };
    let site_id = first.site.clone();
    let Some(site) = app.sites.get(&site_id) else {
        app.toast("That site is not connected");
        return;
    };
    let (begin, end) = pb.suggested_range();

    let from = adw::EntryRow::builder().title("From").text(app::fmt_datetime(begin)).build();
    let to = adw::EntryRow::builder().title("To").text(app::fmt_datetime(end)).build();
    let times = adw::PreferencesGroup::builder().title(app.site_name(&site_id)).description("Local time, YYYY-MM-DD HH:MM:SS").build();
    times.add(&from);
    times.add(&to);

    let cams = adw::PreferencesGroup::builder().title("Cameras").description("Several cameras are saved as one zip with an MP4 each").build();
    let mut checks = Vec::new();
    let mut chans = site.channels();
    chans.sort_by_key(|c| c.id);
    for ch in chans {
        let check = gtk::CheckButton::builder().active(sel.iter().any(|c| c.channel == ch.id)).valign(gtk::Align::Center).build();
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(&app.channel_name(&site_id, ch.id))).subtitle(if ch.online { "" } else { "offline" }).activatable_widget(&check).build();
        row.add_prefix(&check);
        cams.add(&row);
        checks.push((ch.id, check));
    }

    let quality = adw::ComboRow::builder().title("Quality").model(&gtk::StringList::new(&["Full resolution (main stream)", "Smaller (sub stream)"])).build();
    let dest = app.cfg.borrow().download_dir.clone();
    let save_to = adw::ActionRow::builder().title("Save to").subtitle(dest.display().to_string()).build();
    let opts = adw::PreferencesGroup::new();
    opts.add(&quality);
    opts.add(&save_to);

    let error = gtk::Label::builder().css_classes(["error"]).wrap(true).xalign(0.0).visible(false).build();
    let start = gtk::Button::builder().label("Start download").css_classes(["suggested-action", "pill"]).halign(gtk::Align::Center).build();

    let page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(18).margin_top(12).margin_bottom(18).margin_start(18).margin_end(18).build();
    page.append(&times);
    page.append(&cams);
    page.append(&opts);
    page.append(&error);
    page.append(&start);
    let scroll = gtk::ScrolledWindow::builder().child(&page).hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true).build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroll));
    let dialog = adw::Dialog::builder().title("Download clip").content_width(480).content_height(640).child(&toolbar).build();

    let (app2, dialog2) = (app.clone(), dialog.clone());
    start.connect_clicked(move |_| {
        let fail = |msg: &str| {
            error.set_label(msg);
            error.set_visible(true);
        };
        let (Some(b), Some(e)) = (app::parse_datetime(&from.text()), app::parse_datetime(&to.text())) else {
            return fail("Enter times as YYYY-MM-DD HH:MM:SS");
        };
        if e <= b {
            return fail("The end must be after the start.");
        }
        if e > now() {
            return fail("The end is in the future.");
        }
        if e - b > MAX_MINUTES * 60 {
            return fail(&format!("Clips are limited to {MAX_MINUTES} minutes."));
        }
        let channels: Vec<u32> = checks.iter().filter(|(_, c)| c.is_active()).map(|(id, _)| *id).collect();
        if channels.is_empty() {
            return fail("Pick at least one camera.");
        }
        let stream = if quality.selected() == 0 { lapi::Stream::Main } else { lapi::Stream::Sub };
        let (scale, dest) = {
            let c = app2.cfg.borrow();
            (c.download_scale, c.download_dir.clone())
        };
        app2.audit.record(Entry {
            action: "download",
            site: Some(site_id.clone()),
            channels: channels.clone(),
            begin: Some(b),
            end: Some(e),
            detail: Some(format!("{stream:?} → {}", dest.display())),
        });
        let names = channels.iter().map(|&ch| (ch, app2.channel_name(&site_id, ch))).collect();
        let req = Request { site: site_id.clone(), dest, channels, names, begin: b, end: e, stream, scale };
        let (downloads, sites) = (app2.downloads.clone(), app2.sites.clone());
        let app3 = app2.clone();
        app::spawn_bg(async move { downloads.start(&sites, req).await }, move |r| match r {
            Ok(_) => {
                if let Some(v) = app3.downloads_view.borrow().as_ref() {
                    v.refresh();
                }
                app3.stack.set_visible_child_name("downloads");
            }
            Err(e) => app3.toast(format!("Download failed to start: {e:#}")),
        });
        dialog2.close();
    });
    dialog.present(Some(&app.window));
}
