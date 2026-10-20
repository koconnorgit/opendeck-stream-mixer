//! "Volume Memory" keypad action.
//!
//! Hold stores the volume and mute state of every source currently on the
//! mixer — the store fires, and the key flashes, the moment the hold reaches
//! [`LONG_PRESS_MS`], while the key is still down. A tap recalls. Each button instance keeps its own
//! snapshot in its action settings, so any number of memory buttons can
//! coexist and a key that leaves the screen (page or profile switch) keeps
//! its preset.
//!
//! Presets never outlive the audio session they were taken in. Every
//! snapshot is stamped with the current [`EPOCH`], which changes when the
//! plugin starts (so a reboot or an OpenDeck restart forgets everything)
//! and again whenever the mixer runs out of sources. Keys on screen are
//! wiped the moment the mixer empties; a key that comes back later with a
//! snapshot from an older epoch is wiped as it appears.
//!
//! A companion "Clear Preset" action works in two steps: press it to arm
//! (it reads "Tap the button to clear it" and every stored memory button
//! turns red), then tap the memory button to erase. Pressing the clear key
//! again, or waiting [`ARM_TIMEOUT_SECS`], disarms without clearing.
//! Holding the clear key for [`CLEAR_ALL_HOLD_MS`] erases every preset on
//! the deck at once.
//!
//! Recall matches stored entries to live sources by media identity first
//! (so a Twitch tab finds its own entry even though every Firefox tab shares
//! an `app_id`), then by exact app + stream name, then by app alone (so a
//! stored "Spotify at 40%" still applies after the track changed). Sources
//! with no stored entry are left untouched; stored entries whose source is
//! gone are ignored.
//!
//! The key last stored or recalled is highlighted "loaded". That highlight
//! only means something while the mixer still matches the preset, so after
//! every mixer refresh the loaded snapshot is re-planned against the live
//! sources exactly as a recall would be, and if any covered source has moved
//! away from its stored level or mute (a dial turn, a system change, an app
//! adjusting itself) the highlight is dropped and the key goes back to
//! reading "N sources".

use openaction::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::{
    audio, gfx,
    mixer::{self, MixerChannel},
};

/// Hold at least this long to store; anything shorter recalls. Matches the
/// grid action's long-press threshold so the two feel the same.
pub const LONG_PRESS_MS: u128 = 1000;

/// How long the key stays bright after a store, so the flash is visible but
/// doesn't lag the finger lifting.
const STORE_FLASH_MS: u64 = 160;

/// Hold the clear key this long to erase every preset. Deliberately longer
/// than a store hold so it can't happen by accident.
pub const CLEAR_ALL_HOLD_MS: u64 = 3000;

/// How long the clear key reads "all cleared" after a clear-all hold.
const CLEAR_ALL_FLASH_MS: u64 = 600;

/// An armed clear key disarms itself after this long with no memory key
/// tapped, so a forgotten press can't erase a preset minutes later.
pub const ARM_TIMEOUT_SECS: u64 = 10;

/// Default key text when no label is set: "Preset N", where N is the key's
/// position among the visible memory keys in reading order.
const DEFAULT_LABEL_PREFIX: &str = "Preset";

/// A live level this far (in percent) from its stored level still counts
/// as matching the loaded preset. PulseAudio volumes are integers on a
/// 0..65536 scale, so a recalled level reads back within a hundredth of a
/// percent; anything beyond this is a real change.
pub const DRIFT_TOLERANCE_PERCENT: f32 = 0.5;

