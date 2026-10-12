//! Where a transmission's picture and sound come from, as description
//! fragments that end in raw video or raw audio.
//!
//! The PipeWire capture must target exactly the node the portal or Mutter
//! authorised (see `crate::capture`); a file replaces the capture with a
//! decoder whose audio and video branches share one `decodebin`.

use std::os::fd::RawFd;

use gstreamer as gst;

use super::init;

/// The device buffer asked of `pulsesrc`, in milliseconds.
///
/// A pipeline adopts the largest latency any source reports, and `pulsesrc`
/// reports its buffer: its 200 ms default made the sound decide how late the
/// picture arrived. Ten milliseconds is a single read period and breaks up on
/// slow machines; four periods is the usual floor for low-latency capture.
/// `BIGNETSCREEN_AUDIO_BUFFER_MS` overrides it within 5..=500.
const CAPTURE_AUDIO_BUFFER_MS: u32 = 40;

/// The ` buffer-time=` property, in the microseconds `pulsesrc` expects.
fn audio_buffer_property() -> String {
    let ms = std::env::var("BIGNETSCREEN_AUDIO_BUFFER_MS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(CAPTURE_AUDIO_BUFFER_MS)
        .clamp(5, 500);
    format!(" buffer-time={}", ms * 1_000)
}

/// The capture element's name, so its negotiated caps can be read back.
pub const CAPTURE_SOURCE_NAME: &str = "capture";

/// Pins the captured frames to square pixels.
///
/// The compositor does not say what shape its pixels are, so `pipewiresrc`
/// offers the whole range `[1/2147483647, 2147483647/1]`. Against a ranged
/// size (`VideoTarget::UpTo`) the scaler derives the output from the input
/// aspect, and fixation picks the range's minimum: `vapostproc` overflows and
/// `videoscale` settles on 2x2, ending the session in a negotiation error.
///
/// `(ANY)` keeps whatever memory the compositor offers: caps without features
/// mean system memory, and the frames arrive as DMA-BUFs. Files do not get
/// this filter, because a film's pixel shape is real.
pub const SQUARE_PIXELS: &str = "capsfilter caps=\"video/x-raw(ANY),pixel-aspect-ratio=1/1\"";

/// How long `pipewiresrc` waits before resending the last frame, in ms.
///
/// A compositor repaints only what changed, so on a still screen this is the
/// frame rate: one frame at 30 Hz keeps the encoder fed and a typed character
/// on the receiver within a frame, at the price of duplicates a P-frame codes
/// in a few bytes.
///
/// `BIGNETSCREEN_CAPTURE_KEEPALIVE_MS` overrides it within 1..=5000. A resent
/// frame carries the clock's timestamp where a real one keeps the
/// compositor's, so comparing values while typing tells a timestamp problem
/// from a compositor that sends no damage.
const CAPTURE_KEEPALIVE_MS: u32 = 33;

fn capture_keepalive_ms() -> u32 {
    std::env::var("BIGNETSCREEN_CAPTURE_KEEPALIVE_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map(|ms: u32| ms.clamp(1, 5_000))
        .unwrap_or(CAPTURE_KEEPALIVE_MS)
}

/// Where the video frames come from.
#[derive(Clone, Debug)]
pub enum VideoSource {
    /// The PipeWire stream handed over by the portal or Mutter.
    ///
    /// `fd` is the portal's PipeWire remote, which holds only the authorised
    /// node (required under Flatpak); Mutter's node lives in the session's own
    /// daemon and has no `fd`. See [`Self::description`] for how the node is
    /// targeted.
    PipeWire {
        fd: Option<RawFd>,
        /// The node ID, targeted only when the portal gave no serial.
        node_id: u32,
        /// The portal's `pipewire-serial`, which unlike a node ID is never
        /// reused for another object.
        serial: Option<u64>,
        /// The smallest buffer pool to accept from the producer.
        ///
        /// `None` leaves `pipewiresrc`'s default of one, and the producer
        /// settles anywhere in the range it offers (KWin: two to four, decided
        /// once per stream). A small pool starves the compositor, which can only
        /// paint into a buffer given back while `videorate` holds one. Set it
        /// only where the producer's range is known: asking for more than it
        /// offers fails the allocation instead of being clamped.
        min_buffers: Option<u32>,
        /// The size a virtual monitor was announced at.
        ///
        /// Not applied: forcing caps on `pipewiresrc` makes the authorised node
        /// fail negotiation (see [`Self::description`]), so a virtual monitor
        /// still takes whatever size PipeWire negotiates.
        size: Option<(u32, u32)>,
    },
    /// A media file or URL played to the receiver in place of the screen.
    ///
    /// A Miracast receiver is only a display, so the file is decoded and
    /// re-encoded here. The decoder is named `filedec` because
    /// [`AudioSource::MediaFile`] takes the sound from the same one rather than
    /// decoding the file twice.
    Media {
        source: crate::media::MediaSource,
        kind: crate::media::MediaKind,
        /// What to write across a file that has no picture of its own.
        title: String,
    },
    /// A still test pattern that needs no capture session.
    Test,
    /// A moving ball under a running clock: a frozen picture is told from a
    /// live one at a glance, and the clock against the local screen gives
    /// the delay.
    Diagnostic,
}

impl VideoSource {
    /// Is this local or online media, rather than a captured desktop?
    ///
    /// A capture arrives at the pace of what it captures; a file arrives as
    /// fast as it can be read, so it is paced and its queues never drop data.
    pub fn is_media(&self) -> bool {
        matches!(self, VideoSource::Media { .. })
    }

    /// The description fragment that produces this source's raw frames.
    ///
    /// Public so a diagnostic (`nd-capture`'s `cursor_probe` example) can build
    /// a pipeline of its own around the capture.
    pub fn description(&self) -> String {
        match self {
            VideoSource::PipeWire {
                fd,
                node_id,
                serial,
                min_buffers,
                size: _,
            } => pipewire_description(*fd, *node_id, *serial, *min_buffers),
            VideoSource::Test => "videotestsrc is-live=true".to_string(),
            VideoSource::Diagnostic => "videotestsrc is-live=true pattern=ball ! \
                 timeoverlay halignment=center valignment=center font-desc=\"Sans 48\" \
                 time-mode=running-time"
                .to_string(),
            VideoSource::Media {
                source,
                kind,
                title,
            } => media_description(source, *kind, title),
        }
    }
}

/// `pipewiresrc` on exactly the node the capture session authorised.
///
/// No caps are forced on the producer. `pipewiresrc` defaults `autoconnect` to
/// true, and when the authorised node could not satisfy forced caps it went
/// looking for another video peer and streamed the laptop's webcam. Turning
/// `autoconnect` off is not the fix either: the element then never connects.
fn pipewire_description(
    fd: Option<RawFd>,
    node_id: u32,
    serial: Option<u64>,
    min_buffers: Option<u32>,
) -> String {
    let fd = fd.map(|fd| format!("fd={fd} ")).unwrap_or_default();
    let pool = min_buffers
        .map(|n| format!(" min-buffers={n}"))
        .unwrap_or_default();
    // With a serial there is no fallback to the node ID, which may have been
    // recycled for another producer: a plugin without `target-object` fails.
    let target = match serial {
        Some(serial) => format!("target-object={serial}"),
        None => format!("path={node_id}"),
    };
    // `resend-last` repeats the last frame after `keepalive-time` of a still
    // screen, so the encoder and the receiver's clock keep moving.
    format!(
        "pipewiresrc name={CAPTURE_SOURCE_NAME} {fd}{target} \
         do-timestamp=true keepalive-time={keepalive} resend-last=true{pool} ! \
         {SQUARE_PIXELS}",
        keepalive = capture_keepalive_ms()
    )
}

/// A file's decoder and the picture branch that fits its kind.
fn media_description(
    source: &crate::media::MediaSource,
    kind: crate::media::MediaKind,
    title: &str,
) -> String {
    let decoder = match source {
        crate::media::MediaSource::File(path) => format!(
            "filesrc location={} ! decodebin name=filedec",
            quoted(&path.to_string_lossy())
        ),
        crate::media::MediaSource::Url(uri) => {
            format!("uridecodebin uri={} name=filedec", quoted(uri))
        }
    };
    match kind {
        // Sent once, a single frame shows nothing on a receiver expecting a
        // stream, so `imagefreeze` repeats it for the whole session.
        crate::media::MediaKind::Photo => {
            format!("{decoder} filedec. ! queue ! imagefreeze name=file-photo ! videoconvert")
        }
        crate::media::MediaKind::Video => format!(
            "{decoder} \
             filedec. ! queue ! identity name=file-video-sync sync=true ! videoconvert"
        ),
        // A song has no picture; the decoder is still declared for the audio
        // branch, and the screen shows what is playing.
        crate::media::MediaKind::Music => format!(
            "{decoder} \
             videotestsrc name=file-picture is-live=true pattern=black \
             ! textoverlay text={title} halignment=center valignment=center \
               font-desc=\"Sans 32\" ! videoconvert",
            title = quoted(title),
        ),
    }
}

/// Where the streamed programme's audio comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioSource {
    /// Synthesised silence.
    ///
    /// Required on WFD: the sink declares an audio and video session and
    /// discards a programme that arrives with video alone.
    Silence,
    /// System audio: an output's *monitor*, that is, what is playing through
    /// it. Which output is [`Monitor`].
    System(Monitor),
    /// The microphone alone, at the given gain (`1.0` = as captured).
    ///
    /// For narrating over what is on screen without sending the computer's own
    /// sound back through the receiver's speakers.
    Mic { volume: f64 },
    /// The computer's sound and the microphone, mixed.
    SystemAndMic { monitor: Monitor, volume: f64 },
    /// The sound of the media file being played, from the same decoder that
    /// produces its picture.
    MediaFile,
}

/// Which output's monitor "the computer's sound" means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Monitor {
    /// The default output, resolved by the sound server so it follows a device
    /// switch mid-session. Carries everything the computer plays.
    Default,
    /// The virtual card's monitor: only what an application was routed to it.
    Virtual,
}

