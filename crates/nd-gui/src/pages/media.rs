//! Sending photos, films and music to a receiver.
//!
//! This is not mirroring, and the difference is the whole point: the receiver
//! fetches the file and decodes it itself, so a film keeps its own quality and
//! this computer does nothing but serve bytes. It also means only a Chromecast
//! can do it — the media receiver app is a Cast feature, with no counterpart in
//! Miracast, which knows only how to be a screen.
//!
//! The grid lists what is in the standard Pictures, Videos and Music folders,
//! because that is where the files usually are; anything else is reachable
//! through "Select files". Files this receiver cannot play are not shown as
//! broken tiles — they are refused when picked, with the reason said out loud.

use std::path::PathBuf;

use relm4::adw::{self, prelude::*};
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender, FactoryVecDeque};
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::gdk_pixbuf::Pixbuf;
use relm4::gtk::glib;
use relm4::prelude::*;

use nd_chromecast::file_server::{MediaFile, MediaKind};
use nd_chromecast::media::MediaStatus;

use crate::tr;

/// How many files a tab shows.
///
/// A cap, not a page size: a picture folder of several thousand images would
/// spend seconds building tiles nobody scrolls to. The most recent ones are the
/// ones a person came here to send.
const GRID_LIMIT: usize = 60;

/// The size a thumbnail is decoded at.
///
/// Decoding at this size rather than shrinking afterwards is what keeps a grid
/// of photos from holding hundreds of megabytes of full-resolution pixels.
const THUMB: (i32, i32) = (260, 180);

// ----------------------------------------------------------------------------
// One tile
// ----------------------------------------------------------------------------

#[derive(Debug)]
pub struct MediaTile {
    file: MediaFile,
    selected: bool,
}

#[derive(Debug)]
pub enum TileMsg {
    Toggled(bool),
}

#[derive(Debug)]
pub enum TileOutput {
    /// This file was selected, or unselected.
    Selected(PathBuf, bool),
}

#[relm4::factory(pub)]
impl FactoryComponent for MediaTile {
    type Init = MediaFile;
    type Input = TileMsg;
    type Output = TileOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::FlowBox;

    view! {
        gtk::ToggleButton {
            set_css_classes: &["media-tile", "flat"],
            #[watch]
            set_active: self.selected,
            set_tooltip_text: Some(&self.file.path.to_string_lossy()),

            connect_toggled[sender] => move |button| {
                sender.input(TileMsg::Toggled(button.is_active()));
            },

            gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 2,

                #[name = "thumb"]
                gtk::Picture {
                    // Fills the tile and crops the overflow, so a portrait and
                    // a landscape photo make a tidy grid instead of one tall
                    // row.
                    set_content_fit: relm4::gtk::ContentFit::Cover,
                    set_width_request: 150,
                    set_height_request: 104,
                    add_css_class: "thumb",
                },

                gtk::Label {
                    set_label: &self.file.title(),
                    set_ellipsize: gtk::pango::EllipsizeMode::Middle,
                    set_max_width_chars: 16,
                    add_css_class: "media-name",
                },
            },
        }
    }

    fn init_model(file: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self {
            file,
            selected: false,
        }
    }

    fn init_widgets(
        &mut self,
        _index: &DynamicIndex,
        root: Self::Root,
        _returned: &<Self::ParentWidget as relm4::factory::FactoryView>::ReturnedWidget,
        sender: FactorySender<Self>,
    ) -> Self::Widgets {
        let widgets = view_output!();

        // The picture is filled in on an idle callback rather than here.
        // Decoding sixty photos while building the grid freezes the window for
        // as long as it takes; one per idle turn keeps it answering the pointer,
        // and the tiles fill in visibly from the top.
        let path = self.file.path.clone();
        let kind = self.file.kind;
        let picture = widgets.thumb.clone();
        glib::idle_add_local_once(move || match kind {
            MediaKind::Photo => {
                match Pixbuf::from_file_at_scale(&path, THUMB.0, THUMB.1, true) {
                    Ok(pixbuf) => picture.set_paintable(Some(&gdk::Texture::for_pixbuf(&pixbuf))),
                    // A file that cannot be decoded here is one the receiver is
                    // unlikely to play either, but that is its decision to make:
                    // the tile falls back to an icon instead of vanishing.
                    Err(err) => {
                        tracing::debug!(path = %path.display(), %err, "no thumbnail");
                        picture.set_paintable(icon_paintable("image-x-generic-symbolic").as_ref());
                    }
                }
            }
            MediaKind::Video => {
                picture.set_paintable(icon_paintable("video-x-generic-symbolic").as_ref())
            }
            MediaKind::Music => {
                picture.set_paintable(icon_paintable("audio-x-generic-symbolic").as_ref())
            }
        });

        widgets
    }

    fn update(&mut self, message: Self::Input, sender: FactorySender<Self>) {
        match message {
            TileMsg::Toggled(selected) => {
                self.selected = selected;
                sender
                    .output(TileOutput::Selected(self.file.path.clone(), selected))
                    .ok();
            }
        }
    }
}

