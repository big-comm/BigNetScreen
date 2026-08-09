//! The application's root component (relm4 + libadwaita).
//!
//! It discovers receivers (Chromecast/AirPlay over mDNS, Miracast over Wi-Fi
//! Direct) through a `MetaProvider` running in the background and shows each
//! one as a clickable row. Clicking starts the screen capture and the cast.
//!
//! Design points worth knowing:
//!
//! - **failures are visible**: an unavailable provider becomes a banner
//!   explaining why, instead of a `warn` in the log with the UI stuck on
//!   "Searching…";
//! - **there is a way to retry**: a rescan button in the header;
//! - **the empty state has a deadline**: after a few seconds with nothing
//!   found, the screen explains what to check instead of spinning forever;
//! - **an existing receiver is updated, not replaced**: swapping the `Arc`
//!   mid-session would lose the connection state;
//! - all text goes through `tr!()` (gettext).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use relm4::adw::{self, prelude::*};
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender, FactoryVecDeque};
use relm4::gtk;
use relm4::prelude::*;

use nd_chromecast::MdnsProvider;
use nd_core::capture::SourceType;
use nd_core::meta::MetaProvider;
use nd_core::provider::{DiscoveryEvent, Provider};
use nd_core::sink::{Sink, SinkKind, SinkState};
use nd_wfd::WfdP2pProvider;

use crate::tr;

/// After this long with no receiver, the screen stops saying "searching".
const EMPTY_HINT_AFTER: Duration = Duration::from_secs(12);
/// How often the UI re-reads the state of sinks in a session.
const STATE_POLL: Duration = Duration::from_millis(400);

fn protocol_label(kind: SinkKind) -> String {
    match kind {
        SinkKind::Chromecast => tr!("Chromecast"),
        SinkKind::AirPlay => tr!("AirPlay"),
        SinkKind::WfdP2p | SinkKind::WfdMice => tr!("Miracast"),
        SinkKind::Dummy => tr!("Test"),
    }
}

fn icon_for(kind: SinkKind) -> &'static str {
    match kind {
        SinkKind::Chromecast => "tv-symbolic",
        SinkKind::AirPlay => "display-projector-symbolic",
        SinkKind::WfdP2p | SinkKind::WfdMice => "video-display-symbolic",
        SinkKind::Dummy => "applications-system-symbolic",
    }
}

fn state_label(state: SinkState) -> String {
    match state {
        SinkState::Disconnected => String::new(),
        SinkState::Connecting => tr!("Connecting…"),
        SinkState::EnsuringFirewall => tr!("Opening the firewall port…"),
        SinkState::WaitSocket => tr!("Waiting for the receiver…"),
        SinkState::WaitStreaming => tr!("Preparing the video…"),
        SinkState::Streaming => tr!("Streaming"),
        SinkState::Error => tr!("Failed"),
    }
}

/// Wrapper for carrying an `Arc<dyn Sink>` as a `CommandOutput`.
#[derive(Clone)]
pub struct DiscoveredSink(pub Arc<dyn Sink>);

impl std::fmt::Debug for DiscoveredSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DiscoveredSink({})", self.0.info().id)
    }
}

// ----------------------------------------------------------------------------
// A row in the list (one receiver)
// ----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SinkRowInit {
    pub id: String,
    pub name: String,
    pub subtitle: String,
    pub icon: &'static str,
    pub castable: bool,
}

#[derive(Debug)]
pub struct SinkRow {
    id: String,
    name: String,
    subtitle: String,
    icon: &'static str,
    castable: bool,
    state: SinkState,
    detail: String,
}

#[derive(Debug)]
pub enum SinkRowMsg {
    /// Estado/mensagem vindos do sink correspondente.
    Update {
        state: SinkState,
        message: Option<String>,
        subtitle: String,
        name: String,
    },
}

#[derive(Debug)]
pub enum SinkRowOutput {
    Activated(String),
}

