//! Saved video walls: the rules for comparing, resizing and naming them, and the small dialogs
//! the Live toolbar uses to name or delete one. A wall's tiles may come from any site.

use std::rc::Rc;

use adw::prelude::*;

use crate::{
    app::App,
    config::{Config, LiveCam, Wall},
};

/// Grid sizes the live view offers.
pub const LAYOUTS: [usize; 4] = [1, 4, 9, 16];

/// The nearest offered grid that holds at least `n` tiles (16 at most).
pub fn snap_layout(n: usize) -> usize {
    LAYOUTS.iter().copied().find(|&l| l >= n.max(1)).unwrap_or(16)
}

/// Fits a tile list to `n` tiles: assignments that fit are kept, the rest dropped, new ones empty.
pub fn resize(tiles: &[Option<LiveCam>], n: usize) -> Vec<Option<LiveCam>> {
    (0..n).map(|i| tiles.get(i).cloned().flatten()).collect()
}

/// Whether the working grid shows exactly what `wall` holds (same size, cameras and streams).
pub fn grid_matches(layout: usize, live: &[Option<LiveCam>], wall: &Wall) -> bool {
    layout == snap_layout(wall.layout) && resize(live, layout) == resize(&wall.tiles, layout)
}

/// The saved wall the grid was loaded from or last saved as, if it still exists.
pub fn active(cfg: &Config) -> Option<&Wall> {
    let name = cfg.active_wall.as_deref()?;
    cfg.walls.iter().find(|w| w.name == name)
}

/// The grid has been changed since its wall was loaded or saved.
pub fn is_dirty(cfg: &Config) -> bool {
    active(cfg).is_some_and(|w| !grid_matches(cfg.layout, &cfg.live, w))
}

/// Checks a proposed wall name: trimmed, not empty, and not already used by another wall
/// (ignoring case). `except` is the wall being renamed. Returns the trimmed name.
pub fn check_name(name: &str, walls: &[Wall], except: Option<&str>) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Enter a name".into());
    }
    let taken = walls.iter().find(|w| Some(w.name.as_str()) != except && w.name.trim().to_lowercase() == name.to_lowercase());
    if let Some(w) = taken {
        return Err(format!("There is already a wall called “{}”", w.name));
    }
    Ok(name.to_string())
}

/// Asks for a wall name. `except` is the wall being renamed (its own name stays allowed).
/// `done` gets the trimmed, unique name.
pub fn ask_name(app: &Rc<App>, heading: &str, confirm: &str, initial: &str, except: Option<String>, done: impl Fn(String) + 'static) {
    let entry = gtk::Entry::builder().text(initial).placeholder_text("e.g. Loading docks").build();
    let error = gtk::Label::builder().xalign(0.0).css_classes(["caption", "error"]).visible(false).build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.append(&entry);
    body.append(&error);
    let d = adw::AlertDialog::new(Some(heading), Some("A wall keeps the grid size and what each tile shows, from any site."));
    d.set_extra_child(Some(&body));
    d.add_response("cancel", "Cancel");
    d.add_response("ok", confirm);
    d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    d.set_default_response(Some("ok"));
    d.set_close_response("cancel");

    // Re-checked on every keystroke; an empty name disables Save but is not shown as an error
    // until something was typed. The dialog and entry own this closure through their signals,
    // so it must hold them weakly or the dialog never goes away.
    let check = {
        let (app, d, entry, error) = (app.clone(), d.downgrade(), entry.downgrade(), error.downgrade());
        move |show: bool| -> Option<String> {
            let (Some(d), Some(entry), Some(error)) = (d.upgrade(), entry.upgrade(), error.upgrade()) else { return None };
            let r = check_name(&entry.text(), &app.cfg.borrow().walls, except.as_deref());
            d.set_response_enabled("ok", r.is_ok());
            match &r {
                Err(e) if show => {
                    error.set_label(e);
                    error.set_visible(true);
                    entry.add_css_class("error");
                }
                _ => {
                    error.set_visible(false);
                    entry.remove_css_class("error");
                }
            }
            r.ok()
        }
    };
    let check = Rc::new(check);
    check(!initial.trim().is_empty());
    let c = check.clone();
    entry.connect_changed(move |_| {
        c(true);
    });
    let done = Rc::new(done);
    let (c, f, dd) = (check.clone(), done.clone(), d.downgrade());
    entry.connect_activate(move |_| {
        if let Some(name) = c(true) {
            f(name);
            if let Some(d) = dd.upgrade() {
                d.close();
            }
        }
    });
    d.connect_response(Some("ok"), move |_, _| {
        if let Some(name) = check(true) {
            done(name);
        }
    });
    d.present(Some(&app.window));
    entry.grab_focus();
}

