//! A square cover image with rounded corners that fades in when it arrives.
//! As a record (the Vinyl style's player), the cover is the label of a black
//! disc that turns while music plays.

use std::cell::{Cell, RefCell};

use adw::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

use crate::images;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Cover {
        pub size: Cell<i32>,
        pub radius: Cell<f32>,
        pub url: RefCell<Option<String>>,
        pub texture: RefCell<Option<gdk::Texture>>,
        pub opacity: Cell<f64>,
        pub fade: RefCell<Option<adw::TimedAnimation>>,
        pub record: Cell<bool>,
        /// The record's turn, in degrees.
        pub angle: Cell<f64>,
        /// Drawn this far to the right of where it's laid out (the record
        /// sliding out of its sleeve: drawing only, no relayout).
        pub shift: Cell<f32>,
        pub spin: RefCell<Option<gtk::TickCallbackId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Cover {
        const NAME: &'static str = "OnifyCover";
        type Type = super::Cover;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("cover");
        }
    }

    impl ObjectImpl for Cover {}

    fn circle(center: &graphene::Point, r: f32) -> gsk::RoundedRect {
        let rect = graphene::Rect::new(center.x() - r, center.y() - r, r * 2.0, r * 2.0);
        gsk::RoundedRect::from_rect(rect, r)
    }

    impl Cover {
        /// A black disc with faint grooves, the cover as its label turning
        /// with the record, and light that stays put as it spins.
        fn snapshot_record(&self, snapshot: &gtk::Snapshot, bounds: &graphene::Rect) {
            let center = bounds.center();
            let r = bounds.width() / 2.0;
            snapshot.push_rounded_clip(&circle(&center, r));
            snapshot.append_color(&gdk::RGBA::new(0.045, 0.045, 0.055, 1.0), bounds);
            for (f, alpha) in [(0.94, 0.07), (0.86, 0.05), (0.78, 0.06), (0.70, 0.04)] {
                snapshot.append_border(&circle(&center, r * f), &[0.6; 4], &[gdk::RGBA::new(1.0, 1.0, 1.0, alpha); 4]);
            }

            let label = r * 0.62;
            snapshot.save();
            snapshot.translate(&center);
            snapshot.rotate(self.angle.get() as f32);
            snapshot.translate(&graphene::Point::new(-center.x(), -center.y()));
            snapshot.push_rounded_clip(&circle(&center, label));
            let label_rect = graphene::Rect::new(center.x() - label, center.y() - label, label * 2.0, label * 2.0);
            snapshot.append_color(&gdk::RGBA::new(0.16, 0.16, 0.16, 1.0), &label_rect);
            if let Some(texture) = self.texture.borrow().as_ref() {
                snapshot.push_opacity(self.opacity.get());
                snapshot.append_texture(texture, &label_rect);
                snapshot.pop();
            }
            snapshot.pop();
            snapshot.restore();

            // The spindle hole, and a sheen across the vinyl.
            snapshot.push_rounded_clip(&circle(&center, (r * 0.07).max(1.5)));
            snapshot.append_color(&gdk::RGBA::new(0.02, 0.02, 0.03, 1.0), bounds);
            snapshot.pop();
            snapshot.append_linear_gradient(
                bounds,
                &graphene::Point::new(bounds.x(), bounds.y()),
                &graphene::Point::new(bounds.x() + bounds.width(), bounds.y() + bounds.height()),
                &[
                    gsk::ColorStop::new(0.0, gdk::RGBA::new(1.0, 1.0, 1.0, 0.0)),
                    gsk::ColorStop::new(0.35, gdk::RGBA::new(1.0, 1.0, 1.0, 0.10)),
                    gsk::ColorStop::new(0.5, gdk::RGBA::new(1.0, 1.0, 1.0, 0.0)),
                    gsk::ColorStop::new(0.75, gdk::RGBA::new(1.0, 1.0, 1.0, 0.06)),
                    gsk::ColorStop::new(1.0, gdk::RGBA::new(1.0, 1.0, 1.0, 0.0)),
                ],
            );
            snapshot.pop();
        }
    }

    impl WidgetImpl for Cover {
        fn measure(&self, _: gtk::Orientation, _: i32) -> (i32, i32, i32, i32) {
            let size = self.size.get();
            (size, size, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            // Always a centred square, however much room the parent gives.
            let widget = self.obj();
            let (w, h) = (widget.width() as f32, widget.height() as f32);
            let side = w.min(h);
            let bounds = graphene::Rect::new((w - side) / 2.0, (h - side) / 2.0, side, side);
            let shift = self.shift.get();
            if shift != 0.0 {
                snapshot.save();
                snapshot.translate(&graphene::Point::new(shift, 0.0));
            }
            if self.record.get() {
                self.snapshot_record(snapshot, &bounds);
            } else {
                self.snapshot_cover(snapshot, &bounds);
            }
            if shift != 0.0 {
                snapshot.restore();
            }
        }
    }

    impl Cover {
        fn snapshot_cover(&self, snapshot: &gtk::Snapshot, bounds: &graphene::Rect) {
            let bounds = *bounds;
            let radius = self.radius.get().min(bounds.width() / 2.0);
            snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(bounds, radius));
            let opacity = self.opacity.get();
            if opacity < 1.0 || self.texture.borrow().is_none() {
                snapshot.append_color(&gdk::RGBA::new(0.16, 0.16, 0.16, 1.0), &bounds);
            }
            if let Some(texture) = self.texture.borrow().as_ref() {
                snapshot.push_opacity(opacity);
                snapshot.append_texture(texture, &bounds);
                snapshot.pop();
            }
            snapshot.pop();
        }
    }
}

