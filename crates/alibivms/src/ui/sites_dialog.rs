//! Manage Star4Live accounts, sites (NVRs) and download settings.

use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::{
    app::{App, spawn_bg},
    audit::Entry,
    config::{CloudAccount, CloudLink, Config, SiteConfig, normalize_url},
};

/// Rebuilds the settings dialog's rows; handed (weakly, so the rows never keep it alive) to the
/// dialogs that change sites.
type Refill = Rc<RefCell<Option<Weak<dyn Fn()>>>>;

/// The list of sites plus general settings.
pub fn open(app: &Rc<App>) {
    let page = adw::PreferencesPage::new();
    let accounts = adw::PreferencesGroup::builder()
        .title("Star4Live")
        .description("Sign in with your Star4Live account to watch its NVRs from anywhere, through the cloud.")
        .build();
    let sign_in = gtk::Button::builder().label("Sign in…").css_classes(["flat"]).valign(gtk::Align::Center).build();
    accounts.set_header_suffix(Some(&sign_in));
    page.add(&accounts);
    let sites = adw::PreferencesGroup::builder().title("Sites").description("Each site is one Alibi NVR, reached through Star4Live or by its local address.").build();
    let add = gtk::Button::builder().icon_name("list-add-symbolic").tooltip_text("Add site by address").css_classes(["flat"]).valign(gtk::Align::Center).build();
    sites.set_header_suffix(Some(&add));
    page.add(&sites);

    let settings = adw::PreferencesGroup::builder().title("Downloads").build();
    let folder = adw::ActionRow::builder().title("Save clips to").subtitle(app.cfg.borrow().download_dir.display().to_string()).activatable(true).build();
    folder.add_suffix(&gtk::Image::from_icon_name("folder-open-symbolic"));
    let speed = adw::SpinRow::builder()
        .title("Download speed")
        .subtitle("How much faster than real time to pull clips from the NVR (1 = real time)")
        .adjustment(&gtk::Adjustment::new(app.cfg.borrow().download_scale as f64, 1.0, 16.0, 1.0, 4.0, 0.0))
        .build();
    settings.add(&folder);
    settings.add(&speed);
    page.add(&settings);
    let thumbs = adw::SwitchRow::builder().title("Camera thumbnails").subtitle("A still of each camera in the camera list, from its sub stream every few minutes").active(app.cfg.borrow().thumbnails).build();
    let a = app.clone();
    thumbs.connect_active_notify(move |s| {
        a.cfg.borrow_mut().thumbnails = s.is_active();
        a.thumbnails.set_enabled(s.is_active());
        a.save_config();
    });
    let camera_list = adw::PreferencesGroup::builder().title("Camera list").build();
    camera_list.add(&thumbs);
    page.add(&camera_list);
    let updates = adw::SwitchRow::builder().title("Check for updates").subtitle("Asks GitHub once a day whether a newer version is out").active(app.cfg.borrow().check_updates).build();
    let a = app.clone();
    updates.connect_active_notify(move |s| {
        a.cfg.borrow_mut().check_updates = s.is_active();
        a.save_config();
    });
    let updates_group = adw::PreferencesGroup::builder().title("Updates").build();
    updates_group.add(&updates);
    page.add(&updates_group);

    let backup = adw::PreferencesGroup::builder().title("Backup").description("Sites, video walls and camera names, as a JSON file to keep or move to another computer.").build();
    let export = adw::ActionRow::builder().title("Export settings…").activatable(true).build();
    export.add_suffix(&gtk::Image::from_icon_name("document-save-symbolic"));
    let import = adw::ActionRow::builder().title("Import settings…").activatable(true).build();
    import.add_suffix(&gtk::Image::from_icon_name("document-open-symbolic"));
    backup.add(&export);
    backup.add(&import);
    page.add(&backup);

    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Sites and settings");
    dialog.add(&page);

    // Rebuilds the site rows and resets the settings rows from the config; also handed to the
    // edit/delete/import dialogs to call after a change.
    let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::default();
    let account_rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::default();
    let refill: Refill = Rc::default();
    let fill: Rc<dyn Fn()> = {
        let (app, sites, rows, refill) = (app.clone(), sites.clone(), rows.clone(), refill.clone());
        let (accounts, account_rows) = (accounts.clone(), account_rows.clone());
        let (speed, thumbs, updates) = (speed.clone(), thumbs.clone(), updates.clone());
        Rc::new(move || {
            for r in rows.borrow_mut().drain(..) {
                sites.remove(&r);
            }
            for r in account_rows.borrow_mut().drain(..) {
                accounts.remove(&r);
            }
            // Read first: each row's handler writes the config back when its value changes.
            let (scale, thumbs_on, updates_on) = {
                let cfg = app.cfg.borrow();
                (cfg.download_scale as f64, cfg.thumbnails, cfg.check_updates)
            };
            speed.set_value(scale);
            thumbs.set_active(thumbs_on);
            updates.set_active(updates_on);
            let again = refill.borrow().clone();
            for acc in app.cfg.borrow().cloud_accounts.clone() {
                let n = app.cfg.borrow().sites.iter().filter(|s| s.cloud.as_ref().is_some_and(|c| c.account == acc.login_name)).count();
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&acc.login_name))
                    .subtitle(if acc.password_hash.is_empty() { "Signed out: sign in again".to_string() } else { plural(n, "NVR") + " added" })
                    .build();
                let pick = gtk::Button::builder().label("Add NVRs…").css_classes(["flat"]).valign(gtk::Align::Center).build();
                let out = gtk::Button::builder().icon_name("system-log-out-symbolic").tooltip_text("Sign out and remove its sites").css_classes(["flat"]).valign(gtk::Align::Center).build();
                row.add_suffix(&pick);
                row.add_suffix(&out);
                let (a, name, again2) = (app.clone(), acc.login_name.clone(), again.clone());
                pick.connect_clicked(move |b| pick_devices(&a, &name, b, upgrade(&again2).unwrap_or_else(|| Rc::new(|| {}))));
                let (a, name, again2) = (app.clone(), acc.login_name.clone(), again.clone());
                out.connect_clicked(move |_| confirm_sign_out(&a, &name, upgrade(&again2)));
                accounts.add(&row);
                account_rows.borrow_mut().push(row);
            }
            for s in app.cfg.borrow().sites.clone() {
                let row = adw::ActionRow::builder().title(glib::markup_escape_text(&s.name)).subtitle(glib::markup_escape_text(&s.location())).activatable(true).build();
                let del = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text("Remove site").css_classes(["flat"]).valign(gtk::Align::Center).build();
                row.add_suffix(&del);
                row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                let (a, id, again2) = (app.clone(), s.id.clone(), again.clone());
                row.connect_activated(move |_| edit(&a, Some(id.clone()), upgrade(&again2)));
                let (a, s2, again2) = (app.clone(), s.clone(), again.clone());
                del.connect_clicked(move |_| confirm_delete(&a, &s2, upgrade(&again2)));
                sites.add(&row);
                rows.borrow_mut().push(row);
            }
        })
    };
    *refill.borrow_mut() = Some(Rc::downgrade(&fill));
    fill();

    let (a, f) = (app.clone(), fill.clone());
    add.connect_clicked(move |_| edit(&a, None, Some(f.clone())));
    let (a, f) = (app.clone(), fill.clone());
    sign_in.connect_clicked(move |_| cloud_sign_in(&a, None, f.clone()));
    let a = app.clone();
    speed.connect_value_notify(move |s| {
        a.cfg.borrow_mut().download_scale = s.value() as u32;
        a.save_config();
    });
    let a = app.clone();
    folder.connect_activated(move |row| {
        let (a2, row) = (a.clone(), row.clone());
        let fd = gtk::FileDialog::builder().title("Save clips to").initial_folder(&gio::File::for_path(&a.cfg.borrow().download_dir)).build();
        fd.select_folder(Some(&a.window), gio::Cancellable::NONE, move |r| {
            if let Ok(dir) = r
                && let Some(p) = dir.path()
            {
                row.set_subtitle(&p.display().to_string());
                a2.cfg.borrow_mut().download_dir = p;
                a2.save_config();
            }
        });
    });
    let a = app.clone();
    export.connect_activated(move |_| export_settings(&a));
    let (a, f) = (app.clone(), fill.clone());
    import.connect_activated(move |_| import_settings(&a, f.clone()));
    dialog.present(Some(&app.window));
}