#[relm4::factory(pub)]
impl FactoryComponent for SinkRow {
    type Init = SinkRowInit;
    type Input = SinkRowMsg;
    type Output = SinkRowOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::ListBox;

    view! {
        adw::ActionRow {
            #[watch]
            set_title: &self.name,
            #[watch]
            set_subtitle: &self.display_subtitle(),
            #[watch]
            set_activatable: self.castable && !self.state.is_busy(),
            #[watch]
            set_sensitive: self.castable,

            add_prefix = &gtk::Image {
                set_icon_name: Some(self.icon),
            },

            add_suffix = &adw::Spinner {
                #[watch]
                set_visible: self.state.is_busy(),
            },

            add_suffix = &gtk::Image {
                set_icon_name: Some("dialog-error-symbolic"),
                add_css_class: "error",
                #[watch]
                set_visible: self.state == SinkState::Error,
            },

            add_suffix = &gtk::Image {
                set_icon_name: Some("media-playback-start-symbolic"),
                add_css_class: "success",
                #[watch]
                set_visible: self.state == SinkState::Streaming,
            },

            add_suffix = &gtk::Image {
                set_icon_name: Some("go-next-symbolic"),
                add_css_class: "dim-label",
                #[watch]
                set_visible: self.castable
                    && matches!(self.state, SinkState::Disconnected),
            },

            connect_activated[sender, id = self.id.clone()] => move |_| {
                sender.output(SinkRowOutput::Activated(id.clone())).ok();
            },
        }
    }

    fn init_model(init: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self {
            id: init.id,
            name: init.name,
            subtitle: init.subtitle,
            icon: init.icon,
            castable: init.castable,
            state: SinkState::Disconnected,
            detail: String::new(),
        }
    }

    fn update(&mut self, message: Self::Input, _sender: FactorySender<Self>) {
        match message {
            SinkRowMsg::Update {
                state,
                message,
                subtitle,
                name,
            } => {
                self.state = state;
                self.subtitle = subtitle;
                self.name = name;
                self.detail = message.unwrap_or_default();
            }
        }
    }
}

impl SinkRow {
    /// The subtitle shown: protocol + address, or the state when there is
    /// something to say (progress, or the cause of an error).
    fn display_subtitle(&self) -> String {
        if self.state == SinkState::Error && !self.detail.is_empty() {
            return self.detail.clone();
        }
        let state = state_label(self.state);
        if state.is_empty() {
            if self.castable {
                self.subtitle.clone()
            } else {
                format!("{} · {}", self.subtitle, tr!("discovery only"))
            }
        } else {
            format!("{} · {}", self.subtitle, state)
        }
    }
}

// ----------------------------------------------------------------------------
// Janela principal
// ----------------------------------------------------------------------------

/// A banner about an unavailable protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProviderIssue {
    provider: &'static str,
    reason: String,
}

pub struct AppModel {
    sinks: FactoryVecDeque<SinkRow>,
    /// Receptores descobertos, por id.
    registry: HashMap<String, Arc<dyn Sink>>,
    status: String,
    /// Providers that failed to start (shown as a banner).
    issues: Vec<ProviderIssue>,
    /// `true` enquanto vale mostrar "procurando".
    searching: bool,
    /// The running cast session (the sink's id).
    active_cast: Option<String>,
    /// What to capture when the user picks a receiver.
    source_type: SourceType,
    /// The scan generation: discards events from an older discovery run.
    generation: u64,
}

#[derive(Debug)]
pub enum AppMsg {
    Activated(String),
    /// Stop the running stream.
    Stop,
    /// Change what will be captured (the whole screen or a window).
    SetSource(SourceType),
    Rescan,
    DismissIssues,
}

#[derive(Debug)]
pub enum AppCmd {
    Added(DiscoveredSink, u64),
    Updated(DiscoveredSink, u64),
    Removed(String, u64),
    ProviderUnavailable {
        provider: &'static str,
        reason: String,
        generation: u64,
    },
    ProviderReady {
        provider: &'static str,
        generation: u64,
    },
    /// Prazo do "procurando" esgotado.
    SearchTimedOut(u64),
    /// The periodic re-read of the sinks' state.
    PollStates,
    /// A cast session ended.
    CastFinished {
        id: String,
        error: Option<String>,
    },
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
            set_default_width: 460,
            set_default_height: 700,

            adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {
                    #[wrap(Some)]
                    set_title_widget = &adw::WindowTitle {
                        set_title: "BigNetScreen",
                        #[watch]
                        set_subtitle: &model.status,
                    },

                    pack_end = &gtk::Button {
                        set_icon_name: "view-refresh-symbolic",
                        set_tooltip_text: Some(&tr!("Scan again")),
                        #[watch]
                        set_sensitive: model.active_cast.is_none(),
                        connect_clicked => AppMsg::Rescan,
                    },

                    pack_start = &gtk::Button {
                        set_label: &tr!("Stop"),
                        add_css_class: "destructive-action",
                        #[watch]
                        set_visible: model.active_cast.is_some(),
                        connect_clicked => AppMsg::Stop,
                    },
                },

                #[wrap(Some)]
                set_content = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,

                    // Banners for unavailable protocols: the reason is
                    // visible to the user, not just in the log.
                    adw::Banner {
                        #[watch]
                        set_revealed: !model.issues.is_empty(),
                        #[watch]
                        set_title: &model.issues_summary(),
                        set_button_label: Some(&tr!("Got it")),
                        connect_button_clicked => AppMsg::DismissIssues,
                    },

                    // What to capture. The portal still asks *which* monitor
                    // or window; here the user picks the kind.
                    adw::Clamp {
                        set_maximum_size: 520,
                        set_margin_top: 12,
                        set_margin_start: 12,
                        set_margin_end: 12,

                        // It has to be a `ListBox`: `adw::ActionRow` is a
                        // `GtkListBoxRow`, and loose inside a `gtk::Box` GTK
                        // complains when trying to focus the row.
                        gtk::ListBox {
                            set_selection_mode: gtk::SelectionMode::None,
                            add_css_class: "boxed-list",

                            adw::ActionRow {
                                set_title: &tr!("What to share"),
                                #[watch]
                                set_sensitive: model.active_cast.is_none(),

                                add_suffix = &gtk::Box {
                                    set_valign: gtk::Align::Center,
                                    add_css_class: "linked",

                                    gtk::ToggleButton {
                                        set_label: &tr!("Whole screen"),
                                        #[watch]
                                        set_active: model.source_type == SourceType::Monitor,
                                        connect_toggled[sender] => move |btn| {
                                            if btn.is_active() {
                                                sender.input(AppMsg::SetSource(SourceType::Monitor));
                                            }
                                        },
                                    },

                                    gtk::ToggleButton {
                                        set_label: &tr!("A window"),
                                        #[watch]
                                        set_active: model.source_type == SourceType::Window,
                                        connect_toggled[sender] => move |btn| {
                                            if btn.is_active() {
                                                sender.input(AppMsg::SetSource(SourceType::Window));
                                            }
                                        },
                                    },
                                },
                            },
                        },
                    },

