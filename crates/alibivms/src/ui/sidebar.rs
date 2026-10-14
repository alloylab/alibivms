//! Site and camera list. Clicking a camera adds it to Live, or toggles it in Playback; cameras
//! can also be dragged onto a tile, and renamed locally from their right-click menu.

use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    rc::Rc,
    time::Duration,
};

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::{
    app::{App, Cam},
    ui::{alerts, camera_name, diagnostics, sites_dialog, tile},
};

pub struct Sidebar {
    pub root: gtk::Box,
    list: gtk::Box,
    filter: gtk::SearchEntry,
    offline_only: gtk::ToggleButton,
    status: gtk::Label,
    seen: Cell<u64>,
    /// Sites the user has opened in this session; all start collapsed.
    expanded: Rc<RefCell<HashSet<String>>>,
    app: Rc<App>,
}

impl Sidebar {
    pub fn new(app: &Rc<App>) -> Rc<Self> {
        let filter = gtk::SearchEntry::builder().placeholder_text("Filter cameras").hexpand(true).build();
        let offline_only = gtk::ToggleButton::builder().label("Offline").tooltip_text("Show only offline cameras and NVRs").css_classes(["flat", "caption"]).build();
        let filter_row = gtk::Box::builder().spacing(4).build();
        filter_row.append(&filter);
        filter_row.append(&offline_only);
        let status = gtk::Label::builder().xalign(0.0).css_classes(["dim-label", "caption"]).margin_start(4).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let list = gtk::Box::new(gtk::Orientation::Vertical, 12);
        let scroll = gtk::ScrolledWindow::builder().child(&list).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
        let manage = gtk::Button::builder().label("Manage sites…").css_classes(["flat"]).build();
        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).margin_top(8).margin_bottom(8).margin_start(8).margin_end(8).build();
        root.append(&filter_row);
        root.append(&status);
        root.append(&scroll);
        root.append(&manage);
        let sb = Rc::new(Self { root, list, filter, offline_only, status, seen: Cell::new(u64::MAX), expanded: Rc::default(), app: app.clone() });

