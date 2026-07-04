use crate::audio::audio_system::AppInfo;
use crate::media_session::{self, ChannelArt};
use crate::utils::get_app_icon_uri;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::SystemTime;
use tokio::sync::Mutex;

#[derive(Clone, Debug)]
pub struct MixerChannel {
    pub header_id: Option<String>,
    pub upper_vol_btn_id: Option<String>,
    pub lower_vol_btn_id: Option<String>,
    pub dial_id: Option<String>,
    pub uid: u32,
    /// All sink-input indices this channel controls (collapsed multi-stream apps
    /// list every member here; a normal channel just holds `[uid]`). Volume and
    /// mute are applied to every member.
    pub member_uids: Vec<u32>,
    pub pid: Option<u32>,
    pub app_name: String,
    /// Stable app identity used for exclude-list matching (see `AppInfo::app_id`).
    pub app_id: String,
    pub sink_name: Option<String>,
    pub mute: bool,
    pub vol_percent: f32,
    pub icon_uri: String,
    pub icon_uri_mute: String,
    pub uses_default_icon: bool,
    /// Resolved media identity key (e.g. "ttv:nmplol"), when this stream's
    /// content is known. The single naming/art authority since phase 3 of
    /// docs/mpris-subscription-design.md.
    pub identity: Option<String>,
    /// The identity-art bytes currently rendered into the icon. `Arc` from the
    /// session table / art caches, so pointer equality detects "unchanged" and
    /// skips PNG re-encoding on every refresh.
    pub art_data: Option<Arc<Vec<u8>>>,
    /// A cross-identity name shift waiting out its confirmation window (the
    /// proposed new name + when it was first seen). Firefox re-tags EXISTING
    /// sink-inputs with the active session's title when another tab starts
    /// playing, and only time separates that lie from a genuine same-tab
    /// navigation: the lie's real owner shows up seconds later and collides.
    pub pending_name_shift: Option<(String, SystemTime)>,
    pub is_device: bool,
    pub is_multi_sink_app: bool,
}

impl MixerChannel {
    /// Text shown on the LCD / header for this channel. Collapsed multi-sink apps
    /// show the sink name; everything else shows the stream name, with Kick tab
    /// titles trimmed to just the streamer handle. The full
    /// `"<handle> Stream - Watch Live on Kick"` is kept in `app_name` (identity
    /// matching keys off it) but is too long/noisy to display.
    pub fn display_label(&self) -> String {
        if self.is_multi_sink_app {
            // Per-stream sink names disambiguate same-named app instances, but
            // browser sink-inputs are all literally named "AudioStream" — a
            // generic sink name is never a better label than the app name.
            if let Some(sink) = self
                .sink_name
                .as_deref()
                .filter(|s| !crate::mpris::is_generic_name(s))
            {
                return sink.to_string();
            }
        }
        crate::kick::display_name(&self.app_name)
    }

    fn art_bytes(&self) -> Option<&[u8]> {
        self.art_data.as_ref().map(|a| a.as_slice())
    }
}

pub static MIXER_CHANNELS: LazyLock<Mutex<HashMap<u8, MixerChannel>>> =
    LazyLock::new(|| Mutex::const_new(HashMap::new()));

/// Maps encoder dial position → mixer channel index (independent from button columns)
pub static ENCODER_TO_CHANNEL_MAP: LazyLock<Mutex<HashMap<u8, u8>>> =
    LazyLock::new(|| Mutex::const_new(HashMap::new()));

/// How long a cross-identity name shift must persist, uncontested, before it
/// is believed. Long enough for the title's real owner (the other tab's own
/// sink-input) to appear and collide; short enough that a genuine same-tab
/// navigation only lags its relabel by a couple of refreshes.
const SHIFT_CONFIRM: std::time::Duration = std::time::Duration::from_secs(5);