/// Asks before deleting a wall; `done` runs on confirmation.
pub fn confirm_delete(app: &Rc<App>, name: &str, done: impl Fn() + 'static) {
    let d = adw::AlertDialog::new(Some(&format!("Delete “{name}”?")), Some("The cameras on screen stay as they are; only the saved wall is removed."));
    d.add_response("cancel", "Cancel");
    d.add_response("delete", "Delete");
    d.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    d.set_close_response("cancel");
    d.connect_response(Some("delete"), move |_, _| done());
    d.present(Some(&app.window));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(site: &str, channel: u32) -> Option<LiveCam> {
        Some(LiveCam { site: site.into(), channel, main: false })
    }

    fn wall(name: &str, layout: usize, tiles: Vec<Option<LiveCam>>) -> Wall {
        Wall { name: name.into(), layout, tiles }
    }

    #[test]
    fn layouts_snap() {
        assert_eq!(snap_layout(0), 1);
        assert_eq!(snap_layout(4), 4);
        assert_eq!(snap_layout(5), 9);
        assert_eq!(snap_layout(99), 16);
    }

    #[test]
    fn resizing_keeps_what_fits() {
        let tiles = vec![cam("hq", 1), None, cam("dock", 3), cam("hq", 4)];
        assert_eq!(resize(&tiles, 1), vec![cam("hq", 1)]);
        let big = resize(&tiles, 9);
        assert_eq!(big.len(), 9);
        assert_eq!(&big[..4], &tiles[..]);
        assert!(big[4..].iter().all(Option::is_none));
        assert_eq!(resize(&[], 4), vec![None; 4]);
    }

    #[test]
    fn dirty_tracking() {
        // No wall loaded: an unsaved layout, not a modified wall.
        let mut c = Config { layout: 4, live: vec![cam("hq", 1), cam("dock", 2)], ..Default::default() };
        assert!(!is_dirty(&c));
        c.walls.push(wall("Mixed", 4, vec![cam("hq", 1), cam("dock", 2), None, None]));
        c.active_wall = Some("Mixed".into());
        // Trailing empty tiles do not count as a difference.
        assert!(!is_dirty(&c));
        c.live[1].as_mut().unwrap().main = true;
        assert!(is_dirty(&c), "HD toggle changes the wall");
        c.live[1].as_mut().unwrap().main = false;
        c.live[1].as_mut().unwrap().site = "hq".into();
        assert!(is_dirty(&c), "same channel on another site is a different camera");
        c.live[1] = cam("dock", 2);
        c.layout = 9;
        assert!(is_dirty(&c));
        // An active name with no matching wall (deleted elsewhere) is just unsaved.
        c.active_wall = Some("Gone".into());
        assert!(active(&c).is_none() && !is_dirty(&c));
    }

    #[test]
    fn names() {
        let walls = vec![wall("Docks", 4, vec![]), wall("Front", 1, vec![])];
        assert_eq!(check_name("  Yard ", &walls, None), Ok("Yard".into()));
        assert!(check_name("   ", &walls, None).is_err());
        assert!(check_name("docks", &walls, None).is_err());
        assert!(check_name(" Front", &walls, None).is_err());
        // Renaming a wall to itself (or a new case of itself) is fine.
        assert_eq!(check_name("DOCKS", &walls, Some("Docks")), Ok("DOCKS".into()));
        assert!(check_name("Front", &walls, Some("Docks")).is_err());
    }
}
