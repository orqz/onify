//! Library, playlists, search and catalog pages.
//!
//! This talks to the same GraphQL API the official Spotify apps use, with the
//! session's own access token. (The public Web API is unusable for librespot
//! clients: its shared quota is permanently exhausted.) Queries are addressed
//! by hashes that Spotify rotates now and then; when one stops working the
//! current set is read back out of the web player and cached on disk.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{Method, Request};
use librespot_core::session::Session;
use librespot_core::spotify_uri::SpotifyUri;
use librespot_metadata::{Metadata, Track as TrackMetadata};
use librespot_protocol::playlist4_external as pl4;
use protobuf::{Message, MessageField};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Semaphore, mpsc::UnboundedSender};

const PATHFINDER: &str = "https://api-partner.spotify.com/pathfinder/v2/query";
const WEB_PLAYER: &str = "https://open.spotify.com/";
const CHUNK_BASE: &str = "https://open.spotifycdn.com/cdn/build/web-player/";

/// Query hashes as of 10 October 2026; refreshed automatically when they rotate.
const DEFAULT_HASHES: &[(&str, &str)] = &[
    ("searchDesktop", "1148393611bbc58e84e47aed35ecc731275df9f9eb660956962e352dd3631d89"),
    ("fetchPlaylist", "8964e8eafb21aa992a7d951d256d83285c04be2105d209262901de70cb97584a"),
    ("getAlbum", "6a74b456cd1735c9193d9e8ec8cc5184cad7ce13572210315229db3975964361"),
    ("getTrack", "a8ef9e9f02b836feb0da3003c31dbb30decc6f4b473ef89ca88c882386d668de"),
    ("queryArtistOverview", "9f8134ef565e78621f1e1793555bd6633c5ac144ae0f89604ed3ae3f80b3c8e6"),
    ("home", "76243c78b0e20ecdbe41b794dec8cbe73f75e585b0a7201b8d2e84578412847a"),
    ("libraryV3", "390c78e5b951029bad359785e69b07b536a509c581cbcd0aded5e5067f187455"),
    ("fetchLibraryTracks", "087278b20b743578a6262c2b0b4bcd20d879c503cc359a2285baf083ef944240"),
    ("areEntitiesInLibrary", "134337999233cc6fdd6b1e6dbf94841409f04a946c5c7b744b09ba0dfe5a85ed"),
    ("addToLibrary", "896ebcb47815681340860d121cb5d494e157e2a78d3950385cd54e0393c67148"),
    ("removeFromLibrary", "896ebcb47815681340860d121cb5d494e157e2a78d3950385cd54e0393c67148"),
    ("addToPlaylist", "b907ad6b088d5bf7680fa81198996d58398c2d55dfcdf59fcc959e0be98a418a"),
    ("removeFromPlaylist", "b907ad6b088d5bf7680fa81198996d58398c2d55dfcdf59fcc959e0be98a418a"),
    ("moveItemsInPlaylist", "b907ad6b088d5bf7680fa81198996d58398c2d55dfcdf59fcc959e0be98a418a"),
];

static HASHES: LazyLock<RwLock<HashMap<String, String>>> = LazyLock::new(|| {
    let mut map: HashMap<String, String> = DEFAULT_HASHES
        .iter()
        .map(|(op, hash)| (op.to_string(), hash.to_string()))
        .collect();
    if let Some(saved) = std::fs::read(hashes_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<HashMap<String, String>>(&b).ok())
    {
        map.extend(saved);
    }
    RwLock::new(map)
});
static LAST_REFRESH: Mutex<Option<Instant>> = Mutex::const_new(None);

fn hashes_path() -> std::path::PathBuf {
    crate::spotify::cache_dir().join("graphql.json")
}

pub type Result<T> = std::result::Result<T, String>;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Named {
    pub name: String,
    pub uri: String,
}

/// Image URLs with their widths, smallest first.
#[derive(Debug, Clone, Default)]
pub struct Images(Vec<(String, u32)>);