fn upgrade(f: &Option<Weak<dyn Fn()>>) -> Option<Rc<dyn Fn()>> {
    f.as_ref().and_then(Weak::upgrade)
}

/// Tells the live view, the sidebar, the alerts poller and the thumbnails that sites or settings
/// changed behind their backs.
fn config_replaced(app: &Rc<App>) {
    app.sites_changed();
    if let Some(l) = app.live.borrow().as_ref() {
        l.build();
    }
    if let Some(al) = app.alerts.borrow().as_ref() {
        al.resync();
    }
    app.thumbnails.set_enabled(app.cfg.borrow().thumbnails);
}

fn json_filters() -> gio::ListStore {
    let f = gtk::FileFilter::new();
    f.set_name(Some("JSON files"));
    f.add_mime_type("application/json");
    f.add_suffix("json");
    let store = gio::ListStore::new::<gtk::FileFilter>();
    store.append(&f);
    store
}

/// Asks whether to include NVR passwords, then where to save the backup.
fn export_settings(app: &Rc<App>) {
    let passwords = adw::SwitchRow::builder().title("Include NVR passwords").subtitle("Anyone with the file can then log in to the NVRs").active(false).build();
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    list.append(&passwords);
    let d = adw::AlertDialog::new(
        Some("Export settings"),
        Some("Saves sites, video walls and camera names. Without passwords, each site must have its password entered again after importing."),
    );
    d.set_extra_child(Some(&list));
    d.add_response("cancel", "Cancel");
    d.add_response("export", "Export…");
    d.set_response_appearance("export", adw::ResponseAppearance::Suggested);
    d.set_default_response(Some("export"));
    let a = app.clone();
    d.connect_response(Some("export"), move |_, _| {
        let with_passwords = passwords.is_active();
        let day = glib::DateTime::now_local().ok().and_then(|d| d.format("%Y-%m-%d").ok()).map(|s| s.to_string()).unwrap_or_default();
        let filters = json_filters();
        let fd = gtk::FileDialog::builder().title("Export settings").initial_name(format!("alibivms-config-{day}.json")).filters(&filters).modal(true).build();
        let a2 = a.clone();
        fd.save(Some(&a.window), gio::Cancellable::NONE, move |r| {
            let Some(path) = r.ok().and_then(|f| f.path()) else { return };
            match a2.cfg.borrow().export_to(&path, with_passwords) {
                Ok(()) => a2.toast(format!("Settings exported to {}", path.display())),
                Err(e) => a2.toast(format!("Could not export settings: {e:#}")),
            }
            a2.audit.record(Entry {
                action: "config.export",
                detail: Some(format!("{}{}", path.display(), if with_passwords { " (with passwords)" } else { "" })),
                ..Default::default()
            });
        });
    });
    d.present(Some(&app.window));
}

