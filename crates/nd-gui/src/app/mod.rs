//! The window: a sidebar, a page for each thing the application does, and the
//! state that outlives any one of them.
//!
//! The window is a client of the session service ([`service_link`]): it asks for
//! actions over D-Bus and draws what the service publishes. Closing it stops
//! the drawing, never a running session.
//!
//! - The pages are told, they do not ask: each change pushes the current list
//!   to every page that shows one, so two parts of the window cannot disagree
//!   about what is connected.
//! - Failures are visible: a protocol that will not start becomes a banner
//!   saying why, and a refused action replaces the status line.

mod dialogs;
mod service_link;
mod wording;

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use relm4::adw::{self, prelude::*};
use relm4::gtk;
use relm4::prelude::*;

use nd_chromecast::file_server::MediaFile;
use nd_core::capture::SourceType;
use nd_core::settings::{self, Settings};
use nd_service::dbus::ServiceProxy;
use nd_service::wire::{self, StatusKind};

use crate::pages::Page;
use crate::pages::devices::{DevicesMsg, DevicesOutput, DevicesPage};
use crate::pages::home::{HomeMsg, HomeOutput, HomePage};
use crate::pages::media::{MediaMsg, MediaOutput, MediaPage};
use crate::pages::settings::{SettingsMsg, SettingsOutput, SettingsPage};
use crate::tr;
use service_link::{Connection, ServiceState, follow_the_service};
use wording::{audio_summary, describe_refusal, describe_status, friendly_reason, web_page_text};

pub struct AppModel {
    home: Controller<HomePage>,
    devices: Controller<DevicesPage>,
    media: Controller<MediaPage>,
    settings_page: Controller<SettingsPage>,

    page: Page,
    /// The session service, once it has answered. `None` while connecting, or
    /// after it could not be reached, which shows as a banner.
    service: Option<ServiceProxy<'static>>,
    state: ServiceState,
    /// The sentence under the window title, in the person's language.
    status: String,
    failure: Option<String>,
    settings_changed: bool,
    /// Issues the person has dismissed, so the banner does not come back for
    /// something they have already read.
    dismissed: Vec<wire::Issue>,
    /// What will be captured when a receiver is picked.
    source_type: SourceType,
    ndi_installing: Rc<Cell<bool>>,
    ndi_install_dialog: Option<adw::AlertDialog>,
    /// Lets the next close request through; set once nothing is left to save.
    allow_close: Rc<Cell<bool>>,
}

#[derive(Debug)]
pub enum AppMsg {
    CloseRequested,
    Navigate(Page),
    /// Start streaming to this receiver.
    Cast(String),
    PublishNdi(SourceType),
    /// Share to web browsers on the network.
    PublishWeb(SourceType),
    InstallNdi,
    /// Start streaming this to this receiver, in one step.
    CastWith(String, SourceType),
    /// Stream the screen with this connector to this receiver.
    CastOnScreen(String, String),
    Stop,
    Rescan,
    DismissIssues,
    ShowIssues,
    ShowFailure,
    SetAutoDiscovery(bool),
    SettingsChanged(Settings),
    SettingsSaved(Result<(), String>),
    /// Send these files to the receiver with this id.
    SendMedia(Vec<MediaFile>, String),
    CancelMedia,
    ControlMedia(nd_core::media::MediaCommand),
    /// Send files to this receiver (from the devices page).
    MediaTarget(String),
    About,
}

#[derive(Debug)]
pub enum AppCmd {
    NdiInstalled(std::result::Result<(), String>),
    NdiInstallOffer(bool),
    /// The service answered and is ours to talk to.
    Connected(Box<Connection>),
    /// The service could not be reached, with the reason to show.
    Unreachable(String),
    /// A fresh reading of everything the service publishes.
    State(Box<ServiceState>),
    /// The screens the service streams directly, (connector, description),
    /// for a screen cast to this receiver; empty where the portal asks.
    Screens {
        id: String,
        screens: Vec<(String, String)>,
    },
    /// A call the service refused.
    Refused(String),
    Done,
    PreferencesFlushed(Result<(), String>),
}

