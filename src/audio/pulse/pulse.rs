use crate::audio::{AppInfo, AudioSystem};
use libpulse_binding::callbacks::ListResult;
use libpulse_binding::volume::ChannelVolumes;
use pulsectl::controllers::{AppControl, DeviceControl, SinkController};
use pulsectl::controllers::types::ApplicationInfo;
use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::rc::Rc;

/// Identity recovered from a PulseAudio *client* proplist. Some apps (notably
/// pipewire-native ones) leave the sink-input proplist sparse and only populate
/// these on the owning client.
#[derive(Default, Clone)]
struct ClientProps {
    name: Option<String>,
    binary: Option<String>,
    pid: Option<u32>,
}

const PA_VOLUME_NORM: u32 = 98304; // 150% in PulseAudio

const BROWSER_KEYWORDS: &[&str] = &[
    "firefox", "chrome", "chromium", "brave", "edge", "opera", "vivaldi", "safari",
];

fn is_browser_node(name: &str) -> bool {
    let lower = name.to_lowercase();
    BROWSER_KEYWORDS.iter().any(|b| lower.contains(b))
}

/// Returns (node_name, is_browser). When node.name is absent we fall through to
/// the legacy media.name / application.name logic, same as browsers do.
fn classify_node(app: &ApplicationInfo) -> (Option<String>, bool) {
    let node_name = app.proplist.get_str("node.name");
    let is_browser = node_name.as_deref().map(is_browser_node).unwrap_or(true);
    (node_name, is_browser)
}

fn get_display_name(app: &ApplicationInfo) -> String {
    let (node_name, is_browser) = classify_node(app);
    if !is_browser && let Some(nn) = node_name {
        return nn;
    }
    app.proplist
        .get_str("media.name")
        .or_else(|| app.proplist.get_str("application.name"))
        .or(app.name.clone())
        .unwrap_or("app_stream".to_string())
}

fn get_icon_search_name(app: &ApplicationInfo) -> String {
    let (node_name, is_browser) = classify_node(app);
    if !is_browser && let Some(nn) = node_name {
        return nn.to_lowercase();
    }
    app.proplist
        .get_str("application.name")
        .or_else(|| app.proplist.get_str("application.process.binary"))
        .or(app.name.clone())
        .unwrap_or("app_stream".to_string())
        .to_lowercase()
}

/// Collapse sink-inputs that belong to the same process *and* share a display
/// name into a single entry that controls all of them. This catches an app (e.g.
/// a Wine/Proton game) that opens several indistinct streams like "audio stream
/// #1/2/3" — they all resolve to the same `node.name`, so they group together.
///
/// Streams that are meaningfully distinct keep different display names (browser
/// tabs carry their tab title via `media.name`), so they have different group
/// keys and are left untouched. Streams without a PID can't be safely attributed
/// to a process, so they pass through individually.
///
/// Browser streams NEVER collapse: Firefox tags a recreated sink-input with the
/// browser's *active* Media Session title, so two different tabs can transiently
/// share one name — merging them hands both tabs to one encoder and the "other"
/// tab vanishes from the deck (observed 2026-07-04 with a Twitch + Kick pair).
/// The mixer's collision machinery re-identifies them instead.
///
/// The merged entry keeps the lowest member uid as its stable identity, reports
/// the loudest member's volume, is muted only when every member is muted, and
/// lists all member uids so volume/mute can be applied to the whole group.
fn collapse_indistinct(streams: Vec<AppInfo>) -> Vec<AppInfo> {
    let key_of = |s: &AppInfo| {
        if s.is_browser {
            return None;
        }
        s.pid.map(|pid| (pid, s.app_name.to_lowercase()))
    };

    let mut counts: HashMap<(u32, String), usize> = HashMap::new();
    for s in &streams {
        if let Some(k) = key_of(s) {
            *counts.entry(k).or_insert(0) += 1;
        }
    }

    let mut out: Vec<AppInfo> = Vec::with_capacity(streams.len());
    let mut merged_idx: HashMap<(u32, String), usize> = HashMap::new();

    for s in streams {
        match key_of(&s) {
            Some(k) if counts[&k] > 1 => {
                if let Some(&idx) = merged_idx.get(&k) {
                    let m = &mut out[idx];
                    m.member_uids.push(s.uid);
                    m.uid = m.uid.min(s.uid); // stable identity = lowest uid
                    m.vol_percent = m.vol_percent.max(s.vol_percent); // loudest member
                    m.mute = m.mute && s.mute; // muted only if all are muted
                } else {
                    merged_idx.insert(k, out.len());
                    out.push(s);
                }
            }
            _ => out.push(s),
        }
    }

    for a in &mut out {
        a.member_uids.sort_unstable();
    }
    out
}

pub struct PulseAudioSystem {
    controller: SinkController,
}

