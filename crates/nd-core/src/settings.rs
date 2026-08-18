//! The preferences the person sets once and expects to hold.
//!
//! Two rules shape this module, and both come from the same place — a setting
//! that does not change anything is worse than no setting at all:
//!
//! 1. **Everything here has a consumer.** Each field is read somewhere that
//!    changes what leaves the machine: the encoder's resolution and frame rate,
//!    the audio branch, which receivers are listed, the port the receiver
//!    connects back to. Nothing is stored just because a mock-up drew a switch.
//! 2. **It is global, like [`crate::latency`].** A preference describes what
//!    the person wants of the application, not a property of one stream, and
//!    the sessions that read it are started deep inside the protocol crates.
//!
//! The file is `~/.config/bignetscreen/settings.conf`, `key = value`, one per
//! line. Hand-written rather than serde: it is a dozen scalars, the format has
//! to survive being edited by hand, and an unknown key must never stop the
//! application from starting.

use std::path::PathBuf;
use std::sync::RwLock;

/// Which protocol to prefer when a receiver offers more than one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Protocol {
    /// Whatever gives the best result for each receiver.
    #[default]
    Auto,
    /// Wi-Fi Direct only (Miracast).
    Miracast,
    /// Network receivers only (Chromecast/AirPlay discovery).
    Cast,
}

impl Protocol {
    fn as_key(self) -> &'static str {
        match self {
            Protocol::Auto => "auto",
            Protocol::Miracast => "miracast",
            Protocol::Cast => "cast",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Protocol::Auto),
            "miracast" => Some(Protocol::Miracast),
            "cast" => Some(Protocol::Cast),
            _ => None,
        }
    }
}

/// The ceiling put on the picture that is sent.
///
/// A cap, never a target: a screen smaller than the cap is sent at its own
/// size, and the receiver's own limit still applies on top of this one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quality {
    /// 3840x2160.
    ///
    /// Above what a Cast receiver is *known* to take: the protocol never
    /// declares a maximum, so this is the person saying "send what my screen
    /// has and let the receiver refuse if it must".
    Max,
    /// 2560x1440.
    Ultra,
    /// 1920x1080.
    #[default]
    High,
    /// 1280x720 — for a weak link, or a receiver that stutters at 1080p.
    Medium,
    /// 854x480.
    Low,
}

impl Quality {
    /// The resolution cap this quality stands for.
    pub fn resolution(self) -> (u32, u32) {
        match self {
            Quality::Max => (3840, 2160),
            Quality::Ultra => (2560, 1440),
            Quality::High => (1920, 1080),
            Quality::Medium => (1280, 720),
            Quality::Low => (854, 480),
        }
    }

    fn as_key(self) -> &'static str {
        match self {
            Quality::Max => "max",
            Quality::Ultra => "ultra",
            Quality::High => "high",
            Quality::Medium => "medium",
            Quality::Low => "low",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "max" => Some(Quality::Max),
            "ultra" => Some(Quality::Ultra),
            "high" => Some(Quality::High),
            "medium" => Some(Quality::Medium),
            "low" => Some(Quality::Low),
            _ => None,
        }
    }
}

/// Everything the person can change, in one place.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub protocol: Protocol,
    pub quality: Quality,
    /// Frames per second asked of the encoder. Clamped to 1..=60 on load.
    pub fps: u32,
    /// Send what the computer is playing.
    pub system_audio: bool,
    /// Mix the microphone into the audio that is sent.
    pub microphone: bool,
    /// The microphone's gain, 0..=100.
    pub mic_volume: u8,
    /// Search for receivers as soon as the application starts.
    pub auto_discovery: bool,
    /// Buffered playback ([`crate::latency::Profile::Film`]).
    pub film_mode: bool,
    /// How this computer names itself to a receiver. Empty = the host name.
    pub device_name: String,
    /// A fixed port for the receiver to connect back to, for a firewall that
    /// is managed by hand. `0` = let the system pick one.
    pub port: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            protocol: Protocol::Auto,
            quality: Quality::High,
            fps: 30,
            system_audio: true,
            microphone: false,
            mic_volume: 80,
            auto_discovery: true,
            film_mode: false,
            device_name: String::new(),
            port: 0,
        }
    }
}

impl Settings {
    /// The name to show on the receiver: what was configured, or the host name.
    pub fn display_name(&self) -> String {
        let configured = self.device_name.trim();
        if !configured.is_empty() {
            return configured.to_string();
        }
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "BigNetScreen".to_string())
    }

    fn to_file(&self) -> String {
        format!(
            "# BigNetScreen settings. Edited by the application; safe to edit by hand.\n\
             protocol = {}\n\
             quality = {}\n\
             fps = {}\n\
             system_audio = {}\n\
             microphone = {}\n\
             mic_volume = {}\n\
             auto_discovery = {}\n\
             film_mode = {}\n\
             device_name = {}\n\
             port = {}\n",
            self.protocol.as_key(),
            self.quality.as_key(),
            self.fps,
            self.system_audio,
            self.microphone,
            self.mic_volume,
            self.auto_discovery,
            self.film_mode,
            self.device_name,
            self.port,
        )
    }

    /// Reads what it recognises and keeps the default for the rest.
    ///
    /// A key it does not know, or a value it cannot parse, is ignored rather
    /// than fatal: a file from a newer version, or one edited by hand with a
    /// typo, must not stop the application from starting.
    fn from_file(text: &str) -> Self {
        let mut settings = Settings::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "protocol" => settings.protocol = Protocol::parse(value).unwrap_or_default(),
                "quality" => settings.quality = Quality::parse(value).unwrap_or_default(),
                "fps" => {
                    if let Ok(fps) = value.parse::<u32>() {
                        settings.fps = fps.clamp(1, 60);
                    }
                }
                "system_audio" => settings.system_audio = value == "true",
                "microphone" => settings.microphone = value == "true",
                // Parsed wide and then clamped, not parsed as `u8`: as a `u8`,
                // 150 would be accepted and clamped while 900 was rejected and
                // silently kept the default. Same kind of mistake, two
                // different outcomes.
                "mic_volume" => {
                    if let Ok(volume) = value.parse::<u32>() {
                        settings.mic_volume = volume.min(100) as u8;
                    }
                }
                "auto_discovery" => settings.auto_discovery = value == "true",
                "film_mode" => settings.film_mode = value == "true",
                "device_name" => settings.device_name = value.to_string(),
                "port" => {
                    if let Ok(port) = value.parse::<u16>() {
                        settings.port = port;
                    }
                }
                _ => tracing::debug!(%key, "unknown setting ignored"),
            }
        }
        settings
    }
}