impl Monitor {
    fn device(self) -> &'static str {
        match self {
            // Resolved by PulseAudio or PipeWire's Pulse server.
            Monitor::Default => "@DEFAULT_MONITOR@",
            Monitor::Virtual => crate::virtual_sink::MONITOR,
        }
    }

    pub fn from_settings(settings: &crate::settings::Settings) -> Self {
        if settings.virtual_audio {
            Monitor::Virtual
        } else {
            Monitor::Default
        }
    }
}

impl AudioSource {
    /// [`Self::for_settings`] with the preferences in force.
    pub fn detect() -> Self {
        Self::for_settings(&crate::settings::current())
    }

    /// The source the person asked for, or the closest thing this machine can
    /// actually produce.
    ///
    /// Silence when `pulsesrc` is missing or both switches are off: the audio
    /// branch always exists, because WFD discards a video-only programme.
    pub fn for_settings(settings: &crate::settings::Settings) -> Self {
        if init().is_err() || gst::ElementFactory::find("pulsesrc").is_none() {
            tracing::info!("no `pulsesrc`; the cast will go without audio");
            return AudioSource::Silence;
        }
        let volume = settings.mic_volume as f64 / 100.0;
        // A minimal image can lack `audiomixer`; sending the system audio
        // alone beats a pipeline that fails to build.
        let can_mix = gst::ElementFactory::find("audiomixer").is_some();
        let monitor = Monitor::from_settings(settings);
        match (settings.system_audio, settings.microphone) {
            (true, true) if can_mix => AudioSource::SystemAndMic { monitor, volume },
            (true, true) => {
                tracing::warn!("no `audiomixer`; sending system audio without the microphone");
                AudioSource::System(monitor)
            }
            (true, false) => AudioSource::System(monitor),
            (false, true) => AudioSource::Mic { volume },
            (false, false) => AudioSource::Silence,
        }
    }