/// After a store or recall, drift checks wait this long before they can
/// drop the highlight. A refresh already in flight when the recall wrote
/// its levels can report the old values, and the writes themselves land a
/// few events later; neither is the user changing anything.
const DRIFT_GRACE_MS: u64 = 1500;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct VolumeMemorySettings {
    /// Text drawn on the button; empty means [`DEFAULT_LABEL`].
    pub label: String,
    pub snapshot: Option<MemorySnapshot>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct MemorySnapshot {
    /// Unix seconds when the snapshot was taken.
    pub saved_at: u64,
    /// The [`EPOCH`] the snapshot was taken in. Anything else is stale and
    /// gets dropped (see [`is_stale`]). Snapshots from before this field
    /// existed read as 0 and are stale too.
    pub epoch: u64,
    pub entries: Vec<MemoryEntry>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct MemoryEntry {
    pub app_id: String,
    pub app_name: String,
    /// Resolved media identity (e.g. "ttv:nmplol") when the stream had one.
    pub identity: Option<String>,
    pub is_device: bool,
    pub vol_percent: f32,
    pub mute: bool,
}

/// One volume/mute write produced by [`plan_recall`].
#[derive(Clone, Debug, PartialEq)]
pub struct RecallStep {
    pub app_name: String,
    pub member_uids: Vec<u32>,
    pub is_device: bool,
    pub vol_percent: f32,
    pub mute: bool,
}

/// Capture every channel, in slot order, as a snapshot.
pub fn snapshot_channels<'a>(
    channels: impl IntoIterator<Item = &'a MixerChannel>,
    saved_at: u64,
    epoch: u64,
) -> MemorySnapshot {
    MemorySnapshot {
        saved_at,
        epoch,
        entries: channels
            .into_iter()
            .map(|ch| MemoryEntry {
                app_id: ch.app_id.clone(),
                app_name: ch.app_name.clone(),
                identity: ch.identity.clone(),
                is_device: ch.is_device,
                vol_percent: ch.vol_percent,
                mute: ch.mute,
            })
            .collect(),
    }
}

/// Decide which stored entry each live channel recalls.
///
/// Three passes, each only over channels still unmatched:
/// 1. identity equality (consumes the entry),
/// 2. `app_id` + `app_name` equality (consumes the entry),
/// 3. `app_id` alone — prefers an unconsumed entry but will reuse one, so a
///    single stored "Firefox" level applies to every open tab.
pub fn plan_recall(snapshot: &MemorySnapshot, channels: &[MixerChannel]) -> Vec<RecallStep> {
    let entries = &snapshot.entries;
    channels
        .iter()
        .zip(assign_entries(snapshot, channels))
        .filter_map(|(ch, ei)| {
            let e = &entries[ei?];
            Some(RecallStep {
                app_name: ch.app_name.clone(),
                member_uids: ch.member_uids.clone(),
                is_device: ch.is_device,
                vol_percent: e.vol_percent,
                mute: e.mute,
            })
        })
        .collect()
}

/// One live source that no longer sits where the loaded preset put it.
#[derive(Clone, Debug, PartialEq)]
pub struct Drift {
    pub app_name: String,
    pub stored_vol: f32,
    pub live_vol: f32,
    pub stored_mute: bool,
    pub live_mute: bool,
}

/// The first live channel the snapshot covers whose volume (beyond
/// [`DRIFT_TOLERANCE_PERCENT`]) or mute differs from its stored entry, or
/// `None` when the mixer still matches the preset. Uses the same matching
/// as [`plan_recall`], so "matches" means exactly "a recall would write
/// nothing new". Sources the preset doesn't cover are ignored, as a recall
/// would leave them alone too.
pub fn find_drift(snapshot: &MemorySnapshot, channels: &[MixerChannel]) -> Option<Drift> {
    channels
        .iter()
        .zip(assign_entries(snapshot, channels))
        .find_map(|(ch, ei)| {
            let e = &snapshot.entries[ei?];
            let moved = (ch.vol_percent - e.vol_percent).abs() > DRIFT_TOLERANCE_PERCENT;
            (moved || ch.mute != e.mute).then(|| Drift {
                app_name: ch.app_name.clone(),
                stored_vol: e.vol_percent,
                live_vol: ch.vol_percent,
                stored_mute: e.mute,
                live_mute: ch.mute,
            })
        })
}

/// The matching behind [`plan_recall`] and [`find_drift`]: for each channel
/// (by position), the index of the stored entry it pairs with, if any.
fn assign_entries(snapshot: &MemorySnapshot, channels: &[MixerChannel]) -> Vec<Option<usize>> {
    let entries = &snapshot.entries;
    let mut used = vec![false; entries.len()];
    let mut assigned: Vec<Option<usize>> = vec![None; channels.len()];

    let same_kind = |e: &MemoryEntry, ch: &MixerChannel| e.is_device == ch.is_device;

    // Pass 1: identity.
    for (ci, ch) in channels.iter().enumerate() {
        let Some(id) = ch.identity.as_deref() else { continue };
        if let Some(ei) = entries.iter().enumerate().position(|(i, e)| {
            !used[i] && same_kind(e, ch) && e.identity.as_deref() == Some(id)
        }) {
            used[ei] = true;
            assigned[ci] = Some(ei);
        }
    }

    // Pass 2: exact app + name.
    for (ci, ch) in channels.iter().enumerate() {
        if assigned[ci].is_some() {
            continue;
        }
        if let Some(ei) = entries.iter().enumerate().position(|(i, e)| {
            !used[i] && same_kind(e, ch) && e.app_id == ch.app_id && e.app_name == ch.app_name
        }) {
            used[ei] = true;
            assigned[ci] = Some(ei);
        }
    }

    // Pass 3: app alone, unconsumed first, then any.
    for (ci, ch) in channels.iter().enumerate() {
        if assigned[ci].is_some() {
            continue;
        }
        let candidates = || {
            entries
                .iter()
                .enumerate()
                .filter(|(_, e)| same_kind(e, ch) && e.app_id == ch.app_id)
        };
        let pick = candidates()
            .find(|(i, _)| !used[*i])
            .or_else(|| candidates().next())
            .map(|(i, _)| i);
        if let Some(ei) = pick {
            used[ei] = true;
            assigned[ci] = Some(ei);
        }
    }

    assigned
}

/// A memory key currently held down.
#[derive(Clone, Copy)]
struct Press {
    started: Instant,
    /// Set once the hold timer has stored, so the release does nothing more.
    stored: bool,
}

/// Keys currently held, by instance id, so a release can tell a tap from a
/// hold and the hold timer can see whether its press is still live. Shared
/// by the memory and clear keys (instance ids never collide). Separate from
/// the grid action's press control so the two never confuse each other's
/// buttons.
static PRESS_STARTED: LazyLock<Mutex<HashMap<String, Press>>> =
    LazyLock::new(|| Mutex::const_new(HashMap::new()));

/// Record a fresh press for `instance_id` and return its start time.
async fn begin_press(instance_id: &str) -> Instant {
    let started = Instant::now();
    PRESS_STARTED
        .lock()
        .await
        .insert(instance_id.to_string(), Press { started, stored: false });
    started
}

/// Hold-timer handshake: if the press that began at `started` is still
/// down and unconsumed, consume it and return true; otherwise false.
async fn consume_hold(instance_id: &str, started: Instant) -> bool {
    let mut presses = PRESS_STARTED.lock().await;
    match presses.get_mut(instance_id) {
        Some(p) if p.started == started && !p.stored => {
            p.stored = true;
            true
        }
        _ => false,
    }
}

/// The current audio session: a fresh value when the plugin starts and
/// again every time the mixer empties (see [`advance_epoch`]). Unix
/// milliseconds, so no two runs of the plugin ever share one and a snapshot
/// persisted by an earlier process can never match.
static EPOCH: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(now_unix_millis()));

fn current_epoch() -> u64 {
    EPOCH.load(Ordering::Acquire)
}

/// Start a new epoch so every snapshot taken so far reads as stale. Always
/// strictly greater than the old value, even within the same millisecond.
fn advance_epoch() -> u64 {
    let next = now_unix_millis().max(current_epoch() + 1);
    EPOCH.store(next, Ordering::Release);
    next
}

/// True when `snapshot` was taken in a different epoch than `epoch`: before
/// the plugin last started, or before the mixer last ran out of sources.
pub fn is_stale(snapshot: &MemorySnapshot, epoch: u64) -> bool {
    snapshot.epoch != epoch
}

