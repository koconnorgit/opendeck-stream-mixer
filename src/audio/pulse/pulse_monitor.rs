use crate::{audio, mixer, utils};
use libpulse_binding::{
    context::{
        Context, FlagSet,
        subscribe::{Facility, InterestMaskSet, Operation},
    },
    mainloop::threaded::Mainloop,
    proplist::Proplist,
};
use std::collections::HashSet;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex as StdMutex;
use tokio::sync::mpsc;

static MONITOR_STARTED: AtomicBool = AtomicBool::new(false);

/// Sink-input uids present on the previous refresh. A muted browser stream whose
/// uid is *not* in here is brand new, so its mute can only have come from
/// PulseAudio's `module-stream-restore` replaying the last mute we set against the
/// per-app `sink-input-by-application-name` key. See
/// [`unmute_restored_browser_streams`].
static SEEN_STREAM_UIDS: LazyLock<StdMutex<HashSet<u32>>> =
    LazyLock::new(|| StdMutex::new(HashSet::new()));

// Global channel for refresh requests
static REFRESH_CHANNEL: LazyLock<(
    mpsc::UnboundedSender<()>,
    std::sync::Mutex<Option<mpsc::UnboundedReceiver<()>>>,
)> = LazyLock::new(|| {
    let (tx, rx) = mpsc::unbounded_channel();
    (tx, std::sync::Mutex::new(Some(rx)))
});

