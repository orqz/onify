//! The always-visible playback bar.
//!
//! The progress bar is interpolated locally from the last position the player
//! reported. Instead of redrawing every frame it wakes exactly when the knob
//! reaches the next pixel (or the clock the next second), so it moves as
//! smoothly as the screen can show while costing almost no CPU.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use super::cover::Cover;
use super::glass::Glass;
use super::ctx;
use super::track_row::format_duration;
use crate::spotify::NowPlaying;

#[derive(Default)]
struct State {
    has_track: bool,
    track_uri: String,
    playing: bool,
    /// Position at `at` (monotonic µs).
    position_ms: u32,
    at: i64,
    duration_ms: u32,
    shuffle: bool,
    repeat_context: bool,
    repeat_track: bool,
    liked: bool,
    volume: f64,
    muted_from: Option<f64>,
    seek_to: Option<u32>,
}

pub struct PlayerBar {
    pub widget: Glass,
    /// Hidden when the window is compact: shuffle, repeat and the times.
    pub extras: Vec<gtk::Widget>,
    /// Hidden when the window is narrow; lyrics stays.
    pub volume_controls: Vec<gtk::Widget>,
    pub like: gtk::Button,
    lyrics: gtk::Button,
    cover: Cover,
    title: gtk::Label,
    artists: gtk::Label,
    /// The playing song's menu (right-click its title or artists).
    song_menu: gtk::PopoverMenu,
    /// Title and artists; fades in with each new song, like the cover.
    text: gtk::Box,
    text_fade: RefCell<Option<adw::TimedAnimation>>,
    shuffle: gtk::Button,
    play: gtk::Button,
    repeat: gtk::Button,
    elapsed: gtk::Label,
    progress: gtk::Scale,
    total: gtk::Label,
    volume_button: gtk::Button,
    /// Spotify Connect: pick the device to play on.
    devices: gtk::MenuButton,
    /// "Playing on Kitchen Speaker", while another device plays.
    playing_on: gtk::Label,
    volume: gtk::Scale,
    /// In narrow windows the slider hides; hovering the volume icon pops up
    /// this one instead.
    volume_pop: gtk::Popover,
    pop_volume: gtk::Scale,
    pop_close: RefCell<Option<glib::SourceId>>,
    state: RefCell<State>,
    /// Other shuffle toggles (on playlist pages) that light up with this one.
    shuffle_buttons: RefCell<Vec<glib::WeakRef<gtk::Button>>>,
    tick: RefCell<Option<glib::SourceId>>,
    seek_debounce: RefCell<Option<glib::SourceId>>,
    /// Spotify hears of volume changes once the slider rests.
    volume_debounce: RefCell<Option<glib::SourceId>>,
    /// When the user last moved the volume (monotonic µs); echoes from
    /// Spotify within a moment of that are stale and ignored.
    volume_touched: Cell<i64>,
    /// A volume the user chose that Spotify hasn't confirmed yet. Spotify
    /// ignores changes while nothing plays here, then reports its own value
    /// when playback starts; until it echoes this one, ours wins.
    pending_volume: Cell<Option<u16>>,
    dragging: Cell<bool>,
}

/// The cover fills the capsule's height inside its padding (see style.css).
const COVER: i32 = 46;
/// The same height with the Vinyl style's thinner padding.
const RECORD: i32 = 54;

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
    button.add_css_class("flat");
    button.add_css_class("circular");
    button.set_valign(gtk::Align::Center);
    button
}

fn now_us() -> i64 {
    glib::monotonic_time()
}

fn volume_icon(value: f64) -> &'static str {
    match value {
        v if v <= 0.0 => "onify-audio-volume-muted-symbolic",
        v if v < 0.34 => "onify-audio-volume-low-symbolic",
        v if v < 0.67 => "onify-audio-volume-medium-symbolic",
        _ => "onify-audio-volume-high-symbolic",
    }
}