/// The memory button whose preset is currently "loaded": the last one that
/// stored or successfully recalled, for as long as the mixer still matches
/// it (see [`drop_loaded_if_drifted`]). Rendered highlighted so the deck
/// shows which preset the mixer is set to. `None` until a button is used
/// after startup, and again once the levels have moved away.
static LOADED_PRESET: LazyLock<Mutex<Option<LoadedPreset>>> =
    LazyLock::new(|| Mutex::const_new(None));

#[derive(Clone, Debug)]
struct LoadedPreset {
    instance_id: String,
    /// When the store/recall happened; drift checks hold off for
    /// [`DRIFT_GRACE_MS`] after it.
    at: Instant,
}

/// Set while a drift check inside the grace window has queued a re-check
/// for when the window closes, so one window spawns at most one timer.
static DRIFT_RECHECK_PENDING: AtomicBool = AtomicBool::new(false);

/// True when `instance_id` is the loaded preset.
fn is_loaded(loaded: &Option<LoadedPreset>, instance_id: &str) -> bool {
    loaded.as_ref().is_some_and(|l| l.instance_id == instance_id)
}

/// Forget the loaded preset if it is `instance_id`. Returns whether it was.
fn unload(loaded: &mut Option<LoadedPreset>, instance_id: &str) -> bool {
    let was = is_loaded(loaded, instance_id);
    if was {
        *loaded = None;
    }
    was
}

/// Last-known settings per visible instance. The SDK keeps settings private
/// on `Instance`, and the previously loaded button must be re-rendered (to
/// drop its highlight) without an event carrying its settings.
static SETTINGS_CACHE: LazyLock<Mutex<HashMap<String, VolumeMemorySettings>>> =
    LazyLock::new(|| Mutex::const_new(HashMap::new()));

/// True while the mixer has no sources at all. Memory buttons are blanked
/// then — there is nothing to store or recall — and redrawn when audio
/// returns. Flipped only by [`sync_with_mixer`].
static DARK: AtomicBool = AtomicBool::new(false);

/// True while a clear key is armed: the next memory key tapped is erased
/// instead of recalled or stored.
static CLEAR_ARMED: AtomicBool = AtomicBool::new(false);

/// Bumped on every arm/disarm so a stale auto-disarm timer from an earlier
/// arming can't cancel a later one.
static ARM_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Re-evaluate the keys against the mixer. Called after every refresh. A
/// blank/drawn transition redraws every key; otherwise only the loaded
/// preset is checked for drift, so steady state costs a couple of locks and
/// no image writes.
pub async fn sync_with_mixer() {
    let empty = mixer::MIXER_CHANNELS.lock().await.is_empty();
    if DARK.swap(empty, Ordering::AcqRel) == empty {
        if !empty {
            drop_loaded_if_drifted().await;
        }
        return;
    }
    if empty {
        // No sources left: the session is over, so every preset goes with
        // it. Keys not on screen right now carry the old epoch and are
        // wiped when they next appear.
        let epoch = advance_epoch();
        let cleared = erase_all_presets().await;
        println!("Volume memory: mixer went empty, cleared {cleared} stored preset(s) (epoch {epoch})");
        if CLEAR_ARMED.swap(false, Ordering::AcqRel) {
            // Nothing left to clear; the blanked keys can't show the armed state.
            ARM_GENERATION.fetch_add(1, Ordering::AcqRel);
            println!("Clear preset: disarmed (mixer went empty)");
        }
    }
    if !empty {
        drop_loaded_if_drifted().await;
    }
    refresh_memory_buttons().await;
    refresh_clear_buttons().await;
}

/// If a preset is loaded and some source it covers has moved away from the
/// stored level or mute, forget it and redraw its key without the highlight.
///
/// Inside the grace window after a store/recall a mismatch is not trusted
/// (the recall's own writes may not have been read back yet); instead one
/// full refresh is queued for when the window closes, which lands back
/// here with settled levels. That also covers a manual change made during
/// the window, which would otherwise wait for the next audio event.
async fn drop_loaded_if_drifted() {
    let (instance_id, at) = {
        let loaded = LOADED_PRESET.lock().await;
        let Some(l) = loaded.as_ref() else { return };
        (l.instance_id.clone(), l.at)
    };
    let Some(settings) = SETTINGS_CACHE.lock().await.get(&instance_id).cloned() else { return };
    let Some(snapshot) = settings.snapshot.as_ref() else { return };

    let channels = ordered_channels().await;
    let Some(drift) = find_drift(snapshot, &channels) else { return };

    let grace = Duration::from_millis(DRIFT_GRACE_MS);
    let since = at.elapsed();
    if since < grace {
        if !DRIFT_RECHECK_PENDING.swap(true, Ordering::AcqRel) {
            tokio::spawn(async move {
                tokio::time::sleep(grace - since).await;
                DRIFT_RECHECK_PENDING.store(false, Ordering::Release);
                crate::audio::pulse::pulse_monitor::request_refresh();
            });
        }
        return;
    }

    {
        let mut loaded = LOADED_PRESET.lock().await;
        // A newer store/recall may have replaced it while we were planning.
        if !loaded.as_ref().is_some_and(|l| l.instance_id == instance_id && l.at == at) {
            return;
        }
        *loaded = None;
    }
    let inst = get_instance(instance_id.clone()).await;
    let label = match &inst {
        Some(inst) => display_label(inst, &settings).await,
        None => settings.label.trim().to_string(),
    };
    println!(
        "Volume memory {instance_id}: '{label}' no longer loaded: {} is {}%{} (stored {}%{})",
        drift.app_name,
        drift.live_vol.round(),
        if drift.live_mute { " muted" } else { "" },
        drift.stored_vol.round(),
        if drift.stored_mute { " muted" } else { "" },
    );
    if let Some(inst) = inst {
        render(&inst, &settings).await;
    }
}

