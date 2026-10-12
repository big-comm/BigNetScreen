//! The dialogs the window presents. Each call builds, connects and presents
//! one; a response that needs the window again comes back as an [`AppMsg`].

use std::cell::Cell;
use std::rc::Rc;

use relm4::adw::{self, prelude::*};
use relm4::gtk;

use nd_service::wire;

use super::AppMsg;
use crate::tr;

/// The provider failures behind the banner, in words and as the technical
/// detail. "Got it" dismisses them.
pub(super) fn issues(
    root: &adw::ApplicationWindow,
    issues: &[wire::Issue],
    input: relm4::Sender<AppMsg>,
) {
    let summary = issues
        .iter()
        .map(|issue| super::friendly_reason(&issue.provider, &issue.reason))
        .collect::<Vec<_>>()
        .join("\n\n");
    let detail = issues
        .iter()
        .map(|issue| format!("{}: {}", issue.provider, issue.reason))
        .collect::<Vec<_>>()
        .join("\n");
    let dialog = adw::AlertDialog::new(Some(&tr!("Device discovery problems")), Some(&summary));
    add_detail(root, &dialog, &detail);
    dialog.add_response("dismiss", &tr!("Got it"));
    dialog.set_default_response(Some("dismiss"));
    dialog.connect_response(Some("dismiss"), move |_, _| {
        input.emit(AppMsg::DismissIssues)
    });
    dialog.present(Some(root));
}

/// Puts `detail` under the dialog's text where it can be selected, with a
/// response that copies it, and a Close response.
fn add_detail(root: &adw::ApplicationWindow, dialog: &adw::AlertDialog, detail: &str) {
    let label = gtk::Label::builder()
        .label(detail)
        .selectable(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(["caption", "monospace"])
        .build();
    let scrolled = gtk::ScrolledWindow::builder()
        .child(&label)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .max_content_height(200)
        .propagate_natural_height(true)
        .build();
    dialog.set_extra_child(Some(&scrolled));
    dialog.add_response("close", &tr!("Close"));
    dialog.add_response("copy", &tr!("Copy details"));
    dialog.set_close_response("close");
    // Focus starts on a response, so the detail is not shown all selected.
    dialog.set_default_response(Some("close"));
    label.set_can_focus(false);
    let window = root.downgrade();
    let detail = detail.to_string();
    dialog.connect_response(Some("copy"), move |_, _| {
        if let Some(window) = window.upgrade() {
            window.clipboard().set_text(&detail);
        }
    });
}

/// Which of several screens to share with the receiver `id`.
pub(super) fn choose_screen(
    root: &adw::ApplicationWindow,
    id: String,
    screens: &[(String, String)],
    input: relm4::Sender<AppMsg>,
) {
    let dialog = adw::AlertDialog::new(
        Some(&tr!("Which screen do you want to share?")),
        Some(&tr!(
            "More than one screen is connected. The receiver shows the one you choose."
        )),
    );
    for (name, description) in screens {
        // big-compositor ends the description with the connector; others
        // leave it out, and two identical monitors differ only by it.
        let label = if description.contains(name.as_str()) {
            description.clone()
        } else if description.is_empty() {
            name.clone()
        } else {
            format!("{description} ({name})")
        };
        dialog.add_response(name, &label);
    }
    dialog.add_response("cancel", &tr!("Cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, response| {
        if response != "cancel" {
            input.emit(AppMsg::CastOnScreen(id.clone(), response.to_string()));
        }
    });
    dialog.present(Some(root));
}

/// Why sharing failed, with the technical detail and a way to the settings
/// that can fix it.
pub(super) fn failure(
    root: &adw::ApplicationWindow,
    summary: &str,
    detail: &str,
    input: relm4::Sender<AppMsg>,
) {
    let dialog = adw::AlertDialog::new(Some(&tr!("Sharing could not be completed")), Some(summary));
    add_detail(root, &dialog, detail);
    dialog.add_response("settings", &tr!("Settings"));
    dialog.connect_response(Some("settings"), move |_, _| {
        input.emit(AppMsg::ShowSettings)
    });
    dialog.present(Some(root));
}

/// The preferences could not be saved while the window stays open.
pub(super) fn settings_failure(root: &adw::ApplicationWindow, detail: &str) {
    let dialog = adw::AlertDialog::new(
        Some(&tr!("Could not save settings")),
        Some(&tr!(
            "The changes apply until BigNetScreen closes. Check that your configuration folder can be written."
        )),
    );
    add_detail(root, &dialog, detail);
    dialog.present(Some(root));
}

/// Another transmission is running: stop it and start the new one, or keep it.
pub(super) fn switch_stream(root: &adw::ApplicationWindow, input: relm4::Sender<AppMsg>) {
    let dialog = adw::AlertDialog::new(
        Some(&tr!("Stop the current sharing?")),
        Some(&tr!("The current sharing stops before the new one starts.")),
    );
    dialog.add_response("keep", &tr!("Keep Sharing"));
    dialog.add_response("switch", &tr!("Stop and Switch"));
    dialog.set_response_appearance("switch", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("keep"));
    dialog.set_close_response("keep");
    dialog.connect_response(Some("switch"), move |_, _| input.emit(AppMsg::SwitchStream));
    dialog.present(Some(root));
}

/// The preferences could not be saved on close: keep the window open, or close
/// it and lose them. `allow_close` lets the window's close request through.
pub(super) fn save_failure(
    root: &adw::ApplicationWindow,
    reason: &str,
    allow_close: Rc<Cell<bool>>,
) {
    let dialog = adw::AlertDialog::new(Some(&tr!("Could not save settings")), Some(reason));
    dialog.add_response("back", &tr!("Keep open"));
    dialog.add_response("close", &tr!("Close without saving"));
    dialog.set_default_response(Some("back"));
    dialog.set_close_response("back");
    let window = root.downgrade();
    dialog.connect_response(Some("close"), move |_, _| {
        allow_close.set(true);
        if let Some(window) = window.upgrade() {
            window.close();
        }
    });
    dialog.present(Some(root));
}

/// Shown while the NDI installer runs. It cannot be dismissed; the caller
/// force-closes it when the installer finishes.
pub(super) fn ndi_installing(root: &adw::ApplicationWindow) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::new(
        Some(&tr!("Installing NDI…")),
        Some(&tr!(
            "Authenticate when prompted. Downloading and building the package may take a few minutes. Keep the app open until installation finishes."
        )),
    );
    dialog.set_can_close(false);
    dialog.set_extra_child(Some(&adw::Spinner::new()));
    dialog.present(Some(root));
    dialog
}

