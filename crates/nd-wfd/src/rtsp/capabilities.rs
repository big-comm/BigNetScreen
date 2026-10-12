//! What a Wi-Fi Display sink can show, and the one mode we pick from it.
//!
//! The sink answers M3 with `wfd_video_formats`, a set of bitmaps into three
//! fixed tables of resolutions (CEA, VESA, Handheld), plus its audio codecs
//! and RTP port. M4 names exactly one bit of one table back. The tables are
//! the Wi-Fi Display 1.1 specification's; a bit's meaning is its index.

/// A WFD resolution table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolutionTable {
    Cea = 0,
    Vesa = 1,
    Handheld = 2,
}

/// A video mode the sink supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WfdMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub interlaced: bool,
    pub table: ResolutionTable,
    /// The bit's index within the table.
    pub index: u8,
}

impl WfdMode {
    fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

/// The CEA table (bit → mode). Source: Wi-Fi Display 1.1 spec, table 5-9.
const CEA_MODES: &[(u32, u32, u32, bool)] = &[
    (640, 480, 60, false),
    (720, 480, 60, false),
    (720, 480, 60, true),
    (720, 576, 50, false),
    (720, 576, 50, true),
    (1280, 720, 30, false),
    (1280, 720, 60, false),
    (1920, 1080, 30, false),
    (1920, 1080, 60, false),
    (1920, 1080, 60, true),
    (1280, 720, 25, false),
    (1280, 720, 50, false),
    (1920, 1080, 25, false),
    (1920, 1080, 50, false),
    (1920, 1080, 50, true),
    (1280, 720, 24, false),
    (1920, 1080, 24, false),
];

/// The VESA table (bit → mode).
const VESA_MODES: &[(u32, u32, u32, bool)] = &[
    (800, 600, 30, false),
    (800, 600, 60, false),
    (1024, 768, 30, false),
    (1024, 768, 60, false),
    (1152, 864, 30, false),
    (1152, 864, 60, false),
    (1280, 768, 30, false),
    (1280, 768, 60, false),
    (1280, 800, 30, false),
    (1280, 800, 60, false),
    (1360, 768, 30, false),
    (1360, 768, 60, false),
    (1366, 768, 30, false),
    (1366, 768, 60, false),
    (1280, 1024, 30, false),
    (1280, 1024, 60, false),
    (1400, 1050, 30, false),
    (1400, 1050, 60, false),
    (1440, 900, 30, false),
    (1440, 900, 60, false),
    (1600, 900, 30, false),
    (1600, 900, 60, false),
    (1600, 1200, 30, false),
    (1600, 1200, 60, false),
    (1680, 1024, 30, false),
    (1680, 1024, 60, false),
    (1680, 1050, 30, false),
    (1680, 1050, 60, false),
    (1920, 1200, 30, false),
    (1920, 1200, 60, false),
];

/// The Handheld table (bit → mode).
const HH_MODES: &[(u32, u32, u32, bool)] = &[
    (800, 480, 30, false),
    (800, 480, 60, false),
    (854, 480, 30, false),
    (854, 480, 60, false),
    (864, 480, 30, false),
    (864, 480, 60, false),
    (640, 360, 30, false),
    (640, 360, 60, false),
    (960, 540, 30, false),
    (960, 540, 60, false),
    (848, 480, 30, false),
    (848, 480, 60, false),
];

/// The largest picture Wi-Fi Display can carry, over every table.
///
/// A sink advertises its modes as bits into the three tables above, so nothing
/// larger than this can be negotiated with any sink that exists. It is what a
/// virtual screen should be created at: making a 3440x1440 desktop for a
/// protocol whose ceiling is 1920x1200 only buys a `videoscale` on every frame.
///
/// Computed rather than written down, so adding a mode cannot leave it stale.
pub const MAX_SOURCE_SIZE: (u32, u32) = {
    let (width, height) = widest(CEA_MODES, 0, 0);
    let (width, height) = widest(VESA_MODES, width, height);
    widest(HH_MODES, width, height)
};

const fn widest(modes: &[(u32, u32, u32, bool)], mut width: u32, mut height: u32) -> (u32, u32) {
    let mut index = 0;
    while index < modes.len() {
        if modes[index].0 > width {
            width = modes[index].0;
        }
        if modes[index].1 > height {
            height = modes[index].1;
        }
        index += 1;
    }
    (width, height)
}

fn decode_table(
    bitmap: u32,
    table: ResolutionTable,
    modes: &[(u32, u32, u32, bool)],
) -> Vec<WfdMode> {
    modes
        .iter()
        .enumerate()
        .filter(|(index, _)| bitmap & (1u32 << index) != 0)
        .map(|(index, (width, height, fps, interlaced))| WfdMode {
            width: *width,
            height: *height,
            fps: *fps,
            interlaced: *interlaced,
            table,
            index: index as u8,
        })
        .collect()
}

/// The capabilities the sink advertises (M3's response).
#[derive(Debug, Default, Clone)]
pub struct SinkCaps {
    /// Raw value of `wfd_video_formats`.
    pub video_formats: Option<String>,
    /// Video modes decoded from the CEA/VESA/HH tables.
    pub modes: Vec<WfdMode>,
    /// Accepted H.264 profile (bitmask; 0x01 = Constrained Baseline).
    pub profile: u8,
    /// The accepted H.264 level (a bitmask).
    pub level: u8,
    /// Raw value of `wfd_audio_codecs`.
    pub audio_codecs: Option<String>,
    /// The RTP port where the sink wants to receive the stream.
    pub rtp_port: u16,
    /// The native mode the sink declares: its actual panel.
    ///
    /// The first field of `wfd_video_formats`, index in the high 5 bits and
    /// table in the low 3. Without it, a 16:10 source screen would pick
    /// 1680x1050 (VESA) for a Full HD projector, which then rescales
    /// everything.
    pub native: Option<WfdMode>,
}

impl SinkCaps {
    /// The best mode for `source` among those within `limit` and `fps`, or
    /// `None` when the sink offers nothing inside them.
    pub fn preferred_mode(
        &self,
        source: (u32, u32),
        limit: (u32, u32),
        fps: u32,
    ) -> Option<WfdMode> {
        let mut compatible = self.clone();
        compatible
            .modes
            .retain(|mode| mode.fps <= fps && mode.width <= limit.0 && mode.height <= limit.1);
        compatible.best_mode_for(source, limit)
    }

