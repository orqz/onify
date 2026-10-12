//! The window, navigation and the glue between the UI and the engine.

mod backdrop;
mod cover;
mod devices;
#[cfg(windows)]
mod frame;
mod glass;
mod integrations;
mod lyrics;
mod pages;
mod player_bar;
mod playlists;
mod preferences;
#[cfg(windows)]
mod thumbbar;
mod track_row;
#[cfg(windows)]
mod tray;
mod updater;
mod zoom;

#[cfg(windows)]
pub use frame::hand_over_to_running;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use librespot_core::authentication::Credentials;
use librespot_core::error::ErrorKind;
use tokio::sync::mpsc::UnboundedSender;

use crate::api::{Api, Card, Images, Kind};
use crate::audio::Output;
use crate::settings::Settings;
use crate::spotify::{self, Engine, Event, NowPlaying};
use crate::{images, rt};
use backdrop::Backdrop;
use glass::{Glass, GlassHost};
use cover::Cover;
use pages::{HomePage, Loaded, SearchPage};
use player_bar::PlayerBar;

pub const APP_ID: &str = "io.github.orqz.onIfy";
const DISCORD_SERVER: &str = "https://discord.gg/WPKMx4gmwp";
const REPO: &str = "https://github.com/orqz/onify";

#[derive(Clone)]
enum Route {
    Home,
    Search,
    Card(Card),
}

impl Route {
    fn tag(&self) -> String {
        match self {
            Route::Home => "home".into(),
            Route::Search => "search".into(),
            Route::Card(card) if card.kind == Kind::Liked => "liked".into(),
            Route::Card(card) => card.uri.clone(),
        }
    }
}

/// Home, Search, Liked Songs and Local Files; library rows follow these.
const FIXED_ROWS: usize = 4;
/// Room kept under the pages for the floating player: its 62px capsule, its
/// 12px bottom gap and 8px more, so nothing ever sits behind it.
const PLAYER_SPACE: i32 = 82;

struct Login {
    widget: adw::ToolbarView,
    button: gtk::Button,
    status: gtk::Label,
}

pub struct Ctx {
    app: adw::Application,
    window: adw::ApplicationWindow,
    zoom: zoom::Zoom,
    toasts: adw::ToastOverlay,
    backdrop: Backdrop,
    host: GlassHost,
    root: gtk::Stack,
    login: Login,
    split: adw::NavigationSplitView,
    nav: adw::NavigationView,
    sidebar: gtk::ListBox,
    side: Sidebar,
    sidebar_routes: RefCell<Vec<Option<Route>>>,
    pub bar: Rc<PlayerBar>,
    home: HomePage,
    search: SearchPage,
    engine: RefCell<Option<Arc<Engine>>>,
    api: RefCell<Option<Api>>,
    output: Arc<Output>,
    events: UnboundedSender<Event>,
    generation: Cell<u64>,
    reconnect_attempts: Cell<u32>,
    pub stores: RefCell<HashMap<String, Rc<Loaded>>>,
    settings: RefCell<Settings>,
    save_pending: Cell<bool>,
    /// The local folders changed while music played; restart the engine at
    /// the next pause so it picks them up.
    engine_stale: Cell<bool>,
    last_unavailable: Cell<Option<std::time::Instant>>,
    /// Songs in a row that wouldn't load (see Event::Unavailable).
    unavailable_streak: Cell<u32>,
    #[cfg(target_os = "linux")]
    mpris: RefCell<Option<Rc<mpris_server::Player>>>,
    now: RefCell<Option<NowPlaying>>,
    /// The song to pick up from, saved as it plays (see crate::resume).
    last_played: RefCell<Option<crate::resume::LastPlayed>>,
    /// Play was pressed on it before Spotify had connected.
    resume_pending: Cell<bool>,
    /// Next or Previous pressed with nothing loaded: done once the song from
    /// last time (or another device's) plays, if that's within 15 s.
    skip_after_resume: Cell<Option<(Skip, std::time::Instant)>>,
    /// Saves the position every few seconds while music plays.
    remember_tick: RefCell<Option<glib::SourceId>>,
    /// A link to open once the library has loaded.
    pending_link: RefCell<Option<String>>,
    /// What the sidebar and Home show, to skip redrawing identical data.
    shown_library: RefCell<Option<Vec<Card>>>,
    shown_home: RefCell<Option<crate::api::Home>>,
}

thread_local! {
    static CTX: OnceCell<Rc<Ctx>> = const { OnceCell::new() };
}

pub fn ctx() -> Rc<Ctx> {
    CTX.with(|c| c.get().expect("UI not built yet").clone())
}

impl Ctx {
    pub fn with_engine(&self, f: impl FnOnce(&Engine)) {
        if let Some(engine) = self.engine.borrow().as_ref() {
            f(engine);
        }
    }

    pub fn api(&self) -> Option<Api> {
        self.api.borrow().clone()
    }

    /// The context URI that plays the user's Liked Songs.
    pub fn collection_uri(&self) -> String {
        let user = self
            .engine
            .borrow()
            .as_ref()
            .map(|e| e.session.username())
            .unwrap_or_default();
        format!("spotify:user:{user}:collection")
    }
}

/// The logged-in user's Spotify username (empty before connecting).
pub fn username() -> String {
    ctx().engine.borrow().as_ref().map(|e| e.session.username()).unwrap_or_default()
}

/// The page for `uri` went away (a deleted playlist): if it's showing, Home.
pub fn left_page(uri: &str) {
    if stack_has(&ctx().nav, uri) {
        navigate(Route::Home, true);
    }
}

pub fn toast(message: &str) {
    log::info!("toast: {message}");
    let toast = adw::Toast::new(message);
    toast.set_use_markup(false);
    toast.set_timeout(3);
    ctx().toasts.add_toast(toast);
}

fn spawn_local(fut: impl Future<Output = ()> + 'static) {
    glib::spawn_future_local(fut);
}

#[cfg(target_os = "linux")]
fn with_mpris<F, Fut>(f: F)
where
    F: FnOnce(Rc<mpris_server::Player>) -> Fut,
    Fut: Future<Output = ()> + 'static,
{
    let player = ctx().mpris.borrow().clone();
    if let Some(player) = player {
        spawn_local(f(player));
    }
}

pub fn startup(_: &adw::Application) {
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    // Windows and macOS take the taskbar/dock icon from here.
    gtk::Window::set_default_icon_name(APP_ID);
    // The sidebar already shows onify's logo; drop the second, smaller one
    // some desktops add beside the window buttons.
    if let Some(settings) = gtk::Settings::default() {
        strip_window_icon(&settings);
        settings.connect_gtk_decoration_layout_notify(strip_window_icon);
        #[cfg(windows)]
        windows_fonts(&settings);
    }
}

/// Windows' own UI font, Segoe UI. Windows hands GTK its 9pt menu size, while
/// onify is laid out for 11pt (what Linux uses).
#[cfg(windows)]
fn windows_fonts(settings: &gtk::Settings) {
    settings.set_gtk_font_name(Some("Segoe UI 11"));
    settings.set_gtk_font_rendering(gtk::FontRendering::Manual);
    settings.set_gtk_hint_font_metrics(true);
    settings.set_gtk_xft_antialias(1);
    settings.set_gtk_xft_hinting(1);
    settings.set_gtk_xft_hintstyle(Some("hintfull"));
}

fn strip_window_icon(settings: &gtk::Settings) {
    let Some(layout) = settings.gtk_decoration_layout() else { return };
    if !layout.contains("icon") {
        return;
    }
    let sides: Vec<String> = layout
        .split(':')
        .map(|side| side.split(',').filter(|b| *b != "icon").collect::<Vec<_>>().join(","))
        .collect();
    settings.set_gtk_decoration_layout(Some(&sides.join(":")));
}