#[relm4::component(pub)]
impl Component for AppModel {
    type Init = ();
    type Input = AppMsg;
    type Output = ();
    type CommandOutput = AppCmd;

    view! {
        adw::ApplicationWindow {
            set_title: Some("BigNetScreen"),
            add_css_class: "bns-window",
            set_default_width: 1280,
            set_default_height: 860,
            set_width_request: 480,
            set_height_request: 480,

            #[name = "split"]
            adw::OverlaySplitView {
                set_max_sidebar_width: 270.0,
                set_min_sidebar_width: 250.0,
                set_collapsed: false,
                set_show_sidebar: true,

                #[wrap(Some)]
                // The sidebar has no header bar, so its top would not drag the
                // window; the `WindowHandle` around the identity block does.
                set_sidebar = &gtk::Box {
                    add_css_class: "bns-sidebar",
                    set_orientation: gtk::Orientation::Vertical,

                    gtk::WindowHandle {
                        gtk::Box {
                            set_spacing: 10,
                            add_css_class: "app-identity",

                            gtk::Image {
                                set_icon_name: Some("tv-symbolic"),
                                set_pixel_size: 22,
                                add_css_class: "app-mark",
                            },
                            gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_valign: gtk::Align::Center,

                                gtk::Label {
                                    set_label: "BigNetScreen",
                                    set_xalign: 0.0,
                                    add_css_class: "title",
                                },
                                gtk::Label {
                                    set_label: &tr!("Share your screen wirelessly"),
                                    set_xalign: 0.0,
                                    add_css_class: "subtitle",
                                },
                            },
                        },
                    },

                    #[name = "nav"]
                        gtk::ListBox {
                            set_margin_all: 8,
                            add_css_class: "navigation-sidebar",
                            connect_row_selected[sender] => move |_, row| {
                                if let Some(page) = row
                                    .and_then(|row| usize::try_from(row.index()).ok())
                                    .and_then(|index| Page::all().get(index).copied())
                                {
                                    sender.input(AppMsg::Navigate(page));
                                }
                            },
                        },

                        gtk::Box { set_vexpand: true },

                        // What is connected, at the foot of the sidebar.
                        gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            add_css_class: "connected-card",
                            #[watch]
                            set_visible: !model.state.session.is_idle(),

                            gtk::Box {
                                set_spacing: 6,

                                gtk::Label {
                                    set_label: &tr!("Connected to"),
                                    set_hexpand: true,
                                    set_xalign: 0.0,
                                    add_css_class: "caption",
                                },
                                gtk::Label {
                                    set_label: "●",
                                    add_css_class: "live-dot",
                                },
                            },
                            gtk::Label {
                                #[watch]
                                set_label: &model.state.session.display_name,
                                set_xalign: 0.0,
                                set_ellipsize: gtk::pango::EllipsizeMode::End,
                                add_css_class: "heading",
                            },
                            gtk::Label {
                                #[watch]
                                set_label: &model.state.session.address,
                                set_xalign: 0.0,
                                add_css_class: "dim-label",
                            },
                        },

                        gtk::Box {
                            set_spacing: 12,
                            add_css_class: "sidebar-footer",

                            gtk::Button {
                                set_icon_name: "help-about-symbolic",
                                set_tooltip_text: Some(&tr!("About BigNetScreen")),
                                connect_clicked => AppMsg::About,
                            },
                            gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_valign: gtk::Align::Center,
                                gtk::Label { set_label: "BigNetScreen", set_xalign: 0.0, add_css_class: "dim-label" },
                                gtk::Label { set_label: crate::APP_VERSION, set_xalign: 0.0, add_css_class: "caption", add_css_class: "dim-label" },
                            },
                        },
                },

