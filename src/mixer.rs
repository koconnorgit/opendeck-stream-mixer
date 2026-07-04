use crate::audio::audio_system::AppInfo;
use crate::utils::get_app_icon_uri;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
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
    /// Cached MPRIS art image bytes — locked once captured.
    pub mpris_art_data: Option<Vec<u8>>,
    /// Filesystem path of the MPRIS art file this channel has claimed.
    /// Other channels exclude this path so they never inherit this slot's art.
    pub mpris_art_path: Option<PathBuf>,
    /// Once true, name and icon are locked and won't be overwritten by refreshes.
    /// Only volume, mute, and other live data update. Reset when stream stops.
    pub locked: bool,
    /// Wall-clock time this sink-input was first observed appearing, used to
    /// correlate art files to the stream by timing. `None` means the stream was
    /// already playing at cold start (true start unknown → best-effort claim).
    pub first_seen: Option<std::time::SystemTime>,
    /// Once true, the art-claim decision is final and never re-attempted —
    /// whether it produced art or a deliberate "no art". Stops a later refresh
    /// from grabbing an unrelated file. Reset only on a name-shift.
    pub art_resolved: bool,
    pub is_device: bool,
    pub is_multi_sink_app: bool,
}

impl MixerChannel {
    /// Text shown on the LCD / header for this channel. Collapsed multi-sink apps
    /// show the sink name; everything else shows the stream name, with Kick tab
    /// titles trimmed to just the streamer handle. The full
    /// `"<handle> Stream - Watch Live on Kick"` is kept in `app_name` (the avatar
    /// fetch and Kick detection key off it) but is too long/noisy to display.
    pub fn display_label(&self) -> String {
        if self.is_multi_sink_app {
            self.sink_name.clone().unwrap_or_else(|| self.app_name.clone())
        } else {
            crate::kick::display_name(&self.app_name)
        }
    }
}

pub static MIXER_CHANNELS: LazyLock<Mutex<HashMap<u8, MixerChannel>>> =
    LazyLock::new(|| Mutex::const_new(HashMap::new()));

/// Maps encoder dial position → mixer channel index (independent from button columns)
pub static ENCODER_TO_CHANNEL_MAP: LazyLock<Mutex<HashMap<u8, u8>>> =
    LazyLock::new(|| Mutex::const_new(HashMap::new()));

/// How long a logical stream's art is remembered after the sink-input it was
/// attached to disappears. Long enough to bridge a stream being torn down and
/// recreated under a new sink-input index (e.g. a browser re-routing a `<video>`
/// through the Web Audio API for a compressor/EQ), short enough that art is not
/// mis-applied to an unrelated later stream that happens to reuse the identity.
const ART_MEMORY_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// MPRIS art remembered by *logical* stream identity (PID + display name) rather
/// than the volatile sink-input index that drives `MixerChannel::uid`. When a
/// stream is recreated under a new index its successor can inherit this art
/// instead of reverting to a generic app icon. See `update_mixer_channels`.
struct RememberedArt {
    path: PathBuf,
    data: Vec<u8>,
    last_seen: SystemTime,
}

static ART_MEMORY: LazyLock<std::sync::Mutex<HashMap<(u32, String), RememberedArt>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Record a channel's claimed art under its logical identity, if it has both a
/// PID and a real (non-generic) name. No-op for channels without art.
fn remember_channel_art(
    mem: &mut HashMap<(u32, String), RememberedArt>,
    ch: &MixerChannel,
    now: SystemTime,
) {
    if crate::mpris::is_generic_name(&ch.app_name) {
        return;
    }
    let (Some(pid), Some(path), Some(data)) =
        (ch.pid, ch.mpris_art_path.clone(), ch.mpris_art_data.clone())
    else {
        return;
    };
    mem.insert(
        (pid, ch.app_name.clone()),
        RememberedArt {
            path,
            data,
            last_seen: now,
        },
    );
}

/// Re-identify a stream that Firefox tagged with the wrong title. A freshly
/// created sink-input can inherit the browser's *active* Media Session title
/// instead of its own tab's (e.g. Twitch unmuted in-browser while a YouTube
/// video is the active session comes back wearing the video's title). When that
/// mis-tag is detected (the name collides with a live sibling on the same PID),
/// the true identity is recovered here: if exactly one remembered identity on
/// this PID is *not* currently held by a live stream, it is almost certainly
/// this returning stream. Returns its name (the caller's art-inheritance then
/// restores the matching art). Ambiguous/empty cases return `None`, never a
/// guess. `live_names` holds the lowercased names of every live stream this
/// refresh, so an identity still on screen is never reused.
fn recover_identity_from_memory(pid: u32, live_names: &HashSet<String>) -> Option<String> {
    let mem = ART_MEMORY.lock().unwrap();
    let mut dormant = mem
        .keys()
        .filter(|(p, name)| *p == pid && !live_names.contains(&name.to_lowercase()))
        .map(|(_, name)| name.clone());
    let candidate = dormant.next()?;
    if dormant.next().is_some() {
        return None; // Ambiguous — don't guess which returning stream this is.
    }
    Some(candidate)
}