/// An icon, sized for a tile, for files that have no picture to show.
fn icon_paintable(name: &str) -> Option<gdk::Paintable> {
    let display = gdk::Display::default()?;
    let theme = gtk::IconTheme::for_display(&display);
    Some(
        theme
            .lookup_icon(
                name,
                &[],
                64,
                1,
                gtk::TextDirection::None,
                gtk::IconLookupFlags::empty(),
            )
            .upcast(),
    )
}

// ----------------------------------------------------------------------------
// The page
// ----------------------------------------------------------------------------

pub struct MediaPage {
    tiles: FactoryVecDeque<MediaTile>,
    kind: MediaKind,
    /// What the grid is showing.
    listed: Vec<MediaFile>,
    /// Files chosen, in the order they were chosen.
    chosen: Vec<MediaFile>,
    /// Files that were picked and cannot be played, with the reason.
    refused: Vec<String>,
    /// The receivers that can play a file: (id, name).
    targets: Vec<(String, String)>,
    /// Set while the chooser is being rebuilt, so that the selection signal it
    /// emits on the way is not read as the person picking something.
    settling: bool,
    /// The one chosen to send to. Never filled in automatically — sending to
    /// the wrong device is visible in someone else's room and cannot be undone
    /// from here.
    chosen_target: Option<String>,
    status: Option<MediaStatus>,
}

#[derive(Debug)]
pub enum MediaMsg {
    SetKind(MediaKind),
    /// A tile was selected or unselected.
    Selected(PathBuf, bool),
    /// Files chosen in the file dialog.
    Picked(Vec<PathBuf>),
    PickFiles,
    PickFolder,
    Send,
    Cancel,
    /// The receivers that can play a file: (id, name).
    Targets(Vec<(String, String)>),
    /// Select this receiver (chosen on the devices page).
    Choose(String),
    /// A destination was picked in the list, by position.
    TargetPicked(usize),
    Status(Option<MediaStatus>),
}

#[derive(Debug)]
pub enum MediaOutput {
    /// Send these files to the receiver with this id.
    Send(Vec<MediaFile>, String),
    Cancel,
}

#[relm4::component(pub)]
impl Component for MediaPage {
    type Init = ();
    type Input = MediaMsg;
    type Output = MediaOutput;
    type CommandOutput = ();