/// Picks a backup, summarises it, and merges it in or replaces the current settings with it.
fn import_settings(app: &Rc<App>, after: Rc<dyn Fn()>) {
    let filters = json_filters();
    let fd = gtk::FileDialog::builder().title("Import settings").filters(&filters).modal(true).build();
    let a = app.clone();
    fd.open(Some(&app.window), gio::Cancellable::NONE, move |r| {
        let Some(path) = r.ok().and_then(|f| f.path()) else { return };
        let incoming = match Config::import_from(&path) {
            Ok(c) => c,
            Err(e) => return a.toast(format!("Could not import {}: {e:#}", path.display())),
        };
        let with_password = incoming.sites.iter().filter(|s| !s.password.is_empty()).count();
        let body = format!(
            "{} contains {}, {} and {}{}.\n\nMerge adds what is new and updates sites that already exist. Replace swaps your current sites, walls and camera names for these (the download folder stays).",
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            plural(incoming.sites.len(), "site"),
            plural(incoming.walls.len(), "video wall"),
            plural(incoming.camera_name_count(), "camera name"),
            match (incoming.sites.len(), with_password) {
                (0, _) => String::new(),
                (n, p) if p == n => ", with passwords".to_string(),
                (_, 0) => ", without passwords".to_string(),
                (_, p) => format!(", {p} with passwords"),
            }
        );
        let d = adw::AlertDialog::new(Some("Import settings?"), Some(&body));
        d.add_response("cancel", "Cancel");
        d.add_response("replace", "Replace");
        d.add_response("merge", "Merge");
        d.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
        d.set_response_appearance("merge", adw::ResponseAppearance::Suggested);
        d.set_default_response(Some("merge"));
        d.set_close_response("cancel");
        let (a2, after, incoming) = (a.clone(), after.clone(), std::cell::RefCell::new(Some(incoming)));
        d.connect_response(None, move |_, resp| {
            let replace = match resp {
                "merge" => false,
                "replace" => true,
                _ => return,
            };
            let Some(incoming) = incoming.borrow_mut().take() else { return };
            let sum = a2.cfg.borrow_mut().merge_from(incoming, replace);
            a2.audit.record(Entry {
                action: "config.import",
                detail: Some(format!("{} from {}: {sum:?}", if replace { "replace" } else { "merge" }, path.display())),
                ..Default::default()
            });
            config_replaced(&a2);
            a2.cameras_renamed();
            after();
            let mut msg = format!("Imported: {} added, {} updated", plural(sum.sites_added, "site"), sum.sites_updated);
            if sum.sites_removed > 0 {
                msg.push_str(&format!(", {} removed", sum.sites_removed));
            }
            msg.push_str(&format!(", {}, {}", plural(sum.walls_added, "wall"), plural(sum.names_added, "camera name")));
            if sum.without_password > 0 {
                msg.push_str(&format!(". {} no password and will show offline until you edit {}", if sum.without_password == 1 { "1 site has" } else { "Some sites have" }, if sum.without_password == 1 { "it" } else { "them" }));
            }
            if sum.accounts_missing > 0 {
                msg.push_str(&format!(". {} a Star4Live account signed in before coming online", if sum.accounts_missing == 1 { "1 cloud site needs" } else { "Some cloud sites need" }));
            }
            a2.toast(msg);
        });
        d.present(Some(&a.window));
    });
}

