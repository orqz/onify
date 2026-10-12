//! The content pages: home, search, track lists and artists.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use super::cover::Cover;
use super::track_row::{HeaderSlot, RowMode, column_header, factory, group_digits, objects, short_list};
use super::{ctx, open_card};
use crate::api::{Api, Card, Chunk, Header, Home, Images, Kind, SearchResults, Track, id_of};
use crate::rt;

// Pages line up on one gutter, set in style.css (`.gutter`, `.gutter-inset`
// for lists whose rows carry their own 12px padding) and narrowing with the
// window's tier.

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    for class in classes {
        label.add_css_class(class);
    }
    label
}

fn vbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Vertical, spacing)
}

pub fn page(title: &str, tag: &str, content: &impl IsA<gtk::Widget>, header: &adw::HeaderBar) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(header);
    toolbar.set_content(Some(content));
    toolbar.set_extend_content_to_top_edge(false);
    // Pages just slide in: fading the whole page in as well had GTK draw it
    // off-screen every frame of the slide, which big windows felt.
    adw::NavigationPage::with_tag(&toolbar, title, tag)
}

fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(child)
        .build();
    glide_wheel(&scroller);
    scroller
}

/// How long one wheel notch glides for.
const GLIDE_US: i64 = 160_000;

struct Glide {
    from: f64,
    to: f64,
    started: i64,
    /// The value this last set; anything else means the user took over.
    set: f64,
    tick: gtk::TickCallbackId,
}

/// A mouse wheel moves in whole notches and GTK jumps each one at once (about
/// 90px), which reads as stutter, worse the higher the screen's refresh rate
/// (frames are smooth, the content just leaps). Browsers glide a notch
/// instead, so this does too. Touchpads already scroll smoothly.
pub fn glide_wheel(scroller: &gtk::ScrolledWindow) {
    let glide: Rc<RefCell<Option<Glide>>> = Rc::default();
    let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
    wheel.connect_scroll(glib::clone!(
        #[weak]
        scroller,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |wheel, _, dy| {
            let animate = gtk::Settings::default().is_some_and(|s| s.is_gtk_enable_animations());
            let shift = wheel.current_event_state().contains(gtk::gdk::ModifierType::SHIFT_MASK);
            if wheel.unit() != gtk::gdk::ScrollUnit::Wheel || shift || !animate {
                return glib::Propagation::Proceed;
            }
            let adj = scroller.vadjustment();
            let end = (adj.upper() - adj.page_size()).max(adj.lower());
            let mut slot = glide.borrow_mut();
            // Notches in a row add up, from where the last one was heading.
            let base = match slot.take() {
                Some(old) => {
                    old.tick.remove();
                    if (adj.value() - old.set).abs() < 1.0 { old.to } else { adj.value() }
                }
                None => adj.value(),
            };
            let to = (base + dy * adj.page_size().powf(2.0 / 3.0)).clamp(adj.lower(), end);
            let tick = scroller.add_tick_callback(glib::clone!(
                #[strong]
                glide,
                move |scroller, clock| step_glide(scroller, clock, &glide)
            ));
            *slot = Some(Glide { from: adj.value(), to, started: glib::monotonic_time(), set: adj.value(), tick });
            glib::Propagation::Stop
        }
    ));
    scroller.add_controller(wheel);
}

fn step_glide(scroller: &gtk::ScrolledWindow, clock: &gtk::gdk::FrameClock, glide: &RefCell<Option<Glide>>) -> glib::ControlFlow {
    let mut slot = glide.borrow_mut();
    let Some(g) = slot.as_mut() else { return glib::ControlFlow::Break };
    let adj = scroller.vadjustment();
    // Dragging the scrollbar (or the page changing under it) ends the glide.
    if (adj.value() - g.set).abs() >= 1.0 {
        *slot = None;
        return glib::ControlFlow::Break;
    }
    let t = ((clock.frame_time() - g.started) as f64 / GLIDE_US as f64).clamp(0.0, 1.0);
    let eased = 1.0 - (1.0 - t).powi(3);
    let end = (adj.upper() - adj.page_size()).max(adj.lower());
    adj.set_value((g.from + (g.to - g.from) * eased).clamp(adj.lower(), end));
    g.set = adj.value();
    if t >= 1.0 {
        *slot = None;
        return glib::ControlFlow::Break;
    }
    glib::ControlFlow::Continue
}

/// A header bar whose title fades in once the page's own title scrolls away.
fn fading_header(title: &str, adjustment: &gtk::Adjustment, threshold: f64) -> adw::HeaderBar {
    let window_title = adw::WindowTitle::new(title, "");
    window_title.add_css_class("fade-title");
    window_title.set_opacity(0.0);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&window_title));
    adjustment.connect_value_changed(glib::clone!(
        #[weak]
        window_title,
        move |adj| {
            let shown = adj.value() > threshold;
            window_title.set_opacity(if shown { 1.0 } else { 0.0 });
        }
    ));
    header
}

fn spinner() -> gtk::Widget {
    let spinner = adw::Spinner::new();
    spinner.set_size_request(36, 36);
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    spinner.set_vexpand(true);
    spinner.set_margin_top(48);
    spinner.upcast()
}

fn play_button(tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name("onify-media-playback-start-symbolic");
    button.add_css_class("hero-play");
    button.add_css_class("circular");
    button.set_tooltip_text(Some(tooltip));
    button.set_valign(gtk::Align::Center);
    button
}

fn glass_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.add_css_class("glass-button");
    button.add_css_class("circular");
    button.set_tooltip_text(Some(tooltip));
    button.set_valign(gtk::Align::Center);
    button
}

/// The big cover, title and actions at the top of a collection page.
struct Hero {
    widget: gtk::Box,
    art: gtk::Box,
    title: gtk::Label,
    subtitle: gtk::Label,
    actions: gtk::Box,
}

/// How much room the pages get, from the window's size (breakpoints in
/// ui/mod.rs). Page headers, gutters and rows scale with it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tier {
    #[default]
    Large,
    /// Under ~1100px wide or ~720px tall: smaller headers.
    Medium,
    /// Phone width: headers stack the cover above the title.
    Compact,
}

impl Tier {
    fn art(self) -> i32 {
        match self {
            Tier::Large => 232,
            Tier::Medium => 168,
            Tier::Compact => 148,
        }
    }
}

thread_local! {
    /// Every page header (and its art), so they all follow the tier.
    static HEROES: RefCell<Vec<(glib::WeakRef<gtk::Box>, glib::WeakRef<gtk::Box>)>> = const { RefCell::new(Vec::new()) };
    static TIER: Cell<Tier> = const { Cell::new(Tier::Large) };
    /// The Vinyl style: album and playlist covers have a record peeking out.
    static VINYL: Cell<bool> = const { Cell::new(false) };
}

