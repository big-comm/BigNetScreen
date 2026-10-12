//! The window: the choices made before sharing on the left, where to share on
//! the right, and the state that outlives either.
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
mod session;
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

use crate::pages::Discovery;
use crate::pages::media::{MediaMsg, MediaOutput, MediaPage};
use crate::pages::options::{OptionsMsg, OptionsOutput, OptionsPanel, Share};
use crate::pages::settings::{SettingsMsg, SettingsOutput, SettingsPage};
use crate::pages::share::{ShareMsg, ShareOutput, SharePage};
use crate::tr;
use service_link::{Connection, ServiceState, follow_the_service};
use wording::{
    SAVE_FAILED, describe_refusal, describe_status, friendly_reason, known_refusal, web_page_text,
};

pub struct AppModel {
    options: Controller<OptionsPanel>,
    share_page: Controller<SharePage>,
    media: Controller<MediaPage>,
    settings_page: Controller<SettingsPage>,
    /// Holds the settings page; kept to be presented again.
    settings_dialog: adw::Dialog,

    /// What is shared, as chosen in the sidebar.
    share: Share,
    /// The session service, once it has answered. `None` while connecting, or
    /// after it could not be reached, which shows as a banner.
    service: Option<ServiceProxy<'static>>,
    state: ServiceState,
    /// The sentence under the window title, in the person's language.
    status: String,
    failure: Option<String>,
    /// The receiver picked while another session ran, until the person says
    /// whether to switch.
    switch_to: Option<String>,
    /// The receiver to start once the running session has stopped.
    pending_cast: Option<String>,
    settings_changed: bool,
    /// Issues the person has dismissed, so the banner does not come back for
    /// something they have already read.
    dismissed: Vec<wire::Issue>,
    /// What will be captured when a receiver is picked: the last screen
    /// choice, kept while files are chosen.
    source_type: SourceType,
    ndi_installing: Rc<Cell<bool>>,
    ndi_install_dialog: Option<adw::AlertDialog>,
    /// Lets the next close request through; set once nothing is left to save.
    allow_close: Rc<Cell<bool>>,
}

#[derive(Debug)]
pub enum AppMsg {
    CloseRequested,
    /// What to share was chosen in the sidebar.
    Share(Share),
    /// Settings changed in the sidebar.
    OptionsChanged(Settings),
    ShowSettings,
    PublishNdi,
    /// Share to web browsers on the network.
    PublishWeb,
    InstallNdi,
    /// Start streaming the chosen source to this receiver.
    Cast(String),
    /// Stream the screen with this connector to this receiver.
    CastOnScreen(String, String),
    Stop,
    /// Stop the running session, then start the one asked for meanwhile.
    SwitchStream,
    Rescan,
    DismissIssues,
    ShowIssues,
    ShowFailure,
    SettingsChanged(Settings),
    SettingsSaved(Result<(), String>),
    /// Send these files to the receiver with this id.
    SendMedia(Vec<MediaFile>, String),
    CancelMedia,
    ControlMedia(nd_core::media::MediaCommand),
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
            set_default_width: 1180,
            set_default_height: 800,
            set_width_request: 480,
            set_height_request: 480,