/// Decide a deferred name shift. Returns true when the shift is confirmed
/// (uncontested for `SHIFT_CONFIRM`). `contested` means another live stream
/// currently maps to the proposed name's identity — the shift is the re-tag
/// lie, so the pending state is dropped entirely.
fn confirm_deferred_shift(
    pending: &mut Option<(String, SystemTime)>,
    new_name: &str,
    now: SystemTime,
    contested: bool,
) -> bool {
    if contested {
        *pending = None;
        return false;
    }
    match pending {
        Some((name, since)) if name == new_name => {
            let confirmed = now
                .duration_since(*since)
                .map(|d| d >= SHIFT_CONFIRM)
                .unwrap_or(false);
            if confirmed {
                *pending = None;
            }
            confirmed
        }
        _ => {
            *pending = Some((new_name.to_string(), now));
            false
        }
    }
}

/// Whether an app is on the user's exclude list. Matches the stable app id
/// (so an app stays excluded across track/tab/title changes) and, for backward
/// compatibility, the display name (older entries were stored that way).
fn is_ignored(app: &AppInfo, ignored_apps: &[String]) -> bool {
    ignored_apps.iter().any(|n| n == &app.app_id || n == &app.app_name)
}

/// Cold start: clear any stale state and reconcile from scratch. Everything
/// else is the ordinary refresh path — with identity resolution replacing the
/// old timing anchors, cold start is no longer a special case.
pub async fn create_mixer_channels(applications: Vec<AppInfo>, ignored_apps: &[String]) {
    MIXER_CHANNELS.lock().await.clear();
    update_mixer_channels(applications, ignored_apps).await;
}

pub async fn update_mixer_channels(applications: Vec<AppInfo>, ignored_apps: &[String]) {
    let mut channels = MIXER_CHANNELS.lock().await;

    let active_apps: Vec<_> = applications
        .into_iter()
        .filter(|app| !is_ignored(app, ignored_apps))
        .collect();

    // Index previous channels by UID so identity, name, and art carry over.
    let mut previous_by_uid: HashMap<u32, MixerChannel> =
        channels.drain().map(|(_, ch)| (ch.uid, ch)).collect();

    let now = SystemTime::now();

    // Count cleaned media.names per PID across all live streams. A count > 1
    // means two streams in one browser process share a (cleaned) title — the
    // tell-tale that Firefox tagged a sink-input with the active session's
    // title instead of its own tab's. Such a name must not be trusted.
    let mut cleaned_name_counts: HashMap<(u32, String), usize> = HashMap::new();
    for a in &active_apps {
        if let Some(pid) = a.pid {
            let key = (pid, crate::mpris::clean_stream_title(&a.app_name).to_lowercase());
            *cleaned_name_counts.entry(key).or_insert(0) += 1;
        }
    }
    let title_collides = |pid: Option<u32>, name: &str| -> bool {
        pid.map_or(false, |pid| {
            let key = (pid, crate::mpris::clean_stream_title(name).to_lowercase());
            cleaned_name_counts.get(&key).copied().unwrap_or(0) > 1
        })
    };

    // How many live streams' RAW names map to each identity this refresh.
    // Catches two different title forms of the same content colliding.
    let mut identity_counts: HashMap<String, usize> = HashMap::new();
    for a in &active_apps {
        if let Some(key) = a
            .pid
            .and_then(|pid| media_session::match_identity(pid, &a.app_name))
        {
            *identity_counts.entry(key).or_insert(0) += 1;
        }
    }

    // Identities held by surviving channels, and the identity → owner-uid claim
    // map that keeps two channels from wearing the same identity this refresh.
    let active_uids: HashSet<u32> = active_apps.iter().map(|a| a.uid).collect();
    let prev_identities: HashSet<String> = previous_by_uid
        .values()
        .filter(|ch| active_uids.contains(&ch.uid))
        .filter_map(|ch| ch.identity.clone())
        .collect();
    let mut claimed: HashMap<String, u32> = previous_by_uid
        .values()
        .filter(|ch| active_uids.contains(&ch.uid))
        .filter_map(|ch| ch.identity.clone().map(|k| (k, ch.uid)))
        .collect();

    // Identities a re-identification must never adopt: everything a live
    // stream already wears or names.
    let recovery_worn: HashSet<String> = prev_identities
        .iter()
        .cloned()
        .chain(identity_counts.keys().cloned())
        .collect();

    let mut new_channels: HashMap<u8, MixerChannel> = HashMap::new();
    let mut col_key: u8 = 0;

    for app in active_apps {
        let raw_identity = app
            .pid
            .and_then(|pid| media_session::match_identity(pid, &app.app_name));

        let mut channel = match previous_by_uid.remove(&app.uid) {
            Some(prev) => reconcile_existing(
                prev,
                &app,
                raw_identity,
                &title_collides,
                &identity_counts,
                &recovery_worn,
                now,
            ),
            None => build_new(
                &app,
                raw_identity,
                &recovery_worn,
                &prev_identities,
                &title_collides,
                &identity_counts,
                now,
            ),
        };

        // Live data always follows the stream.
        channel.mute = app.mute;
        channel.vol_percent = app.vol_percent;
        channel.sink_name = app.sink_name.clone();
        channel.is_device = app.is_device;
        channel.is_multi_sink_app = app.is_multi_sink_app;
        channel.member_uids = app.member_uids.clone();
        channel.app_id = app.app_id.clone();
        // Position-specific IDs are reassigned by update_stream_deck_buttons.
        channel.header_id = None;
        channel.upper_vol_btn_id = None;
        channel.lower_vol_btn_id = None;
        channel.dial_id = None;

        resolve_channel_art(&mut channel, &app, &mut claimed);

        new_channels.insert(col_key, channel);
        col_key += 1;
    }

    *channels = new_channels;

    println!(
        "Updated mixer channels (filtered {} ignored apps)",
        ignored_apps.len()
    );
}

