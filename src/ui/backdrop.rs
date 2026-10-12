//! The window background: the playing song's cover, blurred and stretched
//! edge to edge, cross-fading whenever the song changes. It also hands the
//! cover's accent colour to the rest of the UI.
//!
//! The cover, the shading over it and the vignette are four full-window
//! layers, drawn every frame. Where the GPU is weak, or the whole window is
//! redrawn each frame (Windows), that's most of a frame's work, so once the
//! size settles they're composed into one small texture off the UI thread
//! (`bake`) and that's drawn instead, stretched: it's all soft gradients, so
//! a quarter of the resolution looks the same.

use std::cell::{Cell, OnceCell, RefCell};
use std::time::Duration;

use adw::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

use crate::images::{self, BACKDROP, Rgb};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Backdrop {
        pub url: RefCell<Option<String>>,
        pub current: RefCell<Option<gdk::Texture>>,
        pub previous: RefCell<Option<gdk::Texture>>,
        pub mix: Cell<f64>,
        pub fade: RefCell<Option<adw::TimedAnimation>>,
        /// How much the cover is dimmed: see `Mood`.
        pub mood: Cell<super::Mood>,
        /// Off: a plain background, and no cover images kept for it.
        pub covers_off: Cell<bool>,
        /// Covers composed with their shading at a window size and mood.
        pub baked: RefCell<Vec<super::Baked>>,
        pub bake_timer: RefCell<Option<glib::SourceId>>,
        /// The size the pending bake is for.
        pub bake_size: Cell<(i32, i32, i32)>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Backdrop {
        const NAME: &'static str = "OnifyBackdrop";
        type Type = super::Backdrop;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Backdrop {}

    impl WidgetImpl for Backdrop {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let (w, h) = (widget.width() as f32, widget.height() as f32);
            let bounds = graphene::Rect::new(0.0, 0.0, w, h);
            let mix = self.mix.get();
            let previous = self.previous.borrow().clone();
            let current = self.current.borrow().clone();
            let size = (widget.width(), widget.height(), widget.scale_factor());
            let baked = |cover: &gdk::Texture| {
                let mood = self.mood.get();
                let found = self.baked.borrow().iter().find(|b| b.cover == *cover && b.mood == mood && b.size == size).map(|b| b.texture.clone());
                if found.is_none() {
                    widget.bake_soon();
                }
                found
            };
            // The composed covers, when they're ready for this size.
            let ready = match (&previous, &current) {
                (Some(previous), Some(current)) => baked(previous).zip(baked(current)).map(|(p, c)| (Some(p), Some(c))),
                (None, Some(current)) => baked(current).map(|c| (None, Some(c))),
                _ => None,
            };
            let plain = previous.is_none() && (current.is_none() || mix < 1.0);
            if plain {
                // Until a cover arrives (nothing played yet, or covers off):
                // dark grey in the top-left corner fading to black.
                snapshot.append_linear_gradient(
                    &bounds,
                    &graphene::Point::new(0.0, 0.0),
                    &graphene::Point::new(w, h),
                    &[
                        gsk::ColorStop::new(0.0, gdk::RGBA::new(0.19, 0.19, 0.21, 1.0)),
                        gsk::ColorStop::new(0.55, gdk::RGBA::new(0.07, 0.07, 0.08, 1.0)),
                        gsk::ColorStop::new(1.0, gdk::RGBA::new(0.015, 0.015, 0.02, 1.0)),
                    ],
                );
            }
            if let Some((previous, current)) = ready {
                for (texture, alpha) in [(previous, 1.0), (current, mix)] {
                    let Some(texture) = texture else { continue };
                    if alpha <= 0.0 {
                        continue;
                    }
                    snapshot.push_opacity(alpha);
                    snapshot.append_texture(&texture, &bounds);
                    snapshot.pop();
                }
                return;
            }
            if current.is_none() {
                return;
            }

            let draw = |texture: &gdk::Texture, alpha: f64| {
                if alpha <= 0.0 {
                    return;
                }
                // Fill the window, overscanned so the soft edges stay outside.
                let (tw, th) = (texture.width() as f32, texture.height() as f32);
                let scale = (w / tw).max(h / th) * OVERSCAN;
                let (dw, dh) = (tw * scale, th * scale);
                let rect = graphene::Rect::new((w - dw) / 2.0, (h - dh) / 2.0, dw, dh);
                snapshot.push_opacity(alpha);
                snapshot.append_texture(texture, &rect);
                snapshot.pop();
            };
            if let Some(previous) = &previous {
                draw(previous, 1.0);
            }
            if let Some(current) = &current {
                draw(current, mix);
            }

            // Deepen the colour, keep the top airy and the bottom grounded.
            // Over the plain gradient there's nothing to deepen, so this fades
            // in with the first cover. (`bake` does the same.)
            let cover = if previous.is_some() { 1.0 } else { mix as f32 };
            let black = |a: f32| gdk::RGBA::new(SHADE[0], SHADE[1], SHADE[2], a * cover);
            let Shading { top, middle, bottom, edge } = self.mood.get().shading();
            snapshot.append_linear_gradient(
                &bounds,
                &graphene::Point::new(0.0, 0.0),
                &graphene::Point::new(0.0, h),
                &[
                    gsk::ColorStop::new(0.0, black(top)),
                    gsk::ColorStop::new(0.5, black(middle)),
                    gsk::ColorStop::new(1.0, black(bottom)),
                ],
            );
            // Vignette.
            snapshot.append_radial_gradient(
                &bounds,
                &graphene::Point::new(w * VIGNETTE_CENTER.0, h * VIGNETTE_CENTER.1),
                w * VIGNETTE_RADIUS.0,
                h * VIGNETTE_RADIUS.1,
                0.0,
                1.0,
                &[
                    gsk::ColorStop::new(VIGNETTE_FROM, black(0.0)),
                    gsk::ColorStop::new(1.0, black(edge)),
                ],
            );
        }
    }
}

