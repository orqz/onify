//! Making and changing playlists, as in Spotify's apps: Add to Playlist from
//! any song, New Playlist, Edit Details, Delete, removing songs, moving them
//! by dragging, and Add Songs (a search) on the user's own playlists.
//!
//! Changes show at once; the playlist is then fetched again so the page has
//! Spotify's own copy (with the new songs' entry ids, for removing them).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use super::{ctx, rt, toast};
use crate::api::{Card, Images, Kind, Track};

thread_local! {
    /// The library as last loaded, to know which playlists are the user's.
    static LIBRARY: RefCell<Vec<Card>> = const { RefCell::new(Vec::new()) };
}

/// The sidebar and Home again after a playlist changed: now, and a moment
/// later, since Spotify's library can lag a second behind the change itself
/// (a new playlist went missing from it).
fn reload_library() {
    super::load_library();
    glib::timeout_add_local_once(std::time::Duration::from_millis(2500), super::load_library);
}

/// The library (re)loaded.
pub fn library_loaded(library: &[Card]) {
    LIBRARY.with_borrow_mut(|l| *l = library.to_vec());
}

/// The user's own playlists, as the library lists them (newest first).
pub fn own() -> Vec<Card> {
    let me = super::username();
    if me.is_empty() {
        return Vec::new();
    }
    LIBRARY.with_borrow(|l| l.iter().filter(|c| c.kind == Kind::Playlist && c.owner == me).cloned().collect())
}

fn in_library(playlist: &str) -> bool {
    LIBRARY.with_borrow(|l| l.iter().any(|c| c.uri == playlist))
}

fn name_of(playlist: &str) -> String {
    LIBRARY
        .with_borrow(|l| l.iter().find(|c| c.uri == playlist).map(|c| c.name.clone()))
        .unwrap_or_else(|| "the playlist".into())
}

/// "Add to Playlist" for a song menu: New Playlist (named after the song),
/// then the user's own.
pub fn add_to_menu(track: &str, name: &str) -> gio::Menu {
    let menu = gio::Menu::new();
    let item = |label: &str, action: &str, target: &str| {
        let item = gio::MenuItem::new(Some(label), None);
        item.set_action_and_target_value(Some(action), Some(&target.to_variant()));
        item
    };
    let top = gio::Menu::new();
    top.append_item(&item("New Playlist", "app.new-playlist-with", &format!("{track}\t{name}")));
    menu.append_section(None, &top);
    let mine = gio::Menu::new();
    for playlist in own().iter().take(40) {
        mine.append_item(&item(&playlist.name, "app.add-to-playlist", &format!("{}\t{track}", playlist.uri)));
    }
    menu.append_section(None, &mine);
    menu
}

/// Adds songs to a playlist (from a menu: "playlist\ttrack").
pub fn add_from_menu(target: &str) {
    if let Some((playlist, track)) = target.split_once('\t') {
        add(playlist, vec![track.to_owned()]);
    }
}

pub fn add(playlist: &str, tracks: Vec<String>) {
    let Some(api) = ctx().api() else { return };
    let playlist = playlist.to_owned();
    glib::spawn_future_local(async move {
        let (asked, songs) = (playlist.clone(), tracks.clone());
        match rt::spawn(async move { api.add_to_playlist(&asked, &songs).await }).await {
            Ok(()) => {
                toast(&format!("Added to {}", name_of(&playlist)));
                super::pages::refresh_list(&playlist);
            }
            Err(e) => toast(&format!("Couldn't add to the playlist: {e}")),
        }
    });
}

/// Takes a song out of a playlist (from a menu: "playlist\tuid").
pub fn remove_from_menu(target: &str) {
    let Some((playlist, uid)) = target.split_once('\t') else { return };
    let Some(api) = ctx().api() else { return };
    super::pages::take_out(playlist, uid);
    let (playlist, uid) = (playlist.to_owned(), uid.to_owned());
    glib::spawn_future_local(async move {
        let asked = playlist.clone();
        let result = rt::spawn(async move { api.remove_from_playlist(&asked, &[uid]).await }).await;
        match result {
            Ok(()) => toast(&format!("Removed from {}", name_of(&playlist))),
            Err(e) => toast(&format!("Couldn't remove it: {e}")),
        }
        super::pages::refresh_list(&playlist);
    });
}

/// Moves a song (by entry uid) to just before `before`, or to the end.
pub fn move_song(playlist: &str, uid: &str, before: Option<String>) {
    let Some(api) = ctx().api() else { return };
    super::pages::move_within(playlist, uid, before.as_deref());
    let (playlist, uid) = (playlist.to_owned(), uid.to_owned());
    glib::spawn_future_local(async move {
        let asked = playlist.clone();
        let result =
            rt::spawn(async move { api.move_in_playlist(&asked, &[uid], before.as_deref()).await }).await;
        if let Err(e) = result {
            toast(&format!("Couldn't move it: {e}"));
            super::pages::refresh_list(&playlist);
        }
    });
}