pub fn activate(app: &adw::Application) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }

    let settings = Settings::load();
    let output = Output::new(settings.volume);
    let (events, mut event_rx) = tokio::sync::mpsc::unbounded_channel();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("onify")
        .default_width(1280)
        .default_height(820)
        .build();
    // Windows draws the frame, shadow and rounded corners instead of GTK
    // (frame.rs), so GTK's invisible resize border around the window goes.
    #[cfg(windows)]
    {
        window.set_decorated(false);
        window.add_css_class("platform-windows");
        window.connect_realize(|window| frame::install(window.upcast_ref()));
    }

    let login = build_login();
    let home = HomePage::new();
    let search = SearchPage::new();
    let nav = adw::NavigationView::new();
    nav.add(&home.page);
    nav.add(&search.page);
    nav.replace_with_tags(&["home"]);
    nav.set_margin_bottom(PLAYER_SPACE);
    let content = adw::NavigationPage::builder().title("onify").child(&nav).build();
    let (sidebar_page, sidebar, side) = build_sidebar();
    let sidebar_glass = side.glass.clone();
    let split = adw::NavigationSplitView::builder()
        .sidebar(&sidebar_page)
        .content(&content)
        .min_sidebar_width(220.0)
        .max_sidebar_width(300.0)
        .sidebar_width_fraction(0.22)
        .build();
    let bar = PlayerBar::new();

    let root = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .transition_duration(200)
        .build();
    root.add_named(&login.widget, Some("login"));
    root.add_named(&split, Some("main"));
    // The blurred cover sits underneath everything, the pages scroll over it,
    // and the player floats on glass above both.
    let backdrop = Backdrop::new();
    backdrop.set_mood(backdrop::Mood::from_name(&settings.background));
    backdrop.set_covers(settings.cover_background);
    set_animations(settings.animations);
    images::set_low_memory(settings.low_memory);
    let host = GlassHost::new(&backdrop, &root, &bar.widget);
    host.set_lane(&content);
    host.add_panel(&sidebar_glass);
    // The player only shows over the app itself, never the login page (which
    // the stack starts on without notifying).
    bar.widget.set_visible(false);
    root.connect_visible_child_name_notify(glib::clone!(
        #[weak(rename_to = bar)]
        bar.widget,
        move |root| bar.set_visible(root.visible_child_name().as_deref() == Some("main"))
    ));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&host));
    let style = settings.style.clone();
    // Size tiers follow the room the pages get, which zooming changes, so
    // they're judged inside the zoom rather than on the window.
    let sized = adw::BreakpointBin::builder().width_request(300).height_request(480).child(&toasts).build();
    let zoom = zoom::Zoom::new(&sized);
    zoom.set_scale(settings.zoom);
    window.set_content(Some(&zoom));

    // Window size tiers: pages scale their headers, gutters and rows (see
    // pages::Tier). Only the last matching breakpoint applies, so each one
    // carries everything its size needs.
    let medium = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 1100sp or max-height: 720sp").unwrap());
    medium.connect_apply(|bp| set_tier(bp, pages::Tier::Medium));
    medium.connect_unapply(|bp| set_tier(bp, pages::Tier::Large));
    sized.add_breakpoint(medium);
    let narrow = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 760sp").unwrap());
    narrow.connect_apply(|bp| set_tier(bp, pages::Tier::Medium));
    narrow.connect_unapply(|bp| set_tier(bp, pages::Tier::Large));
    narrow.add_setter(&split, "collapsed", Some(&true.to_value()));
    // Collapsed, the sidebar fills the window, so it makes room for the player.
    narrow.add_setter(&sidebar_glass, "margin-bottom", Some(&(PLAYER_SPACE - 12).to_value()));
    for widget in &bar.volume_controls {
        narrow.add_setter(widget, "visible", Some(&false.to_value()));
    }
    narrow.add_setter(&bar.widget, "homogeneous", Some(&false.to_value()));
    sized.add_breakpoint(narrow);
    // Phone-width windows: stack page headers, keep only the core controls.
    let compact = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 560sp").unwrap());
    compact.add_setter(&split, "collapsed", Some(&true.to_value()));
    compact.add_setter(&sidebar_glass, "margin-bottom", Some(&(PLAYER_SPACE - 12).to_value()));
    for widget in &bar.volume_controls {
        compact.add_setter(widget, "visible", Some(&false.to_value()));
    }
    compact.add_setter(&bar.widget, "homogeneous", Some(&false.to_value()));
    for widget in &bar.extras {
        compact.add_setter(widget, "visible", Some(&false.to_value()));
    }
    compact.connect_apply(|bp| set_tier(bp, pages::Tier::Compact));
    compact.connect_unapply(|bp| set_tier(bp, pages::Tier::Large));
    sized.add_breakpoint(compact);

    let ctx = Rc::new(Ctx {
        app: app.clone(),
        window: window.clone(),
        zoom,
        toasts,
        backdrop,
        host,
        root,
        login,
        split,
        nav,
        sidebar,
        side,
        sidebar_routes: RefCell::new(vec![
            Some(Route::Home),
            Some(Route::Search),
            Some(Route::Card(liked_card())),
            Some(Route::Card(local_card())),
        ]),
        bar,
        home,
        search,
        engine: RefCell::default(),
        api: RefCell::default(),
        output,
        events,
        generation: Cell::new(0),
        reconnect_attempts: Cell::new(0),
        stores: RefCell::default(),
        settings: RefCell::new(settings),
        save_pending: Cell::new(false),
        engine_stale: Cell::new(false),
        last_unavailable: Cell::default(),
        unavailable_streak: Cell::new(0),
        #[cfg(target_os = "linux")]
        mpris: RefCell::default(),
        now: RefCell::default(),
        last_played: RefCell::default(),
        resume_pending: Cell::new(false),
        skip_after_resume: Cell::new(None),
        remember_tick: RefCell::default(),
        pending_link: RefCell::default(),
        shown_library: RefCell::default(),
        shown_home: RefCell::default(),
    });
    CTX.with(|c| {
        let _ = c.set(ctx.clone());
    });
    tier_classes(&ctx.window, pages::tier());
    apply_style(&style);
    apply_rail();
    // A narrow window shows the sidebar full width, never as a rail.
    ctx.split.connect_collapsed_notify(|_| apply_rail());

    ctx.bar.set_volume(ctx.settings.borrow().volume);
    // The lyrics button lights up while lyrics are showing.
    ctx.nav.connect_visible_page_notify(|nav| {
        let open = nav.visible_page().and_then(|p| p.tag()).as_deref() == Some(lyrics::TAG);
        self::ctx().bar.set_lyrics_open(open);
    });
    ctx.nav.connect_popped(|_, _| drop_hidden_pages());
    install_actions(app);
    install_keys(&window);
    if let Some(row) = ctx.sidebar.row_at_index(0) {
        ctx.sidebar.select_row(Some(&row));
    }

    spawn_local(async move {
        while let Some(event) = event_rx.recv().await {
            handle_event(event);
        }
    });

    window.connect_close_request(|_| {
        // Windows: closing keeps the music going from the tray icon.
        #[cfg(windows)]
        if self::ctx().settings.borrow().close_to_tray && tray::shown() {
            self::ctx().window.set_visible(false);
            return glib::Propagation::Stop;
        }
        quit();
        glib::Propagation::Stop
    });

    match spotify::cached_credentials() {
        Some(credentials) => {
            ctx.root.set_visible_child_name("main");
            show_cached_library();
            show_last_played();
            connect(credentials);
        }
        None => ctx.root.set_visible_child_name("login"),
    }
    window.present();

    integrations::start(window.upcast_ref());
    #[cfg(windows)]
    tray::start();
    images::prune_disk_cache();
    updater::start();
}

fn liked_card() -> Card {
    Card {
        kind: Kind::Liked,
        uri: "liked".into(),
        name: "Liked Songs".into(),
        subtitle: String::new(),
        images: Images::default(),
        owner: String::new(),
    }
}

fn local_card() -> Card {
    Card {
        kind: Kind::Local,
        uri: "local".into(),
        name: "Local Files".into(),
        subtitle: String::new(),
        images: Images::default(),
        owner: String::new(),
    }
}

fn build_login() -> Login {
    let button = gtk::Button::builder()
        .label("Log in with Spotify")
        .halign(gtk::Align::Center)
        .build();
    button.add_css_class("pill");
    button.add_css_class("suggested-action");
    let status = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).build();
    status.add_css_class("dim-label");
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();
    content.append(&button);
    content.append(&status);
    let page = adw::StatusPage::builder()
        .icon_name(APP_ID)
        .title("onify")
        .description("Spotify, smooth as butter.\nRequires Spotify Premium.")
        .child(&content)
        .vexpand(true)
        .build();
    let header = adw::HeaderBar::new();
    header.set_show_title(false);
    let widget = adw::ToolbarView::new();
    widget.add_top_bar(&header);
    widget.set_content(Some(&page));

    button.connect_clicked(|button| {
        button.set_sensitive(false);
        let ctx = ctx();
        ctx.login.status.set_label("Finish logging in in your browser…");
        spawn_local(async move {
            let credentials =
                rt::spawn(async { tokio::task::spawn_blocking(spotify::login_in_browser).await }).await;
            match credentials {
                Ok(Ok(credentials)) => {
                    self::ctx().login.status.set_label("Connecting…");
                    connect(credentials);
                }
                Ok(Err(e)) => show_login(&format!("Login failed: {e}")),
                Err(e) => show_login(&format!("Login failed: {e}")),
            }
        });
    });
    Login { widget, button, status }
}

fn show_login(message: &str) {
    let ctx = ctx();
    ctx.login.status.set_label(message);
    ctx.login.button.set_sensitive(true);
    ctx.root.set_visible_child_name("login");
}

fn nav_row(icon: &str, name: &str) -> gtk::ListBoxRow {
    let content = gtk::Box::builder().spacing(14).build();
    content.append(&gtk::Image::from_icon_name(icon));
    let label = gtk::Label::builder().label(name).xalign(0.0).build();
    content.append(&label);
    let row = gtk::ListBoxRow::builder().child(&content).tooltip_text(name).build();
    row.add_css_class("nav");
    row
}

