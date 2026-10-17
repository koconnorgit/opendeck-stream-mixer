use std::error::Error;

#[derive(Debug)]
pub struct AppInfo {
    pub uid: u32,
    /// Every PulseAudio sink-input index this entry controls. Usually just
    /// `[uid]`, but a collapsed app (e.g. a game opening "audio stream #1/2/3")
    /// carries all of its streams here so volume/mute apply to all of them.
    pub member_uids: Vec<u32>,
    pub app_name: String,
    /// Stable, content-independent identity for the app (PulseAudio
    /// `application.name`, e.g. "Spotify"/"Firefox"). Unlike `app_name`, it does
    /// not change when the track/tab/window title changes, so the exclude list
    /// keys off this to keep an app dismissed across tracks.
    pub app_id: String,
    pub icon_search_name: String,
    pub pid: Option<u32>,
    pub sink_name: Option<String>,
    pub mute: bool,
    pub vol_percent: f32,
    pub icon_name: Option<String>,
    pub is_device: bool,
    /// True for browser sink-inputs (Firefox/Chrome/…). Browsers route every tab
    /// through one `module-stream-restore` key (`sink-input-by-application-name`),
    /// so a mute applied to one tab is replayed onto newly opened tabs. The
    /// refresh path uses this flag to unmute freshly opened tabs. See
    /// `pulse_monitor::unmute_restored_browser_streams`.
    pub is_browser: bool,
    pub is_multi_sink_app: bool,
    /// PID from the owning PulseAudio *client* proplist. For some apps (e.g.
    /// pipewire-native ones) the sink-input has no PID but the client does.
    pub client_pid: Option<u32>,
    /// `application.name` from the client proplist.
    pub client_name: Option<String>,
    /// `application.process.binary` from the client proplist — a clean icon key.
    pub client_binary: Option<String>,
    /// Normalized app-id of the matched XWayland window (lowercased, `.exe`
    /// stripped), used as a high-quality themed-icon lookup key.
    pub wm_class: Option<String>,
    /// Real icon pixels pulled from the matched window's `_NET_WM_ICON`.
    pub window_icon: Option<crate::window_icons::WindowIcon>,
}

pub trait AudioSystem {
    fn list_applications(&mut self) -> Result<Vec<AppInfo>, Box<dyn Error>>;
    fn increase_volume(
        &mut self,
        app_index: u32,
        percent: f64,
        is_device: bool,
    ) -> Result<(), Box<dyn Error>>;
    fn decrease_volume(
        &mut self,
        app_index: u32,
        percent: f64,
        is_device: bool,
    ) -> Result<(), Box<dyn Error>>;
    fn mute_volume(
        &mut self,
        app_index: u32,
        mute: bool,
        is_device: bool,
    ) -> Result<(), Box<dyn Error>>;
    /// Set an absolute volume, on the same 0-100 scale `AppInfo::vol_percent`
    /// reports, so a stored reading round-trips exactly. Every PulseAudio
    /// channel of the stream is set to the same level.
    fn set_volume(
        &mut self,
        app_index: u32,
        percent: f32,
        is_device: bool,
    ) -> Result<(), Box<dyn Error>>;
}