impl PlayerBar {
    pub fn new() -> Rc<Self> {
        // Three equal columns, like Spotify: the controls stay centred no
        // matter how long the song title is. Corners are concentric: the
        // capsule's 21px radius minus its 8px padding is the cover's 13px.
        let widget = Glass::new(gtk::Orientation::Horizontal, 12, 21.0);
        widget.set_homogeneous(true);
        widget.add_css_class("player-bar");

        // Left: what's playing.
        let side_start = gtk::Box::builder().spacing(12).build();
        let cover = Cover::new(COVER, 13.0);
        cover.set_cursor_from_name(Some("pointer"));
        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .spacing(2)
            .build();
        let title = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        title.add_css_class("now-title");
        let artists = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        artists.add_css_class("now-artists");
        text.append(&title);
        text.append(&artists);
        let song_menu = gtk::PopoverMenu::from_model(None::<&gio::MenuModel>);
        song_menu.set_parent(&text);
        song_menu.set_has_arrow(false);
        let like = icon_button("onify-heart-symbolic", "Save to Liked Songs");
        like.set_visible(false);
        side_start.append(&cover);
        side_start.append(&text);
        side_start.append(&like);

        // Center: transport and progress.
        let center = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .build();
        let buttons = gtk::Box::builder().spacing(8).halign(gtk::Align::Center).build();
        let shuffle = icon_button("onify-media-playlist-shuffle-symbolic", "Shuffle");
        let prev = icon_button("onify-media-skip-backward-symbolic", "Previous");
        let play = gtk::Button::from_icon_name("onify-media-playback-start-symbolic");
        play.set_tooltip_text(Some("Play"));
        play.add_css_class("play-button");
        play.add_css_class("circular");
        play.set_valign(gtk::Align::Center);
        let next = icon_button("onify-media-skip-forward-symbolic", "Next");
        let repeat = icon_button("onify-media-playlist-repeat-symbolic", "Repeat");
        for b in [&shuffle, &prev, &play, &next, &repeat] {
            buttons.append(b);
        }
        let timeline = gtk::Box::builder().spacing(8).build();
        let elapsed = gtk::Label::builder().label("0:00").width_chars(5).xalign(1.0).build();
        let total = gtk::Label::builder().label("0:00").width_chars(5).xalign(0.0).build();
        for l in [&elapsed, &total] {
            l.add_css_class("numeric");
            l.add_css_class("caption");
            l.add_css_class("dim-label");
        }
        let progress = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1000.0);
        progress.set_hexpand(true);
        progress.add_css_class("progress");
        progress.set_sensitive(false);
        timeline.append(&elapsed);
        timeline.append(&progress);
        timeline.append(&total);
        center.append(&buttons);
        center.append(&timeline);

        // Right: where it plays, lyrics and volume.
        let side_end = gtk::Box::builder().spacing(4).halign(gtk::Align::End).build();
        let playing_on = gtk::Label::builder()
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(22)
            .visible(false)
            .build();
        playing_on.add_css_class("playing-on");
        side_end.append(&playing_on);
        let devices = gtk::MenuButton::builder()
            .icon_name("onify-devices-symbolic")
            .tooltip_text("Connect to a device")
            .valign(gtk::Align::Center)
            .popover(&super::devices::picker())
            .build();
        devices.add_css_class("flat");
        devices.add_css_class("circular");
        devices.add_css_class("devices-button");
        side_end.append(&devices);
        let lyrics = icon_button("onify-lyrics-symbolic", "Lyrics");
        lyrics.connect_clicked(|_| super::toggle_lyrics());
        side_end.append(&lyrics);
        let volume_button = icon_button("onify-audio-volume-high-symbolic", "Mute");
        let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
        volume.set_width_request(96);
        volume.add_css_class("progress");
        side_end.append(&volume_button);
        side_end.append(&volume);
        let pop_volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
        pop_volume.set_width_request(160);
        pop_volume.add_css_class("progress");
        let volume_pop = gtk::Popover::builder()
            .child(&pop_volume)
            .position(gtk::PositionType::Top)
            .autohide(false)
            .has_arrow(false)
            .build();
        volume_pop.add_css_class("volume-pop");
        volume_pop.set_parent(&volume_button);

        widget.append(&side_start);
        widget.append(&center);
        widget.append(&side_end);