    view! {
        gtk::Box {
            set_orientation: gtk::Orientation::Vertical,
            set_margin_all: 24,
            set_spacing: 6,

            gtk::Label {
                set_label: &tr!("Share media"),
                set_xalign: 0.0,
                add_css_class: "page-title",
            },
            gtk::Label {
                set_label: &tr!("Pick files and choose where to play them."),
                set_xalign: 0.0,
                set_margin_bottom: 18,
                add_css_class: "page-subtitle",
            },

            gtk::Box {
                set_spacing: 8,

                #[name = "kind_group"]
                adw::ToggleGroup {
                    connect_active_notify[sender] => move |group| {
                        sender.input(MediaMsg::SetKind(match group.active() {
                            1 => MediaKind::Video,
                            2 => MediaKind::Music,
                            _ => MediaKind::Photo,
                        }));
                    },
                },

                gtk::Box { set_hexpand: true },

                gtk::Button {
                    connect_clicked => MediaMsg::PickFiles,
                    adw::ButtonContent {
                        set_icon_name: "document-open-symbolic",
                        set_label: &tr!("Select files"),
                    },
                },
                gtk::Button {
                    connect_clicked => MediaMsg::PickFolder,
                    adw::ButtonContent {
                        set_icon_name: "folder-symbolic",
                        set_label: &tr!("Send a folder"),
                    },
                },
            },

            gtk::ScrolledWindow {
                set_vexpand: true,
                set_margin_top: 12,
                set_hscrollbar_policy: gtk::PolicyType::Never,

                #[local_ref]
                grid -> gtk::FlowBox {
                    set_selection_mode: gtk::SelectionMode::None,
                    set_valign: gtk::Align::Start,
                    set_column_spacing: 10,
                    set_row_spacing: 10,
                    set_homogeneous: true,
                    set_max_children_per_line: 8,
                },
            },

            gtk::Label {
                #[watch]
                set_label: &model.empty_message(),
                #[watch]
                set_visible: model.listed.is_empty(),
                set_wrap: true,
                set_margin_top: 24,
                set_margin_bottom: 24,
                add_css_class: "dim-label",
            },

            // What was refused and why. Silence here would leave a person
            // wondering why the file they picked never appeared.
            gtk::Label {
                #[watch]
                set_label: &model.refused.join("\n"),
                #[watch]
                set_visible: !model.refused.is_empty(),
                set_xalign: 0.0,
                set_wrap: true,
                set_margin_top: 8,
                add_css_class: "warning",
            },

            gtk::Box {
                set_spacing: 12,
                set_margin_top: 12,
                add_css_class: "action-bar",

                gtk::Label {
                    #[watch]
                    set_label: &model.selection_summary(),
                    set_hexpand: true,
                    set_xalign: 0.0,
                },
                gtk::Label {
                    #[watch]
                    set_label: &model.selection_size(),
                    add_css_class: "dim-label",
                },

                gtk::Label {
                    set_label: &tr!("Send to"),
                    add_css_class: "dim-label",
                },
                #[name = "target_chooser"]
                gtk::DropDown {
                    set_valign: gtk::Align::Center,
                    // Its contents are filled in from the receivers found, so
                    // it is built empty here.
                    connect_selected_notify[sender] => move |chooser| {
                        sender.input(MediaMsg::TargetPicked(chooser.selected() as usize));
                    },
                },

                gtk::Button {
                    add_css_class: "suggested-action",
                    add_css_class: "pill",
                    // Both conditions, and no default for the second: a file
                    // is only ever sent to a device someone pointed at.
                    #[watch]
                    set_sensitive: !model.chosen.is_empty()
                        && model.chosen_target.is_some(),
                    connect_clicked => MediaMsg::Send,

                    adw::ButtonContent {
                        set_icon_name: "document-send-symbolic",
                        set_label: &tr!("Send to device"),
                    },
                },
            },

            gtk::Label {
                #[watch]
                set_label: &model.destination_hint(),
                #[watch]
                set_visible: !model.destination_hint().is_empty(),
                set_xalign: 0.0,
                set_margin_top: 6,
                add_css_class: "dim-label",
            },

            // The queue, while something is playing.
            gtk::Box {
                set_spacing: 12,
                set_margin_top: 12,
                add_css_class: "info-card",
                #[watch]
                set_visible: model.status.is_some(),

                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_hexpand: true,

                    gtk::Label {
                        set_label: &tr!("Now playing"),
                        set_xalign: 0.0,
                        add_css_class: "heading",
                    },
                    gtk::Label {
                        #[watch]
                        set_label: &model.queue_line(),
                        set_xalign: 0.0,
                        set_wrap: true,
                        add_css_class: "dim-label",
                    },
                },
                gtk::Button {
                    set_icon_name: "window-close-symbolic",
                    set_valign: gtk::Align::Center,
                    set_tooltip_text: Some(&tr!("Stop sending")),
                    connect_clicked => MediaMsg::Cancel,
                },
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let tiles =
            FactoryVecDeque::builder()
                .launch_default()
                .forward(sender.input_sender(), |output| match output {
                    TileOutput::Selected(path, selected) => MediaMsg::Selected(path, selected),
                });

        let mut model = MediaPage {
            tiles,
            kind: MediaKind::Photo,
            listed: Vec::new(),
            chosen: Vec::new(),
            refused: Vec::new(),
            targets: Vec::new(),
            settling: false,
            chosen_target: None,
            status: None,
        };

        let grid = model.tiles.widget();
        let widgets = view_output!();

        for label in [tr!("Photos"), tr!("Videos"), tr!("Music")] {
            widgets
                .kind_group
                .add(adw::Toggle::builder().label(&label).build());
        }
        widgets.kind_group.set_active(0);
        model.reload();

        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match message {
            MediaMsg::SetKind(kind) => {
                if kind != self.kind {
                    self.kind = kind;
                    self.reload();
                }
            }
            MediaMsg::Selected(path, selected) => {
                if selected {
                    if let Some(file) = self.listed.iter().find(|f| f.path == path) {
                        if !self.chosen.iter().any(|f| f.path == path) {
                            self.chosen.push(file.clone());
                        }
                    }
                } else {
                    self.chosen.retain(|f| f.path != path);
                }
            }
            MediaMsg::Picked(paths) => self.add_paths(paths),
            MediaMsg::PickFiles => open_file_dialog(root, sender.input_sender().clone(), false),
            MediaMsg::PickFolder => open_file_dialog(root, sender.input_sender().clone(), true),
            MediaMsg::Send => {
                if let Some(target) = self.chosen_target.clone() {
                    if !self.chosen.is_empty() {
                        sender
                            .output(MediaOutput::Send(self.chosen.clone(), target))
                            .ok();
                    }
                }
            }
            MediaMsg::Cancel => {
                sender.output(MediaOutput::Cancel).ok();
            }
            MediaMsg::Targets(targets) => {
                if targets != self.targets {
                    self.targets = targets;
                    // A receiver that has gone off the network cannot stay
                    // selected: the send button would point at nothing.
                    if let Some(chosen) = &self.chosen_target {
                        if !self.targets.iter().any(|(id, _)| id == chosen) {
                            self.chosen_target = None;
                        }
                    }
                    self.fill_chooser(widgets);
                }
            }
            MediaMsg::Choose(id) => {
                if self.targets.iter().any(|(listed, _)| *listed == id) {
                    self.chosen_target = Some(id);
                    self.fill_chooser(widgets);
                }
            }
            MediaMsg::TargetPicked(index) => {
                if self.settling {
                    return;
                }
                // Index 0 is the "choose a device" placeholder, which is not a
                // device — picking it means nothing is selected.
                self.chosen_target = index
                    .checked_sub(1)
                    .and_then(|index| self.targets.get(index))
                    .map(|(id, _)| id.clone());
            }
            MediaMsg::Status(status) => self.status = status,
        }
        self.update_view(widgets, sender);
    }
}

impl MediaPage {
    /// Rebuilds the list of destinations, keeping what was chosen selected.
    ///
    /// The first entry is a placeholder rather than a device, so that a page
    /// which has just opened is not silently pointing at somebody's television.
    fn fill_chooser(&mut self, widgets: &MediaPageWidgets) {
        self.settling = true;
        let list = gtk::StringList::new(&[]);
        list.append(&tr!("Choose a device…"));
        for (_, name) in &self.targets {
            list.append(name);
        }
        widgets.target_chooser.set_model(Some(&list));

        let selected = self
            .chosen_target
            .as_ref()
            .and_then(|chosen| self.targets.iter().position(|(id, _)| id == chosen))
            .map(|index| index as u32 + 1)
            .unwrap_or(0);
        widgets.target_chooser.set_selected(selected);
        widgets
            .target_chooser
            .set_sensitive(!self.targets.is_empty());
        self.settling = false;
    }

