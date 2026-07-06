//! The header-bar profile button and the light popover it opens.
//!
//! The popover is menu-weight — who you are, which send-email identity is
//! active, where mail goes, and two lead-in rows — following the GNOME split
//! between a primary menu and a settings surface. The dense git-send-email
//! configuration lives in an [`adw::PreferencesDialog`] reached from the
//! "Sending details" row, not crammed into the popover.
//!
//! Everything is stock Adwaita with built-in style classes; no custom CSS, no
//! avatars. The popover rebuilds from live `git config` on every open, so an
//! identity switch (which just writes `sendemail.identity` and closes) is
//! reflected the next time it opens — no in-place rebuild needed.

use adw::prelude::*;
use gtk::glib;

use crate::profile::{self, Profile, Setting};

/// `[sendemail]` keys shown under "Server" in the details dialog.
const SERVER_KEYS: &[&str] = &[
    "smtpServer",
    "smtpServerPort",
    "smtpServerOption",
    "smtpEncryption",
    "smtpUser",
    "smtpDomain",
    "smtpSslCertPath",
    "sendmailCmd",
];

/// `[sendemail]` keys shown under "Addressing"; everything else is "Behavior".
const ADDRESSING_KEYS: &[&str] = &[
    "from",
    "envelopeSender",
    "to",
    "cc",
    "toCmd",
    "ccCmd",
    "suppressCc",
    "suppressFrom",
];

/// The account button for the header bar: an icon that opens the profile
/// popover.
pub fn build_profile_button() -> gtk::MenuButton {
    let popover = gtk::Popover::new();
    let button = gtk::MenuButton::builder()
        .icon_name("avatar-default-symbolic")
        .tooltip_text("Profile")
        .build();
    button.set_popover(Some(&popover));
    button.add_css_class("flat");

    // Repopulate on every open from live git config; pass the button so the
    // "Sending details" row can resolve the window to present its dialog on.
    popover.connect_show(glib::clone!(
        #[weak]
        button,
        move |popover| {
            popover.set_child(Some(&build_content(popover, &button, &profile::load())));
        }
    ));

    button
}

fn build_content(popover: &gtk::Popover, button: &gtk::MenuButton, profile: &Profile) -> gtk::Widget {
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .width_request(300)
        .build();

    if profile.is_empty() {
        root.append(&heading_label("No git profile"));
        root.append(&wrapped_dim_label(
            "Koshi reads your name, email, and SMTP settings from git. \
             Configure git send-email to see them here.",
        ));
        let group = adw::PreferencesGroup::new();
        group.add(&account_settings_row(popover));
        root.append(&group);
        return root.upcast();
    }

    root.append(&build_header(profile));
    if profile.identities.len() >= 2 {
        root.append(&build_identities(popover, profile));
    }
    if let Some(transport) = build_transport(profile) {
        root.append(&transport);
    }
    root.append(&build_actions(popover, button, profile));

    root.upcast()
}

/// Name and email, plus — when exactly one identity is configured — a caption
/// naming it (there is nothing to switch to, so it is context, not a control).
fn build_header(profile: &Profile) -> gtk::Widget {
    let block = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();

    let name = profile
        .user_name
        .clone()
        .or_else(|| profile.active().map(|id| id.name.clone()))
        .or_else(|| profile.user_email.clone())
        .unwrap_or_else(|| "Profile".to_string());
    block.append(&heading_label(&name));

    if let Some(email) = &profile.user_email {
        let label = wrapped_dim_label(email);
        label.add_css_class("caption");
        label.set_selectable(true);
        block.append(&label);
    }

    if profile.identities.len() == 1 {
        let label = wrapped_dim_label(&format!("Identity: {}", profile.identities[0].name));
        label.add_css_class("caption");
        block.append(&label);
    }

    block.upcast()
}

/// The identity switcher, shown only when two or more identities exist: one
/// activatable row each, the active one marked with a checkmark. Activating a
/// different row writes `sendemail.identity` and closes the popover.
fn build_identities(popover: &gtk::Popover, profile: &Profile) -> gtk::Widget {
    let group = adw::PreferencesGroup::new();

    for identity in &profile.identities {
        let is_active = profile.active_identity.as_deref() == Some(identity.name.as_str());

        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&identity.name))
            .activatable(true)
            .build();
        if let Some(email) = &identity.email {
            row.set_subtitle(&glib::markup_escape_text(email));
        }
        if is_active {
            row.add_suffix(&gtk::Image::from_icon_name("object-select-symbolic"));
        }

        let name = identity.name.clone();
        row.connect_activated(glib::clone!(
            #[weak]
            popover,
            move |_| {
                if !is_active {
                    profile::set_active_identity(&name);
                }
                popover.popdown();
            }
        ));
        group.add(&row);
    }

    group.upcast()
}