/// A new playlist: from a song menu ("New Playlist", named after the song,
/// with it in), or from the sidebar's + (named "My Playlist #n", opened and
/// ready to name).
pub fn new_playlist(target: &str) {
    let Some(api) = ctx().api() else { return };
    // From a song menu: "track\tsong name".
    let (track, song) = match target.split_once('\t') {
        Some((uri, name)) => (Some(uri.to_owned()), Some(name.to_owned())),
        None => (None, None),
    };
    let name = song.filter(|s| !s.is_empty()).unwrap_or_else(|| format!("My Playlist #{}", own().len() + 1));
    glib::spawn_future_local(async move {
        let (asked, songs) = (name.clone(), track.clone());
        let made = rt::spawn(async move {
            let uri = api.create_playlist(&asked).await?;
            if let Some(song) = songs {
                api.add_to_playlist(&uri, &[song]).await?;
            }
            Ok::<_, String>(uri)
        })
        .await;
        match made {
            Ok(uri) => {
                reload_library();
                let card = Card {
                    kind: Kind::Playlist,
                    uri: uri.clone(),
                    name: name.clone(),
                    subtitle: String::new(),
                    images: Images::default(),
                    owner: super::username(),
                };
                LIBRARY.with_borrow_mut(|l| l.insert(0, card.clone()));
                if track.is_some() {
                    toast(&format!("Added to {name}"));
                } else {
                    super::open_card(&card);
                    edit_details(&uri, &name, "");
                }
            }
            Err(e) => toast(&format!("Couldn't make the playlist: {e}")),
        }
    });
}

/// Name and description, as Spotify's Edit Details.
pub fn edit_details(playlist: &str, name: &str, description: &str) {
    let name_entry = gtk::Entry::builder().text(name).placeholder_text("Name").activates_default(true).build();
    let description_view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .accepts_tab(false)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(10)
        .right_margin(10)
        .build();
    description_view.buffer().set_text(description);
    description_view.add_css_class("playlist-description");
    let description_box = gtk::ScrolledWindow::builder()
        .child(&description_view)
        .min_content_height(96)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    description_box.add_css_class("card");
    let caption = |text: &str| {
        let label = gtk::Label::builder().label(text).xalign(0.0).build();
        label.add_css_class("caption-heading");
        label.add_css_class("dim-label");
        label
    };
    let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
    form.append(&caption("Name"));
    form.append(&name_entry);
    let description_caption = caption("Description (optional)");
    description_caption.set_margin_top(8);
    form.append(&description_caption);
    form.append(&description_box);
    let dialog = adw::AlertDialog::builder().heading("Edit Details").extra_child(&form).build();
    dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");
    let playlist = playlist.to_owned();
    let entry = name_entry.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "save" {
            return;
        }
        let name = entry.text().trim().to_owned();
        let buffer = description_view.buffer();
        let description = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
        if name.is_empty() {
            toast("A playlist needs a name");
            return;
        }
        let Some(api) = ctx().api() else { return };
        let playlist = playlist.clone();
        glib::spawn_future_local(async move {
            let asked = playlist.clone();
            match rt::spawn(async move { api.edit_playlist(&asked, &name, &description).await }).await {
                Ok(()) => {
                    super::pages::refresh_list(&playlist);
                    reload_library();
                }
                Err(e) => toast(&format!("Couldn't save the details: {e}")),
            }
        });
    });
    // The name has the focus once the dialog is up.
    dialog.set_focus(Some(&name_entry));
    dialog.present(Some(&ctx().window));
}

/// Takes a playlist out of the library: the user's own are deleted.
pub fn delete(playlist: &str, name: &str, own: bool) {
    let (heading, body, verb) = if own {
        ("Delete Playlist?", format!("{name} will be deleted from Your Library."), "Delete")
    } else {
        ("Remove from Your Library?", format!("{name} will be removed from Your Library."), "Remove")
    };
    let dialog = adw::AlertDialog::builder().heading(heading).body(body).build();
    dialog.add_responses(&[("cancel", "Cancel"), ("delete", verb)]);
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_close_response("cancel");
    let playlist = playlist.to_owned();
    dialog.connect_response(None, move |_, response| {
        if response != "delete" {
            return;
        }
        let Some(api) = ctx().api() else { return };
        let playlist = playlist.clone();
        glib::spawn_future_local(async move {
            let asked = playlist.clone();
            match rt::spawn(async move { api.delete_playlist(&asked).await }).await {
                Ok(()) => {
                    LIBRARY.with_borrow_mut(|l| l.retain(|c| c.uri != playlist));
                    ctx().stores.borrow_mut().remove(&playlist);
                    super::left_page(&playlist);
                    reload_library();
                }
                Err(e) => toast(&format!("Couldn't do that: {e}")),
            }
        });
    });
    dialog.present(Some(&ctx().window));
}

