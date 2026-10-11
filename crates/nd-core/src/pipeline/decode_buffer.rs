//! Declares how many decoded pictures a receiver must hold.
//!
//! An H.264 SPS may carry `bitstream_restriction` in its VUI, with
//! `max_dec_frame_buffering` and `max_num_reorder_frames`. Without them a
//! decoder has to assume the most its level allows: at 2560x1440 level 5.1
//! that is 12 pictures, about 66 MB. NVENC leaves the restriction out. A
//! Panasonic 75GX880 started x264 streams that declared one picture 8 times in
//! 8, and 2 in 6 when they declared twelve; NVENC went from 3 starts in 5 to 4
//! in 5 once its SPS stated three pictures and no reordering.
//!
//! Every encoder here runs without B-frames, so pictures leave in decoding
//! order and `max_num_reorder_frames` is 0, and the buffer is the reference
//! count the SPS already names. An SPS that declares the restriction itself is
//! left alone, as is one this parser does not cover (scaling matrices), rather
//! than guessed at.

use std::sync::Mutex;

use gst::prelude::*;
use gstreamer as gst;

use super::ENCODER_NAME;

/// The last SPS the encoder sent, and what it became.
type LastSps = Option<(Vec<u8>, Option<Vec<u8>>)>;

/// Rewrites the SPS in the encoder's output so it states its decode buffer.
///
/// Applies to byte-stream output only, the format every path here asks for;
/// anything else passes through.
pub(super) fn declare_decode_buffer(pipeline: &gst::Pipeline) {
    let Some(pad) = pipeline
        .by_name(ENCODER_NAME)
        .and_then(|encoder| encoder.static_pad("src"))
    else {
        return;
    };
    // The SPS repeats at every IDR; the rewrite of the last one is kept.
    let last: Mutex<LastSps> = Mutex::new(None);
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let byte_stream = pad.current_caps().is_some_and(|caps| {
            caps.structure(0)
                .and_then(|s| s.get::<&str>("stream-format").ok())
                .is_none_or(|format| format == "byte-stream")
        });
        if !byte_stream {
            return gst::PadProbeReturn::Ok;
        }
        let Some(buffer) = info.buffer_mut() else {
            return gst::PadProbeReturn::Ok;
        };
        let rewritten = {
            let Ok(map) = buffer.map_readable() else {
                return gst::PadProbeReturn::Ok;
            };
            let mut last = last.lock().unwrap_or_else(|poison| poison.into_inner());
            rewrite_access_unit(&map, |sps| match &*last {
                Some((seen, answer)) if seen.as_slice() == sps => answer.clone(),
                _ => {
                    let answer = with_decode_buffer(sps);
                    *last = Some((sps.to_vec(), answer.clone()));
                    answer
                }
            })
        };
        if let Some(data) = rewritten {
            let mut replacement = gst::Buffer::from_mut_slice(data);
            if let (Some(new), Some(old)) = (replacement.get_mut(), info.buffer()) {
                let _ = old.copy_into(
                    new,
                    gst::BufferCopyFlags::FLAGS
                        | gst::BufferCopyFlags::TIMESTAMPS
                        | gst::BufferCopyFlags::META,
                    ..,
                );
            }
            info.data = Some(gst::PadProbeData::Buffer(replacement));
        }
        gst::PadProbeReturn::Ok
    });
}

/// The access unit with each SPS that `rewrite` changes replaced, or `None`
/// when nothing changed. `rewrite` gets the NAL unit without its start code.
fn rewrite_access_unit(
    data: &[u8],
    mut rewrite: impl FnMut(&[u8]) -> Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    let starts = start_codes(data);
    let mut out: Option<Vec<u8>> = None;
    for (i, &(code, nal)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map_or(data.len(), |&(next, _)| next);
        let unit = &data[nal..end];
        let replacement = (unit.first().is_some_and(|header| header & 0x1f == 7))
            .then(|| rewrite(unit))
            .flatten();
        match (replacement, &mut out) {
            (Some(sps), out) => {
                let out = out.get_or_insert_with(|| data[..code].to_vec());
                out.extend_from_slice(&data[code..nal]);
                out.extend_from_slice(&sps);
            }
            (None, Some(out)) => out.extend_from_slice(&data[code..end]),
            (None, None) => {}
        }
    }
    out
}