/// Look up remembered art for a *confirmed* logical identity (PID + name) and
/// return it unless a still-active stream already holds that file. Used to
/// restore an avatar/art the instant the browser re-publishes a stream's real
/// `media.name` after it briefly reported a generic one (e.g. an in-browser
/// mute/unmute recreated the sink-input). Keyed on the confirmed name, so it
/// can never graft an unrelated stream's art onto this one.
fn remembered_art_for(
    pid: u32,
    name: &str,
    claimed_paths: &HashSet<PathBuf>,
) -> Option<(PathBuf, Vec<u8>)> {
    let mem = ART_MEMORY.lock().unwrap();
    mem.get(&(pid, name.to_string())).and_then(|a| {
        if claimed_paths.contains(&a.path) {
            None
        } else {
            Some((a.path.clone(), a.data.clone()))
        }
    })
}

/// Count how many active streams share each PID, so the cold-start art claim can
/// tell an unambiguous single-stream PID from an ambiguous multi-stream one.
fn count_pids(apps: &[AppInfo]) -> HashMap<u32, usize> {
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for app in apps {
        // Kick streams never write a Firefox MPRIS art file, so they don't make a
        // shared PID ambiguous for a sibling tab's cold-start claim. Counting them
        // would wrongly block e.g. a Twitch tab from claiming its art when a Kick
        // tab is open in the same browser process.
        if crate::kick::kick_slug(&app.app_name).is_some() {
            continue;
        }
        if let Some(pid) = app.pid {
            *counts.entry(pid).or_insert(0) += 1;
        }
    }
    counts
}

/// Apply a claimed art file to a channel: mark the path claimed and rebuild the
/// icon (normal + muted) from the art bytes.
fn apply_art(
    channel: &mut MixerChannel,
    icon_name: Option<String>,
    icon_search_name: String,
    path: PathBuf,
    bytes: Vec<u8>,
    claimed_paths: &mut HashSet<PathBuf>,
) {
    claimed_paths.insert(path.clone());
    let (icon_uri, icon_uri_mute, uses_default_icon) =
        get_app_icon_uri(icon_name, icon_search_name, Some(&bytes), None, None);
    channel.icon_uri = icon_uri;
    channel.icon_uri_mute = icon_uri_mute;
    channel.uses_default_icon = uses_default_icon;
    channel.mpris_art_data = Some(bytes);
    channel.mpris_art_path = Some(path);
}

/// The identity key a channel's synthetic `identity://` art path encodes, if
/// that's what it is wearing.
fn identity_of_path(path: Option<&PathBuf>) -> Option<&str> {
    path.and_then(|p| p.to_str())
        .and_then(|s| s.strip_prefix("identity://"))
}

/// Outcome of the identity-first art attempt (phase 2 of
/// docs/mpris-subscription-design.md).
enum IdentityArt {
    /// Identity art applied; the art decision is final for this refresh.
    Applied,
    /// Identity known but its fetch is in flight — keep the channel unlocked
    /// so a later refresh can swap the art in (same contract as `kick_pending`).
    Pending,
    /// No identity/art — the legacy pipeline's decision stands.
    Unresolved,
}