/// How far the record shows past a sleeve `px` wide.
fn peek(px: i32) -> i32 {
    px * 3 / 8
}

/// An album or playlist cover as a record sleeve: in the Vinyl style the
/// record (the same art as its label) slides out from behind it as the page
/// opens. The overlay's first child only holds the room for both.
fn sleeve(url: Option<&str>) -> (gtk::Overlay, Cover) {
    let room = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let record = Cover::new(212, 0.0);
    record.set_record(true);
    record.add_css_class("hero-record");
    record.set_url(url);
    let cover = Cover::new(212, 14.0);
    cover.add_css_class("hero-sleeve");
    cover.set_url(url);
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&room));
    overlay.add_overlay(&record);
    overlay.add_overlay(&cover);
    (overlay, record)
}

/// Sizes a sleeve's cover, record and room for `px`, record shown or not.
fn size_sleeve(overlay: &gtk::Overlay, px: i32) {
    let vinyl = VINYL.get();
    let mut child = overlay.first_child();
    while let Some(widget) = child {
        match widget.downcast_ref::<Cover>() {
            Some(cover) => {
                cover.set_size(px);
                if cover.has_css_class("hero-record") {
                    cover.set_visible(vinyl);
                    cover.set_margin_start(if vinyl { peek(px) } else { 0 });
                }
            }
            None => widget.set_size_request(px + if vinyl { peek(px) } else { 0 }, px),
        }
        child = widget.next_sibling();
    }
}

/// Points a header's art (plain cover or sleeve) at a new image.
fn set_art_url(art: &gtk::Box, url: &str) {
    let Some(first) = art.first_child() else { return };
    if let Some(cover) = first.downcast_ref::<Cover>() {
        cover.set_url(Some(url));
    }
    let mut child = first.downcast_ref::<gtk::Overlay>().and_then(|o| o.first_child());
    while let Some(widget) = child {
        if let Some(cover) = widget.downcast_ref::<Cover>() {
            cover.set_url(Some(url));
        }
        child = widget.next_sibling();
    }
}

/// Called when the style changes between Vinyl and glass.
pub fn set_vinyl(vinyl: bool) {
    VINYL.set(vinyl);
    set_tier(TIER.get());
}

fn apply_tier(hero: &gtk::Box, art: &gtk::Box, tier: Tier) {
    let compact = tier == Tier::Compact;
    // Cover above the title when there's no room for them side by side.
    hero.set_orientation(if compact { gtk::Orientation::Vertical } else { gtk::Orientation::Horizontal });
    hero.set_spacing(match tier {
        Tier::Large => 36,
        Tier::Medium => 24,
        Tier::Compact => 18,
    });
    hero.set_margin_bottom(if tier == Tier::Large { 32 } else { 20 });
    for class in ["medium", "compact"] {
        hero.remove_css_class(class);
    }
    match tier {
        Tier::Medium => hero.add_css_class("medium"),
        Tier::Compact => hero.add_css_class("compact"),
        Tier::Large => {}
    }
    // The cover, the sleeve with its record, or the gradient tile standing
    // in for a cover.
    if let Some(child) = art.first_child() {
        let px = tier.art();
        if let Some(overlay) = child.downcast_ref::<gtk::Overlay>() {
            size_sleeve(overlay, px);
            art.set_css_classes(&["hero-art", "sleeved"]);
        } else {
            match child.downcast::<Cover>() {
                Ok(cover) => cover.set_size(px),
                Err(tile) => tile.set_size_request(px, px),
            }
        }
    }
}

pub fn tier() -> Tier {
    TIER.get()
}

/// Called as the window's size crosses a tier.
pub fn set_tier(tier: Tier) {
    TIER.set(tier);
    HEROES.with_borrow_mut(|heroes| {
        heroes.retain(|(h, _)| h.upgrade().is_some());
        for (hero, art) in heroes.iter().filter_map(|(h, a)| Some((h.upgrade()?, a.upgrade()?))) {
            apply_tier(&hero, &art, tier);
        }
    });
    super::track_row::set_album_column(tier != Tier::Compact);
}

fn hero(eyebrow: &str, title: &str, art: &impl IsA<gtk::Widget>) -> Hero {
    let widget = gtk::Box::builder().spacing(32).margin_top(8).build();
    widget.add_css_class("hero");
    widget.add_css_class("gutter");
    let art_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    art_box.add_css_class("hero-art");
    art_box.set_valign(gtk::Align::Center);
    art_box.set_halign(gtk::Align::Start);
    art_box.append(art);
    widget.append(&art_box);
    apply_tier(&widget, &art_box, TIER.get());
    HEROES.with_borrow_mut(|heroes| heroes.push((widget.downgrade(), art_box.downgrade())));

    let info = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::End)
        .spacing(6)
        .hexpand(true)
        .build();
    info.append(&label(eyebrow, &["eyebrow"]));
    let title_label = label(title, &["title-hero"]);
    title_label.set_wrap(true);
    title_label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    title_label.set_lines(2);
    info.append(&title_label);
    let subtitle = label("", &["meta"]);
    subtitle.set_wrap(true);
    subtitle.set_lines(2);
    super::track_row::open_links(&subtitle);
    info.append(&subtitle);
    let actions = gtk::Box::builder().spacing(14).margin_top(18).build();
    info.append(&actions);
    widget.append(&info);
    Hero {
        widget,
        art: art_box,
        title: title_label,
        subtitle,
        actions,
    }
}

pub fn card(card: &Card) -> gtk::Widget {
    let button = gtk::Button::new();
    button.add_css_class("card-tile");
    button.add_css_class("flat");
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .width_request(CARD)
        .build();
    let cover = if card.kind == Kind::Artist {
        Cover::round(CARD)
    } else {
        Cover::new(CARD, 14.0)
    };
    cover.set_url(card.images.pick(300));
    cover.set_margin_bottom(10);
    content.append(&cover);
    let name = label(&card.name, &["card-title"]);
    name.set_max_width_chars(1);
    name.set_hexpand(true);
    let subtitle = label(&card.subtitle, &["card-subtitle"]);
    subtitle.set_max_width_chars(1);
    subtitle.set_wrap(true);
    subtitle.set_lines(2);
    if card.kind == Kind::Artist {
        name.set_xalign(0.5);
        subtitle.set_xalign(0.5);
    }
    content.append(&name);
    content.append(&subtitle);
    button.set_child(Some(&content));
    button.set_tooltip_text(Some(&card.name));
    let card = card.clone();
    button.connect_clicked(move |_| open_card(&card));
    button.upcast()
}

const CARD: i32 = 164;