static CURRENT: RwLock<Option<Settings>> = RwLock::new(None);

/// Where the settings file lives.
pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("bignetscreen").join("settings.conf"))
}

/// The settings in force, reading the file on first use.
pub fn current() -> Settings {
    if let Some(settings) = CURRENT.read().ok().and_then(|s| s.clone()) {
        return settings;
    }
    let loaded = path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|text| Settings::from_file(&text))
        .unwrap_or_default();
    // The profile lives in its own module because the pipeline reads it on
    // every session; keep the two in step from the moment the file is read.
    crate::latency::set(if loaded.film_mode {
        crate::latency::Profile::Film
    } else {
        crate::latency::Profile::Responsive
    });
    if let Ok(mut guard) = CURRENT.write() {
        *guard = Some(loaded.clone());
    }
    loaded
}

/// Applies new settings and writes them to disk.
///
/// Failing to write is reported and otherwise survivable: the session in front
/// of the person keeps the settings they just chose, and only the memory of
/// them across restarts is lost.
pub fn set(settings: Settings) {
    crate::latency::set(if settings.film_mode {
        crate::latency::Profile::Film
    } else {
        crate::latency::Profile::Responsive
    });
    if let Ok(mut guard) = CURRENT.write() {
        *guard = Some(settings.clone());
    }
    let Some(path) = path() else {
        tracing::warn!("no configuration directory; settings will not persist");
        return;
    };
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            tracing::warn!(%err, "could not create the configuration directory");
            return;
        }
    }
    if let Err(err) = std::fs::write(&path, settings.to_file()) {
        tracing::warn!(%err, path = %path.display(), "could not save the settings");
    }
}

/// Resets everything to the defaults, on disk as well.
pub fn reset() {
    set(Settings::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_written_is_what_is_read() {
        let settings = Settings {
            protocol: Protocol::Miracast,
            quality: Quality::Medium,
            fps: 60,
            system_audio: false,
            microphone: true,
            mic_volume: 42,
            auto_discovery: false,
            film_mode: true,
            device_name: "Tales' laptop".to_string(),
            port: 31789,
        };
        assert_eq!(Settings::from_file(&settings.to_file()), settings);
    }

    #[test]
    fn a_broken_file_still_starts_the_application() {
        // Everything that could come out of a hand-edited file: an unknown key,
        // a value of the wrong type, a stray line, a comment.
        let text = "# comment\n\
                    protocol = telepathy\n\
                    fps = plenty\n\
                    mic_volume = 900\n\
                    nonsense\n\
                    future_option = 7\n\
                    quality = medium\n";
        let settings = Settings::from_file(text);
        assert_eq!(settings.protocol, Protocol::Auto, "unparseable → default");
        assert_eq!(settings.fps, Settings::default().fps);
        assert_eq!(settings.mic_volume, 100, "clamped, not rejected");
        assert_eq!(settings.quality, Quality::Medium, "the valid key survives");
    }

    #[test]
    fn the_frame_rate_cannot_be_absurd() {
        // A hand-edited 240 would be asked of the encoder verbatim and produce
        // a stream no receiver accepts.
        assert_eq!(Settings::from_file("fps = 240").fps, 60);
        assert_eq!(Settings::from_file("fps = 0").fps, 1);
    }

    #[test]
    fn every_step_is_smaller_than_the_one_above_it() {
        // A "lower" quality that enlarged one side would cost bandwidth
        // instead of saving it, and a step that repeated the one above would
        // be a choice with no effect.
        let ladder = [
            Quality::Max,
            Quality::Ultra,
            Quality::High,
            Quality::Medium,
            Quality::Low,
        ];
        for pair in ladder.windows(2) {
            let (aw, ah) = pair[0].resolution();
            let (bw, bh) = pair[1].resolution();
            assert!(bw < aw && bh < ah, "{:?} vs {:?}", pair[0], pair[1]);
        }
    }

    #[test]
    fn every_quality_survives_the_file() {
        for quality in [
            Quality::Max,
            Quality::Ultra,
            Quality::High,
            Quality::Medium,
            Quality::Low,
        ] {
            let written = Settings {
                quality,
                ..Default::default()
            };
            assert_eq!(Settings::from_file(&written.to_file()).quality, quality);
        }
    }

    #[test]
    fn a_nameless_computer_still_has_something_to_show() {
        let settings = Settings {
            device_name: "   ".to_string(),
            ..Default::default()
        };
        assert!(
            !settings.display_name().is_empty(),
            "blank must fall back, never show an empty name on the receiver"
        );
        let named = Settings {
            device_name: " Studio TV ".to_string(),
            ..Default::default()
        };
        assert_eq!(named.display_name(), "Studio TV");
    }
}