fn library_row(card: &Card) -> gtk::ListBoxRow {
    let content = gtk::Box::builder().spacing(12).build();
    let cover = if card.kind == Kind::Artist { Cover::round(42) } else { Cover::new(42, 8.0) };
    cover.set_url(card.images.pick(100));
    content.append(&cover);
    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .spacing(1)
        .build();
    let name = gtk::Label::builder()
        .label(&card.name)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    name.add_css_class("library-title");
    let kind = match card.kind {
        Kind::Album => "Album",
        Kind::Artist => "Artist",
        _ => "Playlist",
    };
    let detail = gtk::Label::builder().label(kind).xalign(0.0).build();
    detail.add_css_class("library-detail");
    text.append(&name);
    text.append(&detail);
    content.append(&text);
    let row = gtk::ListBoxRow::builder().child(&content).build();
    row.add_css_class("library");
    row.set_tooltip_text(Some(&card.name));
    row
}

/// The sidebar's parts that change when it folds into a rail of icons.
struct Sidebar {
    glass: Glass,
    header: adw::HeaderBar,
    brand: gtk::Box,
    names: gtk::Box,
    menu: gtk::MenuButton,
    toggle: gtk::Button,
    /// The settings menu again, at the bottom of the rail.
    rail_menu: gtk::MenuButton,
}

fn build_sidebar() -> (adw::NavigationPage, gtk::ListBox, Sidebar) {
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    list.append(&nav_row("onify-go-home-symbolic", "Home"));
    list.append(&nav_row("onify-system-search-symbolic", "Search"));
    list.append(&nav_row("onify-heart-filled-symbolic", "Liked Songs"));
    list.append(&nav_row("onify-folder-music-symbolic", "Local Files"));
    list.connect_row_activated(|_, row| {
        let route = ctx().sidebar_routes.borrow().get(row.index() as usize).cloned().flatten();
        if let Some(route) = route {
            navigate(route, true);
        }
    });

    let menu = gio::Menu::new();
    let main_section = gio::Menu::new();
    main_section.append(Some("Preferences"), Some("app.preferences"));
    main_section.append(Some("Join the Discord Server"), Some("app.discord"));
    main_section.append(Some("About onify"), Some("app.about"));
    menu.append_section(None, &main_section);
    let session_section = gio::Menu::new();
    session_section.append(Some("Log Out"), Some("app.logout"));
    session_section.append(Some("Quit"), Some("app.quit"));
    menu.append_section(None, &session_section);
    let menu_button = gtk::MenuButton::builder()
        .icon_name("onify-settings-symbolic")
        .menu_model(&menu)
        .tooltip_text("Settings")
        .build();
    let rail_menu = gtk::MenuButton::builder()
        .icon_name("onify-settings-symbolic")
        .menu_model(&menu)
        .tooltip_text("Settings")
        .halign(gtk::Align::Center)
        .margin_bottom(12)
        .visible(false)
        .build();
    rail_menu.add_css_class("rail-menu");
    let toggle = gtk::Button::builder()
        .icon_name("onify-sidebar-symbolic")
        .tooltip_text("Collapse Sidebar (Ctrl+B)")
        .action_name("app.toggle-sidebar")
        .build();
    // Logo centred over the menu icons below it, the name level with their
    // labels.
    let brand = gtk::Box::builder().spacing(8).margin_start(12).build();
    // The logo in the playing cover's colour (style.css, .brand-logo).
    let logo = gtk::Image::from_icon_name("onify-logo-symbolic");
    logo.add_css_class("brand-logo");
    logo.set_pixel_size(28);
    let name = gtk::Label::builder().label("onify").xalign(0.0).build();
    name.add_css_class("brand");
    let version = gtk::Label::builder().label(concat!("v", env!("ONIFY_VERSION"))).xalign(0.0).build();
    version.add_css_class("brand-version");
    let names = gtk::Box::builder().orientation(gtk::Orientation::Vertical).valign(gtk::Align::Center).build();
    names.append(&name);
    names.append(&version);
    brand.append(&logo);
    brand.append(&names);
    let header = adw::HeaderBar::new();
    header.set_show_title(false);
    header.pack_start(&brand);
    header.pack_end(&menu_button);
    header.pack_end(&toggle);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_bottom_bar(&rail_menu);
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list)
        .build();
    pages::glide_wheel(&scroller);
    toolbar.set_content(Some(&scroller));
    toolbar.set_vexpand(true);
    // A floating glass panel; rows scroll inside its rounded corners.
    let glass = Glass::new(gtk::Orientation::Vertical, 0, 24.0);
    glass.add_css_class("sidebar-glass");
    glass.set_overflow(gtk::Overflow::Hidden);
    glass.append(&toolbar);
    // No `.sidebar` class: libadwaita draws a divider line for it.
    let page = adw::NavigationPage::builder().title("onify").child(&glass).build();
    let parts = Sidebar { glass, header, brand, names, menu: menu_button, toggle, rail_menu };
    (page, list, parts)
}

/// Width of the folded sidebar: a column of 42px covers and icons.
const RAIL_WIDTH: f64 = 92.0;

/// Folds the sidebar into a rail of icons and covers, like Spotify's
/// collapsed library, or opens it out again (Settings: sidebar_collapsed).
/// A narrow window shows the sidebar on its own, full width, so it's never a
/// rail then.
fn apply_rail() {
    let ctx = ctx();
    let rail = ctx.settings.borrow().sidebar_collapsed && !ctx.split.is_collapsed();
    let side = &ctx.side;
    if rail {
        ctx.split.set_min_sidebar_width(RAIL_WIDTH);
        ctx.split.set_max_sidebar_width(RAIL_WIDTH);
        side.glass.add_css_class("rail");
    } else {
        ctx.split.set_min_sidebar_width(220.0);
        ctx.split.set_max_sidebar_width(300.0);
        side.glass.remove_css_class("rail");
    }
    side.names.set_visible(!rail);
    side.brand.set_visible(!rail);
    side.menu.set_visible(!rail);
    side.rail_menu.set_visible(rail);
    side.header.set_show_start_title_buttons(!rail);
    // The rail's header holds just the toggle, centred over the icons.
    let centred = side.header.title_widget().as_ref() == Some(side.toggle.upcast_ref());
    if rail && !centred {
        side.header.remove(&side.toggle);
        side.header.set_title_widget(Some(&side.toggle));
        side.header.set_show_title(true);
    } else if !rail && centred {
        side.header.set_title_widget(None::<&gtk::Widget>);
        side.header.set_show_title(false);
        side.header.pack_end(&side.toggle);
    }
    side.toggle.set_tooltip_text(Some(if rail { "Expand Sidebar (Ctrl+B)" } else { "Collapse Sidebar (Ctrl+B)" }));
    let mut row = ctx.sidebar.first_child();
    while let Some(widget) = row {
        if let Some(row) = widget.downcast_ref::<gtk::ListBoxRow>() {
            rail_row(row, rail);
        }
        row = widget.next_sibling();
    }
}

/// One sidebar row as a rail item: just its icon or cover, centred (the
/// "Your Library" heading goes).
fn rail_row(row: &gtk::ListBoxRow, rail: bool) {
    let Some(content) = row.child() else { return };
    if row.has_css_class("heading-row") {
        row.set_visible(!rail);
        return;
    }
    content.set_halign(if rail { gtk::Align::Center } else { gtk::Align::Fill });
    let mut child = content.first_child().and_then(|c| c.next_sibling());
    while let Some(widget) = child {
        widget.set_visible(!rail);
        child = widget.next_sibling();
    }
}

fn fill_sidebar(library: &[Card]) {
    let ctx = ctx();
    let mut routes = ctx.sidebar_routes.borrow_mut();
    while routes.len() > FIXED_ROWS {
        routes.pop();
        if let Some(row) = ctx.sidebar.row_at_index(FIXED_ROWS as i32) {
            ctx.sidebar.remove(&row);
        }
    }
    if library.is_empty() {
        return;
    }
    // "Your Library", with + for a new playlist.
    let heading = gtk::Label::builder().label("Your Library").xalign(0.0).hexpand(true).build();
    heading.add_css_class("sidebar-heading");
    let create = gtk::Button::builder()
        .icon_name("onify-list-add-symbolic")
        .tooltip_text("Create Playlist")
        .valign(gtk::Align::End)
        .action_name("app.new-playlist-with")
        .action_target(&"".to_variant())
        .build();
    create.add_css_class("flat");
    create.add_css_class("circular");
    create.add_css_class("create-playlist");
    let heading_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    heading_box.append(&heading);
    heading_box.append(&create);
    let heading_row = gtk::ListBoxRow::builder()
        .child(&heading_box)
        .activatable(false)
        .selectable(false)
        .build();
    heading_row.add_css_class("heading-row");
    ctx.sidebar.append(&heading_row);
    routes.push(None);
    let rail = ctx.settings.borrow().sidebar_collapsed && !ctx.split.is_collapsed();
    rail_row(&heading_row, rail);
    for card in library {
        let row = library_row(card);
        rail_row(&row, rail);
        ctx.sidebar.append(&row);
        routes.push(Some(Route::Card(card.clone())));
    }
}

/// Pages follow the window's size tier; style.css keys spacing off the
/// window's `tier-medium` / `tier-compact` classes.
fn set_tier(_: &adw::Breakpoint, tier: pages::Tier) {
    pages::set_tier(tier);
    if let Some(ctx) = CTX.with(|c| c.get().cloned()) {
        tier_classes(&ctx.window, tier);
    }
}