/// A titled row of cards that scrolls sideways, with arrows at its ends.
fn carousel(title: &str, cards: &[Card]) -> Option<gtk::Box> {
    if cards.is_empty() {
        return None;
    }
    let section = vbox(12);
    let heading = label(title, &["title-section"]);
    heading.add_css_class("gutter");
    section.append(&heading);
    // Packed to the left: wide windows used to spread a short row out.
    let row = gtk::Box::builder().spacing(4).halign(gtk::Align::Start).build();
    row.add_css_class("gutter-inset");
    for c in cards {
        row.append(&card(c));
    }
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&row)
        .build();
    scroller.add_css_class("carousel");
    let shelf = gtk::Overlay::builder().child(&scroller).build();
    shelf.add_css_class("shelf");
    let back = shelf_arrow("onify-go-previous-symbolic", "Back", gtk::Align::Start, &scroller, -1.0);
    let forward = shelf_arrow("onify-go-next-symbolic", "More", gtk::Align::End, &scroller, 1.0);
    shelf.add_overlay(&back);
    shelf.add_overlay(&forward);
    // Each arrow shows only while there's more that way.
    let adj = scroller.hadjustment();
    let update = glib::clone!(
        #[weak]
        back,
        #[weak]
        forward,
        move |adj: &gtk::Adjustment| {
            back.set_visible(adj.value() > adj.lower() + 1.0);
            forward.set_visible(adj.value() < adj.upper() - adj.page_size() - 1.0);
        }
    );
    update(&adj);
    adj.connect_value_changed(update.clone());
    adj.connect_changed(update);
    section.append(&shelf);
    Some(section)
}

/// A round arrow over one end of a carousel that slides it a screenful.
fn shelf_arrow(icon: &str, tooltip: &str, side: gtk::Align, scroller: &gtk::ScrolledWindow, direction: f64) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.add_css_class("shelf-arrow");
    button.add_css_class("circular");
    button.set_tooltip_text(Some(tooltip));
    button.set_halign(side);
    button.set_valign(gtk::Align::Start);
    // Level with the middle of the covers (cards have 12px of padding).
    button.set_margin_top(12 + CARD / 2 - 20);
    button.set_margin_start(12);
    button.set_margin_end(12);
    button.set_visible(false);
    let slide: Rc<RefCell<Option<adw::TimedAnimation>>> = Rc::default();
    button.connect_clicked(glib::clone!(
        #[weak]
        scroller,
        move |_| {
            let adj = scroller.hadjustment();
            let end = (adj.upper() - adj.page_size()).max(adj.lower());
            // Whole cards: a screenful less one card, so the last one
            // seen stays in view as a landmark.
            let step = ((adj.page_size() / (CARD + 28) as f64).floor() - 1.0).max(1.0) * (CARD + 28) as f64;
            let to = (adj.value() + direction * step).clamp(adj.lower(), end);
            let target = adw::PropertyAnimationTarget::new(&adj, "value");
            let animation = adw::TimedAnimation::new(&scroller, adj.value(), to, 380, target);
            animation.set_easing(adw::Easing::EaseOutCubic);
            animation.play();
            if let Some(old) = slide.replace(Some(animation)) {
                old.pause();
            }
        }
    ));
    button
}

/// Home's quick picks: compact tiles in a grid.
fn shortcuts(cards: &[Card]) -> gtk::FlowBox {
    let grid = gtk::FlowBox::builder()
        .homogeneous(true)
        .selection_mode(gtk::SelectionMode::None)
        .min_children_per_line(2)
        .max_children_per_line(4)
        .column_spacing(12)
        .row_spacing(12)
        .css_classes(["gutter"])
        .build();
    for c in cards {
        let button = gtk::Button::new();
        button.add_css_class("shortcut");
        let content = gtk::Box::builder().spacing(14).build();
        let cover = if c.kind == Kind::Artist { Cover::round(48) } else { Cover::new(48, 8.0) };
        cover.set_url(c.images.pick(100));
        content.append(&cover);
        let name = label(&c.name, &["shortcut-title"]);
        name.set_max_width_chars(1);
        name.set_hexpand(true);
        content.append(&name);
        button.set_child(Some(&content));
        let c = c.clone();
        button.connect_clicked(move |_| open_card(&c));
        grid.append(&button);
    }
    let mut child = grid.first_child();
    while let Some(c) = child {
        c.set_focusable(false);
        child = c.next_sibling();
    }
    grid
}

fn greeting() -> &'static str {
    let hour = glib::DateTime::now_local().map(|d| d.hour()).unwrap_or(12);
    match hour {
        5..=11 => "Good morning",
        12..=17 => "Good afternoon",
        _ => "Good evening",
    }
}

pub struct HomePage {
    pub page: adw::NavigationPage,
    body: gtk::Box,
}

impl HomePage {
    pub fn new() -> Self {
        let body = vbox(40);
        body.set_margin_top(8);
        body.set_margin_bottom(40);
        let title = label(greeting(), &["title-hero"]);
        title.add_css_class("gutter");
        body.append(&title);
        body.append(&spinner());
        let scroller = scrolled(&body);
        let header = fading_header("Home", &scroller.vadjustment(), 60.0);
        let page = page("Home", "home", &scroller, &header);
        Self { page, body }
    }

    pub fn fill(&self, home: &Home) {
        while let Some(child) = self.body.last_child() {
            if child == self.body.first_child().unwrap() {
                break;
            }
            self.body.remove(&child);
        }
        let mut sections = home.sections.iter();
        // The first row is usually recent picks: show it as quick tiles.
        if let Some((_, cards)) = home.sections.first() {
            let picks: Vec<Card> = cards.iter().take(8).cloned().collect();
            self.body.append(&shortcuts(&picks));
            if cards.len() <= 8 {
                sections.next();
            }
        }
        for (title, cards) in sections {
            if let Some(section) = carousel(title, cards) {
                self.body.append(&section);
            }
        }
    }
}

pub struct SearchPage {
    pub page: adw::NavigationPage,
    pub entry: gtk::SearchEntry,
}

