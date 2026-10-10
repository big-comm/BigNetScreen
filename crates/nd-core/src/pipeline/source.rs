//! Where a transmission's picture and sound come from, as description
//! fragments that end in raw video or raw audio.
//!
//! The PipeWire capture must target exactly the node the portal or Mutter
//! authorised (see `crate::capture`); a file replaces the capture with a
//! decoder whose audio and video branches share one `decodebin`.

use std::os::fd::RawFd;

use gstreamer as gst;

use super::init;

/// The device buffer asked of `pulsesrc`, in microseconds.
///
/// The element's own default is 200 ms, documented as "the maximum latency
/// that the source reports". A pipeline adopts the largest latency any of its
/// sources reports and the screen capture reports almost none, so that default
/// made the audio branch decide how late the picture was — reported from use
/// as the screen arriving noticeably sooner with audio switched off, and
/// confirmed by asking for less and watching the delay go.
///
/// Forty milliseconds, not the ten that was measured working. Ten is one read
/// period — `latency-time` asks for ten and gets one buffer's worth of room,
/// which is no headroom at all, and this already logs an overrun at startup
/// with two hundred. It held on one fast machine; this ships to slow ones,
/// where the cost is not latency but audio breaking up. Four periods is the
/// usual floor for low-latency capture and still cuts the old default by five.
///
/// `BIGNETSCREEN_AUDIO_BUFFER_MS` overrides it either way.
const CAPTURE_AUDIO_BUFFER_MS: u32 = 40;

