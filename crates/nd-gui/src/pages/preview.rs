//! Bounded, silent media previews. Pixel buffers cross threads; widgets do not.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use gstreamer::{self as gst, prelude::*};
use nd_core::media::MediaKind;
use relm4::gtk::gdk_pixbuf::prelude::*;
use relm4::gtk::{
    gdk,
    gdk_pixbuf::{Pixbuf, PixbufLoader},
    glib,
};

const SIZE: i32 = 360;
pub(super) static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
static CACHE: LazyLock<Mutex<VecDeque<CacheEntry>>> = LazyLock::new(Mutex::default);

struct CacheEntry {
    path: PathBuf,
    modified: Option<SystemTime>,
    size: u64,
    preview: Preview,
}

#[derive(Clone, Debug, Default)]
pub struct Preview {
    pub image: Option<Thumbnail>,
    pub duration: Option<f64>,
    pub artist: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Thumbnail {
    pub(super) pixels: glib::Bytes,
    pub(super) width: i32,
    pub(super) height: i32,
    pub(super) stride: i32,
    pub(super) alpha: bool,
}

impl Thumbnail {
    fn from_pixbuf(pixbuf: &Pixbuf) -> Self {
        Self {
            pixels: pixbuf.read_pixel_bytes(),
            width: pixbuf.width(),
            height: pixbuf.height(),
            stride: pixbuf.rowstride(),
            alpha: pixbuf.has_alpha(),
        }
    }

    fn decode(path: &Path) -> Result<Self, String> {
        Pixbuf::from_file_at_scale(path, SIZE, SIZE, true)
            .map(|image| Self::from_pixbuf(&image))
            .map_err(|err| err.to_string())
    }

    fn embedded(sample: &gst::Sample) -> Option<Self> {
        let buffer = sample.buffer()?.map_readable().ok()?;
        let loader = PixbufLoader::new();
        loader.connect_size_prepared(|loader, width, height| {
            let factor = f64::from(SIZE) / f64::from(width.max(height).max(1));
            loader.set_size(
                (f64::from(width) * factor).round().max(1.0) as i32,
                (f64::from(height) * factor).round().max(1.0) as i32,
            );
        });
        let written = loader.write(buffer.as_slice());
        let closed = loader.close();
        written.ok()?;
        closed.ok()?;
        loader.pixbuf().map(|image| Self::from_pixbuf(&image))
    }

    pub fn texture(&self) -> gdk::Texture {
        gdk::Texture::for_pixbuf(&Pixbuf::from_bytes(
            &self.pixels,
            relm4::gtk::gdk_pixbuf::Colorspace::Rgb,
            self.alpha,
            8,
            self.width,
            self.height,
            self.stride,
        ))
    }
}

pub async fn load(path: PathBuf, kind: MediaKind) -> Result<Preview, String> {
    let permit = SLOTS.acquire().await.map_err(|err| err.to_string())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let metadata = std::fs::metadata(&path).map_err(|err| err.to_string())?;
        let modified = metadata.modified().ok();
        let size = metadata.len();
        if let Some(entry) = CACHE
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .iter()
            .find(|entry| entry.path == path && entry.modified == modified && entry.size == size)
        {
            return Ok(entry.preview.clone());
        }
        let preview = decode(&path, kind)?;
        let mut cache = CACHE.lock().unwrap_or_else(|err| err.into_inner());
        cache.retain(|entry| entry.path != path);
        if cache.len() >= 96 {
            cache.pop_front();
        }
        cache.push_back(CacheEntry {
            path,
            modified,
            size,
            preview: preview.clone(),
        });
        Ok(preview)
    })
    .await
    .map_err(|err| err.to_string())?
}