fn tier_classes(window: &adw::ApplicationWindow, tier: pages::Tier) {
    window.remove_css_class("tier-medium");
    window.remove_css_class("tier-compact");
    match tier {
        pages::Tier::Medium => window.add_css_class("tier-medium"),
        pages::Tier::Compact => window.add_css_class("tier-compact"),
        pages::Tier::Large => {}
    }
}

/// "vinyl": no glass, panels straight on the cover with a soft shade, the
/// player's cover a turning record, accents taken from the cover (style.css,
/// `window.style-vinyl`). "glass": the liquid glass look.
pub fn apply_style(name: &str) {
    let ctx = ctx();
    let vinyl = name != "glass";
    if vinyl {
        ctx.window.add_css_class("style-vinyl");
    } else {
        ctx.window.remove_css_class("style-vinyl");
    }
    ctx.host.set_shade_only(vinyl);
    ctx.bar.set_record(vinyl);
    pages::set_vinyl(vinyl);
}

thread_local! {
    /// The "Zoom 120%" toast, reused while it shows.
    static ZOOM_TOAST: RefCell<Option<glib::WeakRef<adw::Toast>>> = const { RefCell::new(None) };
}

/// Zoom steps, like a browser's.
const ZOOM_LEVELS: &[f64] = &[0.3, 0.4, 0.5, 0.6, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];

/// Scales the whole window's contents (see zoom.rs); 1.0 is normal size.
pub fn set_zoom(zoom: f64) {
    ctx().zoom.set_scale(zoom);
}

/// Ctrl + (1), Ctrl - (-1) and Ctrl 0 (0), a step at a time.
fn step_zoom(step: i32) {
    let ctx = ctx();
    let current = ctx.settings.borrow().zoom;
    let zoom = match step {
        0 => 1.0,
        1 => ZOOM_LEVELS.iter().copied().find(|&z| z > current + 0.001).unwrap_or(current),
        _ => ZOOM_LEVELS.iter().rev().copied().find(|&z| z < current - 0.001).unwrap_or(current),
    };
    ctx.settings.borrow_mut().zoom = zoom;
    ctx.settings.borrow().save();
    set_zoom(zoom);
    let message = format!("Zoom {}%", (zoom * 100.0).round());
    let shown = ZOOM_TOAST.with_borrow(|t| t.as_ref().and_then(|t| t.upgrade()));
    match shown {
        Some(toast) => toast.set_title(&message),
        None => {
            let toast = adw::Toast::new(&message);
            toast.set_timeout(2);
            toast.connect_dismissed(|_| ZOOM_TOAST.with_borrow_mut(|t| *t = None));
            ZOOM_TOAST.with_borrow_mut(|t| *t = Some(toast.downgrade()));
            ctx.toasts.add_toast(toast);
        }
    }
}

/// Off means onify's own animations and transitions are skipped; on follows
/// the system setting.
pub fn set_animations(on: bool) {
    if let Some(settings) = gtk::Settings::default() {
        if on {
            settings.reset_property("gtk-enable-animations");
        } else {
            settings.set_gtk_enable_animations(false);
        }
    }
}

/// Low memory mode: forget loaded pages that aren't on screen or in the back
/// history. They reopen from the disk cache, so it costs no waiting.
pub fn drop_hidden_pages() {
    let ctx = ctx();
    if !ctx.settings.borrow().low_memory {
        return;
    }
    let nav = ctx.nav.clone();
    ctx.stores.borrow_mut().retain(|key, _| stack_has(&nav, key));
    crate::memory::trim_soon();
}

fn navigate(route: Route, root: bool) {
    let ctx = ctx();
    let tag = route.tag();
    let on_top = ctx.nav.visible_page().and_then(|p| p.tag()).as_deref() == Some(tag.as_str());
    if !on_top {
        if root {
            match &route {
                Route::Home | Route::Search => ctx.nav.replace_with_tags(&[tag.as_str()]),
                Route::Card(card) => ctx.nav.replace(&[page_for(card)]),
            }
        } else if stack_has(&ctx.nav, &tag) {
            ctx.nav.pop_to_tag(&tag);
        } else {
            match &route {
                Route::Home | Route::Search => ctx.nav.push_by_tag(&tag),
                Route::Card(card) => ctx.nav.push(&page_for(card)),
            }
        }
    }
    if ctx.split.is_collapsed() {
        ctx.split.set_show_content(true);
    }
    if root {
        let index = ctx
            .sidebar_routes
            .borrow()
            .iter()
            .position(|r| r.as_ref().is_some_and(|r| r.tag() == tag));
        match index.and_then(|i| ctx.sidebar.row_at_index(i as i32)) {
            Some(row) => ctx.sidebar.select_row(Some(&row)),
            None => ctx.sidebar.unselect_all(),
        }
    }
    if matches!(route, Route::Search) {
        ctx.search.entry.grab_focus();
    }
    drop_hidden_pages();
}

fn stack_has(nav: &adw::NavigationView, tag: &str) -> bool {
    nav.navigation_stack()
        .iter::<adw::NavigationPage>()
        .flatten()
        .any(|p| p.tag().as_deref() == Some(tag))
}

fn page_for(card: &Card) -> adw::NavigationPage {
    match card.kind {
        Kind::Artist => pages::artist_page(card),
        kind => pages::tracks_page(kind, &card.uri, &card.name, &card.images),
    }
}
/// Shows the lyrics, or goes back from them if they're already showing.
pub fn toggle_lyrics() {
    let ctx = ctx();
    let on_top = ctx.nav.visible_page().and_then(|p| p.tag()).as_deref() == Some(lyrics::TAG);
    if on_top {
        ctx.nav.pop();
        return;
    }
    if stack_has(&ctx.nav, lyrics::TAG) {
        ctx.nav.pop_to_tag(lyrics::TAG);
    } else {
        ctx.nav.push(&lyrics::page());
    }
    if ctx.split.is_collapsed() {
        ctx.split.set_show_content(true);
    }
}

pub fn open_card(card: &Card) {
    navigate(Route::Card(card.clone()), false);
}

/// Opens a `spotify:album|artist|playlist:…` URI.
pub fn open_uri(uri: &str) {
    let kind = match uri.split(':').nth(1) {
        Some("album") => Kind::Album,
        Some("artist") => Kind::Artist,
        Some("playlist") => Kind::Playlist,
        _ => return,
    };
    let name = match kind {
        Kind::Album => "Album",
        Kind::Artist => "Artist",
        _ => "Playlist",
    };
    open_card(&Card {
        kind,
        uri: uri.to_owned(),
        name: name.into(),
        subtitle: String::new(),
        images: Images::default(),
        owner: String::new(),
    });
}

pub fn open_album_of(track_uri: &str) {
    if !track_uri.starts_with("spotify:track:") {
        return;
    }
    let Some(api) = ctx().api() else { return };
    let uri = track_uri.to_owned();
    spawn_local(async move {
        match rt::spawn(async move { api.album_of(&uri).await }).await {
            Ok(card) => open_card(&card),
            Err(e) => toast(&format!("Couldn't open album: {e}")),
        }
    });
}

pub fn check_liked(uri: &str) {
    let Some(api) = ctx().api() else { return };
    let uri = uri.to_owned();
    spawn_local(async move {
        let track = uri.clone();
        if let Ok(liked) = rt::spawn(async move { api.is_liked(&track).await }).await {
            let ctx = ctx();
            if ctx.bar.track_uri() == uri {
                ctx.bar.set_liked(liked);
            }
            track_row::set_known_liked([uri], liked);
        }
    });
}

/// Saves a song to (or removes it from) Liked Songs, from the player or a
/// song row; both show the change at once.
pub fn set_liked(uri: &str, liked: bool) {
    let Some(api) = ctx().api() else { return };
    let uri = uri.to_owned();
    if ctx().bar.track_uri() == uri {
        ctx().bar.set_liked(liked);
    }
    track_row::set_known_liked([uri.clone()], liked);
    spawn_local(async move {
        let track = uri.clone();
        let result = rt::spawn(async move { api.set_liked(&track, liked).await }).await;
        let ctx = ctx();
        match result {
            Ok(()) => {
                // Liked Songs reloads fresh next time it's opened.
                ctx.stores.borrow_mut().remove("liked");
                toast(if liked { "Added to Liked Songs" } else { "Removed from Liked Songs" });
            }
            Err(e) => {
                if ctx.bar.track_uri() == uri {
                    ctx.bar.set_liked(!liked);
                }
                track_row::set_known_liked([uri.clone()], !liked);
                toast(&format!("Couldn't update Liked Songs: {e}"));
            }
        }
    });
}

/// Last time's song in the player, paused where it was, ready to play.
fn show_last_played() {
    let Some(last) = crate::resume::load() else { return };
    let ctx = ctx();
    ctx.bar.set_resumable(&last.now, last.position_ms);
    ctx.backdrop.set_cover(last.now.cover(300));
    ctx.now.replace(Some(last.now.clone()));
    integrations::track_changed(&last.now);
    integrations::paused(last.position_ms);
    ctx.last_played.replace(Some(last));
}