fn capture_audio_buffer_us() -> u32 {
    std::env::var("BIGNETSCREEN_AUDIO_BUFFER_MS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(CAPTURE_AUDIO_BUFFER_MS)
        .clamp(5, 500)
        * 1_000
}

/// The `buffer-time=` property as `pulsesrc` spells it.
fn audio_buffer_property() -> String {
    format!(" buffer-time={}", capture_audio_buffer_us())
}

/// The capture element's name, so its negotiated caps can be read back.
pub const CAPTURE_SOURCE_NAME: &str = "capture";

/// Pins the captured frames to square pixels.
///
/// The compositor does not say what shape its pixels are, so `pipewiresrc`
/// offers the whole range, `[1/2147483647, 2147483647/1]`. An exact target
/// never noticed: its caps carry `pixel-aspect-ratio=1/1` and the scaler
/// works from the dimensions. A *range* of sizes ([`VideoTarget::UpTo`]) is
/// different — the scaler derives the output from the input aspect, and
/// fixation had picked the range's minimum: `vapostproc` overflowed ("Error
/// calculating the output scaled size") and `videoscale` settled on 2x2, so
/// the DLNA and Cast mirroring sessions ended in "negotiation problem" three
/// seconds after they started (measured on GNOME 49, through the portal and
/// the direct Mutter backend alike).
///
/// A plain `video/x-raw` filter would not do: caps without features mean
/// system memory, and the frames arrive as DMA-BUFs. `(ANY)` keeps whatever
/// memory the compositor offers and only settles the ratio. Not applied to
/// files: a film's pixel shape is real and [`VideoTarget::Exact`] handles it.
pub const SQUARE_PIXELS: &str = "capsfilter caps=\"video/x-raw(ANY),pixel-aspect-ratio=1/1\"";

/// How long `pipewiresrc` waits before resending the last frame, in ms.
///
/// One frame at 30 Hz, two at 60. It is the frame rate a still screen gets,
/// because nothing else produces buffers when the compositor has nothing to
/// repaint — at the 1000 ms it used to be, typing in a terminal updated the
/// picture about once a second.
///
/// `BIGNETSCREEN_CAPTURE_KEEPALIVE_MS` overrides it, and exists for an open
/// question rather than for tuning. A resent frame is not a copy of the real
/// one: `gstpipewiresrc` rewrites its timestamp to the clock, while a real
/// frame keeps the one the compositor gave it. If those two disagree, a real
/// frame arriving between resends can look older than what already went out —
/// which would show up as exactly the reported symptom, a keystroke taking far
/// too long to appear while a moving pointer stays fluid. Comparing 33 against
/// a much larger value while typing is what tells that apart from the
/// compositor simply not sending the damage.
fn capture_keepalive_ms() -> u32 {
    std::env::var("BIGNETSCREEN_CAPTURE_KEEPALIVE_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map(|ms: u32| ms.clamp(1, 5_000))
        .unwrap_or(CAPTURE_KEEPALIVE_MS)
}

const CAPTURE_KEEPALIVE_MS: u32 = 33;

/// Where the video frames come from.
// Not `Copy`: a media file carries its path. Cloning a source is rare (once
// per session) and cheap next to what the session then does.
#[derive(Clone, Debug)]
pub enum VideoSource {
    /// The PipeWire stream handed over by the portal/Mutter.
    ///
    /// `node_id` may **never** be missing: a `pipewiresrc` without `path`
    /// picks an arbitrary node from the daemon instead of the one the capture
    /// session authorised. `fd` is the portal's remote descriptor (mandatory
    /// under Flatpak); with Mutter directly the node lives in the session's own
    /// daemon and `fd` is `None`.
    PipeWire {
        fd: Option<RawFd>,
        node_id: u32,
        /// Prefer the non-reusable portal serial over the legacy node ID.
        serial: Option<u64>,
        /// The smallest buffer pool to accept from the producer.
        ///
        /// `None` leaves `pipewiresrc`'s own default of one, which lets the
        /// producer settle anywhere in the range it offers. KWin offers two to
        /// four and prefers three (`screencaststream.cpp`), and where it lands
        /// is decided once per stream and kept — which is why a new virtual
        /// screen came out fluid or stuttering at random and stayed that way,
        /// with no load to blame: measured at 9 frames a second on a bad one
        /// and 36 on a good one, with the compositor at 7% of a core both
        /// times.
        ///
        /// A small pool starves the compositor: it can only paint into a
        /// buffer we have given back, and this pipeline holds one in
        /// `videorate` by design. With two, that is half the pool.
        ///
        /// Only set where the producer's range is known from its source.
        /// Asking for more than it offers is not clamped — it fails the
        /// allocation outright, and a capture that will not start is worse
        /// than one that sometimes stutters.
        min_buffers: Option<u32>,
        /// The size to demand from the producer, when it must not be left open.
        ///
        /// A **virtual monitor** has no panel to take a resolution from, so
        /// Mutter leaves it to PipeWire to negotiate one — and creates the
        /// screen at whatever comes out of that negotiation. With the size only
        /// fixed downstream of the scaler, `pipewiresrc` accepted anything and
        /// the monitor was created at **16x16**, then stretched to fill the
        /// receiver. That is what a blurry picture looks like from here.
        ///
        /// `None` for a real monitor: its resolution is its own.
        size: Option<(u32, u32)>,
    },
    /// A media file played **to** the receiver.
    ///
    /// Miracast has no notion of "play this file": the receiver is a screen and
    /// nothing else. So the file is decoded here and takes the place of the
    /// screen capture — full screen on the receiver, without the desktop
    /// showing. It costs a re-encode, which is the price of a protocol that
    /// only knows how to be a display.
    ///
    /// The element is named `filedec` because [`AudioSource::MediaFile`] takes
    /// the sound from the **same** decoder: decoding a film twice to split its
    /// two tracks would double the cost of the thing that is already the most
    /// expensive part.
    Media {
        source: crate::media::MediaSource,
        kind: crate::media::MediaKind,
        /// What to write across a file that has no picture of its own.
        title: String,
    },
    /// A test pattern (diagnostic, needing no capture session).
    Test,
    /// An **animated** pattern with a clock overlaid.
    ///
    /// Colour bars are static: on a remote screen there is no way to tell "live
    /// picture" from "frozen picture". A moving ball plus a running clock
    /// settle that at a glance — and the clock also allows measuring the delay
    /// by comparing against the local screen.
    Diagnostic,
}

impl VideoSource {
    /// Is this local or online media, rather than a captured desktop?
    ///
    /// The distinction decides pacing: a capture arrives at the pace of the
    /// thing it captures, and a file arrives as fast as it can be read.
    pub fn is_media(&self) -> bool {
        matches!(self, VideoSource::Media { .. })
    }

    /// The GStreamer fragment that produces this source's frames.
    ///
    /// Public so diagnostics can build a pipeline of their own — the
    /// `cursor_probe` example needs raw video, where the production pipelines
    /// all end in an encoder.
    pub fn description(&self) -> String {
        match self {
            // `keepalive-time`/`resend-last` make the source re-emit the last
            // frame when the screen is still. Without them the encoder starves
            // and the receiver drops the session for lack of data.
            VideoSource::PipeWire {
                fd,
                node_id,
                serial,
                size,
                min_buffers,
            } => {
                let pool = match min_buffers {
                    Some(n) => format!(" min-buffers={n}"),
                    None => String::new(),
                };
                let fd_prop = match fd {
                    Some(fd) => format!("fd={fd} "),
                    None => String::new(),
                };
                // No caps are forced on the producer, and that is deliberate.
                //
                // Demanding a size here looked like the way to stop Mutter
                // creating a 16x16 virtual monitor. What it actually did was
                // make the authorised node unable to satisfy the negotiation —
                // and `pipewiresrc`, whose `autoconnect` defaults to true,
                // then went looking for another video peer and found the
                // laptop's **webcam**, which went out to the projector.
                //
                // Turning `autoconnect` off is not the fix either: with it off
                // the element never connects at all (measured: zero frames,
                // the monitor is never created). So the size stays negotiated,
                // and sizing the virtual monitor properly is still open.
                let _ = size;
                // Do not fall back to a node ID when a serial was supplied:
                // a recycled ID could point at a different producer. A plugin
                // without target-object must fail, not capture another node.
                let target = match serial {
                    Some(serial) => format!("target-object={serial}"),
                    None => format!("path={node_id}"),
                };
                // `keepalive-time` is the floor on the capture's frame rate: a
                // compositor only repaints what changed, so when the screen is
                // still this is the only thing producing buffers at all.
                //
                // It used to be 1000 ms, which is a floor of one frame per
                // second — measured, and visible as exactly that: typing in a
                // terminal changes a character cell, the compositor sends
                // almost nothing, and the picture updated about once a second.
                // One frame interval keeps the encoder fed and the receiver's
                // clock moving, at the price of duplicate frames that a P-frame
                // codes in a handful of bytes. The encoder was already emitting
                // 60 fps against 13 arriving, so the duplicates are not new —
                // only their timing is, and regular beats bursty.
                format!(
                    "pipewiresrc name={CAPTURE_SOURCE_NAME} {fd_prop}{target} \
                     do-timestamp=true keepalive-time={keepalive} resend-last=true{pool} ! \
                     {SQUARE_PIXELS}",
                    keepalive = capture_keepalive_ms()
                )
            }
            VideoSource::Test => "videotestsrc is-live=true".to_string(),
            VideoSource::Diagnostic => "videotestsrc is-live=true pattern=ball ! \
                 timeoverlay halignment=center valignment=center font-desc=\"Sans 48\" \
                 time-mode=running-time"
                .to_string(),
            VideoSource::Media {
                source,
                kind,
                title,
            } => {
                let decoder = match source {
                    crate::media::MediaSource::File(path) => format!(
                        "filesrc location={} ! decodebin name=filedec",
                        escape_location(path)
                    ),
                    crate::media::MediaSource::Url(uri) => format!(
                        "uridecodebin uri={} name=filedec",
                        escape_location(std::path::Path::new(uri))
                    ),
                };
                match kind {
                    // A photo is one frame. Sent once, a receiver expecting a
                    // video stream shows nothing at all, so `imagefreeze`
                    // repeats it for as long as the session lasts.
                    crate::media::MediaKind::Photo => format!(
                        "{decoder} \
                         filedec. ! queue ! imagefreeze name=file-photo ! videoconvert"
                    ),
                    crate::media::MediaKind::Video => format!(
                        "{decoder} \
                         filedec. ! queue ! identity name=file-video-sync sync=true ! videoconvert"
                    ),
                    // A song has no picture. The decoder is still declared —
                    // the audio branch takes its sound from it — and the screen
                    // gets the one thing worth showing: what is playing.
                    crate::media::MediaKind::Music => format!(
                        "{decoder} \
                         videotestsrc name=file-picture is-live=true pattern=black \
                         ! textoverlay text={title} halignment=center valignment=center \
                           font-desc=\"Sans 32\" ! videoconvert",
                        title = escape_location(std::path::Path::new(title)),
                    ),
                }
            }
        }
    }
}

/// Where the streamed programme's audio comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioSource {
    /// Synthesised silence.
    ///
    /// **Not optional on WFD:** the sink declares an A/V session and discards
    /// the programme if it arrives with video alone.
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
    /// The default output. Resolved by the server, so it follows the person
    /// when they switch device mid-session, and it carries **everything** the
    /// computer is playing.
    Default,
    /// The virtual card's monitor: only what an application was routed to it.
    Virtual,
}

impl Monitor {
    fn device(self) -> &'static str {
        match self {
            // Resolved by the server (PulseAudio, or PipeWire's compatibility
            // mode) — there is nothing for us to query.
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
    /// Picks the best source available on this machine.
    ///
    /// Falls back to silence if there is no way to capture system audio — the
    /// audio branch is **not optional** on WFD (the sink discards a video-only
    /// programme), so sending silence beats sending nothing.
    pub fn detect() -> Self {
        Self::for_settings(&crate::settings::current())
    }

    /// The source the person asked for, or the closest thing this machine can
    /// actually produce.
    ///
    /// Turning both off is a legitimate choice — sharing a screen in a meeting
    /// where the sound would echo — and it still yields silence rather than no
    /// audio branch at all.
    pub fn for_settings(settings: &crate::settings::Settings) -> Self {
        if init().is_err() || gst::ElementFactory::find("pulsesrc").is_none() {
            tracing::info!("no `pulsesrc`; the cast will go without audio");
            return AudioSource::Silence;
        }
        let volume = settings.mic_volume as f64 / 100.0;
        // Mixing needs an element that may not be installed (`audiomixer` is in
        // gst-plugins-base, but a minimal image can still lack it). Without it
        // the honest answer is the system audio, not a pipeline that fails to
        // build at the moment the person hits cast.
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
}

impl AudioSource {
    pub fn description(&self) -> String {
        match self {
            // `samplesperbuffer=480` = 10 ms at 48 kHz (the default, 1024, is
            // 21 ms). The audio branch is what sets the pipeline's latency
            // floor: measured in the field, 41 ms with the default.
            AudioSource::Silence => {
                "audiotestsrc is-live=true wave=silence samplesperbuffer=480".to_string()
            }
            // `provide-clock=false`: the screen capture sets the pace; a second
            // clock in the pipeline fights with it.
            //
            // `buffer-time`: see `CAPTURE_AUDIO_BUFFER_MS`. This branch reports
            // the pipeline's largest latency, so it decides when the picture
            // arrives.
            AudioSource::System(monitor) => format!(
                "pulsesrc device={device} provide-clock=false do-timestamp=true{buffer}",
                device = monitor.device(),
                buffer = audio_buffer_property()
            ),
            // `@DEFAULT_SOURCE@`, the counterpart of `@DEFAULT_MONITOR@`: the
            // input the person selected in their sound settings, following
            // them when they plug a headset in mid-session.
            AudioSource::Mic { volume } => format!(
                "pulsesrc device=@DEFAULT_SOURCE@ provide-clock=false do-timestamp=true{buffer} \
                 ! audioconvert ! audioresample ! volume volume={volume:.2}",
                buffer = audio_buffer_property()
            ),
            // Two live sources into one branch. What matters here:
            //
            // - `audiomixer` (not the deprecated `audiomixer`-less `adder`)
            //   resamples and aligns by timestamp, so the two sources need not
            //   agree on format or arrive in step;
            // - both are converted to a common rate **before** the mixer, or it
            //   refuses to link;
            // - `latency=20000000` (20 ms) is how long the mixer waits for a
            //   late source before outputting without it. The default, 0, makes
            //   a microphone that hiccups drop the system audio with it.
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
            // Mixed with silence, and that is not belt and braces: a film with
            // no audio track, and every photo, leaves this branch with nothing
            // linked to it. The muxer would then wait for a track that never
            // comes and the picture would never leave the machine. The mixer
            // always has the silence to fall back on.
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

/// Quotes a path for `gst_parse_launch`.
///
/// A file called `My Holiday - Côte d'Azur.mp4` would otherwise end the
/// property at the first space and take the rest as elements. Backslashes and
/// quotes are escaped first, so a name containing one cannot close the string
/// and start writing pipeline of its own.
pub(super) fn escape_location(path: &std::path::Path) -> String {
    let text = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{text}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_still_screen_is_not_left_at_one_frame_per_second() {
        // A compositor repaints what changed, so on a still screen the only
        // thing producing buffers is this timeout — it *is* the frame rate.
        // At the 1000 ms it used to be, typing in a terminal changed one
        // character cell and the picture updated about once a second.
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
    fn mutter_source_omits_the_fd_but_keeps_the_node() {
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
    fn the_virtual_card_replaces_the_default_monitor_rather_than_adding_to_it() {
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
    fn the_switch_is_what_chooses_the_monitor() {
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
    fn a_buffer_floor_is_asked_for_only_where_it_was_given() {
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
    fn the_capture_source_never_has_caps_forced_on_it() {
        // A privacy regression seen in the field, and the reason this is a
        // test rather than a comment.
        //
        // `pipewiresrc` documents `autoconnect` as "Attempt to find a peer to
        // connect to" and defaults it to true. Forcing a size on the producer
        // made the authorised node unable to negotiate, the element went
        // looking for another video peer, and the laptop's **webcam** was
        // streamed to the projector.
        //
        // The portal path is protected by its `fd`: that PipeWire remote holds
        // only the node the user authorised, cameras included in nothing. The
        // Mutter path has no `fd` and lives in the session's own daemon, where
        // the camera does exist — so nothing may narrow the negotiation there.
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
}