/// Drops sites from the config and from everything showing them, then saves.
fn forget_sites(app: &Rc<App>, ids: &[String]) {
    {
        let mut cfg = app.cfg.borrow_mut();
        for id in ids {
            cfg.remove_site(id);
        }
    }
    if let Some(p) = app.playback.borrow().as_ref() {
        p.selection.borrow_mut().retain(|c| !ids.contains(&c.site));
    }
    config_replaced(app);
}

fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

fn confirm_delete(app: &Rc<App>, site: &SiteConfig, after: Option<Rc<dyn Fn()>>) {
    let d = adw::AlertDialog::new(Some(&format!("Remove {}?", site.name)), Some("Its NVR keeps recording; only this app forgets it."));
    d.add_response("cancel", "Cancel");
    d.add_response("remove", "Remove");
    d.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    let (a, id) = (app.clone(), site.id.clone());
    d.connect_response(Some("remove"), move |_, _| {
        forget_sites(&a, std::slice::from_ref(&id));
        a.audit.record(Entry { action: "site.remove", site: Some(id.clone()), ..Default::default() });
        if let Some(f) = &after {
            f();
        }
    });
    d.present(Some(&app.window));
}

/// Add (`None`) or edit a site.
pub fn edit(app: &Rc<App>, id: Option<String>, after: Option<Rc<dyn Fn()>>) {
    let existing = id.as_ref().and_then(|id| app.cfg.borrow().site(id).cloned());
    let name = adw::EntryRow::builder().title("Name").text(existing.as_ref().map(|s| s.name.as_str()).unwrap_or("")).build();
    let group_row = adw::EntryRow::builder().title("Group (optional, e.g. a region or customer)").text(existing.as_ref().map(|s| s.group.as_str()).unwrap_or("")).build();
    let url = adw::EntryRow::builder().title("NVR address").text(existing.as_ref().map(|s| s.nvr_url.as_str()).unwrap_or("")).build();
    let user = adw::EntryRow::builder().title("NVR username").text(existing.as_ref().map(|s| s.username.as_str()).unwrap_or("admin")).build();
    let pass = adw::PasswordEntryRow::builder().title(if existing.is_some() { "NVR password (leave empty to keep)" } else { "NVR password" }).build();
    let cloud = existing.as_ref().and_then(|s| s.cloud.clone());
    let group = adw::PreferencesGroup::builder()
        .description(match &cloud {
            Some(c) => format!("Reached through Star4Live account {}, device {}", c.account, c.serial),
            None => "Address of the NVR's web page, e.g. 10.9.0.2".to_string(),
        })
        .build();
    group.add(&name);
    group.add(&group_row);
    if cloud.is_none() {
        for r in [url.upcast_ref::<gtk::Widget>(), user.upcast_ref(), pass.upcast_ref()] {
            group.add(r);
        }
    }

    let result = gtk::Label::builder().wrap(true).xalign(0.0).visible(false).build();
    let test = gtk::Button::builder().label("Test connection").build();
    let save = gtk::Button::builder().label(if existing.is_some() { "Save" } else { "Add site" }).css_classes(["suggested-action"]).build();
    let buttons = gtk::Box::builder().spacing(8).halign(gtk::Align::End).build();
    buttons.append(&test);
    buttons.append(&save);

    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(12).margin_bottom(18).margin_start(18).margin_end(18).build();
    body.append(&group);
    body.append(&result);
    body.append(&buttons);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&body));
    let dialog = adw::Dialog::builder().title(if existing.is_some() { "Edit site" } else { "Add site" }).content_width(440).child(&toolbar).build();

    let gather = {
        let (name, url, user, pass, existing) = (name.clone(), url.clone(), user.clone(), pass.clone(), existing.clone());
        let group_row = group_row.clone();
        move || -> Result<SiteConfig, String> {
            let n = name.text().trim().to_string();
            let g = group_row.text().trim().to_string();
            if let Some(e) = existing.as_ref().filter(|e| e.cloud.is_some()) {
                if n.is_empty() {
                    return Err("Fill in the name.".into());
                }
                return Ok(SiteConfig { name: n, group: g, ..e.clone() });
            }
            let u = url.text().trim().to_string();
            let un = user.text().trim().to_string();
            if n.is_empty() || u.is_empty() || un.is_empty() {
                return Err("Fill in the name, address and username.".into());
            }
            let mut p = pass.text().to_string();
            if p.is_empty() {
                p = existing.as_ref().map(|e| e.password.clone()).unwrap_or_default();
            }
            if p.is_empty() {
                return Err("Enter the NVR password.".into());
            }
            Ok(SiteConfig { id: existing.as_ref().map(|e| e.id.clone()).unwrap_or_default(), name: n, nvr_url: normalize_url(&u), username: un, password: p, cloud: None, group: g })
        }
    };
    let gather = Rc::new(gather);

    let show = {
        let result = result.clone();
        move |text: &str, ok: bool| {
            result.set_label(text);
            result.set_css_classes(if ok { &["success"] } else { &["error"] });
            result.set_visible(true);
        }
    };
    let show = Rc::new(show);

    {
        let (gather, show, test2, a) = (gather.clone(), show.clone(), test.clone(), app.clone());
        test.connect_clicked(move |_| {
            let s = match gather() {
                Ok(s) => s,
                Err(e) => return show(&e, false),
            };
            let account = s.cloud.as_ref().and_then(|c| a.sites.account(&c.account));
            if s.cloud.is_some() && account.is_none() {
                return show("Its Star4Live account is not signed in.", false);
            }
            test2.set_sensitive(false);
            show("Connecting…", true);
            let (show, test3) = (show.clone(), test2.clone());
            spawn_bg(
                async move {
                    let c = match (account, &s.cloud) {
                        (Some(acc), Some(link)) => lapi::Client::cloud(acc, &link.serial)?,
                        _ => lapi::Client::new(&s.nvr_url, &lapi::Credentials { username: s.username, password: s.password })?,
                    };
                    let r = tokio::time::timeout(std::time::Duration::from_secs(20), async {
                        c.login().await?;
                        let d = c.device_info().await?;
                        let ch = c.channels().await?;
                        let _ = c.logout().await;
                        Ok::<_, lapi::Error>((d, ch))
                    })
                    .await
                    .map_err(|_| lapi::Error::Login("no answer within 20 s".into()))??;
                    Ok::<_, lapi::Error>(r)
                },
                move |r| {
                    test3.set_sensitive(true);
                    match r {
                        Ok((d, ch)) => show(
                            &format!(
                                "Connected: {} (firmware {}), {} cameras, {} online.",
                                d.device_model,
                                d.firmware_version,
                                ch.len(),
                                ch.iter().filter(|c| c.online).count()
                            ),
                            true,
                        ),
                        Err(e) => show(&format!("Could not connect: {e}"), false),
                    }
                },
            );
        });
    }
    {
        let (a, dialog2) = (app.clone(), dialog.clone());
        save.connect_clicked(move |_| {
            let mut s = match gather() {
                Ok(s) => s,
                Err(e) => return show(&e, false),
            };
            let action = {
                let mut cfg = a.cfg.borrow_mut();
                if s.id.is_empty() {
                    s.id = cfg.new_site_id(&s.name);
                    cfg.sites.push(s.clone());
                    "site.add"
                } else {
                    if let Some(e) = cfg.sites.iter_mut().find(|e| e.id == s.id) {
                        *e = s.clone();
                    }
                    "site.edit"
                }
            };
            a.audit.record(Entry { action, site: Some(s.id.clone()), detail: Some(s.location()), ..Default::default() });
            a.sites_changed();
            if let Some(f) = &after {
                f();
            }
            dialog2.close();
        });
    }
    dialog.present(Some(&app.window));
}