    /// Picks the best common mode: largest area, then highest frame rate.
    ///
    /// Interlaced modes are discarded: the pipeline produces progressive, and
    /// sending progressive while declaring interlaced confuses the sink.
    pub(crate) fn best_mode(&self) -> Option<WfdMode> {
        self.modes
            .iter()
            .copied()
            .filter(|m| !m.interlaced)
            .max_by_key(|m| (m.pixels(), m.fps))
    }

    /// The best mode that does not exceed a resolution cap.
    pub(crate) fn best_mode_within(&self, max_width: u32, max_height: u32) -> Option<WfdMode> {
        self.modes
            .iter()
            .copied()
            .filter(|m| !m.interlaced && m.width <= max_width && m.height <= max_height)
            .max_by_key(|m| (m.pixels(), m.fps))
            .or_else(|| self.best_mode())
    }

    /// The best mode for a specific source screen.
    ///
    /// The criteria, in order:
    /// 1. the aspect ratio closest to the captured screen. A mode
    ///    with another aspect forces letterboxing or distorts the picture,
    ///    which is worse than losing a few pixels;
    /// 2. the largest area, without exceeding the requested cap;
    /// 3. the highest frame rate.
    ///
    /// Without criterion (1), a sink that advertises the whole VESA table
    /// leads to 1920x1200p30 on a 16:9 screen: more pixels, but with bars and
    /// half the smoothness of 1920x1080p60.
    pub fn best_mode_for(&self, source: (u32, u32), max: (u32, u32)) -> Option<WfdMode> {
        // The sink's native mode beats everything when it fits the cap: it is
        // the panel's real resolution, and sending anything else only makes
        // the device rescale (losing sharpness) or refuse.
        if let Some(native) = self.native
            && !native.interlaced
            && native.width <= max.0
            && native.height <= max.1
            && self.modes.contains(&native)
        {
            return Some(native);
        }

        let (src_w, src_h) = source;
        if src_w == 0 || src_h == 0 {
            return self.best_mode_within(max.0, max.1);
        }

        let candidates: Vec<WfdMode> = self
            .modes
            .iter()
            .copied()
            .filter(|m| !m.interlaced && m.width <= max.0 && m.height <= max.1)
            .collect();
        let pool = if candidates.is_empty() {
            self.modes
                .iter()
                .copied()
                .filter(|m| !m.interlaced)
                .collect()
        } else {
            candidates
        };

        pool.into_iter().min_by_key(|m| {
            // The aspect difference in thousandths, using integer arithmetic.
            let cross = (m.width as i64 * src_h as i64) - (src_w as i64 * m.height as i64);
            let aspect_diff = cross.abs() * 1000 / (src_w as i64 * src_h as i64);
            (
                aspect_diff,
                std::cmp::Reverse(m.pixels()),
                std::cmp::Reverse(m.fps),
            )
        })
    }
}

/// Parses the `wfd_video_formats` value the sink advertises.
///
/// Format: `<native> <pref-display-mode> <profile> <level> <CEA> <VESA> <HH>
/// <latency> <min-slice> <slice-enc> <frame-rate-control> [<h-res> <v-res>]`
pub fn parse_video_formats(value: &str) -> SinkCaps {
    let mut caps = SinkCaps {
        video_formats: Some(value.to_string()),
        ..Default::default()
    };

    let fields: Vec<&str> = value.split_whitespace().collect();
    if fields.len() < 7 {
        tracing::warn!(%value, "wfd_video_formats with too few fields");
        return caps;
    }

    caps.profile = u8::from_str_radix(fields[2], 16).unwrap_or(0x01);
    caps.level = u8::from_str_radix(fields[3], 16).unwrap_or(0x02);

    if let Ok(native) = u8::from_str_radix(fields[0], 16) {
        caps.native = mode_from_native(native);
    }

    let cea = u32::from_str_radix(fields[4], 16).unwrap_or(0);
    let vesa = u32::from_str_radix(fields[5], 16).unwrap_or(0);
    let hh = u32::from_str_radix(fields[6], 16).unwrap_or(0);

    caps.modes
        .extend(decode_table(cea, ResolutionTable::Cea, CEA_MODES));
    caps.modes
        .extend(decode_table(vesa, ResolutionTable::Vesa, VESA_MODES));
    caps.modes
        .extend(decode_table(hh, ResolutionTable::Handheld, HH_MODES));

    caps
}

/// Decodes the `native` field: index in the high 5 bits, table in the low 3.
pub fn mode_from_native(native: u8) -> Option<WfdMode> {
    let index = (native >> 3) as usize;
    let (table, modes) = match native & 0b111 {
        0 => (ResolutionTable::Cea, CEA_MODES),
        1 => (ResolutionTable::Vesa, VESA_MODES),
        2 => (ResolutionTable::Handheld, HH_MODES),
        _ => return None,
    };
    let (width, height, fps, interlaced) = *modes.get(index)?;
    Some(WfdMode {
        width,
        height,
        fps,
        interlaced,
        table,
        index: index as u8,
    })
}

/// Builds M4's `wfd_video_formats` with a single selected mode.
pub(crate) fn m4_video_formats(mode: &WfdMode, profile: u8, level: u8) -> String {
    let bit = 1u32 << mode.index;
    let (cea, vesa, hh) = match mode.table {
        ResolutionTable::Cea => (bit, 0, 0),
        ResolutionTable::Vesa => (0, bit, 0),
        ResolutionTable::Handheld => (0, 0, bit),
    };
    // `native` = the mode's index (5 bits) + the table (3 bits).
    let native = (mode.index << 3) | (mode.table as u8);
    // We only advertise Constrained Baseline, which is what the pipeline produces.
    let profile = if profile & 0x01 != 0 { 0x01 } else { profile };
    format!(
        "{native:02x} 00 {profile:02x} {level:02x} \
         {cea:08X} {vesa:08X} {hh:08X} 00 0000 0000 00 none none"
    )
}

/// Parses the `text/parameters` body of the M3 response.
pub(crate) fn parse_caps(body: &str) -> SinkCaps {
    let mut caps = SinkCaps::default();
    for line in body.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "wfd_video_formats" => {
                caps = SinkCaps {
                    audio_codecs: caps.audio_codecs,
                    rtp_port: caps.rtp_port,
                    ..parse_video_formats(value)
                };
            }
            "wfd_audio_codecs" => caps.audio_codecs = Some(value.to_string()),
            "wfd_client_rtp_ports" => {
                // ex.: "RTP/AVP/UDP;unicast 19000 0 mode=play"
                if let Some(port) = value
                    .split_whitespace()
                    .nth(1)
                    .and_then(|p| p.parse::<u16>().ok())
                {
                    caps.rtp_port = port;
                }
            }
            _ => {}
        }
    }
    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real M3 response from a Samsung Projector LSP3.
    ///
    /// `native = 0x40` → index 8 of the CEA table = 1920x1080p60 (its panel).
    const M3_BODY: &str = "wfd_audio_codecs: LPCM 00000003 00, AAC 00000001 00\r\n\
         wfd_video_formats: 40 00 01 10 000001e3 0f3fffff 00000fff 00 0000 00c8 01 none none\r\n\
         wfd_client_rtp_ports: RTP/AVP/UDP;unicast 19002 0 mode=play\r\n";

    #[test]
    fn advertised_modes_fit_the_ceiling() {
        // MAX_SOURCE_SIZE decides how big a virtual screen gets made, so a new
        // table entry that outgrew it would silently cost a scale on every
        // frame instead of failing here.
        for modes in [CEA_MODES, VESA_MODES, HH_MODES] {
            for (width, height, _, _) in modes {
                assert!(
                    *width <= MAX_SOURCE_SIZE.0 && *height <= MAX_SOURCE_SIZE.1,
                    "{width}x{height} is outside {MAX_SOURCE_SIZE:?}"
                );
            }
        }
    }

    #[test]
    fn parses_a_real_m3_response() {
        let caps = parse_caps(M3_BODY);
        assert_eq!(caps.rtp_port, 19002);
        assert!(caps.audio_codecs.unwrap().contains("AAC"));
        assert_eq!(caps.profile, 0x01, "Constrained Baseline");
        assert_eq!(caps.level, 0x10, "level 4.2");
        assert!(!caps.modes.is_empty());
    }

    #[test]
    fn native_mode_survives_parse_caps() {
        // Losing `native` silently disables the panel rule, and a Full HD
        // projector gets 1680x1050.
        let caps = parse_caps(M3_BODY);
        let native = caps.native.expect("the sink declared a native mode");
        assert_eq!((native.width, native.height, native.fps), (1920, 1080, 60));
        assert_eq!(native.table, ResolutionTable::Cea);
    }

    #[test]
    fn native_mode_wins_over_source_aspect() {
        // On a 16:10 source screen, the projector's panel mode (16:9) must
        // win: sending 1680x1050 makes the device rescale or refuse.
        let caps = parse_caps(M3_BODY);
        let mode = caps
            .best_mode_for((1920, 1200), (1920, 1080))
            .expect("some mode");
        assert_eq!((mode.width, mode.height, mode.fps), (1920, 1080, 60));
    }

    #[test]
    fn oversized_native_mode_is_ignored() {
        let caps = parse_caps(M3_BODY);
        let mode = caps
            .best_mode_for((1920, 1080), (1280, 720))
            .expect("some mode");
        assert!(mode.width <= 1280 && mode.height <= 720, "{mode:?}");
    }

    #[test]
    fn decodes_the_native_field() {
        // 0x40 = index 8, table 0 (CEA) = 1920x1080p60.
        let mode = mode_from_native(0x40).expect("a valid mode");
        assert_eq!((mode.width, mode.height, mode.fps), (1920, 1080, 60));
        // 0x49 = index 9, table 1 (VESA) = 1280x800p60.
        let mode = mode_from_native(0x49).expect("a valid mode");
        assert_eq!((mode.width, mode.height), (1280, 800));
        assert_eq!(mode.table, ResolutionTable::Vesa);
        // A table that does not exist.
        assert!(mode_from_native(0x07).is_none());
    }

    #[test]
    fn aspectless_choice_picks_widest_mode() {
        let caps = parse_caps(M3_BODY);
        let mode = caps.best_mode().expect("some mode");
        // On area alone, this sink reaches 1920x1080p60.
        assert_eq!((mode.width, mode.height, mode.fps), (1920, 1080, 60));
        assert!(!mode.interlaced);
    }

    #[test]
    fn source_aspect_beats_pixel_count() {
        // Without `native`, between 1600x900p60 and 1680x1050p60 on a 16:9
        // source, the 16:9 one wins even with fewer pixels.
        let mut caps = parse_caps(M3_BODY);
        caps.native = None;
        let mode = caps
            .best_mode_for((1920, 1080), (1700, 1080))
            .expect("some mode");
        assert_eq!(
            (mode.width, mode.height),
            (1600, 900),
            "16:9 must win over 1680x1050"
        );
    }

    #[test]
    fn aspect_rule_without_native_mode() {
        // Without `native`, the source-aspect rule applies. A 16:10 source
        // should get the closest 16:10 mode (1680x1050 from the VESA table).
        let mut caps = parse_caps(M3_BODY);
        caps.native = None;
        let mode = caps
            .best_mode_for((1920, 1200), (1920, 1200))
            .expect("some mode");
        assert_eq!((mode.width, mode.height), (1680, 1050));
    }

    #[test]
    fn source_size_zero_falls_back_gracefully() {
        let caps = parse_caps(M3_BODY);
        assert!(caps.best_mode_for((0, 0), (1920, 1080)).is_some());
    }

    #[test]
    fn respects_the_resolution_ceiling() {
        let caps = parse_caps(M3_BODY);
        let mode = caps.best_mode_within(1280, 720).expect("some mode");
        assert_eq!((mode.width, mode.height, mode.fps), (1280, 720, 60));
    }

    #[test]
    fn sink_limited_to_720p_gets_720p() {
        // CEA bit 5 = 1280x720p30, bit 6 = 1280x720p60.
        let body = "wfd_video_formats: 00 00 01 01 00000060 00000000 00000000 00 0000 0000 00 none none\r\n\
                    wfd_client_rtp_ports: RTP/AVP/UDP;unicast 19000 0 mode=play\r\n";
        let caps = parse_caps(body);
        let mode = caps.best_mode().expect("some mode");
        assert_eq!((mode.width, mode.height, mode.fps), (1280, 720, 60));
    }

    #[test]
    fn handheld_only_sink_is_supported() {
        // A cheap dongle advertising only the HH table: bit 9 = 960x540p60.
        let body =
            "wfd_video_formats: 00 00 01 01 00000000 00000000 00000200 00 0000 0000 00 none none";
        let caps = parse_caps(body);
        let mode = caps.best_mode().expect("some mode");
        assert_eq!((mode.width, mode.height, mode.fps), (960, 540, 60));
        assert_eq!(mode.table, ResolutionTable::Handheld);
    }

    #[test]
    fn interlaced_modes_are_never_selected() {
        // CEA bit 2 = 720x480i60 and bit 9 = 1920x1080i60, both interlaced.
        let body =
            "wfd_video_formats: 00 00 01 01 00000204 00000000 00000000 00 0000 0000 00 none none";
        let caps = parse_caps(body);
        assert!(caps.best_mode().is_none(), "there is no progressive mode");
    }

    #[test]
    fn m4_encodes_exactly_one_mode() {
        let mode = WfdMode {
            width: 1280,
            height: 720,
            fps: 60,
            interlaced: false,
            table: ResolutionTable::Cea,
            index: 6,
        };
        let value = m4_video_formats(&mode, 0x01, 0x02);
        let fields: Vec<&str> = value.split_whitespace().collect();
        let cea = u32::from_str_radix(fields[4], 16).unwrap();
        assert_eq!(cea, 1 << 6, "only the chosen mode's bit");
        assert_eq!(u32::from_str_radix(fields[5], 16).unwrap(), 0, "VESA empty");
        assert_eq!(u32::from_str_radix(fields[6], 16).unwrap(), 0, "HH empty");
        // `native` = index<<3 | table.
        assert_eq!(u8::from_str_radix(fields[0], 16).unwrap(), 6 << 3);
    }

    #[test]
    fn m4_marks_vesa_table() {
        let mode = WfdMode {
            width: 1280,
            height: 800,
            fps: 60,
            interlaced: false,
            table: ResolutionTable::Vesa,
            index: 9,
        };
        let value = m4_video_formats(&mode, 0x01, 0x02);
        let fields: Vec<&str> = value.split_whitespace().collect();
        assert_eq!(u32::from_str_radix(fields[4], 16).unwrap(), 0);
        assert_eq!(u32::from_str_radix(fields[5], 16).unwrap(), 1 << 9);
        assert_eq!(u8::from_str_radix(fields[0], 16).unwrap(), (9 << 3) | 1);
    }

    #[test]
    fn malformed_video_formats_are_tolerated() {
        for value in ["", "garbage", "00 00", "zz zz zz zz zz zz zz"] {
            assert!(parse_video_formats(value).modes.is_empty(), "{value}");
        }
    }

    #[test]
    fn preferences_pick_advertised_mode() {
        let caps =
            parse_video_formats("40 00 01 02 000001e0 00000000 00000000 00 0000 0000 00 none none");
        let mode = caps.preferred_mode((1920, 1080), (1920, 1080), 30).unwrap();
        assert_eq!((mode.width, mode.height, mode.fps), (1920, 1080, 30));
        assert!(caps.modes.contains(&mode));
        let mode = caps.preferred_mode((1920, 1080), (1280, 720), 30).unwrap();
        assert_eq!((mode.width, mode.height, mode.fps), (1280, 720, 30));
        assert!(caps.preferred_mode((1920, 1080), (854, 480), 30).is_none());
    }
}