                    gtk::ScrolledWindow {
                        set_vexpand: true,
                        set_hscrollbar_policy: gtk::PolicyType::Never,

                        #[wrap(Some)]
                        set_child = &adw::Clamp {
                            set_maximum_size: 520,
                            set_margin_top: 18,
                            set_margin_bottom: 18,
                            set_margin_start: 12,
                            set_margin_end: 12,

                            #[local_ref]
                            sinks_box -> gtk::ListBox {
                                set_selection_mode: gtk::SelectionMode::None,
                                set_valign: gtk::Align::Start,
                                add_css_class: "boxed-list",
                            },
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
        let sinks =
            FactoryVecDeque::builder()
                .launch_default()
                .forward(sender.input_sender(), |output| match output {
                    SinkRowOutput::Activated(id) => AppMsg::Activated(id),
                });

        let model = AppModel {
            sinks,
            registry: HashMap::new(),
            status: tr!("Searching…"),
            issues: Vec::new(),
            searching: true,
            active_cast: None,
            source_type: SourceType::Monitor,
            generation: 0,
        };

        let placeholder = build_placeholder();
        model.sinks.widget().set_placeholder(Some(&placeholder));

        let sinks_box = model.sinks.widget();
        let widgets = view_output!();

        start_discovery(&sender, 0);
        start_state_poll(&sender);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match message {
            AppMsg::Activated(id) => self.begin_cast(id, &sender),
            AppMsg::Stop => self.stop_cast(&sender),
            AppMsg::SetSource(source) => {
                self.source_type = source;
                tracing::info!(?source, "fonte de captura escolhida");
            }
            AppMsg::Rescan => {
                // Invalida a descoberta anterior e limpa a lista.
                self.generation += 1;
                self.issues.clear();
                self.searching = true;
                self.registry.clear();
                self.sinks.guard().clear();
                self.status = tr!("Searching…");
                start_discovery(&sender, self.generation);
            }
            AppMsg::DismissIssues => self.issues.clear(),
        }
    }

    fn update_cmd(
        &mut self,
        message: Self::CommandOutput,
        sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match message {
            AppCmd::Added(handle, generation) => {
                if generation != self.generation {
                    return;
                }
                let info = handle.0.info();
                let subtitle = subtitle_for(&info);
                let mut guard = self.sinks.guard();
                if !guard.iter().any(|r| r.id == info.id) {
                    guard.push_back(SinkRowInit {
                        id: info.id.clone(),
                        name: info.display_name.clone(),
                        subtitle,
                        icon: icon_for(info.kind),
                        castable: info.kind.is_castable(),
                    });
                }
                drop(guard);
                // `entry`: an mDNS re-resolve must not replace the instance —
                // it may be in the middle of a session.
                self.registry.entry(info.id).or_insert(handle.0);
                self.searching = false;
                self.refresh_status();
            }
            AppCmd::Updated(handle, generation) => {
                if generation != self.generation {
                    return;
                }
                let info = handle.0.info();
                let subtitle = subtitle_for(&info);
                if let Some(index) = self.index_of(&info.id) {
                    self.sinks.send(
                        index,
                        SinkRowMsg::Update {
                            state: handle.0.state(),
                            message: handle.0.error_message(),
                            subtitle,
                            name: info.display_name,
                        },
                    );
                }
            }
            AppCmd::Removed(id, generation) => {
                if generation != self.generation {
                    return;
                }
                // Never remove the receiver that is streaming right now: a
                // momentary mDNS dropout would take the active session's row
                // away.
                if self.active_cast.as_deref() == Some(id.as_str()) {
                    return;
                }
                if let Some(index) = self.index_of(&id) {
                    self.sinks.guard().remove(index);
                }
                self.registry.remove(&id);
                self.refresh_status();
            }
            AppCmd::ProviderUnavailable {
                provider,
                reason,
                generation,
            } => {
                if generation != self.generation {
                    return;
                }
                let issue = ProviderIssue { provider, reason };
                if !self.issues.contains(&issue) {
                    self.issues.push(issue);
                }
            }
            AppCmd::ProviderReady {
                provider,
                generation,
            } => {
                if generation != self.generation {
                    return;
                }
                self.issues.retain(|i| i.provider != provider);
            }
            AppCmd::SearchTimedOut(generation) => {
                if generation == self.generation && self.registry.is_empty() {
                    self.searching = false;
                    self.refresh_status();
                }
            }
            AppCmd::PollStates => {
                self.sync_states();
                start_state_poll(&sender);
            }
            AppCmd::CastFinished { id, error } => {
                if self.active_cast.as_deref() == Some(id.as_str()) {
                    self.active_cast = None;
                }
                match error {
                    Some(err) => {
                        tracing::warn!(%id, %err, "cast session ended with an error");
                        self.status = err;
                    }
                    None => {
                        tracing::info!(%id, "cast session ended");
                        self.refresh_status();
                    }
                }
                self.sync_states();
            }
        }
    }
}

impl AppModel {
    fn index_of(&self, id: &str) -> Option<usize> {
        self.sinks.iter().position(|r| r.id == id)
    }