/// The shading's colour, a blue-tinted black.
const SHADE: [f32; 3] = [0.02, 0.02, 0.04];
/// Covers are drawn this much bigger than the window, so their soft edges
/// stay outside it.
const OVERSCAN: f32 = 1.3;
/// The vignette's centre and radii, as fractions of the window's size, and
/// where (from the centre out) it starts to darken.
const VIGNETTE_CENTER: (f32, f32) = (0.5, 0.4);
const VIGNETTE_RADIUS: (f32, f32) = (0.75, 0.9);
const VIGNETTE_FROM: f32 = 0.55;
/// Composed backgrounds are this many times smaller than the window.
const BAKE_DIVISOR: i32 = 4;

/// How dark the shading gets: at the top, middle and bottom of the window,
/// and in the vignette's corners.
#[derive(Clone, Copy)]
struct Shading {
    top: f32,
    middle: f32,
    bottom: f32,
    edge: f32,
}

/// A cover composed with its shading (see the module docs).
pub struct Baked {
    cover: gdk::Texture,
    mood: Mood,
    /// Width, height and scale factor.
    size: (i32, i32, i32),
    texture: gdk::Texture,
}

/// How dim the blurred cover behind everything is. Text stays white in all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mood {
    /// The cover's colours glow.
    Bright,
    #[default]
    Normal,
    /// Deep and moody.
    Dark,
}

impl Mood {
    pub const ALL: [Mood; 3] = [Mood::Bright, Mood::Normal, Mood::Dark];

    fn shading(self) -> Shading {
        let (top, middle, bottom, edge) = match self {
            Mood::Bright => (0.08, 0.18, 0.44, 0.26),
            Mood::Normal => (0.20, 0.34, 0.58, 0.38),
            Mood::Dark => (0.48, 0.60, 0.78, 0.50),
        };
        Shading { top, middle, bottom, edge }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mood::Bright => "bright",
            Mood::Normal => "normal",
            Mood::Dark => "dark",
        }
    }

    pub fn from_name(name: &str) -> Self {
        Self::ALL.into_iter().find(|m| m.name() == name).unwrap_or_default()
    }
}