impl SearchPage {
    pub fn new() -> Self {
        let entry = gtk::SearchEntry::builder()
            .placeholder_text("What do you want to play?")
            .hexpand(true)
            .search_delay(160)
            .build();
        entry.add_css_class("search-pill");
        let clamp = adw::Clamp::builder().maximum_size(620).child(&entry).build();
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&clamp));

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(140)
            .build();
        let empty = adw::StatusPage::builder()
            .icon_name("onify-system-search-symbolic")
            .title("Find your next favourite")
            .description("Songs, artists, albums and playlists")
            .build();
        let results = ResultsView::new();
        let nothing = adw::StatusPage::builder()
            .icon_name("onify-system-search-symbolic")
            .title("No results")
            .build();
        // Top result beside the songs while there's room for both, above
        // them when there isn't (judged by the page's own width, since the
        // sidebar takes a share of the window).
        let bin = adw::BreakpointBin::builder()
            .width_request(240)
            .height_request(200)
            .child(&scrolled(&results.body))
            .build();
        // Side by side the songs need ~420px next to the 300px tile.
        let narrow = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 800sp").unwrap());
        narrow.add_setter(&results.top, "orientation", Some(&gtk::Orientation::Vertical.to_value()));
        narrow.add_setter(&results.top, "spacing", Some(&36.to_value()));
        narrow.add_setter(&results.best, "width-request", Some(&(-1).to_value()));
        bin.add_breakpoint(narrow);
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&bin, Some("results"));
        stack.add_named(&nothing, Some("nothing"));
        stack.add_named(&spinner(), Some("loading"));
        let page = page("Search", "search", &stack, &header);

        let generation = Rc::new(Cell::new(0u64));
        let cache: Rc<RefCell<HashMap<String, Rc<SearchResults>>>> = Rc::default();
        entry.connect_search_changed(move |entry| {
            let query = entry.text().trim().to_owned();
            let current = generation.get() + 1;
            generation.set(current);
            if query.is_empty() {
                stack.set_visible_child_name("empty");
                return;
            }
            if let Some(found) = cache.borrow().get(&query) {
                show_results(&stack, &results, found);
                return;
            }
            if stack.visible_child_name().as_deref() != Some("results") {
                stack.set_visible_child_name("loading");
            }
            let Some(api) = ctx().api() else { return };
            let (generation, cache, stack, results) =
                (generation.clone(), cache.clone(), stack.clone(), results.clone());
            glib::spawn_future_local(async move {
                let q = query.clone();
                let found = rt::spawn(async move { api.search(&q).await }).await;
                if generation.get() != current {
                    return;
                }
                match found {
                    Ok(found) => {
                        let found = Rc::new(found);
                        cache.borrow_mut().insert(query, found.clone());
                        show_results(&stack, &results, &found);
                    }
                    Err(e) => {
                        stack.set_visible_child_name("empty");
                        super::toast(&format!("Search failed: {e}"));
                    }
                }
            });
        });

        Self { page, entry }
    }
}

/// The search results. The top row's boxes stay put between searches so
/// the narrow-page breakpoint can rearrange them; only their contents change.
#[derive(Clone)]
struct ResultsView {
    body: gtk::Box,
    /// The top result and the songs, side by side or stacked.
    top: gtk::Box,
    best: gtk::Box,
    songs: gtk::Box,
}

impl ResultsView {
    fn new() -> Self {
        let body = vbox(40);
        body.set_margin_top(12);
        body.set_margin_bottom(40);
        // On the inset gutter, since the song rows carry 12px of padding; the
        // tile and headings take those 12px as margins to line up with them.
        let top = gtk::Box::builder().spacing(4).css_classes(["gutter-inset"]).build();
        let best = vbox(12);
        best.set_width_request(300);
        best.set_margin_start(12);
        best.set_margin_end(12);
        let songs = vbox(12);
        songs.set_hexpand(true);
        top.append(&best);
        top.append(&songs);
        body.append(&top);
        Self { body, top, best, songs }
    }

    fn clear(&self) {
        for parent in [&self.best, &self.songs] {
            while let Some(child) = parent.first_child() {
                parent.remove(&child);
            }
        }
        while let Some(child) = self.body.last_child() {
            if child == self.top {
                break;
            }
            self.body.remove(&child);
        }
    }
}

/// The best match, as a big glass tile.
fn top_result(card: &Card) -> gtk::Widget {
    let button = gtk::Button::new();
    button.add_css_class("top-result");
    let content = vbox(14);
    let cover = if card.kind == Kind::Artist { Cover::round(104) } else { Cover::new(104, 12.0) };
    cover.set_url(card.images.pick(300));
    content.append(&cover);
    let name = label(&card.name, &["title-page"]);
    name.set_max_width_chars(1);
    content.append(&name);
    let kind = match card.kind {
        Kind::Artist => "Artist",
        Kind::Album => "Album",
        _ => "Playlist",
    };
    let pill = label(kind, &["pill-label"]);
    pill.set_halign(gtk::Align::Start);
    content.append(&pill);
    button.set_child(Some(&content));
    let card = card.clone();
    button.connect_clicked(move |_| open_card(&card));
    button.upcast()
}

fn show_results(stack: &gtk::Stack, results: &ResultsView, found: &SearchResults) {
    results.clear();
    if found.tracks.is_empty() && found.artists.is_empty() && found.albums.is_empty() && found.playlists.is_empty() {
        stack.set_visible_child_name("nothing");
        return;
    }

    let best = found.artists.first().or(found.albums.first()).or(found.playlists.first());
    results.best.set_visible(best.is_some());
    if let Some(best) = best {
        results.best.append(&label("Top result", &["title-section"]));
        let tile = top_result(best);
        tile.set_vexpand(true);
        results.best.append(&tile);
    }
    results.songs.set_visible(!found.tracks.is_empty());
    if !found.tracks.is_empty() {
        let heading = label("Songs", &["title-section"]);
        heading.set_margin_start(12);
        results.songs.append(&heading);
        let uris: Vec<String> = found.tracks.iter().map(|t| t.uri.clone()).collect();
        let tracks: Vec<Track> = found.tracks.iter().take(5).cloned().collect();
        let list = short_list(&tracks, RowMode::Compact, move |i| {
            ctx().with_engine(|e| e.play_tracks(uris.clone(), i));
        });
        results.songs.append(&list);
    }
    let results = &results.body;

    for (title, cards) in [
        ("Artists", &found.artists),
        ("Albums", &found.albums),
        ("Playlists", &found.playlists),
    ] {
        if let Some(section) = carousel(title, cards) {
            results.append(&section);
        }
    }
    stack.set_visible_child_name("results");
}

/// A loaded (or loading) track list, kept for instant revisits.
pub struct Loaded {
    pub store: gio::ListStore,
    pub header: RefCell<Option<Header>>,
    pub listeners: RefCell<Vec<Box<dyn Fn(&Header)>>>,
}