    fn refresh_status(&mut self) {
        let n = self.registry.len();
        self.status = if n == 0 {
            if self.searching {
                tr!("Searching…")
            } else {
                tr!("No receivers found")
            }
        } else if n == 1 {
            tr!("1 receiver found")
        } else {
            format!("{n} {}", tr!("receivers found"))
        };
    }

    fn issues_summary(&self) -> String {
        self.issues
            .iter()
            .map(|i| i.reason.clone())
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// Re-reads each sink's state and reflects it in the rows.
    fn sync_states(&mut self) {
        let updates: Vec<(usize, SinkState, Option<String>, String, String)> = self
            .sinks
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                let sink = self.registry.get(&row.id)?;
                let state = sink.state();
                let message = sink.error_message();
                if state == row.state && message.unwrap_or_default() == row.detail {
                    return None;
                }
                let info = sink.info();
                Some((
                    index,
                    state,
                    sink.error_message(),
                    subtitle_for(&info),
                    info.display_name,
                ))
            })
            .collect();

        for (index, state, message, subtitle, name) in updates {
            self.sinks.send(
                index,
                SinkRowMsg::Update {
                    state,
                    message,
                    subtitle,
                    name,
                },
            );
        }
    }

    /// Ends the running stream.
    fn stop_cast(&mut self, sender: &ComponentSender<Self>) {
        let Some(id) = self.active_cast.clone() else {
            return;
        };
        let Some(sink) = self.registry.get(&id).cloned() else {
            return;
        };
        tracing::info!(%id, "stopping the stream at the user's request");
        self.status = tr!("Stopping…");
        // `stop_stream` signals the session; the `begin_cast` command returns
        // on its own and emits `CastFinished`.
        sender.oneshot_command(async move {
            let _ = sink.stop_stream().await;
            AppCmd::PollStates
        });
    }

    fn begin_cast(&mut self, id: String, sender: &ComponentSender<Self>) {
        if self.active_cast.is_some() {
            self.status = tr!("A stream is already running");
            return;
        }
        let Some(sink) = self.registry.get(&id).cloned() else {
            return;
        };
        let info = sink.info();
        if !info.kind.is_castable() {
            self.status = format!(
                "{} {}",
                info.display_name,
                tr!("shows up in discovery only")
            );
            return;
        }

        tracing::info!(name = %info.display_name, "starting the stream");
        self.status = format!("{} · {}", info.display_name, tr!("Connecting…"));
        self.active_cast = Some(id.clone());

        let source_type = self.source_type;
        sender.oneshot_command(async move {
            let error = run_cast(sink, source_type).await.err();
            AppCmd::CastFinished { id, error }
        });
    }
}

fn subtitle_for(info: &nd_core::sink::SinkInfo) -> String {
    let base = match &info.address {
        Some(addr) => format!("{} · {}", protocol_label(info.kind), addr),
        None => protocol_label(info.kind),
    };
    match latency_hint(info.kind) {
        Some(hint) => format!("{base} · {hint}"),
        None => base,
    }
}

/// The delay to expect, per protocol.
///
/// Not a cosmetic detail: the two paths differ by an order of magnitude, and
/// the user needs to know that **before** choosing. Miracast opens a direct
/// link and we control the latency end to end. Chromecast uses the device's
/// mirroring app, and only falls back to the media player — which buffers for
/// seconds — on receivers without mirroring support.
fn latency_hint(kind: SinkKind) -> Option<String> {
    match kind {
        SinkKind::WfdP2p | SinkKind::WfdMice => Some(tr!("instant response")),
        // Chromecast uses the mirroring app (direct RTP). It only falls back
        // to the HTTP path — which does cost seconds — on devices without
        // mirroring, and the row itself shows that state.
        SinkKind::Chromecast => Some(tr!("quick response")),
        _ => None,
    }
}