/// Mark `instance` as the loaded preset and un-highlight whichever button
/// held that status before.
async fn mark_loaded(instance: &Instance, settings: &VolumeMemorySettings) {
    let previous = LOADED_PRESET.lock().await.replace(LoadedPreset {
        instance_id: instance.instance_id.clone(),
        at: Instant::now(),
    });
    render(instance, settings).await;

    if let Some(prev_id) = previous.map(|l| l.instance_id).filter(|id| id != &instance.instance_id) {
        let prev_settings = SETTINGS_CACHE.lock().await.get(&prev_id).cloned();
        if let (Some(prev), Some(prev_settings)) = (get_instance(prev_id).await, prev_settings) {
            render(&prev, &prev_settings).await;
        }
    }
}

/// Redraw every memory button from its cached settings.
async fn refresh_memory_buttons() {
    let empty = DARK.load(Ordering::Acquire);
    let cache = SETTINGS_CACHE.lock().await.clone();
    for inst in visible_instances(VolumeMemoryAction::UUID).await {
        match cache.get(&inst.instance_id) {
            Some(settings) => render(&inst, settings).await,
            // Appeared before we cached anything: blank it if that's the
            // state, otherwise leave it for its own will_appear render.
            None if empty => blank(&inst).await,
            None => {}
        }
    }
}

/// Flip the armed flag and redraw every key that shows it. Returns the new
/// generation, or `None` when already in that state (nothing redrawn).
async fn apply_armed(armed: bool) -> Option<u64> {
    if CLEAR_ARMED.swap(armed, Ordering::AcqRel) == armed {
        return None;
    }
    let generation = ARM_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    refresh_memory_buttons().await;
    refresh_clear_buttons().await;
    Some(generation)
}

/// Arm or disarm the clear step. Arming starts the auto-disarm timer, which
/// only fires if nothing re-armed or disarmed in the meantime.
async fn set_armed(armed: bool) {
    let Some(generation) = apply_armed(armed).await else { return };
    if armed {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(ARM_TIMEOUT_SECS)).await;
            if ARM_GENERATION.load(Ordering::Acquire) == generation && CLEAR_ARMED.load(Ordering::Acquire) {
                println!("Clear preset: disarmed after {ARM_TIMEOUT_SECS}s with no memory key tapped");
                apply_armed(false).await;
            }
        });
    }
}

async fn blank(instance: &Instance) {
    let _ = instance
        .set_image(Some(gfx::TRANSPARENT_ICON.as_str()), None)
        .await;
}

async fn remember_settings(instance: &Instance, settings: &VolumeMemorySettings) {
    SETTINGS_CACHE
        .lock()
        .await
        .insert(instance.instance_id.clone(), settings.clone());
}

/// Take `settings` into the cache as the key's current state, first
/// dropping a snapshot from an earlier epoch (a previous run of the plugin,
/// or before the mixer last emptied): the key's settings are rewritten
/// without it so the stale preset can't be recalled or shown. Returns what
/// the key should now render.
async fn adopt_settings(instance: &Instance, settings: &VolumeMemorySettings) -> VolumeMemorySettings {
    let stale = settings
        .snapshot
        .as_ref()
        .filter(|snapshot| is_stale(snapshot, current_epoch()));
    let Some(snapshot) = stale else {
        remember_settings(instance, settings).await;
        return settings.clone();
    };
    let updated = VolumeMemorySettings {
        label: settings.label.clone(),
        snapshot: None,
    };
    match instance.set_settings(&updated).await {
        Ok(()) => println!(
            "Volume memory {}: dropped stale preset '{}' ({} sources from epoch {}, now {})",
            instance.instance_id,
            display_label(instance, &updated).await,
            snapshot.entries.len(),
            snapshot.epoch,
            current_epoch(),
        ),
        Err(e) => eprintln!(
            "Volume memory {}: failed to drop stale preset: {e}",
            instance.instance_id
        ),
    }
    remember_settings(instance, &updated).await;
    unload(&mut *LOADED_PRESET.lock().await, &instance.instance_id);
    updated
}

/// The key's text: its custom label, or "Preset N" by position.
async fn display_label(instance: &Instance, settings: &VolumeMemorySettings) -> String {
    let label = settings.label.trim();
    if !label.is_empty() {
        return label.to_string();
    }
    format!("{DEFAULT_LABEL_PREFIX} {}", preset_ordinal(instance).await)
}

/// 1-based position of `instance` among the visible memory keys, counted
/// top to bottom then left to right (reading order), per device. Keys with
/// no coordinates (inside a multi action) come last, in id order.
async fn preset_ordinal(instance: &Instance) -> usize {
    let mut keys: Vec<_> = visible_instances(VolumeMemoryAction::UUID)
        .await
        .into_iter()
        .map(|i| (preset_sort_key(&i), i.instance_id.clone()))
        .collect();
    if !keys.iter().any(|(_, id)| id == &instance.instance_id) {
        // Not listed yet (first render during will_appear on some hosts).
        keys.push((preset_sort_key(instance), instance.instance_id.clone()));
    }
    keys.sort();
    keys.iter()
        .position(|(_, id)| id == &instance.instance_id)
        .map_or(1, |p| p + 1)
}

fn preset_sort_key(instance: &Instance) -> (String, bool, u8, u8) {
    match instance.coordinates {
        Some(c) => (instance.device_id.clone(), false, c.row, c.column),
        None => (instance.device_id.clone(), true, 0, 0),
    }
}

async fn render_clear(instance: &Instance) {
    if DARK.load(Ordering::Acquire) {
        blank(instance).await;
        return;
    }
    let face = if CLEAR_ARMED.load(Ordering::Acquire) {
        gfx::ClearFace::Armed
    } else {
        gfx::ClearFace::Idle
    };
    match gfx::get_clear_button_data_uri(face) {
        Ok(uri) => {
            let _ = instance.set_image(Some(uri), None).await;
        }
        Err(e) => eprintln!("Failed to render clear-preset button image: {e}"),
    }
}

/// Redraw every clear button so they all show the same armed/idle state.
async fn refresh_clear_buttons() {
    for inst in visible_instances(ClearPresetAction::UUID).await {
        render_clear(&inst).await;
    }
}