                #[wrap(Some)]
                set_content = &adw::ToolbarView {
                    add_top_bar = &adw::HeaderBar {
                        add_css_class: "flat",

                        #[name = "navigation_button"]
                        pack_start = &gtk::Button {
                            set_icon_name: "sidebar-show-symbolic",
                            set_tooltip_text: Some(&tr!("Show navigation")),
                            set_visible: false,
                        },

                        #[wrap(Some)]
                        set_title_widget = &adw::WindowTitle {
                            #[watch]
                            set_title: &model.page.title(),
                            #[watch]
                            set_subtitle: &model.status,
                        },

                        pack_end = &gtk::Button {
                            set_label: &tr!("Stop"),
                            add_css_class: "destructive-action",
                            #[watch]
                            set_visible: model.is_busy(),
                            connect_clicked => AppMsg::Stop,
                        },
                    },

                    #[wrap(Some)]
                    set_content = &gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,

                        // Both banners can carry text from the service.
                        adw::Banner {
                            set_use_markup: false,
                            #[watch]
                            set_revealed: !model.issues_summary().is_empty(),
                            #[watch]
                            set_title: &model.issues_summary(),
                            set_button_label: Some(&tr!("Details")),
                            connect_button_clicked => AppMsg::ShowIssues,
                        },

                        adw::Banner {
                            set_use_markup: false,
                            #[watch]
                            set_revealed: model.failure.is_some() || model.state.status.is(StatusKind::Error),
                            #[watch]
                            set_title: &describe_refusal(model.failure.as_deref().unwrap_or(&model.state.status.detail)),
                            set_button_label: Some(&tr!("Details")),
                            connect_button_clicked => AppMsg::ShowFailure,
                        },

                        #[name = "stack"]
                        gtk::Stack {
                            set_vexpand: true,
                            // The visible page is set from `update_with_view`
                            // rather than watched here: the pages are added to
                            // the stack after this view is built, so a watch
                            // would fire once against an empty stack.
                            set_transition_type: gtk::StackTransitionType::Crossfade,
                        },
                    },
                },
            }
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let current = settings::current();

        let home = HomePage::builder()
            .launch(())
            .forward(sender.input_sender(), |output| match output {
                // The home page names both halves of the decision in one
                // message: who receives, and what they receive.
                HomeOutput::PublishNdi(source) => AppMsg::PublishNdi(source),
                HomeOutput::PublishWeb(source) => AppMsg::PublishWeb(source),
                HomeOutput::Cast { id, source } => AppMsg::CastWith(id, source),
                HomeOutput::SendMedia(id) => AppMsg::MediaTarget(id),
                HomeOutput::Stop => AppMsg::Stop,
                HomeOutput::Rescan => AppMsg::Rescan,
                HomeOutput::AudioSettings => AppMsg::Navigate(Page::Settings),
            });
        let devices = DevicesPage::builder()
            .launch(())
            .forward(sender.input_sender(), |output| match output {
                DevicesOutput::Cast(id) => AppMsg::Cast(id),
                DevicesOutput::Stop => AppMsg::Stop,
                DevicesOutput::Rescan => AppMsg::Rescan,
                DevicesOutput::AutoDiscovery(on) => AppMsg::SetAutoDiscovery(on),
                DevicesOutput::SendMedia(id) => AppMsg::MediaTarget(id),
            });
        let media = MediaPage::builder()
            .launch(())
            .forward(sender.input_sender(), |output| match output {
                MediaOutput::Send(files, target) => AppMsg::SendMedia(files, target),
                MediaOutput::Cancel => AppMsg::CancelMedia,
                MediaOutput::Control(command) => AppMsg::ControlMedia(command),
            });
        let settings_page = SettingsPage::builder().launch(()).forward(
            sender.input_sender(),
            |output| match output {
                SettingsOutput::Changed(settings) => AppMsg::SettingsChanged(settings),
                SettingsOutput::Saved(result) => AppMsg::SettingsSaved(result),
            },
        );

        let model = AppModel {
            failure: None,
            settings_changed: false,
            home,
            devices,
            media,
            settings_page,
            page: Page::Home,
            service: None,
            state: ServiceState::default(),
            status: tr!("Connecting…"),
            dismissed: Vec::new(),
            source_type: SourceType::Monitor,
            ndi_installing: Default::default(),
            ndi_install_dialog: None,
            allow_close: Default::default(),
        };

        let widgets = view_output!();
        let compact = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 1000px").expect("valid breakpoint"),
        );
        compact.add_setter(&widgets.split, "collapsed", Some(&true.to_value()));
        compact.add_setter(&widgets.split, "show-sidebar", Some(&false.to_value()));
        compact.add_setter(
            &widgets.navigation_button,
            "visible",
            Some(&true.to_value()),
        );
        big_gtk_kit::breakpoint::add(&root, compact);
        let split = widgets.split.downgrade();
        widgets.navigation_button.connect_clicked(move |_| {
            if let Some(split) = split.upgrade() {
                split.set_show_sidebar(true);
            }
        });

        let ndi_installing = model.ndi_installing.clone();
        let allow_close = model.allow_close.clone();
        let close_input = sender.input_sender().clone();
        root.connect_close_request(move |_| {
            if allow_close.get() {
                return gtk::glib::Propagation::Proceed;
            }
            if !ndi_installing.get() {
                close_input.emit(AppMsg::CloseRequested);
            }
            gtk::glib::Propagation::Stop
        });

        let mapped = std::cell::Cell::new(false);
        root.connect_map(move |window| {
            if mapped.replace(true) {
                return;
            }
            tracing::info!(
                elapsed_ms = crate::STARTED.elapsed().as_millis() as u64,
                renderer = ?window.renderer().map(|renderer| renderer.type_().name()),
                "main window mapped"
            );
            if let Some(clock) = window.frame_clock() {
                let handler = std::rc::Rc::new(std::cell::Cell::new(None));
                let id = clock.connect_after_paint({
                    let handler = handler.clone();
                    move |clock| {
                        tracing::info!(
                            elapsed_ms = crate::STARTED.elapsed().as_millis() as u64,
                            "first window frame rendered"
                        );
                        if let Some(id) = handler.take() {
                            clock.disconnect(id);
                        }
                    }
                });
                handler.set(Some(id));
            }
        });

        // The sidebar rows follow `Page::all()`: the selection handler maps a
        // row index straight onto that array.
        for page in Page::all() {
            let row = adw::ActionRow::builder().title(page.title()).build();
            row.add_prefix(&gtk::Image::from_icon_name(page.icon()));
            widgets.nav.append(&row);
            let content: &gtk::Widget = match page {
                Page::Home => model.home.widget().upcast_ref(),
                Page::Devices => model.devices.widget().upcast_ref(),
                Page::Media => model.media.widget().upcast_ref(),
                Page::Settings => model.settings_page.widget().upcast_ref(),
            };
            widgets.stack.add_named(content, Some(page.id()));
        }
        if let Some(row) = widgets.nav.row_at_index(0) {
            widgets.nav.select_row(Some(&row));
        }

        // The pages are added after the view is built, so the first pass of
        // `set_visible_child_name` ran against an empty stack. Setting it here
        // keeps the window from opening blank, and GTK from warning that
        // "home" does not exist.
        widgets.stack.set_visible_child_name(model.page.id());

        model.home.emit(HomeMsg::Searching(current.auto_discovery));
        model
            .home
            .emit(HomeMsg::AudioSummary(audio_summary(&current)));
        follow_the_service(&sender);

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
            AppMsg::CloseRequested => {
                if !self.settings_changed {
                    self.allow_close.set(true);
                    root.close();
                    return;
                }
                let service = self.service.clone();
                sender.oneshot_command(async move {
                    let result = relm4::spawn_blocking(settings::persist)
                        .await
                        .map_err(|err| err.to_string())
                        .and_then(|result| result.map_err(|err| err.to_string()));
                    if result.is_ok()
                        && let Some(service) = service
                    {
                        // Closing before the debounce fires must still notify the service.
                        let _ = tokio::time::timeout(
                            Duration::from_secs(10),
                            service.reload_settings(),
                        )
                        .await;
                    }
                    AppCmd::PreferencesFlushed(result)
                });
            }
            AppMsg::Navigate(page) => {
                self.page = page;
                if widgets.split.is_collapsed() {
                    widgets.split.set_show_sidebar(false);
                }
            }
            AppMsg::Cast(id) => self.begin_cast(id, &sender),
            // The service checks the plugins and refuses with a reason the
            // status line explains.
            AppMsg::PublishNdi(source) => {
                if !self.ndi_installing.get() {
                    self.source_type = source;
                    self.begin_cast(nd_ndi::ID.into(), &sender);
                }
            }
            AppMsg::PublishWeb(source) => {
                self.source_type = source;
                self.begin_cast(nd_webrtc::ID.into(), &sender);
            }
            AppMsg::InstallNdi => {
                if self.ndi_installing.get() {
                    return;
                }
                self.ndi_installing.set(true);
                self.ndi_install_dialog = Some(dialogs::ndi_installing(root));
                sender.oneshot_command(async {
                    AppCmd::NdiInstalled(crate::ndi_setup::install().await)
                });
            }
            AppMsg::CastWith(id, source) => {
                self.source_type = source;
                tracing::info!(?source, "capture source chosen");
                self.begin_cast(id, &sender);
            }
            AppMsg::CastOnScreen(id, screen) => {
                tracing::info!(%screen, "screen chosen");
                let monitor = wire::source_name(SourceType::Monitor);
                self.cast(id, format!("{monitor}:{screen}"), &sender);
            }
            AppMsg::Stop | AppMsg::CancelMedia => {
                self.failure = None;
                self.status = tr!("Stopping…");
                self.call(&sender, |service| async move { service.stop().await });
            }
            AppMsg::Rescan => {
                self.failure = None;
                self.dismissed.clear();
                self.status = tr!("Searching…");
                self.call(&sender, |service| async move { service.rescan().await });
            }
            AppMsg::DismissIssues => self.dismissed = self.state.issues.clone(),
            AppMsg::ShowIssues => {
                dialogs::issues(root, &self.state.issues, sender.input_sender().clone())
            }
            AppMsg::ShowFailure => {
                let detail = self.failure.as_deref().unwrap_or(&self.state.status.detail);
                dialogs::failure(root, detail, sender.input_sender().clone());
            }
            AppMsg::SetAutoDiscovery(on) => {
                self.settings_page.emit(SettingsMsg::SyncAutoDiscovery(on));
            }
            AppMsg::SettingsChanged(new) => {
                self.settings_changed = true;
                self.failure = None;
                self.home.emit(HomeMsg::AudioSummary(audio_summary(&new)));
                self.devices
                    .emit(DevicesMsg::SyncAutoDiscovery(new.auto_discovery));
            }
            AppMsg::SettingsSaved(result) => match result {
                Ok(()) => self.call(
                    &sender,
                    |service| async move { service.reload_settings().await },
                ),
                Err(reason) => {
                    self.failure = Some(format!("could not save settings: {reason}"));
                    self.status = tr!("Could not save settings");
                }
            },
            AppMsg::MediaTarget(id) => {
                // Chosen on the devices page: the media page opens with that
                // receiver already selected, because the person just said so.
                self.media.emit(MediaMsg::Choose(id));
                self.page = Page::Media;
            }
            AppMsg::SendMedia(files, target) => {
                let paths: Vec<String> = files
                    .iter()
                    .map(|file| file.path.display().to_string())
                    .collect();
                self.call_with_settings(&sender, move |service| async move {
                    service.send_media(&target, &paths).await
                });
            }
            AppMsg::ControlMedia(command) => {
                use nd_core::media::MediaCommand;
                let (name, value, path) = match command {
                    MediaCommand::TogglePause => ("toggle-pause", 0.0, PathBuf::new()),
                    MediaCommand::SetPaused(paused) => {
                        (if paused { "pause" } else { "play" }, 0.0, PathBuf::new())
                    }
                    MediaCommand::SeekTo(seconds) => ("seek-to", seconds, PathBuf::new()),
                    MediaCommand::SetVolume(level) => ("volume", level, PathBuf::new()),
                    MediaCommand::SetMute(muted) => {
                        (if muted { "mute" } else { "unmute" }, 0.0, PathBuf::new())
                    }
                    MediaCommand::Next => ("next", 0.0, PathBuf::new()),
                    MediaCommand::SeekRelative(seconds) => ("seek", seconds, PathBuf::new()),
                    MediaCommand::Remove(path) => ("remove", 0.0, path),
                };
                let path = path.display().to_string();
                self.call(&sender, move |service| async move {
                    service.control_media(name, value, &path).await
                });
            }
            AppMsg::About => dialogs::about(root),
        }

        // The page the model says is current.
        widgets.stack.set_visible_child_name(self.page.id());

        // A page can also be reached without touching the sidebar: "Media" on
        // the home page, or "Send media" on a device. The highlight has to
        // follow, or it points at the page the person just left.
        let index = Page::all()
            .iter()
            .position(|page| *page == self.page)
            .unwrap_or(0) as i32;
        if widgets.nav.selected_row().map(|row| row.index()) != Some(index)
            && let Some(row) = widgets.nav.row_at_index(index)
        {
            widgets.nav.select_row(Some(&row));
        }

        self.update_view(widgets, sender);
    }

    /// Handles a background result and repaints: overriding
    /// `update_cmd_with_view` replaces relm4's default cycle, so without the
    /// `update_view` call at the end no command result reaches the window.
    fn update_cmd_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::CommandOutput,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        // relm4 prints its `update_cmd_with_view` span only when an event
        // happens inside it, so a variant that logs nothing would leave no
        // trace that it arrived.
        tracing::debug!(
            kind = match &message {
                AppCmd::NdiInstalled(_) => "ndi-installed",
                AppCmd::NdiInstallOffer(_) => "ndi-install-offer",
                AppCmd::Connected(_) => "connected",
                AppCmd::Unreachable(_) => "unreachable",
                AppCmd::State(_) => "state",
                AppCmd::Screens { .. } => "screens",
                AppCmd::Refused(_) => "refused",
                AppCmd::Done => "done",
                AppCmd::PreferencesFlushed(_) => "preferences-flushed",
            },
            "command output arrived"
        );
        match message {
            AppCmd::Done => return,
            AppCmd::PreferencesFlushed(result) => match result {
                Ok(()) => {
                    self.allow_close.set(true);
                    root.close();
                }
                Err(reason) => dialogs::save_failure(root, &reason, self.allow_close.clone()),
            },
            AppCmd::NdiInstalled(result) => {
                self.ndi_installing.set(false);
                if let Some(dialog) = self.ndi_install_dialog.take() {
                    dialog.force_close();
                }
                dialogs::ndi_install_result(root, result);
            }
            AppCmd::NdiInstallOffer(can_install) => {
                if self.state.status.is(StatusKind::NdiRuntimeMissing) {
                    dialogs::ndi_install_offer(root, can_install, sender.input_sender().clone());
                }
            }
            AppCmd::Connected(connection) => {
                self.failure = None;
                tracing::info!("connected to the session service");
                self.service = Some(connection.0);
            }
            AppCmd::Unreachable(reason) => {
                tracing::error!(%reason, "the session service could not be reached");
                self.service = None;
                self.state = ServiceState::default();
                self.home.emit(HomeMsg::Searching(false));
                self.home.emit(HomeMsg::VirtualAvailable(false));
                self.push_devices();
                self.push_media_status();
                self.status = tr!("The sharing service is not responding. Try again.");
            }
            AppCmd::Screens { id, mut screens } => {
                let monitor = wire::source_name(SourceType::Monitor);
                match screens.len() {
                    0 => self.cast(id, monitor.to_string(), &sender),
                    1 => {
                        let (name, _) = screens.remove(0);
                        self.cast(id, format!("{monitor}:{name}"), &sender);
                    }
                    _ => dialogs::choose_screen(root, id, &screens, sender.input_sender().clone()),
                }
            }
            AppCmd::Refused(reason) => {
                self.status = describe_refusal(&reason);
                self.failure = Some(reason);
            }
            AppCmd::State(state) => self.apply_state(*state, &sender),
        }

        self.update_view(widgets, sender);
    }
}

