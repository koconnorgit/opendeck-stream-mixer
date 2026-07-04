//! Media-session watcher — Phase 1 (shadow mode) of
//! `docs/mpris-subscription-design.md`.
//!
//! Subscribes to every MPRIS player's `PropertiesChanged` signals and harvests
//! atomic (url, title, artist, art) records into a per-PID session table. The
//! tab URL in `xesam:url` is ground-truth identity (verified 2026-07-04:
//! YouTube/Twitch/Kick all expose it), and `mpris:artUrl` — which appears only
//! as a sub-second transient in the signal stream, never reliably in
//! steady-state reads — names the one art file Firefox keeps on disk, whose
//! bytes must therefore be copied the moment the signal arrives.
//!
//! In this phase nothing consumes the table for rendering: `shadow_log` prints
//! what identity/art *would* be chosen next to what the current pipeline chose,
//! so the design can soak against real usage before any switchover.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use futures_lite::StreamExt;
use zbus::zvariant::OwnedValue;

/// How often the player registry re-lists MPRIS bus names to pick up new
/// players and reap vanished ones. Players come and go with browser processes,
/// so this is a slow-moving set.
const REGISTRY_POLL: Duration = Duration::from_secs(10);
/// How long a signal burst is coalesced before processing. Observed bursts are
/// 3 signals in ~2ms with partially-filled dicts; artUrl often only exists in
/// the middle signal.
const BURST_WINDOW: Duration = Duration::from_millis(250);
/// How long sessions of a vanished player (browser closed) are kept for
/// identity/art recovery before pruning.
const DORMANT_TTL: Duration = Duration::from_secs(300);

/// Ground-truth identity of a piece of playing media, derived from the tab URL.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaIdentity {
    YouTube { video_id: String },
    Twitch { login: String },
    Kick { slug: String },
    /// Anything else with a real title (Reddit, SoundCloud, …).
    Web { host: String, title: String },
}

impl MediaIdentity {
    /// Canonical string form; the session-table and (future) art-cache key.
    pub fn key(&self) -> String {
        match self {
            MediaIdentity::YouTube { video_id } => format!("yt:{video_id}"),
            MediaIdentity::Twitch { login } => format!("ttv:{login}"),
            MediaIdentity::Kick { slug } => format!("kick:{slug}"),
            MediaIdentity::Web { host, title } => {
                format!("web:{host}:{}", title.to_lowercase())
            }
        }
    }
}

/// One learned media session (the browser's active session at some moment).
struct SessionRecord {
    /// Unread in phase 1 (the table is keyed by `identity.key()`); phase 2's
    /// art resolver dispatches on the variant (yt-thumbnail vs ttv-preview).
    #[allow(dead_code)]
    identity: MediaIdentity,
    title: String,
    artist: Option<String>,
    /// Art bytes captured at signal time (the on-disk file is deleted on the
    /// next session change, so these bytes are the only durable copy).
    art: Option<Arc<Vec<u8>>>,
    last_active: SystemTime,
    /// Set when the owning player vanishes from the bus; prunes after
    /// `DORMANT_TTL`.
    dormant_since: Option<SystemTime>,
}

/// (browser PID, identity key) → learned session.
static SESSIONS: LazyLock<Mutex<HashMap<(u32, String), SessionRecord>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Last shadow decision logged per sink-input uid, so `shadow_log` only prints
/// deltas and the soak log stays readable.
static SHADOW_LAST: LazyLock<Mutex<HashMap<u32, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// ---------------------------------------------------------------------------
// URL → identity
// ---------------------------------------------------------------------------

/// Derive a media identity from a tab URL (+ page title for the generic case).
/// Returns `None` for transitional/homepage URLs (`kick.com/`, `twitch.tv/`)
/// observed during navigation — they carry no identity.
pub fn identity_from_url(url: &str, title: &str) -> Option<MediaIdentity> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (host, path_query) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("m."))
        .unwrap_or(host)
        .to_lowercase();
    // Path without fragment; query kept for /watch.
    let path_query = path_query.split('#').next().unwrap_or("");
    let (path, query) = path_query.split_once('?').unwrap_or((path_query, ""));
    let path = path.trim_end_matches('/');

    let web = || {
        let title = title.trim();
        (!title.is_empty()).then(|| MediaIdentity::Web {
            host: host.clone(),
            title: title.to_string(),
        })
    };

    match host.as_str() {
        "youtube.com" => {
            if path == "watch" {
                let video_id = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("v="))
                    .filter(|v| !v.is_empty())?;
                Some(MediaIdentity::YouTube {
                    video_id: video_id.to_string(),
                })
            } else if let Some(id) = path.strip_prefix("shorts/") {
                let id = id.split('/').next().unwrap_or("");
                (!id.is_empty()).then(|| MediaIdentity::YouTube {
                    video_id: id.to_string(),
                })
            } else if path.is_empty() {
                None
            } else {
                web() // channel pages, playlists, … — session art still applies
            }
        }
        "twitch.tv" => match single_channel_segment(path) {
            Some(login) if !TWITCH_NON_CHANNEL.contains(&login.as_str()) => {
                Some(MediaIdentity::Twitch { login })
            }
            Some(_) => web(),
            None if path.is_empty() => None,
            None => web(),
        },
        "kick.com" => match single_channel_segment(path) {
            Some(slug) => Some(MediaIdentity::Kick { slug }),
            None if path.is_empty() => None,
            None => web(),
        },
        _ => web(),
    }
}