/// Each Annex B start code as (where the code begins, where its NAL begins).
fn start_codes(data: &[u8]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            let code = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            found.push((code, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    found
}

/// An SPS NAL unit (header byte included, emulation prevention in place)
/// with `bitstream_restriction` added, or `None` when it needs none or this
/// parser cannot follow it.
fn with_decode_buffer(nal: &[u8]) -> Option<Vec<u8>> {
    let (&header, escaped) = nal.split_first()?;
    let rbsp = unescape(escaped);
    let mut bits = BitReader::new(&rbsp);
    let profile_idc = bits.read(8)?;
    bits.skip(16)?; // constraint flags, reserved bits, level_idc
    bits.ue()?; // seq_parameter_set_id
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        if bits.ue()? == 3 {
            bits.skip(1)?; // separate_colour_plane_flag
        }
        bits.ue()?; // bit_depth_luma_minus8
        bits.ue()?; // bit_depth_chroma_minus8
        bits.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if bits.flag()? {
            return None; // scaling matrices: not followed here
        }
    }
    bits.ue()?; // log2_max_frame_num_minus4
    match bits.ue()? {
        0 => {
            bits.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            bits.skip(1)?;
            bits.se()?;
            bits.se()?;
            for _ in 0..bits.ue()? {
                bits.se()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = bits.ue()?;
    bits.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    bits.ue()?; // pic_width_in_mbs_minus1
    bits.ue()?; // pic_height_in_map_units_minus1
    if !bits.flag()? {
        bits.skip(1)?; // mb_adaptive_frame_field_flag
    }
    bits.skip(1)?; // direct_8x8_inference_flag
    if bits.flag()? {
        for _ in 0..4 {
            bits.ue()?; // frame_crop_*_offset
        }
    }

    let vui_flag_at = bits.position();
    let mut out = BitWriter::default();
    if bits.flag()? {
        if bits.flag()? && bits.read(8)? == 255 {
            bits.skip(32)?; // sar_width, sar_height
        }
        if bits.flag()? {
            bits.skip(1)?; // overscan_appropriate_flag
        }
        if bits.flag()? {
            bits.skip(4)?; // video_format, video_full_range_flag
            if bits.flag()? {
                bits.skip(24)?; // colour primaries, transfer, matrix
            }
        }
        if bits.flag()? {
            bits.ue()?;
            bits.ue()?;
        }
        if bits.flag()? {
            bits.skip(65)?; // num_units_in_tick, time_scale, fixed_frame_rate_flag
        }
        let nal_hrd = bits.flag()?;
        if nal_hrd {
            skip_hrd(&mut bits)?;
        }
        let vcl_hrd = bits.flag()?;
        if vcl_hrd {
            skip_hrd(&mut bits)?;
        }
        if nal_hrd || vcl_hrd {
            bits.skip(1)?; // low_delay_hrd_flag
        }
        bits.skip(1)?; // pic_struct_present_flag
        let restriction_at = bits.position();
        if bits.flag()? {
            return None; // already declared
        }
        out.copy(&rbsp, restriction_at);
    } else {
        out.copy(&rbsp, vui_flag_at);
        out.put(1, 1); // vui_parameters_present_flag
        // aspect ratio, overscan, video signal, chroma location, timing, NAL
        // and VCL HRD, pic_struct: none of them present.
        out.put(0, 8);
    }
    out.put(1, 1); // bitstream_restriction_flag
    out.put(1, 1); // motion_vectors_over_pic_boundaries_flag
    out.ue(2); // max_bytes_per_pic_denom: the value the spec infers
    out.ue(1); // max_bits_per_mb_denom
    out.ue(15); // log2_max_mv_length_horizontal
    out.ue(15); // log2_max_mv_length_vertical
    out.ue(0); // max_num_reorder_frames
    out.ue(max_num_ref_frames.max(1)); // max_dec_frame_buffering
    out.trailing_bits();

    let mut nal = vec![header];
    nal.extend(escape(&out.bytes));
    Some(nal)
}

fn skip_hrd(bits: &mut BitReader) -> Option<()> {
    let count = bits.ue()? + 1;
    bits.skip(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..count {
        bits.ue()?;
        bits.ue()?;
        bits.skip(1)?;
    }
    bits.skip(20) // four 5-bit lengths
}

/// RBSP from NAL payload: drops the 0x03 of every 00 00 03.
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &byte in data {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// NAL payload from RBSP: a 0x03 after two zeros when the next byte is 0..=3.
fn escape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 4);
    let mut zeros = 0;
    for &byte in data {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn position(&self) -> usize {
        self.at
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = self.data.get(self.at / 8)?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Some(u32::from(bit))
    }

    fn flag(&mut self) -> Option<bool> {
        self.bit().map(|bit| bit == 1)
    }

    fn read(&mut self, count: u32) -> Option<u32> {
        (0..count).try_fold(0, |value, _| Some((value << 1) | self.bit()?))
    }

    fn skip(&mut self, count: usize) -> Option<()> {
        (self.at + count <= self.data.len() * 8).then(|| self.at += count)
    }

    /// Exp-Golomb, unsigned. Values past 32 bits do not occur in an SPS.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.read(zeros)?)
    }

    fn se(&mut self) -> Option<i64> {
        let code = i64::from(self.ue()?);
        Some(if code % 2 == 1 {
            (code + 1) / 2
        } else {
            -code / 2
        })
    }
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn put(&mut self, value: u32, count: u32) {
        for shift in (0..count).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if (value >> shift) & 1 == 1 {
                *self.bytes.last_mut().expect("pushed above") |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
    }

    /// The first `count` bits of `data`, unchanged.
    fn copy(&mut self, data: &[u8], count: usize) {
        for at in 0..count {
            self.put(u32::from((data[at / 8] >> (7 - at % 8)) & 1), 1);
        }
    }

    fn ue(&mut self, value: u32) {
        let code = value + 1;
        let length = 32 - code.leading_zeros();
        self.put(0, length - 1);
        self.put(code, length);
    }

    fn trailing_bits(&mut self) {
        self.put(1, 1);
        while !self.bits.is_multiple_of(8) {
            self.put(0, 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AUD, SPS, PPS and IDR from `nvh264enc` at 160x96, constrained baseline:
    /// a VUI without `bitstream_restriction`, three reference frames.
    const NVENC_IDR: &[u8] = include_bytes!("../../tests/fixtures/nvenc-idr-160x96.h264");

    fn sps(access_unit: &[u8]) -> Vec<u8> {
        let starts = start_codes(access_unit);
        starts
            .iter()
            .enumerate()
            .find_map(|(i, &(_, nal))| {
                let end = starts
                    .get(i + 1)
                    .map_or(access_unit.len(), |&(next, _)| next);
                (access_unit[nal] & 0x1f == 7).then(|| access_unit[nal..end].to_vec())
            })
            .unwrap()
    }

    /// The VUI's restriction fields, read back, or `None` without them.
    fn declared(nal: &[u8]) -> Option<(u32, u32)> {
        let rbsp = unescape(&nal[1..]);
        let mut bits = BitReader::new(&rbsp);
        bits.skip(24)?;
        bits.ue()?;
        bits.ue()?;
        if bits.ue()? == 0 {
            bits.ue()?;
        }
        bits.ue()?;
        bits.skip(1)?;
        bits.ue()?;
        bits.ue()?;
        if !bits.flag()? {
            bits.skip(1)?;
        }
        bits.skip(1)?;
        if bits.flag()? {
            for _ in 0..4 {
                bits.ue()?;
            }
        }
        if !bits.flag()? {
            return None;
        }
        if bits.flag()? && bits.read(8)? == 255 {
            bits.skip(32)?;
        }
        if bits.flag()? {
            bits.skip(1)?;
        }
        if bits.flag()? {
            bits.skip(4)?;
            if bits.flag()? {
                bits.skip(24)?;
            }
        }
        if bits.flag()? {
            bits.ue()?;
            bits.ue()?;
        }
        if bits.flag()? {
            bits.skip(65)?;
        }
        let (nal_hrd, vcl_hrd) = (bits.flag()?, bits.flag()?);
        assert!(!nal_hrd && !vcl_hrd, "the fixtures carry no HRD");
        bits.skip(1)?;
        if !bits.flag()? {
            return None;
        }
        bits.skip(1)?;
        for _ in 0..4 {
            bits.ue()?;
        }
        Some((bits.ue()?, bits.ue()?))
    }

    #[test]
    fn an_nvenc_sps_declares_its_three_pictures_and_no_reordering() {
        let original = sps(NVENC_IDR);
        assert_eq!(
            declared(&original),
            None,
            "the fixture should lack the restriction"
        );
        let rewritten = with_decode_buffer(&original).unwrap();
        assert_eq!(declared(&rewritten), Some((0, 3)));
        // Everything before the restriction is the encoder's own.
        assert_eq!(rewritten[..8], original[..8]);
    }

    #[test]
    fn an_sps_that_already_declares_it_is_left_alone() {
        let rewritten = with_decode_buffer(&sps(NVENC_IDR)).unwrap();
        assert_eq!(with_decode_buffer(&rewritten), None);
    }

    #[test]
    fn an_sps_without_a_vui_gets_one_holding_only_the_restriction() {
        // The NVENC SPS cut at its VUI flag, which then reads 0.
        let original = sps(NVENC_IDR);
        let rbsp = unescape(&original[1..]);
        let mut bits = BitReader::new(&rbsp);
        bits.skip(24).unwrap();
        for _ in 0..3 {
            bits.ue().unwrap();
        }
        bits.ue().unwrap();
        bits.skip(1).unwrap();
        bits.ue().unwrap();
        bits.ue().unwrap();
        bits.skip(3).unwrap();
        let mut bare = BitWriter::default();
        bare.copy(&rbsp, bits.position());
        bare.put(0, 1);
        bare.trailing_bits();
        let mut nal = vec![original[0]];
        nal.extend(escape(&bare.bytes));

        assert_eq!(declared(&nal), None);
        assert_eq!(declared(&with_decode_buffer(&nal).unwrap()), Some((0, 3)));
    }

    #[test]
    fn only_the_sps_of_an_access_unit_changes() {
        let rewritten = rewrite_access_unit(NVENC_IDR, with_decode_buffer).unwrap();
        let (before, after) = (start_codes(NVENC_IDR), start_codes(&rewritten));
        assert_eq!(before.len(), after.len());
        let unit = |data: &[u8], starts: &[(usize, usize)], i: usize| {
            let end = starts.get(i + 1).map_or(data.len(), |&(next, _)| next);
            data[starts[i].1..end].to_vec()
        };
        for i in 0..before.len() {
            let (old, new) = (unit(NVENC_IDR, &before, i), unit(&rewritten, &after, i));
            if old[0] & 0x1f == 7 {
                assert_ne!(old, new);
            } else {
                assert_eq!(old, new);
            }
        }
    }

    #[test]
    fn escaping_round_trips_through_emulation_prevention() {
        let rbsp = [0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 4, 9];
        let escaped = escape(&rbsp);
        assert_eq!(escaped, [0, 0, 3, 0, 1, 0, 0, 3, 2, 0, 0, 3, 3, 0, 0, 4, 9]);
        assert_eq!(unescape(&escaped), rbsp);
    }

    #[test]
    fn a_rewritten_stream_still_decodes() {
        crate::pipeline::init().unwrap();
        let rewritten = rewrite_access_unit(NVENC_IDR, with_decode_buffer).unwrap();
        let path =
            std::env::temp_dir().join(format!("nd-decode-buffer-{}.h264", std::process::id()));
        std::fs::write(&path, rewritten).unwrap();
        let pipeline = gst::parse::launch(&format!(
            "filesrc location={} ! h264parse ! avdec_h264 ! fakesink name=out",
            path.display()
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let decoded = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = decoded.clone();
        pipeline
            .by_name("out")
            .and_then(|sink| sink.static_pad("sink"))
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .unwrap();
        pipeline.set_state(gst::State::Null).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");
        assert_eq!(decoded.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