    /// What is missing before anything can be sent.
    fn destination_hint(&self) -> String {
        if self.targets.is_empty() {
            return tr!("No device found yet. Turn your TV, projector or Chromecast on.");
        }
        if self.chosen_target.is_none() && !self.chosen.is_empty() {
            return tr!("Choose which device to send to.");
        }
        String::new()
    }

    /// Fills the grid with what is in the folder for the current tab.
    fn reload(&mut self) {
        self.listed = scan_library(self.kind);
        // Selections from another tab stay chosen — a person may well send a
        // song and a film together — so the grid is rebuilt but `chosen` is not
        // touched.
        let mut guard = self.tiles.guard();
        guard.clear();
        for file in &self.listed {
            guard.push_back(file.clone());
        }
    }

    /// Adds files picked by hand, refusing what cannot be played.
    fn add_paths(&mut self, paths: Vec<PathBuf>) {
        self.refused.clear();
        for path in paths {
            // A folder was picked: take the files inside it, one level deep.
            // Recursing without being asked would queue a whole photo archive
            // from a single click on its parent.
            let candidates: Vec<PathBuf> = if path.is_dir() {
                let mut entries: Vec<PathBuf> = std::fs::read_dir(&path)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file())
                    .collect();
                entries.sort();
                entries
            } else {
                vec![path]
            };

            for candidate in candidates {
                match MediaFile::inspect(&candidate) {
                    Ok(file) => {
                        if !self.chosen.iter().any(|f| f.path == file.path) {
                            self.chosen.push(file.clone());
                        }
                        if !self.listed.iter().any(|f| f.path == file.path) {
                            self.listed.push(file.clone());
                            self.tiles.guard().push_back(file);
                        }
                    }
                    Err(reason) => {
                        let name = candidate
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        // One line per problem file, not a single "some files
                        // were skipped": the person needs to know which.
                        let line = format!("{name}: {reason}");
                        if !self.refused.contains(&line) {
                            self.refused.push(line);
                        }
                    }
                }
            }
        }
    }