/// Well-known twitch.tv paths that are site sections, not channel logins.
const TWITCH_NON_CHANNEL: &[&str] = &[
    "directory", "videos", "search", "settings", "subscriptions", "wallet",
    "drops", "downloads", "jobs", "turbo", "p",
];

/// A path that is exactly one `[A-Za-z0-9_]+` segment → lowercased channel name.
fn single_channel_segment(path: &str) -> Option<String> {
    if path.is_empty() || path.contains('/') {
        return None;
    }
    path.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
        .then(|| path.to_lowercase())
}

// ---------------------------------------------------------------------------
// Shadow matching (what the new pipeline WOULD decide for a sink-input)
// ---------------------------------------------------------------------------

/// Twitch tab titles reach PulseAudio as `"<Channel> - Twitch"`; the channel
/// login (lowercased) joins them to a `ttv:` identity. Lowercasing first makes
/// the ASCII-suffix strip char-boundary-safe by construction.
fn twitch_login_from_media_name(media_name: &str) -> Option<String> {
    let login = media_name.trim().to_lowercase().strip_suffix(" - twitch")?.trim().to_string();
    (!login.is_empty() && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then_some(login)
}

/// The art the shadow pipeline would render for an identity.
enum ShadowArt {
    /// Browser-published art captured from the session table (bytes length).
    Session(usize),
    /// Would fetch from a deterministic source (label).
    Fetch(&'static str),
    /// No art source; themed-icon path.
    None,
}

impl std::fmt::Display for ShadowArt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShadowArt::Session(len) => write!(f, "session({}KB)", len / 1024),
            ShadowArt::Fetch(src) => write!(f, "fetch:{src}"),
            ShadowArt::None => write!(f, "none"),
        }
    }
}

fn session_art_len(pid: u32, key: &str) -> Option<usize> {
    let sessions = SESSIONS.lock().unwrap();
    sessions
        .get(&(pid, key.to_string()))
        .and_then(|r| r.art.as_ref().map(|a| a.len()))
}

/// Match one sink-input to an identity + art source, per the design's matching
/// order: self-identifying `media.name` (Twitch/Kick/exact-title), no
/// collision-recovery here (phase 1 observes; it doesn't re-identify).
fn shadow_match(pid: u32, media_name: &str) -> Option<(String, ShadowArt)> {
    // 1. Twitch: media.name carries the channel.
    if let Some(login) = twitch_login_from_media_name(media_name) {
        let key = format!("ttv:{login}");
        let art = match session_art_len(pid, &key) {
            Some(len) => ShadowArt::Session(len),
            None => ShadowArt::Fetch("ttv-preview-cdn"),
        };
        return Some((key, art));
    }
    // 2. Kick: media.name carries the slug.
    if let Some(slug) = crate::kick::kick_slug(media_name) {
        let key = format!("kick:{slug}");
        let art = match session_art_len(pid, &key) {
            Some(len) => ShadowArt::Session(len),
            None => ShadowArt::Fetch("kick-avatar-api"),
        };
        return Some((key, art));
    }
    // 3. Exact title match against learned sessions on this PID (YouTube, Web).
    let wanted = media_name.trim().to_lowercase();
    let sessions = SESSIONS.lock().unwrap();
    for ((p, key), rec) in sessions.iter() {
        if *p != pid || rec.title.trim().to_lowercase() != wanted {
            continue;
        }
        let art = match rec.art.as_ref() {
            Some(a) => ShadowArt::Session(a.len()),
            None if key.starts_with("yt:") => ShadowArt::Fetch("yt-thumbnail"),
            None => ShadowArt::None,
        };
        return Some((key.clone(), art));
    }
    None
}