glib::wrapper! {
    pub struct Cover(ObjectSubclass<imp::Cover>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Cover {
    pub fn new(size: i32, radius: f32) -> Self {
        let cover: Self = glib::Object::new();
        cover.imp().size.set(size);
        cover.imp().radius.set(radius);
        cover.set_halign(gtk::Align::Start);
        cover.set_valign(gtk::Align::Center);
        cover
    }

    /// A circular cover, for artists.
    pub fn round(size: i32) -> Self {
        Self::new(size, size as f32 / 2.0)
    }

    /// Resizes, keeping a round cover round.
    pub fn set_size(&self, size: i32) {
        let imp = self.imp();
        if imp.size.get() == size {
            return;
        }
        if imp.radius.get() * 2.0 >= imp.size.get() as f32 {
            imp.radius.set(size as f32 / 2.0);
        }
        imp.size.set(size);
        self.queue_resize();
    }

    /// Draws it `px` to the right of its place (negative: to the left).
    pub fn set_shift(&self, px: f32) {
        if self.imp().shift.replace(px) != px {
            self.queue_draw();
        }
    }

    /// Draws the cover as a record (see the module docs).
    pub fn set_record(&self, record: bool) {
        self.imp().record.set(record);
        if !record {
            self.set_spinning(false);
        }
        self.queue_draw();
    }

    /// Turns the record while music plays, at 10 rpm, unless animations are
    /// off. Redrawn 24 times a second, not every frame: at 10 rpm that looks
    /// the same, and where GTK can't redraw just the disc (Windows, X11) each
    /// redraw is the whole window, 144 times a second on a 144 Hz screen.
    pub fn set_spinning(&self, on: bool) {
        let imp = self.imp();
        let animations = gtk::Settings::default().is_none_or(|s| s.is_gtk_enable_animations());
        let on = on && imp.record.get() && animations;
        if on == imp.spin.borrow().is_some() {
            return;
        }
        if !on {
            if let Some(id) = imp.spin.take() {
                id.remove();
            }
            return;
        }
        const STEP_US: i64 = 1_000_000 / 24;
        let last = Cell::new(None::<i64>);
        let id = self.add_tick_callback(move |cover, clock| {
            let now = clock.frame_time();
            match last.get() {
                None => last.set(Some(now)),
                Some(before) if now - before >= STEP_US => {
                    last.set(Some(now));
                    let degrees = (now - before) as f64 / 1_000_000.0 * 60.0;
                    let imp = cover.imp();
                    imp.angle.set((imp.angle.get() + degrees) % 360.0);
                    cover.queue_draw();
                }
                Some(_) => {}
            }
            glib::ControlFlow::Continue
        });
        imp.spin.replace(Some(id));
    }

    pub fn set_url(&self, url: Option<&str>) {
        let imp = self.imp();
        if imp.url.borrow().as_deref() == url {
            return;
        }
        imp.url.replace(url.map(str::to_owned));
        if let Some(fade) = imp.fade.take() {
            fade.pause();
        }
        let Some(url) = url else {
            imp.texture.replace(None);
            self.queue_draw();
            return;
        };

        let px = (imp.size.get() * images::scale()) as u32;
        if let Some(texture) = images::cached(url, px) {
            imp.texture.replace(Some(texture));
            imp.opacity.set(1.0);
            self.queue_draw();
            return;
        }

        imp.texture.replace(None);
        imp.opacity.set(0.0);
        self.queue_draw();

        let want = url.to_owned();
        let weak = self.downgrade();
        let still_wanted = {
            let weak = weak.clone();
            let want = want.clone();
            move || {
                weak.upgrade()
                    .is_some_and(|c| c.imp().url.borrow().as_deref() == Some(want.as_str()))
            }
        };
        images::request(url, px, still_wanted, move |texture| {
            if let Some(cover) = weak.upgrade() {
                cover.fade_in(texture);
            }
        });
    }

    fn fade_in(&self, texture: &gdk::Texture) {
        let imp = self.imp();
        imp.texture.replace(Some(texture.clone()));
        if !self.is_mapped() {
            imp.opacity.set(1.0);
            self.queue_draw();
            return;
        }
        let weak = self.downgrade();
        let target = adw::CallbackAnimationTarget::new(move |value| {
            if let Some(cover) = weak.upgrade() {
                cover.imp().opacity.set(value);
                cover.queue_draw();
            }
        });
        let fade = adw::TimedAnimation::new(self, 0.0, 1.0, 180, target);
        fade.set_easing(adw::Easing::EaseOutCubic);
        fade.play();
        imp.fade.replace(Some(fade));
    }
}