/// How the NDI installation ended, with the installer's output on failure.
pub(super) fn ndi_install_result(root: &adw::ApplicationWindow, result: Result<(), String>) {
    let dialog = match result {
        Ok(()) => adw::AlertDialog::new(
            Some(&tr!("NDI installed")),
            Some(&tr!("Restart BigNetScreen to use NDI.")),
        ),
        Err(error) => {
            tracing::warn!(%error, "NDI installation failed or was cancelled");
            let dialog = adw::AlertDialog::new(
                Some(&tr!("NDI installation did not finish")),
                Some(&tr!(
                    "Installation failed or authentication was cancelled. You can try again or install the library manually."
                )),
            );
            let details = gtk::TextView::builder()
                .editable(false)
                .cursor_visible(false)
                .monospace(true)
                .wrap_mode(gtk::WrapMode::WordChar)
                .build();
            details.buffer().set_text(&error);
            let scroll = gtk::ScrolledWindow::builder()
                .min_content_height(140)
                .max_content_height(200)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(&details)
                .build();
            dialog.set_extra_child(Some(&scroll));
            dialog
        }
    };
    dialog.add_response("close", &tr!("Close"));
    dialog.set_close_response("close");
    dialog.present(Some(root));
}

/// NDI needs a proprietary runtime: offers to install it where the installer
/// works, and points to the vendor's guide everywhere else.
pub(super) fn ndi_install_offer(
    root: &adw::ApplicationWindow,
    can_install: bool,
    input: relm4::Sender<AppMsg>,
) {
    let body = if can_install {
        tr!(
            "NDI needs a proprietary library that is not included with BigNetScreen. Install ndi-sdk from the AUR and the required build tools? Your system will ask for administrator authentication. Other sharing methods work without it."
        )
    } else {
        tr!(
            "NDI needs a proprietary library that is not included with BigNetScreen. Install the NDI 5 or 6 runtime for your distribution, then restart the app. Other sharing methods work without it."
        )
    };
    let dialog = adw::AlertDialog::new(Some(&tr!("NDI runtime required")), Some(&body));
    dialog.add_response("close", &tr!("Close"));
    dialog.set_close_response("close");
    let link = if can_install {
        dialog.add_response("install", &tr!("Install"));
        dialog.set_response_appearance("install", adw::ResponseAppearance::Suggested);
        dialog.connect_response(Some("install"), move |_, _| {
            input.emit(AppMsg::InstallNdi);
        });
        gtk::LinkButton::with_label(crate::ndi_setup::AUR_URL, &tr!("View ndi-sdk in the AUR"))
    } else {
        gtk::LinkButton::with_label(crate::ndi_setup::GUIDE_URL, &tr!("NDI setup"))
    };
    dialog.set_extra_child(Some(&link));
    dialog.present(Some(root));
}

pub(super) fn about(root: &adw::ApplicationWindow) {
    let about = adw::AboutDialog::builder()
        .application_name("BigNetScreen")
        .application_icon("br.com.biglinux.BigNetScreen")
        .version(crate::APP_VERSION)
        .developer_name("BigCommunity")
        .license_type(gtk::License::Gpl30)
        .website("https://github.com/big-comm/BigNetScreen")
        .comments(tr!(
            "Share your screen with TVs, projectors and web browsers"
        ))
        .build();
    about.present(Some(root));
}