fn decode(path: &Path, kind: MediaKind) -> Result<Preview, String> {
    if kind == MediaKind::Photo {
        return Ok(Preview {
            image: Some(Thumbnail::decode(path)?),
            ..Default::default()
        });
    }
    gst::init().map_err(|err| err.to_string())?;
    let path = std::fs::canonicalize(path).map_err(|err| err.to_string())?;
    let uri = glib::filename_to_uri(&path, None).map_err(|err| err.to_string())?;
    let playbin = gst::ElementFactory::make("playbin")
        .property("uri", uri.as_str())
        .build()
        .map_err(|err| err.to_string())?;
    let pipeline = playbin
        .downcast::<gst::Pipeline>()
        .map_err(|_| "playbin is not a pipeline")?;
    let _guard = nd_core::pipeline::PipelineGuard::new(pipeline.clone());
    for property in ["video-sink", "audio-sink"] {
        let sink = gst::ElementFactory::make("fakesink")
            .build()
            .map_err(|err| err.to_string())?;
        pipeline.set_property(property, &sink);
    }
    pipeline.set_property_from_str("flags", "video+audio+force-sw-decoders");
    pipeline
        .set_state(gst::State::Paused)
        .map_err(|err| err.to_string())?;
    let (result, state, _) = pipeline.state(gst::ClockTime::from_seconds(3));
    result.map_err(|err| err.to_string())?;
    if state != gst::State::Paused {
        return Err("preview preroll timed out".into());
    }
    let duration = pipeline
        .query_duration::<gst::ClockTime>()
        .map(|duration| duration.seconds_f64());
    let tags = pipeline.emit_by_name::<Option<gst::TagList>>("get-audio-tags", &[&0i32]);
    let artist = tags
        .as_ref()
        .and_then(|tags| tags.get::<gst::tags::Artist>())
        .map(|tag| tag.get().to_string());
    let mut image = tags
        .as_ref()
        .and_then(|tags| {
            tags.get::<gst::tags::Image>()
                .map(|tag| tag.get().to_owned())
                .or_else(|| {
                    tags.get::<gst::tags::PreviewImage>()
                        .map(|tag| tag.get().to_owned())
                })
        })
        .and_then(|sample| Thumbnail::embedded(&sample));
    if kind == MediaKind::Video {
        // Seek beyond opening black frames; keep the preroll if seeking fails.
        if let Some(duration) = duration.filter(|duration| *duration > 1.0) {
            let _ = pipeline.seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_seconds_f64((duration * 0.1).min(5.0)),
            );
            let _ = pipeline.state(gst::ClockTime::from_seconds(2));
        }
        if let Some(sample) = pipeline.property::<Option<gst::Sample>>("sample") {
            if let Some(info) = sample
                .caps()
                .and_then(|caps| gstreamer_video::VideoInfo::from_caps(caps).ok())
            {
                let ratio = f64::from(info.width()) * f64::from(info.par().numer())
                    / (f64::from(info.height()) * f64::from(info.par().denom()));
                let (width, height) = if ratio >= 1.0 {
                    (SIZE, (f64::from(SIZE) / ratio).round().max(1.0) as i32)
                } else {
                    ((f64::from(SIZE) * ratio).round().max(1.0) as i32, SIZE)
                };
                let caps = gst::Caps::builder("video/x-raw")
                    .field("format", "RGB")
                    .field("width", width)
                    .field("height", height)
                    .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                    .build();
                if let Some(sample) =
                    pipeline.emit_by_name::<Option<gst::Sample>>("convert-sample", &[&caps])
                {
                    if let (Some(buffer), Some(info)) = (
                        sample.buffer(),
                        sample
                            .caps()
                            .and_then(|caps| gstreamer_video::VideoInfo::from_caps(caps).ok()),
                    ) {
                        if let Ok(map) = buffer.map_readable() {
                            image = Some(Thumbnail {
                                pixels: glib::Bytes::from_owned(map.as_slice().to_vec()),
                                width: info.width() as i32,
                                height: info.height() as i32,
                                stride: info.stride()[0],
                                alpha: false,
                            });
                        }
                    }
                }
            }
        }
    } else if image.is_none() {
        // Conventional album artwork beside the track; never scan recursively.
        if let Some(parent) = path.parent() {
            image = ["cover.jpg", "cover.png", "folder.jpg", "front.jpg"]
                .iter()
                .find_map(|name| Thumbnail::decode(&parent.join(name)).ok());
        }
    }
    Ok(Preview {
        image,
        duration,
        artist,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(crate) fn fixture(path: &Path, video: bool, cover: bool) {
        gst::init().unwrap();
        let description = if video {
            "videotestsrc num-buffers=60 pattern=ball ! video/x-raw,width=320,height=180,framerate=30/1 ! vp8enc deadline=1 ! webmmux ! filesink name=out"
        } else {
            "audiotestsrc num-buffers=100 ! audioconvert ! vorbisenc name=encoder ! oggmux ! filesink name=out"
        };
        let pipeline = gst::parse::launch(description)
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        let _guard = nd_core::pipeline::PipelineGuard::new(pipeline.clone());
        pipeline
            .by_name("out")
            .unwrap()
            .set_property("location", path.to_str().unwrap());
        if !video {
            let mut tags = gst::TagList::new();
            if cover {
                let pixbuf =
                    Pixbuf::new(relm4::gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 80, 80).unwrap();
                pixbuf.fill(0x238adfff);
                let data = pixbuf.save_to_bufferv("png", &[]).unwrap();
                let sample = gst::Sample::builder()
                    .buffer(&gst::Buffer::from_mut_slice(data))
                    .caps(&gst::Caps::builder("image/png").build())
                    .build();
                tags.get_mut()
                    .unwrap()
                    .add::<gst::tags::Image>(&sample, gst::TagMergeMode::Replace);
            }
            tags.get_mut()
                .unwrap()
                .add::<gst::tags::Artist>(&"Preview test artist", gst::TagMergeMode::Replace);
            pipeline
                .by_name("encoder")
                .unwrap()
                .downcast::<gst::TagSetter>()
                .unwrap()
                .merge_tags(&tags, gst::TagMergeMode::Replace);
        }
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .unwrap();
        assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");
    }

    #[test]
    fn video_preview_has_real_pixels_duration_and_original_aspect_ratio() {
        let path =
            std::env::temp_dir().join(format!("bns-video-preview-{}.webm", std::process::id()));
        fixture(&path, true, false);
        let preview = decode(&path, MediaKind::Video).unwrap();
        let image = preview.image.expect("decoded video frame");
        assert_eq!((image.width, image.height), (360, 203));
        assert!(preview.duration.is_some_and(|duration| duration > 1.0));
        assert!(image.pixels.iter().any(|pixel| *pixel > 50));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn music_preview_without_artwork_keeps_metadata() {
        // Isolate the track from adjacent covers in the system temp directory.
        let dir = std::env::temp_dir().join(format!("bns-audio-metadata-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("track.ogg");
        fixture(&path, false, false);
        let preview = decode(&path, MediaKind::Music).unwrap();
        assert!(preview.image.is_none());
        assert_eq!(preview.artist.as_deref(), Some("Preview test artist"));
        assert!(preview.duration.is_some_and(|duration| duration > 1.0));
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    #[ignore = "requires an image loader with a working sandbox (bwrap or Flatpak)"]
    fn music_preview_uses_embedded_cover_and_metadata() {
        let path =
            std::env::temp_dir().join(format!("bns-audio-preview-{}.ogg", std::process::id()));
        fixture(&path, false, true);
        let preview = decode(&path, MediaKind::Music).unwrap();
        let image = preview.image.expect("embedded album cover");
        assert_eq!(image.width, image.height);
        assert_eq!(&image.pixels[..3], &[0x23, 0x8a, 0xdf]);
        assert_eq!(preview.artist.as_deref(), Some("Preview test artist"));
        assert!(preview.duration.is_some_and(|duration| duration > 1.0));
        std::fs::remove_file(path).unwrap();
    }
}
