//! The queue as drawn under the grid: a row for each chosen file, with a
//! thumbnail loaded in the background and a button that removes it.

use super::tile::kind_icon;
use super::{MediaMsg, MediaPage};
use crate::pages::preview::{self, Preview};
use crate::tr;
use nd_chromecast::file_server::MediaFile;
use relm4::adw::{self, prelude::*};
use relm4::gtk;
use relm4::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(super) struct QueueList {
    /// The paths the rows show, in order, so an unchanged queue is not redrawn.
    rendered: Vec<PathBuf>,
    thumbnails: HashMap<PathBuf, gtk::Image>,
    /// Signalled on every redraw, which cancels the previews still loading.
    preview_cancel: tokio::sync::watch::Sender<()>,
}

impl QueueList {
    pub(super) fn new() -> Self {
        Self {
            rendered: Vec::new(),
            thumbnails: HashMap::new(),
            preview_cancel: tokio::sync::watch::channel(()).0,
        }
    }

    /// Redraws `list` for `chosen`, unless it already shows exactly that.
    pub(super) fn render(
        &mut self,
        list: &gtk::ListBox,
        chosen: &[MediaFile],
        sender: &ComponentSender<MediaPage>,
    ) {
        let paths: Vec<_> = chosen.iter().map(|file| file.path.clone()).collect();
        if paths == self.rendered {
            return;
        }
        self.preview_cancel.send_replace(());
        while let Some(row) = list.first_child() {
            list.remove(&row);
        }
        self.thumbnails.clear();
        for file in chosen {
            let row = adw::ActionRow::builder()
                .title(file.title())
                .use_markup(false)
                .subtitle(file.path.to_string_lossy())
                .build();
            let picture = gtk::Image::builder()
                .icon_name(kind_icon(file.kind))
                .pixel_size(48)
                .valign(gtk::Align::Center)
                .css_classes(["queue-thumb"])
                .build();
            row.add_prefix(&picture);
            self.thumbnails.insert(file.path.clone(), picture);
            let path = file.path.clone();
            let kind = file.kind;
            let mut cancel = self.preview_cancel.subscribe();
            sender.command(move |out, shutdown| {
                shutdown
                    .register(async move {
                        tokio::select! {
                            preview = preview::load(path.clone(), kind) => {
                                let _ = out.send((path, preview));
                            },
                            _ = cancel.changed() => {},
                        }
                    })
                    .drop_on_shutdown()
            });
            let remove = gtk::Button::from_icon_name("list-remove-symbolic");
            remove.set_valign(gtk::Align::Center);
            remove.set_tooltip_text(Some(&tr!("Remove from queue")));
            let path = file.path.clone();
            let input = sender.input_sender().clone();
            remove.connect_clicked(move |_| {
                let _ = input.send(MediaMsg::Remove(path.clone()));
            });
            row.add_suffix(&remove);
            list.append(&row);
        }
        self.rendered = paths;
    }

    /// Puts a loaded preview on its row, if the row is still there.
    pub(super) fn show_preview(&self, path: &Path, preview: Result<Preview, String>) {
        if let (Some(picture), Ok(preview)) = (self.thumbnails.get(path), preview)
            && let Some(image) = preview.image
        {
            picture.set_paintable(Some(&image.texture(48)));
        }
    }
}