/// Reconcile a stream that already had a channel: identity is sticky, and the
/// raw media.name only changes it through guarded paths.
fn reconcile_existing(
    mut prev: MixerChannel,
    app: &AppInfo,
    raw_identity: Option<String>,
    title_collides: &dyn Fn(Option<u32>, &str) -> bool,
    identity_counts: &HashMap<String, usize>,
    recovery_worn: &HashSet<String>,
    now: SystemTime,
) -> MixerChannel {
    let raw = &app.app_name;

    if prev.identity.is_some() {
        if raw_identity == prev.identity {
            // Same content, possibly a different title form (canonical name vs
            // the stream subtitle Firefox keeps reporting). Nothing to do; the
            // canonical name is refreshed in resolve_channel_art.
            prev.pending_name_shift = None;
        } else if let Some(new_key) = raw_identity {
            // Cross-identity shift: either a genuine same-tab navigation or
            // Firefox re-tagging this live sink-input with another tab's title.
            // Deferred; contested shifts (the title's real owner is live) drop.
            let contested = identity_counts.get(&new_key).copied().unwrap_or(0) > 1
                || title_collides(app.pid, raw);
            if confirm_deferred_shift(&mut prev.pending_name_shift, raw, now, contested) {
                println!(
                    "Identity shift confirmed for uid={}: {:?} → {:?}",
                    prev.uid, prev.identity, new_key
                );
                prev.identity = Some(new_key);
                prev.app_name = raw.clone();
                prev.art_data = None;
            }
        } else if !crate::mpris::is_generic_name(raw) && !title_collides(app.pid, raw) {
            // Shift to an unknown (identity-less) title — e.g. navigating the
            // tab to a site we don't track. Same deferral, so a transient
            // re-tag to some other tab's random title doesn't stick instantly.
            if confirm_deferred_shift(&mut prev.pending_name_shift, raw, now, false) {
                prev.identity = None;
                prev.app_name = raw.clone();
                prev.art_data = None;
            }
        } else {
            // Generic or colliding report — noise while wearing an identity.
            prev.pending_name_shift = None;
        }
        return prev;
    }

    // No identity yet.
    if let Some(key) = raw_identity {
        // The raw name now resolves (a Kick title landed, a session got
        // learned). Adopt unless another live stream already maps to it.
        if identity_counts.get(&key).copied().unwrap_or(0) <= 1 {
            prev.identity = Some(key);
            prev.app_name = raw.clone();
        }
    } else if crate::mpris::is_generic_name(&prev.app_name) {
        // Upgrade FROM generic only, and never to a colliding title.
        if !crate::mpris::is_generic_name(raw) && !title_collides(app.pid, raw) {
            prev.app_name = raw.clone();
        } else if let Some(key) = app
            .pid
            .and_then(|pid| media_session::recover_recent_identity(pid, recovery_worn, now))
        {
            // A returning stream is created ~100ms after the PulseAudio event,
            // but Firefox flips its active media session to the unmuted tab a
            // beat later — so creation-time recovery races the session signal
            // and can lose. A still-generic channel must keep retrying on
            // later refreshes (the watcher queues one when the session lands),
            // or "AudioStream" sticks forever.
            let name = app
                .pid
                .and_then(|pid| media_session::display_name_for(pid, &key))
                .unwrap_or_else(|| prev.app_name.clone());
            println!(
                "Recovered identity {:?} ({}) for generic stream uid={}",
                name, key, prev.uid
            );
            prev.identity = Some(key);
            prev.app_name = name;
        }
    } else if raw != &prev.app_name
        && !crate::mpris::is_generic_name(raw)
        && !title_collides(app.pid, raw)
    {
        // Identity-less rename (track change in a music app, a game window
        // title settling). Browsers virtually always resolve an identity, so
        // this is the non-browser path.
        prev.app_name = raw.clone();
        prev.art_data = None;
    }
    prev
}

