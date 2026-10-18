use openaction::*;
use openaction::global_events::{GlobalEventHandler, DidReceiveGlobalSettingsEvent, set_global_event_handler};

use serde::{Deserialize, Serialize};

use crate::{
    audio::{self, pulse::pulse_monitor::refresh_guarded, *},
    gfx::{self},
    mixer::{self, ENCODER_TO_CHANNEL_MAP},
    utils::{self, ButtonPressControl},
};
use std::{collections::HashMap, sync::LazyLock};
use tokio::sync::Mutex;

const DEFAULT_VOLUME_INCREMENT: f64 = 0.1;
const MIN_VOLUME_INCREMENT: f64 = 0.01;
const MAX_VOLUME_INCREMENT: f64 = 0.5;

fn default_volume_increment() -> f64 {
    DEFAULT_VOLUME_INCREMENT
}

fn clamp_volume_increment(v: f64) -> f64 {
    if v.is_nan() {
        DEFAULT_VOLUME_INCREMENT
    } else {
        v.clamp(MIN_VOLUME_INCREMENT, MAX_VOLUME_INCREMENT)
    }
}

pub static COLUMN_TO_CHANNEL_MAP: LazyLock<Mutex<HashMap<u8, u8>>> =
    LazyLock::new(|| Mutex::const_new(HashMap::new()));

pub static BUTTON_PRESS_CONTROL: LazyLock<Mutex<ButtonPressControl>> =
    LazyLock::new(|| Mutex::const_new(ButtonPressControl::new()));

pub static SHARED_SETTINGS: LazyLock<Mutex<VolumeControllerSettings>> =
    LazyLock::new(|| Mutex::const_new(VolumeControllerSettings::new_default()));

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct VolumeControllerSettings {
    pub show_sys_mixer: bool,
    pub ignored_apps_list: Vec<String>,
    pub volume_increment: f64,
}

impl VolumeControllerSettings {
    const fn new_default() -> Self {
        Self {
            show_sys_mixer: false,
            ignored_apps_list: Vec::new(),
            volume_increment: DEFAULT_VOLUME_INCREMENT,
        }
    }
}

