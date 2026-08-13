//! What kind of thing a media file is.
//!
//! Lives here, not in the Cast crate, because the answer decides how a file is
//! **played** on either protocol: a Chromecast is handed the file and decodes
//! it, a Miracast receiver is a screen and has to be shown a picture. A photo
//! has no sound, a song has no picture, and a film may have either missing —
//! each of which stalls a pipeline built for the other.

/// What a media file contains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Photo,
    Video,
    Music,
}

impl MediaKind {
    /// Does the file carry a picture of its own?
    ///
    /// A song does not, so something has to be drawn for the screen that is
    /// showing it.
    pub fn has_picture(self) -> bool {
        matches!(self, MediaKind::Photo | MediaKind::Video)
    }

    /// Does the picture stand still?
    ///
    /// A photo has exactly one frame, and a receiver expecting a video stream
    /// needs that frame repeated rather than sent once and never again.
    pub fn is_still(self) -> bool {
        self == MediaKind::Photo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_song_has_nothing_to_show() {
        assert!(!MediaKind::Music.has_picture());
        assert!(MediaKind::Video.has_picture());
        assert!(MediaKind::Photo.has_picture());
    }

    #[test]
    fn only_a_photo_stands_still() {
        assert!(MediaKind::Photo.is_still());
        assert!(!MediaKind::Video.is_still());
        assert!(!MediaKind::Music.is_still());
    }
}