    fn empty_message(&self) -> String {
        match self.kind {
            MediaKind::Photo => tr!("No photos in your Pictures folder. Use “Select files”."),
            MediaKind::Video => tr!("No videos in your Videos folder. Use “Select files”."),
            MediaKind::Music => tr!("No music in your Music folder. Use “Select files”."),
        }
    }

    fn selection_summary(&self) -> String {
        match self.chosen.len() {
            0 => tr!("Nothing selected"),
            1 => tr!("1 file selected"),
            n => format!("{n} {}", tr!("files selected")),
        }
    }

    fn selection_size(&self) -> String {
        let bytes: u64 = self.chosen.iter().map(|f| f.size).sum();
        if bytes == 0 {
            return String::new();
        }
        human_size(bytes)
    }

    fn queue_line(&self) -> String {
        let Some(status) = &self.status else {
            return String::new();
        };
        if let Some(error) = &status.error {
            return error.clone();
        }
        if status.finished {
            return tr!("Finished");
        }
        format!(
            "{} · {} {}/{}",
            status.title,
            tr!("item"),
            status.position,
            status.total
        )
    }
}

/// Lists the standard folder for this kind of file.
fn scan_library(kind: MediaKind) -> Vec<MediaFile> {
    let Some(directory) = user_directory(kind) else {
        return Vec::new();
    };
    let mut files: Vec<(std::time::SystemTime, MediaFile)> = std::fs::read_dir(&directory)
        .into_iter()
        .flatten()
        .flatten()
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
        .collect();

    // Newest first: the photo taken this afternoon is the one being shown to
    // the room, not the oldest file in the folder.
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    files.into_iter().take(GRID_LIMIT).map(|(_, f)| f).collect()
}

/// The XDG folder for this kind of file, falling back to the home directory.
fn user_directory(kind: MediaKind) -> Option<PathBuf> {
    let directory = match kind {
        MediaKind::Photo => glib::user_special_dir(glib::UserDirectory::Pictures),
        MediaKind::Video => glib::user_special_dir(glib::UserDirectory::Videos),
        MediaKind::Music => glib::user_special_dir(glib::UserDirectory::Music),
    };
    directory.or_else(|| glib::home_dir().into())
}

/// Opens the desktop's file chooser.
fn open_file_dialog(root: &gtk::Box, sender: relm4::Sender<MediaMsg>, folder: bool) {
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
                if let Ok(file) = result {
                    if let Some(path) = file.path() {
                        sender.emit(MediaMsg::Picked(vec![path]));
                    }
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

/// A size a person can read.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chooser's positions: 0 is the placeholder, devices start at 1.
    fn target_at(targets: &[(String, String)], index: usize) -> Option<String> {
        index
            .checked_sub(1)
            .and_then(|index| targets.get(index))
            .map(|(id, _): &(String, String)| id.clone())
    }

    #[test]
    fn nothing_is_selected_until_someone_selects_it() {
        // The bug this pins down: with no destination chosen, the application
        // fell back to the first Chromecast it had found and played a song on
        // a projector in another room. Position 0 is a placeholder, and a
        // placeholder is not a device.
        let targets = vec![
            ("id-a".to_string(), "Living room".to_string()),
            ("id-b".to_string(), "Mum's projector".to_string()),
        ];
        assert_eq!(
            target_at(&targets, 0),
            None,
            "the placeholder sends nowhere"
        );
        assert_eq!(target_at(&targets, 1).as_deref(), Some("id-a"));
        assert_eq!(target_at(&targets, 2).as_deref(), Some("id-b"));
        // A position past the end selects nothing rather than the last device.
        assert_eq!(target_at(&targets, 3), None);
        assert_eq!(target_at(&[], 1), None);
    }

    #[test]
    fn sizes_read_the_way_a_person_expects() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(47_500_000), "45.3 MB");
        // Bytes are never shown with a decimal point: "512.0 B" is noise.
        assert!(!human_size(999).contains('.'));
    }

    #[test]
    fn the_largest_unit_is_not_exceeded() {
        // Without the bound on `unit`, a large enough number would index past
        // the end of the table.
        assert!(human_size(u64::MAX).ends_with("GB"));
    }
}