    /// The description fragment that produces this source's raw audio.
    pub fn description(&self) -> String {
        match self {
            // 480 samples is 10 ms at 48 kHz, against 21 ms for the default
            // 1024; the audio branch sets the pipeline's latency floor.
            AudioSource::Silence => {
                "audiotestsrc is-live=true wave=silence samplesperbuffer=480".to_string()
            }
            // `provide-clock=false`: the capture sets the pace, and a second
            // clock would fight it. `buffer-time`: see `CAPTURE_AUDIO_BUFFER_MS`.
            AudioSource::System(monitor) => format!(
                "pulsesrc device={device} provide-clock=false do-timestamp=true{buffer}",
                device = monitor.device(),
                buffer = audio_buffer_property()
            ),
            // `@DEFAULT_SOURCE@` follows the input selected in the sound
            // settings, including a headset plugged in mid-session.
            AudioSource::Mic { volume } => format!(
                "pulsesrc device=@DEFAULT_SOURCE@ provide-clock=false do-timestamp=true{buffer} \
                 ! audioconvert ! audioresample ! volume volume={volume:.2}",
                buffer = audio_buffer_property()
            ),
            // `audiomixer` aligns the two live sources by timestamp, but they
            // must reach it at a common rate or it refuses to link.
            // `latency=20000000` (20 ms) is how long it waits for a late
            // source; with the default of 0 a microphone hiccup drops the
            // system audio with it.
            AudioSource::SystemAndMic { monitor, volume } => format!(
                "audiomixer name=micmix latency=20000000 \
                 pulsesrc device={device} provide-clock=false do-timestamp=true{buffer} \
                 ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! micmix. \
                 pulsesrc device=@DEFAULT_SOURCE@ provide-clock=false do-timestamp=true{buffer} \
                 ! audioconvert ! audioresample ! volume volume={volume:.2} \
                 ! audio/x-raw,rate=48000,channels=2 ! micmix. \
                 micmix.",
                device = monitor.device(),
                buffer = audio_buffer_property()
            ),
            // A film without sound and every photo leave the decoder's audio
            // unlinked, and a muxer would wait forever for that track; the
            // mixed-in silence keeps the branch producing.
            AudioSource::MediaFile => "audiomixer name=filemix latency=20000000 ignore-inactive-pads=true \
                 audiotestsrc name=file-silence is-live=true wave=silence samplesperbuffer=480 \
                 ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! filemix. \
                 filedec. ! queue name=file-audio-queue ! identity name=file-audio-sync sync=true ! audioconvert ! audioresample \
                 ! audio/x-raw,rate=48000,channels=2 ! filemix. \
                 filemix. ! volume name=file-volume"
                .to_string(),
        }
    }
}