/// Phase 2: try to dress a channel in identity-keyed art (session-captured or
/// deterministically fetched). Runs *after* the legacy pipeline so identity
/// art wins when available and everything else is untouched. The art is
/// claimed under a synthetic `identity://<key>` path so it flows through the
/// existing claim-exclusion and ART_MEMORY plumbing (identity-recovery and
/// art-inheritance keep working unchanged until phase 3 replaces them).
fn try_identity_art(
    channel: &mut MixerChannel,
    icon_name: Option<String>,
    icon_search_name: String,
    claimed_paths: &mut HashSet<PathBuf>,
) -> IdentityArt {
    let Some(pid) = channel.pid else {
        return IdentityArt::Unresolved;
    };
    match crate::media_session::channel_art(pid, &channel.app_name) {
        crate::media_session::ChannelArt::Ready(key, bytes) => {
            let path = PathBuf::from(format!("identity://{key}"));
            if claimed_paths.contains(&path) && channel.mpris_art_path.as_ref() != Some(&path) {
                // A live sibling already wears this identity (two tabs, same
                // content) — don't double-claim.
                return IdentityArt::Unresolved;
            }
            // Canonicalize the display name: a recreated sink-input wears the
            // active session's title (the stream subtitle on Twitch), and
            // whatever name sticks here also becomes the ART_MEMORY identity
            // that collision recovery later restores — it must be the real one.
            if let Some(canon) = crate::media_session::display_name_for(pid, &key) {
                channel.app_name = canon;
            }
            if channel.mpris_art_path.as_ref() == Some(&path) {
                // Already wearing this identity's art; keep the claim fresh.
                claimed_paths.insert(path);
                return IdentityArt::Applied;
            }
            if let Some(old) = channel.mpris_art_path.take() {
                claimed_paths.remove(&old);
            }
            apply_art(
                channel,
                icon_name,
                icon_search_name,
                path,
                (*bytes).clone(),
                claimed_paths,
            );
            IdentityArt::Applied
        }
        crate::media_session::ChannelArt::Pending(_) => IdentityArt::Pending,
        crate::media_session::ChannelArt::None => IdentityArt::Unresolved,
    }
}

/// Make (or defer) the art-claim decision for a channel. No-op once resolved.
///
/// For an observed stream (`first_seen = Some`) it waits `SETTLE` for the art to
/// be written, then commits a final decision either way. At cold start it
/// best-effort claims for unambiguous single-stream PIDs and otherwise leaves
/// the channel unresolved so a later refresh can try again once the art lands.
fn resolve_art(
    channel: &mut MixerChannel,
    icon_name: Option<String>,
    icon_search_name: String,
    now: SystemTime,
    pid_counts: &HashMap<u32, usize>,
    claimed_paths: &mut HashSet<PathBuf>,
) {
    if channel.art_resolved {
        return;
    }
    let Some(pid) = channel.pid else {
        // Devices / system audio never have Firefox art; decision is immediate.
        channel.art_resolved = true;
        return;
    };
    let same_pid_count = pid_counts.get(&pid).copied().unwrap_or(1);

    match channel.first_seen {
        Some(first_seen) => {
            // Too early: the browser may not have written art yet. Retry later.
            let too_early = now
                .duration_since(first_seen)
                .map(|age| age < crate::mpris::SETTLE)
                .unwrap_or(false);
            if too_early {
                return;
            }
            if let Some((path, bytes)) =
                crate::mpris::claim_art(pid, Some(first_seen), same_pid_count, claimed_paths)
            {
                apply_art(channel, icon_name, icon_search_name, path, bytes, claimed_paths);
            }
            // Final whether or not a file matched.
            channel.art_resolved = true;
        }
        None => {
            // Cold start. Refuse to guess when the PID is shared.
            if same_pid_count > 1 {
                channel.art_resolved = true;
                return;
            }
            if let Some((path, bytes)) =
                crate::mpris::claim_art(pid, None, same_pid_count, claimed_paths)
            {
                apply_art(channel, icon_name, icon_search_name, path, bytes, claimed_paths);
                channel.art_resolved = true;
            }
            // No art on disk yet → stay unresolved; a later refresh can claim it.
        }
    }
}

/// Whether an app is on the user's exclude list. Matches the stable app id
/// (so an app stays excluded across track/tab/title changes) and, for backward
/// compatibility, the display name (older entries were stored that way).
fn is_ignored(app: &AppInfo, ignored_apps: &[String]) -> bool {
    ignored_apps.iter().any(|n| n == &app.app_id || n == &app.app_name)
}