glib::wrapper! {
    pub struct Backdrop(ObjectSubclass<imp::Backdrop>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

thread_local! {
    static ACCENT_CSS: OnceCell<gtk::CssProvider> = const { OnceCell::new() };
}

impl Backdrop {
    pub fn new() -> Self {
        let backdrop: Self = glib::Object::new();
        backdrop.set_can_target(false);
        backdrop.set_hexpand(true);
        backdrop.set_vexpand(true);
        backdrop
    }

    pub fn set_mood(&self, mood: Mood) {
        self.imp().mood.set(mood);
        self.queue_draw();
    }

    /// Turns the album cover background on or off.
    pub fn set_covers(&self, on: bool) {
        let imp = self.imp();
        imp.covers_off.set(!on);
        if on {
            let url = imp.url.take();
            self.set_cover(url.as_deref());
        } else {
            imp.current.take();
            imp.previous.take();
            if let Some(fade) = imp.fade.take() {
                fade.skip();
            }
            imp.baked.borrow_mut().clear();
        }
        self.queue_draw();
    }

    pub fn set_cover(&self, url: Option<&str>) {
        let imp = self.imp();
        if imp.url.borrow().as_deref() == url {
            return;
        }
        imp.url.replace(url.map(str::to_owned));
        let Some(url) = url else { return };
        if imp.covers_off.get() {
            return;
        }

        if let Some(texture) = images::cached(url, BACKDROP) {
            self.show(&texture, images::accent(url));
            return;
        }
        let want = url.to_owned();
        let weak = self.downgrade();
        let wanted = {
            let weak = weak.clone();
            let want = want.clone();
            move || weak.upgrade().is_some_and(|b| b.imp().url.borrow().as_deref() == Some(want.as_str()))
        };
        images::request(url, BACKDROP, wanted, move |texture| {
            if let Some(backdrop) = weak.upgrade() {
                backdrop.show(texture, images::accent(&want));
            }
        });
    }

    fn show(&self, texture: &gdk::Texture, accent: Option<Rgb>) {
        let imp = self.imp();
        if let Some(fade) = imp.fade.take() {
            fade.skip();
        }
        let old = imp.current.replace(Some(texture.clone()));
        imp.previous.replace(old);
        imp.mix.set(0.0);
        self.forget_old_bakes();
        if let Some(accent) = accent {
            set_accent(accent);
        }

        let weak = self.downgrade();
        let target = adw::CallbackAnimationTarget::new(move |value| {
            if let Some(backdrop) = weak.upgrade() {
                backdrop.imp().mix.set(value);
                backdrop.queue_draw();
            }
        });
        let fade = adw::TimedAnimation::new(self, 0.0, 1.0, 900, target);
        fade.set_easing(adw::Easing::EaseInOutCubic);
        let weak = self.downgrade();
        fade.connect_done(move |_| {
            if let Some(backdrop) = weak.upgrade() {
                // The old cover is fully hidden now; stop drawing it.
                backdrop.imp().previous.replace(None);
                backdrop.forget_old_bakes();
            }
        });
        fade.play();
        imp.fade.replace(Some(fade));
    }
}

impl Backdrop {
    /// Composes the covers showing at the current size and mood, once the
    /// size has held still for a moment (not on every frame of a resize).
    fn bake_soon(&self) {
        let imp = self.imp();
        let size = (self.width(), self.height(), self.scale_factor());
        if imp.bake_timer.borrow().is_some() && imp.bake_size.replace(size) == size {
            return;
        }
        imp.bake_size.set(size);
        if let Some(timer) = imp.bake_timer.take() {
            timer.remove();
        }
        let weak = self.downgrade();
        let timer = glib::timeout_add_local_once(Duration::from_millis(120), move || {
            if let Some(backdrop) = weak.upgrade() {
                backdrop.imp().bake_timer.take();
                backdrop.bake();
            }
        });
        imp.bake_timer.replace(Some(timer));
    }

    fn bake(&self) {
        let imp = self.imp();
        let size = (self.width(), self.height(), self.scale_factor());
        let scale = size.2.max(1);
        if size.0 <= 0 || size.1 <= 0 {
            return;
        }
        let mood = imp.mood.get();
        let covers = [imp.previous.borrow().clone(), imp.current.borrow().clone()];
        for cover in covers.into_iter().flatten() {
            let done = imp.baked.borrow().iter().any(|b| b.cover == cover && b.mood == mood && b.size == size);
            if done {
                continue;
            }
            let (cw, ch) = (cover.width() as usize, cover.height() as usize);
            let mut pixels = vec![0u8; cw * ch * 4];
            let mut downloader = gdk::TextureDownloader::new(&cover);
            downloader.set_format(gdk::MemoryFormat::R8g8b8a8Premultiplied);
            let (bytes, stride) = downloader.download_bytes();
            for row in 0..ch {
                pixels[row * cw * 4..(row + 1) * cw * 4].copy_from_slice(&bytes[row * stride..row * stride + cw * 4]);
            }
            let out = ((size.0 * scale / BAKE_DIVISOR).max(16), (size.1 * scale / BAKE_DIVISOR).max(16));
            let weak = self.downgrade();
            glib::spawn_future_local(async move {
                let shading = mood.shading();
                let job = crate::rt::handle().spawn_blocking(move || compose(&pixels, (cw, ch), (size.0, size.1), out, shading));
                let Ok(composed) = job.await else { return };
                let Some(backdrop) = weak.upgrade() else { return };
                let imp = backdrop.imp();
                // Still wanted: showing, at this size and mood.
                let showing = [imp.previous.borrow().clone(), imp.current.borrow().clone()].contains(&Some(cover.clone()));
                if !showing || imp.mood.get() != mood || (backdrop.width(), backdrop.height(), backdrop.scale_factor()) != size {
                    return;
                }
                let texture = gdk::MemoryTexture::new(
                    out.0,
                    out.1,
                    gdk::MemoryFormat::R8g8b8a8Premultiplied,
                    &glib::Bytes::from_owned(composed),
                    out.0 as usize * 4,
                )
                .upcast::<gdk::Texture>();
                let mut baked = imp.baked.borrow_mut();
                baked.retain(|b| b.cover != cover);
                baked.push(Baked { cover, mood, size, texture });
                drop(baked);
                backdrop.queue_draw();
            });
        }
    }

    /// Bakes of covers no longer showing.
    fn forget_old_bakes(&self) {
        let imp = self.imp();
        let showing = [imp.previous.borrow().clone(), imp.current.borrow().clone()];
        imp.baked.borrow_mut().retain(|b| showing.contains(&Some(b.cover.clone())));
    }
}

/// The cover (`pixels`, RGBA) filling a `window`-sized area as `snapshot`
/// draws it, with the shading and vignette over it, at `out` pixels.
fn compose(pixels: &[u8], (cw, ch): (usize, usize), window: (i32, i32), out: (i32, i32), shading: Shading) -> Vec<u8> {
    let (w, h) = (window.0 as f32, window.1 as f32);
    let (ow, oh) = (out.0 as usize, out.1 as usize);
    let scale = (w / cw as f32).max(h / ch as f32) * OVERSCAN;
    let (dw, dh) = (cw as f32 * scale, ch as f32 * scale);
    let (left, top) = ((w - dw) / 2.0, (h - dh) / 2.0);
    let texel = |x: usize, y: usize, c: usize| pixels[(y.min(ch - 1) * cw + x.min(cw - 1)) * 4 + c] as f32 / 255.0;
    // Linear between the shading's stops, as GSK draws a gradient.
    let vertical = |t: f32| {
        if t < 0.5 {
            shading.top + (shading.middle - shading.top) * (t / 0.5)
        } else {
            shading.middle + (shading.bottom - shading.middle) * ((t - 0.5) / 0.5)
        }
    };
    let (cx, cy) = (w * VIGNETTE_CENTER.0, h * VIGNETTE_CENTER.1);
    let (rx, ry) = (w * VIGNETTE_RADIUS.0, h * VIGNETTE_RADIUS.1);
    let mut composed = vec![0u8; ow * oh * 4];
    for oy in 0..oh {
        let y = (oy as f32 + 0.5) * h / oh as f32;
        let darken = vertical((y / h).clamp(0.0, 1.0));
        // Bilinear, like the GPU's linear filter.
        let v = ((y - top) / dh * ch as f32 - 0.5).max(0.0);
        let (y0, fy) = (v.floor() as usize, v.fract());
        for ox in 0..ow {
            let x = (ox as f32 + 0.5) * w / ow as f32;
            let u = ((x - left) / dw * cw as f32 - 0.5).max(0.0);
            let (x0, fx) = (u.floor() as usize, u.fract());
            let d = (((x - cx) / rx).powi(2) + ((y - cy) / ry).powi(2)).sqrt();
            let vignette = shading.edge * ((d - VIGNETTE_FROM) / (1.0 - VIGNETTE_FROM)).clamp(0.0, 1.0);
            let at = (oy * ow + ox) * 4;
            for c in 0..3 {
                let top_row = texel(x0, y0, c) * (1.0 - fx) + texel(x0 + 1, y0, c) * fx;
                let bottom_row = texel(x0, y0 + 1, c) * (1.0 - fx) + texel(x0 + 1, y0 + 1, c) * fx;
                let mut value = top_row * (1.0 - fy) + bottom_row * fy;
                value = value * (1.0 - darken) + SHADE[c] * darken;
                value = value * (1.0 - vignette) + SHADE[c] * vignette;
                composed[at + c] = (value * 255.0).round().clamp(0.0, 255.0) as u8;
            }
            composed[at + 3] = 255;
        }
    }
    composed
}

/// Recolours accents (progress bar, playing row, toggles, the logo) to match
/// the cover.
fn set_accent([r, g, b]: Rgb) {
    let hex = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    let color = format!("#{:02x}{:02x}{:02x}", hex(r), hex(g), hex(b));
    ACCENT_CSS.with(|css| {
        let provider = css.get_or_init(|| {
            let provider = gtk::CssProvider::new();
            if let Some(display) = gdk::Display::default() {
                gtk::style_context_add_provider_for_display(
                    &display,
                    &provider,
                    gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
                );
            }
            provider
        });
        provider.load_from_string(&format!(
            ":root {{ --accent-color: {color}; --accent-bg-color: {color}; --accent-fg-color: #101010; --logo-color: {color}; }}"
        ));
    });
}