fn load_tracks(kind: Kind, uri: &str) -> Rc<Loaded> {
    let key = if kind == Kind::Liked { "liked".to_owned() } else { uri.to_owned() };
    if let Some(loaded) = ctx().stores.borrow().get(&key) {
        return loaded.clone();
    }
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    store.append(&glib::BoxedAnyObject::new(HeaderSlot));
    let loaded = Rc::new(Loaded {
        store,
        header: RefCell::default(),
        listeners: RefCell::default(),
    });
    ctx().stores.borrow_mut().insert(key.clone(), loaded.clone());

    if kind == Kind::Local {
        scan_local(&loaded);
        return loaded;
    }
    let id = id_of(uri).to_owned();
    let weak = Rc::downgrade(&loaded);
    glib::spawn_future_local(async move {
        // What this list looked like last time, from disk, straight away,
        // even while onify is still connecting to Spotify.
        let (offline, cached_id) = (Api::cache_only(), id.clone());
        let cached = rt::spawn(async move { offline.all_tracks(kind, cached_id).await }).await;
        let shown_uris: Option<Vec<String>> = match cached {
            Ok((header, tracks)) if !tracks.is_empty() => {
                let Some(loaded) = weak.upgrade() else { return };
                if let Some(header) = header {
                    set_header(&loaded, header);
                }
                let uris = tracks.iter().map(|t| t.uri.clone()).collect();
                note_liked(kind, &tracks);
                loaded.store.extend_from_slice(&objects(tracks, 0));
                Some(uris)
            }
            _ => None,
        };

        // Spotify's current copy needs the connection; wait for it if need be.
        let api = loop {
            if let Some(api) = ctx().api() {
                break api;
            }
            if weak.upgrade().is_none() {
                return;
            }
            glib::timeout_future(std::time::Duration::from_millis(200)).await;
        };
        match shown_uris {
            // Swap the fresh list in only if something changed.
            Some(uris) => {
                let fresh = rt::spawn(async move { api.all_tracks(kind, id).await }).await;
                let Some(loaded) = weak.upgrade() else { return };
                if let Ok((header, tracks)) = fresh {
                    if let Some(header) = header {
                        set_header(&loaded, header);
                    }
                    note_liked(kind, &tracks);
                    if tracks.iter().map(|t| &t.uri).ne(uris.iter()) {
                        let n = loaded.store.n_items();
                        loaded.store.splice(1, n.saturating_sub(1), &objects(tracks, 0));
                    }
                }
            }
            // Nothing saved yet: show each page as it arrives.
            None => {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                rt::handle().spawn(api.stream_tracks(kind, id, tx));
                while let Some(chunk) = rx.recv().await {
                    let Some(loaded) = weak.upgrade() else { return };
                    match chunk {
                        Chunk::Header(header) => set_header(&loaded, header),
                        Chunk::Tracks(tracks) => {
                            note_liked(kind, &tracks);
                            let listed = loaded.store.n_items().saturating_sub(1);
                            loaded.store.extend_from_slice(&objects(tracks, listed));
                        }
                        Chunk::Failed(e) => {
                            ctx().stores.borrow_mut().remove(&key);
                            super::toast(&format!("Couldn't load: {e}"));
                        }
                    }
                }
            }
        }
        crate::memory::trim_soon();
    });
    loaded
}

/// Everything in Liked Songs is liked: their hearts show without asking.
fn note_liked(kind: Kind, tracks: &[Track]) {
    if kind == Kind::Liked {
        super::track_row::set_known_liked(tracks.iter().map(|t| t.uri.clone()), true);
    }
}

/// The page follows its list's header, also when it changes later (a
/// playlist renamed or opened up for editing).
fn set_header(loaded: &Loaded, header: Header) {
    for listener in loaded.listeners.borrow().iter() {
        listener(&header);
    }
    loaded.header.replace(Some(header));
}

/// The loaded list for a playlist, if its page has been opened.
fn loaded_list(playlist: &str) -> Option<Rc<Loaded>> {
    ctx().stores.borrow().get(playlist).cloned()
}

/// Fetches a playlist again after it changed (songs added, details
/// edited): its header and songs, with their entry ids.
pub fn refresh_list(playlist: &str) {
    let Some(loaded) = loaded_list(playlist) else { return };
    let Some(api) = ctx().api() else { return };
    let id = id_of(playlist).to_owned();
    let weak = Rc::downgrade(&loaded);
    glib::spawn_future_local(async move {
        let fresh = rt::spawn(async move { api.all_tracks(Kind::Playlist, id).await }).await;
        let (Some(loaded), Ok((header, tracks))) = (weak.upgrade(), fresh) else { return };
        if let Some(header) = header {
            set_header(&loaded, header);
        }
        let n = loaded.store.n_items();
        loaded.store.splice(1, n.saturating_sub(1), &objects(tracks, 0));
    });
}

/// Index of a playlist entry (by uid) in its list (0 is the header slot).
fn entry_index(store: &gio::ListStore, uid: &str) -> Option<u32> {
    (1..store.n_items()).find(|&i| {
        store
            .item(i)
            .and_downcast::<glib::BoxedAnyObject>()
            .is_some_and(|o| o.try_borrow::<Track>().is_ok_and(|t| t.uid == uid))
    })
}

/// A song leaves the list straight away (Spotify hears of it after).
pub fn take_out(playlist: &str, uid: &str) {
    let Some(loaded) = loaded_list(playlist) else { return };
    if let Some(i) = entry_index(&loaded.store, uid) {
        loaded.store.remove(i);
    }
}

/// A song moves in the list straight away: before `before`, or to the end.
pub fn move_within(playlist: &str, uid: &str, before: Option<&str>) {
    let Some(loaded) = loaded_list(playlist) else { return };
    let store = &loaded.store;
    let Some(from) = entry_index(store, uid) else { return };
    let Some(item) = store.item(from) else { return };
    store.remove(from);
    let to = before.and_then(|b| entry_index(store, b)).unwrap_or(store.n_items());
    store.insert(to, &item);
}

/// A song dragged onto another: above it, or below it (before the next).
pub fn drop_song(playlist: &str, moved: &str, target: &str, below: bool) {
    let Some(loaded) = loaded_list(playlist) else { return };
    let store = &loaded.store;
    let Some(at) = entry_index(store, target) else { return };
    let before = if below {
        store
            .item(at + 1)
            .and_downcast::<glib::BoxedAnyObject>()
            .and_then(|o| o.try_borrow::<Track>().ok().map(|t| t.uid.clone()))
    } else {
        Some(target.to_owned())
    };
    if before.as_deref() == Some(moved) {
        return;
    }
    super::playlists::move_song(playlist, moved, before);
}

/// Fills Local Files from the music folders, scanned off the UI thread.
fn scan_local(loaded: &Rc<Loaded>) {
    let folders = ctx().settings.borrow().local_folders.clone();
    let weak = Rc::downgrade(loaded);
    glib::spawn_future_local(async move {
        let scanned = folders.clone();
        let tracks = rt::spawn(async move {
            tokio::task::spawn_blocking(move || crate::local::scan(&scanned))
                .await
                .unwrap_or_default()
        })
        .await;
        let Some(loaded) = weak.upgrade() else { return };
        let places: Vec<String> = folders
            .iter()
            .map(|f| f.file_name().unwrap_or(f.as_os_str()).to_string_lossy().into_owned())
            .collect();
        let subtitle = match (tracks.len(), places.is_empty()) {
            (_, true) => "Choose a music folder to see your songs here.".to_owned(),
            (0, false) => format!("No songs found in {}. MP3, FLAC and MP4 files play here.", places.join(", ")),
            (1, false) => format!("1 song from {}", places.join(", ")),
            (n, false) => format!("{n} songs from {}", places.join(", ")),
        };
        let header = Header { subtitle, ..Default::default() };
        set_header(&loaded, header);
        loaded.store.extend_from_slice(&objects(tracks, 0));
    });
}

