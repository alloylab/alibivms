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

use gtk::prelude::*;

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
