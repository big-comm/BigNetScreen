//! Componente raiz da aplicação (relm4 + libadwaita).
//!
//! Fase 1: descobre receptores reais (Chromecast/AirPlay via mDNS, Miracast via
//! Wi-Fi Direct) num `MetaProvider` rodando como *command* em background, e
//! mostra cada um numa lista dinâmica clicável (`FactoryVecDeque`). Clicar
//! seleciona o receptor e dá feedback; o cast em si entra nas Fases 2c/3c.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use relm4::adw::{self, prelude::*};
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender, FactoryVecDeque};
use relm4::gtk;
use relm4::prelude::*;

use nd_chromecast::MdnsProvider;
use nd_core::meta::MetaProvider;
use nd_core::provider::{DiscoveryEvent, Provider};
use nd_core::sink::{Sink, SinkKind};
use nd_wfd::WfdP2pProvider;

/// Rótulo amigável do protocolo de um receptor.
fn protocol_label(kind: SinkKind) -> &'static str {
    match kind {
        SinkKind::Chromecast => "Chromecast",
        SinkKind::AirPlay => "AirPlay",
        SinkKind::WfdP2p | SinkKind::WfdMice => "Miracast",
        SinkKind::Dummy => "Teste",
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

/// Wrapper para transportar um `Arc<dyn Sink>` como `CommandOutput` (que exige
/// `Debug`).
#[derive(Clone)]
pub struct DiscoveredSink(pub Arc<dyn Sink>);

impl std::fmt::Debug for DiscoveredSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DiscoveredSink({})", self.0.info().id)
    }
}

// ----------------------------------------------------------------------------
// Linha da lista (um receptor)
// ----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SinkRowInit {
    pub id: String,
    pub name: String,
    pub subtitle: String,
    pub icon: &'static str,
}

pub struct SinkRow {
    id: String,
    name: String,
    subtitle: String,
    icon: &'static str,
}

#[derive(Debug)]
pub enum SinkRowOutput {
    Activated(String),
}

#[relm4::factory(pub)]
impl FactoryComponent for SinkRow {
    type Init = SinkRowInit;
    type Input = ();
    type Output = SinkRowOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::ListBox;

    view! {
        adw::ActionRow {
            set_title: &self.name,
            set_subtitle: &self.subtitle,
            set_activatable: true,
            add_prefix = &gtk::Image {
                set_icon_name: Some(self.icon),
            },
            add_suffix = &gtk::Image {
                set_icon_name: Some("go-next-symbolic"),
                add_css_class: "dim-label",
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
        }
    }
}

// ----------------------------------------------------------------------------
// Janela principal
// ----------------------------------------------------------------------------

pub struct AppModel {
    sinks: FactoryVecDeque<SinkRow>,
    /// Receptores descobertos, por id — base para iniciar o cast (Fases 2c/3c).
    registry: HashMap<String, Arc<dyn Sink>>,
    /// Texto do subtítulo do cabeçalho (feedback de seleção).
    status: String,
}

#[derive(Debug)]
pub enum AppMsg {
    /// Usuário clicou num receptor (por id).
    Activated(String),
}

#[derive(Debug)]
pub enum AppCmd {
    Added(DiscoveredSink),
    Removed(String),
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
            set_default_width: 440,
            set_default_height: 680,

            adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {
                    #[wrap(Some)]
                    set_title_widget = &adw::WindowTitle {
                        set_title: "BigNetScreen",
                        #[watch]
                        set_subtitle: &model.status,
                    },
                },

