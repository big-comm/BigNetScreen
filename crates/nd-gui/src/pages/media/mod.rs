//! Sending photos, films and music to a receiver.
//!
//! The newest files of one kind from the standard folder ([`library`]) as a
//! grid of [`tile`]s, and the files chosen as a queue. It sits under the
//! receivers on the share page, whose Share buttons send the queue
//! ([`MediaMsg::SendTo`]).

mod library;
mod queue;
mod tile;

use super::preview::Preview;
use crate::{tr, tr_n};
use library::{GRID_LIMIT, inspect_paths, open_file_dialog, scan_library};
use nd_chromecast::file_server::{MediaFile, MediaKind};
use nd_chromecast::media::MediaStatus;
use nd_core::media::MediaCommand;
use queue::QueueList;
use relm4::adw::{self, prelude::*};
use relm4::factory::FactoryVecDeque;
use relm4::gtk;
use relm4::prelude::*;
use std::path::PathBuf;
use tile::{MediaTile, TileMsg, TileOutput, format_time};

pub struct MediaPage {
    tiles: FactoryVecDeque<MediaTile>,
    kind: MediaKind,
    /// What the grid is showing.
    listed: Vec<MediaFile>,
    /// A folder scan is running, so an empty grid means nothing yet.
    scanning: bool,
    /// Files chosen, in the order they were chosen.
    chosen: Vec<MediaFile>,
    /// Files that were picked and cannot be played, with the reason.
    refused: Vec<String>,
    status: Option<MediaStatus>,
    scan_generation: u64,
    inspection_generation: u64,
    queue: QueueList,
    /// What the root component was last told about [`Self::chosen`].
    announced_chosen: bool,
    scan_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug)]
pub enum MediaMsg {
    EnsureLoaded,
    SetKind(MediaKind),
    /// A tile was selected or unselected.
    Selected(PathBuf, bool),
    /// Files chosen in the file dialog.
    Picked(Vec<PathBuf>),
    PickFiles,
    PickFolder,
    /// Send the queue to the receiver with this id, which the person picked.
    SendTo(String),
    Cancel,
    Status(Option<MediaStatus>),
    Scanned(u64, Vec<MediaFile>),
    Inspected(u64, Vec<MediaFile>, Vec<String>),
    Remove(PathBuf),
    Clear,
    Control(MediaCommand),
}

#[derive(Debug)]
pub enum MediaOutput {
    /// Send these files to the receiver with this id.
    Send(Vec<MediaFile>, String),
    /// Whether any file is chosen, so a receiver can be offered.
    Chosen(bool),
    Cancel,
    Control(MediaCommand),
}

#[relm4::component(pub)]
impl Component for MediaPage {
    type Init = ();
    type Input = MediaMsg;
    type Output = MediaOutput;
    type CommandOutput = (PathBuf, Result<Preview, String>);

