//! Componente raiz da aplicação (relm4).
//!
//! Esqueleto da Fase 1: janela libadwaita moderna com uma `AdwStatusPage` de
//! "procurando receptores". A lista de sinks (alimentada pelo `MetaProvider`)
//! e o fluxo de cast entram nas fases seguintes.

use relm4::adw::{self, prelude::*};
use relm4::prelude::*;

/// Estado da aplicação.
pub struct AppModel {
    /// Quantidade de receptores descobertos (placeholder até o MetaProvider).
    sink_count: usize,
}

/// Mensagens de entrada do componente.
#[derive(Debug)]
pub enum AppMsg {
    /// Reiniciar a varredura por receptores.
    Rescan,
}

#[relm4::component(pub)]
impl SimpleComponent for AppModel {
    type Init = ();
    type Input = AppMsg;
    type Output = ();

    view! {
        adw::ApplicationWindow {
            set_title: Some("BigNetScreen"),
            set_default_width: 420,
            set_default_height: 640,

            adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {
                    #[wrap(Some)]
                    set_title_widget = &adw::WindowTitle {
                        set_title: "BigNetScreen",
                        set_subtitle: "Transmitir a tela",
                    },
                },

                #[wrap(Some)]
                set_content = &adw::StatusPage {
                    set_icon_name: Some("video-display-symbolic"),
                    set_title: "Procurando receptores…",
                    set_description: Some(
                        "Dispositivos Chromecast e Miracast na sua rede aparecerão aqui.",
                    ),
                },
            }
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = AppModel { sink_count: 0 };
        let widgets = view_output!();

        // Dispara a primeira varredura assim que a UI sobe.
        sender.input(AppMsg::Rescan);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            AppMsg::Rescan => {
                // TODO Fase 2/3: iniciar ChromecastProvider + WfdP2pProvider e
                // popular a lista de sinks.
                tracing::debug!("rescan solicitado (sinks atuais: {})", self.sink_count);
            }
        }
    }
}