impl Images {
    /// Parses a GraphQL `sources` array.
    fn parse(sources: &Value) -> Self {
        let mut list: Vec<(String, u32)> = sources
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| {
                let url = i["url"].as_str()?.to_owned();
                // Mosaic playlist covers come without a size; they are 640px.
                let w = i["width"].as_u64().or(i["maxWidth"].as_u64()).unwrap_or(640) as u32;
                Some((url, w))
            })
            .collect();
        list.sort_by_key(|i| i.1);
        Self(list)
    }

    /// One image, e.g. a local file's embedded cover.
    pub fn single(url: String, width: u32) -> Self {
        Self(vec![(url, width)])
    }

    /// The smallest image at least `px` wide, or the largest there is.
    pub fn pick(&self, px: u32) -> Option<&str> {
        self.0
            .iter()
            .find(|i| i.1 >= px)
            .or(self.0.last())
            .map(|i| i.0.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Track {
    pub uri: String,
    pub name: String,
    pub artists: Vec<Named>,
    pub album: Named,
    pub images: Images,
    pub duration_ms: u32,
    pub explicit: bool,
    pub playable: bool,
    pub number: u32,
    /// Streams, where Spotify reports them (an artist's popular songs).
    pub plays: u64,
    /// Where it sits in its list, from 1 (set when listed; see track_row::objects).
    pub position: u32,
    /// Its entry in a playlist (the same song can be in one twice), for
    /// removing and moving it.
    pub uid: String,
}

impl Track {
    pub fn artist_names(&self) -> String {
        join_names(&self.artists)
    }

    /// A local file in a playlist. Its URI carries everything:
    /// `spotify:local:{artist}:{album}:{title}:{seconds}`, each part form-encoded.
    pub fn from_local_uri(uri: &str) -> Option<Self> {
        let decode = |part: &str| -> String {
            url::form_urlencoded::parse(part.as_bytes())
                .next()
                .map(|(k, _)| k.into_owned())
                .unwrap_or_default()
        };
        let parts: Vec<&str> = uri.strip_prefix("spotify:local:")?.split(':').collect();
        let [artist, album, title, seconds] = parts.as_slice() else { return None };
        let artist = decode(artist);
        Some(Self {
            uri: uri.to_owned(),
            name: decode(title),
            artists: [artist]
                .into_iter()
                .filter(|a| !a.is_empty())
                .map(|name| Named { name, uri: String::new() })
                .collect(),
            album: Named {
                name: decode(album),
                uri: String::new(),
            },
            images: Images::default(),
            duration_ms: seconds.parse::<u32>().unwrap_or(0).saturating_mul(1000),
            explicit: false,
            playable: true,
            number: 0,
            plays: 0,
            position: 0,
            uid: String::new(),
        })
    }

    /// Parses a `Track`, or a `TrackResponseWrapper` around one.
    fn parse(v: &Value, album: Option<(&Named, &Images)>) -> Option<Self> {
        let (t, wrapper_uri) = if v.get("data").is_some() {
            (&v["data"], v["_uri"].as_str())
        } else {
            (v, None)
        };
        if t.is_null() {
            return None;
        }
        let kind = t["__typename"].as_str();
        let uri = t["uri"].as_str().or(wrapper_uri)?.to_owned();
        if kind == Some("LocalTrack") || uri.starts_with("spotify:local:") {
            let mut track = Self::from_local_uri(&uri)?;
            if let Some(name) = t["name"].as_str().filter(|n| !n.is_empty()) {
                track.name = name.to_owned();
            }
            return Some(track);
        }
        if kind.is_some_and(|k| k != "Track") {
            return None;
        }
        let (album, images) = match album {
            Some((a, i)) => (a.clone(), i.clone()),
            None => (
                Named {
                    name: str_of(&t["albumOfTrack"]["name"]),
                    uri: str_of(&t["albumOfTrack"]["uri"]),
                },
                Images::parse(&t["albumOfTrack"]["coverArt"]["sources"]),
            ),
        };
        let duration = &t["duration"]["totalMilliseconds"];
        let duration = duration.as_u64().or(t["trackDuration"]["totalMilliseconds"].as_u64());
        Some(Self {
            playable: t["playability"]["playable"].as_bool().unwrap_or(true),
            name: str_of(&t["name"]),
            artists: artists(&t["artists"]),
            album,
            images,
            duration_ms: duration.unwrap_or(0) as u32,
            explicit: t["contentRating"]["label"] == "EXPLICIT",
            number: t["trackNumber"].as_u64().unwrap_or(0) as u32,
            plays: t["playcount"]
                .as_str()
                .and_then(|p| p.parse().ok())
                .or(t["playcount"].as_u64())
                .unwrap_or(0),
            position: 0,
            uid: String::new(),
            uri,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Playlist,
    Album,
    Artist,
    Liked,
    /// Songs from the music folders on this computer.
    Local,
}

/// Anything that opens a page: a playlist, album or artist.
#[derive(Debug, Clone)]
pub struct Card {
    pub kind: Kind,
    pub uri: String,
    pub name: String,
    pub subtitle: String,
    pub images: Images,
    /// A playlist's owner (their username), so onify knows which are the
    /// user's own to add songs to.
    pub owner: String,
}

impl Card {
    pub fn id(&self) -> &str {
        id_of(&self.uri)
    }

    /// Parses a `Playlist`, `Album` or `Artist`, or a response wrapper around one.
    fn parse(v: &Value) -> Option<Self> {
        let (d, wrapper_uri) = if v.get("data").is_some() {
            (&v["data"], v["_uri"].as_str())
        } else {
            (v, None)
        };
        let uri = d["uri"].as_str().or(wrapper_uri).unwrap_or_default().to_owned();
        let card = match d["__typename"].as_str()? {
            "Playlist" => {
                let description = strip_tags(&str_of(&d["description"]));
                let owner = str_of(&d["ownerV2"]["data"]["name"]);
                Self {
                    kind: Kind::Playlist,
                    uri,
                    name: str_of(&d["name"]),
                    subtitle: if !description.is_empty() {
                        description
                    } else if !owner.is_empty() {
                        format!("By {owner}")
                    } else {
                        "Playlist".into()
                    },
                    images: Images::parse(&d["images"]["items"][0]["sources"]),
                    owner: str_of(&d["ownerV2"]["data"]["username"]),
                }
            }
            "Album" => {
                let year = d["date"]["year"]
                    .as_u64()
                    .map(|y| y.to_string())
                    .unwrap_or_else(|| str_of(&d["date"]["isoString"]).chars().take(4).collect());
                let artists = join_names(&artists(&d["artists"]));
                Self {
                    kind: Kind::Album,
                    uri,
                    name: str_of(&d["name"]),
                    subtitle: [year, artists]
                        .into_iter()
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" • "),
                    images: Images::parse(&d["coverArt"]["sources"]),
                    owner: String::new(),
                }
            }
            "Artist" => Self {
                kind: Kind::Artist,
                uri,
                name: str_of(&d["profile"]["name"]),
                subtitle: "Artist".into(),
                images: Images::parse(&d["visuals"]["avatarImage"]["sources"]),
                owner: String::new(),
            },
            _ => return None,
        };
        (!card.uri.is_empty() && !card.name.is_empty()).then_some(card)
    }
}

impl Card {
    /// A release in an artist's discography, which (unlike everywhere else)
    /// comes without a `__typename`: "2026 • Single".
    fn release(d: &Value) -> Option<Self> {
        let (uri, name) = (str_of(&d["uri"]), str_of(&d["name"]));
        if !uri.starts_with("spotify:album:") || name.is_empty() {
            return None;
        }
        let kind = match d["type"].as_str() {
            Some("SINGLE") => "Single",
            Some("EP") => "EP",
            Some("COMPILATION") => "Compilation",
            _ => "Album",
        };
        let year = d["date"]["year"].as_u64().map(|y| y.to_string()).unwrap_or_default();
        Some(Self {
            kind: Kind::Album,
            uri,
            name,
            subtitle: [year, kind.to_owned()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" • "),
            images: Images::parse(&d["coverArt"]["sources"]),
            owner: String::new(),
        })
    }
}

/// A song's lyrics: each line with the time it starts, if they're synced.
#[derive(Debug, Clone)]
pub struct Lyrics {
    pub synced: bool,
    pub lines: Vec<(u32, String)>,
    pub provider: String,
}

/// What a track list page shows above its tracks.
#[derive(Debug, Clone, Default)]
pub struct Header {
    pub title: String,
    pub subtitle: String,
    pub images: Images,
    /// An album's artists, which start its subtitle and link to their pages.
    pub artists: Vec<Named>,
    /// A playlist's own description (its subtitle shows the owner instead).
    pub description: String,
    /// The playlist is the user's (or they collaborate on it): songs can be
    /// added, removed and moved.
    pub can_edit_items: bool,
    /// Its name and description can be changed, and it can be deleted.
    pub can_edit_details: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub tracks: Vec<Track>,
    pub artists: Vec<Card>,
    pub albums: Vec<Card>,
    pub playlists: Vec<Card>,
}

#[derive(Debug, Clone, Default)]
pub struct ArtistPage {
    pub name: String,
    pub followers: u64,
    pub monthly_listeners: u64,
    pub images: Images,
    pub top: Vec<Track>,
    /// The newest album, single or EP.
    pub latest: Option<Card>,
    /// Releases people play most, albums and singles alike.
    pub popular: Vec<Card>,
    pub albums: Vec<Card>,
    pub singles: Vec<Card>,
}

/// Spotify's own home feed: titled rows of playlists, albums and artists.
#[derive(Debug, Clone, Default)]
pub struct Home {
    pub sections: Vec<(String, Vec<Card>)>,
}

/// Messages from a streaming track list load.
pub enum Chunk {
    Header(Header),
    Tracks(Vec<Track>),
    Failed(String),
}

/// Answers worth keeping on disk: shown instantly next time, then refreshed.
const CACHED: &[&str] = &["libraryV3", "home", "fetchPlaylist", "getAlbum", "fetchLibraryTracks", "queryArtistOverview"];

fn cache_file(operation: &str, variables: &Value) -> std::path::PathBuf {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    (operation, variables.to_string()).hash(&mut hasher);
    crate::spotify::cache_dir().join("api").join(format!("{:016x}.json", hasher.finish()))
}

#[derive(Clone)]
pub struct Api {
    /// `None` for `cache_only`, before Spotify has connected.
    session: Option<Session>,
    /// Answer from the disk cache only, never the network.
    offline: bool,
}

impl Api {
    pub fn new(session: Session) -> Self {
        Self { session: Some(session), offline: false }
    }

    /// Answers from what was saved last time, instantly and without the
    /// network, even before Spotify has connected. Anything not saved fails
    /// with "not cached".
    pub fn cache_only() -> Self {
        Self { session: None, offline: true }
    }

    fn session(&self) -> Result<&Session> {
        self.session.as_ref().ok_or_else(|| "not connected".to_owned())
    }

    async fn get_text(&self, url: &str) -> Result<String> {
        let request = Request::get(url).body(Bytes::new()).map_err(|e| e.to_string())?;
        let body = self.session()?.http_client().request_body(request).await.map_err(|e| e.to_string())?;
        String::from_utf8(body.to_vec()).map_err(|e| e.to_string())
    }

    /// Re-reads the current query hashes out of Spotify's web player.
    async fn refresh_hashes(&self) {
        let mut last = LAST_REFRESH.lock().await;
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(300)) {
            return;
        }
        *last = Some(Instant::now());
        log::info!("refreshing GraphQL query hashes from the web player");

        let Ok(html) = self.get_text(WEB_PLAYER).await else { return };
        let mut found = HashMap::new();
        for script in find_scripts(&html) {
            let Ok(js) = self.get_text(&script).await else { continue };
            extract_hashes(&js, &mut found);
            // Search lives in a lazily loaded chunk.
            for chunk in chunk_files(&js, "xpui-routes-search") {
                if let Ok(js) = self.get_text(&format!("{CHUNK_BASE}{chunk}")).await {
                    extract_hashes(&js, &mut found);
                }
            }
        }
        if found.is_empty() {
            log::warn!("couldn't find any GraphQL hashes in the web player");
            return;
        }
        let mut map = HASHES.write().unwrap();
        map.extend(found);
        let _ = std::fs::create_dir_all(crate::spotify::cache_dir());
        let _ = std::fs::write(hashes_path(), serde_json::to_vec(&*map).unwrap_or_default());
    }

    async fn query(&self, operation: &str, variables: Value) -> Result<Value> {
        let file = cache_file(operation, &variables);
        if self.offline {
            let bytes = tokio::fs::read(&file).await.map_err(|_| "not cached".to_owned())?;
            return serde_json::from_slice(&bytes).map_err(|e| e.to_string());
        }
        let started = Instant::now();
        let result = self.query_inner(operation, variables).await;
        log::debug!("{operation} took {} ms", started.elapsed().as_millis());
        if let (Ok(data), true) = (&result, CACHED.contains(&operation)) {
            if let Some(dir) = file.parent() {
                let _ = tokio::fs::create_dir_all(dir).await;
            }
            let _ = tokio::fs::write(&file, data.to_string()).await;
        }
        result
    }

    async fn query_inner(&self, operation: &str, variables: Value) -> Result<Value> {
        for attempt in 0..2 {
            let hash = HASHES.read().unwrap().get(operation).cloned().unwrap_or_default();
            let token = self.session()?.login5().auth_token().await.map_err(|e| e.to_string())?;
            let client_token = self.session()?.spclient().client_token().await.map_err(|e| e.to_string())?;
            let body = json!({
                "variables": variables,
                "operationName": operation,
                "extensions": { "persistedQuery": { "version": 1, "sha256Hash": hash } },
            });
            let request = Request::builder()
                .method(Method::POST)
                .uri(PATHFINDER)
                .header("Authorization", format!("Bearer {}", token.access_token))
                .header("client-token", client_token)
                .header("Content-Type", "application/json;charset=UTF-8")
                .header("Accept", "application/json")
                .header("App-Platform", "WebPlayer")
                .body(Bytes::from(body.to_string()))
                .map_err(|e| e.to_string())?;
            let response = self.session()?.http_client().request_body(request);
            let response = tokio::time::timeout(Duration::from_secs(20), response)
                .await
                .map_err(|_| "Spotify took too long to answer".to_owned())?;

            let stale = match &response {
                Ok(body) => {
                    let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
                    if !v["data"].is_null() {
                        return Ok(v["data"].clone());
                    }
                    if !v["errors"].to_string().contains("PersistedQueryNotFound") {
                        return Err(first_error(&v));
                    }
                    true
                }
                Err(e) => e.to_string().contains("400") || e.to_string().contains("404"),
            };
            if stale && attempt == 0 {
                self.refresh_hashes().await;
                continue;
            }
            return response.map(|_| Value::Null).map_err(|e| e.to_string());
        }
        Err("query failed".into())
    }

    pub fn is_premium(&self) -> bool {
        self.session
            .as_ref()
            .and_then(|s| s.get_user_attribute("type"))
            .is_none_or(|t| t == "premium")
    }

    /// Everything in the user's library: playlists (folders flattened),
    /// saved albums and followed artists.
    pub async fn library(&self) -> Result<Vec<Card>> {
        let mut cards = Vec::new();
        let mut offset = 0;
        loop {
            let v = self
                .query(
                    "libraryV3",
                    json!({
                        "filters": [], "order": null, "textFilter": "",
                        "features": ["LIKED_SONGS", "YOUR_EPISODES"],
                        "limit": 50, "offset": offset,
                        "flatten": true, "expandedFolders": [], "folderUri": null,
                        "includeFoldersWhenFlattening": false,
                    }),
                )
                .await?;
            let page = &v["me"]["libraryV3"];
            let items = page["items"].as_array().cloned().unwrap_or_default();
            cards.extend(items.iter().filter_map(|i| Card::parse(&i["item"])));
            offset += items.len();
            let total = page["totalCount"].as_u64().unwrap_or(0) as usize;
            if items.is_empty() || offset >= total {
                return Ok(cards);
            }
        }
    }

    pub async fn home(&self) -> Result<Home> {
        let time_zone = gtk::glib::TimeZone::local().identifier().to_string();
        let v = self
            .query(
                "home",
                json!({
                    "homeEndUserIntegration": "INTEGRATION_WEB_PLAYER",
                    "timeZone": time_zone, "sp_t": "", "facet": "",
                    "sectionItemsLimit": 12, "includeEpisodeContentRatingsV2": false,
                }),
            )
            .await?;
        let sections = v["home"]["sectionContainer"]["sections"]["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|section| {
                let title = section["data"]["title"]["transformedLabel"]
                    .as_str()
                    .or(section["data"]["title"]["text"].as_str())?
                    .to_owned();
                let cards: Vec<Card> = section["sectionItems"]["items"]
                    .as_array()?
                    .iter()
                    .filter_map(|item| Card::parse(&item["content"]))
                    .collect();
                (!cards.is_empty()).then_some((title, cards))
            })
            .collect();
        Ok(Home { sections })
    }

    pub async fn search(&self, query: &str) -> Result<SearchResults> {
        let v = self
            .query(
                "searchDesktop",
                json!({
                    "searchTerm": query, "offset": 0, "limit": 10, "numberOfTopResults": 5,
                    "includeAudiobooks": false, "includeArtistHasConcertsField": false,
                    "includePreReleases": false, "includeLocalConcertsField": false,
                    "includeAuthors": false,
                }),
            )
            .await?;
        let s = &v["searchV2"];
        let cards = |v: &Value| -> Vec<Card> {
            v["items"].as_array().into_iter().flatten().filter_map(Card::parse).collect()
        };
        Ok(SearchResults {
            tracks: s["tracksV2"]["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|i| Track::parse(&i["item"], None))
                .collect(),
            artists: cards(&s["artists"]),
            albums: cards(&s["albumsV2"]),
            playlists: cards(&s["playlists"]),
        })
    }

    pub async fn artist(&self, id: &str) -> Result<ArtistPage> {
        let v = self
            .query(
                "queryArtistOverview",
                json!({ "uri": format!("spotify:artist:{id}"), "locale": "", "includePrerelease": true }),
            )
            .await?;
        let a = &v["artistUnion"];
        let d = &a["discography"];
        let releases = |v: &Value| -> Vec<Card> {
            v["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|i| Card::release(&i["releases"]["items"][0]))
                .collect()
        };
        let popular: Vec<Card> =
            d["popularReleasesAlbums"]["items"].as_array().into_iter().flatten().filter_map(Card::release).collect();
        let mut albums = releases(&d["albums"]);
        if albums.is_empty() {
            albums = popular.iter().filter(|c| c.subtitle.ends_with("Album")).cloned().collect();
        }
        Ok(ArtistPage {
            latest: Card::release(&d["latest"]),
            popular,
            name: str_of(&a["profile"]["name"]),
            followers: a["stats"]["followers"].as_u64().unwrap_or(0),
            monthly_listeners: a["stats"]["monthlyListeners"].as_u64().unwrap_or(0),
            images: Images::parse(&a["visuals"]["avatarImage"]["sources"]),
            top: a["discography"]["topTracks"]["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|i| Track::parse(&i["track"], None))
                .collect(),
            albums,
            singles: releases(&d["singles"]),
        })
    }

    /// A whole track list at once: its header and every track.
    pub async fn all_tracks(self, kind: Kind, id: String) -> Result<(Option<Header>, Vec<Track>)> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(self.stream_tracks(kind, id, tx));
        let (mut header, mut tracks) = (None, Vec::new());
        while let Some(chunk) = rx.recv().await {
            match chunk {
                Chunk::Header(h) => header = Some(h),
                Chunk::Tracks(t) => tracks.extend(t),
                Chunk::Failed(e) => return Err(e),
            }
        }
        Ok((header, tracks))
    }

    /// Streams a track list page by page, so long lists appear immediately.
    pub async fn stream_tracks(self, kind: Kind, id: String, tx: UnboundedSender<Chunk>) {
        if let Err(e) = self.stream_tracks_inner(kind, &id, &tx).await {
            let _ = tx.send(Chunk::Failed(e));
        }
    }

    async fn page(&self, kind: Kind, id: &str, offset: usize, limit: usize) -> Result<Value> {
        match kind {
            Kind::Playlist => {
                self.query(
                    "fetchPlaylist",
                    json!({
                        "uri": format!("spotify:playlist:{id}"), "offset": offset, "limit": limit,
                        "enableWatchFeedEntrypoint": false,
                    }),
                )
                .await
            }
            Kind::Album => {
                self.query(
                    "getAlbum",
                    json!({ "uri": format!("spotify:album:{id}"), "locale": "", "offset": offset, "limit": limit }),
                )
                .await
            }
            Kind::Liked => self.query("fetchLibraryTracks", json!({ "offset": offset, "limit": limit })).await,
            Kind::Artist | Kind::Local => Err("not a Spotify track list".into()),
        }
    }

    async fn stream_tracks_inner(&self, kind: Kind, id: &str, tx: &UnboundedSender<Chunk>) -> Result<()> {
        let send = |chunk| tx.send(chunk).map_err(|_| "page closed".to_owned());
        // A small first page shows sooner; the rest come in bigger pages.
        let limit = if kind == Kind::Playlist { 100 } else { 50 };
        let first = self.page(kind, id, 0, 50).await?;

        // Where the tracks live in each kind of response, and how to read them.
        let tracks_of = move |v: &Value, album: Option<(&Named, &Images)>| -> (Vec<Track>, usize) {
            let (items, field): (&Value, &str) = match kind {
                Kind::Playlist => (&v["playlistV2"]["content"]["items"], "itemV2"),
                Kind::Album => (&v["albumUnion"]["tracksV2"]["items"], "track"),
                _ => (&v["me"]["library"]["tracks"]["items"], "track"),
            };
            let items = items.as_array().map(Vec::as_slice).unwrap_or_default();
            let tracks = items
                .iter()
                .filter_map(|i| {
                    let mut track = Track::parse(&i[field], album)?;
                    track.uid = str_of(&i["uid"]);
                    Some(track)
                })
                .collect();
            (tracks, items.len())
        };

        let (total, header, album) = match kind {
            Kind::Playlist => {
                let p = &first["playlistV2"];
                if p["__typename"] != "Playlist" {
                    return Err("this playlist isn't available".into());
                }
                let total = p["content"]["totalCount"].as_u64().unwrap_or(0) as usize;
                let owner = str_of(&p["ownerV2"]["data"]["name"]);
                let subtitle = [owner, songs(total)].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>();
                let capabilities = &p["currentUserCapabilities"];
                let header = Header {
                    title: str_of(&p["name"]),
                    subtitle: subtitle.join(" • "),
                    images: Images::parse(&p["images"]["items"][0]["sources"]),
                    artists: Vec::new(),
                    description: strip_tags(&str_of(&p["description"])),
                    can_edit_items: capabilities["canEditItems"].as_bool().unwrap_or(false),
                    can_edit_details: capabilities["canEditMetadata"].as_bool().unwrap_or(false),
                };
                (total, header, None)
            }
            Kind::Album => {
                let a = &first["albumUnion"];
                let total = a["tracksV2"]["totalCount"].as_u64().unwrap_or(0) as usize;
                let year: String = str_of(&a["date"]["isoString"]).chars().take(4).collect();
                let album_artists = artists(&a["artists"]);
                let subtitle = [join_names(&album_artists), year, songs(total)];
                let named = Named {
                    name: str_of(&a["name"]),
                    uri: str_of(&a["uri"]),
                };
                let images = Images::parse(&a["coverArt"]["sources"]);
                let header = Header {
                    title: named.name.clone(),
                    subtitle: subtitle.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" • "),
                    images: images.clone(),
                    artists: album_artists,
                    ..Default::default()
                };
                (total, header, Some((named, images)))
            }
            _ => {
                let total = first["me"]["library"]["tracks"]["totalCount"].as_u64().unwrap_or(0) as usize;
                let header = Header {
                    title: "Liked Songs".into(),
                    subtitle: songs(total),
                    ..Default::default()
                };
                (total, header, None)
            }
        };
        send(Chunk::Header(header))?;
        let album_ref = album.as_ref().map(|(n, i)| (n, i));
        let (tracks, count) = tracks_of(&first, album_ref);
        send(Chunk::Tracks(tracks))?;
        if count == 0 {
            return Ok(());
        }

        // Fetch the remaining pages in parallel, deliver them in order.
        let permits = Arc::new(Semaphore::new(4));
        let pages: Vec<_> = (count..total)
            .step_by(limit)
            .map(|offset| {
                let (api, id, permits) = (self.clone(), id.to_owned(), permits.clone());
                tokio::spawn(async move {
                    let _permit = permits.acquire().await;
                    api.page(kind, &id, offset, limit).await
                })
            })
            .collect();
        for page in pages {
            let v = page.await.map_err(|e| e.to_string())??;
            send(Chunk::Tracks(tracks_of(&v, album_ref).0))?;
        }
        Ok(())
    }

    pub async fn is_liked(&self, uri: &str) -> Result<bool> {
        let v = self.query("areEntitiesInLibrary", json!({ "uris": [uri] })).await?;
        Ok(v["lookup"][0]["data"]["saved"].as_bool().unwrap_or(false))
    }

    pub async fn set_liked(&self, uri: &str, liked: bool) -> Result<()> {
        let operation = if liked { "addToLibrary" } else { "removeFromLibrary" };
        self.query(operation, json!({ "libraryItemUris": [uri] })).await.map(drop)
    }

    /// Queues a track on this device through Spotify Connect.
    pub async fn add_to_queue(&self, uri: &str) -> Result<()> {
        let device = self.session()?.device_id().to_owned();
        let body = json!({
            "command": {
                "endpoint": "add_to_queue",
                "track": { "uri": uri, "metadata": { "is_queued": "true" }, "provider": "queue" },
                "logging_params": {},
            }
        });
        self.session()?
            .spclient()
            .request_as_json(
                &Method::POST,
                &format!("/connect-state/v1/player/command/from/{device}/to/{device}"),
                None,
                Some(&body.to_string()),
            )
            .await
            .map(drop)
            .map_err(|e| e.to_string())
    }

    /// Adds songs to the end of a playlist.
    pub async fn add_to_playlist(&self, playlist: &str, tracks: &[String]) -> Result<()> {
        let variables = json!({
            "playlistUri": playlist,
            "playlistItemUris": tracks,
            "newPosition": { "moveType": "BOTTOM_OF_PLAYLIST", "fromUid": null },
            "interactionId": null,
        });
        self.query("addToPlaylist", variables).await.map(drop)
    }

    /// Takes songs (by their entries' uids) out of a playlist.
    pub async fn remove_from_playlist(&self, playlist: &str, uids: &[String]) -> Result<()> {
        self.query("removeFromPlaylist", json!({ "playlistUri": playlist, "uids": uids })).await.map(drop)
    }

    /// Moves songs (by uid) to just before the entry `before`, or to the end.
    pub async fn move_in_playlist(&self, playlist: &str, uids: &[String], before: Option<&str>) -> Result<()> {
        let position = match before {
            Some(uid) => json!({ "moveType": "BEFORE_UID", "fromUid": uid }),
            None => json!({ "moveType": "BOTTOM_OF_PLAYLIST", "fromUid": null }),
        };
        self.query("moveItemsInPlaylist", json!({ "playlistUri": playlist, "uids": uids, "newPosition": position }))
            .await
            .map(drop)
    }

    /// Sends a playlist4 message (`Delta`, `ListChanges`) to the playlist
    /// service, as Spotify's desktop app does, and returns its answer.
    async fn playlist_service(&self, endpoint: &str, message: &impl Message) -> Result<Bytes> {
        use http_body_util::BodyExt;
        let session = self.session()?;
        let body = message.write_to_bytes().map_err(|e| e.to_string())?;
        let base = session.spclient().base_url().await.map_err(|e| e.to_string())?;
        let token = session.login5().auth_token().await.map_err(|e| e.to_string())?;
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(format!("{base}{endpoint}"))
            .header("Authorization", format!("Bearer {}", token.access_token))
            .header("Content-Type", "application/x-protobuf")
            .header("Accept", "application/x-protobuf");
        if let Ok(client_token) = session.spclient().client_token().await {
            request = request.header("client-token", client_token);
        }
        let request = request.body(Bytes::from(body)).map_err(|e| e.to_string())?;
        let response = session.http_client().request_fut(request).map_err(|e| e.to_string())?;
        let response = tokio::time::timeout(Duration::from_secs(20), response)
            .await
            .map_err(|_| "Spotify took too long to answer".to_owned())?
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let bytes = response.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        if status.is_success() {
            return Ok(bytes);
        }
        log::warn!("playlist service {endpoint}: {status} {}", String::from_utf8_lossy(&bytes));
        Err(format!("Spotify said {status}"))
    }

    /// Developer aid: a playlist's songs as "name (uid)", in order.
    pub async fn describe_playlist(&self, playlist: &str) -> Vec<(String, String)> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        self.clone().stream_tracks(Kind::Playlist, id_of(playlist).to_owned(), tx).await;
        let mut songs = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            if let Chunk::Tracks(tracks) = chunk {
                songs.extend(tracks.into_iter().map(|t| (t.name, t.uid)));
            }
        }
        songs
    }

    /// Developer aid: the start of the user's rootlist, as the service has it.
    pub async fn describe_rootlist(&self) -> Result<String> {
        let user = self.session()?.username();
        let user = url::form_urlencoded::byte_serialize(user.as_bytes()).collect::<String>();
        let bytes = self
            .session()?
            .spclient()
            .request_as_json(&Method::GET, &format!("/playlist/v2/user/{user}/rootlist?decorate=revision,length,attributes&from=0&length=6"), None, None)
            .await
            .map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&bytes).chars().take(3000).collect())
    }

    /// Changes to the user's list of playlists (their library's playlists).
    async fn rootlist_changes(&self, op: pl4::Op) -> Result<()> {
        let user = self.session()?.username();
        let user = url::form_urlencoded::byte_serialize(user.as_bytes()).collect::<String>();
        self.playlist_service(&format!("/playlist/v2/user/{user}/rootlist/changes"), &list_changes(op)).await.map(drop)
    }

    /// Makes a new, empty playlist at the top of the user's library and
    /// returns its uri: the playlist first, then an entry for it in the
    /// user's list of playlists (their "rootlist").
    pub async fn create_playlist(&self, name: &str) -> Result<String> {
        let mut attributes = pl4::ListAttributes::new();
        attributes.set_name(name.to_owned());
        let mut request = pl4::ListUpdateRequest::new();
        request.attributes = MessageField::some(attributes);
        let reply = self.playlist_service("/playlist/v2/playlist", &request).await?;
        let made = pl4::CreateListReply::parse_from_bytes(&reply).map_err(|e| e.to_string())?;
        let uri = made.uri().to_owned();
        log::debug!("created {uri}");
        if !uri.starts_with("spotify:playlist:") {
            return Err("Spotify didn't make the playlist".into());
        }
        // The service makes it without a name whatever the request says;
        // naming it is a change like Edit Details.
        self.edit_playlist(&uri, name, "").await?;
        let mut item = pl4::Item::new();
        item.set_uri(uri.clone());
        let mut item_attributes = pl4::ItemAttributes::new();
        item_attributes.set_timestamp(now_ms());
        item.attributes = MessageField::some(item_attributes);
        let mut add = pl4::Add::new();
        add.set_add_first(true);
        add.items.push(item);
        let mut op = pl4::Op::new();
        op.set_kind(pl4::op::Kind::ADD);
        op.add = MessageField::some(add);
        self.rootlist_changes(op).await?;
        Ok(uri)
    }

    /// Renames a playlist and sets its description (empty clears it).
    pub async fn edit_playlist(&self, playlist: &str, name: &str, description: &str) -> Result<()> {
        let mut attributes = pl4::ListAttributes::new();
        attributes.set_name(name.to_owned());
        let mut cleared = Vec::new();
        match description.trim() {
            "" => cleared.push(pl4::ListAttributeKind::LIST_DESCRIPTION),
            text => attributes.set_description(text.to_owned()),
        }
        let changes = list_changes(list_attributes_op(attributes, cleared));
        let endpoint = format!("/playlist/v2/playlist/{}/changes", id_of(playlist));
        self.playlist_service(&endpoint, &changes).await.map(drop)
    }

    /// Takes a playlist out of the user's library; for their own playlists
    /// that's Spotify's "Delete".
    pub async fn delete_playlist(&self, playlist: &str) -> Result<()> {
        let mut item = pl4::Item::new();
        item.set_uri(playlist.to_owned());
        let mut rem = pl4::Rem::new();
        rem.set_items_as_key(true);
        rem.items.push(item);
        let mut op = pl4::Op::new();
        op.set_kind(pl4::op::Kind::REM);
        op.rem = MessageField::some(rem);
        self.rootlist_changes(op).await
    }

    /// Sends a Spotify Connect command to another of the account's devices:
    /// "pause", "resume", "skip_next", "skip_prev", "seek_to" (with "value"),
    /// "set_shuffling_context" / "set_repeating_*" (with a bool "value").
    pub async fn remote(&self, to: &str, endpoint: &str, extra: Value) -> Result<()> {
        let me = self.session()?.device_id().to_owned();
        let mut command = json!({ "endpoint": endpoint, "logging_params": {} });
        if let (Some(command), Some(extra)) = (command.as_object_mut(), extra.as_object()) {
            command.extend(extra.clone());
        }
        let body = json!({ "command": command });
        self.session()?
            .spclient()
            .request_as_json(
                &Method::POST,
                &format!("/connect-state/v1/player/command/from/{me}/to/{to}"),
                None,
                Some(&body.to_string()),
            )
            .await
            .map(drop)
            .map_err(|e| e.to_string())
    }

    /// Sets another device's volume (0 to 65535).
    pub async fn remote_volume(&self, to: &str, volume: u16) -> Result<()> {
        let me = self.session()?.device_id().to_owned();
        let body = json!({ "volume": volume });
        self.session()?
            .spclient()
            .request_as_json(
                &Method::PUT,
                &format!("/connect-state/v1/connect/volume/from/{me}/to/{to}"),
                None,
                Some(&body.to_string()),
            )
            .await
            .map(drop)
            .map_err(|e| e.to_string())
    }

    /// Moves playback from one device to another, paused or playing as it was.
    pub async fn transfer(&self, from: &str, to: &str) -> Result<()> {
        use librespot_core::dealer::protocol::TransferOptions;
        use librespot_core::spclient::TransferRequest;
        let request = TransferRequest {
            transfer_options: TransferOptions { restore_paused: Some("restore".into()), ..Default::default() },
        };
        self.session()?
            .spclient()
            .transfer(from, to, Some(&request))
            .await
            .map(drop)
            .map_err(|e| e.to_string())
    }

    /// The song's lyrics from Spotify (Musixmatch), or `None` if it has none.
    pub async fn lyrics(&self, track_uri: &str) -> Result<Option<Lyrics>> {
        let endpoint = format!(
            "/color-lyrics/v2/track/{}?format=json&vocalRemoval=false&market=from_token",
            id_of(track_uri)
        );
        let mut headers = http::HeaderMap::new();
        headers.insert("app-platform", http::HeaderValue::from_static("WebPlayer"));
        let bytes = match self.session()?.spclient().request_as_json(&Method::GET, &endpoint, Some(headers), None).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind == librespot_core::error::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        if bytes.is_empty() {
            return Ok(None);
        }
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let l = &v["lyrics"];
        let lines: Vec<(u32, String)> = l["lines"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|line| {
                let start = line["startTimeMs"].as_str().and_then(|t| t.parse().ok()).unwrap_or(0);
                (start, str_of(&line["words"]))
            })
            .collect();
        if lines.is_empty() {
            return Ok(None);
        }
        Ok(Some(Lyrics {
            synced: l["syncType"] == "LINE_SYNCED",
            lines,
            provider: str_of(&l["providerDisplayName"]),
        }))
    }

    /// How many times a song has been played on Spotify. Playlists don't
    /// include it, so it's asked for when a song is selected.
    pub async fn plays(&self, track_uri: &str) -> Result<u64> {
        let v = self.query("getTrack", json!({ "uri": track_uri })).await?;
        let count = &v["trackUnion"]["playcount"];
        count
            .as_str()
            .and_then(|p| p.parse().ok())
            .or(count.as_u64())
            .ok_or_else(|| "no play count".to_owned())
    }

    /// The album a track belongs to, for "Go to album" from the player bar.
    pub async fn album_of(&self, track_uri: &str) -> Result<Card> {
        let uri = SpotifyUri::from_uri(track_uri).map_err(|e| e.to_string())?;
        let track = TrackMetadata::get(self.session()?, &uri).await.map_err(|e| e.to_string())?;
        Ok(Card {
            kind: Kind::Album,
            uri: track.album.id.to_uri().map_err(|e| e.to_string())?,
            name: track.album.name.clone(),
            subtitle: String::new(),
            images: Images::default(),
            owner: String::new(),
        })
    }
}