pub async fn create_mixer_channels(applications: Vec<AppInfo>, ignored_apps: &[String]) {
    let mut channels = MIXER_CHANNELS.lock().await;
    let mut claimed_paths: HashSet<PathBuf> = HashSet::new();
    let now = SystemTime::now();

    let mut active: Vec<AppInfo> = Vec::new();
    for app in applications.into_iter() {
        if is_ignored(&app, ignored_apps) {
            println!("Skipping ignored app: {}", app.app_name);
            continue;
        }
        active.push(app);
    }
    let pid_counts = count_pids(&active);

    let mut col_key: u8 = 0;
    for app in active.into_iter() {
        // Kept aside for the identity-art post-pass; `app`'s own copies are
        // moved into the legacy pipeline below.
        let pp_icon_name = app.icon_name.clone();
        let pp_icon_search = app.icon_search_name.clone();

        // A Kick avatar (when already fetched) takes the MPRIS-art slot.
        let (icon_uri, icon_uri_mute, uses_default_icon) = get_app_icon_uri(
            app.icon_name.clone(),
            app.icon_search_name.clone(),
            app.kick_art.as_deref(),
            app.wm_class.as_deref(),
            app.window_icon.as_ref(),
        );

        let mut channel = MixerChannel {
            header_id: None,
            upper_vol_btn_id: None,
            lower_vol_btn_id: None,
            dial_id: None,
            uid: app.uid,
            member_uids: app.member_uids.clone(),
            pid: app.pid,
            app_name: app.app_name.clone(),
            app_id: app.app_id.clone(),
            sink_name: app.sink_name.clone(),
            mute: app.mute,
            vol_percent: app.vol_percent,
            icon_uri,
            icon_uri_mute,
            uses_default_icon,
            mpris_art_data: app.kick_art.clone(),
            mpris_art_path: None,
            // Cold start: these streams may have been playing for a while, so we
            // have no reliable timing anchor for them.
            first_seen: None,
            art_resolved: false,
            locked: false,
            is_device: app.is_device,
            is_multi_sink_app: app.is_multi_sink_app,
        };

        if crate::kick::kick_slug(&app.app_name).is_some() {
            // Kick streams never write their own MPRIS art file; their only art
            // source is the avatar (already in `mpris_art_data` above, if fetched).
            // Skip the file-claim so a sibling tab's art can't be mis-attributed.
            channel.art_resolved = true;
        } else {
            resolve_art(
                &mut channel,
                app.icon_name,
                app.icon_search_name,
                now,
                &pid_counts,
                &mut claimed_paths,
            );
        }

        let has_good_name = !crate::mpris::is_generic_name(&channel.app_name);
        // Keep a Kick channel unlocked until its avatar fetch settles, so a later
        // refresh can swap in the avatar instead of locking the default icon.
        channel.locked = has_good_name && channel.art_resolved && !app.kick_pending;

        // Phase 2: identity-keyed art overrides the legacy decision when known.
        match try_identity_art(&mut channel, pp_icon_name, pp_icon_search, &mut claimed_paths) {
            IdentityArt::Applied => {
                channel.art_resolved = true;
                channel.locked = has_good_name && !app.kick_pending;
            }
            IdentityArt::Pending => channel.locked = false,
            IdentityArt::Unresolved => {}
        }

        channels.insert(col_key, channel);
        col_key += 1;
    }
}