/// Build a channel for a stream with no prior state, re-identifying it from
/// the session table when its reported name can't be trusted.
fn build_new(
    app: &AppInfo,
    raw_identity: Option<String>,
    recovery_worn: &HashSet<String>,
    prev_identities: &HashSet<String>,
    title_collides: &dyn Fn(Option<u32>, &str) -> bool,
    identity_counts: &HashMap<String, usize>,
    now: SystemTime,
) -> MixerChannel {
    let raw = &app.app_name;

    // A new sink-input's name is untrusted when it is generic, duplicates a
    // live sibling's title, or maps to an identity another stream already
    // owns — all signatures of Firefox naming it after the active session
    // rather than its own tab.
    let identity_taken = raw_identity
        .as_ref()
        .map(|k| identity_counts.get(k).copied().unwrap_or(0) > 1 || prev_identities.contains(k))
        .unwrap_or(false);
    let untrusted = crate::mpris::is_generic_name(raw)
        || title_collides(app.pid, raw)
        || identity_taken;

    let (identity, app_name) = if untrusted {
        // The tab that just started playing is almost always the freshest
        // media session on this PID (an unmute re-activates it *now*).
        let recovered = app
            .pid
            .and_then(|pid| media_session::recover_recent_identity(pid, recovery_worn, now));
        match recovered {
            Some(key) => {
                let name = app
                    .pid
                    .and_then(|pid| media_session::display_name_for(pid, &key))
                    .unwrap_or_else(|| raw.clone());
                println!(
                    "Recovered identity {:?} ({}) for new stream uid={} (reported {:?})",
                    name, key, app.uid, raw
                );
                (Some(key), name)
            }
            // No trustworthy identity: keep the reported name (the label may
            // be transiently wrong; the multi-sink label guard and later
            // media.name updates keep it presentable).
            None => (None, raw.clone()),
        }
    } else {
        (raw_identity, raw.clone())
    };

    let (icon_uri, icon_uri_mute, uses_default_icon) = get_app_icon_uri(
        app.icon_name.clone(),
        app.icon_search_name.clone(),
        None,
        app.wm_class.as_deref(),
        app.window_icon.as_ref(),
    );

    MixerChannel {
        header_id: None,
        upper_vol_btn_id: None,
        lower_vol_btn_id: None,
        dial_id: None,
        uid: app.uid,
        member_uids: app.member_uids.clone(),
        pid: app.pid,
        app_name,
        app_id: app.app_id.clone(),
        sink_name: app.sink_name.clone(),
        mute: app.mute,
        vol_percent: app.vol_percent,
        icon_uri,
        icon_uri_mute,
        uses_default_icon,
        identity,
        art_data: None,
        pending_name_shift: None,
        is_device: app.is_device,
        is_multi_sink_app: app.is_multi_sink_app,
    }
}