/// Every track URI in a loaded list, in order.
fn track_uris(store: &gio::ListStore) -> Vec<String> {
    store
        .iter::<glib::BoxedAnyObject>()
        .flatten()
        .filter_map(|o| o.try_borrow::<Track>().ok().map(|t| t.uri.clone()))
        .collect()
}

/// Plays a whole track list from the top, shuffled if shuffle is on. Local
/// files have no Spotify context, so they play as a plain list.
fn play_all(kind: Kind, context: &str, store: &gio::ListStore) {
    if kind == Kind::Local {
        let uris = track_uris(store);
        if !uris.is_empty() {
            ctx().with_engine(|e| e.play_list(uris, None, None));
        }
    } else {
        ctx().with_engine(|e| e.play_context(context, None, None));
    }
}

/// The gradient tile that stands in for a cover on Liked Songs and Local Files.
fn icon_tile(icon: &str, class: &str) -> gtk::Widget {
    // Not expanding, or the centred icon's expand spreads to the header and
    // the title drifts away from the tile.
    let tile = gtk::Box::builder()
        .width_request(212)
        .height_request(212)
        .hexpand(false)
        .build();
    tile.add_css_class("icon-tile");
    tile.add_css_class(class);
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(72);
    image.set_hexpand(true);
    image.set_halign(gtk::Align::Center);
    tile.append(&image);
    tile.upcast()
}

pub fn tracks_page(kind: Kind, uri: &str, title: &str, images: &Images) -> adw::NavigationPage {
    let loaded = load_tracks(kind, uri);
    // Asked when playing: Liked Songs' context needs the username, which
    // isn't known until onify has connected.
    let context = {
        let uri = uri.to_owned();
        move || match kind {
            Kind::Liked => ctx().collection_uri(),
            _ => uri.clone(),
        }
    };

    let mut record = None;
    let art: gtk::Widget = match kind {
        Kind::Liked => icon_tile("onify-heart-filled-symbolic", "liked-tile"),
        Kind::Local => icon_tile("onify-folder-music-symbolic", "local-tile"),
        _ => {
            let (sleeve, disc) = sleeve(images.pick(480));
            record = Some(disc);
            sleeve.upcast()
        }
    };
    let eyebrow = match kind {
        Kind::Album => "Album",
        Kind::Local => "On this computer",
        _ => "Playlist",
    };
    let hero = hero(eyebrow, title, &art);
    let play = play_button("Play");
    let shuffle = glass_button("onify-media-playlist-shuffle-symbolic", "Shuffle");
    ctx().bar.add_shuffle_button(&shuffle);
    hero.actions.append(&play);
    hero.actions.append(&shuffle);
    if kind == Kind::Local {
        let folders = glass_button("onify-list-add-symbolic", "Choose music folders");
        folders.set_action_name(Some("app.preferences"));
        hero.actions.append(&folders);
    }
    // A playlist's ⋯: Add Songs, Edit Details, Delete, as far as the user
    // may (known once it loads).
    let list_info = (kind == Kind::Playlist).then(|| {
        Rc::new(super::track_row::ListInfo { uri: uri.to_owned(), editable: Cell::new(false) })
    });
    let more = gtk::MenuButton::builder()
        .icon_name("onify-view-more-symbolic")
        .tooltip_text("More")
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    more.add_css_class("glass-button");
    more.add_css_class("circular");
    hero.actions.append(&more);

    let header_widget = vbox(0);
    header_widget.append(&hero.widget);
    let mode = if kind == Kind::Album { RowMode::Album } else { RowMode::Playlist };
    header_widget.append(&column_header(mode));

    let apply_header = {
        let title = hero.title.downgrade();
        let subtitle = hero.subtitle.downgrade();
        let art = hero.art.downgrade();
        move |h: &Header| {
            let (Some(t), Some(s)) = (title.upgrade(), subtitle.upgrade()) else { return };
            if !h.title.is_empty() {
                t.set_label(&h.title);
            }
            // An album's artists link to their pages; the rest is plain text.
            let rest = h.subtitle.strip_prefix(&crate::api::join_names(&h.artists));
            match rest.filter(|_| !h.artists.is_empty()) {
                Some(rest) => s.set_markup(&format!(
                    "{}{}",
                    super::track_row::artist_links(&h.artists),
                    glib::markup_escape_text(rest)
                )),
                None => s.set_label(&h.subtitle),
            }
            if let (Some(art), Some(url)) = (art.upgrade(), h.images.pick(480)) {
                set_art_url(&art, url);
            }
        }
    };

    // Find in this list: songs whose title, artists or album contain what's
    // typed (the header slot always stays).
    let query: Rc<RefCell<String>> = Rc::default();
    let filter = gtk::CustomFilter::new(glib::clone!(
        #[strong]
        query,
        move |item| {
            let query = query.borrow();
            if query.is_empty() {
                return true;
            }
            let Some(object) = item.downcast_ref::<glib::BoxedAnyObject>() else { return true };
            match object.try_borrow::<Track>() {
                Ok(track) => matches_query(&track, &query),
                Err(_) => true,
            }
        }
    ));
    let shown = gtk::FilterListModel::new(Some(loaded.store.clone()), Some(filter.clone()));

    // A click selects a song (and shows its play count); its ▶ button on
    // hover, a double-click or Enter plays it.
    let selection = gtk::SingleSelection::new(Some(shown.clone()));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    selection.connect_selected_item_notify(|selection| {
        let item = selection.selected_item().and_downcast::<glib::BoxedAnyObject>();
        if let Some(track) = item.as_ref().and_then(|o| o.try_borrow::<Track>().ok()) {
            super::track_row::want_plays(&track);
        }
    });
    let list = gtk::ListView::builder()
        .model(&selection)
        .factory(&factory(mode, header_widget.upcast(), list_info.clone()))
        .single_click_activate(false)
        .build();
    list.add_css_class("tracks");
    let store = loaded.store.clone();
    let ctx_uri = context.clone();
    list.connect_activate(move |list, position| {
        let Some(object) = list.model().and_then(|m| m.item(position)).and_downcast::<glib::BoxedAnyObject>() else {
            return;
        };
        let Ok(track) = object.try_borrow::<Track>() else { return };
        if !track.playable {
            return;
        }
        if kind == Kind::Local {
            // By uri: while finding, positions are the filtered list's.
            let uris = track_uris(&store);
            let index = uris.iter().position(|u| *u == track.uri);
            ctx().with_engine(|e| e.play_list(uris, index, None));
        } else {
            ctx().with_engine(|e| e.play_context(&ctx_uri(), Some(&track.uri), None));
        }
    });

    let store = loaded.store.clone();
    play.connect_clicked(move |_| play_all(kind, &context(), &store));
    shuffle.connect_clicked(|_| ctx().bar.toggle_shuffle());

    let scroller = scrolled(&list);
    let header = fading_header(title, &scroller.vadjustment(), 200.0);
    let window_title = header.title_widget().and_downcast::<adw::WindowTitle>().map(|t| t.downgrade());
    let find = find_in_list(&header, kind, &query, &filter, &scroller);
    let details: Rc<RefCell<Header>> = Rc::default();
    let update = {
        let (list_info, more, details) = (list_info.clone(), more.downgrade(), details.clone());
        let playlist = uri.to_owned();
        move |h: &Header| {
            apply_header(h);
            if let Some(t) = window_title.as_ref().and_then(|w| w.upgrade()) {
                if !h.title.is_empty() {
                    t.set_title(&h.title);
                }
            }
            details.replace(h.clone());
            if let (Some(info), Some(more)) = (&list_info, more.upgrade()) {
                info.editable.set(h.can_edit_items);
                let menu = super::playlists::page_menu(&playlist, h.can_edit_items, h.can_edit_details);
                more.set_visible(menu.n_items() > 0 && (0..menu.n_items()).any(|i| {
                    menu.item_link(i, gio::MENU_LINK_SECTION).is_some_and(|s| s.n_items() > 0)
                }));
                more.set_menu_model(Some(&menu));
            }
        }
    };
    let ready = loaded.header.borrow().clone();
    if let Some(h) = ready {
        update(&h);
    }
    loaded.listeners.borrow_mut().push(Box::new(update));
    let tag = if kind == Kind::Liked { "liked" } else { uri };
    let page = page(title, tag, &scroller, &header);
    if kind == Kind::Playlist {
        page.insert_action_group("playlist", Some(&playlist_actions(uri, &details)));
    }
    FINDERS.with_borrow_mut(|finders| {
        finders.retain(|(p, _)| p.upgrade().is_some());
        finders.push((page.downgrade(), find.downgrade()));
    });
    if let Some(record) = record {
        slide_out_on_show(&page, &record);
    }
    page
}