/// Play with nothing loaded: the song from last time where it was left
/// (once connected), else whatever Spotify last played on any device.
pub fn resume_or_take_over() {
    let ctx = ctx();
    let Some(engine) = ctx.engine.borrow().clone() else {
        ctx.resume_pending.set(ctx.last_played.borrow().is_some());
        return;
    };
    let last = ctx.last_played.borrow();
    match last.as_ref() {
        Some(last) if last.now.uri == ctx.bar.track_uri() => {
            engine.resume(&last.now.uri, last.context.as_ref(), ctx.bar.position_ms());
        }
        _ => engine.take_over(),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Skip {
    Next,
    Previous,
}

/// Next or Previous. With nothing loaded (just after starting, last time's
/// song only shown), librespot ignores both, so last time's song is loaded
/// first, in its playlist, and the skip happens once it plays.
fn skip(direction: Skip) {
    if devices::is_remote() {
        let endpoint = if direction == Skip::Next { "skip_next" } else { "skip_prev" };
        devices::command(endpoint, serde_json::json!({}));
        return;
    }
    let ctx = ctx();
    if !ctx.bar.has_track() {
        ctx.skip_after_resume.set(Some((direction, std::time::Instant::now())));
        // From the song's start, so Previous goes back a song.
        ctx.bar.set_position(0);
        resume_or_take_over();
        return;
    }
    ctx.with_engine(|e| match direction {
        Skip::Next => e.next(),
        Skip::Previous => e.prev(),
    });
}

/// A skip waiting for playback to start (see `skip`).
fn skip_after_resume() {
    let ctx = ctx();
    let Some((direction, asked)) = ctx.skip_after_resume.take() else { return };
    if asked.elapsed() > Duration::from_secs(15) {
        return;
    }
    ctx.with_engine(|e| match direction {
        Skip::Next => e.next(),
        Skip::Previous => e.prev(),
    });
}

/// A new session starts with nothing loaded, and librespot ignores play,
/// next and previous until something is. So the song showing becomes one to
/// pick up, like last time's song at startup, and if it was playing it
/// carries on once connected.
fn hold_for_new_session(playing: bool) {
    let ctx = ctx();
    if devices::is_remote() || !ctx.bar.has_track() {
        return;
    }
    remember(ctx.bar.position_ms());
    let Some((now, position_ms)) = ctx.last_played.borrow().as_ref().map(|l| (l.now.clone(), l.position_ms)) else { return };
    ctx.bar.set_resumable(&now, position_ms);
    ctx.window.set_title(Some("onify"));
    integrations::paused(position_ms);
    ctx.resume_pending.set(playing);
}

/// Saves the playing song and how far in it is, for next time.
fn remember(position_ms: u32) {
    let ctx = ctx();
    let Some(now) = ctx.now.borrow().clone() else { return };
    let mut last = ctx.last_played.borrow_mut();
    let context = ctx.engine.borrow().as_ref().and_then(|e| e.context()).or_else(|| {
        // Not started from onify (picked up from another device): keep what
        // was known for this song.
        last.as_ref().filter(|l| l.now.uri == now.uri).and_then(|l| l.context.clone())
    });
    let remembered = crate::resume::LastPlayed { now, position_ms, context };
    crate::resume::save(&remembered);
    *last = Some(remembered);
}

/// While music plays, the position is saved every 10 s.
fn remember_while_playing(playing: bool) {
    let ctx = ctx();
    if let Some(id) = ctx.remember_tick.take() {
        id.remove();
    }
    if playing {
        let id = glib::timeout_add_seconds_local(10, || {
            remember(self::ctx().bar.position_ms());
            glib::ControlFlow::Continue
        });
        ctx.remember_tick.replace(Some(id));
    }
}

fn connect(credentials: Credentials) {
    let ctx = ctx();
    let generation = ctx.generation.get() + 1;
    ctx.generation.set(generation);
    let (device_id, folders, quality) = {
        let settings = ctx.settings.borrow();
        (settings.device_id.clone(), settings.local_folders.clone(), spotify::Quality::from_name(&settings.quality))
    };
    let (output, events) = (ctx.output.clone(), ctx.events.clone());
    spawn_local(async move {
        let started =
            rt::spawn(Engine::start(credentials, device_id, quality, folders, output, events, generation)).await;
        let ctx = self::ctx();
        if ctx.generation.get() != generation {
            if let Ok(engine) = started {
                engine.shutdown();
            }
            return;
        }
        match started {
            Ok(engine) => {
                ctx.reconnect_attempts.set(0);
                engine.set_crossfade(ctx.settings.borrow().crossfade);
                let first = ctx.api.borrow().is_none();
                ctx.api.replace(Some(Api::new(engine.session.clone())));
                ctx.engine.replace(Some(engine));
                ctx.root.set_visible_child_name("main");
                if first {
                    load_library();
                }
                if ctx.resume_pending.take() {
                    resume_or_take_over();
                }
                // Last time's song showed before there was a connection to
                // ask whether it's liked.
                let shown = ctx.bar.track_uri();
                if shown.starts_with("spotify:track:") {
                    check_liked(&shown);
                }
            }
            Err(e) => {
                log::warn!("connecting failed: {e}");
                let auth = matches!(e.kind, ErrorKind::Unauthenticated | ErrorKind::PermissionDenied);
                if auth {
                    spotify::forget_credentials();
                    show_login(&format!("Spotify said no: {e}"));
                } else {
                    reconnect();
                }
            }
        }
    });
}

fn reconnect() {
    let ctx = ctx();
    let attempt = ctx.reconnect_attempts.get();
    ctx.reconnect_attempts.set(attempt + 1);
    if attempt == 0 {
        toast("Can't reach Spotify. Retrying…");
    }
    let delay = [1, 2, 5, 10, 30][attempt.min(4) as usize];
    glib::timeout_add_local_once(Duration::from_secs(delay), || {
        if self::ctx().engine.borrow().is_some() {
            return;
        }
        match spotify::cached_credentials() {
            Some(credentials) => connect(credentials),
            None => show_login("Please log in again."),
        }
    });
}

/// The player's settings (streaming quality, local folders) are fixed when it
/// starts, so a change needs a fresh engine. That waits for a pause if music
/// is playing.
pub fn player_settings_changed() {
    let ctx = ctx();
    if ctx.engine.borrow().is_none() {
        return;
    }
    ctx.engine_stale.set(true);
    if !ctx.bar.is_playing() {
        restart_if_stale();
    }
}

/// librespot indexes the local folders when its player starts, so new folders
/// need a fresh engine. That waits for a pause if music is playing.
pub fn local_folders_changed() {
    let ctx = ctx();
    ctx.stores.borrow_mut().remove("local");
    if ctx.engine.borrow().is_none() {
        return;
    }
    ctx.engine_stale.set(true);
    if !ctx.bar.is_playing() {
        restart_if_stale();
    }
}

fn restart_if_stale() {
    let ctx = ctx();
    if !ctx.engine_stale.replace(false) {
        return;
    }
    let Some(credentials) = spotify::cached_credentials() else { return };
    ctx.generation.set(ctx.generation.get() + 1);
    if let Some(engine) = ctx.engine.take() {
        engine.shutdown();
    }
    hold_for_new_session(false);
    // Give the old device a moment to sign off before this one takes its id.
    glib::timeout_add_local_once(Duration::from_millis(300), move || connect(credentials));
}

/// Last time's library and Home from disk, the moment the window opens:
/// connecting to Spotify takes a few seconds.
fn show_cached_library() {
    spawn_local(async {
        let cached = Api::cache_only();
        let (library, home) = rt::spawn(async move { tokio::join!(cached.library(), cached.home()) }).await;
        let ctx = ctx();
        if let Ok(library) = library {
            playlists::library_loaded(&library);
            if ctx.shown_library.borrow().is_none() {
                fill_sidebar(&library);
                ctx.shown_library.replace(Some(library));
            }
        }
        if let Ok(home) = home {
            if ctx.shown_home.borrow().is_none() {
                ctx.home.fill(&home);
                ctx.shown_home.replace(Some(home));
            }
        }
    });
}

/// Spotify's current library and Home, redrawn only where they changed.
fn load_library() {
    let Some(api) = ctx().api() else { return };
    if !api.is_premium() {
        toast("Playback needs Spotify Premium");
    }
    spawn_local(async move {
        let (library, home) = rt::spawn(async move { tokio::join!(api.library(), api.home()) }).await;
        let ctx = ctx();
        match library {
            Ok(library) => {
                playlists::library_loaded(&library);
                let same = ctx.shown_library.borrow().as_deref().is_some_and(|shown| same_cards(shown, &library));
                if !same {
                    fill_sidebar(&library);
                }
                ctx.shown_library.replace(Some(library));
            }
            Err(e) if ctx.shown_library.borrow().is_none() => toast(&format!("Couldn't load your library: {e}")),
            Err(_) => {}
        }
        if let Some(link) = ctx.pending_link.take() {
            open_link(&link);
        }
        match home {
            Ok(home) => {
                let same = ctx.shown_home.borrow().as_ref().is_some_and(|shown| {
                    shown.sections.len() == home.sections.len()
                        && shown.sections.iter().zip(&home.sections).all(|(a, b)| a.0 == b.0 && same_cards(&a.1, &b.1))
                });
                if !same {
                    ctx.home.fill(&home);
                }
                ctx.shown_home.replace(Some(home));
            }
            Err(e) if ctx.shown_home.borrow().is_none() => {
                ctx.home.fill(&Default::default());
                toast(&format!("Couldn't load Home: {e}"));
            }
            Err(_) => {}
        }
    });
}

/// Whether two card lists would look the same.
fn same_cards(a: &[Card], b: &[Card]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| a.uri == b.uri && a.name == b.name && a.images.pick(100) == b.images.pick(100))
}

fn logout() {
    let ctx = ctx();
    ctx.generation.set(ctx.generation.get() + 1);
    if let Some(engine) = ctx.engine.take() {
        engine.shutdown();
    }
    ctx.api.take();
    ctx.stores.borrow_mut().clear();
    ctx.shown_library.take();
    ctx.shown_home.take();
    fill_sidebar(&[]);
    spotify::forget_credentials();
    navigate(Route::Home, true);
    show_login("");
}

fn handle_event(event: Event) {
    let ctx = ctx();
    match event {
        Event::Track(now) => {
            track_row::set_now_playing(&now.uri);
            ctx.bar.set_track(&now);
            ctx.backdrop.set_cover(now.cover(300));
            let artists = crate::api::join_names(&now.artists);
            if ctx.bar.is_playing() {
                ctx.window.set_title(Some(&format!("{} • {artists}", now.name)));
            }
            ctx.now.replace(Some(now.clone()));
            integrations::track_changed(&now);
            lyrics::track_changed(&now.uri);
            remember(0);
        }
        Event::Playing { position_ms } => {
            ctx.bar.set_playing(true, position_ms);
            // Spotify applies its own idea of the volume when playback moves
            // here, and ignored any change made before; the user's choice wins.
            let wanted = (ctx.bar.volume() * u16::MAX as f64).round() as u16;
            if ctx.output.volume() != wanted {
                ctx.output.set_volume(wanted);
                ctx.with_engine(|e| e.set_volume(wanted));
            }
            if let Some(now) = ctx.now.borrow().as_ref() {
                let artists = crate::api::join_names(&now.artists);
                ctx.window.set_title(Some(&format!("{} • {artists}", now.name)));
            }
            integrations::playing(position_ms);
            lyrics::resync();
            remember_while_playing(true);
            skip_after_resume();
        }
        Event::Paused { position_ms } => {
            remember_while_playing(false);
            remember(position_ms);
            ctx.bar.set_playing(false, position_ms);
            ctx.window.set_title(Some("onify"));
            integrations::paused(position_ms);
            lyrics::resync();
            restart_if_stale();
        }
        Event::Position { position_ms } => {
            ctx.bar.set_position(position_ms);
            integrations::seeked(position_ms);
            lyrics::resync();
        }
        Event::Loading => {}
        Event::Stopped => {
            remember_while_playing(false);
            // Playback moved to another device, which the player shows now.
            if devices::is_remote() {
                return;
            }
            ctx.bar.stopped();
            ctx.window.set_title(Some("onify"));
            integrations::paused(0);
            restart_if_stale();
        }
        Event::Shuffle(shuffle) => {
            ctx.bar.set_shuffle(shuffle);
            ctx.with_engine(|e| e.note_shuffle(shuffle));
            integrations::shuffle(shuffle);
        }
        Event::Repeat { context, track } => {
            ctx.bar.set_repeat(context, track);
            ctx.with_engine(|e| e.note_repeat(context));
            integrations::repeat(context, track);
        }
        Event::Volume(volume) => {
            if let Some(wanted) = ctx.bar.reported_volume(volume) {
                ctx.output.set_volume(wanted);
                ctx.with_engine(|e| e.set_volume(wanted));
                return;
            }
            // The bar may have ignored a stale echo; keep what it shows.
            let volume = (ctx.bar.volume() * u16::MAX as f64).round() as u16;
            ctx.settings.borrow_mut().volume = volume;
            if !ctx.save_pending.replace(true) {
                glib::timeout_add_local_once(Duration::from_secs(1), || {
                    let ctx = self::ctx();
                    ctx.save_pending.set(false);
                    ctx.settings.borrow().save();
                });
            }
            integrations::volume(volume);
        }
        Event::Unavailable => {
            // librespot skips on to the next song when one won't load. Several
            // in a row means this session stopped getting song keys from
            // Spotify (it happens after a while); a fresh session gets them at
            // once, so stop skipping through the queue and reconnect.
            let now = std::time::Instant::now();
            let recent = ctx.last_unavailable.get().is_some_and(|t| now.duration_since(t) < Duration::from_secs(10));
            let streak = if recent { ctx.unavailable_streak.get() + 1 } else { 1 };
            ctx.unavailable_streak.set(streak);
            ctx.last_unavailable.set(Some(now));
            match streak {
                1 => toast("This song isn't available"),
                3 => {
                    ctx.with_engine(|e| e.pause());
                    ctx.engine_stale.set(true);
                    restart_if_stale();
                    toast("Spotify stopped sending songs, so onify reconnected. Press play to continue");
                }
                _ => {}
            }
        }
        Event::Disconnected(generation) => {
            if generation == ctx.generation.get() {
                let playing = ctx.bar.is_playing();
                ctx.engine.take();
                hold_for_new_session(playing);
                reconnect();
            }
        }
        Event::Devices(info) => devices::changed(info),
        Event::RemoteTrack(now) => devices::show_remote_track(now),
        Event::NotPremium(generation) => {
            if generation == ctx.generation.get() {
                logout();
                show_login("onify needs Spotify Premium: Spotify only streams to other apps for Premium accounts.");
            }
        }
    }
}

pub fn play_pause() {
    ctx().bar.toggle_play();
}

fn nudge_volume(delta: f64) {
    let bar = ctx().bar.clone();
    bar.set_volume_by_user(bar.volume() + delta);
}

pub fn seek_to(ms: i64) {
    if devices::is_remote() {
        devices::seek(ms.max(0) as u32);
    } else {
        ctx().with_engine(|e| e.seek(ms.max(0) as u32));
    }
}

/// Next song, here or on the device that's playing.
pub fn next() {
    skip(Skip::Next);
}

/// Previous song (or the start of this one), here or on the device playing.
pub fn prev() {
    skip(Skip::Previous);
}

fn install_actions(app: &adw::Application) {
    let string = Some(glib::VariantTy::STRING);
    let action = |name: &str, f: fn()| {
        let a = gio::SimpleAction::new(name, None);
        a.connect_activate(move |_, _| f());
        app.add_action(&a);
    };
    action("search", || navigate(Route::Search, true));
    // Ctrl+F finds in the open playlist, album or Liked Songs; elsewhere it
    // goes to Search.
    action("find", || {
        let page = ctx().nav.visible_page();
        if !page.is_some_and(|p| pages::find_on(&p)) {
            navigate(Route::Search, true);
        }
    });
    action("play-pause", play_pause);
    action("next", next);
    action("prev", prev);
    action("volume-up", || nudge_volume(0.05));
    action("volume-down", || nudge_volume(-0.05));
    action("logout", logout);
    action("lyrics", toggle_lyrics);
    action("preferences", || preferences::present(&ctx().window));
    action("toggle-sidebar", || {
        let collapsed = !ctx().settings.borrow().sidebar_collapsed;
        ctx().settings.borrow_mut().sidebar_collapsed = collapsed;
        ctx().settings.borrow().save();
        apply_rail();
    });
    action("zoom-in", || step_zoom(1));
    action("zoom-out", || step_zoom(-1));
    action("zoom-reset", || step_zoom(0));
    action("quit", quit);
    action("about", || {
        let about = adw::AboutDialog::builder()
            .application_name("onify")
            .application_icon(APP_ID)
            .version(env!("ONIFY_VERSION"))
            .comments("A native Spotify client that stays smooth and light.")
            .developer_name("orqz")
            .license_type(gtk::License::MitX11)
            .website(REPO)
            .issue_url(format!("{REPO}/issues"))
            .build();
        about.add_link("Discord Server", DISCORD_SERVER);
        about.present(Some(&ctx().window));
    });
    action("discord", || {
        gtk::UriLauncher::new(DISCORD_SERVER).launch(Some(&ctx().window), gio::Cancellable::NONE, |_| {});
    });

    let with_string = |name: &str, f: fn(&str)| {
        let a = gio::SimpleAction::new(name, string);
        a.connect_activate(move |_, v| {
            if let Some(s) = v.and_then(|v| v.str()) {
                f(s);
            }
        });
        app.add_action(&a);
    };
    with_string("open", open_uri);
    with_string("add-to-playlist", playlists::add_from_menu);
    with_string("new-playlist-with", playlists::new_playlist);
    with_string("remove-from-playlist", playlists::remove_from_menu);
    with_string("album-of", open_album_of);
    // Developer aid: with ONIFY_DEV set, `app.dev-render` saves the window as a
    // PNG at 2x, even while it's on another workspace.
    if std::env::var_os("ONIFY_DEV").is_some() {
        // Plays one song (Spotify or local) as if clicked, for testing.
        with_string("dev-play", |uri| ctx().with_engine(|e| e.play_tracks(vec![uri.to_owned()], 0)));
        // Opens home, search, liked or local, as the sidebar would.
        with_string("dev-route", |name| {
            let route = match name {
                "home" => Route::Home,
                "search" => Route::Search,
                "liked" => Route::Card(liked_card()),
                "local" => Route::Card(local_card()),
                _ => return,
            };
            navigate(route, true);
        });
        with_string("dev-update", |_| updater::update_now());
        // Drops the session as a network loss would, or restarts it as a
        // settings change does.
        with_string("dev-disconnect", |_| {
            let ctx = ctx();
            if let Some(engine) = ctx.engine.borrow().as_ref() {
                engine.shutdown();
            }
            handle_event(Event::Disconnected(ctx.generation.get()));
        });
        with_string("dev-restart", |_| {
            ctx().engine_stale.set(true);
            restart_if_stale();
        });
        // Scrolls the visible page top to bottom in 4 s and logs frame times.
        with_string("dev-scroll", |_| dev_scroll());
        with_string("dev-volume", |_| ctx().bar.show_volume_pop());
        with_string("dev-devices", |_| log::warn!("dev-devices: {}", devices::describe()));
        // Types into the focused search field (Add Songs, Search).
        with_string("dev-type", |text| {
            let entry = gtk::prelude::GtkWindowExt::focus(&ctx().window)
                .and_then(|f| f.ancestor(gtk::SearchEntry::static_type()))
                .and_downcast::<gtk::SearchEntry>();
            if let Some(entry) = entry {
                entry.set_text(text);
            }
        });
        // Runs one of the visible playlist page's ⋯ actions (edit, add-songs).
        with_string("dev-page-action", |name| {
            if let Some(page) = ctx().nav.visible_page() {
                let tag = page.tag();
                let ran = page.activate_action(&format!("playlist.{name}"), None);
                log::warn!("dev-page-action {name} on {tag:?}: {ran:?}");
            }
        });
        with_string("dev-shuffle", |_| ctx().bar.toggle_shuffle());
        // Times each frame's phases (tick callbacks, layout, drawing) for 5 s.
        with_string("dev-frames", |_| dev_frames());
        // Renders the window's current frame 30 times off-screen and logs
        // the average: GTK's renderer alone, without building the frame.
        with_string("dev-rendercost", |_| {
            let window = ctx().window.clone();
            let paintable = gtk::WidgetPaintable::new(Some(&window));
            window.queue_draw();
            glib::timeout_add_local_once(Duration::from_millis(300), move || {
                let (w, h) = (window.width() as f64, window.height() as f64);
                let snapshot = gtk::Snapshot::new();
                paintable.current_image().snapshot(&snapshot, w, h);
                let (Some(node), Some(renderer)) = (snapshot.to_node(), window.renderer()) else { return };
                let started = std::time::Instant::now();
                for _ in 0..30 {
                    let _ = renderer.render_texture(&node, None);
                }
                log::warn!("dev-rendercost: {:.2} ms per render", started.elapsed().as_secs_f64() * 1000.0 / 30.0);
            });
        });
        // The window's render node, as text, for finding costly nodes.
        with_string("dev-node", |path| {
            let window = ctx().window.clone();
            let paintable = gtk::WidgetPaintable::new(Some(&window));
            let path = path.to_owned();
            window.queue_draw();
            glib::timeout_add_local_once(Duration::from_millis(300), move || {
                let snapshot = gtk::Snapshot::new();
                paintable.current_image().snapshot(&snapshot, window.width() as f64, window.height() as f64);
                if let Some(node) = snapshot.to_node() {
                    let _ = std::fs::write(&path, node.serialize());
                }
            });
        });
        // Adds two songs to a playlist, moves the last to the top, removes
        // the second, logging the playlist after each step.
        with_string("dev-playlist-check", |playlist| {
            let Some(api) = ctx().api() else { return };
            let playlist = playlist.to_owned();
            spawn_local(async move {
                let report = rt::spawn(async move {
                    let mut log = Vec::new();
                    let added = api
                        .add_to_playlist(&playlist, &["spotify:track:7qiZfU4dY1lWllzX7mPBI3".into(), "spotify:track:0VjIjW4GlUZAMYd2vXMi3b".into()])
                        .await;
                    log.push(format!("add: {added:?}"));
                    let songs = api.describe_playlist(&playlist).await;
                    log.push(format!("after add: {songs:?}"));
                    if let (Some(last), Some(first)) = (songs.last(), songs.first()) {
                        let moved = api.move_in_playlist(&playlist, &[last.1.clone()], Some(&first.1)).await;
                        log.push(format!("move: {moved:?}"));
                    }
                    let songs = api.describe_playlist(&playlist).await;
                    log.push(format!("after move: {songs:?}"));
                    if let Some(second) = songs.get(1) {
                        let removed = api.remove_from_playlist(&playlist, &[second.1.clone()]).await;
                        log.push(format!("remove: {removed:?}"));
                    }
                    log.push(format!("after remove: {:?}", api.describe_playlist(&playlist).await));
                    log.join("\n")
                })
                .await;
                log::warn!("dev-playlist-check:\n{report}");
            });
        });
        with_string("dev-delete-playlist", |playlist| {
            let Some(api) = ctx().api() else { return };
            let playlist = playlist.to_owned();
            spawn_local(async move {
                let asked = playlist.clone();
                let done = rt::spawn(async move { api.delete_playlist(&asked).await }).await;
                log::warn!("dev-delete-playlist {playlist}: {done:?}");
            });
        });
        with_string("dev-rootlist", |_| {
            let Some(api) = ctx().api() else { return };
            spawn_local(async move {
                let found = rt::spawn(async move { api.describe_rootlist().await }).await;
                log::warn!("dev-rootlist: {found:?}");
            });
        });
        // Plays a playlist or album from the top, as its Play button does.
        with_string("dev-play-context", |uri| ctx().with_engine(|e| e.play_context(uri, None, None)));
        // Finds in the open list, as typing after Ctrl+F would.
        with_string("dev-find", |query| {
            let Some(page) = ctx().nav.visible_page() else { return };
            if pages::find_on(&page) {
                let entry = gtk::prelude::GtkWindowExt::focus(&ctx().window)
                    .and_then(|f| f.ancestor(gtk::SearchEntry::static_type()))
                    .and_downcast::<gtk::SearchEntry>();
                if let Some(entry) = entry {
                    entry.set_text(query);
                }
            }
        });
        with_string("dev-search", |query| {
            navigate(Route::Search, true);
            ctx().search.entry.set_text(query);
        });
        with_string("dev-render", |path| {
            // A widget paintable only fills in on the next redraw, so ask for
            // one and save a moment later.
            let window = ctx().window.clone();
            // The whole window, so dialogs (Preferences) show too.
            let content: gtk::Widget = window.clone().upcast();
            let paintable = gtk::WidgetPaintable::new(Some(&content));
            content.queue_draw();
            let path = path.to_owned();
            glib::timeout_add_local_once(Duration::from_millis(400), move || {
                let (w, h) = (window.width() as f32, window.height() as f32);
                let snapshot = gtk::Snapshot::new();
                snapshot.scale(2.0, 2.0);
                paintable.current_image().snapshot(&snapshot, w as f64, h as f64);
                let (Some(node), Some(renderer)) = (snapshot.to_node(), window.renderer()) else {
                    log::warn!("dev-render: nothing to render ({w}x{h})");
                    return;
                };
                let viewport = gtk::graphene::Rect::new(0.0, 0.0, w * 2.0, h * 2.0);
                if let Err(e) = renderer.render_texture(&node, Some(&viewport)).save_to_png(&path) {
                    log::warn!("dev-render failed: {e}");
                }
            });
        });
    }
    with_string("copy-text", |text| {
        #[cfg(windows)]
        let copied = frame::copy_text(text);
        #[cfg(not(windows))]
        let copied = false;
        if !copied {
            ctx().window.clipboard().set_text(text);
        }
        toast("Link copied");
    });
    with_string("queue", |uri| {
        let Some(api) = ctx().api() else { return };
        let uri = uri.to_owned();
        spawn_local(async move {
            match rt::spawn(async move { api.add_to_queue(&uri).await }).await {
                Ok(()) => toast("Added to queue"),
                Err(e) => toast(&format!("Couldn't add to queue: {e}")),
            }
        });
    });

    app.set_accels_for_action("app.search", &["<Ctrl>k", "<Ctrl>l"]);
    app.set_accels_for_action("app.find", &["<Ctrl>f"]);
    app.set_accels_for_action("app.next", &["<Ctrl>Right"]);
    app.set_accels_for_action("app.prev", &["<Ctrl>Left"]);
    app.set_accels_for_action("app.volume-up", &["<Ctrl>Up"]);
    app.set_accels_for_action("app.volume-down", &["<Ctrl>Down"]);
    app.set_accels_for_action("app.quit", &["<Ctrl>q"]);
    app.set_accels_for_action("app.preferences", &["<Ctrl>comma"]);
    app.set_accels_for_action("app.toggle-sidebar", &["<Ctrl>b"]);
    app.set_accels_for_action("app.zoom-in", &["<Ctrl>plus", "<Ctrl>equal", "<Ctrl>KP_Add"]);
    app.set_accels_for_action("app.zoom-out", &["<Ctrl>minus", "<Ctrl>KP_Subtract"]);
    app.set_accels_for_action("app.zoom-reset", &["<Ctrl>0", "<Ctrl>KP_0"]);
}

/// Space plays/pauses from anywhere except while typing.
fn install_keys(window: &adw::ApplicationWindow) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(|controller, key, _, modifiers| {
        if key != gdk::Key::space || !modifiers.is_empty() {
            return glib::Propagation::Proceed;
        }
        let typing = controller
            .widget()
            .and_then(|w| w.root())
            .and_then(|r| r.focus())
            .is_some_and(|f| f.is::<gtk::Text>() || f.is::<gtk::TextView>());
        if typing || ctx().root.visible_child_name().as_deref() != Some("main") {
            return glib::Propagation::Proceed;
        }
        play_pause();
        glib::Propagation::Stop
    });
    window.add_controller(keys);

    // Ctrl + mouse wheel zooms, like a browser.
    let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
    wheel.connect_scroll(|wheel, _, dy| {
        let ctrl = wheel.current_event_state().contains(gdk::ModifierType::CONTROL_MASK);
        if !ctrl || wheel.unit() != gdk::ScrollUnit::Wheel || dy == 0.0 {
            return glib::Propagation::Proceed;
        }
        step_zoom(if dy < 0.0 { 1 } else { -1 });
        glib::Propagation::Stop
    });
    window.add_controller(wheel);
}

