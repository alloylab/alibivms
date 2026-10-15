pub mod alerts;
pub mod camera_name;
pub mod diagnostics;
pub mod download_dialog;
pub mod downloads;
pub mod live;
pub mod playback;
pub mod sidebar;
pub mod sites_dialog;
pub mod tile;
pub mod walls;
pub mod zoom;
pub mod window;

use std::rc::Rc;

use gtk::prelude::*;

use crate::ui::tile::Tile;

/// Lays `tiles` out as the smallest square-ish grid that holds them (1, 2 wide, 2×2, 3×3, 4×4),
/// replacing whatever the grid showed.
pub fn fill_grid(grid: &gtk::Grid, tiles: &[Rc<Tile>]) {
    while let Some(child) = grid.first_child() {
        grid.remove(&child);
    }
    let cols = (tiles.len() as f64).sqrt().ceil().max(1.0) as i32;
    for (i, t) in tiles.iter().enumerate() {
        grid.attach(&t.root, i as i32 % cols, i as i32 / cols, 1, 1);
    }
}

/// A tile callback that runs `f` on `owner` for as long as it lives: the shape of every view's
/// tile wiring, without the view keeping itself alive through its tiles.
pub fn weak_fn<T: 'static>(owner: &Rc<T>, f: impl Fn(&Rc<T>) + 'static) -> Box<dyn Fn()> {
    let w = Rc::downgrade(owner);
    Box::new(move || {
        if let Some(o) = w.upgrade() {
            f(&o);
        }
    })
}

/// A small "?" button whose popover lists how a view is used, instead of a hint line that gets
/// cut off on narrow windows.
pub fn tips_button(tips: &[&str]) -> gtk::MenuButton {
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).margin_top(10).margin_bottom(10).margin_start(12).margin_end(12).build();
    for t in tips {
        body.append(&gtk::Label::builder().label(format!("• {t}")).xalign(0.0).wrap(true).max_width_chars(48).build());
    }
    gtk::MenuButton::builder()
        .icon_name("help-about-symbolic")
        .tooltip_text("Tips")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .popover(&gtk::Popover::builder().child(&body).build())
        .build()
}