/// What a playlist page's ⋯ menu does.
fn playlist_actions(playlist: &str, details: &Rc<RefCell<Header>>) -> gio::SimpleActionGroup {
    let group = gio::SimpleActionGroup::new();
    let add = |name: &str, run: Box<dyn Fn(&str, &Header)>| {
        let action = gio::SimpleAction::new(name, None);
        let (playlist, details) = (playlist.to_owned(), details.clone());
        action.connect_activate(move |_, _| run(&playlist, &details.borrow()));
        group.add_action(&action);
    };
    add("add-songs", Box::new(|p, h| super::playlists::add_songs(p, &h.title)));
    add("edit", Box::new(|p, h| super::playlists::edit_details(p, &h.title, &h.description)));
    add("delete", Box::new(|p, h| super::playlists::delete(p, &h.title, h.can_edit_details)));
    group
}

thread_local! {
    /// Each track list page's find button, for Ctrl+F.
    static FINDERS: RefCell<Vec<(glib::WeakRef<adw::NavigationPage>, glib::WeakRef<gtk::ToggleButton>)>> =
        const { RefCell::new(Vec::new()) };
}

/// Ctrl+F: opens the visible page's find field. False if the page has none.
pub fn find_on(page: &adw::NavigationPage) -> bool {
    let find = FINDERS.with_borrow(|finders| {
        finders.iter().find(|(p, _)| p.upgrade().as_ref() == Some(page)).and_then(|(_, f)| f.upgrade())
    });
    match find {
        Some(find) => {
            find.set_active(false);
            find.set_active(true);
            true
        }
        None => false,
    }
}

/// Whether a song's title, artists or album contain `query` (lowercase).
fn matches_query(track: &Track, query: &str) -> bool {
    track.name.to_lowercase().contains(query)
        || track.album.name.to_lowercase().contains(query)
        || track.artists.iter().any(|a| a.name.to_lowercase().contains(query))
}

/// A find button in the page's header that swaps the title for a search
/// field; typing narrows the list, Escape or the button again closes it.
fn find_in_list(
    header: &adw::HeaderBar,
    kind: Kind,
    query: &Rc<RefCell<String>>,
    filter: &gtk::CustomFilter,
    scroller: &gtk::ScrolledWindow,
) -> gtk::ToggleButton {
    let what = match kind {
        Kind::Album => "this album",
        Kind::Liked => "Liked Songs",
        Kind::Local => "Local Files",
        _ => "this playlist",
    };
    let toggle = gtk::ToggleButton::builder()
        .icon_name("onify-system-search-symbolic")
        .tooltip_text(format!("Find in {what} (Ctrl+F)"))
        .build();
    header.pack_end(&toggle);
    let entry = gtk::SearchEntry::builder().placeholder_text(format!("Find in {what}")).hexpand(true).build();
    entry.add_css_class("search-pill");
    let clamp = adw::Clamp::builder().maximum_size(420).child(&entry).build();
    let titles = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .transition_duration(140)
        .hhomogeneous(false)
        .build();
    if let Some(title) = header.title_widget() {
        header.set_title_widget(None::<&gtk::Widget>);
        titles.add_named(&title, Some("title"));
    }
    titles.add_named(&clamp, Some("find"));
    header.set_title_widget(Some(&titles));

    toggle.connect_toggled(glib::clone!(
        #[weak]
        entry,
        #[weak]
        titles,
        move |toggle| {
            if toggle.is_active() {
                titles.set_visible_child_name("find");
                entry.grab_focus();
            } else {
                entry.set_text("");
                titles.set_visible_child_name("title");
            }
        }
    ));
    entry.connect_search_changed(glib::clone!(
        #[strong]
        query,
        #[weak]
        filter,
        #[weak]
        scroller,
        move |entry| {
            let text = entry.text().trim().to_lowercase();
            if *query.borrow() == text {
                return;
            }
            let narrower = text.contains(query.borrow().as_str());
            query.replace(text);
            filter.changed(if narrower { gtk::FilterChange::MoreStrict } else { gtk::FilterChange::Different });
            // Matches start right under the header.
            let adj = scroller.vadjustment();
            if adj.value() > 0.0 {
                adj.set_value(0.0);
            }
        }
    ));
    entry.connect_stop_search(glib::clone!(
        #[weak]
        toggle,
        move |_| toggle.set_active(false)
    ));
    toggle
}