/// Phase-1 hook, called at the end of `mixer::update_mixer_channels`: log what
/// the shadow pipeline would decide per channel, next to what the current
/// pipeline chose. Prints only when a channel's decision changes. Observation
/// only — no behavior depends on this.
pub fn shadow_log(channels: &HashMap<u8, crate::mixer::MixerChannel>) {
    let mut last = SHADOW_LAST.lock().unwrap();
    let mut seen: HashSet<u32> = HashSet::new();
    for ch in channels.values() {
        if ch.is_device {
            continue;
        }
        let Some(pid) = ch.pid else { continue };
        seen.insert(ch.uid);

        let would = match shadow_match(pid, &ch.app_name) {
            Some((key, art)) => format!("{key} art={art}"),
            None => "no-identity".to_string(),
        };
        let is = if ch.mpris_art_data.is_some() {
            "art"
        } else if !ch.uses_default_icon {
            "themed"
        } else {
            "default"
        };
        let line = format!("would: {would} | is: {is}");
        if last.get(&ch.uid) != Some(&line) {
            println!("SHADOW uid={} {:?} → {}", ch.uid, ch.app_name, line);
            last.insert(ch.uid, line);
        }
    }
    last.retain(|uid, _| seen.contains(uid));
}

// ---------------------------------------------------------------------------
// The watcher
// ---------------------------------------------------------------------------