pub fn start_pulse_monitoring() {
    if MONITOR_STARTED.load(Ordering::Acquire) {
        return; // Already started
    }

    MONITOR_STARTED.store(true, Ordering::Release);

    // Start the refresh processor in tokio runtime
    start_refresh_processor();

    // Start PulseAudio monitoring in a regular thread
    std::thread::spawn(move || {
        println!("Starting PulseAudio monitoring...");

        // Create mainloop
        let mut mainloop = match Mainloop::new() {
            Some(m) => m,
            None => {
                eprintln!("Failed to create PulseAudio mainloop");
                return;
            }
        };

        if mainloop.start().is_err() {
            eprintln!("Failed to start PulseAudio mainloop");
            return;
        }

        // Create context
        let mut proplist = Proplist::new().unwrap();
        proplist
            .set_str("application.name", "Volume Controller")
            .unwrap();

        let mut context =
            match Context::new_with_proplist(&mainloop, "VolumeControllerMonitor", &proplist) {
                Some(c) => c,
                None => {
                    eprintln!("Failed to create PulseAudio context");
                    return;
                }
            };

        // Get the sender for refresh requests
        let refresh_sender = REFRESH_CHANNEL.0.clone();

        // Set up subscription callback
        context.set_subscribe_callback(Some(Box::new(move |facility, operation, _index| {
            match (facility, operation) {
                (Some(Facility::SinkInput), Some(Operation::New)) => {
                    println!("New audio application detected");
                    let _ = refresh_sender.send(());
                }
                (Some(Facility::SinkInput), Some(Operation::Removed)) => {
                    println!("Audio application removed");
                    let _ = refresh_sender.send(());
                }
                (Some(Facility::SinkInput), Some(Operation::Changed)) => {
                    println!("Audio application volume/mute changed");
                    let _ = refresh_sender.send(());
                }
                (Some(Facility::Sink), Some(Operation::Changed)) => {
                    println!("System sink (main PC audio) volume/mute changed");
                    let _ = refresh_sender.send(());
                }
                _ => {}
            }
        })));

        // Connect to PulseAudio
        if context.connect(None, FlagSet::NOFLAGS, None).is_err() {
            eprintln!("Failed to connect to PulseAudio");
            return;
        }

        // Wait for connection
        loop {
            match context.get_state() {
                libpulse_binding::context::State::Ready => break,
                libpulse_binding::context::State::Failed => {
                    eprintln!("PulseAudio connection failed");
                    return;
                }
                libpulse_binding::context::State::Terminated => {
                    eprintln!("PulseAudio connection terminated");
                    return;
                }
                _ => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }

        // Subscribe to sink input events and sink events
        context.subscribe(
            InterestMaskSet::SINK_INPUT | InterestMaskSet::SINK,
            |_success| {},
        );

        println!("PulseAudio monitoring started successfully");

        // Keep the context and mainloop alive
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
}

/// Unmute browser streams that came up muted only because PulseAudio's
/// `module-stream-restore` replayed a previous mute.
///
/// Every browser tab shares one restore key (`sink-input-by-application-name:Firefox`),
/// so muting one tab from the Stream Deck makes PulseAudio start every *new* tab
/// muted. We can't tell PulseAudio "don't persist this", so instead we watch for
/// brand-new muted browser streams (a uid we didn't see last refresh) and unmute
/// them. Streams we saw before are left untouched, so a tab the user actively
/// muted stays muted. The unmute also writes `mute=false` back to the restore
/// entry, so the persisted state self-heals.
fn unmute_restored_browser_streams(applications: &mut [audio::AppInfo]) {
    let to_unmute = {
        let mut seen = SEEN_STREAM_UIDS
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        select_restored_browser_unmutes(applications, &mut seen)
    };

    if to_unmute.is_empty() {
        return;
    }

    match audio::create() {
        Ok(mut audio_system) => {
            for uid in to_unmute {
                match audio_system.mute_volume(uid, false, false) {
                    Ok(()) => println!(
                        "Unmuted new browser stream {} (cleared stream-restore mute)",
                        uid
                    ),
                    Err(e) => eprintln!("Failed to unmute browser stream {}: {}", uid, e),
                }
            }
        }
        Err(e) => eprintln!("Audio system unavailable to unmute browser stream: {:?}", e),
    }
}

/// Pure half of [`unmute_restored_browser_streams`]: flip each newly seen muted
/// browser stream to unmuted, return the member uids to unmute on PulseAudio, and
/// refresh `seen` to the streams present now. Split out so it can be tested
/// without a live PulseAudio.
fn select_restored_browser_unmutes(
    applications: &mut [audio::AppInfo],
    seen: &mut HashSet<u32>,
) -> Vec<u32> {
    let mut to_unmute: Vec<u32> = Vec::new();
    for app in applications.iter_mut() {
        // Only an unseen (new) muted browser stream is a restore-inheritance
        // victim. An already-seen muted stream was muted on purpose.
        if !app.is_device
            && app.is_browser
            && app.mute
            && app.member_uids.iter().all(|u| !seen.contains(u))
        {
            to_unmute.extend(app.member_uids.iter().copied());
            app.mute = false;
        }
    }

    // Rebuild the seen-set from the streams present now: closed tabs drop out (so a
    // reused sink-input index counts as new again) and the tabs we just handled
    // count as seen next time.
    seen.clear();
    for app in applications.iter().filter(|a| !a.is_device) {
        seen.extend(app.member_uids.iter().copied());
    }

    to_unmute
}

fn start_refresh_processor() {
    // Take the receiver from the global channel
    let receiver = REFRESH_CHANNEL.1.lock().unwrap().take();

    if let Some(mut receiver) = receiver {
        tokio::spawn(async move {
            loop {
                // Wait for first refresh request
                if receiver.recv().await.is_none() {
                    break;
                }

                // Debounce: wait 100ms and drain all pending refresh requests
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Drain any additional refresh requests that came in during debounce period
                while receiver.try_recv().is_ok() {
                    // Just drain them
                }

                println!("Processing debounced refresh request...");
                if refresh_guarded("debounced").await {
                    println!("Audio applications refreshed successfully");
                    // Schedule a delayed full refresh to catch MPRIS art files
                    // and PulseAudio metadata that arrive after initial events
                    crate::mpris::schedule_delayed_refresh();
                }
            }
        });
    }
}

/// Queue a debounced full refresh, exactly as a PulseAudio event would.
/// For background completions (art fetches) that need the mixer re-rendered.
pub fn request_refresh() {
    let _ = REFRESH_CHANNEL.0.send(());
}

/// Run a full refresh on its own task so a panic anywhere in the refresh
/// pipeline is contained and logged instead of killing the caller. Refreshes
/// run inside the debounced-refresh loop and openaction event handlers; an
/// uncontained panic permanently stops all future updates (stale channels then
/// send volume writes to dead sink-input uids) or all deck input. Returns
/// whether the refresh succeeded.
pub async fn refresh_guarded(context: &'static str) -> bool {
    let result = tokio::spawn(async {
        refresh_audio_applications().await.map_err(|e| e.to_string())
    })
    .await;
    match result {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            eprintln!("Failed to refresh audio applications ({context}): {e}");
            false
        }
        Err(e) => {
            eprintln!("Refresh panicked ({context}); loop kept alive: {e}");
            false
        }
    }
}

pub async fn refresh_audio_applications() -> Result<(), Box<dyn std::error::Error>> {
    // Get current applications (same logic as manual-detection)
    let mut applications = {
        let mut audio_system = audio::create()
            .map_err(|e| format!("Audio system unavailable: {:?}", e))?;
        audio_system
            .list_applications()
            .map_err(|e| format!("Error fetching applications: {:?}", e))?
    };

    // A freshly opened browser tab inherits the last mute we applied via
    // module-stream-restore; unmute it so new tabs start audible.
    unmute_restored_browser_streams(&mut applications);

    // Recover real names for streams whose media.name is generic (e.g. Kick.com).
    crate::mpris::enrich_generic_names(&mut applications).await;

    // Attach real app icons from XWayland windows where available.
    crate::window_icons::enrich(&mut applications);

    // Kick.com streams publish no media art; fetch the streamer avatar instead.
    crate::kick::enrich_kick_art(&mut applications);

    // Get ignored apps list from shared settings
    let ignored_apps = {
        let settings = crate::plugin::SHARED_SETTINGS.lock().await;
        settings.ignored_apps_list.clone()
    };

    // Update mixers and Stream Deck buttons
    mixer::update_mixer_channels(applications, &ignored_apps).await;
    crate::scroll::sync_scroll_state().await;
    utils::update_stream_deck_buttons().await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(uid: u32, is_browser: bool, mute: bool) -> audio::AppInfo {
        audio::AppInfo {
            uid,
            member_uids: vec![uid],
            app_name: "stream".to_string(),
            app_id: "stream".to_string(),
            icon_search_name: "stream".to_string(),
            pid: None,
            sink_name: None,
            mute,
            vol_percent: 100.0,
            icon_name: None,
            is_device: false,
            is_browser,
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
    fn unmutes_new_muted_browser_stream() {
        // A tab we've never seen, muted only because module-stream-restore replayed
        // the per-app mute, gets unmuted.
        let mut seen = HashSet::new();
        let mut apps = vec![stream(7, true, true)];

        let unmute = select_restored_browser_unmutes(&mut apps, &mut seen);

        assert_eq!(unmute, vec![7]);
        assert!(!apps[0].mute, "the new tab is reported as unmuted");
        assert!(seen.contains(&7), "the handled tab is now seen");
    }

    #[test]
    fn leaves_already_seen_muted_browser_stream_alone() {
        // The tab the user actively muted (already seen) must stay muted.
        let mut seen = HashSet::from([7]);
        let mut apps = vec![stream(7, true, true)];

        let unmute = select_restored_browser_unmutes(&mut apps, &mut seen);

        assert!(unmute.is_empty());
        assert!(apps[0].mute, "the deliberately muted tab stays muted");
    }

    #[test]
    fn ignores_non_browser_and_device_streams() {
        // A muted non-browser app (e.g. a game) and a muted device are never touched
        // — only browsers suffer the shared-restore-key inheritance.
        let mut seen = HashSet::new();
        let mut device = stream(2, false, true);
        device.is_device = true;
        let mut apps = vec![stream(1, false, true), device];

        let unmute = select_restored_browser_unmutes(&mut apps, &mut seen);

        assert!(unmute.is_empty());
        assert!(apps[0].mute && apps[1].mute, "neither is unmuted");
    }

    #[test]
    fn new_then_seen_across_refreshes() {
        // First refresh: a new muted tab is unmuted. Second refresh with the same
        // (now unmuted) tab leaves it alone, and a user-mute that lands after is kept.
        let mut seen = HashSet::new();

        let mut first = vec![stream(7, true, true)];
        assert_eq!(select_restored_browser_unmutes(&mut first, &mut seen), vec![7]);

        // Same tab returns, now muted by the user via the Stream Deck.
        let mut second = vec![stream(7, true, true)];
        let unmute = select_restored_browser_unmutes(&mut second, &mut seen);
        assert!(unmute.is_empty());
        assert!(second[0].mute, "a seen tab the user muted is preserved");
    }

    #[test]
    fn closed_tab_drops_from_seen_so_reused_uid_counts_as_new() {
        // uid 7 seen, then the tab closes (absent from the next refresh). When uid 7
        // is later reused by a brand-new muted tab, it must be treated as new again.
        let mut seen = HashSet::new();
        let mut present = vec![stream(7, true, false)];
        select_restored_browser_unmutes(&mut present, &mut seen);
        assert!(seen.contains(&7));

        // Refresh with no streams: uid 7 drops out of the seen-set.
        let mut empty: Vec<audio::AppInfo> = vec![];
        select_restored_browser_unmutes(&mut empty, &mut seen);
        assert!(!seen.contains(&7));

        // uid 7 reused by a new muted tab → unmuted.
        let mut reused = vec![stream(7, true, true)];
        assert_eq!(select_restored_browser_unmutes(&mut reused, &mut seen), vec![7]);
    }
}