impl Default for VolumeControllerSettings {
    fn default() -> Self {
        Self::new_default()
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct GlobalPluginSettings {
    pub ignored_apps_list: Vec<String>,
    #[serde(default = "default_volume_increment")]
    pub volume_increment: f64,
}

impl Default for GlobalPluginSettings {
    fn default() -> Self {
        Self {
            ignored_apps_list: Vec::new(),
            volume_increment: DEFAULT_VOLUME_INCREMENT,
        }
    }
}

pub struct GlobalHandler;

#[async_trait]
impl GlobalEventHandler for GlobalHandler {
    async fn plugin_ready(&self) -> OpenActionResult<()> {
        // Request global settings on startup so ignored_apps_list is loaded
        let _ = get_global_settings().await;
        Ok(())
    }

    async fn did_receive_global_settings(&self, event: DidReceiveGlobalSettingsEvent) -> OpenActionResult<()> {
        let global: GlobalPluginSettings = serde_json::from_value(event.payload.settings)
            .unwrap_or_default();

        let incoming_increment = clamp_volume_increment(global.volume_increment);

        println!(
            "did_receive_global_settings: {} ignored apps, volume_increment={}",
            global.ignored_apps_list.len(),
            incoming_increment
        );

        let mut shared = SHARED_SETTINGS.lock().await;
        let ignored_changed = shared.ignored_apps_list != global.ignored_apps_list;
        let increment_changed = (shared.volume_increment - incoming_increment).abs() > f64::EPSILON;

        if ignored_changed || increment_changed {
            shared.ignored_apps_list = global.ignored_apps_list.clone();
            shared.volume_increment = incoming_increment;
            let current = shared.clone();
            drop(shared);

            // Sync global-sourced fields into all instance settings
            for inst in visible_instances(VolumeControllerAction::UUID).await {
                let _ = inst.set_settings(&current).await;
            }

            if ignored_changed {
                // Guarded: this runs inside the openaction event loop — a panic
                // here would kill all deck input, not just this refresh.
                refresh_guarded("ignored-apps change").await;
            }
        }

        Ok(())
    }
}

pub struct VolumeControllerAction;

#[async_trait]
impl Action for VolumeControllerAction {
    const UUID: ActionUuid = "io.github.koconnorgit.stream-mixer.volctrl";
    type Settings = VolumeControllerSettings;

    async fn will_disappear(
        &self,
        instance: &Instance,
        _: &Self::Settings,
    ) -> OpenActionResult<()> {
        let Some(coords) = instance.coordinates else {
            println!("Warning: Instance {} has no coordinates", instance.instance_id);
            return Ok(());
        };

        if instance.controller == "Encoder" {
            utils::cleanup_encoder_dial(instance).await;
            ENCODER_TO_CHANNEL_MAP.lock().await.remove(&coords.column);
        } else {
            utils::cleanup_sd_column(instance).await;
            COLUMN_TO_CHANNEL_MAP.lock().await.remove(&coords.column);
        }

        Ok(())
    }

    async fn did_receive_settings(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        println!("did_receive_settings for instance {}: show_sys_mixer={}",
            instance.instance_id, settings.show_sys_mixer);

        // Check if show_sys_mixer changed to avoid infinite loops
        let mut cached = SHARED_SETTINGS.lock().await;
        let settings_changed = cached.show_sys_mixer != settings.show_sys_mixer;

        if settings_changed {
            println!("Settings changed, broadcasting to all instances");
            cached.show_sys_mixer = settings.show_sys_mixer;
            drop(cached);

            // Broadcast show_sys_mixer to all other instances
            for inst in visible_instances(Self::UUID).await {
                if inst.instance_id != instance.instance_id {
                    println!("Broadcasting to instance {}", inst.instance_id);
                    let _ = inst.set_settings(settings).await;
                }
            }

            // Apply show_sys_mixer setting
            utils::set_show_system_mixer(settings.show_sys_mixer);
            // Guarded: this runs inside the openaction event loop — a panic
            // here would kill all deck input, not just this refresh.
            refresh_guarded("show-sys-mixer change").await;
        } else {
            drop(cached);
            println!("Settings unchanged, skipping broadcast");
        }

        Ok(())
    }

    async fn will_appear(&self, instance: &Instance, _: &Self::Settings) -> OpenActionResult<()> {
        // Sync with shared settings when appearing
        let shared = SHARED_SETTINGS.lock().await;
        let _ = instance.set_settings(&*shared).await;
        drop(shared);

        let Some(coords) = instance.coordinates else {
            println!("Warning: Instance {} has no coordinates", instance.instance_id);
            return Ok(());
        };

        if instance.controller == "Encoder" {
            let mut encoder_map = ENCODER_TO_CHANNEL_MAP.lock().await;
            let mut channels = mixer::MIXER_CHANNELS.lock().await;
            let dial_pos = coords.column;
            let next_index = encoder_map.len() as u8;
            let channel_index = *encoder_map.entry(dial_pos).or_insert(next_index);
            match channels.get_mut(&channel_index) {
                Some(channel) => {
                    channel.dial_id = Some(instance.instance_id.clone());
                    utils::update_encoder_dial(channel, instance).await;
                }
                None => utils::cleanup_encoder_dial(instance).await,
            }
            return Ok(());
        }

        // --- Keypad ---
        let mut column_map = COLUMN_TO_CHANNEL_MAP.lock().await;
        let mut channels = mixer::MIXER_CHANNELS.lock().await;
        let sd_column = coords.column;
        let next_index = column_map.len() as u8;
        let channel_index = *column_map.entry(sd_column).or_insert(next_index);

        let channel = match channels.get_mut(&channel_index) {
            Some(ch) => ch,
            None => {
                utils::cleanup_sd_column(instance).await;
                return Ok(());
            }
        };

        match coords.row {
            0 => {
                utils::update_header(instance, channel).await;
                channel.header_id = Some(instance.instance_id.clone());
            }
            1 | 2 => {
                if let Ok((upper_img, lower_img)) =
                    gfx::get_volume_bar_data_uri_split(channel.vol_percent)
                {
                    let img;
                    if coords.row == 1 {
                        channel.upper_vol_btn_id = Some(instance.instance_id.clone());
                        img = upper_img;
                    } else {
                        channel.lower_vol_btn_id = Some(instance.instance_id.clone());
                        img = lower_img;
                    };
                    instance.set_image(Some(img), None).await?;
                }
            }
            _ => {}
        }

        Ok(())
    }

    async fn key_up(
        &self,
        instance: &Instance,
        _settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        let mut press_control = BUTTON_PRESS_CONTROL.lock().await;

        // Validate this is the correct button press
        if let Some(action_id) = press_control.action_id.as_ref() {
            if action_id != &instance.instance_id {
                drop(press_control);
                return Ok(());
            }
        }

        if let Some(duration_ms) = press_control.get_release_time() {
            println!(
                "Button {} held for {} ms",
                instance.instance_id, duration_ms
            );
            drop(press_control);

            let Some(coords) = instance.coordinates else {
                println!("Warning: Instance {} has no coordinates", instance.instance_id);
                return Ok(());
            };
            let sd_column = coords.column;

            if duration_ms > 1000 && coords.row == 0 {
                let column_map = COLUMN_TO_CHANNEL_MAP.lock().await;
                let mut channels = mixer::MIXER_CHANNELS.lock().await;

                // Look up the channel index for this SD column
                let Some(&channel_index) = column_map.get(&sd_column) else {
                    return Ok(());
                };

                if let Some(channel) = channels.get_mut(&channel_index) {
                    let app_name = channel.app_name.clone();
                    let app_id = channel.app_id.clone();
                    let members = channel.member_uids.clone();
                    let is_device = channel.is_device;

                    channel.mute = false;

                    // Drop locks before potentially blocking operations
                    drop(channels);
                    drop(column_map);

                    match audio::create() {
                        Ok(mut audio_system) => {
                            for uid in &members {
                                if let Err(e) = audio_system.mute_volume(*uid, false, is_device) {
                                    println!("Warning: Failed to unmute audio: {}", e);
                                }
                            }
                        }
                        Err(e) => println!("Warning: audio system unavailable: {}", e),
                    }

                    // Read cached shared settings, append app, and save back
                    let updated_settings = {
                        let mut shared_settings = SHARED_SETTINGS.lock().await;
                        if !shared_settings.ignored_apps_list.contains(&app_id) {
                            shared_settings.ignored_apps_list.push(app_id.clone());
                        }
                        shared_settings.clone()
                    };

                    // Save ignored apps to global settings
                    let global = GlobalPluginSettings {
                        ignored_apps_list: updated_settings.ignored_apps_list.clone(),
                        volume_increment: updated_settings.volume_increment,
                    };
                    let _ = set_global_settings(global).await;

                    // Broadcast to ALL instances (including this one)
                    for inst in visible_instances(Self::UUID).await {
                        let _ = inst.set_settings(&updated_settings).await;
                    }

                    println!("Added {} to ignored apps list and broadcast to all instances", app_name);
                }
            }
        }

        Ok(())
    }

    async fn key_down(&self, instance: &Instance, _: &Self::Settings) -> OpenActionResult<()> {
        let mut press_control = BUTTON_PRESS_CONTROL.lock().await;
        press_control.set_press_time(instance.instance_id.clone());
        drop(press_control); // Release lock early

        let Some(coords) = instance.coordinates else {
            println!("Warning: Instance {} has no coordinates", instance.instance_id);
            return Ok(());
        };

        let column_map = COLUMN_TO_CHANNEL_MAP.lock().await;
        let mut channels = mixer::MIXER_CHANNELS.lock().await;

        let sd_column = coords.column;

        // Look up the channel index for this SD column
        let Some(&channel_index) = column_map.get(&sd_column) else {
            return Ok(());
        };

        if let Some(channel) = channels.get_mut(&channel_index) {
            match coords.row {
                0 => {
                    channel.mute = !channel.mute;
                    let mute = channel.mute;
                    let is_device = channel.is_device;
                    let Ok(mut audio_system) = audio::create() else {
                        println!("Warning: audio system unavailable");
                        return Ok(());
                    };
                    for uid in &channel.member_uids {
                        if let Err(e) = audio_system.mute_volume(*uid, mute, is_device) {
                            println!("Warning: Failed to toggle mute for {}: {}", channel.app_name, e);
                        }
                    }
                    println!("Muting app {}", channel.app_name);
                }
                1 => {
                    if channel.vol_percent >= 100.0 {
                        return Ok(());
                    }

                    let is_device = channel.is_device;
                    let increment = SHARED_SETTINGS.lock().await.volume_increment;
                    let Ok(mut audio_system) = audio::create() else {
                        println!("Warning: audio system unavailable");
                        return Ok(());
                    };
                    for uid in &channel.member_uids {
                        if let Err(e) = audio_system.increase_volume(*uid, increment, is_device) {
                            println!("Warning: Failed to increase volume for {}: {}", channel.app_name, e);
                        }
                    }
                    println!("Volume up in app {} {}", channel.app_name, channel.vol_percent);
                }
                2 => {
                    let is_device = channel.is_device;
                    let increment = SHARED_SETTINGS.lock().await.volume_increment;
                    let Ok(mut audio_system) = audio::create() else {
                        println!("Warning: audio system unavailable");
                        return Ok(());
                    };
                    for uid in &channel.member_uids {
                        if let Err(e) = audio_system.decrease_volume(*uid, increment, is_device) {
                            println!("Warning: Failed to decrease volume for {}: {}", channel.app_name, e);
                        }
                    }
                    println!("Volume down in app {} {}", channel.app_name, channel.vol_percent);
                }
                _ => {}
            }
        }

        Ok(())
    }

    async fn dial_rotate(
        &self,
        instance: &Instance,
        _: &Self::Settings,
        ticks: i16,
        _pressed: bool,
    ) -> OpenActionResult<()> {
        let dial_pos = instance.coordinates.as_ref().map(|c| c.column).unwrap_or(0);
        let encoder_map = ENCODER_TO_CHANNEL_MAP.lock().await;
        let Some(&channel_index) = encoder_map.get(&dial_pos) else {
            return Ok(());
        };
        drop(encoder_map);

        let mut channels = mixer::MIXER_CHANNELS.lock().await;
        let Some(channel) = channels.get_mut(&channel_index) else {
            return Ok(());
        };

        let members = channel.member_uids.clone();
        let is_device = channel.is_device;
        drop(channels);

        let increment = SHARED_SETTINGS.lock().await.volume_increment * ticks.abs() as f64;

        let Ok(mut audio) = audio::create() else {
            println!("Warning: audio system unavailable");
            return Ok(());
        };
        for uid in &members {
            let result = if ticks > 0 {
                audio.increase_volume(*uid, increment, is_device)
            } else if ticks < 0 {
                audio.decrease_volume(*uid, increment, is_device)
            } else {
                continue;
            };
            // A failure here usually means the channel holds a stale sink-input
            // uid (refreshes stopped or lagged) — losing these errors made a
            // dead refresh loop look like broken encoders.
            if let Err(e) = result {
                println!("Warning: Failed to adjust volume for stream {}: {}", uid, e);
            }
        }

        Ok(())
    }

    async fn dial_down(&self, instance: &Instance, _: &Self::Settings) -> OpenActionResult<()> {
        let dial_pos = instance.coordinates.as_ref().map(|c| c.column).unwrap_or(0);
        let encoder_map = ENCODER_TO_CHANNEL_MAP.lock().await;
        let Some(&channel_index) = encoder_map.get(&dial_pos) else {
            return Ok(());
        };
        drop(encoder_map);

        let mut channels = mixer::MIXER_CHANNELS.lock().await;
        let Some(channel) = channels.get_mut(&channel_index) else {
            return Ok(());
        };

        channel.mute = !channel.mute;
        let members = channel.member_uids.clone();
        let mute = channel.mute;
        let is_device = channel.is_device;
        drop(channels);

        let Ok(mut audio) = audio::create() else {
            println!("Warning: audio system unavailable");
            return Ok(());
        };
        for uid in &members {
            if let Err(e) = audio.mute_volume(*uid, mute, is_device) {
                println!("Warning: Failed to set mute for stream {}: {}", uid, e);
            }
        }

        Ok(())
    }

    async fn touch_tap(
        &self,
        instance: &Instance,
        _: &Self::Settings,
        _tap_pos: (u16, u16),
        hold: bool,
    ) -> OpenActionResult<()> {
        let dial_pos = instance.coordinates.as_ref().map(|c| c.column).unwrap_or(0);
        let encoder_map = ENCODER_TO_CHANNEL_MAP.lock().await;
        let Some(&channel_index) = encoder_map.get(&dial_pos) else {
            return Ok(());
        };
        drop(encoder_map);

        if !hold {
            // Short tap: toggle mute, same as pressing the dial.
            let mut channels = mixer::MIXER_CHANNELS.lock().await;
            let Some(channel) = channels.get_mut(&channel_index) else {
                return Ok(());
            };
            channel.mute = !channel.mute;
            let members = channel.member_uids.clone();
            let mute = channel.mute;
            let is_device = channel.is_device;
            drop(channels);

            let Ok(mut audio) = audio::create() else {
                println!("Warning: audio system unavailable");
                return Ok(());
            };
            for uid in &members {
                if let Err(e) = audio.mute_volume(*uid, mute, is_device) {
                    println!("Warning: Failed to set mute for stream {}: {}", uid, e);
                }
            }
            return Ok(());
        }

        // Long press: dismiss this stream (unmute it and add it to the ignore list),
        // mirroring the long-press behaviour of the keypad mute button.
        let mut channels = mixer::MIXER_CHANNELS.lock().await;
        let Some(channel) = channels.get_mut(&channel_index) else {
            return Ok(());
        };
        let app_name = channel.app_name.clone();
        let app_id = channel.app_id.clone();
        let members = channel.member_uids.clone();
        let is_device = channel.is_device;
        channel.mute = false;
        drop(channels);

        match audio::create() {
            Ok(mut audio) => {
                for uid in &members {
                    if let Err(e) = audio.mute_volume(*uid, false, is_device) {
                        println!("Warning: Failed to unmute audio: {}", e);
                    }
                }
            }
            Err(e) => println!("Warning: audio system unavailable: {}", e),
        }

        let updated_settings = {
            let mut shared_settings = SHARED_SETTINGS.lock().await;
            if !shared_settings.ignored_apps_list.contains(&app_id) {
                shared_settings.ignored_apps_list.push(app_id.clone());
            }
            shared_settings.clone()
        };

        let global = GlobalPluginSettings {
            ignored_apps_list: updated_settings.ignored_apps_list.clone(),
            volume_increment: updated_settings.volume_increment,
        };
        let _ = set_global_settings(global).await;

        for inst in visible_instances(Self::UUID).await {
            let _ = inst.set_settings(&updated_settings).await;
        }

        println!("Touch-dismissed {} (added to ignored apps list)", app_name);
        Ok(())
    }
}

pub async fn init() -> OpenActionResult<()> {
    println!("Stream Deck connected - starting PulseAudio monitoring");

    // start listening to changes
    audio::pulse::start_pulse_monitoring();

    // Shadow-mode media-session watcher (docs/mpris-subscription-design.md
    // phase 1): learns identity/art from MPRIS signals and logs what it would
    // decide; nothing rendered depends on it yet.
    crate::media_session::start();

    // create initial map (ignored apps will be loaded via did_receive_global_settings).
    // If PulseAudio isn't ready yet, start empty and let the monitor populate us on
    // its first event rather than refusing to start the plugin at all.
    let mut applications = match create() {
        Ok(mut audio_system) => audio_system.list_applications().unwrap_or_else(|e| {
            println!("Warning: initial application list failed: {}", e);
            Vec::new()
        }),
        Err(e) => {
            println!("Warning: PulseAudio unavailable at startup: {}", e);
            Vec::new()
        }
    };


    // Attach real app icons from XWayland windows where available.
    crate::window_icons::enrich(&mut applications);


    let ignored_apps = SHARED_SETTINGS.lock().await.ignored_apps_list.clone();
    mixer::create_mixer_channels(applications, &ignored_apps).await;
    crate::scroll::sync_scroll_state().await;
    // Seed the memory buttons' blank/drawn state before any key appears.
    crate::memory::sync_with_mixer().await;

    // Start scroll animation timer for long text
    crate::scroll::start_scroll_timer();

    // Register global event handler and action
    set_global_event_handler(&GlobalHandler);
    register_action(VolumeControllerAction).await;
    register_action(crate::memory::VolumeMemoryAction).await;
    register_action(crate::memory::ClearPresetAction).await;

    run(std::env::args().collect()).await
}