impl AppModel {
    /// Is anything running that the Stop button should end?
    fn is_busy(&self) -> bool {
        !self.state.session.is_idle() || self.state.media.active
    }

    /// Calls the service, turning a refusal into something the window can say.
    ///
    /// Every action goes through here so that no call site has to remember
    /// that the service may not be there yet.
    fn call<F, Fut>(&self, sender: &ComponentSender<Self>, call: F)
    where
        F: FnOnce(ServiceProxy<'static>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = zbus::Result<()>> + Send,
    {
        let Some(service) = self.service.clone() else {
            sender.oneshot_command(async { AppCmd::Unreachable("not connected".into()) });
            return;
        };
        sender.oneshot_command(async move {
            match call(service).await {
                Ok(()) => AppCmd::Done,
                Err(err) => AppCmd::Refused(err.to_string()),
            }
        });
    }

    fn call_with_settings<F, Fut>(&self, sender: &ComponentSender<Self>, call: F)
    where
        F: FnOnce(ServiceProxy<'static>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = zbus::Result<()>> + Send,
    {
        let settings_changed = self.settings_changed;
        self.call(sender, move |service| async move {
            if settings_changed {
                relm4::spawn_blocking(settings::persist)
                    .await
                    .map_err(|err| zbus::Error::Failure(err.to_string()))?
                    .map_err(|err| {
                        zbus::Error::Failure(format!("could not save settings: {err}"))
                    })?;
            }
            service.reload_settings().await?;
            call(service).await
        });
    }

    fn begin_cast(&mut self, id: String, sender: &ComponentSender<Self>) {
        self.failure = None;
        if self.is_busy() {
            self.status = tr!("A stream is already running");
            return;
        }
        // Which screen, when the compositor streams them directly, is asked
        // here; where it does not, the portal asks once the cast starts.
        if self.source_type == SourceType::Monitor
            && let Some(service) = self.service.clone()
        {
            sender.oneshot_command(async move {
                match service.screens().await {
                    Ok(screens) => AppCmd::Screens { id, screens },
                    Err(err) => AppCmd::Refused(err.to_string()),
                }
            });
            return;
        }
        let source = wire::source_name(self.source_type).to_string();
        self.cast(id, source, sender);
    }

    /// Asks the service to stream `source` to the receiver `id`.
    fn cast(&mut self, id: String, source: String, sender: &ComponentSender<Self>) {
        // The page a browser is shown is for a stranger on the network, not for
        // a client, so the service cannot write it: it has no locale. This does.
        let page_text = web_page_text();
        self.status = tr!("Connecting…");
        self.call_with_settings(sender, move |service| async move {
            service.cast(&id, &source, &page_text).await
        });
    }

    /// Takes a fresh reading of the service and tells each page only what
    /// changed for it.
    fn apply_state(&mut self, state: ServiceState, sender: &ComponentSender<Self>) {
        let receivers_changed = state.receivers != self.state.receivers;
        let session_changed =
            state.session != self.state.session || state.link_probe != self.state.link_probe;
        let media_changed = state.media != self.state.media;
        let searching_changed = state.status.kind != self.state.status.kind;
        let virtual_changed = state.virtual_available != self.state.virtual_available;
        let ndi_missing = state.status.is(StatusKind::NdiRuntimeMissing)
            && !self.state.status.is(StatusKind::NdiRuntimeMissing);
        let was_streaming = !self.state.session.is_idle();
        self.state = state;
        self.status = describe_status(&self.state.status, &self.state.session);
        if virtual_changed {
            self.home
                .emit(HomeMsg::VirtualAvailable(self.state.virtual_available));
        }
        if searching_changed {
            self.home.emit(HomeMsg::Searching(
                self.state.status.is(StatusKind::Searching),
            ));
        }
        if receivers_changed || session_changed {
            self.push_devices();
        }
        if media_changed {
            self.push_media_status();
        }
        if ndi_missing {
            sender.oneshot_command(async {
                AppCmd::NdiInstallOffer(crate::ndi_setup::can_install().await)
            });
        }
        if was_streaming && self.state.session.is_idle() {
            tracing::info!("the session ended");
        }
    }

    /// Pushes the current list to every page that shows receivers.
    fn push_devices(&mut self) {
        let entries = self.state.entries();
        self.home.emit(HomeMsg::Devices(entries.clone()));
        self.home.emit(HomeMsg::Session(self.state.session_info()));
        self.devices.emit(DevicesMsg::Devices(entries));
        self.devices.emit(DevicesMsg::Active(
            (!self.state.session.is_idle()).then(|| self.state.session.id.clone()),
        ));
        self.media
            .emit(MediaMsg::Targets(self.state.media_receivers()));
    }

    fn push_media_status(&mut self) {
        if !self.state.media.active {
            self.media.emit(MediaMsg::Status(None));
            return;
        }
        self.media
            .emit(MediaMsg::Status(Some(self.state.media_status())));
    }

    /// The banner text: what is wrong that the person has not already read.
    fn issues_summary(&self) -> String {
        self.state
            .issues
            .iter()
            .filter(|issue| !self.dismissed.contains(issue))
            .map(|issue| friendly_reason(&issue.provider, &issue.reason))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires isolated GTK and XDG_CONFIG_HOME=/proc/bns-leak-test"]
    fn save_error_dialog_releases_its_window() {
        assert_eq!(
            std::env::var("XDG_CONFIG_HOME").unwrap(),
            "/proc/bns-leak-test"
        );
        adw::init().unwrap();
        struct Finalized(std::rc::Rc<std::cell::Cell<bool>>);
        impl Drop for Finalized {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let context = gtk::glib::MainContext::default();
        for force in [false, true].into_iter().cycle().take(10) {
            let finalized = std::rc::Rc::new(std::cell::Cell::new(false));
            let controller = AppModel::builder().launch(());
            // SAFETY: this test exclusively owns the key and observes its destructor.
            unsafe {
                controller
                    .widget()
                    .set_data("bns-save-finalization", Finalized(finalized.clone()));
            }
            controller.widget().present();
            controller.state().get_mut().model.settings_changed = true;
            controller.emit(AppMsg::CloseRequested);
            for _ in 0..20 {
                context.block_on(gtk::glib::timeout_future(Duration::from_millis(50)));
                if controller.widget().visible_dialog().is_some() {
                    break;
                }
            }
            let dialog = controller
                .widget()
                .visible_dialog()
                .unwrap()
                .downcast::<adw::AlertDialog>()
                .unwrap();
            if force {
                controller.widget().destroy();
            } else {
                dialog.set_close_response("close");
                dialog.close();
            }
            drop(dialog);
            drop(controller);
            context.block_on(gtk::glib::timeout_future(Duration::from_millis(500)));
            assert!(
                finalized.get(),
                "save-error window did not finalize (force={force})"
            );
        }
        println!("save-error window census: new=10, fin=10");
    }

    #[test]
    #[ignore = "requires an isolated GTK session"]
    fn navigation_tree_finalizes_after_window_close() {
        adw::init().unwrap();
        let finalized = std::rc::Rc::new(std::cell::Cell::new(0));
        struct Finalized(std::rc::Rc<std::cell::Cell<usize>>);
        impl Drop for Finalized {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let context = gtk::glib::MainContext::default();
        let cycles = std::env::var("BIGNETSCREEN_LEAK_CYCLES")
            .map(|value| value.parse::<usize>().unwrap())
            .unwrap_or(5);
        for cycle in 1..=cycles {
            let controller = AppModel::builder().launch(());
            // SAFETY: this test owns this qdata key and never reads its value.
            // Its destructor observes finalization, rather than disposal.
            unsafe {
                controller
                    .widgets()
                    .split
                    .set_data("bns-test-finalization", Finalized(finalized.clone()));
            }
            controller.widget().present();
            context.block_on(gtk::glib::timeout_future(Duration::from_millis(100)));
            controller.widgets().split.set_show_sidebar(false);
            controller.widgets().navigation_button.emit_clicked();
            assert!(controller.widgets().split.shows_sidebar());
            controller.widget().destroy();
            drop(controller);
            context.block_on(gtk::glib::timeout_future(Duration::from_millis(100)));
            assert_eq!(finalized.get(), cycle, "navigation panel did not finalize");
        }
        println!("navigation census: new={cycles}, fin={}", finalized.get());
    }
}