/// Quotes a property value for `gst_parse_launch`.
///
/// Unquoted, a file called `My Holiday.mp4` ends the property at the space
/// and the rest is parsed as elements. Backslashes and quotes are escaped
/// first, so a file name, URL or title cannot close the string and write
/// pipeline of its own.
pub(super) fn quoted(text: &str) -> String {
    let text = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{text}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gst::prelude::*;

    #[test]
    fn still_screen_keeps_frame_rate() {
        // A compositor repaints what changed, so on a still screen the only
        // thing producing buffers is this timeout: it is the frame rate.
        const _: () = assert!(CAPTURE_KEEPALIVE_MS <= 1000 / 30);
        let description = VideoSource::PipeWire {
            fd: Some(7),
            node_id: 42,
            serial: None,
            size: None,
            min_buffers: None,
        }
        .description();
        assert!(
            description.contains("keepalive-time=33") && description.contains("resend-last=true"),
            "{description}"
        );
    }

    #[test]
    fn mutter_source_has_node_without_fd() {
        // Mutter publishes into the session's PipeWire daemon: there is no fd
        // to pass, but `path` remains mandatory.
        let desc = VideoSource::PipeWire {
            fd: None,
            node_id: 42,
            serial: None,
            size: None,
            min_buffers: None,
        }
        .description();
        assert!(desc.contains("pipewiresrc name=capture path=42"), "{desc}");
        assert!(!desc.contains("fd="), "{desc}");
    }

    #[test]
    fn virtual_card_replaces_default_monitor() {
        // The whole point of the option is what is *not* captured. A branch
        // that read both would send the private call the person switched the
        // card on to avoid, and would look perfectly healthy doing it.
        let virtual_only = AudioSource::System(Monitor::Virtual).description();
        assert!(
            virtual_only.contains(crate::virtual_sink::MONITOR),
            "{virtual_only}"
        );
        assert!(
            !virtual_only.contains("@DEFAULT_MONITOR@"),
            "the default monitor is still being captured: {virtual_only}"
        );

        // And the microphone still mixes on top, which is the answer to
        // "only what is routed to it": a person's own voice is not something
        // an application routes anywhere.
        let with_mic = AudioSource::SystemAndMic {
            monitor: Monitor::Virtual,
            volume: 0.8,
        }
        .description();
        assert!(
            with_mic.contains(crate::virtual_sink::MONITOR),
            "{with_mic}"
        );
        assert!(with_mic.contains("@DEFAULT_SOURCE@"), "{with_mic}");
        assert!(
            !with_mic.contains("@DEFAULT_MONITOR@"),
            "the default monitor leaked into the mix: {with_mic}"
        );
    }

    #[test]
    fn switch_chooses_the_monitor() {
        let off = crate::settings::Settings {
            virtual_audio: false,
            ..Default::default()
        };
        assert_eq!(Monitor::from_settings(&off), Monitor::Default);
        let on = crate::settings::Settings {
            virtual_audio: true,
            ..Default::default()
        };
        assert_eq!(Monitor::from_settings(&on), Monitor::Virtual);
    }

    #[test]
    fn buffer_floor_only_where_given() {
        let source = |min_buffers| {
            VideoSource::PipeWire {
                fd: None,
                node_id: 7,
                serial: None,
                size: None,
                min_buffers,
            }
            .description()
        };
        // Silence by default: a producer whose advertised range we have not
        // read is left to the element's own minimum, because asking for more
        // than it offers fails the allocation instead of being clamped.
        assert!(!source(None).contains("min-buffers"), "{}", source(None));
        assert!(
            source(Some(4)).contains("min-buffers=4"),
            "{}",
            source(Some(4))
        );
    }

    #[test]
    fn capture_source_has_no_forced_caps() {
        // A privacy contract. `pipewiresrc` defaults `autoconnect` to true:
        // when forced caps left the authorised node unable to negotiate, it
        // found another video peer and streamed the webcam. The portal's `fd`
        // holds only the authorised node, but the Mutter path has no `fd` and
        // shares the session daemon with the camera.
        for size in [None, Some((1920, 1080))] {
            let desc = VideoSource::PipeWire {
                fd: None,
                node_id: 42,
                serial: None,
                size,
                min_buffers: None,
            }
            .description();
            assert!(
                !desc.contains("video/x-raw,width"),
                "no caps may be forced on the producer: {desc}"
            );
            assert!(
                desc.contains("path=42"),
                "and the node must be named: {desc}"
            );
        }
    }

    #[test]
    fn hostile_file_name_stays_quoted() {
        // GStreamer's own parser is the oracle: the value must come back whole,
        // and no element named in it may be created.
        init().unwrap();
        for name in [
            "My Holiday - Côte d'Azur.mp4",
            r#"a" ! fakesink name=injected b="c.mp4"#,
            r"back\slash\.mp4",
        ] {
            let description = format!("filesrc name=src location={} ! fakesink", quoted(name));
            let pipeline = gst::parse::launch(&description)
                .unwrap()
                .downcast::<gst::Pipeline>()
                .unwrap();
            let location = pipeline
                .by_name("src")
                .unwrap()
                .property::<Option<String>>("location");
            assert_eq!(location.as_deref(), Some(name), "{description}");
            assert!(pipeline.by_name("injected").is_none(), "{description}");
        }
    }
}