/// Developer aid: where each frame's time goes, averaged over 5 s:
/// "update" (tick callbacks and animations), "layout" and "paint"
/// (snapshot and rendering).
fn dev_frames() {
    let Some(clock) = ctx().window.frame_clock() else { return };
    #[derive(Default)]
    struct Acc {
        mark: i64,
        frames: u32,
        update: i64,
        layout: i64,
        paint: i64,
        /// When this frame's work began, and the longest frame's work.
        start: i64,
        worst: i64,
        slow: u32,
        handlers: Vec<glib::SignalHandlerId>,
    }
    let acc: Rc<RefCell<Acc>> = Rc::default();
    let now = || glib::monotonic_time();
    let a = acc.clone();
    let h1 = clock.connect_update(move |_| {
        let mut a = a.borrow_mut();
        a.mark = now();
        a.start = a.mark;
    });
    let a = acc.clone();
    let h2 = clock.connect_layout(move |_| {
        let mut a = a.borrow_mut();
        let t = now();
        if a.mark > 0 {
            a.update += t - a.mark;
        }
        a.mark = t;
    });
    // The surface's own layout and render signals: connected after GTK's,
    // these fire once GTK has allocated / drawn.
    let surface = ctx().window.surface();
    let a = acc.clone();
    let s1 = surface.as_ref().map(|s| {
        s.connect_layout(move |_, _, _| {
            let mut a = a.borrow_mut();
            let t = now();
            if a.mark > 0 {
                a.layout += t - a.mark;
            }
            a.mark = t;
        })
    });
    let a = acc.clone();
    let s2 = surface.as_ref().map(|s| {
        s.connect_render(move |_, _| {
            let mut a = a.borrow_mut();
            let t = now();
            if a.mark > 0 {
                a.paint += t - a.mark;
            }
            a.mark = t;
            false
        })
    });
    let a = acc.clone();
    let h3 = clock.connect_paint(move |_| {
        let mut a = a.borrow_mut();
        if a.mark == 0 {
            a.mark = now();
        }
    });
    let a = acc.clone();
    let h4 = clock.connect_after_paint(move |_| {
        let mut a = a.borrow_mut();
        a.frames += 1;
        a.mark = 0;
        if a.start > 0 {
            let took = now() - a.start;
            a.worst = a.worst.max(took);
            a.slow += u32::from(took > 16_000);
            a.start = 0;
        }
    });
    acc.borrow_mut().handlers = vec![h1, h2, h3, h4];
    glib::timeout_add_local_once(Duration::from_secs(5), move || {
        let mut a = acc.borrow_mut();
        for h in a.handlers.drain(..) {
            clock.disconnect(h);
        }
        if let Some(surface) = surface {
            for h in [s1, s2].into_iter().flatten() {
                surface.disconnect(h);
            }
        }
        let n = a.frames.max(1) as f64;
        log::warn!(
            "dev-frames: {} frames in 5 s, per frame: update {:.2} ms, layout {:.2} ms, render {:.2} ms; worst {:.1} ms, {} over 16 ms",
            a.frames,
            a.update as f64 / n / 1000.0,
            a.layout as f64 / n / 1000.0,
            a.paint as f64 / n / 1000.0,
            a.worst as f64 / 1000.0,
            a.slow,
        );
    });
}