        let w = Rc::downgrade(&sb);
        sb.filter.connect_search_changed(move |_| if let Some(s) = w.upgrade() { s.render() });
        let w = Rc::downgrade(&sb);
        sb.offline_only.connect_toggled(move |_| if let Some(s) = w.upgrade() { s.render() });
        sb.add_site_actions();
        let a = app.clone();
        manage.connect_clicked(move |_| sites_dialog::open(&a));
        let w = Rc::downgrade(&sb);
        glib::timeout_add_local(Duration::from_millis(1500), move || {
            let Some(s) = w.upgrade() else { return glib::ControlFlow::Break };
            let g = s.app.sites.generation();
            if g != s.seen.get() {
                s.seen.set(g);
                s.render();
                if let Some(l) = s.app.live.borrow().as_ref() {
                    l.refresh_labels();
                }
                if let Some(p) = s.app.playback.borrow().as_ref() {
                    p.refresh();
                }
            }
            glib::ControlFlow::Continue
        });
        sb.render();
        sb
    }

    pub fn render(&self) {
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        let q = self.filter.text().to_lowercase();
        let configs = self.app.cfg.borrow().sites.clone();
        if configs.is_empty() {
            let page = adw::StatusPage::builder().icon_name("network-server-symbolic").title("No sites").description("Add an NVR to get started.").build();
            page.add_css_class("compact");
            let add = gtk::Button::builder().label("Add site…").css_classes(["suggested-action", "pill"]).halign(gtk::Align::Center).build();
            let a = self.app.clone();
            add.connect_clicked(move |_| sites_dialog::edit(&a, None, None));
            page.set_child(Some(&add));
            self.list.append(&page);
            return;
        }
        let offline_only = self.offline_only.is_active();
        let sites = self.app.sites.all(&configs);
        let (mut nvrs_up, mut cams, mut cams_up) = (0, 0, 0);
        for site in &sites {
            let st = site.status();
            nvrs_up += st.online as usize;
            let ch = site.channels();
            cams += ch.len();
            cams_up += ch.iter().filter(|c| c.online && st.online).count();
        }
        self.status.set_label(&format!("{nvrs_up} of {} NVRs online · {cams_up} of {cams} cameras", sites.len()));

        // Favourites first.
        let favs: Vec<Cam> = self.app.cfg.borrow().favorites.iter().map(|f| Cam { site: f.site.clone(), channel: f.channel }).collect();
        let fav_rows: Vec<_> = favs
            .iter()
            .filter_map(|cam| {
                let site = self.app.sites.get(&cam.site)?;
                let ch = site.channel(cam.channel)?;
                let shown = self.app.cam_label(cam);
                let keep = (q.is_empty() || shown.to_lowercase().contains(&q)) && (!offline_only || !(ch.online && site.status().online));
                keep.then(|| self.camera_row(cam.clone(), &shown, &ch))
            })
            .collect();
        if !fav_rows.is_empty() {
            self.list.append(&heading("Favourites"));
            let group = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
            for r in fav_rows {
                group.append(&r);
            }
            self.list.append(&group);
        }

        // Sites, under their group headings in the order groups first appear.
        let mut groups: Vec<String> = Vec::new();
        for c in &configs {
            if !groups.contains(&c.group) {
                groups.push(c.group.clone());
            }
        }
        groups.sort_by_key(|g| g.is_empty());
        let grouped = groups.iter().any(|g| !g.is_empty());
        for g in &groups {
            let mut shown_heading = false;
            for site in sites.iter().filter(|s| &s.cfg.group == g) {
                let Some(expander) = self.site_row(site, &q, offline_only) else { continue };
                if grouped && !shown_heading {
                    self.list.append(&heading(if g.is_empty() { "Other" } else { g }));
                    shown_heading = true;
                }
                let group = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
                group.append(&expander);
                self.list.append(&group);
            }
        }
        self.mark_selection();
    }

    /// One NVR and its cameras, or `None` when the filter hides all of them.
    fn site_row(&self, site: &std::sync::Arc<crate::sites::Site>, q: &str, offline_only: bool) -> Option<adw::ExpanderRow> {
        let st = site.status();
        let site_match = site.cfg.name.to_lowercase().contains(q) || (!site.cfg.group.is_empty() && site.cfg.group.to_lowercase().contains(q));
        let mut chans: Vec<_> = site
            .channels()
            .into_iter()
            .map(|c| (self.app.channel_name(&site.cfg.id, c.id), c))
            .filter(|(shown, c)| q.is_empty() || site_match || shown.to_lowercase().contains(q) || c.name.to_lowercase().contains(q))
            .filter(|(_, c)| !offline_only || !(c.online && st.online))
            .collect();
        chans.sort_by_key(|(_, c)| c.id);
        let filtering = !q.is_empty() || offline_only;
        if filtering && chans.is_empty() && !(offline_only && !st.online) {
            return None;
        }
        let online = site.channels().iter().filter(|c| c.online).count();
        let (subtitle, tooltip) = if st.online {
            let s = format!("{online}/{} cameras online{}", site.channels().len(), st.time_zone.map(|z| format!(" · {z}")).unwrap_or_default());
            (s.clone(), s)
        } else {
            match st.error {
                Some(e) => (format!("Offline: {}", crate::app::short_error(&e)), format!("Offline: {e}")),
                None => ("Connecting…".into(), "Connecting…".into()),
            }
        };
        let expander = adw::ExpanderRow::builder()
            .title(glib::markup_escape_text(&site.cfg.name))
            .subtitle(glib::markup_escape_text(&subtitle))
            .subtitle_lines(1)
            .tooltip_text(&tooltip)
            // While filtering, open every site with a match.
            .expanded(filtering || self.expanded.borrow().contains(&site.cfg.id))
            .build();
        if !filtering {
            let (set, id) = (self.expanded.clone(), site.cfg.id.clone());
            expander.connect_expanded_notify(move |e| {
                if e.is_expanded() {
                    set.borrow_mut().insert(id.clone());
                } else {
                    set.borrow_mut().remove(&id);
                }
            });
        }
        expander.add_prefix(&dot(st.online));
        let menu = gio::Menu::new();
        let target = glib::Variant::from(site.cfg.id.as_str());
        let item = |label: &str, action: &str| {
            let i = gio::MenuItem::new(Some(label), None);
            i.set_action_and_target_value(Some(action), Some(&target));
            i
        };
        menu.append_item(&item("Diagnostics…", "sb.diagnostics"));
        menu.append_item(&item("Edit site…", "sb.edit"));
        let all = gio::Menu::new();
        all.append(Some("Expand all"), Some("sb.expand-all"));
        all.append(Some("Collapse all"), Some("sb.collapse-all"));
        menu.append_section(None, &all);
        let more = gtk::MenuButton::builder().icon_name("view-more-symbolic").tooltip_text("Site options").menu_model(&menu).css_classes(["flat", "circular"]).valign(gtk::Align::Center).build();
        expander.add_suffix(&more);
        for (shown, ch) in chans {
            let cam = Cam { site: site.cfg.id.clone(), channel: ch.id };
            expander.add_row(&self.camera_row(cam, &shown, &ch));
        }
        Some(expander)
    }

    /// `sb.*` actions behind each site's options menu.
    fn add_site_actions(self: &Rc<Self>) {
        let group = gio::SimpleActionGroup::new();
        let a = self.app.clone();
        let act = gio::SimpleAction::new("diagnostics", Some(glib::VariantTy::STRING));
        act.connect_activate(move |_, p| {
            if let Some(id) = p.and_then(|p| p.get::<String>()) {
                diagnostics::site(&a, &id);
            }
        });
        group.add_action(&act);
        let a = self.app.clone();
        let act = gio::SimpleAction::new("edit", Some(glib::VariantTy::STRING));
        act.connect_activate(move |_, p| {
            if let Some(id) = p.and_then(|p| p.get::<String>()) {
                sites_dialog::edit(&a, Some(id), None);
            }
        });
        group.add_action(&act);
        for (name, open) in [("expand-all", true), ("collapse-all", false)] {
            let w = Rc::downgrade(self);
            let act = gio::SimpleAction::new(name, None);
            act.connect_activate(move |_, _| {
                let Some(s) = w.upgrade() else { return };
                {
                    let mut set = s.expanded.borrow_mut();
                    set.clear();
                    if open {
                        set.extend(s.app.cfg.borrow().sites.iter().map(|c| c.id.clone()));
                    }
                }
                s.render();
            });
            group.add_action(&act);
        }
        self.root.insert_action_group("sb", Some(&group));
    }

    /// One camera: click to pick, drag onto a tile, right-click or the pencil to rename.
    fn camera_row(&self, cam: Cam, shown: &str, ch: &lapi::Channel) -> adw::ActionRow {
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(shown)).activatable(true).build();
        let local = self.app.cfg.borrow().camera_name(&cam.site, cam.channel).is_some();
        row.set_tooltip_text(Some(&if local { format!("NVR name: {}\n{}", ch.name, ch.ip) } else { ch.ip.clone() }));
        row.add_prefix(&dot(ch.online));
        row.add_prefix(&self.app.thumbnails.widget(&cam));
        if self.app.ptz(&cam).0 {
            row.add_suffix(&gtk::Label::builder().label("PTZ").css_classes(["dim-label", "caption"]).build());
        }
        // Rename button, only visible while the pointer is over the row (or it has focus).
        let edit = gtk::Button::builder().icon_name("document-edit-symbolic").tooltip_text("Rename…").css_classes(["flat", "circular"]).valign(gtk::Align::Center).opacity(0.0).build();
        row.add_suffix(&edit);
        let hover = gtk::EventControllerMotion::new();
        let e = edit.clone();
        hover.connect_enter(move |_, _, _| e.set_opacity(1.0));
        let e = edit.clone();
        hover.connect_leave(move |_| if !e.has_focus() { e.set_opacity(0.0) });
        row.add_controller(hover);
        let focus = gtk::EventControllerFocus::new();
        let e = edit.clone();
        focus.connect_enter(move |_| e.set_opacity(1.0));
        let e = edit.clone();
        focus.connect_leave(move |_| e.set_opacity(0.0));
        row.add_controller(focus);
        let (a, c) = (self.app.clone(), cam.clone());
        edit.connect_clicked(move |_| camera_name::rename(&a, &c));
        if !ch.online {
            row.add_css_class("dim-label");
        }
        row.set_widget_name(&format!("{}\u{1f}{}", cam.site, cam.channel));

        // Right-click (or long-press) menu, built when asked for.
        let show_menu = {
            let (app, cam, row) = (self.app.clone(), cam.clone(), row.downgrade());
            move |x: f64, y: f64| {
                if let Some(row) = row.upgrade() {
                    camera_menu(&app, &cam, &row, local, x, y);
                }
            }
        };
        let show_menu = Rc::new(show_menu);
        let right = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
        let m = show_menu.clone();
        right.connect_pressed(move |g, _, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            m(x, y);
        });
        row.add_controller(right);
        let long = gtk::GestureLongPress::builder().touch_only(true).build();
        long.connect_pressed(move |_, x, y| show_menu(x, y));
        row.add_controller(long);

        // Drag onto a Live tile.
        let drag = gtk::DragSource::builder().actions(gdk::DragAction::COPY).build();
        let payload = tile::cam_to_drag(&cam);
        drag.connect_prepare(move |_, _, _| Some(gdk::ContentProvider::for_value(&payload.to_value())));
        let r = row.downgrade();
        drag.connect_drag_begin(move |src, _| {
            if let Some(r) = r.upgrade() {
                src.set_icon(Some(&gtk::WidgetPaintable::new(Some(&r))), 0, 0);
            }
        });
        row.add_controller(drag);

        let a = self.app.clone();
        row.connect_activated(move |_| a.pick_camera(cam.clone()));
        row
    }

    /// Highlights cameras chosen for Playback.
    pub fn mark_selection(&self) {
        let playback = self.app.current_view() == "playback";
        let sel = self.app.playback.borrow().as_ref().map(|p| p.selection.borrow().clone()).unwrap_or_default();
        let mut stack = vec![self.list.clone().upcast::<gtk::Widget>()];
        while let Some(w) = stack.pop() {
            if let Some(row) = w.downcast_ref::<adw::ActionRow>() {
                let name = row.widget_name();
                if let Some((site, ch)) = name.split_once('\u{1f}') {
                    let chosen = playback && sel.iter().any(|c| c.site == site && c.channel.to_string() == ch);
                    if chosen {
                        row.add_css_class("chosen");
                    } else {
                        row.remove_css_class("chosen");
                    }
                }
            }
            let mut c = w.first_child();
            while let Some(child) = c {
                c = child.next_sibling();
                stack.push(child);
            }
        }
    }
}

