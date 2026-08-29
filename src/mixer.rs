use crate::audio::audio_system::AppInfo;
use crate::media_session::{self, ChannelArt};
use crate::reconcile::{self, Effects, IdentityState, RefreshInput};
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
    /// Set when the identity came from the departure trail alone — no session
    /// signal named this stream, the trail was merely the best inference. The
    /// adoption stands, but until `until` a fresh session signal newer than
    /// `floor` may correct it: on a streamer switch Firefox emits the new
    /// tab's Media Session ~0.5-1s AFTER its sink-input, so trail-based
    /// recovery decides before the real answer arrives (observed 2026-08-29:
    /// agent00→ariathome handed the new stream the dead tab's identity, then
    /// re-laundered it through the departure table on every churn).
    pub provisional: Option<ProvisionalRecovery>,
    pub is_device: bool,
    pub is_multi_sink_app: bool,
}

/// The correction window riding a departure-trail identity adoption.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProvisionalRecovery {
    /// When the window closes; silence until then confirms the adoption.
    pub until: SystemTime,
    /// The departure that seeded the pick. Only a session signal strictly
    /// newer than it describes what replaced the departed stream — anything
    /// older predates the churn and says nothing about the new stream.
    pub floor: SystemTime,
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

/// The reconciler's memory between refreshes (departed identities, pending
/// claims). Owned here and handed to `reconcile::reconcile` explicitly; the
/// core itself holds no statics.
static RECONCILE_STATE: LazyLock<std::sync::Mutex<IdentityState>> =
    LazyLock::new(|| std::sync::Mutex::new(IdentityState::default()));

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

/// One refresh: snapshot the world, run the pure reconciler, resolve art and
/// icons for the result, publish it, and carry out the requested effects.
pub async fn update_mixer_channels(applications: Vec<AppInfo>, ignored_apps: &[String]) {
    let mut channels = MIXER_CHANNELS.lock().await;

    let apps: Vec<AppInfo> = applications
        .into_iter()
        .filter(|app| !is_ignored(app, ignored_apps))
        .collect();

    // One immutable view of the session table for this whole refresh: every
    // identity decision is a function of this snapshot, never the live table.
    let pids: HashSet<u32> = apps.iter().filter_map(|a| a.pid).collect();
    let sessions = media_session::snapshot(&pids);

    // Previous channels in slot order.
    let mut prev: Vec<(u8, MixerChannel)> = channels.drain().collect();
    prev.sort_by_key(|(slot, _)| *slot);
    let prev: Vec<MixerChannel> = prev.into_iter().map(|(_, ch)| ch).collect();

    let Effects {
        channels: mut built,
        wake_after,
        poll_players,
    } = {
        let mut state = RECONCILE_STATE.lock().unwrap();
        reconcile::reconcile(
            &mut state,
            RefreshInput {
                prev,
                apps: &apps,
                sessions: &sessions,
                now: SystemTime::now(),
            },
        )
    };

    // Art/icon pass (impure: art caches, fetch scheduling, icon-theme lookups,
    // PNG encoding). First channel wearing an identity owns its art.
    let app_by_uid: HashMap<u32, &AppInfo> = apps.iter().map(|a| (a.uid, a)).collect();
    let mut art_owned: HashSet<String> = HashSet::new();
    for ch in built.iter_mut() {
        if let Some(app) = app_by_uid.get(&ch.uid) {
            resolve_channel_art(ch, app, &mut art_owned);
        }
    }

    *channels = built
        .into_iter()
        .enumerate()
        .map(|(i, ch)| (i as u8, ch))
        .collect();
    drop(channels);

    if let Some(delay) = wake_after {
        // A pending claim only confirms during a refresh, and refreshes are
        // event-driven — schedule one just past the confirmation window so a
        // quiet system (one steady stream, no PulseAudio events) still
        // confirms on time.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                tokio::time::sleep(delay).await;
                crate::audio::pulse::pulse_monitor::request_refresh();
            });
        }
    }
    if poll_players {
        // Rate-limited internally.
        media_session::poll_players_now();
    }

    println!(
        "Updated mixer channels (filtered {} ignored apps)",
        ignored_apps.len()
    );
}

/// Resolve a channel's art from its identity (session-captured bytes or a
/// deterministic fetch), falling back to the themed/window-icon pipeline.
/// Idempotent per refresh: unchanged art (same `Arc`) skips re-encoding.
fn resolve_channel_art(
    channel: &mut MixerChannel,
    app: &AppInfo,
    art_owned: &mut HashSet<String>,
) {
    let identity_art = match (&channel.identity, channel.pid) {
        (Some(key), Some(pid)) if art_owned.insert(key.clone()) => {
            match media_session::art_for_identity(pid, key) {
                ChannelArt::Ready(bytes) => Some(bytes),
                // Pending/None: keep whatever icon is showing; the completed
                // fetch queues a refresh.
                _ => None,
            }
        }
        _ => None, // no identity, or a sibling owns it
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

    // No identity art: while the channel is still on the default icon (which
    // includes a brand-new channel, whose icon fields start empty), keep
    // re-resolving from the latest enrichment — an app's XWayland window (and
    // the themed icon it unlocks) can become resolvable a refresh or two
    // after the audio stream first appears.
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
    use crate::reconcile::test_support::bare_channel;

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
}