/// Start the media-session watcher under a supervisor: a panic or bus error is
/// logged and the watcher restarts with backoff, per the post-684ebf4 rule
/// that no background failure may silently kill a subsystem.
pub fn start() {
    tokio::spawn(async {
        let mut backoff = 5u64;
        loop {
            match tokio::spawn(watcher_main()).await {
                Ok(Ok(())) => backoff = 5, // clean end (bus gone?) — retry soon
                Ok(Err(e)) => eprintln!("MPRIS-WATCH error: {e}; restart in {backoff}s"),
                Err(e) => eprintln!("MPRIS-WATCH panicked; restart in {backoff}s: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(60);
        }
    });
}

/// Registry loop: keep one `watch_player` task per live MPRIS bus name.
async fn watcher_main() -> zbus::Result<()> {
    let conn = zbus::Connection::session().await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let mut players: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    println!("MPRIS-WATCH started");

    loop {
        let names: HashSet<String> = dbus
            .list_names()
            .await?
            .into_iter()
            .map(|n| n.to_string())
            .filter(|n| n.starts_with("org.mpris.MediaPlayer2."))
            .collect();

        for name in &names {
            if players.contains_key(name) {
                continue;
            }
            let Ok(bus_name) = zbus::names::BusName::try_from(name.as_str()) else {
                continue;
            };
            let Ok(pid) = dbus.get_connection_unix_process_id(bus_name).await else {
                continue;
            };
            println!("MPRIS-WATCH player up: {name} pid={pid}");
            let task_conn = conn.clone();
            let task_name = name.clone();
            players.insert(
                name.clone(),
                tokio::spawn(async move {
                    watch_player(task_conn, task_name, pid).await;
                }),
            );
        }

        players.retain(|name, task| {
            let alive = names.contains(name) && !task.is_finished();
            if !alive {
                println!("MPRIS-WATCH player gone: {name}");
                task.abort();
            }
            alive
        });

        mark_dormant_and_prune(&players, &conn).await;

        tokio::time::sleep(REGISTRY_POLL).await;
    }
}

/// Mark sessions whose PID has no live player as dormant; prune dormant ones
/// past `DORMANT_TTL`.
async fn mark_dormant_and_prune(
    players: &HashMap<String, tokio::task::JoinHandle<()>>,
    conn: &zbus::Connection,
) {
    // Re-resolve live player PIDs (cheap; the set is tiny).
    let mut live_pids: HashSet<u32> = HashSet::new();
    if let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await {
        for name in players.keys() {
            if let Ok(bus_name) = zbus::names::BusName::try_from(name.as_str()) {
                if let Ok(pid) = dbus.get_connection_unix_process_id(bus_name).await {
                    live_pids.insert(pid);
                }
            }
        }
    }
    let now = SystemTime::now();
    let mut sessions = SESSIONS.lock().unwrap();
    for ((pid, _), rec) in sessions.iter_mut() {
        if live_pids.contains(pid) {
            rec.dormant_since = None;
        } else if rec.dormant_since.is_none() {
            rec.dormant_since = Some(now);
        }
    }
    sessions.retain(|_, rec| match rec.dormant_since {
        Some(t) => now.duration_since(t).map(|d| d < DORMANT_TTL).unwrap_or(true),
        None => true,
    });
}

/// Fields extracted from one (possibly partial) Metadata/PlaybackStatus update.
#[derive(Default)]
struct MetaFields {
    url: Option<String>,
    title: Option<String>,
    artist: Option<String>,
    art_url: Option<String>,
}

impl MetaFields {
    /// Merge a later burst signal into this one. Later values win per field,
    /// but a field absent later never erases one seen earlier — observed
    /// bursts publish artUrl mid-burst and drop it from the final signal.
    fn merge(&mut self, later: MetaFields) {
        self.url = later.url.or(self.url.take());
        self.title = later.title.or(self.title.take());
        self.artist = later.artist.or(self.artist.take());
        self.art_url = later.art_url.or(self.art_url.take());
    }

    fn from_metadata_dict(dict: &HashMap<String, OwnedValue>) -> Self {
        let get_str = |k: &str| -> Option<String> {
            dict.get(k)
                .and_then(|v| String::try_from(v.clone()).ok())
                .filter(|s| !s.is_empty())
        };
        let artist = dict
            .get("xesam:artist")
            .and_then(|v| Vec::<String>::try_from(v.clone()).ok())
            .and_then(|v| v.into_iter().find(|s| !s.is_empty()));
        MetaFields {
            url: get_str("xesam:url"),
            title: get_str("xesam:title"),
            artist,
            art_url: get_str("mpris:artUrl"),
        }
    }
}

/// Subscribe to one player's PropertiesChanged stream; runs until the stream
/// ends or the registry aborts the task.
async fn watch_player(conn: zbus::Connection, bus_name: String, pid: u32) {
    let Ok(props) = zbus::fdo::PropertiesProxy::builder(&conn)
        .destination(bus_name.as_str())
        .and_then(|b| b.path("/org/mpris/MediaPlayer2"))
        .map(|b| b.build())
    else {
        return;
    };
    let Ok(props) = props.await else { return };

    // Cold start: read the current metadata once — no ambiguity, no timing.
    let iface = zbus::names::InterfaceName::try_from("org.mpris.MediaPlayer2.Player").ok();
    if let Some(iface) = &iface {
        if let Ok(meta) = props.get(iface.clone(), "Metadata").await {
            if let Ok(dict) = HashMap::<String, OwnedValue>::try_from(meta) {
                record_update(pid, MetaFields::from_metadata_dict(&dict));
            }
        }
    }

    let Ok(mut stream) = props.receive_properties_changed().await else {
        return;
    };
    while let Some(signal) = stream.next().await {
        let Some(mut fields) = fields_from_signal(&signal) else {
            continue;
        };
        // Coalesce the burst: keep merging until the window closes.
        loop {
            match tokio::time::timeout(BURST_WINDOW, stream.next()).await {
                Ok(Some(sig2)) => {
                    if let Some(more) = fields_from_signal(&sig2) {
                        fields.merge(more);
                    }
                }
                Ok(None) => return, // stream ended
                Err(_) => break,    // window closed
            }
        }
        record_update(pid, fields);
    }
}

/// Extract MetaFields from one PropertiesChanged signal (Player interface
/// Metadata only; other property updates carry no identity).
fn fields_from_signal(signal: &zbus::fdo::PropertiesChanged) -> Option<MetaFields> {
    let args = signal.args().ok()?;
    if args.interface_name.as_str() != "org.mpris.MediaPlayer2.Player" {
        return None;
    }
    let meta = args.changed_properties.get("Metadata")?;
    let owned = meta.try_to_owned().ok()?;
    let dict = HashMap::<String, OwnedValue>::try_from(owned).ok()?;
    Some(MetaFields::from_metadata_dict(&dict))
}

/// Fold one coalesced update into the session table, capturing art bytes
/// immediately (the on-disk file is deleted on the next session change).
fn record_update(pid: u32, fields: MetaFields) {
    let (Some(url), Some(title)) = (&fields.url, &fields.title) else {
        return;
    };
    let Some(identity) = identity_from_url(url, title) else {
        return;
    };
    let key = identity.key();

    let art: Option<Arc<Vec<u8>>> = fields
        .art_url
        .as_deref()
        .and_then(|u| u.strip_prefix("file://"))
        .and_then(|path| std::fs::read(path).ok())
        .map(|bytes| Arc::new(bytes));

    let now = SystemTime::now();
    let mut sessions = SESSIONS.lock().unwrap();
    let entry = sessions.entry((pid, key.clone()));
    match entry {
        std::collections::hash_map::Entry::Occupied(mut e) => {
            let rec = e.get_mut();
            let art_grew = art.is_some();
            if let Some(a) = art {
                rec.art = Some(a); // newest wins (e.g. a looping Short re-publishes)
            }
            rec.title = title.clone();
            rec.artist = fields.artist.or(rec.artist.take());
            rec.last_active = now;
            rec.dormant_since = None;
            if art_grew {
                println!(
                    "MPRIS-WATCH art captured: pid={pid} {key} ({}KB)",
                    rec.art.as_ref().map(|a| a.len() / 1024).unwrap_or(0)
                );
            }
        }
        std::collections::hash_map::Entry::Vacant(e) => {
            println!(
                "MPRIS-WATCH session learned: pid={pid} {key} title={:?} artist={:?} art={}",
                title,
                fields.artist,
                art.as_ref().map(|a| format!("{}KB", a.len() / 1024)).unwrap_or("none".into()),
            );
            e.insert(SessionRecord {
                identity,
                title: title.clone(),
                artist: fields.artist,
                art,
                last_active: now,
                dormant_since: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(url: &str, title: &str) -> Option<String> {
        identity_from_url(url, title).map(|i| i.key())
    }

    #[test]
    fn youtube_watch_and_shorts() {
        assert_eq!(
            ident("https://www.youtube.com/watch?v=wQt7GHDSBEE&t=1324s", "x").as_deref(),
            Some("yt:wQt7GHDSBEE")
        );
        assert_eq!(
            ident("https://www.youtube.com/shorts/eAz9dcpAz-Q", "x").as_deref(),
            Some("yt:eAz9dcpAz-Q")
        );
    }

    #[test]
    fn youtube_channel_page_is_web() {
        assert_eq!(
            ident("https://www.youtube.com/@punchingdownpodcast", "Punching Down").as_deref(),
            Some("web:youtube.com:punching down")
        );
    }

    #[test]
    fn twitch_channel_and_homepage() {
        assert_eq!(
            ident("https://www.twitch.tv/nmplol", "t").as_deref(),
            Some("ttv:nmplol")
        );
        assert_eq!(ident("https://www.twitch.tv/", "t"), None);
        // Site sections are not channels.
        assert_eq!(
            ident("https://www.twitch.tv/directory", "Browse").as_deref(),
            Some("web:twitch.tv:browse")
        );
    }

    #[test]
    fn kick_channel_and_homepage() {
        assert_eq!(
            ident("https://kick.com/piratesoftware", "t").as_deref(),
            Some("kick:piratesoftware")
        );
        assert_eq!(ident("https://kick.com/", "t"), None);
    }

    #[test]
    fn generic_site_uses_host_and_title() {
        assert_eq!(
            ident(
                "https://www.reddit.com/r/LivestreamFail/#res:ner-page=3",
                "r/LivestreamFail"
            )
            .as_deref(),
            Some("web:reddit.com:r/livestreamfail")
        );
    }

    #[test]
    fn twitch_media_name_yields_login() {
        assert_eq!(
            twitch_login_from_media_name("Nmplol - Twitch").as_deref(),
            Some("nmplol")
        );
        assert_eq!(twitch_login_from_media_name("Some Song - YouTube"), None);
        // Emoji title must not panic (post-684ebf4 rule) and is not a login.
        assert_eq!(twitch_login_from_media_name("🎵🎵🎵🎵"), None);
    }

    #[test]
    fn shadow_match_uses_learned_session_title() {
        let pid = 910_001; // private test PID, same convention as mixer tests
        SESSIONS.lock().unwrap().insert(
            (pid, "yt:abc123".into()),
            SessionRecord {
                identity: MediaIdentity::YouTube {
                    video_id: "abc123".into(),
                },
                title: "Example Video Title".into(),
                artist: None,
                art: Some(Arc::new(vec![0u8; 2048])),
                last_active: SystemTime::now(),
                dormant_since: None,
            },
        );
        let m = shadow_match(pid, "Example Video Title");
        assert_eq!(m.as_ref().map(|(k, _)| k.as_str()), Some("yt:abc123"));
        assert!(matches!(m.unwrap().1, ShadowArt::Session(2048)));
        SESSIONS.lock().unwrap().retain(|(p, _), _| *p != pid);
    }

    #[test]
    fn shadow_match_twitch_without_session_falls_back_to_cdn() {
        let m = shadow_match(910_002, "Nmplol - Twitch");
        assert_eq!(m.as_ref().map(|(k, _)| k.as_str()), Some("ttv:nmplol"));
        assert!(matches!(m.unwrap().1, ShadowArt::Fetch("ttv-preview-cdn")));
    }
}