/// Runs a cast session: screen capture plus sink.
async fn run_cast(sink: Arc<dyn Sink>, source_type: SourceType) -> std::result::Result<(), String> {
    let backend = nd_capture::select_backend_for(source_type).await;
    let source = backend
        .start(source_type)
        .await
        .map_err(|e| e.to_string())?;

    let result = sink.start_stream(source).await.map_err(|e| e.to_string());
    let _ = backend.stop().await;
    result
}

fn start_state_poll(sender: &ComponentSender<AppModel>) {
    sender.oneshot_command(async move {
        tokio::time::sleep(STATE_POLL).await;
        AppCmd::PollStates
    });
}

/// Dispara a descoberta e o prazo do estado vazio.
fn start_discovery(sender: &ComponentSender<AppModel>, generation: u64) {
    sender.command(move |out, shutdown| {
        shutdown
            .register(async move { run_discovery(out, generation).await })
            .drop_on_shutdown()
    });

    sender.oneshot_command(async move {
        tokio::time::sleep(EMPTY_HINT_AFTER).await;
        AppCmd::SearchTimedOut(generation)
    });
}

/// Runs discovery across every provider and emits events to the UI.
async fn run_discovery(out: relm4::Sender<AppCmd>, generation: u64) {
    let mut providers: Vec<Arc<dyn Provider>> = Vec::new();

    // A single mDNS daemon for both Chromecast and AirPlay.
    match MdnsProvider::all_media_receivers() {
        Ok(provider) => providers.push(Arc::new(provider)),
        Err(err) => {
            let _ = out.send(AppCmd::ProviderUnavailable {
                provider: "mdns",
                reason: format!("{}: {err}", tr!("Local network discovery unavailable")),
                generation,
            });
        }
    }

    providers.push(Arc::new(WfdP2pProvider));

    if std::env::var_os("NETWORK_DISPLAYS_DUMMY").is_some() {
        providers.push(Arc::new(nd_core::dummy::DummyProvider));
    }

    let meta = MetaProvider::new(providers);
    let mut stream = meta.discover().await;

    while let Some(event) = stream.next().await {
        let cmd = match event {
            DiscoveryEvent::Added(sink) => AppCmd::Added(DiscoveredSink(sink), generation),
            DiscoveryEvent::Updated(sink) => AppCmd::Updated(DiscoveredSink(sink), generation),
            DiscoveryEvent::Removed(id) => AppCmd::Removed(id, generation),
            DiscoveryEvent::ProviderUnavailable { provider, reason } => {
                AppCmd::ProviderUnavailable {
                    provider,
                    reason: friendly_reason(provider, &reason),
                    generation,
                }
            }
            DiscoveryEvent::ProviderReady { provider } => AppCmd::ProviderReady {
                provider,
                generation,
            },
        };
        if out.send(cmd).is_err() {
            break;
        }
    }
}

/// Turns the provider's technical error into something the user can act on.
fn friendly_reason(provider: &str, reason: &str) -> String {
    if provider == "wfd-p2p" {
        if nd_capture::is_sandboxed() {
            return tr!(
                "Miracast does not work in the Flatpak build (it needs the system NetworkManager)"
            );
        }
        if reason.contains("Wi-Fi P2P") || reason.contains("Wi-Fi Direct") {
            return tr!("Miracast unavailable: this Wi-Fi card does not support Wi-Fi Direct");
        }
        return format!("{}: {reason}", tr!("Miracast unavailable"));
    }
    reason.to_string()
}

/// Estado vazio do `ListBox`.
fn build_placeholder() -> adw::StatusPage {
    let page = adw::StatusPage::builder()
        .icon_name("video-display-symbolic")
        .title(tr!("Looking for receivers…"))
        .description(tr!(
            "Chromecasts and TVs show up on their own. For Miracast, put the TV \
             or the projector into “Screen Mirroring” mode."
        ))
        .build();
    page.add_css_class("compact");
    page
}