fn artists(v: &Value) -> Vec<Named> {
    v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| Named {
            name: str_of(&a["profile"]["name"]),
            uri: str_of(&a["uri"]),
        })
        .filter(|a| !a.name.is_empty())
        .collect()
}

fn first_error(v: &Value) -> String {
    v["errors"][0]["message"]
        .as_str()
        .unwrap_or("Spotify returned no data")
        .to_owned()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// A playlist4 op setting a playlist's name, description... (`cleared`
/// are the attributes to remove).
fn list_attributes_op(values: pl4::ListAttributes, cleared: Vec<pl4::ListAttributeKind>) -> pl4::Op {
    let mut state = pl4::ListAttributesPartialState::new();
    state.values = MessageField::some(values);
    state.no_value = cleared.into_iter().map(Into::into).collect();
    let mut update = pl4::UpdateListAttributes::new();
    update.new_attributes = MessageField::some(state);
    let mut op = pl4::Op::new();
    op.set_kind(pl4::op::Kind::UPDATE_LIST_ATTRIBUTES);
    op.update_list_attributes = MessageField::some(update);
    op
}

/// One op as a change to a playlist (or rootlist), as Spotify's apps send.
fn list_changes(op: pl4::Op) -> pl4::ListChanges {
    let mut delta = pl4::Delta::new();
    delta.ops.push(op);
    let mut changes = pl4::ListChanges::new();
    changes.deltas.push(delta);
    changes
}

fn str_of(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

/// Playlist descriptions contain links; keep just the text.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&").replace("&#x27;", "'").replace("&quot;", "\"")
}

fn songs(n: usize) -> String {
    match n {
        0 => String::new(),
        1 => "1 song".into(),
        n => format!("{n} songs"),
    }
}

pub fn join_names(names: &[Named]) -> String {
    names.iter().map(|n| n.name.as_str()).collect::<Vec<_>>().join(", ")
}

pub fn id_of(uri: &str) -> &str {
    uri.rsplit(':').next().unwrap_or_default()
}

/// Script URLs of the web player's own bundles.
fn find_scripts(html: &str) -> Vec<String> {
    html.split('"')
        .filter(|s| s.starts_with(CHUNK_BASE) && s.ends_with(".js"))
        .map(str::to_owned)
        .collect()
}

/// Collects `"operation","query|mutation","<sha256>"` triples.
fn extract_hashes(js: &str, found: &mut HashMap<String, String>) {
    for marker in ["\",\"query\",\"", "\",\"mutation\",\""] {
        let mut rest = js;
        while let Some(i) = rest.find(marker) {
            let before = &rest[..i];
            let after = &rest[i + marker.len()..];
            let name = before.rsplit('"').next().unwrap_or_default();
            let hash = after.get(..64).unwrap_or_default();
            if !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric())
                && hash.len() == 64
                && hash.chars().all(|c| c.is_ascii_hexdigit())
            {
                found.insert(name.to_owned(), hash.to_owned());
            }
            rest = after;
        }
    }
}

/// File names of a lazily loaded chunk, from the bundle's chunk tables.
fn chunk_files(js: &str, chunk: &str) -> Vec<String> {
    let needle = format!(":\"{chunk}\"");
    let Some(i) = js.find(&needle) else { return Vec::new() };
    let id: String = js[..i]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if id.is_empty() {
        return Vec::new();
    }
    let key = format!("{id}:\"");
    let mut files = Vec::new();
    let mut rest = js;
    while let Some(i) = rest.find(&key) {
        let start = i + key.len();
        let candidate = rest.get(start..start + 9).unwrap_or_default();
        if candidate.len() == 9 && candidate.ends_with('"') && candidate[..8].chars().all(|c| c.is_ascii_hexdigit()) {
            let file = format!("{chunk}.{}.js", &candidate[..8]);
            if !files.contains(&file) {
                files.push(file);
            }
        }
        rest = &rest[start..];
    }
    files
}
