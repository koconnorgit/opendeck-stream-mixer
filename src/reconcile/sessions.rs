//! An immutable, per-refresh view of the media-session table.
//!
//! `media_session` owns the live table (fed by the MPRIS watcher); the
//! reconciler never reads it directly. One `SessionSnapshot` is cloned at the
//! top of each refresh and every identity query below runs against that copy,
//! so the decision layer is a pure function of its inputs and tests build the
//! rows they need instead of seeding a process-global table.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

/// One learned session, as the reconciler sees it (no art bytes — art is
/// resolved by the impure pass after identities are decided).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub pid: u32,
    pub key: String,
    pub title: String,
    pub artist: Option<String>,
    pub last_active: SystemTime,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionSnapshot {
    pub rows: Vec<SessionRow>,
}

/// How recently a session must have been the active one for
/// `recover_recent_identity` to pin a just-created stream on it. Sink-input
/// creation and the tab's Media Session activation happen within moments of
/// each other (both are "this tab started playing").
pub const RECOVERY_WINDOW: Duration = Duration::from_secs(15);

/// Twitch tab titles reach PulseAudio as `"<Channel> - Twitch"`; the channel
/// login (lowercased) joins them to a `ttv:` identity. Lowercasing first makes
/// the ASCII-suffix strip char-boundary-safe by construction.
pub fn twitch_login_from_media_name(media_name: &str) -> Option<String> {
    let login = media_name
        .trim()
        .to_lowercase()
        .strip_suffix(" - twitch")?
        .trim()
        .to_string();
    (!login.is_empty() && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then_some(login)
}

impl SessionSnapshot {
    pub fn new(rows: Vec<SessionRow>) -> Self {
        Self { rows }
    }

    fn on_pid(&self, pid: u32) -> impl Iterator<Item = &SessionRow> {
        self.rows.iter().filter(move |r| r.pid == pid)
    }

    pub fn get(&self, pid: u32, key: &str) -> Option<&SessionRow> {
        self.on_pid(pid).find(|r| r.key == key)
    }

    /// Match one sink-input to a learned identity: self-identifying
    /// `media.name` first (Twitch/Kick carry the channel in the name), then a
    /// boilerplate-cleaned title match against sessions learned on this PID
    /// (YouTube, generic sites). Both sides go through `clean_stream_title` —
    /// `media.name` carries `" - YouTube"`-style suffixes that `xesam:title`
    /// lacks (soak 2026-07-04 caught this).
    pub fn match_identity(&self, pid: u32, media_name: &str) -> Option<String> {
        if let Some(login) = twitch_login_from_media_name(media_name) {
            return Some(format!("ttv:{login}"));
        }
        if let Some(slug) = crate::kick::kick_slug(media_name) {
            return Some(format!("kick:{slug}"));
        }
        let wanted = crate::mpris::clean_stream_title(media_name).to_lowercase();
        if wanted.is_empty() || crate::mpris::is_generic_name(&wanted) {
            return None;
        }
        self.on_pid(pid)
            .find(|r| crate::mpris::clean_stream_title(&r.title).to_lowercase() == wanted)
            .map(|r| r.key.clone())
    }

    /// Canonical display name for an identity. Twitch is rebuilt from the
    /// session's `xesam:artist` (a recreated sink-input wears the active Media
    /// Session title, which for Twitch is the *stream subtitle* rather than
    /// the "Nmplol - Twitch" tab title); everything else uses the session's
    /// own title (tab title for Kick, video title for YouTube/Web).
    pub fn display_name_for(&self, pid: u32, key: &str) -> Option<String> {
        let record = self.get(pid, key);
        if let Some(login) = key.strip_prefix("ttv:") {
            let artist = record.and_then(|r| r.artist.clone());
            return Some(format!(
                "{} - Twitch",
                artist.unwrap_or_else(|| login.to_string())
            ));
        }
        record.map(|r| r.title.clone())
    }

    /// Re-identify a stream whose `media.name` can't be trusted (generic, or
    /// wearing another live stream's title). The freshest unworn session on
    /// the PID is adopted when either:
    ///  - it signaled within `RECOVERY_WINDOW` (the tab that just started
    ///    playing re-activated its session moments ago), or
    ///  - it is the PID's *current active session* (nothing on the PID is
    ///    newer). Unmuting a tab that was already the active session emits NO
    ///    metadata change — its record looks stale even though the browser is
    ///    actively reporting that content as playing (observed 2026-07-04:
    ///    Twitch mute/unmute never recovered while Kick always did).
    /// Never returns an identity another live stream already wears (`worn`).
    /// `departed` maps identities whose channels recently departed on this
    /// PID to when they did. A STALE candidate in that set is a likely ghost
    /// — a closed tab's session lingers looking "newest" but will never
    /// signal again — and is refused. And NO record older than the latest
    /// recent departure is ever adopted: a bump that predates that departure
    /// happened before the unmute that created the stream being identified,
    /// so it says nothing about that stream (observed 2026-08-24: two Kick
    /// tabs muted then unmuted in reverse — Firefox bumped the first-muted
    /// tab's session while the second was being muted, and the returning
    /// second stream wore the first's identity; both slots swapped). A stale
    /// non-active candidate is always refused rather than guessed.
    pub fn recover_recent_identity(
        &self,
        pid: u32,
        worn: &HashSet<String>,
        departed: &HashMap<String, SystemTime>,
        now: SystemTime,
    ) -> Option<String> {
        let newest_on_pid = self.on_pid(pid).map(|r| r.last_active).max()?;
        let latest_departure = departed.values().max().copied();
        self.on_pid(pid)
            .filter(|r| !worn.contains(&r.key))
            .max_by_key(|r| r.last_active)
            .filter(|r| {
                let fresh = now
                    .duration_since(r.last_active)
                    .map(|age| age < RECOVERY_WINDOW)
                    .unwrap_or(true);
                let after_departures = latest_departure.map_or(true, |d| r.last_active > d);
                after_departures
                    && (fresh
                        || (r.last_active >= newest_on_pid
                            && !departed.contains_key(r.key.as_str())))
            })
            .map(|r| r.key.clone())
    }

    /// The PID's newest session key, when it is strictly newer than `key`'s
    /// own record (or `key` has no record at all). `None` when `key` IS the
    /// PID's current active session, or when the table knows nothing about
    /// this PID.
    ///
    /// The reconciler uses this to notice that a departure-recency guess
    /// disagrees with what the browser currently reports as playing.
    pub fn newer_session_than(&self, pid: u32, key: &str) -> Option<String> {
        let own = self.get(pid, key).map(|r| r.last_active);
        self.on_pid(pid)
            .filter(|r| r.key != key)
            .max_by_key(|r| r.last_active)
            .filter(|r| own.map(|t| r.last_active > t).unwrap_or(true))
            .map(|r| r.key.clone())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A session row with no artist.
    pub fn row(pid: u32, key: &str, title: &str, last_active: SystemTime) -> SessionRow {
        SessionRow {
            pid,
            key: key.to_string(),
            title: title.to_string(),
            artist: None,
            last_active,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::row;
    use super::*;

    #[test]
    fn newer_session_than_names_only_a_strictly_newer_active_session() {
        let pid = 1;
        let now = SystemTime::now();
        let ago = |s: u64| now - Duration::from_secs(s);
        let snap = SessionSnapshot::new(vec![
            row(pid, "ttv:old", "Old", ago(600)),
            row(pid, "ttv:new", "New", ago(30)),
        ]);

        assert_eq!(snap.newer_session_than(pid, "ttv:old").as_deref(), Some("ttv:new"));
        // The newest session is not newer than itself.
        assert_eq!(snap.newer_session_than(pid, "ttv:new"), None);
        // A key with no record at all is superseded by whatever is newest.
        assert_eq!(
            snap.newer_session_than(pid, "kick:absent").as_deref(),
            Some("ttv:new")
        );
        // Another PID's sessions are invisible.
        assert_eq!(snap.newer_session_than(pid + 1, "ttv:old"), None);
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
    fn match_uses_learned_session_title() {
        let snap = SessionSnapshot::new(vec![row(
            1,
            "yt:abc123",
            "Example Video Title",
            SystemTime::now(),
        )]);
        assert_eq!(
            snap.match_identity(1, "Example Video Title").as_deref(),
            Some("yt:abc123")
        );
        // Another PID's sessions don't match.
        assert_eq!(snap.match_identity(2, "Example Video Title"), None);
    }

    #[test]
    fn recovers_most_recent_unworn_session() {
        let pid = 1;
        let now = SystemTime::now();
        let snap = SessionSnapshot::new(vec![
            // just became active (the unmuting tab)
            row(pid, "ttv:fresh", "Fresh", now - Duration::from_secs(2)),
            // an old Short, outside the window
            row(pid, "yt:stale", "Stale", now - Duration::from_secs(600)),
        ]);

        // Freshest unworn session wins.
        assert_eq!(
            snap.recover_recent_identity(pid, &HashSet::new(), &HashMap::new(), now)
                .as_deref(),
            Some("ttv:fresh")
        );
        // A worn identity is never re-adopted; the stale one is neither fresh
        // nor the pid's newest session, so it's refused.
        let worn: HashSet<String> = [String::from("ttv:fresh")].into();
        assert_eq!(
            snap.recover_recent_identity(pid, &worn, &HashMap::new(), now),
            None
        );
        // A fresh signal that PREDATES the latest departure on the PID is not
        // about the stream being identified (it fired before the unmute).
        let departed: HashMap<String, SystemTime> =
            [(String::from("kick:other"), now - Duration::from_secs(1))].into();
        assert_eq!(
            snap.recover_recent_identity(pid, &HashSet::new(), &departed, now),
            None,
            "a session bumped before the last departure never identifies a later stream"
        );
        // ...while one bumped after it does.
        let departed: HashMap<String, SystemTime> =
            [(String::from("kick:other"), now - Duration::from_secs(3))].into();
        assert_eq!(
            snap.recover_recent_identity(pid, &HashSet::new(), &departed, now)
                .as_deref(),
            Some("ttv:fresh")
        );
    }

    #[test]
    fn recovers_stale_session_when_it_is_the_current_active_one() {
        // Unmuting a tab that was ALREADY the active session emits no metadata
        // change — the record is stale, but it's still the pid's newest, i.e.
        // what the browser reports as playing. Must be adopted.
        let pid = 1;
        let now = SystemTime::now();
        let snap = SessionSnapshot::new(vec![SessionRow {
            pid,
            key: "ttv:nmplol".into(),
            title: "Some Subtitle".into(),
            artist: Some("Nmplol".into()),
            last_active: now - Duration::from_secs(1200),
        }]);
        assert_eq!(
            snap.recover_recent_identity(pid, &HashSet::new(), &HashMap::new(), now)
                .as_deref(),
            Some("ttv:nmplol")
        );
        // But once a live stream wears it, it is never handed out again.
        let worn: HashSet<String> = [String::from("ttv:nmplol")].into();
        assert_eq!(
            snap.recover_recent_identity(pid, &worn, &HashMap::new(), now),
            None
        );
        // A stale newest session that recently DEPARTED is a ghost — refused.
        let departed: HashMap<String, SystemTime> =
            [(String::from("ttv:nmplol"), now - Duration::from_secs(60))].into();
        assert_eq!(
            snap.recover_recent_identity(pid, &HashSet::new(), &departed, now),
            None
        );
    }

    #[test]
    fn matches_suffixed_media_name_to_unsuffixed_session_title() {
        // Soak 2026-07-04: media.name carries " - YouTube" boilerplate that
        // xesam:title lacks; both sides must be cleaned before comparing.
        let snap = SessionSnapshot::new(vec![row(1, "yt:xyz789", "Cool Video", SystemTime::now())]);
        assert_eq!(
            snap.match_identity(1, "Cool Video - YouTube").as_deref(),
            Some("yt:xyz789")
        );
    }

    #[test]
    fn canonical_twitch_display_name_from_artist() {
        let snap = SessionSnapshot::new(vec![SessionRow {
            pid: 1,
            key: "ttv:nmplol".into(),
            title: "Some Stream Subtitle".into(),
            artist: Some("Nmplol".into()),
            last_active: SystemTime::now(),
        }]);
        assert_eq!(
            snap.display_name_for(1, "ttv:nmplol").as_deref(),
            Some("Nmplol - Twitch")
        );
        // No session record → login fallback; non-Twitch identities → None.
        assert_eq!(
            snap.display_name_for(2, "ttv:somelogin").as_deref(),
            Some("somelogin - Twitch")
        );
        assert_eq!(snap.display_name_for(1, "yt:abc123"), None);
        assert_eq!(snap.display_name_for(1, "kick:xqc"), None);
    }

    #[test]
    fn twitch_media_name_matches_identity_without_a_session() {
        let snap = SessionSnapshot::default();
        assert_eq!(
            snap.match_identity(7, "Nmplol - Twitch").as_deref(),
            Some("ttv:nmplol")
        );
    }
}