/// A clear key was pressed: arm if idle, disarm if already armed. Arming
/// is refused (with an alert) when no memory key on the deck holds a preset.
async fn toggle_armed(button: &Instance) -> OpenActionResult<()> {
    if CLEAR_ARMED.load(Ordering::Acquire) {
        println!("Clear preset: disarmed (clear key pressed again)");
        set_armed(false).await;
        return Ok(());
    }
    let any_stored = SETTINGS_CACHE.lock().await.values().any(|s| s.snapshot.is_some());
    if !any_stored {
        println!("Clear preset: no memory key holds a preset");
        let _ = button.show_alert().await;
        return Ok(());
    }
    println!("Clear preset: armed, waiting for a memory key tap");
    set_armed(true).await;
    Ok(())
}

/// Erase every visible memory key's stored volumes (labels kept) and drop
/// the loaded highlight. Returns how many keys held a preset. Draws nothing;
/// callers redraw.
async fn erase_all_presets() -> usize {
    let cache = SETTINGS_CACHE.lock().await.clone();
    let mut cleared = 0usize;
    for inst in visible_instances(VolumeMemoryAction::UUID).await {
        let Some(settings) = cache.get(&inst.instance_id) else { continue };
        if settings.snapshot.is_none() {
            continue;
        }
        let updated = VolumeMemorySettings {
            label: settings.label.clone(),
            snapshot: None,
        };
        match inst.set_settings(&updated).await {
            Ok(()) => {
                remember_settings(&inst, &updated).await;
                cleared += 1;
            }
            Err(e) => eprintln!("Clear preset: failed to clear {}: {e}", inst.instance_id),
        }
    }
    *LOADED_PRESET.lock().await = None;
    cleared
}

/// Clear-key hold: erase every preset, disarm, and show "all cleared" on
/// `button` briefly.
async fn clear_all_presets(button: &Instance) {
    let cleared = erase_all_presets().await;
    println!("Clear preset: hold cleared all {cleared} stored preset(s)");

    if let Ok(uri) = gfx::get_clear_button_data_uri(gfx::ClearFace::AllCleared) {
        let _ = button.set_image(Some(uri), None).await;
    }
    // Redraw the memory keys now (empty, unarmed) but leave the clear key
    // on its flash face until the pause is over.
    if apply_armed(false).await.is_none() {
        refresh_memory_buttons().await;
    }
    tokio::time::sleep(Duration::from_millis(CLEAR_ALL_FLASH_MS)).await;
    refresh_clear_buttons().await;
    if cleared > 0 {
        let _ = button.show_ok().await;
    } else {
        let _ = button.show_alert().await;
    }
}

/// Clear-key hold timer: after [`CLEAR_ALL_HOLD_MS`] with the key still
/// down, clear every preset and consume the press so the release doesn't
/// also arm.
fn arm_clear_all_timer(instance_id: String, started: Instant) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(CLEAR_ALL_HOLD_MS)).await;
        if !consume_hold(&instance_id, started).await {
            return;
        }
        if let Some(button) = get_instance(instance_id).await {
            clear_all_presets(&button).await;
        }
    });
}

/// Erase this memory key's stored volumes (keeping its label), drop its
/// highlight if it was the loaded preset, and disarm.
async fn clear_preset(instance: &Instance, settings: &VolumeMemorySettings) -> OpenActionResult<()> {
    if settings.snapshot.is_none() {
        println!(
            "Clear preset: '{}' ({}) is already empty",
            display_label(instance, settings).await,
            instance.instance_id
        );
        set_armed(false).await;
        let _ = instance.show_alert().await;
        return Ok(());
    }
    let updated = VolumeMemorySettings {
        label: settings.label.clone(),
        snapshot: None,
    };
    instance.set_settings(&updated).await?;
    remember_settings(instance, &updated).await;
    unload(&mut *LOADED_PRESET.lock().await, &instance.instance_id);
    println!(
        "Clear preset: erased preset '{}' ({})",
        display_label(instance, &updated).await,
        instance.instance_id
    );
    // Disarming redraws every memory key, this one included, from the cache.
    set_armed(false).await;
    let _ = instance.show_ok().await;
    Ok(())
}

fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Live channels in slot order, cloned out from under the mixer lock.
async fn ordered_channels() -> Vec<MixerChannel> {
    let channels = mixer::MIXER_CHANNELS.lock().await;
    let mut ordered: Vec<(u8, MixerChannel)> =
        channels.iter().map(|(slot, ch)| (*slot, ch.clone())).collect();
    ordered.sort_by_key(|(slot, _)| *slot);
    ordered.into_iter().map(|(_, ch)| ch).collect()
}

async fn render(instance: &Instance, settings: &VolumeMemorySettings) {
    if DARK.load(Ordering::Acquire) {
        blank(instance).await;
        return;
    }
    let label = display_label(instance, settings).await;
    let stored = settings.snapshot.as_ref().map(|s| s.entries.len());
    let loaded = is_loaded(&*LOADED_PRESET.lock().await, &instance.instance_id);
    let armed = CLEAR_ARMED.load(Ordering::Acquire);
    match gfx::get_memory_button_data_uri(&label, stored, loaded, armed, false) {
        Ok(uri) => {
            let _ = instance.set_image(Some(uri), None).await;
        }
        Err(e) => eprintln!("Failed to render memory button image: {e}"),
    }
}

/// Briefly draw the key bright ("saved") so a hold gets instant feedback
/// the moment it has stored, then hand back to the normal render.
async fn flash_saved(instance: &Instance, settings: &VolumeMemorySettings) {
    let label = display_label(instance, settings).await;
    let stored = settings.snapshot.as_ref().map(|s| s.entries.len());
    if let Ok(uri) = gfx::get_memory_button_data_uri(&label, stored, true, false, true) {
        let _ = instance.set_image(Some(uri), None).await;
        tokio::time::sleep(Duration::from_millis(STORE_FLASH_MS)).await;
    }
}