/// The record slides out of its sleeve each time the page comes into view.
/// It's laid out in its final place and only drawn shifted back under the
/// sleeve, so the slide moves pixels without laying the header out again.
fn slide_out_on_show(page: &adw::NavigationPage, record: &Cover) {
    let record = record.downgrade();
    let slide: Rc<RefCell<Option<adw::TimedAnimation>>> = Rc::default();
    page.connect_showing(move |_| {
        let Some(record) = record.upgrade() else { return };
        if !VINYL.get() {
            return;
        }
        let to = peek(TIER.get().art()) as f64;
        record.set_shift(-to as f32);
        let weak = record.downgrade();
        let target = adw::CallbackAnimationTarget::new(move |value| {
            if let Some(record) = weak.upgrade() {
                record.set_shift((value - to) as f32);
            }
        });
        let animation = adw::TimedAnimation::new(&record, 0.0, to, 700, target);
        animation.set_easing(adw::Easing::EaseOutCubic);
        animation.play();
        slide.replace(Some(animation));
    });
}

pub fn artist_page(card: &Card) -> adw::NavigationPage {
    let body = vbox(28);
    body.set_margin_bottom(40);

    let avatar = Cover::round(232);
    avatar.set_url(card.images.pick(480));
    let hero = hero("Artist", &card.name, &avatar);
    hero.art.add_css_class("round");
    let play = play_button("Play");
    let uri = card.uri.clone();
    play.connect_clicked(move |_| ctx().with_engine(|e| e.play_context(&uri, None, None)));
    hero.actions.append(&play);
    body.append(&hero.widget);
    let loading = spinner();
    body.append(&loading);

    // Popular songs with the latest release beside them, like search's top
    // result: side by side while there's room, stacked when there isn't.
    let top = gtk::Box::builder().spacing(4).css_classes(["gutter-inset"]).visible(false).build();
    let popular = vbox(12);
    popular.set_hexpand(true);
    let latest = vbox(12);
    latest.set_width_request(300);
    latest.set_margin_start(12);
    latest.set_margin_end(12);
    top.append(&popular);
    top.append(&latest);
    body.append(&top);

    let id = card.id().to_owned();
    let artist_uri = card.uri.clone();
    let (body_weak, avatar_weak) = (body.downgrade(), avatar.downgrade());
    let (title, subtitle) = (hero.title.clone(), hero.subtitle.clone());
    let (top_weak, popular_weak, latest_weak) = (top.downgrade(), popular.downgrade(), latest.downgrade());
    {
        glib::spawn_future_local(async move {
            // Opened before onify has connected (a link at startup): wait.
            let api = loop {
                if let Some(api) = ctx().api() {
                    break api;
                }
                if body_weak.upgrade().is_none() {
                    return;
                }
                glib::timeout_future(std::time::Duration::from_millis(200)).await;
            };
            let artist = rt::spawn(async move { api.artist(&id).await }).await;
            let (Some(body), Some(avatar)) = (body_weak.upgrade(), avatar_weak.upgrade()) else { return };
            let (Some(top), Some(popular), Some(latest)) = (top_weak.upgrade(), popular_weak.upgrade(), latest_weak.upgrade())
            else {
                return;
            };
            body.remove(&loading);
            let artist = match artist {
                Ok(a) => a,
                Err(e) => {
                    super::toast(&format!("Couldn't load artist: {e}"));
                    return;
                }
            };
            avatar.set_url(artist.images.pick(480));
            title.set_label(&artist.name);
            // Like Spotify: monthly listeners, else followers.
            if artist.monthly_listeners > 0 {
                subtitle.set_label(&format!("{} monthly listeners", group_digits(artist.monthly_listeners)));
            } else if artist.followers > 0 {
                subtitle.set_label(&format!("{} followers", group_digits(artist.followers)));
            }
            popular.set_visible(!artist.top.is_empty());
            if !artist.top.is_empty() {
                let heading = label("Popular", &["title-section"]);
                heading.set_margin_start(12);
                popular.append(&heading);
                let tracks: Vec<Track> = artist.top.iter().take(10).cloned().collect();
                let uris = tracks.clone();
                let list = short_list(&tracks, RowMode::Compact, move |i| {
                    let uri = uris[i].uri.clone();
                    ctx().with_engine(|e| e.play_context(&artist_uri, Some(&uri), None));
                });
                popular.append(&list);
            }
            latest.set_visible(artist.latest.is_some());
            if let Some(release) = &artist.latest {
                latest.append(&label("Latest release", &["title-section"]));
                latest.append(&release_tile(release));
            }
            top.set_visible(!artist.top.is_empty() || artist.latest.is_some());
            for (title, cards) in [
                ("Popular releases", &artist.popular),
                ("Albums", &artist.albums),
                ("Singles and EPs", &artist.singles),
            ] {
                if let Some(section) = carousel(title, cards) {
                    body.append(&section);
                }
            }
        });
    }

    let scroller = scrolled(&body);
    let header = fading_header(&card.name, &scroller.vadjustment(), 200.0);
    // Judged by the page's own width, since the sidebar takes a share.
    let bin = adw::BreakpointBin::builder().width_request(240).height_request(200).child(&scroller).build();
    let narrow = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 800sp").unwrap());
    narrow.add_setter(&top, "orientation", Some(&gtk::Orientation::Vertical.to_value()));
    narrow.add_setter(&top, "spacing", Some(&28.to_value()));
    narrow.add_setter(&latest, "width-request", Some(&(-1).to_value()));
    bin.add_breakpoint(narrow);
    page(&card.name, &card.uri, &bin, &header)
}

/// An artist's latest release: its cover beside the name and what it is.
fn release_tile(card: &Card) -> gtk::Widget {
    let button = gtk::Button::new();
    button.add_css_class("top-result");
    let content = gtk::Box::builder().spacing(18).build();
    let cover = Cover::new(112, 12.0);
    cover.set_url(card.images.pick(300));
    content.append(&cover);
    let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).valign(gtk::Align::Center).spacing(8).build();
    let name = label(&card.name, &["title-section"]);
    name.set_wrap(true);
    name.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    name.set_lines(2);
    name.set_max_width_chars(1);
    name.set_hexpand(true);
    text.append(&name);
    text.append(&label(&card.subtitle, &["card-subtitle"]));
    content.append(&text);
    button.set_child(Some(&content));
    button.set_tooltip_text(Some(&card.name));
    let card = card.clone();
    button.connect_clicked(move |_| open_card(&card));
    button.upcast()
}