/// Signs in to Star4Live (a new account, or `again` for one whose sign-in lapsed), saves the
/// account, then offers its NVRs.
pub fn cloud_sign_in(app: &Rc<App>, again: Option<String>, after: Rc<dyn Fn()>) {
    let user = adw::EntryRow::builder().title("Email or username").text(again.as_deref().unwrap_or("")).build();
    let pass = adw::PasswordEntryRow::builder().title("Password").build();
    let group = adw::PreferencesGroup::builder().description("The account you use in the Star4Live app or at global.star4live.com.").build();
    group.add(&user);
    group.add(&pass);
    let result = gtk::Label::builder().wrap(true).xalign(0.0).visible(false).build();
    let go = gtk::Button::builder().label("Sign in").css_classes(["suggested-action"]).halign(gtk::Align::End).build();
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(12).margin_bottom(18).margin_start(18).margin_end(18).build();
    body.append(&group);
    body.append(&result);
    body.append(&go);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&body));
    let dialog = adw::Dialog::builder().title("Sign in to Star4Live").content_width(420).child(&toolbar).build();

    let submit = {
        let (a, dialog, go, result) = (app.clone(), dialog.clone(), go.clone(), result.clone());
        let (user, pass) = (user.clone(), pass.clone());
        move || {
            // Enter in the password row lands here too; one sign-in at a time.
            if !go.is_sensitive() {
                return;
            }
            let name = user.text().trim().to_string();
            let pw = pass.text().to_string();
            if name.is_empty() || pw.is_empty() {
                result.set_label("Enter your email or username, and password.");
                result.set_css_classes(&["error"]);
                result.set_visible(true);
                return;
            }
            let hard_id = a.cfg.borrow().cloud_account(&name).map(|c| c.hard_id.clone()).unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
            let acc = CloudAccount { login_name: name.clone(), password_hash: lapi::cloud::password_hash(&pw), hard_id };
            go.set_sensitive(false);
            result.set_label("Signing in…");
            result.set_css_classes(&["dim-label"]);
            result.set_visible(true);
            let (a2, dialog2, go2, result2, after2) = (a.clone(), dialog.clone(), go.clone(), result.clone(), after.clone());
            let acc2 = acc.clone();
            spawn_bg(
                async move {
                    let c = lapi::cloud::Account::new(&acc2.login_name, &acc2.password_hash, &acc2.hard_id)?;
                    c.login().await?;
                    c.devices().await
                },
                move |r: lapi::Result<Vec<lapi::cloud::Device>>| {
                    go2.set_sensitive(true);
                    let devices = match r {
                        Ok(d) => d,
                        Err(e) => {
                            result2.set_label(&format!("Could not sign in: {e}"));
                            result2.set_css_classes(&["error"]);
                            return;
                        }
                    };
                    {
                        let mut cfg = a2.cfg.borrow_mut();
                        cfg.cloud_accounts.retain(|c| c.login_name != acc.login_name);
                        cfg.cloud_accounts.push(acc.clone());
                    }
                    a2.audit.record(Entry { action: "cloud.sign_in", detail: Some(acc.login_name.clone()), ..Default::default() });
                    a2.sites_changed();
                    after2();
                    dialog2.close();
                    show_devices(&a2, &acc.login_name, devices, after2.clone());
                },
            );
        }
    };
    let submit = Rc::new(submit);
    let s = submit.clone();
    go.connect_clicked(move |_| s());
    let s = submit.clone();
    pass.connect_entry_activated(move |_| s());
    dialog.present(Some(&app.window));
}