async fn store(instance: &Instance, settings: &VolumeMemorySettings) -> OpenActionResult<()> {
    let channels = ordered_channels().await;
    let snapshot = snapshot_channels(&channels, now_unix_secs(), current_epoch());
    if snapshot.entries.is_empty() {
        println!("Volume memory {}: nothing to store (no sources)", instance.instance_id);
        let _ = instance.show_alert().await;
        return Ok(());
    }

    println!(
        "Volume memory {}: stored {} sources: {}",
        instance.instance_id,
        snapshot.entries.len(),
        snapshot
            .entries
            .iter()
            .map(|e| format!("{}={}%{}", e.app_name, e.vol_percent.round(), if e.mute { " (muted)" } else { "" }))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let updated = VolumeMemorySettings {
        label: settings.label.clone(),
        snapshot: Some(snapshot),
    };
    instance.set_settings(&updated).await?;
    remember_settings(instance, &updated).await;
    flash_saved(instance, &updated).await;
    mark_loaded(instance, &updated).await;
    let _ = instance.show_ok().await;
    Ok(())
}

/// Hold timer: once the key has been down for [`LONG_PRESS_MS`], store
/// right away (still under the finger) and flag the press so the release
/// is a no-op. Does nothing if the key was released, or re-pressed, first.
fn arm_hold_timer(instance_id: String, started: Instant, settings: VolumeMemorySettings) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(LONG_PRESS_MS as u64)).await;
        if !consume_hold(&instance_id, started).await {
            return;
        }
        let Some(instance) = get_instance(instance_id).await else { return };
        if let Err(e) = store(&instance, &settings).await {
            eprintln!("Volume memory {}: store on hold failed: {e}", instance.instance_id);
        }
    });
}

/// Write every step to PulseAudio. `Err` means no audio system at all;
/// `Ok(n)` is how many individual writes failed.
fn apply_steps(steps: &[RecallStep]) -> Result<usize, String> {
    let mut audio_system = audio::create().map_err(|e| e.to_string())?;
    let mut failures = 0usize;
    for step in steps {
        for uid in &step.member_uids {
            if let Err(e) = audio_system.set_volume(*uid, step.vol_percent, step.is_device) {
                failures += 1;
                println!("Warning: Failed to set volume for {}: {}", step.app_name, e);
            }
            if let Err(e) = audio_system.mute_volume(*uid, step.mute, step.is_device) {
                failures += 1;
                println!("Warning: Failed to set mute for {}: {}", step.app_name, e);
            }
        }
    }
    Ok(failures)
}

async fn recall(instance: &Instance, settings: &VolumeMemorySettings) -> OpenActionResult<()> {
    let Some(snapshot) = settings.snapshot.as_ref() else {
        println!("Volume memory {}: recall on an empty button", instance.instance_id);
        let _ = instance.show_alert().await;
        return Ok(());
    };

    let channels = ordered_channels().await;
    let steps = plan_recall(snapshot, &channels);
    if steps.is_empty() {
        println!(
            "Volume memory {}: none of the {} stored sources are running",
            instance.instance_id,
            snapshot.entries.len()
        );
        let _ = instance.show_alert().await;
        return Ok(());
    }

    // The PulseAudio handle is !Send, so every write happens here, before
    // the next await, and only the failure count crosses it.
    let failures = match apply_steps(&steps) {
        Ok(n) => n,
        Err(e) => {
            println!("Warning: audio system unavailable: {}", e);
            let _ = instance.show_alert().await;
            return Ok(());
        }
    };

    println!(
        "Volume memory {}: recalled {} of {} stored sources ({} live): {}",
        instance.instance_id,
        steps.len(),
        snapshot.entries.len(),
        channels.len(),
        steps
            .iter()
            .map(|s| format!("{}={}%{}", s.app_name, s.vol_percent.round(), if s.mute { " (muted)" } else { "" }))
            .collect::<Vec<_>>()
            .join(", ")
    );

    mark_loaded(instance, settings).await;
    if failures == 0 {
        let _ = instance.show_ok().await;
    } else {
        let _ = instance.show_alert().await;
    }
    Ok(())
}

pub struct VolumeMemoryAction;

#[async_trait]
impl Action for VolumeMemoryAction {
    const UUID: ActionUuid = "io.github.koconnorgit.stream-mixer.memory";
    type Settings = VolumeMemorySettings;

    async fn will_appear(&self, instance: &Instance, settings: &Self::Settings) -> OpenActionResult<()> {
        let settings = adopt_settings(instance, settings).await;
        render(instance, &settings).await;
        // A new key can shift the "Preset N" numbering of the others.
        refresh_memory_buttons().await;
        Ok(())
    }

    async fn will_disappear(&self, instance: &Instance, _: &Self::Settings) -> OpenActionResult<()> {
        PRESS_STARTED.lock().await.remove(&instance.instance_id);
        SETTINGS_CACHE.lock().await.remove(&instance.instance_id);
        refresh_memory_buttons().await;
        Ok(())
    }

    /// Label edits (and "clear") arrive from the property inspector here.
    async fn did_receive_settings(&self, instance: &Instance, settings: &Self::Settings) -> OpenActionResult<()> {
        let settings = adopt_settings(instance, settings).await;
        if settings.snapshot.is_none() {
            // A cleared preset can't be the loaded one.
            unload(&mut *LOADED_PRESET.lock().await, &instance.instance_id);
        }
        render(instance, &settings).await;
        Ok(())
    }

    async fn key_down(&self, instance: &Instance, settings: &Self::Settings) -> OpenActionResult<()> {
        let started = begin_press(&instance.instance_id).await;
        if !CLEAR_ARMED.load(Ordering::Acquire) {
            arm_hold_timer(instance.instance_id.clone(), started, settings.clone());
        }
        Ok(())
    }