            #[name = "split"]
            adw::OverlaySplitView {
                set_min_sidebar_width: 300.0,
                set_max_sidebar_width: 360.0,
                set_sidebar_width_fraction: 0.32,

                #[wrap(Some)]
                set_sidebar = &adw::ToolbarView {
                    add_css_class: "bns-sidebar",
                    add_top_bar = &adw::HeaderBar {
                        #[wrap(Some)]
                        set_title_widget = &adw::WindowTitle {
                            set_title: "BigNetScreen",
                        },
                    },
                    #[wrap(Some)]
                    set_content = model.options.widget(),
                },

                #[wrap(Some)]
                set_content = &adw::ToolbarView {
                    add_top_bar = &adw::HeaderBar {
                        #[name = "sidebar_button"]
                        pack_start = &gtk::Button {
                            set_icon_name: "sidebar-show-symbolic",
                            set_tooltip_text: Some(&tr!("Show the options")),
                            set_visible: false,
                        },

                        #[wrap(Some)]
                        set_title_widget = &adw::WindowTitle {
                            #[watch]
                            set_title: &model.status,
                        },

                        pack_end = &gtk::MenuButton {
                            set_icon_name: "open-menu-symbolic",
                            set_tooltip_text: Some(&tr!("Main menu")),
                            set_primary: true,
                            set_menu_model: Some(&main_menu()),
                        },
                        pack_end = &gtk::Button {
                            set_tooltip_text: Some(&tr!("Search for devices again (F5)")),
                            connect_clicked => AppMsg::Rescan,
                            adw::ButtonContent {
                                set_icon_name: "view-refresh-symbolic",
                                set_label: &tr!("Search"),
                            },
                        },
                        // The session card is on the share side only.
                        pack_end = &gtk::Button {
                            set_label: &tr!("Stop sharing"),
                            add_css_class: "destructive-action",
                            #[watch]
                            set_visible: model.share == Share::Files && !model.state.session.is_idle(),
                            #[watch]
                            set_sensitive: !model.state.status.is(StatusKind::Stopping),
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
                            set_transition_type: gtk::StackTransitionType::Crossfade,
                            add_named: (model.share_page.widget(), Some("share")),
                            add_named: (model.media.widget(), Some("media")),
                            #[watch]
                            set_visible_child_name: if model.share == Share::Files { "media" } else { "share" },
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

        let options = OptionsPanel::builder()
            .launch(())
            .forward(sender.input_sender(), |output| match output {
                OptionsOutput::Share(share) => AppMsg::Share(share),
                OptionsOutput::Settings(settings) => AppMsg::OptionsChanged(settings),
            });
        let share_page = SharePage::builder()
            .launch(())
            .forward(sender.input_sender(), |output| match output {
                ShareOutput::Cast(id) => AppMsg::Cast(id),
                ShareOutput::Web => AppMsg::PublishWeb,
                ShareOutput::Ndi => AppMsg::PublishNdi,
                ShareOutput::Stop => AppMsg::Stop,
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
        let settings_view = adw::ToolbarView::new();
        settings_view.add_top_bar(&adw::HeaderBar::new());
        settings_view.set_content(Some(settings_page.widget()));
        let settings_dialog = adw::Dialog::builder()
            .title(tr!("Settings"))
            .content_width(560)
            .child(&settings_view)
            .build();

        let model = AppModel {
            failure: None,
            switch_to: None,
            pending_cast: None,
            settings_changed: false,
            options,
            share_page,
            media,
            settings_page,
            settings_dialog,
            share: Share::Screen,
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
            adw::BreakpointCondition::parse("max-width: 860px").expect("valid breakpoint"),
        );
        compact.add_setter(&widgets.split, "collapsed", Some(&true.to_value()));
        compact.add_setter(&widgets.split, "show-sidebar", Some(&false.to_value()));
        compact.add_setter(&widgets.sidebar_button, "visible", Some(&true.to_value()));
        big_gtk_kit::breakpoint::add(&root, compact);
        let split = widgets.split.downgrade();
        widgets.sidebar_button.connect_clicked(move |_| {
            if let Some(split) = split.upgrade() {
                split.set_show_sidebar(true);
            }
        });

        for (name, message) in [
            ("settings", (|| AppMsg::ShowSettings) as fn() -> AppMsg),
            ("about", || AppMsg::About),
        ] {
            let action = gtk::gio::SimpleAction::new(name, None);
            let input = sender.input_sender().clone();
            action.connect_activate(move |_, _| input.emit(message()));
            root.add_action(&action);
        }

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

        // Ctrl+Q closes through the window, so the close request still
        // flushes preferences; F5 searches again; Ctrl+, opens the settings.
        let shortcuts = gtk::ShortcutController::new();
        shortcuts.set_scope(gtk::ShortcutScope::Managed);
        let add = |trigger: &str, action: gtk::CallbackAction| {
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(trigger),
                Some(action),
            ));
        };
        let emit = |message: fn() -> AppMsg| {
            let input = sender.input_sender().clone();
            gtk::CallbackAction::new(move |_, _| {
                input.emit(message());
                gtk::glib::Propagation::Stop
            })
        };
        let window = root.downgrade();
        add(
            "<Control>q",
            gtk::CallbackAction::new(move |_, _| {
                if let Some(window) = window.upgrade() {
                    window.close();
                }
                gtk::glib::Propagation::Stop
            }),
        );
        add("F5", emit(|| AppMsg::Rescan));
        add("<Control>comma", emit(|| AppMsg::ShowSettings));
        root.add_controller(shortcuts);

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

        model.push_discovery(if current.auto_discovery {
            Discovery::Searching
        } else {
            Discovery::Off
        });
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
            AppMsg::Share(share) => {
                self.share = share;
                if let Some(source) = share.source() {
                    self.source_type = source;
                }
                if widgets.split.is_collapsed() {
                    widgets.split.set_show_sidebar(false);
                }
            }
            AppMsg::OptionsChanged(settings) => {
                self.settings_page.emit(SettingsMsg::Replace(settings));
            }
            AppMsg::ShowSettings => self.settings_dialog.present(Some(root)),
            // The service checks the plugins and refuses with a reason the
            // status line explains.
            AppMsg::PublishNdi => {
                if !self.ndi_installing.get() {
                    self.begin_cast(nd_ndi::ID.into(), &sender);
                }
            }
            AppMsg::PublishWeb => self.begin_cast(nd_webrtc::ID.into(), &sender),
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
            AppMsg::Cast(id) => {
                tracing::info!(source = ?self.source_type, "capture source chosen");
                self.begin_cast(id, &sender);
            }
            AppMsg::CastOnScreen(id, screen) => {
                tracing::info!(%screen, "screen chosen");
                let monitor = wire::source_name(SourceType::Monitor);
                self.cast(id, format!("{monitor}:{screen}"), &sender);
            }
            AppMsg::SwitchStream => {
                self.pending_cast = self.switch_to.take();
                sender.input(AppMsg::Stop);
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
                if let Some(reason) = detail.strip_prefix(SAVE_FAILED) {
                    dialogs::settings_failure(root, reason.trim_start_matches(": "));
                } else {
                    let summary = known_refusal(detail)
                        .unwrap_or_else(|| tr!("The details below say what went wrong."));
                    dialogs::failure(root, &summary, detail, sender.input_sender().clone());
                }
            }
            AppMsg::SettingsChanged(new) => {
                self.settings_changed = true;
                self.failure = None;
                self.options.emit(OptionsMsg::Settings(new));
            }
            AppMsg::SettingsSaved(result) => match result {
                Ok(()) => self.call(
                    &sender,
                    |service| async move { service.reload_settings().await },
                ),
                Err(reason) => {
                    self.failure = Some(format!("{SAVE_FAILED}: {reason}"));
                    self.status = tr!("Could not save settings");
                }
            },
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
                self.push_discovery(Discovery::Unreachable);
                self.options.emit(OptionsMsg::VirtualAvailable(false));
                self.push_devices();
                self.push_media_status();
                self.status = tr!("Reconnecting to the sharing service…");
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

/// The window's menu, flat: GTK leaks unmerged menu sections.
fn main_menu() -> gtk::gio::Menu {
    let menu = gtk::gio::Menu::new();
    menu.append(Some(&tr!("Settings")), Some("win.settings"));
    menu.append(Some(&tr!("About BigNetScreen")), Some("win.about"));
    menu
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
    fn window_tree_finalizes_after_close() {
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
            controller.widgets().sidebar_button.emit_clicked();
            assert!(controller.widgets().split.shows_sidebar());
            // A card of the sidebar and the settings dialog, each opened once.
            controller.model().options.emit(OptionsMsg::Open(0));
            controller.emit(AppMsg::ShowSettings);
            context.block_on(gtk::glib::timeout_future(Duration::from_millis(100)));
            controller.widget().destroy();
            drop(controller);
            context.block_on(gtk::glib::timeout_future(Duration::from_millis(100)));
            assert_eq!(finalized.get(), cycle, "window tree did not finalize");
        }
        println!("window census: new={cycles}, fin={}", finalized.get());
    }
}
