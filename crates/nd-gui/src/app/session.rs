//! The window's side of a session: calls to the service, starting a cast,
//! and folding the service's state into what each page shows.

use super::*;

impl AppModel {
    /// Tells the receiver list where discovery stands.
    pub(super) fn push_discovery(&self, discovery: Discovery) {
        self.share_page.emit(ShareMsg::Discovery(discovery));
    }

    /// Is anything running that the Stop button should end?
    pub(super) fn is_busy(&self) -> bool {
        !self.state.session.is_idle() || self.state.media.active
    }

    /// Calls the service, turning a refusal into something the window can say.
    ///
    /// Every action goes through here so that no call site has to remember
    /// that the service may not be there yet.
    pub(super) fn call<F, Fut>(&self, sender: &ComponentSender<Self>, call: F)
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

    pub(super) fn call_with_settings<F, Fut>(&self, sender: &ComponentSender<Self>, call: F)
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
                    .map_err(|err| zbus::Error::Failure(format!("{SAVE_FAILED}: {err}")))?;
            }
            service.reload_settings().await?;
            call(service).await
        });
    }

    pub(super) fn begin_cast(&mut self, id: String, sender: &ComponentSender<Self>) {
        self.failure = None;
        if self.is_busy() {
            self.switch_to = Some(id);
            if let Some(root) = self
                .share_page
                .widget()
                .root()
                .and_downcast::<adw::ApplicationWindow>()
            {
                dialogs::switch_stream(&root, sender.input_sender().clone());
            }
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
    pub(super) fn cast(&mut self, id: String, source: String, sender: &ComponentSender<Self>) {
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
    pub(super) fn apply_state(&mut self, state: ServiceState, sender: &ComponentSender<Self>) {
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
            self.options
                .emit(OptionsMsg::VirtualAvailable(self.state.virtual_available));
        }
        // The status line changes without focus moving, so a screen reader
        // is told about the moments that matter.
        if searching_changed
            && matches!(
                self.state.status.kind(),
                Some(StatusKind::Connecting | StatusKind::Streaming | StatusKind::Error)
            )
            && let Some(window) = self.share_page.widget().root()
        {
            window.announce(&self.status, gtk::AccessibleAnnouncementPriority::Medium);
        }
        if searching_changed {
            // A session's own kinds say nothing about discovery.
            let discovery = match self.state.status.kind() {
                Some(StatusKind::Searching) => Some(Discovery::Searching),
                Some(StatusKind::DiscoveryOff) => Some(Discovery::Off),
                Some(StatusKind::Found | StatusKind::Empty) => Some(Discovery::Finished),
                _ => None,
            };
            if let Some(discovery) = discovery {
                self.push_discovery(discovery);
            }
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
        if !self.is_busy()
            && let Some(id) = self.pending_cast.take()
        {
            self.begin_cast(id, sender);
        }
    }

    /// Pushes the receivers and the session to the parts that show them.
    pub(super) fn push_devices(&mut self) {
        self.share_page
            .emit(ShareMsg::Devices(self.state.entries()));
        self.share_page
            .emit(ShareMsg::Session(self.state.session_info()));
    }

    /// Tells the share page whether files are shared, and whether any is
    /// chosen to send.
    pub(super) fn push_files_mode(&self) {
        self.share_page.emit(ShareMsg::Files {
            files: self.share == Share::Files,
            chosen: self.files_chosen,
        });
    }

    pub(super) fn push_media_status(&mut self) {
        if !self.state.media.active {
            self.media.emit(MediaMsg::Status(None));
            return;
        }
        self.media
            .emit(MediaMsg::Status(Some(self.state.media_status())));
    }

    /// The banner text: what is wrong that the person has not already read.
    pub(super) fn issues_summary(&self) -> String {
        self.state
            .issues
            .iter()
            .filter(|issue| !self.dismissed.contains(issue))
            .map(|issue| friendly_reason(&issue.provider, &issue.reason))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}
