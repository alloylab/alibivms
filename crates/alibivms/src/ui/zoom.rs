//! Digital zoom: a picture that can be magnified around a point and panned. Only what is drawn
//! changes; the paintable (and so snapshots) keeps the full frame. While zoomed, the view stays
//! on the video itself rather than its letterbox bars.

use gtk::{glib, graphene, prelude::*, subclass::prelude::*};

pub const MAX_ZOOM: f64 = 8.0;

mod imp {
    use std::cell::{Cell, OnceCell};

    use super::*;

    pub struct ZoomView {
        pub picture: OnceCell<gtk::Picture>,
        pub zoom: Cell<f64>,
        /// Top-left of the visible region, as a fraction of the video's on-screen size.
        pub offset: Cell<(f64, f64)>,
    }

    impl Default for ZoomView {
        fn default() -> Self {
            Self { picture: OnceCell::new(), zoom: Cell::new(1.0), offset: Cell::new((0.0, 0.0)) }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ZoomView {
        const NAME: &'static str = "AlibiZoomView";
        type Type = super::ZoomView;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for ZoomView {
        fn constructed(&self) {
            self.parent_constructed();
            let picture = gtk::Picture::builder().content_fit(gtk::ContentFit::Contain).hexpand(true).vexpand(true).build();
            picture.set_parent(&*self.obj());
            let _ = self.picture.set(picture);
        }

        fn dispose(&self) {
            if let Some(p) = self.picture.get() {
                p.unparent();
            }
        }
    }

    impl WidgetImpl for ZoomView {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let z = self.zoom.get();
            let Some(picture) = self.picture.get() else { return };
            if z <= 1.0 {
                obj.snapshot_child(picture, snapshot);
                return;
            }
            let (x, y) = obj.origin();
            snapshot.push_clip(&graphene::Rect::new(0.0, 0.0, obj.width() as f32, obj.height() as f32));
            snapshot.scale(z as f32, z as f32);
            snapshot.translate(&graphene::Point::new(-x as f32, -y as f32));
            obj.snapshot_child(picture, snapshot);
            snapshot.pop();
        }
    }
}

glib::wrapper! {
    pub struct ZoomView(ObjectSubclass<imp::ZoomView>) @extends gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for ZoomView {
    fn default() -> Self {
        glib::Object::builder().property("hexpand", true).property("vexpand", true).build()
    }
}

/// Start of the visible window on one axis: inside the video when it is larger than the window,
/// centred on it otherwise. `o` is the wanted start as a fraction of the video's length.
fn axis(start: f64, len: f64, window: f64, o: f64) -> f64 {
    if len >= window { start + (o * len).clamp(0.0, len - window) } else { start + (len - window) / 2.0 }
}

impl ZoomView {
    pub fn picture(&self) -> &gtk::Picture {
        self.imp().picture.get().expect("built in constructed")
    }

    pub fn zoom(&self) -> f64 {
        self.imp().zoom.get()
    }

    /// Where the video is drawn at 1×: (x, y, width, height), letterboxing excluded.
    fn content(&self) -> (f64, f64, f64, f64) {
        let (w, h) = (self.width().max(1) as f64, self.height().max(1) as f64);
        let ratio = self.picture().paintable().map(|p| p.intrinsic_aspect_ratio()).unwrap_or(0.0);
        if ratio <= 0.0 {
            (0.0, 0.0, w, h)
        } else if w / h > ratio {
            ((w - h * ratio) / 2.0, 0.0, h * ratio, h)
        } else {
            (0.0, (h - w / ratio) / 2.0, w, w / ratio)
        }
    }

    /// Top-left of the visible region in unzoomed widget coordinates.
    pub fn origin(&self) -> (f64, f64) {
        let z = self.zoom();
        let (cx, cy, cw, ch) = self.content();
        let (ox, oy) = self.imp().offset.get();
        (axis(cx, cw, self.width() as f64 / z, ox), axis(cy, ch, self.height() as f64 / z, oy))
    }

    /// Zooms to `z` (clamped to 1–8×) keeping the picture point under (`x`, `y`) in place.
    pub fn zoom_at(&self, z: f64, x: f64, y: f64) {
        let old = self.zoom();
        let (gx, gy) = self.origin();
        let z = z.clamp(1.0, MAX_ZOOM);
        self.imp().zoom.set(z);
        self.set_origin(gx + x / old - x / z, gy + y / old - y / z);
    }

    /// Moves the visible region by (`dx`, `dy`) screen pixels from origin `from`.
    pub fn pan_from(&self, from: (f64, f64), dx: f64, dy: f64) {
        let z = self.zoom();
        self.set_origin(from.0 - dx / z, from.1 - dy / z);
    }

    pub fn reset(&self) {
        self.imp().zoom.set(1.0);
        self.imp().offset.set((0.0, 0.0));
        self.set_cursor_from_name(None);
        self.queue_draw();
    }

    fn set_origin(&self, x: f64, y: f64) {
        let (cx, cy, cw, ch) = self.content();
        self.imp().offset.set(((x - cx) / cw, (y - cy) / ch));
        // Store the clamped position so a pan past the edge does not have to be undone.
        let (x, y) = self.origin();
        self.imp().offset.set(((x - cx) / cw, (y - cy) / ch));
        self.set_cursor_from_name((self.zoom() > 1.0).then_some("grab"));
        self.queue_draw();
    }
}