/// Developer aid: how smoothly the visible page scrolls, as frame times.
fn dev_scroll() {
    fn find_scroller(widget: &gtk::Widget) -> Option<gtk::ScrolledWindow> {
        if let Some(s) = widget.downcast_ref::<gtk::ScrolledWindow>() {
            if s.vadjustment().upper() > s.vadjustment().page_size() {
                return Some(s.clone());
            }
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            if let Some(found) = find_scroller(&c) {
                return Some(found);
            }
            child = c.next_sibling();
        }
        None
    }
    let Some(page) = ctx().nav.visible_page() else { return };
    let Some(scroller) = find_scroller(page.upcast_ref()) else {
        log::warn!("dev-scroll: nothing to scroll");
        return;
    };
    let adj = scroller.vadjustment();
    let (from, to) = (adj.lower(), (adj.upper() - adj.page_size()).min(adj.lower() + 6000.0));
    let start = Rc::new(Cell::new(0i64));
    let times: Rc<RefCell<Vec<i64>>> = Rc::default();
    scroller.add_tick_callback(move |scroller, clock| {
        let now = clock.frame_time();
        if start.get() == 0 {
            start.set(now);
        }
        times.borrow_mut().push(now);
        let t = ((now - start.get()) as f64 / 4_000_000.0).min(1.0);
        scroller.vadjustment().set_value(from + (to - from) * t);
        if t < 1.0 {
            return glib::ControlFlow::Continue;
        }
        let times = times.borrow();
        let mut gaps: Vec<i64> = times.windows(2).map(|w| w[1] - w[0]).collect();
        gaps.sort_unstable();
        let n = gaps.len().max(1);
        let mean = gaps.iter().sum::<i64>() as f64 / n as f64;
        let p95 = gaps.get(n * 95 / 100).copied().unwrap_or(0);
        let worst = gaps.last().copied().unwrap_or(0);
        let refresh = clock.refresh_info(now).0;
        let slow = gaps.iter().filter(|&&g| refresh > 0 && g as f64 > refresh as f64 * 1.5).count();
        log::warn!(
            "dev-scroll: {n} frames, {:.1} fps, mean {:.2} ms, p95 {:.2} ms, worst {:.2} ms, {slow} late (refresh {:.2} ms)",
            1e6 / mean,
            mean / 1000.0,
            p95 as f64 / 1000.0,
            worst as f64 / 1000.0,
            refresh as f64 / 1000.0
        );
        glib::ControlFlow::Break
    });
}