    view! {
        gtk::Box {
            set_orientation: gtk::Orientation::Vertical,
            set_spacing: 6,

            // Wraps onto a second line in a narrow window.
            adw::WrapBox {
                add_css_class: "media-toolbar",
                set_child_spacing: 8,
                set_line_spacing: 8,

                #[name = "kind_group"]
                append = &adw::ToggleGroup {
                    connect_active_notify[sender] => move |group| {
                        sender.input(MediaMsg::SetKind(match group.active() {
                            1 => MediaKind::Video,
                            2 => MediaKind::Music,
                            _ => MediaKind::Photo,
                        }));
                    },
                },

                append = &gtk::Button {
                    #[watch]
                    set_sensitive: !model.playing(),
                    connect_clicked => MediaMsg::PickFiles,
                    adw::ButtonContent {
                        set_icon_name: "document-open-symbolic",
                        set_label: &tr!("Select files"),
                    },
                },
                append = &gtk::Button {
                    #[watch]
                    set_sensitive: !model.playing(),
                    connect_clicked => MediaMsg::PickFolder,
                    adw::ButtonContent {
                        set_icon_name: "folder-symbolic",
                        set_label: &tr!("Add a folder"),
                    },
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

                #[local_ref]
                grid -> gtk::FlowBox {
                    set_margin_top: 12,
                    #[watch]
                    set_sensitive: !model.playing(),
                    set_selection_mode: gtk::SelectionMode::None,
                    set_valign: gtk::Align::Start,
                    set_column_spacing: 10,
                    set_row_spacing: 10,
                    set_homogeneous: true,
                    set_max_children_per_line: 6,
                    set_min_children_per_line: 1,
                    add_css_class: "media-grid",
                },

            gtk::Box {
                set_spacing: 8,
                #[watch]
                set_visible: !model.chosen.is_empty(),
                gtk::Label {
                    set_label: &tr!("Queue"),
                    set_xalign: 0.0,
                    set_hexpand: true,
                    add_css_class: "heading",
                },
                gtk::Button {
                    #[watch]
                    set_label: &if model.playing() {
                        tr!("Stop and clear queue")
                    } else {
                        tr!("Clear queue")
                    },
                    connect_clicked => MediaMsg::Clear,
                },
            },
            #[name = "queue_list"]
            gtk::ListBox {
                #[watch]
                set_visible: !model.chosen.is_empty(),
                set_selection_mode: gtk::SelectionMode::None,
                add_css_class: "boxed-list",
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

            gtk::Label {
                #[watch]
                set_label: &model.selection_line(),
                set_xalign: 0.0,
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
                    set_icon_name: "media-seek-backward-symbolic",
                    set_tooltip_text: Some(&tr!("Back 10 seconds")),
                    #[watch]
                    set_sensitive: model.status.as_ref().is_some_and(|s| !s.finished && s.playback.can_seek),
                    connect_clicked => MediaMsg::Control(MediaCommand::SeekRelative(-10.0)),
                },
                gtk::Button {
                    #[watch]
                    set_icon_name: if model.status.as_ref().is_some_and(|s| s.playback.paused) { "media-playback-start-symbolic" } else { "media-playback-pause-symbolic" },
                    #[watch]
                    set_tooltip_text: Some(&if model.status.as_ref().is_some_and(|s| s.playback.paused) { tr!("Resume") } else { tr!("Pause") }),
                    #[watch]
                    set_sensitive: model.status.as_ref().is_some_and(|s| !s.finished && s.playback.can_pause),
                    connect_clicked => MediaMsg::Control(MediaCommand::TogglePause),
                },
                gtk::Button {
                    set_icon_name: "media-seek-forward-symbolic",
                    set_tooltip_text: Some(&tr!("Forward 10 seconds")),
                    #[watch]
                    set_sensitive: model.status.as_ref().is_some_and(|s| !s.finished && s.playback.can_seek),
                    connect_clicked => MediaMsg::Control(MediaCommand::SeekRelative(10.0)),
                },
                gtk::Button {
                    set_icon_name: "media-skip-forward-symbolic",
                    set_tooltip_text: Some(&tr!("Next file")),
                    #[watch]
                    set_sensitive: model.playing(),
                    connect_clicked => MediaMsg::Control(MediaCommand::Next),
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

        let model = MediaPage {
            tiles,
            kind: MediaKind::Photo,
            listed: Vec::new(),
            scanning: false,
            chosen: Vec::new(),
            refused: Vec::new(),
            status: None,
            scan_generation: 0,
            inspection_generation: 0,
            queue: QueueList::new(),
            announced_chosen: false,
            scan_cancel: Default::default(),
        };

        let grid = model.tiles.widget();
        let widgets = view_output!();

        for label in [tr!("Photos"), tr!("Videos"), tr!("Music")] {
            widgets
                .kind_group
                .add(adw::Toggle::builder().label(&label).build());
        }
        widgets.kind_group.set_active(0);
        root.connect_map(move |_| sender.input(MediaMsg::EnsureLoaded));

        ComponentParts { model, widgets }
    }

    fn update_cmd_with_view(
        &mut self,
        _widgets: &mut Self::Widgets,
        (path, preview): Self::CommandOutput,
        _sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        self.queue.show_preview(&path, preview);
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match message {
            MediaMsg::EnsureLoaded => {
                if self.scan_generation == 0 {
                    self.reload(&sender);
                }
            }
            MediaMsg::SetKind(kind) => {
                if kind != self.kind {
                    self.kind = kind;
                    self.reload(&sender);
                }
            }
            MediaMsg::Selected(path, selected) => {
                if self.playing() {
                    return;
                }
                if selected {
                    if let Some(file) = self.listed.iter().find(|f| f.path == path)
                        && !self.chosen.iter().any(|f| f.path == path)
                    {
                        self.chosen.push(file.clone());
                    }
                } else {
                    self.chosen.retain(|f| f.path != path);
                }
            }
            MediaMsg::Picked(paths) => {
                if self.playing() {
                    return;
                }
                let generation = self.inspection_generation;
                let out = sender.input_sender().clone();
                relm4::spawn(async move {
                    if let Ok((files, refused)) =
                        tokio::task::spawn_blocking(move || inspect_paths(paths)).await
                    {
                        let _ = out.send(MediaMsg::Inspected(generation, files, refused));
                    }
                });
            }
            MediaMsg::Inspected(generation, files, refused) => {
                if generation != self.inspection_generation || self.playing() {
                    return;
                }
                self.refused = refused;
                for file in files {
                    if !self.chosen.iter().any(|f| f.path == file.path) {
                        if self.chosen.len() >= nd_core::media::MAX_FILES {
                            self.refused.push(library::too_many_files());
                            break;
                        }
                        self.chosen.push(file.clone());
                    }
                    if self.listed.len() < GRID_LIMIT
                        && !self.listed.iter().any(|f| f.path == file.path)
                    {
                        self.listed.push(file.clone());
                        self.tiles.guard().push_back(file);
                    }
                }
            }
            MediaMsg::Scanned(generation, files) => {
                if generation == self.scan_generation {
                    self.scanning = false;
                    self.listed = files;
                    let mut guard = self.tiles.guard();
                    guard.clear();
                    for file in &self.listed {
                        guard.push_back(file.clone());
                    }
                }
            }
            MediaMsg::PickFiles => open_file_dialog(root, sender.input_sender().clone(), false),
            MediaMsg::PickFolder => open_file_dialog(root, sender.input_sender().clone(), true),
            MediaMsg::SendTo(target) => {
                if self.playing() || self.chosen.is_empty() {
                    return;
                }
                self.inspection_generation += 1;
                sender
                    .output(MediaOutput::Send(self.chosen.clone(), target))
                    .ok();
            }
            MediaMsg::Cancel => {
                sender.output(MediaOutput::Cancel).ok();
            }
            MediaMsg::Remove(path) => {
                self.chosen.retain(|file| file.path != path);
                if self.playing() {
                    sender
                        .output(MediaOutput::Control(MediaCommand::Remove(path)))
                        .ok();
                }
            }
            MediaMsg::Clear => {
                self.inspection_generation += 1;
                self.chosen.clear();
                self.refused.clear();
                if self.playing() {
                    sender.output(MediaOutput::Cancel).ok();
                }
            }
            MediaMsg::Control(command) => {
                if self.playing() {
                    sender.output(MediaOutput::Control(command)).ok();
                }
            }
            MediaMsg::Status(status) => {
                if let Some(status) = &status {
                    if !status.finished {
                        self.chosen.retain(|file| status.queue.contains(&file.path));
                    }
                } else if self.playing() {
                    self.chosen.clear();
                }
                self.status = status;
            }
        }
        self.sync_selection(widgets, &sender);
        self.update_view(widgets, sender);
    }
}

impl MediaPage {
    fn playing(&self) -> bool {
        self.status.as_ref().is_some_and(|status| !status.finished)
    }

    fn sync_selection(&mut self, widgets: &MediaPageWidgets, sender: &ComponentSender<Self>) {
        let chosen = !self.chosen.is_empty();
        if chosen != self.announced_chosen {
            self.announced_chosen = chosen;
            sender.output(MediaOutput::Chosen(chosen)).ok();
        }
        for (index, file) in self.listed.iter().enumerate() {
            let selected = self.chosen.iter().any(|chosen| chosen.path == file.path);
            if self
                .tiles
                .get(index)
                .is_some_and(|tile| tile.selected != selected)
            {
                self.tiles.send(index, TileMsg::SetSelected(selected));
            }
        }
        self.queue.render(&widgets.queue_list, &self.chosen, sender);
    }

    /// Fills the grid with what is in the folder for the current tab.
    fn reload(&mut self, sender: &ComponentSender<Self>) {
        self.scan_cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.scan_cancel = Default::default();
        self.scan_generation += 1;
        let generation = self.scan_generation;
        self.scanning = true;
        let kind = self.kind;
        let cancel = self.scan_cancel.clone();
        let out = sender.input_sender().clone();
        relm4::spawn(async move {
            if let Ok(files) =
                tokio::task::spawn_blocking(move || scan_library(kind, &cancel)).await
            {
                let _ = out.send(MediaMsg::Scanned(generation, files));
            }
        });
    }

    fn empty_message(&self) -> String {
        if self.scanning {
            return tr!("Looking for files…");
        }
        let folder = library::user_directory(self.kind)
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        match self.kind {
            MediaKind::Photo => tr!("No photos in {}. Use “Select files” to pick them elsewhere."),
            MediaKind::Video => tr!("No videos in {}. Use “Select files” to pick them elsewhere."),
            MediaKind::Music => tr!("No music in {}. Use “Select files” to pick them elsewhere."),
        }
        .replace("{}", &folder)
    }

    /// What is chosen, and what to do next with it.
    fn selection_line(&self) -> String {
        let bytes: u64 = self.chosen.iter().map(|f| f.size).sum();
        match self.chosen.len() {
            0 => tr!("Select files, then choose Share on a device above."),
            n => {
                let count =
                    tr_n!("{} file selected", "{} files selected", n).replace("{}", &n.to_string());
                if bytes == 0 {
                    count
                } else {
                    format!("{count} · {}", gtk::glib::format_size(bytes))
                }
            }
        }
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
        let time = format_time(status.playback.seconds);
        let duration = status
            .playback
            .duration
            .map(format_time)
            .unwrap_or_else(|| "—".into());
        let progress = format!("{time} / {duration}");
        let position = tr!("Item {position} of {total}")
            .replace("{position}", &status.position.to_string())
            .replace("{total}", &status.total.to_string());
        let line = format!("{} · {position} · {progress}", status.title);
        match status.control_error.as_deref() {
            Some(error) if !error.is_empty() => format!("{line}\n{error}"),
            _ => line,
        }
    }
}

impl Drop for MediaPage {
    fn drop(&mut self) {
        self.scan_cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::preview;
    use gtk::{gdk, gdk_pixbuf::Pixbuf, glib};

    #[test]
    #[ignore = "requires a graphical GTK session and image loader"]
    fn lazy_library_thumbnails_keep_pixels() {
        adw::init().expect("GTK session");
        relm4::set_global_css(include_str!("../../style.css"));
        let context = glib::MainContext::default();
        let page = MediaPage::builder().launch(()).detach();
        context.block_on(glib::timeout_future(std::time::Duration::from_millis(50)));
        assert_eq!(page.model().scan_generation, 0);
        assert!(page.model().listed.is_empty());

        let path =
            std::env::temp_dir().join(format!("bignetscreen-thumbnail-{}.png", std::process::id()));
        let pixbuf = Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, true, 8, 800, 400)
            .expect("fixture pixels");
        pixbuf.fill(0x12345680);
        pixbuf.savev(&path, "png", &[]).expect("fixture PNG");
        context.block_on(async {
            let permits = preview::SLOTS
                .acquire_many(2)
                .await
                .expect("thumbnail slots");
            let job = relm4::spawn(preview::load(path.clone(), MediaKind::Photo));
            glib::timeout_future(std::time::Duration::from_millis(50)).await;
            assert!(!job.is_finished(), "decoder concurrency must be bounded");
            drop(permits);
            let thumbnail = job
                .await
                .expect("worker completed")
                .expect("thumbnail decoded")
                .image
                .expect("photo preview");
            assert_eq!((thumbnail.width, thumbnail.height), (360, 180));
            assert!(thumbnail.alpha);
            assert_eq!(&thumbnail.pixels.as_ref()[..4], &[0x12, 0x34, 0x56, 0x80]);
            let texture = thumbnail.texture(360);
            let queue_texture = thumbnail.texture(48);
            assert!(queue_texture.width() <= 48 && queue_texture.height() <= 48);
            assert_eq!((texture.width(), texture.height()), (360, 180));

            std::fs::write(&path, b"invalid image").expect("invalid fixture");
            assert!(
                relm4::spawn(preview::load(path.clone(), MediaKind::Photo))
                    .await
                    .expect("worker completed")
                    .is_err()
            );
        });
        std::fs::remove_file(path).expect("remove fixture");

        let window = gtk::Window::new();
        window.set_child(Some(page.widget()));
        window.present();
        context.block_on(glib::timeout_future(std::time::Duration::from_millis(50)));
        assert_eq!(page.model().scan_generation, 1);
        window.set_visible(false);
        window.present();
        context.block_on(glib::timeout_future(std::time::Duration::from_millis(50)));
        assert_eq!(
            page.model().scan_generation,
            1,
            "showing the page again must not reload it"
        );
        window.close();
    }

    #[test]
    #[ignore = "requires a graphical GTK session"]
    fn queue_edits_sync_tiles() {
        adw::init().expect("GTK session");
        relm4::set_global_css(include_str!("../../style.css"));
        let context = glib::MainContext::default();
        let page = MediaPage::builder().launch(()).detach();
        let flush = || context.block_on(glib::timeout_future(std::time::Duration::from_millis(60)));
        let file = |name: &str| MediaFile {
            path: PathBuf::from(name),
            kind: MediaKind::Video,
            content_type: "video/mp4",
            native: true,
            size: 1024,
        };
        let first = file("/tmp/Video & friends.mp4");
        let second = file("/tmp/Another video.mp4");
        let files = vec![first.clone(), second.clone()];
        page.emit(MediaMsg::Inspected(0, files.clone(), vec![]));
        flush();
        assert_eq!(page.model().chosen.len(), 2);
        assert!(
            page.model().tiles.get(0).unwrap().selected,
            "picked files must appear selected"
        );
        let row = page
            .widgets()
            .queue_list
            .row_at_index(0)
            .and_downcast::<adw::ActionRow>()
            .expect("queue row");
        assert_eq!(
            row.subtitle().as_deref(),
            Some("/tmp/Video & friends.mp4"),
            "a plain-text row shows the path as written"
        );
        page.emit(MediaMsg::Scanned(0, vec![second.clone()]));
        flush();
        assert_eq!(
            page.model().chosen.len(),
            2,
            "changing the library must retain the queue"
        );
        assert!(page.model().tiles.get(0).unwrap().selected);
        page.emit(MediaMsg::Remove(first.path.clone()));
        flush();
        assert_eq!(
            page.model().chosen.len(),
            1,
            "off-grid files must be removable"
        );
        page.emit(MediaMsg::Clear);
        flush();
        assert!(page.model().chosen.is_empty());
        assert!(!page.model().tiles.get(0).unwrap().selected);
        page.emit(MediaMsg::Inspected(0, files.clone(), vec![]));
        flush();
        assert!(
            page.model().chosen.is_empty(),
            "late inspection cannot undo clear"
        );
        page.emit(MediaMsg::Inspected(1, files.clone(), vec![]));
        page.emit(MediaMsg::Status(Some(MediaStatus {
            position: 1,
            total: 2,
            title: first.title(),
            queue: files.iter().map(|file| file.path.clone()).collect(),
            playback: nd_core::media::PlaybackState {
                paused: true,
                seconds: 42.0,
                duration: Some(180.0),
                can_pause: true,
                can_seek: true,
                ..Default::default()
            },
            ..Default::default()
        })));
        flush();
        assert!(page.model().playing());
        assert!(page.model().queue_line().contains("0:42 / 3:00"));
        fn button(widget: &gtk::Widget, tooltip: &str) -> Option<gtk::Button> {
            if let Ok(button) = widget.clone().downcast::<gtk::Button>()
                && button.tooltip_text().as_deref() == Some(tooltip)
            {
                return Some(button);
            }
            let mut child = widget.first_child();
            while let Some(widget) = child {
                if let Some(found) = button(&widget, tooltip) {
                    return Some(found);
                }
                child = widget.next_sibling();
            }
            None
        }
        for tooltip in [
            tr!("Resume"),
            tr!("Back 10 seconds"),
            tr!("Forward 10 seconds"),
            tr!("Next file"),
        ] {
            assert!(
                button(page.widget().upcast_ref(), &tooltip)
                    .expect("playback control")
                    .is_sensitive()
            );
        }

        let window = gtk::Window::builder()
            .default_width(900)
            .default_height(750)
            .child(page.widget())
            .build();
        window.present();
        flush();
        if let Ok(path) = std::env::var("BIGNETSCREEN_MEDIA_TEST_SNAPSHOT") {
            page.widget().allocate(900, 750, -1, None);
            let snapshot = gtk::Snapshot::new();
            window.snapshot_child(page.widget(), &snapshot);
            let node = snapshot.to_node().expect("rendered media page");
            let renderer = gtk::gsk::CairoRenderer::new();
            renderer.realize(None::<&gdk::Surface>).unwrap();
            renderer
                .render_texture(&node, None)
                .save_to_png(path)
                .unwrap();
            renderer.unrealize();
        }
        page.emit(MediaMsg::Status(None));
        flush();
        assert!(
            page.model().chosen.is_empty(),
            "finished playback clears selection"
        );
        window.close();
    }
}