        let extras = vec![shuffle.clone().upcast(), repeat.clone().upcast(), elapsed.clone().upcast(), total.clone().upcast()];
        let volume_controls = vec![volume.clone().upcast()];
        let bar = Rc::new(Self {
            widget,
            extras,
            volume_controls,
            like,
            lyrics,
            cover,
            title,
            artists,
            song_menu,
            text: text.clone(),
            text_fade: RefCell::default(),
            shuffle,
            play,
            repeat,
            elapsed,
            progress,
            total,
            volume_button,
            devices,
            playing_on,
            volume,
            volume_pop,
            pop_volume,
            pop_close: RefCell::default(),
            state: RefCell::default(),
            shuffle_buttons: RefCell::default(),
            tick: RefCell::default(),
            seek_debounce: RefCell::default(),
            volume_debounce: RefCell::default(),
            volume_touched: Cell::new(0),
            pending_volume: Cell::new(None),
            dragging: Cell::new(false),
        });
        bar.connect(&prev, &next);
        bar
    }

    fn connect(self: &Rc<Self>, prev: &gtk::Button, next: &gtk::Button) {
        let weak = Rc::downgrade(self);
        let with = move |f: fn(&PlayerBar)| {
            let weak = weak.clone();
            move |_: &gtk::Button| {
                if let Some(bar) = weak.upgrade() {
                    f(&bar);
                }
            }
        };
        self.play.connect_clicked(with(|bar| bar.toggle_play()));
        prev.connect_clicked(with(|_| super::prev()));
        next.connect_clicked(with(|_| super::next()));
        self.shuffle.connect_clicked(with(|bar| bar.toggle_shuffle()));
        self.repeat.connect_clicked(with(|bar| {
            let (context, track) = {
                let st = bar.state.borrow();
                (st.repeat_context, st.repeat_track)
            };
            // Off → repeat all → repeat one → off.
            let (context, track) = match (context, track) {
                (_, true) => (false, false),
                (true, false) => (true, true),
                (false, false) => (true, false),
            };
            bar.set_repeat(context, track);
            if super::devices::is_remote() {
                super::devices::command("set_repeating_context", serde_json::json!({ "value": context }));
                super::devices::command("set_repeating_track", serde_json::json!({ "value": track }));
            } else {
                ctx().with_engine(|e| e.set_repeat(context, track));
            }
        }));
        self.like.connect_clicked(with(|bar| bar.toggle_like()));
        self.volume_button.connect_clicked(with(|bar| bar.toggle_mute()));

        let open_album = |weak: Weak<Self>| {
            move || {
                if let Some(bar) = weak.upgrade() {
                    let uri = bar.state.borrow().track_uri.clone();
                    if !uri.is_empty() {
                        super::open_album_of(&uri);
                    }
                }
            }
        };
        let click = gtk::GestureClick::new();
        let open = open_album(Rc::downgrade(self));
        click.connect_released(move |_, _, _, _| open());
        self.cover.add_controller(click);
        let open = open_album(Rc::downgrade(self));
        self.title.connect_activate_link(move |_, _| {
            open();
            glib::Propagation::Stop
        });
        self.artists.connect_activate_link(|_, link| {
            if let Some(uri) = super::uri_of_link(link) {
                super::open_uri(&uri);
            }
            glib::Propagation::Stop
        });
        // Right-click: the song's menu, ahead of GTK's text menu for links.
        let menu = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        menu.connect_pressed(glib::clone!(
            #[weak(rename_to = popover)]
            self.song_menu,
            move |gesture, _, x, y| {
                let Some(now) = ctx().now.borrow().clone() else { return };
                gesture.set_state(gtk::EventSequenceState::Claimed);
                let menu = super::track_row::song_menu(&now.uri, &now.name, None, &now.artists, false, None);
                super::track_row::popup_menu(&popover, &menu, x, y);
            }
        ));
        self.text.add_controller(menu);

        // Seeking: follow the knob while it's held, seek once on release.
        let legacy = gtk::EventControllerLegacy::new();
        legacy.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        legacy.connect_event(move |_, event| {
            let Some(bar) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match event.event_type() {
                gdk::EventType::ButtonPress | gdk::EventType::TouchBegin => {
                    bar.dragging.set(true);
                    bar.stop_tick();
                }
                gdk::EventType::ButtonRelease | gdk::EventType::TouchEnd => {
                    bar.dragging.set(false);
                    bar.commit_seek();
                }
                _ => {}
            }
            glib::Propagation::Proceed
        });
        self.progress.add_controller(legacy);

        let weak = Rc::downgrade(self);
        self.progress.connect_change_value(move |_, _, value| {
            let Some(bar) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let duration = bar.state.borrow().duration_ms as f64;
            let ms = value.clamp(0.0, duration) as u32;
            bar.state.borrow_mut().seek_to = Some(ms);
            bar.elapsed.set_label(&format_duration(ms));
            if !bar.dragging.get() {
                // Keyboard or scroll: settle briefly, then seek.
                if let Some(id) = bar.seek_debounce.take() {
                    id.remove();
                }
                let weak = Rc::downgrade(&bar);
                let id = glib::timeout_add_local_once(Duration::from_millis(180), move || {
                    if let Some(bar) = weak.upgrade() {
                        bar.seek_debounce.take();
                        bar.commit_seek();
                    }
                });
                bar.seek_debounce.replace(Some(id));
            }
            glib::Propagation::Proceed
        });

        let weak = Rc::downgrade(self);
        self.volume.connect_change_value(move |_, _, value| {
            if let Some(bar) = weak.upgrade() {
                let value = value.clamp(0.0, 1.0);
                bar.state.borrow_mut().muted_from = None;
                bar.apply_volume(value);
            }
            glib::Propagation::Proceed
        });
        let weak = Rc::downgrade(self);
        self.pop_volume.connect_change_value(move |_, _, value| {
            if let Some(bar) = weak.upgrade() {
                let value = value.clamp(0.0, 1.0);
                bar.state.borrow_mut().muted_from = None;
                bar.apply_volume(value);
            }
            glib::Propagation::Proceed
        });

        // Hover the volume icon (or its pop-up) to keep the pop-up open;
        // only when the inline slider is hidden.
        let hover = |bar: Weak<Self>, entering: bool| {
            move || {
                let Some(bar) = bar.upgrade() else { return };
                if let Some(id) = bar.pop_close.take() {
                    id.remove();
                }
                if entering {
                    if !bar.volume.is_visible() {
                        bar.volume_pop.popup();
                    }
                    return;
                }
                let weak = Rc::downgrade(&bar);
                let id = glib::timeout_add_local_once(Duration::from_millis(350), move || {
                    if let Some(bar) = weak.upgrade() {
                        bar.pop_close.take();
                        bar.volume_pop.popdown();
                    }
                });
                bar.pop_close.replace(Some(id));
            }
        };
        for widget in [self.volume_button.upcast_ref::<gtk::Widget>(), self.volume_pop.upcast_ref()] {
            let motion = gtk::EventControllerMotion::new();
            let enter = hover(Rc::downgrade(self), true);
            let leave = hover(Rc::downgrade(self), false);
            motion.connect_enter(move |_, _, _| enter());
            motion.connect_leave(move |_| leave());
            widget.add_controller(motion);
        }
    }

    pub fn toggle_play(&self) {
        let (has_track, playing) = {
            let st = self.state.borrow();
            (st.has_track, st.playing)
        };
        if !has_track {
            super::resume_or_take_over();
            return;
        }
        // Answer the click immediately; the player confirms a moment later.
        self.set_playing(!playing, self.position_ms());
        if super::devices::is_remote() {
            super::devices::command(if playing { "pause" } else { "resume" }, serde_json::json!({}));
            return;
        }
        ctx().with_engine(|e| if playing { e.pause() } else { e.play() });
    }

    fn commit_seek(&self) {
        let Some(ms) = self.state.borrow_mut().seek_to.take() else {
            self.schedule();
            return;
        };
        {
            let mut st = self.state.borrow_mut();
            st.position_ms = ms;
            st.at = now_us();
        }
        super::seek_to(ms as i64);
        self.schedule();
    }

    fn apply_volume(&self, value: f64) {
        self.state.borrow_mut().volume = value;
        self.pop_volume.set_value(value);
        self.volume_touched.set(now_us());
        self.volume_button.set_icon_name(volume_icon(value));
        // Heard at once; Spotify (and other devices) get one update when the
        // slider stops, instead of one per pixel.
        let volume = (value * u16::MAX as f64).round() as u16;
        if let Some(id) = self.volume_debounce.take() {
            id.remove();
        }
        // Another device playing: the slider is its volume.
        if super::devices::is_remote() {
            let id = glib::timeout_add_local_once(Duration::from_millis(250), move || {
                ctx().bar.volume_debounce.take();
                super::devices::set_volume(volume);
            });
            self.volume_debounce.replace(Some(id));
            return;
        }
        ctx().output.set_volume(volume);
        let id = glib::timeout_add_local_once(Duration::from_millis(250), move || {
            let ctx = ctx();
            ctx.bar.volume_debounce.take();
            ctx.bar.pending_volume.set(Some(volume));
            ctx.with_engine(|e| e.set_volume(volume));
            ctx.settings.borrow_mut().volume = volume;
            ctx.settings.borrow().save();
            super::integrations::volume(volume);
        });
        self.volume_debounce.replace(Some(id));
    }

    fn toggle_mute(&self) {
        let (volume, muted_from) = {
            let st = self.state.borrow();
            (st.volume, st.muted_from)
        };
        let target = match muted_from {
            Some(previous) => previous,
            None if volume > 0.0 => 0.0,
            None => 0.5,
        };
        self.state.borrow_mut().muted_from = (target == 0.0).then_some(volume);
        self.volume.set_value(target);
        self.apply_volume(target);
    }

    fn toggle_like(&self) {
        let (uri, liked) = {
            let st = self.state.borrow();
            (st.track_uri.clone(), !st.liked)
        };
        if uri.is_empty() {
            return;
        }
        self.set_liked(liked);
        super::set_liked(&uri, liked);
    }

    pub fn set_liked(&self, liked: bool) {
        self.state.borrow_mut().liked = liked;
        #[cfg(windows)]
        super::thumbbar::set_liked(liked);
        self.like.set_icon_name(if liked {
            "onify-heart-filled-symbolic"
        } else {
            "onify-heart-symbolic"
        });
        self.like.set_tooltip_text(Some(if liked {
            "Remove from Liked Songs"
        } else {
            "Save to Liked Songs"
        }));
        if liked {
            self.like.add_css_class("active");
        } else {
            self.like.remove_css_class("active");
        }
    }

    pub fn track_uri(&self) -> String {
        self.state.borrow().track_uri.clone()
    }

    pub fn is_playing(&self) -> bool {
        self.state.borrow().playing
    }

    /// Whether a song is loaded in the player (not just last time's song
    /// shown, or nothing).
    pub fn has_track(&self) -> bool {
        self.state.borrow().has_track
    }

    pub fn set_track(&self, now: &NowPlaying) {
        if self.state.borrow().track_uri != now.uri {
            if let Some(fade) = self.text_fade.take() {
                fade.skip();
            }
            let target = adw::PropertyAnimationTarget::new(&self.text, "opacity");
            let fade = adw::TimedAnimation::new(&self.text, 0.0, 1.0, 280, target);
            fade.set_easing(adw::Easing::EaseOutCubic);
            fade.play();
            self.text_fade.replace(Some(fade));
        }
        {
            let mut st = self.state.borrow_mut();
            st.has_track = true;
            st.track_uri = now.uri.clone();
            st.duration_ms = now.duration_ms;
            st.position_ms = 0;
            st.at = now_us();
        }
        self.cover.set_url(now.cover(112));
        // The title opens its album; the link itself is the song's, so
        // copying it shares the song.
        let name = glib::markup_escape_text(&now.name);
        if now.uri.starts_with("spotify:track:") {
            let link = super::track_row::web_link(&now.uri);
            self.title.set_markup(&format!("<a href=\"{}\">{name}</a>", glib::markup_escape_text(&link)));
        } else {
            self.title.set_markup(&name);
        }
        self.title.set_tooltip_text(Some(&now.name));
        let artists: Vec<String> = now
            .artists
            .iter()
            .map(|a| {
                let name = glib::markup_escape_text(&a.name);
                if a.uri.is_empty() {
                    name.to_string()
                } else {
                    format!("<a href=\"{}\">{name}</a>", glib::markup_escape_text(&super::track_row::web_link(&a.uri)))
                }
            })
            .collect();
        self.artists.set_markup(&artists.join(", "));
        self.total.set_label(&format_duration(now.duration_ms));
        self.progress.set_range(0.0, now.duration_ms.max(1) as f64);
        self.progress.set_sensitive(true);
        self.like.set_visible(now.uri.starts_with("spotify:track:"));
        self.set_liked(false);
        let uri = now.uri.clone();
        if self.like.is_visible() {
            super::check_liked(&uri);
        }
        self.schedule();
    }

    /// Last time's song, shown paused at `position_ms`; Play picks it up
    /// from there (ui::resume_or_take_over).
    pub fn set_resumable(&self, now: &NowPlaying, position_ms: u32) {
        self.set_track(now);
        self.state.borrow_mut().has_track = false;
        self.set_playing(false, position_ms);
    }

    pub fn set_playing(&self, playing: bool, position_ms: u32) {
        // A little pop when it flips between play and pause.
        if self.state.borrow().playing != playing {
            self.play.remove_css_class("pop");
            self.play.add_css_class("pop");
            let play = self.play.downgrade();
            glib::timeout_add_local_once(Duration::from_millis(320), move || {
                if let Some(play) = play.upgrade() {
                    play.remove_css_class("pop");
                }
            });
        }
        {
            let mut st = self.state.borrow_mut();
            st.playing = playing;
            st.position_ms = position_ms;
            st.at = now_us();
        }
        self.play.set_icon_name(if playing {
            "onify-media-playback-pause-symbolic"
        } else {
            "onify-media-playback-start-symbolic"
        });
        self.play.set_tooltip_text(Some(if playing { "Pause" } else { "Play" }));
        self.cover.set_spinning(playing);
        self.schedule();
    }

    /// The Vinyl style: the cover becomes a record, a little larger since
    /// there's no capsule padding around it (style.css), turning while
    /// music plays.
    pub fn set_record(&self, on: bool) {
        self.cover.set_size(if on { RECORD } else { COVER });
        self.cover.set_record(on);
        self.cover.set_spinning(self.is_playing());
    }

    pub fn set_position(&self, position_ms: u32) {
        {
            let mut st = self.state.borrow_mut();
            st.position_ms = position_ms;
            st.at = now_us();
        }
        self.schedule();
    }

    /// Playback ended or moved to another device; Play takes it back.
    pub fn stopped(&self) {
        let position = self.position_ms();
        self.state.borrow_mut().has_track = false;
        self.set_playing(false, position);
    }

    /// The volume the user chose, 0 to 1.
    pub fn volume(&self) -> f64 {
        self.state.borrow().volume
    }

    /// A volume change from outside the slider: keys or media controls.
    /// Developer aid (ONIFY_DEV): opens the narrow-window volume pop-up.
    pub fn show_volume_pop(&self) {
        self.volume_pop.popup();
    }

    pub fn set_volume_by_user(&self, value: f64) {
        let value = value.clamp(0.0, 1.0);
        self.state.borrow_mut().muted_from = None;
        self.volume.set_value(value);
        self.apply_volume(value);
    }

    pub fn set_lyrics_open(&self, open: bool) {
        if open {
            self.lyrics.add_css_class("active");
        } else {
            self.lyrics.remove_css_class("active");
        }
    }

    pub fn toggle_shuffle(&self) {
        let shuffle = !self.state.borrow().shuffle;
        self.set_shuffle(shuffle);
        if super::devices::is_remote() {
            super::devices::command("set_shuffling_context", serde_json::json!({ "value": shuffle }));
        } else {
            ctx().with_engine(|e| e.set_shuffle(shuffle));
        }
    }

    /// Another device is playing (its name), or this one again (None).
    pub fn set_remote(&self, device: Option<&str>) {
        match device {
            Some(name) => {
                self.playing_on.set_label(&format!("Playing on {name}"));
                self.playing_on.set_tooltip_text(Some(&format!("Playing on {name}")));
                self.playing_on.set_visible(true);
                self.devices.add_css_class("active");
            }
            None => {
                self.playing_on.set_visible(false);
                self.devices.remove_css_class("active");
            }
        }
    }

    /// Lights `button` up whenever shuffle is on.
    pub fn add_shuffle_button(&self, button: &gtk::Button) {
        let mut buttons = self.shuffle_buttons.borrow_mut();
        buttons.retain(|b| b.upgrade().is_some());
        buttons.push(button.downgrade());
        drop(buttons);
        let shuffle = self.state.borrow().shuffle;
        self.set_shuffle(shuffle);
    }

    pub fn set_shuffle(&self, shuffle: bool) {
        self.state.borrow_mut().shuffle = shuffle;
        let others = self.shuffle_buttons.borrow();
        for button in std::iter::once(self.shuffle.clone()).chain(others.iter().filter_map(|b| b.upgrade())) {
            if shuffle {
                button.add_css_class("active");
            } else {
                button.remove_css_class("active");
            }
        }
    }

    pub fn set_repeat(&self, context: bool, track: bool) {
        {
            let mut st = self.state.borrow_mut();
            st.repeat_context = context;
            st.repeat_track = track;
        }
        self.repeat.set_icon_name(if track {
            "onify-media-playlist-repeat-song-symbolic"
        } else {
            "onify-media-playlist-repeat-symbolic"
        });
        if context || track {
            self.repeat.add_css_class("active");
        } else {
            self.repeat.remove_css_class("active");
        }
    }

    /// Spotify reported `volume`. Returns the volume to insist on instead,
    /// if the user chose one Spotify hasn't caught up with.
    pub fn reported_volume(&self, volume: u16) -> Option<u16> {
        match self.pending_volume.get() {
            Some(pending) if pending != volume => Some(pending),
            _ => {
                self.pending_volume.set(None);
                self.set_volume(volume);
                None
            }
        }
    }

    pub fn set_volume(&self, volume: u16) {
        // Spotify echoes changes back a little later; don't let an old value
        // drag the slider back while it's being moved.
        let recent = now_us() - self.volume_touched.get() < 1_500_000;
        if recent || self.volume_debounce.borrow().is_some() {
            return;
        }
        let value = volume as f64 / u16::MAX as f64;
        self.state.borrow_mut().volume = value;
        self.volume.set_value(value);
        self.pop_volume.set_value(value);
        self.volume_button.set_icon_name(volume_icon(value));
    }

    pub fn position_ms(&self) -> u32 {
        let st = self.state.borrow();
        let mut ms = st.position_ms as i64;
        if st.playing {
            ms += (now_us() - st.at) / 1000;
        }
        ms.clamp(0, st.duration_ms as i64) as u32
    }

    fn stop_tick(&self) {
        if let Some(id) = self.tick.take() {
            id.remove();
        }
    }

    /// Draws the current position and sleeps until it next visibly changes.
    fn schedule(&self) {
        self.stop_tick();
        if self.dragging.get() || self.seek_debounce.borrow().is_some() {
            return;
        }
        let position = self.position_ms();
        let (playing, duration) = {
            let st = self.state.borrow();
            (st.playing, st.duration_ms)
        };
        self.progress.set_value(position as f64);
        self.elapsed.set_label(&format_duration(position));
        if !playing || duration == 0 {
            return;
        }

        let width = self.progress.width().max(1) as f64 * self.progress.scale_factor() as f64;
        let ms_per_px = duration as f64 / width;
        let until_px = ms_per_px - (position as f64 % ms_per_px);
        let until_second = 1000.0 - (position % 1000) as f64;
        let delay = until_px.min(until_second).clamp(4.0, 1000.0) + 1.0;

        let bar = ctx().bar.clone();
        let id = glib::timeout_add_local_once(Duration::from_millis(delay as u64), move || {
            bar.tick.take();
            bar.schedule();
        });
        self.tick.replace(Some(id));
    }
}

impl Drop for PlayerBar {
    fn drop(&mut self) {
        self.stop_tick();
    }
}