pub async fn update_mixer_channels(
    applications: Vec<crate::audio::audio_system::AppInfo>,
    ignored_apps: &[String],
) {
    let mut channels = MIXER_CHANNELS.lock().await;

    // Build list of active apps (filtered)
    let active_apps: Vec<_> = applications
        .into_iter()
        .filter(|app| !is_ignored(app, ignored_apps))
        .collect();

    // Index previous channels by UID so we can carry over name, icon, and art claims.
    let mut previous_by_uid: HashMap<u32, MixerChannel> =
        channels.drain().map(|(_, ch)| (ch.uid, ch)).collect();

    // Pre-seed the claim set with paths owned by streams that are still active.
    // Doing this up front guarantees that a slot without art cannot accidentally
    // re-claim a path already owned by another active slot, regardless of iteration order.
    let active_uids: HashSet<u32> = active_apps.iter().map(|a| a.uid).collect();
    let mut claimed_paths: HashSet<PathBuf> = previous_by_uid
        .iter()
        .filter(|(uid, _)| active_uids.contains(uid))
        .filter_map(|(_, ch)| ch.mpris_art_path.clone())
        .collect();

    let now = SystemTime::now();
    let pid_counts = count_pids(&active_apps);

    // Count cleaned media.names per PID across all live streams. A count > 1 means
    // two streams in one browser process share a (cleaned) title — the tell-tale
    // that Firefox tagged a freshly-created sink-input with the active session's
    // title instead of its own tab's. Such a name must not be trusted as identity.
    let mut cleaned_name_counts: HashMap<(u32, String), usize> = HashMap::new();
    for a in &active_apps {
        if let Some(pid) = a.pid {
            let key = (pid, crate::mpris::clean_stream_title(&a.app_name).to_lowercase());
            *cleaned_name_counts.entry(key).or_insert(0) += 1;
        }
    }
    // Whether a stream's media.name duplicates another live sibling on its PID.
    let title_collides = |pid: Option<u32>, name: &str| -> bool {
        pid.map_or(false, |pid| {
            let key = (pid, crate::mpris::clean_stream_title(name).to_lowercase());
            cleaned_name_counts.get(&key).copied().unwrap_or(0) > 1
        })
    };

    // Lowercased names of every live stream this refresh, so identity recovery
    // never re-adopts an identity that is still on screen.
    let live_names: HashSet<String> =
        active_apps.iter().map(|a| a.app_name.to_lowercase()).collect();

    // Remember every prior channel's art by logical identity *before* processing.
    // This deliberately captures streams that are about to disappear this refresh
    // (their uid is gone from the active set): a recreated stream's successor,
    // processed below, inherits exactly that just-departed predecessor's art.
    // Pruning here also bounds the table to the grace window.
    {
        let mut mem = ART_MEMORY.lock().unwrap();
        mem.retain(|_, a| {
            now.duration_since(a.last_seen)
                .map(|age| age < ART_MEMORY_TTL)
                .unwrap_or(true)
        });
        for ch in previous_by_uid.values() {
            remember_channel_art(&mut mem, ch, now);
        }
    }

    let mut new_channels: HashMap<u8, MixerChannel> = HashMap::new();
    let mut col_key: u8 = 0;

    for app in active_apps {
        // Kept aside for the identity-art post-pass; `app`'s own copies are
        // moved into the legacy pipeline below.
        let pp_icon_name = app.icon_name.clone();
        let pp_icon_search = app.icon_search_name.clone();
        let pp_kick_pending = app.kick_pending;

        let mut channel = if let Some(mut prev) = previous_by_uid.remove(&app.uid) {
            // Detect a media.name shift on the same sink-input (e.g. FFZ's
            // long-lived AudioContext keeps the same uid across Twitch
            // navigations). When this happens, unlock the slot, drop stale
            // art, and reset to a default icon. Re-claim is deferred to the
            // delayed refresh — the new MPRIS art file may not be written
            // yet at this exact moment.
            // A "shift" to a title that collides with a live sibling is Firefox
            // re-tagging this stream with the active session's title, not a real
            // content change — ignore it so a recovered identity (below) is not
            // re-corrupted on every refresh by the persistent wrong media.name.
            // Likewise, a "shift" between two names of the SAME identity (the
            // canonical "Nmplol - Twitch" this channel wears vs. the stream
            // subtitle Firefox keeps reporting in media.name) is not a content
            // change — dropping art and re-anchoring on it would churn every
            // refresh.
            let shifted_within_identity = app
                .pid
                .zip(identity_of_path(prev.mpris_art_path.as_ref()))
                .map(|(pid, key)| {
                    crate::media_session::match_identity(pid, &app.app_name).as_deref()
                        == Some(key)
                })
                .unwrap_or(false);
            let name_shifted = !crate::mpris::is_generic_name(&app.app_name)
                && app.app_name != prev.app_name
                && !title_collides(app.pid, &app.app_name)
                && !shifted_within_identity;

            if name_shifted {
                prev.locked = false;
                prev.app_name = app.app_name.clone();
                if let Some(old_path) = prev.mpris_art_path.take() {
                    claimed_paths.remove(&old_path);
                }
                prev.mpris_art_data = None;
                // New content is a fresh stream as far as art goes: re-anchor the
                // timing and re-open the claim decision so the next refresh can
                // bind the new art file by timing.
                prev.first_seen = Some(now);
                prev.art_resolved = false;
                let (icon_uri, icon_uri_mute, uses_default_icon) = get_app_icon_uri(
                    app.icon_name.clone(),
                    app.icon_search_name.clone(),
                    None,
                    app.wm_class.as_deref(),
                    app.window_icon.as_ref(),
                );
                prev.icon_uri = icon_uri;
                prev.icon_uri_mute = icon_uri_mute;
                prev.uses_default_icon = uses_default_icon;

                // If this now-confirmed identity has art remembered from before it
                // briefly went generic (e.g. an in-browser mute/unmute), restore it
                // immediately instead of waiting on a fresh art file the browser may
                // never rewrite. Keyed on the confirmed name, so it can't graft
                // another stream's art.
                if let Some((path, data)) = app
                    .pid
                    .and_then(|pid| remembered_art_for(pid, &prev.app_name, &claimed_paths))
                {
                    apply_art(
                        &mut prev,
                        app.icon_name.clone(),
                        app.icon_search_name.clone(),
                        path,
                        data,
                        &mut claimed_paths,
                    );
                    prev.art_resolved = true;
                }
            }

            // Update live data
            prev.mute = app.mute;
            prev.vol_percent = app.vol_percent;
            prev.sink_name = app.sink_name;
            prev.is_device = app.is_device;
            prev.is_multi_sink_app = app.is_multi_sink_app;
            prev.member_uids = app.member_uids.clone();
            prev.app_id = app.app_id.clone();

            if crate::kick::kick_slug(&app.app_name).is_some() {
                // Kick streams never write their own MPRIS art file. Skip the
                // file-claim (so a sibling tab's art can't be mis-attributed) and
                // instead drop in the avatar once the fetch lands.
                prev.art_resolved = true;
                if let Some(art) = app.kick_art.as_deref().filter(|_| prev.mpris_art_data.is_none())
                {
                    let (icon_uri, icon_uri_mute, uses_default_icon) = get_app_icon_uri(
                        app.icon_name.clone(),
                        app.icon_search_name.clone(),
                        Some(art),
                        None,
                        None,
                    );
                    prev.icon_uri = icon_uri;
                    prev.icon_uri_mute = icon_uri_mute;
                    prev.uses_default_icon = uses_default_icon;
                    prev.mpris_art_data = Some(art.to_vec());
                }
            } else {
                // Make (or defer) the timing-aware art decision. No-op once resolved.
                resolve_art(
                    &mut prev,
                    app.icon_name.clone(),
                    app.icon_search_name.clone(),
                    now,
                    &pid_counts,
                    &mut claimed_paths,
                );

                // The icon is only computed when a channel is first created, but
                // an app's XWayland window (and thus its `window_icon`/`wm_class`
                // and the themed icon those unlock) can become resolvable a
                // refresh or two *after* the audio stream first appears — by which
                // point the art/lock machinery above would otherwise freeze the
                // generic default icon in place forever. So while this channel is
                // still on the default icon, keep re-resolving from the latest
                // enrichment. Once a real icon lands `uses_default_icon` flips
                // false and this stops; it runs regardless of `locked` because a
                // window that maps late must still be able to upgrade the icon.
                if prev.uses_default_icon {
                    let (icon_uri, icon_uri_mute, uses_default_icon) = get_app_icon_uri(
                        app.icon_name.clone(),
                        app.icon_search_name.clone(),
                        prev.mpris_art_data.as_deref(),
                        app.wm_class.as_deref(),
                        app.window_icon.as_ref(),
                    );
                    prev.icon_uri = icon_uri;
                    prev.icon_uri_mute = icon_uri_mute;
                    prev.uses_default_icon = uses_default_icon;
                }
            }

            // Upgrade a generic name if a better one is now available, then lock.
            if !prev.locked {
                if !crate::mpris::is_generic_name(&app.app_name) {
                    prev.app_name = app.app_name;
                }
                let has_good_name = !crate::mpris::is_generic_name(&prev.app_name);
                // Stay unlocked while a Kick avatar fetch is still in flight.
                prev.locked = has_good_name && prev.art_resolved && !app.kick_pending;
            }

            // Clear position-specific IDs (will be reassigned by update_stream_deck_buttons)
            prev.header_id = None;
            prev.upper_vol_btn_id = None;
            prev.lower_vol_btn_id = None;
            prev.dial_id = None;
            prev
        } else {
            // Brand-new stream. Don't claim MPRIS art immediately — Firefox may
            // not have written the art file for this stream yet (the file on disk
            // could still be from the previous stream). The delayed refresh (2s
            // later) will make a claim against the updated filesystem state.
            // A new sink-input that reports a generic name, or a title that
            // collides with a live sibling on its PID (Firefox tagged it with the
            // active Media Session's title rather than its own), can't be trusted
            // to name itself. Recover its true identity from the art memory — the
            // unique recently-departed identity on this PID. The art-inheritance
            // below then restores its avatar by that recovered name. A genuinely
            // new tab with a unique title isn't a collision, so it's left alone.
            let untrustworthy_name = crate::mpris::is_generic_name(&app.app_name)
                || title_collides(app.pid, &app.app_name);
            let display_name = if untrustworthy_name {
                let recovered = app
                    .pid
                    .and_then(|pid| recover_identity_from_memory(pid, &live_names));
                if let Some(name) = &recovered {
                    println!(
                        "Recovered identity {:?} for returning stream uid={} (reported {:?})",
                        name, app.uid, app.app_name,
                    );
                }
                recovered.unwrap_or_else(|| app.app_name.clone())
            } else {
                app.app_name.clone()
            };

            // Before treating this as a cold stream, check whether it's really the
            // successor of a just-departed (or very recently seen) stream with the
            // same logical identity (PID + display name) — e.g. a browser that
            // re-routed its audio through the Web Audio API, replacing the
            // sink-input under a new index. `uid` can't catch this because it *is*
            // the index; the identity-keyed memory can. Inheriting art skips the
            // generic-icon flash and the timing-window reclaim entirely. Never
            // steal art a still-active sibling already holds this refresh.
            let inherited_art = if !crate::mpris::is_generic_name(&display_name) {
                app.pid.and_then(|pid| {
                    let mem = ART_MEMORY.lock().unwrap();
                    mem.get(&(pid, display_name.clone())).and_then(|a| {
                        if claimed_paths.contains(&a.path) {
                            None
                        } else {
                            Some((a.path.clone(), a.data.clone()))
                        }
                    })
                })
            } else {
                None
            };

            // With inherited art the decision is final; otherwise anchor timing for
            // streams with a PID (browser tabs) so the delayed refresh can claim a
            // freshly written art file. Devices / system audio have no PID and never
            // carry Firefox art, so their art decision is already final.
            let (
                icon_uri,
                icon_uri_mute,
                uses_default_icon,
                mpris_art_data,
                mpris_art_path,
                first_seen,
                art_resolved,
            ) = if let Some((path, data)) = inherited_art {
                claimed_paths.insert(path.clone());
                let (icon_uri, icon_uri_mute, uses_default_icon) = get_app_icon_uri(
                    app.icon_name.clone(),
                    app.icon_search_name.clone(),
                    Some(&data),
                    None,
                    None,
                );
                (
                    icon_uri,
                    icon_uri_mute,
                    uses_default_icon,
                    Some(data),
                    Some(path),
                    Some(now),
                    true,
                )
            } else {
                let (first_seen, art_resolved) = if app.pid.is_some() {
                    (Some(now), false)
                } else {
                    (None, true)
                };
                // A ready Kick avatar takes the MPRIS-art slot for this new stream.
                let (icon_uri, icon_uri_mute, uses_default_icon) = get_app_icon_uri(
                    app.icon_name.clone(),
                    app.icon_search_name.clone(),
                    app.kick_art.as_deref(),
                    app.wm_class.as_deref(),
                    app.window_icon.as_ref(),
                );
                (
                    icon_uri,
                    icon_uri_mute,
                    uses_default_icon,
                    app.kick_art.clone(),
                    None,
                    first_seen,
                    art_resolved,
                )
            };

            let has_good_name = !crate::mpris::is_generic_name(&display_name);
            // Stay unlocked while a Kick avatar fetch is still in flight.
            let locked = has_good_name && art_resolved && !app.kick_pending;

            MixerChannel {
                header_id: None,
                upper_vol_btn_id: None,
                lower_vol_btn_id: None,
                dial_id: None,
                uid: app.uid,
                member_uids: app.member_uids.clone(),
                pid: app.pid,
                app_name: display_name,
                app_id: app.app_id.clone(),
                sink_name: app.sink_name,
                mute: app.mute,
                vol_percent: app.vol_percent,
                icon_uri,
                icon_uri_mute,
                uses_default_icon,
                mpris_art_data,
                mpris_art_path,
                first_seen,
                art_resolved,
                locked,
                is_device: app.is_device,
                is_multi_sink_app: app.is_multi_sink_app,
            }
        };

        // Phase 2: identity-keyed art overrides the legacy decision when known.
        match try_identity_art(&mut channel, pp_icon_name, pp_icon_search, &mut claimed_paths) {
            IdentityArt::Applied => {
                channel.art_resolved = true;
                channel.locked =
                    !crate::mpris::is_generic_name(&channel.app_name) && !pp_kick_pending;
            }
            IdentityArt::Pending => channel.locked = false,
            IdentityArt::Unresolved => {}
        }

        new_channels.insert(col_key, channel);
        col_key += 1;
    }

    // Refresh the identity memory with every currently-live channel's art so its
    // grace window is measured from now. This keeps art for a still-playing stream
    // alive indefinitely and only starts the TTL countdown once it truly stops.
    {
        let mut mem = ART_MEMORY.lock().unwrap();
        for ch in new_channels.values() {
            remember_channel_art(&mut mem, ch, now);
        }
    }

    // Phase-1 shadow mode (docs/mpris-subscription-design.md): log what the
    // media-session watcher would have decided next to what this pipeline
    // chose. Observation only — no behavior depends on it.
    crate::media_session::shadow_log(&new_channels);

    *channels = new_channels;

    println!(
        "Updated mixer channels (filtered {} ignored apps)",
        ignored_apps.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remember(pid: u32, name: &str, path: &str) {
        let mut mem = ART_MEMORY.lock().unwrap();
        mem.insert(
            (pid, name.to_string()),
            RememberedArt {
                path: PathBuf::from(path),
                data: vec![1u8, 2, 3],
                last_seen: SystemTime::now(),
            },
        );
    }

    fn forget(pid: u32) {
        ART_MEMORY.lock().unwrap().retain(|(p, _), _| *p != pid);
    }

    // Each test uses a private PID so the process-global ART_MEMORY stays isolated.

    #[test]
    fn restores_remembered_art_for_confirmed_identity() {
        // After a stream's real media.name is re-confirmed, its prior art is
        // restored — keyed on PID + the confirmed name only.
        let pid = 900_001;
        forget(pid);
        remember(pid, "Example Stream - Twitch", "/run/firefox-mpris/900001_1.png");
        let got = remembered_art_for(pid, "Example Stream - Twitch", &HashSet::new());
        assert_eq!(
            got,
            Some((PathBuf::from("/run/firefox-mpris/900001_1.png"), vec![1u8, 2, 3]))
        );
        forget(pid);
    }

    #[test]
    fn does_not_restore_a_different_identitys_art() {
        // A different identity remembered on the same PID must not be returned —
        // the key is PID + exact name, never PID alone. (Names here are synthetic
        // placeholders; real names only ever enter the table at runtime.)
        let pid = 900_002;
        forget(pid);
        remember(pid, "Example Video - YouTube", "/run/firefox-mpris/900002_1.png");
        assert_eq!(remembered_art_for(pid, "Example Stream - Twitch", &HashSet::new()), None);
        forget(pid);
    }

    #[test]
    fn never_steals_art_a_live_stream_still_holds() {
        let pid = 900_003;
        let path = "/run/firefox-mpris/900003_1.png";
        forget(pid);
        remember(pid, "Example Stream - Twitch", path);
        let claimed: HashSet<PathBuf> = [PathBuf::from(path)].into_iter().collect();
        assert_eq!(remembered_art_for(pid, "Example Stream - Twitch", &claimed), None);
        forget(pid);
    }

    #[test]
    fn no_memory_for_identity_yields_nothing() {
        let pid = 900_004;
        forget(pid);
        assert_eq!(remembered_art_for(pid, "Anything", &HashSet::new()), None);
    }

    fn live(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| n.to_lowercase()).collect()
    }

    // Names below are synthetic placeholders standing in for whatever a session
    // actually played — real names only enter ART_MEMORY at runtime, never here.

    #[test]
    fn recovers_lone_dormant_identity_for_mis_tagged_stream() {
        // A returning stream wears a live video's title; its own remembered
        // identity is the only one remembered-but-absent, so it's recovered.
        let pid = 900_101;
        forget(pid);
        remember(pid, "Example Stream - Twitch", "/run/firefox-mpris/900101_1.png");
        let live = live(&["example video - youtube", "example video"]);
        assert_eq!(
            recover_identity_from_memory(pid, &live).as_deref(),
            Some("Example Stream - Twitch")
        );
        forget(pid);
    }

    #[test]
    fn does_not_reuse_an_identity_still_on_screen() {
        let pid = 900_102;
        forget(pid);
        remember(pid, "Example Stream - Twitch", "/run/firefox-mpris/900102_1.png");
        // That stream is still live — nothing dormant to recover.
        assert_eq!(
            recover_identity_from_memory(pid, &live(&["example stream - twitch"])),
            None
        );
        forget(pid);
    }

    #[test]
    fn identity_of_path_extracts_key_only_from_synthetic_paths() {
        let synthetic = PathBuf::from("identity://ttv:nmplol");
        assert_eq!(identity_of_path(Some(&synthetic)), Some("ttv:nmplol"));
        let file = PathBuf::from("/run/firefox-mpris/1656_1.png");
        assert_eq!(identity_of_path(Some(&file)), None);
        assert_eq!(identity_of_path(None), None);
    }

    #[test]
    fn refuses_to_guess_between_multiple_dormant_identities() {
        let pid = 900_103;
        forget(pid);
        remember(pid, "Example Stream A - Twitch", "/run/firefox-mpris/900103_1.png");
        remember(pid, "Example Stream B - Twitch", "/run/firefox-mpris/900103_2.png");
        assert_eq!(recover_identity_from_memory(pid, &live(&[])), None);
        forget(pid);
    }
}