/// Resolve a channel's art from its identity (session-captured bytes or a
/// deterministic fetch), falling back to the themed/window-icon pipeline.
/// Idempotent per refresh: unchanged art (same `Arc`) skips re-encoding.
fn resolve_channel_art(
    channel: &mut MixerChannel,
    app: &AppInfo,
    claimed: &mut HashMap<String, u32>,
) {
    let identity_art = match (&channel.identity, channel.pid) {
        (Some(key), Some(pid)) => {
            match claimed.get(key) {
                Some(&owner) if owner != channel.uid => None, // sibling owns it
                _ => {
                    claimed.insert(key.clone(), channel.uid);
                    // Keep the display name canonical (Twitch subtitles → the
                    // "<Artist> - Twitch" tab form; others → session title).
                    if let Some(canon) = media_session::display_name_for(pid, key) {
                        channel.app_name = canon;
                    }
                    match media_session::art_for_identity(pid, key) {
                        ChannelArt::Ready(bytes) => Some(bytes),
                        // Pending/None: keep whatever icon is showing; the
                        // completed fetch queues a refresh.
                        _ => None,
                    }
                }
            }
        }
        _ => None,
    };

    if let Some(bytes) = identity_art {
        let unchanged = channel
            .art_data
            .as_ref()
            .map(|a| Arc::ptr_eq(a, &bytes))
            .unwrap_or(false);
        if !unchanged {
            channel.art_data = Some(bytes);
            let (icon_uri, icon_uri_mute, uses_default_icon) = get_app_icon_uri(
                app.icon_name.clone(),
                app.icon_search_name.clone(),
                channel.art_bytes(),
                None,
                None,
            );
            channel.icon_uri = icon_uri;
            channel.icon_uri_mute = icon_uri_mute;
            channel.uses_default_icon = uses_default_icon;
        }
        return;
    }

    // No identity art: while the channel is still on the default icon, keep
    // re-resolving from the latest enrichment — an app's XWayland window (and
    // the themed icon it unlocks) can become resolvable a refresh or two after
    // the audio stream first appears.
    if channel.art_data.is_none() && channel.uses_default_icon {
        let (icon_uri, icon_uri_mute, uses_default_icon) = get_app_icon_uri(
            app.icon_name.clone(),
            app.icon_search_name.clone(),
            None,
            app.wm_class.as_deref(),
            app.window_icon.as_ref(),
        );
        channel.icon_uri = icon_uri;
        channel.icon_uri_mute = icon_uri_mute;
        channel.uses_default_icon = uses_default_icon;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare_channel(app_name: &str, sink_name: Option<&str>, multi: bool) -> MixerChannel {
        MixerChannel {
            header_id: None,
            upper_vol_btn_id: None,
            lower_vol_btn_id: None,
            dial_id: None,
            uid: 1,
            member_uids: vec![1],
            pid: Some(1),
            app_name: app_name.to_string(),
            app_id: app_name.to_string(),
            sink_name: sink_name.map(str::to_string),
            mute: false,
            vol_percent: 100.0,
            icon_uri: String::new(),
            icon_uri_mute: String::new(),
            uses_default_icon: true,
            identity: None,
            art_data: None,
            pending_name_shift: None,
            is_device: false,
            is_multi_sink_app: multi,
        }
    }

    #[test]
    fn multi_sink_label_never_falls_back_to_a_generic_sink_name() {
        // Firefox sink-inputs are all named "AudioStream"; when two channels
        // transiently share an app name, the label must not degrade to that.
        let ch = bare_channel("Nmplol - Twitch", Some("AudioStream"), true);
        assert_eq!(ch.display_label(), "Nmplol - Twitch");
        // A real sink name still disambiguates same-named app instances.
        let ch = bare_channel("Music App", Some("Headphones"), true);
        assert_eq!(ch.display_label(), "Headphones");
    }

    #[test]
    fn deferred_shift_confirms_only_after_uncontested_window() {
        let t0 = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let mut pending = None;

        // First sighting: recorded, not applied.
        assert!(!confirm_deferred_shift(&mut pending, "Kick Title", t0, false));
        assert!(pending.is_some());
        // Still inside the window: not applied.
        let t1 = t0 + std::time::Duration::from_secs(2);
        assert!(!confirm_deferred_shift(&mut pending, "Kick Title", t1, false));
        // Window elapsed, uncontested: confirmed, pending cleared.
        let t2 = t0 + SHIFT_CONFIRM;
        assert!(confirm_deferred_shift(&mut pending, "Kick Title", t2, false));
        assert!(pending.is_none());
    }

    #[test]
    fn deferred_shift_cancelled_when_identity_owner_appears() {
        // The lie: the proposed title's real owner (the other tab's own
        // sink-input) shows up during the window → pending dropped.
        let t0 = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let mut pending = None;
        assert!(!confirm_deferred_shift(&mut pending, "Kick Title", t0, false));
        let t2 = t0 + SHIFT_CONFIRM;
        assert!(!confirm_deferred_shift(&mut pending, "Kick Title", t2, true));
        assert!(pending.is_none(), "contested shift is dropped, not deferred");
    }

    #[test]
    fn deferred_shift_resets_when_proposed_name_changes() {
        let t0 = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let mut pending = None;
        assert!(!confirm_deferred_shift(&mut pending, "Title A", t0, false));
        // A different proposal restarts the window.
        let t2 = t0 + SHIFT_CONFIRM;
        assert!(!confirm_deferred_shift(&mut pending, "Title B", t2, false));
        assert_eq!(pending.as_ref().map(|(n, _)| n.as_str()), Some("Title B"));
    }

    fn app(uid: u32, pid: Option<u32>, name: &str) -> AppInfo {
        AppInfo {
            uid,
            member_uids: vec![uid],
            app_name: name.to_string(),
            app_id: "Firefox".to_string(),
            icon_search_name: "firefox".to_string(),
            pid,
            sink_name: None,
            mute: false,
            vol_percent: 100.0,
            icon_name: None,
            is_device: false,
            is_browser: true,
            is_multi_sink_app: false,
            client_pid: None,
            client_name: None,
            client_binary: None,
            wm_class: None,
            window_icon: None,
        }
    }

    fn no_collision() -> impl Fn(Option<u32>, &str) -> bool {
        |_, _| false
    }

    #[test]
    fn existing_channel_ignores_generic_noise_while_wearing_identity() {
        let mut prev = bare_channel("Nmplol - Twitch", None, false);
        prev.identity = Some("ttv:nmplol".into());
        let a = app(1, Some(1), "AudioStream");
        let out = reconcile_existing(
            prev,
            &a,
            None,
            &no_collision(),
            &HashMap::new(),
            &HashSet::new(),
            SystemTime::now(),
        );
        assert_eq!(out.app_name, "Nmplol - Twitch");
        assert_eq!(out.identity.as_deref(), Some("ttv:nmplol"));
    }

    #[test]
    fn existing_generic_channel_upgrades_to_real_name_and_identity() {
        let prev = bare_channel("AudioStream", None, false);
        let a = app(1, Some(1), "Nmplol - Twitch");
        let out = reconcile_existing(
            prev,
            &a,
            Some("ttv:nmplol".into()),
            &no_collision(),
            &HashMap::new(),
            &HashSet::new(),
            SystemTime::now(),
        );
        assert_eq!(out.identity.as_deref(), Some("ttv:nmplol"));
        assert_eq!(out.app_name, "Nmplol - Twitch");
    }

    #[test]
    fn existing_channel_refuses_identity_already_worn_by_a_sibling() {
        // The raw name resolves to an identity another live stream maps to —
        // the lie, not this channel's own content.
        let prev = bare_channel("AudioStream", None, false);
        let a = app(1, Some(1), "Nmplol - Twitch");
        let counts: HashMap<String, usize> = [("ttv:nmplol".to_string(), 2)].into();
        let out = reconcile_existing(
            prev,
            &a,
            Some("ttv:nmplol".into()),
            &no_collision(),
            &counts,
            &HashSet::new(),
            SystemTime::now(),
        );
        assert_eq!(out.identity, None);
        assert_eq!(out.app_name, "AudioStream", "lie name not adopted");
    }

    #[test]
    fn existing_generic_channel_recovers_identity_on_a_later_refresh() {
        // The unmute race: the channel was created as "AudioStream" before the
        // unmuted tab's session re-published. A later refresh (queued by the
        // watcher when the session lands) must re-identify it.
        let pid = 920_001;
        crate::media_session::test_seed_session(
            pid,
            "kick:asmongold247",
            "Asmongold247 Stream - Watch Live on Kick",
            SystemTime::now(),
        );
        let mut prev = bare_channel("AudioStream", None, false);
        prev.pid = Some(pid);
        let a = {
            let mut a = app(1, Some(pid), "AudioStream");
            a.pid = Some(pid);
            a
        };
        let out = reconcile_existing(
            prev,
            &a,
            None,
            &no_collision(),
            &HashMap::new(),
            &HashSet::new(),
            SystemTime::now(),
        );
        assert_eq!(out.identity.as_deref(), Some("kick:asmongold247"));
        assert_eq!(out.app_name, "Asmongold247 Stream - Watch Live on Kick");
        crate::media_session::test_clear_sessions(pid);
    }

    #[test]
    fn existing_generic_channel_never_recovers_a_worn_identity() {
        let pid = 920_002;
        crate::media_session::test_seed_session(
            pid,
            "kick:asmongold247",
            "Asmongold247 Stream - Watch Live on Kick",
            SystemTime::now(),
        );
        let mut prev = bare_channel("AudioStream", None, false);
        prev.pid = Some(pid);
        let a = app(1, Some(pid), "AudioStream");
        let worn: HashSet<String> = [String::from("kick:asmongold247")].into();
        let out = reconcile_existing(
            prev,
            &a,
            None,
            &no_collision(),
            &HashMap::new(),
            &worn,
            SystemTime::now(),
        );
        assert_eq!(out.identity, None);
        assert_eq!(out.app_name, "AudioStream");
        crate::media_session::test_clear_sessions(pid);
    }

    #[test]
    fn new_stream_with_taken_identity_is_not_trusted() {
        // A brand-new sink-input wearing an identity a surviving channel
        // already owns must not duplicate it (recovery may rename it, but
        // never onto the taken identity).
        let a = app(9, Some(424_242), "Nmplol - Twitch");
        let prev_ids: HashSet<String> = [String::from("ttv:nmplol")].into();
        let worn = prev_ids.clone();
        let out = build_new(
            &a,
            Some("ttv:nmplol".into()),
            &worn,
            &prev_ids,
            &no_collision(),
            &HashMap::new(),
            SystemTime::now(),
        );
        assert_ne!(out.identity.as_deref(), Some("ttv:nmplol"));
    }
}