/// Lists the account's devices to add as sites. `button` is the "Add NVRs…" that asked, held
/// insensitive until the list is up so a second click can't open it twice.
fn pick_devices(app: &Rc<App>, login_name: &str, button: &gtk::Button, after: Rc<dyn Fn()>) {
    let Some(account) = app.sites.account(login_name) else { return cloud_sign_in(app, Some(login_name.to_string()), after) };
    let (a, name, button) = (app.clone(), login_name.to_string(), button.clone());
    button.set_sensitive(false);
    app.toast("Fetching devices…");
    spawn_bg(async move { account.devices().await }, move |r| {
        button.set_sensitive(true);
        match r {
            Ok(d) => show_devices(&a, &name, d, after.clone()),
            Err(e) => {
                a.toast(format!("Star4Live: {e}"));
                cloud_sign_in(&a, Some(name.clone()), after.clone());
            }
        }
    });
}

fn show_devices(app: &Rc<App>, login_name: &str, devices: Vec<lapi::cloud::Device>, after: Rc<dyn Fn()>) {
    let added: Vec<String> = app.cfg.borrow().sites.iter().filter_map(|s| s.cloud.as_ref()).filter(|c| c.account == login_name).map(|c| c.serial.clone()).collect();
    let group = adw::PreferencesGroup::new();
    let mut checks = Vec::new();
    for d in &devices {
        let present = added.contains(&d.serial);
        let check = gtk::CheckButton::builder().active(!present && d.kind == 1).sensitive(!present).valign(gtk::Align::Center).build();
        let state = if present { "already added" } else if d.online { "online" } else { "offline" };
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&d.name))
            .subtitle(glib::markup_escape_text(&format!("{} · {} · {state}", d.model, d.serial)))
            .activatable_widget(&check)
            .build();
        row.add_prefix(&check);
        group.add(&row);
        checks.push((check, d.clone()));
    }
    if devices.is_empty() {
        group.set_description(Some("This account has no devices."));
    }
    let add = gtk::Button::builder().label("Add selected").css_classes(["suggested-action"]).halign(gtk::Align::End).build();
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(12).margin_bottom(18).margin_start(18).margin_end(18).build();
    body.append(&group);
    body.append(&add);
    let scroll = gtk::ScrolledWindow::builder().child(&body).hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true).max_content_height(560).build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroll));
    let dialog = adw::Dialog::builder().title(format!("Devices on {login_name}")).content_width(480).child(&toolbar).build();
    let (a, d2, name) = (app.clone(), dialog.clone(), login_name.to_string());
    add.connect_clicked(move |_| {
        let mut n = 0;
        {
            let mut cfg = a.cfg.borrow_mut();
            for (check, d) in &checks {
                // Added since the list was made (by another copy of this dialog) counts too.
                let present = cfg.sites.iter().any(|s| s.cloud.as_ref().is_some_and(|c| c.account == name && c.serial == d.serial));
                if !check.is_active() || !check.is_sensitive() || present {
                    continue;
                }
                let id = cfg.new_site_id(&d.name);
                cfg.sites.push(SiteConfig { id, name: d.name.clone(), cloud: Some(CloudLink { account: name.clone(), serial: d.serial.clone() }), ..Default::default() });
                n += 1;
            }
        }
        if n > 0 {
            a.audit.record(Entry { action: "site.add", detail: Some(format!("{n} from Star4Live {name}")), ..Default::default() });
            a.sites_changed();
            a.toast(format!("Added {}", plural(n, "NVR")));
        }
        after();
        d2.close();
    });
    dialog.present(Some(&app.window));
}