impl PulseAudioSystem {
    pub fn new() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            controller: SinkController::create()?,
        })
    }

    /// Snapshot every PulseAudio client's identity, keyed by client index.
    /// Drives the controller's mainloop synchronously, mirroring how pulsectl's
    /// own list calls work. Returns an empty map on any failure.
    fn client_proplist_map(&mut self) -> HashMap<u32, ClientProps> {
        let map = Rc::new(RefCell::new(HashMap::new()));
        let map_ref = map.clone();

        let op = self
            .controller
            .handler
            .introspect
            .get_client_info_list(move |result| {
                if let ListResult::Item(item) = result {
                    let pl = &item.proplist;
                    map_ref.borrow_mut().insert(
                        item.index,
                        ClientProps {
                            name: pl.get_str("application.name"),
                            binary: pl.get_str("application.process.binary"),
                            pid: pl
                                .get_str("application.process.id")
                                .and_then(|s| s.parse::<u32>().ok()),
                        },
                    );
                }
            });

        if self.controller.handler.wait_for_operation(op).is_err() {
            return HashMap::new();
        }
        Rc::try_unwrap(map)
            .map(|cell| cell.into_inner())
            .unwrap_or_default()
    }
}

impl AudioSystem for PulseAudioSystem {
    fn list_applications(&mut self) -> Result<Vec<AppInfo>, Box<dyn Error>> {
        let mut res: Vec<AppInfo> = Vec::new();

        // Add individual applications first to collect all app names
        let apps = self.controller.list_applications()?;

        // Recover per-client identity (name/binary/pid) for streams whose
        // sink-input proplist is sparse (e.g. pipewire-native apps).
        let client_map = self.client_proplist_map();

        // Add the default system sink (main PC audio) only if the global flag is set
        if crate::utils::should_show_system_mixer()
            && let Ok(default_sink) = self.controller.get_default_device()
        {
            let system_name = default_sink
                .description
                .clone()
                .unwrap_or("System Audio".to_string());

            res.push(AppInfo {
                uid: default_sink.index,
                member_uids: vec![default_sink.index],
                app_name: system_name.clone(),
                app_id: system_name.clone(),
                icon_search_name: system_name,
                pid: None,
                sink_name: Some("System Audio".to_string()),
                mute: default_sink.mute,
                vol_percent: get_pulse_app_volume_percentage(&default_sink.volume),
                icon_name: Some("audio-card".to_string()),
                is_device: true,
                is_browser: false,
                is_multi_sink_app: false,
                client_pid: None,
                client_name: None,
                client_binary: None,
                wm_class: None,
                window_icon: None,
                kick_art: None,
                kick_pending: false,
            });
        }

        let streams: Vec<AppInfo> = apps.into_iter().map(|app| {
            let app_name = get_display_name(&app);
            let icon_search_name = get_icon_search_name(&app);
            let is_browser = classify_node(&app).1;

            let pid = app.proplist.get_str("application.process.id")
                .and_then(|s| s.parse::<u32>().ok());

            let client = app.client.and_then(|c| client_map.get(&c));

            // Stable app identity for the exclude list — independent of the
            // track/tab/window title that drives `app_name`.
            let app_id = app.proplist.get_str("application.name")
                .or_else(|| app.proplist.get_str("node.name"))
                .or_else(|| client.and_then(|c| c.name.clone()))
                .or_else(|| app.proplist.get_str("application.process.binary"))
                .or_else(|| app.name.clone())
                .unwrap_or_else(|| app_name.clone());

            AppInfo {
                uid: app.index,
                member_uids: vec![app.index],
                app_name,
                app_id,
                icon_search_name,
                pid,
                sink_name: app.name,
                mute: app.mute,
                vol_percent: get_pulse_app_volume_percentage(&app.volume),
                icon_name: app.proplist.get_str("application.icon_name"),
                is_device: false,
                is_browser,
                is_multi_sink_app: false, // recomputed below, after collapsing
                client_pid: client.and_then(|c| c.pid),
                client_name: client.and_then(|c| c.name.clone()),
                client_binary: client.and_then(|c| c.binary.clone()),
                wm_class: None,
                window_icon: None,
                kick_art: None,
                kick_pending: false,
            }
        }).collect();

        res.extend(collapse_indistinct(streams));

        // Flag any remaining same-named streams (e.g. two separate instances of
        // the same app on different PIDs) so the UI can fall back to per-stream
        // labels. Collapsed groups are already a single entry, so they won't trip
        // this and will keep showing their real app name.
        let mut name_counts: HashMap<String, usize> = HashMap::new();
        for a in &res {
            *name_counts.entry(a.app_name.to_lowercase()).or_insert(0) += 1;
        }
        for a in res.iter_mut().filter(|a| !a.is_device) {
            a.is_multi_sink_app =
                name_counts.get(&a.app_name.to_lowercase()).copied().unwrap_or(1) > 1;
        }

        Ok(res)
    }