/// "Rename…" / "Reset to NVR name" popover at (`x`, `y`) on a camera row.
fn camera_menu(app: &Rc<App>, cam: &Cam, row: &adw::ActionRow, local: bool, x: f64, y: f64) {
    let menu = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    let rename = gtk::Button::builder().label("Rename…").css_classes(["flat"]).build();
    let reset = gtk::Button::builder().label("Reset to NVR name").css_classes(["flat"]).sensitive(local).build();
    let diag = gtk::Button::builder().label("Diagnostics…").css_classes(["flat"]).build();
    let fav = gtk::Button::builder().label(if app.is_favorite(cam) { "Remove from favourites" } else { "Add to favourites" }).css_classes(["flat"]).build();
    for b in [&fav, &rename, &reset, &diag] {
        if let Some(l) = b.child().and_downcast::<gtk::Label>() {
            l.set_xalign(0.0);
        }
        menu.append(b);
    }
    if let Some(al) = app.alerts.borrow().clone() {
        let alert = gtk::CheckButton::builder().label("Alert on motion").active(al.is_watched(cam)).tooltip_text(alerts::TOOLTIP).margin_start(6).build();
        let c = cam.clone();
        alert.connect_toggled(move |b| al.set_watched(&c, b.is_active()));
        menu.append(&alert);
    }
    let pop = gtk::Popover::builder().child(&menu).has_arrow(false).position(gtk::PositionType::Bottom).build();
    pop.set_parent(row);
    pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    // Parented by hand, so unparented by hand once it has closed.
    pop.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    let (a, c, p) = (app.clone(), cam.clone(), pop.clone());
    rename.connect_clicked(move |_| {
        p.popdown();
        camera_name::rename(&a, &c);
    });
    let (a, c, p) = (app.clone(), cam.clone(), pop.clone());
    fav.connect_clicked(move |_| {
        p.popdown();
        a.toggle_favorite(&c);
    });
    let (a, c, p) = (app.clone(), cam.clone(), pop.clone());
    diag.connect_clicked(move |_| {
        p.popdown();
        diagnostics::camera(&a, &c);
    });
    let (a, c, p) = (app.clone(), cam.clone(), pop.clone());
    reset.connect_clicked(move |_| {
        p.popdown();
        camera_name::set(&a, &c, None);
    });
    pop.popup();
}

fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).css_classes(["heading", "dim-label"]).margin_start(6).margin_bottom(-6).build()
}

fn dot(on: bool) -> gtk::Widget {
    gtk::Box::builder()
        .css_classes(["dot", if on { "on" } else { "off" }])
        .valign(gtk::Align::Center)
        .width_request(8)
        .height_request(8)
        .build()
        .upcast()
}