fn confirm_sign_out(app: &Rc<App>, login_name: &str, after: Option<Rc<dyn Fn()>>) {
    let n = app.cfg.borrow().sites.iter().filter(|s| s.cloud.as_ref().is_some_and(|c| c.account == login_name)).count();
    let d = adw::AlertDialog::new(Some(&format!("Sign out of {login_name}?")), Some(&format!("Its {} will be removed from this app.", plural(n, "NVR"))));
    d.add_response("cancel", "Cancel");
    d.add_response("out", "Sign out");
    d.set_response_appearance("out", adw::ResponseAppearance::Destructive);
    let (a, name) = (app.clone(), login_name.to_string());
    d.connect_response(Some("out"), move |_, _| {
        let gone: Vec<String> = {
            let mut cfg = a.cfg.borrow_mut();
            cfg.cloud_accounts.retain(|c| c.login_name != name);
            cfg.sites.iter().filter(|s| s.cloud.as_ref().is_some_and(|c| c.account == name)).map(|s| s.id.clone()).collect()
        };
        forget_sites(&a, &gone);
        a.audit.record(Entry { action: "cloud.sign_out", detail: Some(name.clone()), ..Default::default() });
        if let Some(f) = &after {
            f();
        }
    });
    d.present(Some(&app.window));
}