/// The playlist page's ⋯ menu: what the user may do with this playlist.
pub fn page_menu(playlist: &str, can_edit_items: bool, can_edit_details: bool) -> gio::Menu {
    let menu = gio::Menu::new();
    let edit = gio::Menu::new();
    if can_edit_items {
        edit.append(Some("Add Songs"), Some("playlist.add-songs"));
    }
    if can_edit_details {
        edit.append(Some("Edit Details"), Some("playlist.edit"));
    }
    menu.append_section(None, &edit);
    let remove = gio::Menu::new();
    if can_edit_details {
        remove.append(Some("Delete Playlist"), Some("playlist.delete"));
    } else if in_library(playlist) {
        remove.append(Some("Remove from Your Library"), Some("playlist.delete"));
    }
    menu.append_section(None, &remove);
    menu
}

/// Add Songs: search Spotify and add songs with their + button.
pub fn add_songs(playlist: &str, name: &str) {
    let entry = gtk::SearchEntry::builder()
        .placeholder_text("Search for songs")
        .hexpand(true)
        .search_delay(200)
        .build();
    entry.add_css_class("search-pill");
    let results = gtk::ListBox::new();
    results.add_css_class("add-songs");
    results.set_selection_mode(gtk::SelectionMode::None);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&results)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.set_margin_bottom(12);
    content.append(&entry);
    content.append(&scroller);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&content));
    let dialog = adw::Dialog::builder()
        .title(format!("Add to {name}"))
        .content_width(520)
        .content_height(640)
        .child(&toolbar)
        .build();

    let playlist = playlist.to_owned();
    let generation: Rc<std::cell::Cell<u64>> = Rc::default();
    entry.connect_search_changed(glib::clone!(
        #[weak]
        results,
        move |entry| {
            let query = entry.text().trim().to_owned();
            let current = generation.get() + 1;
            generation.set(current);
            results.remove_all();
            let Some(api) = ctx().api() else { return };
            if query.is_empty() {
                return;
            }
            let (generation, playlist) = (generation.clone(), playlist.clone());
            glib::spawn_future_local(async move {
                let found = rt::spawn(async move { api.search(&query).await }).await;
                if generation.get() != current {
                    return;
                }
                match found {
                    Ok(found) => {
                        for track in found.tracks.iter().take(20) {
                            results.append(&song_to_add(track, &playlist));
                        }
                    }
                    Err(e) => toast(&format!("Search failed: {e}")),
                }
            });
        }
    ));
    dialog.present(Some(&ctx().window));
    entry.grab_focus();
}

/// A search result with a + that adds it (and turns into a tick).
fn song_to_add(track: &Track, playlist: &str) -> gtk::ListBoxRow {
    let content = gtk::Box::builder().spacing(12).margin_top(6).margin_bottom(6).build();
    let cover = super::cover::Cover::new(44, 8.0);
    cover.set_url(track.images.pick(64));
    content.append(&cover);
    let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).valign(gtk::Align::Center).hexpand(true).build();
    let title = gtk::Label::builder()
        .label(&track.name)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    title.add_css_class("track-title");
    let artists = gtk::Label::builder()
        .label(track.artist_names())
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    artists.add_css_class("track-artists");
    artists.add_css_class("dim-label");
    text.append(&title);
    text.append(&artists);
    content.append(&text);
    let add = gtk::Button::from_icon_name("onify-list-add-symbolic");
    add.add_css_class("flat");
    add.add_css_class("circular");
    add.set_valign(gtk::Align::Center);
    add.set_tooltip_text(Some("Add to this playlist"));
    let (playlist, uri) = (playlist.to_owned(), track.uri.clone());
    add.connect_clicked(move |button| {
        button.set_sensitive(false);
        button.set_icon_name("onify-check-symbolic");
        add_quietly(&playlist, &uri);
    });
    content.append(&add);
    gtk::ListBoxRow::builder().child(&content).activatable(false).build()
}

/// Add Songs adds one at a time without a toast each.
fn add_quietly(playlist: &str, track: &str) {
    let Some(api) = ctx().api() else { return };
    let (playlist, track) = (playlist.to_owned(), track.to_owned());
    glib::spawn_future_local(async move {
        let asked = playlist.clone();
        match rt::spawn(async move { api.add_to_playlist(&asked, &[track]).await }).await {
            Ok(()) => super::pages::refresh_list(&playlist),
            Err(e) => toast(&format!("Couldn't add it: {e}")),
        }
    });
}