pub fn raise() {
    ctx().window.present();
}

/// Lets Spotify know this device is going away, then exits. Not through
/// closing the window: with a dialog open (Preferences), libadwaita closes
/// the dialog instead and onify would keep running (which stalled updates).
pub fn quit() {
    let ctx = ctx();
    if ctx.bar.is_playing() {
        remember(ctx.bar.position_ms());
    }
    ctx.generation.set(ctx.generation.get() + 1);
    ctx.window.set_visible(false);
    #[cfg(windows)]
    tray::stop();
    let goodbye = match ctx.engine.take() {
        Some(engine) => {
            engine.shutdown();
            300
        }
        None => 0,
    };
    let app = ctx.app.clone();
    glib::timeout_add_local_once(Duration::from_millis(goodbye), move || app.quit());
}

pub fn open_links(app: &adw::Application, files: &[gio::File], _hint: &str) {
    activate(app);
    for file in files {
        let link = file.uri().to_string();
        if self::ctx().api().is_some() {
            open_link(&link);
        } else {
            self::ctx().pending_link.replace(Some(link));
        }
    }
}

/// `spotify:kind:id` for `https://open.spotify.com/[intl-xx/]kind/id`;
/// anything else as it is.
pub fn uri_of_link(link: &str) -> Option<String> {
    let Some(path) = link.strip_prefix("https://open.spotify.com/") else {
        return Some(link.to_owned());
    };
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let parts: Vec<&str> = path.split('/').filter(|p| !p.starts_with("intl-")).collect();
    match parts.as_slice() {
        [kind, id, ..] => Some(format!("spotify:{kind}:{id}")),
        _ => None,
    }
}

/// Opens `spotify:kind:id` or `https://open.spotify.com/[intl-xx/]kind/id`.
fn open_link(link: &str) {
    let Some(uri) = uri_of_link(link) else { return };
    if uri.starts_with("spotify:track:") {
        ctx().with_engine(|e| e.play_tracks(vec![uri.clone()], 0));
    } else {
        open_uri(&uri);
    }
}