    async fn key_up(&self, instance: &Instance, settings: &Self::Settings) -> OpenActionResult<()> {
        let press = PRESS_STARTED.lock().await.remove(&instance.instance_id);

        if CLEAR_ARMED.load(Ordering::Acquire) {
            // Armed by the clear key: this tap (or hold) picks what to erase.
            return clear_preset(instance, settings).await;
        }
        match press {
            // The hold timer already stored and flashed while the key was down.
            Some(Press { stored: true, .. }) => Ok(()),
            // Timer hasn't fired yet but the hold was long enough (it lost a
            // race with the release): store now rather than drop the intent.
            Some(Press { started, .. }) if started.elapsed().as_millis() >= LONG_PRESS_MS => {
                store(instance, settings).await
            }
            _ => recall(instance, settings).await,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct ClearPresetSettings {}

pub struct ClearPresetAction;

#[async_trait]
impl Action for ClearPresetAction {
    const UUID: ActionUuid = "io.github.koconnorgit.stream-mixer.memory-clear";
    type Settings = ClearPresetSettings;

    async fn will_appear(&self, instance: &Instance, _: &Self::Settings) -> OpenActionResult<()> {
        render_clear(instance).await;
        Ok(())
    }

    async fn will_disappear(&self, instance: &Instance, _: &Self::Settings) -> OpenActionResult<()> {
        PRESS_STARTED.lock().await.remove(&instance.instance_id);
        // Only disarm when the last clear key leaves; the others still show it.
        if visible_instances(ClearPresetAction::UUID).await.is_empty() {
            set_armed(false).await;
        }
        Ok(())
    }

    async fn key_down(&self, instance: &Instance, _: &Self::Settings) -> OpenActionResult<()> {
        let started = begin_press(&instance.instance_id).await;
        arm_clear_all_timer(instance.instance_id.clone(), started);
        Ok(())
    }

    /// A short press arms/disarms on release; a hold was already handled
    /// (and consumed) by the clear-all timer.
    async fn key_up(&self, instance: &Instance, _: &Self::Settings) -> OpenActionResult<()> {
        let press = PRESS_STARTED.lock().await.remove(&instance.instance_id);
        match press {
            Some(Press { stored: true, .. }) => Ok(()),
            Some(Press { started, .. }) if started.elapsed().as_millis() >= CLEAR_ALL_HOLD_MS as u128 => {
                // Timer lost the race with the release; honour the hold.
                clear_all_presets(instance).await;
                Ok(())
            }
            _ => toggle_armed(instance).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::test_support::bare_channel;

    fn ch(app_id: &str, app_name: &str, identity: Option<&str>, uid: u32) -> MixerChannel {
        let mut c = bare_channel(app_name, None, false);
        c.app_id = app_id.to_string();
        c.identity = identity.map(str::to_string);
        c.uid = uid;
        c.member_uids = vec![uid];
        c
    }

    fn entry(app_id: &str, app_name: &str, identity: Option<&str>, vol: f32, mute: bool) -> MemoryEntry {
        MemoryEntry {
            app_id: app_id.into(),
            app_name: app_name.into(),
            identity: identity.map(str::to_string),
            is_device: false,
            vol_percent: vol,
            mute,
        }
    }

    fn snap(entries: Vec<MemoryEntry>) -> MemorySnapshot {
        MemorySnapshot { saved_at: 0, epoch: 0, entries }
    }

    #[test]
    fn snapshot_captures_every_channel_in_order() {
        let mut a = ch("Spotify", "Song A", None, 1);
        a.vol_percent = 40.0;
        let mut b = ch("Firefox", "Nmplol - Twitch", Some("ttv:nmplol"), 2);
        b.mute = true;
        b.vol_percent = 75.0;
        let s = snapshot_channels([&a, &b], 123, 7);
        assert_eq!(s.saved_at, 123);
        assert_eq!(s.epoch, 7);
        assert_eq!(
            s.entries,
            vec![
                entry("Spotify", "Song A", None, 40.0, false),
                entry("Firefox", "Nmplol - Twitch", Some("ttv:nmplol"), 75.0, true),
            ]
        );
    }

    #[test]
    fn identity_beats_app_name_for_browser_tabs() {
        // Two Firefox tabs swapped names since the snapshot (Firefox's media.name
        // lie); the identity still finds the right level for each.
        let s = snap(vec![
            entry("Firefox", "Nmplol - Twitch", Some("ttv:nmplol"), 20.0, false),
            entry("Firefox", "Xqc - Twitch", Some("ttv:xqc"), 80.0, false),
        ]);
        let live = vec![
            ch("Firefox", "Xqc - Twitch", Some("ttv:nmplol"), 10),
            ch("Firefox", "Nmplol - Twitch", Some("ttv:xqc"), 11),
        ];
        let steps = plan_recall(&s, &live);
        assert_eq!(steps.len(), 2);
        assert_eq!((steps[0].member_uids[0], steps[0].vol_percent), (10, 20.0));
        assert_eq!((steps[1].member_uids[0], steps[1].vol_percent), (11, 80.0));
    }

    #[test]
    fn app_level_fallback_applies_after_track_change() {
        // Spotify's app_name is the track title; a different track still
        // gets the stored Spotify level. A never-stored app is untouched.
        let s = snap(vec![entry("Spotify", "Song A", None, 40.0, true)]);
        let live = vec![
            ch("Spotify", "Song B", None, 5),
            ch("Discord", "Discord", None, 6),
        ];
        let steps = plan_recall(&s, &live);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].member_uids, vec![5]);
        assert_eq!(steps[0].vol_percent, 40.0);
        assert!(steps[0].mute);
    }

    #[test]
    fn exact_name_match_wins_before_app_fallback() {
        let s = snap(vec![
            entry("Firefox", "Tab A", None, 30.0, false),
            entry("Firefox", "Tab B", None, 60.0, false),
        ]);
        // Live order is reversed; names must still pair up exactly.
        let live = vec![ch("Firefox", "Tab B", None, 2), ch("Firefox", "Tab A", None, 1)];
        let steps = plan_recall(&s, &live);
        assert_eq!(steps[0].vol_percent, 60.0);
        assert_eq!(steps[1].vol_percent, 30.0);
    }

    #[test]
    fn one_stored_browser_level_covers_every_open_tab() {
        let s = snap(vec![entry("Firefox", "Old tab", None, 25.0, false)]);
        let live = vec![
            ch("Firefox", "New tab 1", Some("ttv:a"), 1),
            ch("Firefox", "New tab 2", Some("ttv:b"), 2),
        ];
        let steps = plan_recall(&s, &live);
        assert_eq!(steps.len(), 2);
        assert!(steps.iter().all(|s| s.vol_percent == 25.0));
    }

    #[test]
    fn device_entries_never_match_streams_of_the_same_name() {
        let mut dev_entry = entry("System Audio", "System Audio", None, 50.0, false);
        dev_entry.is_device = true;
        let s = snap(vec![dev_entry]);
        let stream = ch("System Audio", "System Audio", None, 1);
        assert!(plan_recall(&s, &[stream]).is_empty());

        let mut dev = ch("System Audio", "System Audio", None, 9);
        dev.is_device = true;
        let steps = plan_recall(&s, &[dev]);
        assert_eq!(steps.len(), 1);
        assert!(steps[0].is_device);
    }

    #[test]
    fn collapsed_app_recalls_every_member_stream() {
        let s = snap(vec![entry("Game", "Game", None, 70.0, false)]);
        let mut game = ch("Game", "Game", None, 1);
        game.member_uids = vec![1, 2, 3];
        let steps = plan_recall(&s, &[game]);
        assert_eq!(steps[0].member_uids, vec![1, 2, 3]);
    }

    fn ch_at(app_id: &str, app_name: &str, identity: Option<&str>, uid: u32, vol: f32, mute: bool) -> MixerChannel {
        let mut c = ch(app_id, app_name, identity, uid);
        c.vol_percent = vol;
        c.mute = mute;
        c
    }

    #[test]
    fn no_drift_right_after_a_store() {
        let live = vec![
            ch_at("Spotify", "Song A", None, 1, 40.0, false),
            ch_at("Firefox", "Nmplol - Twitch", Some("ttv:nmplol"), 2, 75.0, true),
        ];
        let s = snapshot_channels(&live, 0, 0);
        assert_eq!(find_drift(&s, &live), None);
    }

    #[test]
    fn pulse_readback_rounding_is_not_drift() {
        // 40% written to PulseAudio reads back as 39.9994% (65536 steps).
        let s = snap(vec![entry("Spotify", "Song A", None, 40.0, false)]);
        let live = vec![ch_at("Spotify", "Song A", None, 1, 39.9994, false)];
        assert_eq!(find_drift(&s, &live), None);
    }

    #[test]
    fn a_turned_dial_is_drift() {
        let s = snap(vec![
            entry("Spotify", "Song A", None, 40.0, false),
            entry("Discord", "Discord", None, 80.0, false),
        ]);
        let live = vec![
            ch_at("Spotify", "Song A", None, 1, 40.0, false),
            ch_at("Discord", "Discord", None, 2, 65.0, false),
        ];
        let d = find_drift(&s, &live).expect("Discord moved");
        assert_eq!(d.app_name, "Discord");
        assert_eq!((d.stored_vol, d.live_vol), (80.0, 65.0));
    }

    #[test]
    fn a_mute_toggle_is_drift_even_at_the_same_level() {
        let s = snap(vec![entry("Spotify", "Song A", None, 40.0, false)]);
        let live = vec![ch_at("Spotify", "Song A", None, 1, 40.0, true)];
        let d = find_drift(&s, &live).expect("mute changed");
        assert!(d.live_mute && !d.stored_mute);
    }

    #[test]
    fn sources_outside_the_preset_never_drift_it() {
        // A recall would leave Discord alone, so Discord can't un-load it.
        let s = snap(vec![entry("Spotify", "Song A", None, 40.0, false)]);
        let live = vec![
            ch_at("Spotify", "Song B", None, 1, 40.0, false),
            ch_at("Discord", "Discord", None, 2, 10.0, true),
        ];
        assert_eq!(find_drift(&s, &live), None);
    }

    #[test]
    fn drift_uses_recall_matching_for_browser_tabs() {
        // Same swapped-name scenario as recall: identity decides which stored
        // level each tab is held to, so neither tab reads as drifted.
        let s = snap(vec![
            entry("Firefox", "Nmplol - Twitch", Some("ttv:nmplol"), 20.0, false),
            entry("Firefox", "Xqc - Twitch", Some("ttv:xqc"), 80.0, false),
        ]);
        let live = vec![
            ch_at("Firefox", "Xqc - Twitch", Some("ttv:nmplol"), 10, 20.0, false),
            ch_at("Firefox", "Nmplol - Twitch", Some("ttv:xqc"), 11, 80.0, false),
        ];
        assert_eq!(find_drift(&s, &live), None);
    }

    #[test]
    fn missing_sources_are_not_drift() {
        let s = snap(vec![
            entry("Spotify", "Song A", None, 40.0, false),
            entry("Discord", "Discord", None, 80.0, false),
        ]);
        let live = vec![ch_at("Spotify", "Song A", None, 1, 40.0, false)];
        assert_eq!(find_drift(&s, &live), None);
        assert_eq!(find_drift(&s, &[]), None);
    }

    #[test]
    fn settings_tolerate_missing_fields_and_old_snapshots() {
        let s: VolumeMemorySettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, VolumeMemorySettings::default());
        let s: VolumeMemorySettings =
            serde_json::from_str(r#"{"label":"Game","snapshot":{"entries":[{"app_id":"Spotify"}]}}"#)
                .unwrap();
        assert_eq!(s.label, "Game");
        let snapshot = s.snapshot.unwrap();
        assert_eq!(snapshot.entries[0].app_id, "Spotify");
        // Written before epochs existed: stale against any live epoch.
        assert_eq!(snapshot.epoch, 0);
        assert!(is_stale(&snapshot, current_epoch()));
    }

    #[test]
    fn snapshots_outlive_only_their_own_epoch() {
        let before = current_epoch();
        assert!(before > 0, "epoch is seeded from the clock at startup");
        let s = snapshot_channels(&[], 0, before);
        assert!(!is_stale(&s, before));

        let after = advance_epoch();
        assert!(after > before, "a new epoch is always strictly later");
        assert!(is_stale(&s, after));
        assert!(!is_stale(&snapshot_channels(&[], 0, after), current_epoch()));
    }
}