/// A single calm row saying where mail goes: server as title, "Port 465 · SSL"
/// as subtitle (the Wi-Fi-row idiom). Absent when nothing sends mail.
fn build_transport(profile: &Profile) -> Option<gtk::Widget> {
    // Nothing routes mail (no SMTP server, no sendmail command) — no row.
    profile.effective_transport()?;

    let settings = profile.effective_sendemail();
    let get = |key: &str| {
        settings
            .iter()
            .find(|s| s.key == key)
            .map(|s| s.value.as_str())
    };

    let (title, subtitle) = if let Some(server) = get("smtpServer") {
        let mut parts = Vec::new();
        if let Some(port) = get("smtpServerPort") {
            parts.push(format!("Port {port}"));
        }
        if let Some(encryption) = get("smtpEncryption") {
            parts.push(encryption_label(encryption));
        }
        (server.to_string(), parts.join(" \u{00b7} "))
    } else if let Some(command) = get("sendmailCmd") {
        ("Local sendmail".to_string(), command.to_string())
    } else {
        return None;
    };

    let group = adw::PreferencesGroup::new();
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&title))
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("network-server-symbolic"));
    if !subtitle.is_empty() {
        row.set_subtitle(&glib::markup_escape_text(&subtitle));
    }
    group.add(&row);

    Some(group.upcast())
}

fn encryption_label(value: &str) -> String {
    match value.to_ascii_lowercase().as_str() {
        "ssl" => "SSL".to_string(),
        "tls" => "TLS".to_string(),
        "" | "none" => "No encryption".to_string(),
        _ => value.to_string(),
    }
}

/// The lead-in rows: "Sending details" (opens the config dialog, only when
/// there is config to show) and "Account Settings".
fn build_actions(
    popover: &gtk::Popover,
    button: &gtk::MenuButton,
    profile: &Profile,
) -> gtk::Widget {
    let group = adw::PreferencesGroup::new();

    if !profile.effective_sendemail().is_empty() {
        let row = adw::ActionRow::builder()
            .title("Sending details")
            .subtitle("SMTP and git send-email options")
            .activatable(true)
            .build();
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));

        let profile = profile.clone();
        row.connect_activated(glib::clone!(
            #[weak]
            popover,
            #[weak]
            button,
            move |_| {
                popover.popdown();
                let dialog = build_sending_dialog(&profile);
                dialog.present(button.root().as_ref());
            }
        ));
        group.add(&row);
    }

    group.add(&account_settings_row(popover));
    group.upcast()
}

/// An "Account Settings" row wired to the existing `app.preferences` action.
fn account_settings_row(popover: &gtk::Popover) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("Account Settings")
        .activatable(true)
        .action_name("app.preferences")
        .build();
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    row.connect_activated(glib::clone!(
        #[weak]
        popover,
        move |_| popover.popdown()
    ));
    row
}

/// The full git-send-email configuration, split into Server / Addressing /
/// Behavior groups of read-only `property` rows with per-value copy buttons.
fn build_sending_dialog(profile: &Profile) -> adw::PreferencesDialog {
    let page = adw::PreferencesPage::builder()
        .title("Send Email")
        .icon_name("mail-send-symbolic")
        .build();

    let settings = profile.effective_sendemail();
    let (mut server, mut addressing, mut behavior) = (Vec::new(), Vec::new(), Vec::new());
    for setting in &settings {
        if SERVER_KEYS.contains(&setting.key.as_str()) {
            server.push(setting);
        } else if ADDRESSING_KEYS.contains(&setting.key.as_str()) {
            addressing.push(setting);
        } else {
            behavior.push(setting);
        }
    }

    if !server.is_empty() {
        let group = adw::PreferencesGroup::builder().title("Server").build();
        match profile.active() {
            Some(identity) => group.set_description(Some(&format!(
                "Using identity \u{201c}{}\u{201d}",
                glib::markup_escape_text(&identity.name)
            ))),
            None => group.set_description(Some("git send-email")),
        }
        for setting in server {
            group.add(&property_row(setting));
        }
        page.add(&group);
    }

    for (title, bucket) in [("Addressing", addressing), ("Behavior", behavior)] {
        if bucket.is_empty() {
            continue;
        }
        let group = adw::PreferencesGroup::builder().title(title).build();
        for setting in bucket {
            group.add(&property_row(setting));
        }
        page.add(&group);
    }

    let dialog = adw::PreferencesDialog::builder().title("Send Email").build();
    dialog.add(&page);
    dialog
}

/// A read-only key/value row: the git key as a de-emphasized title, the value
/// emphasized and selectable, with a flat button to copy it.
fn property_row(setting: &Setting) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&setting.key))
        .css_classes(["property"])
        .build();
    // Plain text — a `from` value carries `<addr>` that must not be markup.
    row.set_subtitle(&glib::markup_escape_text(&setting.value));
    row.set_subtitle_selectable(true);

    let copy = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .valign(gtk::Align::Center)
        .tooltip_text("Copy value")
        .css_classes(["flat"])
        .build();
    let value = setting.value.clone();
    copy.connect_clicked(move |button| {
        button.clipboard().set_text(&value);
    });
    row.add_suffix(&copy);

    row
}

fn heading_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .use_markup(false)
        .css_classes(["heading"])
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .wrap(true)
        .build()
}

fn wrapped_dim_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .use_markup(false)
        .css_classes(["dim-label"])
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .wrap(true)
        .build()
}