                #[wrap(Some)]
                set_content = &gtk::ScrolledWindow {
                    set_vexpand: true,
                    set_hscrollbar_policy: gtk::PolicyType::Never,

                    #[wrap(Some)]
                    set_child = &adw::Clamp {
                        set_maximum_size: 500,
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
            }
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let sinks = FactoryVecDeque::builder()
            .launch_default()
            .forward(sender.input_sender(), |output| match output {
                SinkRowOutput::Activated(id) => AppMsg::Activated(id),
            });

        let model = AppModel {
            sinks,
            registry: HashMap::new(),
            status: "Procurando…".to_string(),
        };

        let placeholder = build_searching_placeholder();
        model.sinks.widget().set_placeholder(Some(&placeholder));

        let sinks_box = model.sinks.widget();
        let widgets = view_output!();

        sender.command(|out, shutdown| {
            shutdown
                .register(async move { run_discovery(out).await })
                .drop_on_shutdown()
        });

        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Self::Input, _sender: ComponentSender<Self>, _root: &Self::Root) {
        match message {
            AppMsg::Activated(id) => {
                if let Some(sink) = self.registry.get(&id) {
                    let info = sink.info();
                    tracing::info!(name = %info.display_name, "receptor selecionado");
                    self.status = format!(
                        "{} · {} — cast em desenvolvimento",
                        info.display_name,
                        protocol_label(info.kind),
                    );
                }
            }
        }
    }

    fn update_cmd(
        &mut self,
        message: Self::CommandOutput,
        _sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match message {
            AppCmd::Added(handle) => {
                let info = handle.0.info();
                let mut guard = self.sinks.guard();
                let exists = guard.iter().any(|r| r.id == info.id);
                if !exists {
                    let subtitle = match &info.address {
                        Some(addr) => format!("{} · {}", protocol_label(info.kind), addr),
                        None => protocol_label(info.kind).to_string(),
                    };
                    guard.push_back(SinkRowInit {
                        id: info.id.clone(),
                        name: info.display_name.clone(),
                        subtitle,
                        icon: icon_for(info.kind),
                    });
                }
                drop(guard);
                self.registry.insert(info.id, handle.0);
                self.update_status_count();
            }
            AppCmd::Removed(id) => {
                let mut guard = self.sinks.guard();
                let index = guard.iter().position(|r| r.id == id);
                if let Some(index) = index {
                    guard.remove(index);
                }
                drop(guard);
                self.registry.remove(&id);
                self.update_status_count();
            }
        }
    }
}

impl AppModel {
    fn update_status_count(&mut self) {
        let n = self.sinks.len();
        self.status = match n {
            0 => "Procurando…".to_string(),
            1 => "1 receptor encontrado".to_string(),
            _ => format!("{n} receptores encontrados"),
        };
    }
}

/// Roda a descoberta de todos os providers e emite eventos para a UI.
async fn run_discovery(out: relm4::Sender<AppCmd>) {
    let mut providers: Vec<Arc<dyn Provider>> = Vec::new();
    match MdnsProvider::chromecast() {
        Ok(provider) => providers.push(Arc::new(provider)),
        Err(err) => tracing::error!(%err, "descoberta Chromecast indisponível"),
    }
    match MdnsProvider::airplay() {
        Ok(provider) => providers.push(Arc::new(provider)),
        Err(err) => tracing::error!(%err, "descoberta AirPlay indisponível"),
    }
    providers.push(Arc::new(WfdP2pProvider));

    if std::env::var_os("NETWORK_DISPLAYS_DUMMY").is_some() {
        providers.push(Arc::new(nd_core::dummy::DummyProvider));
    }

    let meta = MetaProvider::new(providers);
    let mut stream = meta.discover().await;

    while let Some(event) = stream.next().await {
        let cmd = match event {
            DiscoveryEvent::Added(sink) => AppCmd::Added(DiscoveredSink(sink)),
            DiscoveryEvent::Removed(id) => AppCmd::Removed(id),
        };
        if out.send(cmd).is_err() {
            break;
        }
    }
}

/// Placeholder animado de "procurando…" para o `ListBox` vazio.
fn build_searching_placeholder() -> gtk::Box {
    let container = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .margin_top(24)
        .margin_bottom(24)
        .build();

    let spinner = gtk::Spinner::builder()
        .spinning(true)
        .width_request(36)
        .height_request(36)
        .build();
    container.append(&spinner);

    let title = gtk::Label::builder().label("Procurando receptores…").build();
    title.add_css_class("title-2");
    container.append(&title);

    let description = gtk::Label::builder()
        .label("Dispositivos Chromecast e Miracast na sua rede aparecerão aqui.")
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    description.add_css_class("dim-label");
    container.append(&description);

    container
}
