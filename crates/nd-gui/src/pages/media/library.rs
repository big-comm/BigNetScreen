//! Where the files on the media page come from: the standard folder for each
//! kind, and the files or folder picked in the desktop's chooser.
//!
//! Scanning and inspection read the disk, so the page runs them on blocking
//! threads; only [`open_file_dialog`] runs on the GTK thread.

use super::MediaMsg;
use crate::{tr, tr_n};
use nd_chromecast::file_server::{MediaFile, MediaKind};
use relm4::gtk::{self, glib, prelude::*};
use std::path::PathBuf;

/// The most files the grid shows, newest first.
pub(super) const GRID_LIMIT: usize = 60;

/// The playable files among `paths`, a folder standing for the files directly
/// in it, and a reason for each one refused. Stops at the queue's file limit.
pub(super) fn inspect_paths(paths: Vec<PathBuf>) -> (Vec<MediaFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut refused = Vec::new();
    for path in paths {
        let candidates = if path.is_dir() {
            let mut paths: Vec<_> = std::fs::read_dir(&path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_file())
                .take(64 * nd_core::media::MAX_FILES)
                .collect();
            // Sorted before the queue limit applies, so a large folder
            // contributes its first files by name rather than arbitrary ones.
            paths.sort();
            paths.truncate(nd_core::media::MAX_FILES + 1);
            paths
        } else {
            vec![path]
        };
        for candidate in candidates {
            if files.len() + refused.len() >= nd_core::media::MAX_FILES {
                refused.push(too_many_files());
                return (files, refused);
            }
            match MediaFile::inspect(&candidate) {
                Ok(file) => files.push(file),
                Err(reason) => refused.push(format!("{}: {reason}", candidate.display())),
            }
        }
    }
    (files, refused)
}

/// Why files past the queue's limit were left out.
pub(super) fn too_many_files() -> String {
    let limit = nd_core::media::MAX_FILES;
    tr_n!(
        "Select up to {} file at a time.",
        "Select up to {} files at a time.",
        limit
    )
    .replace("{}", &limit.to_string())
}

/// Lists the standard folder for this kind of file.
pub(super) fn scan_library(
    kind: MediaKind,
    cancel: &std::sync::atomic::AtomicBool,
) -> Vec<MediaFile> {
    let Some(directory) = user_directory(kind) else {
        return Vec::new();
    };
    let files: Vec<(std::time::SystemTime, MediaFile)> = std::fs::read_dir(&directory)
        .into_iter()
        .flatten()
        .flatten()
        .take_while(|_| !cancel.load(std::sync::atomic::Ordering::Relaxed))
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            let file = MediaFile::inspect(&path).ok()?;
            if file.kind != kind {
                return None;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            Some((modified, file))
        })
        // Newest first, keeping only what the grid shows: the photo taken
        // this afternoon is the one being shown to the room.
        .fold(Vec::with_capacity(GRID_LIMIT + 1), |mut newest, entry| {
            newest.push(entry);
            newest.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
            newest.truncate(GRID_LIMIT);
            newest
        });
    files.into_iter().map(|(_, file)| file).collect()
}

/// The XDG folder for this kind of file, falling back to the home directory.
pub(super) fn user_directory(kind: MediaKind) -> Option<PathBuf> {
    let directory = match kind {
        MediaKind::Photo => glib::user_special_dir(glib::UserDirectory::Pictures),
        MediaKind::Video => glib::user_special_dir(glib::UserDirectory::Videos),
        MediaKind::Music => glib::user_special_dir(glib::UserDirectory::Music),
    };
    directory.or_else(|| glib::home_dir().into())
}

/// Opens the desktop's file chooser.
pub(super) fn open_file_dialog(
    root: &impl IsA<gtk::Widget>,
    sender: relm4::Sender<MediaMsg>,
    folder: bool,
) {
    let window = root.root().and_downcast::<gtk::Window>();
    let dialog = gtk::FileDialog::builder()
        .title(if folder {
            tr!("Choose a folder")
        } else {
            tr!("Choose files")
        })
        .modal(true)
        .build();

    if folder {
        dialog.select_folder(
            window.as_ref(),
            gtk::gio::Cancellable::NONE,
            move |result| {
                if let Ok(file) = result
                    && let Some(path) = file.path()
                {
                    sender.emit(MediaMsg::Picked(vec![path]));
                }
            },
        );
    } else {
        dialog.open_multiple(
            window.as_ref(),
            gtk::gio::Cancellable::NONE,
            move |result| {
                if let Ok(files) = result {
                    let paths: Vec<PathBuf> = files
                        .into_iter()
                        .flatten()
                        .filter_map(|object| object.downcast::<gtk::gio::File>().ok())
                        .filter_map(|file| file.path())
                        .collect();
                    if !paths.is_empty() {
                        sender.emit(MediaMsg::Picked(paths));
                    }
                }
            },
        );
    }
}