    fn increase_volume(
        &mut self,
        app_index: u32,
        percent: f64,
        is_device: bool,
    ) -> Result<(), Box<dyn Error>> {
        if is_device {
            self.controller
                .increase_device_volume_by_percent(app_index, percent);
        } else {
            self.controller
                .increase_app_volume_by_percent(app_index, percent);
        }
        Ok(())
    }

    fn decrease_volume(
        &mut self,
        app_index: u32,
        percent: f64,
        is_device: bool,
    ) -> Result<(), Box<dyn Error>> {
        if is_device {
            self.controller
                .decrease_device_volume_by_percent(app_index, percent);
        } else {
            self.controller
                .decrease_app_volume_by_percent(app_index, percent);
        }
        Ok(())
    }

    fn mute_volume(
        &mut self,
        app_index: u32,
        mute: bool,
        is_device: bool,
    ) -> Result<(), Box<dyn Error>> {
        if is_device {
            self.controller.set_device_mute_by_index(app_index, mute);
        } else {
            self.controller.set_app_mute(app_index, mute)?;
        }
        Ok(())
    }
}

fn get_pulse_app_volume_percentage(channel_volumes: &ChannelVolumes) -> f32 {
    let channel_count = channel_volumes.len();
    if channel_count == 0 {
        return 0.0;
    }

    // Get average of all channels
    let total_volume: u32 = (0..channel_count)
        .map(|i| channel_volumes.get()[i as usize].0)
        .sum();

    let avg_volume = total_volume as f32 / channel_count as f32;
    let perc = (avg_volume / PA_VOLUME_NORM as f32) * 100.0;

    perc.min(100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(uid: u32, pid: Option<u32>, name: &str, vol: f32, mute: bool) -> AppInfo {
        AppInfo {
            uid,
            member_uids: vec![uid],
            app_name: name.to_string(),
            app_id: name.to_string(),
            icon_search_name: name.to_lowercase(),
            pid,
            sink_name: None,
            mute,
            vol_percent: vol,
            icon_name: None,
            is_device: false,
            is_browser: false,
            is_multi_sink_app: false,
            client_pid: None,
            client_name: None,
            client_binary: None,
            wm_class: None,
            window_icon: None,
            kick_art: None,
            kick_pending: false,
        }
    }

    #[test]
    fn collapses_same_pid_same_name_streams() {
        // A game (one PID) opening three indistinct streams + a separate browser.
        let out = collapse_indistinct(vec![
            app(101, Some(3651136), "Gothic 1 Remake", 76.0, false),
            app(102, Some(3651136), "Gothic 1 Remake", 46.0, false),
            app(103, Some(3651136), "Gothic 1 Remake", 78.0, false),
            app(200, Some(1577), "Example Stream - Twitch", 50.0, false),
        ]);

        assert_eq!(out.len(), 2, "Gothic's 3 streams collapse to 1, Firefox stays");
        let gothic = out.iter().find(|a| a.app_name == "Gothic 1 Remake").unwrap();
        assert_eq!(gothic.member_uids, vec![101, 102, 103], "controls all 3 streams");
        assert_eq!(gothic.uid, 101, "stable identity = lowest uid");
        assert_eq!(gothic.vol_percent, 78.0, "reports the loudest member");
    }

    #[test]
    fn mute_only_when_all_members_muted() {
        let all_muted = collapse_indistinct(vec![
            app(1, Some(9), "Game", 50.0, true),
            app(2, Some(9), "Game", 50.0, true),
        ]);
        assert!(all_muted[0].mute, "muted when every member is muted");

        let partial = collapse_indistinct(vec![
            app(1, Some(9), "Game", 50.0, true),
            app(2, Some(9), "Game", 50.0, false),
        ]);
        assert!(!partial[0].mute, "unmuted when any member is unmuted");
    }

    #[test]
    fn browser_streams_never_collapse_even_with_identical_names() {
        // Firefox's media.name lie can give two different tabs the same name
        // transiently; merging them hands both tabs to one encoder.
        let mut a = app(1, Some(50), "Some Stream Title", 50.0, false);
        let mut b = app(2, Some(50), "Some Stream Title", 50.0, true);
        a.is_browser = true;
        b.is_browser = true;
        let out = collapse_indistinct(vec![a, b]);
        assert_eq!(out.len(), 2, "browser tabs stay separate");
        assert!(out.iter().all(|s| s.member_uids.len() == 1));
    }

    #[test]
    fn keeps_distinct_and_pidless_streams_separate() {
        // Two browser tabs (same PID, different titles) must stay separate,
        // and a stream without a PID is never grouped.
        let out = collapse_indistinct(vec![
            app(1, Some(50), "YouTube — song A", 50.0, false),
            app(2, Some(50), "Twitch — stream B", 50.0, false),
            app(3, None, "Some App", 50.0, false),
            app(4, None, "Some App", 50.0, false),
        ]);
        assert_eq!(out.len(), 4, "distinct tabs and pid-less streams are untouched");
        assert!(out.iter().all(|a| a.member_uids.len() == 1));
    }
}
