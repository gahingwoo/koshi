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

fn build_content(
    popover: &gtk::Popover,
    button: &gtk::MenuButton,
    profile: &Profile,
) -> gtk::Widget {
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
        return root.upcast();
    }

    root.append(&build_header(profile));
    if profile.identities.len() >= 2 {
        root.append(&build_identities(popover, profile));
    }
    if let Some(transport) = build_transport(profile) {
        root.append(&transport);
    }
    if let Some(actions) = build_actions(popover, button, profile) {
        root.append(&actions);
    }

    root.upcast()
}

/// Name and email, plus — when exactly one identity is configured — a caption
/// naming it (there is nothing to switch to, so it is context, not a control).
fn build_header(profile: &Profile) -> gtk::Widget {
    let block = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();

    let name = profile
        .user_name
        .clone()
        .or_else(|| profile.active().map(|id| id.name.clone()))
        .or_else(|| profile.user_email.clone())
        .unwrap_or_else(|| "Profile".to_string());
    block.append(&heading_label(&name));

    if let Some(email) = &profile.user_email {
        block.append(&build_email_pill(email));
    }

    if profile.identities.len() == 1 {
        let label = wrapped_dim_label(&format!("Identity: {}", profile.identities[0].name));
        label.add_css_class("caption");
        block.append(&label);
    }

    block.upcast()
}

/// The user's email as a monospace address pill, matching the To/Cc chips in
/// the thread view (built-in `address-chip` class). Clicking it copies the
/// whole address — it is not free-selectable text, so there is no stray
/// cursor or partial-selection.
fn build_email_pill(email: &str) -> gtk::Widget {
    let label = gtk::Label::builder()
        .use_markup(true)
        .label(format!("<tt>{}</tt>", glib::markup_escape_text(email)))
        .css_classes(["caption"])
        .build();

    let pill = gtk::Button::builder()
        .child(&label)
        .halign(gtk::Align::Start)
        .tooltip_text("Copy email address")
        .css_classes(["address-chip"])
        .build();

    let email = email.to_string();
    pill.connect_clicked(move |pill| {
        pill.clipboard().set_text(&email);
    });

    pill.upcast()
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

/// The "Sending details" row, which opens the config dialog. Shown only when
/// there is git send-email config to show.
fn build_actions(
    popover: &gtk::Popover,
    button: &gtk::MenuButton,
    profile: &Profile,
) -> Option<gtk::Widget> {
    if profile.effective_sendemail().is_empty() {
        return None;
    }

    let group = adw::PreferencesGroup::new();

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
            let dialog = build_sending_dialog(&profile);
            // Present on the top-level window (a stable widget) and only
            // then dismiss the popover. Presenting relative to the row
            // inside the closing popover serialises the dialog behind the
            // popover's dismissal grab/animation, which is the source of
            // the open lag; the window is always mapped, so it does not.
            dialog.present(button.root().and_downcast::<gtk::Window>().as_ref());
            popover.popdown();
        }
    ));
    group.add(&row);

    Some(group.upcast())
}

/// The full git-send-email configuration, split into Server / Addressing /
/// Behavior groups of read-only `property` rows with per-value copy buttons.
///
/// A plain `adw::Dialog` (not `adw::PreferencesDialog`): this is a read-only
/// view, so it needs none of the search / view-switcher machinery, and skipping
/// it makes the window open snappily.
fn build_sending_dialog(profile: &Profile) -> adw::Dialog {
    let page = adw::PreferencesPage::new();

    // One note above every group explaining where these values come from and
    // that they are set in git, not here.
    let description = match profile.active() {
        Some(identity) => format!(
            "Read from your git configuration, using the active identity \u{201c}{}\u{201d}. \
             To change them, edit your git send-email settings.",
            glib::markup_escape_text(&identity.name)
        ),
        None => "Read from your git configuration. To change them, edit your git \
                 send-email settings (git config)."
            .to_string(),
    };
    page.set_description(description.as_str());

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

    for (title, bucket) in [
        ("Server", server),
        ("Addressing", addressing),
        ("Behavior", behavior),
    ] {
        if bucket.is_empty() {
            continue;
        }
        let group = adw::PreferencesGroup::builder().title(title).build();
        for setting in bucket {
            group.add(&property_row(setting));
        }
        page.add(&group);
    }

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));

    adw::Dialog::builder()
        .title("Send Email")
        .content_width(460)
        .content_height(620)
        .child(&toolbar)
        .build()
}

/// A read-only key/value row: the git key as a de-emphasized title, the value
/// emphasized, with a flat button to copy it. The value is deliberately NOT
/// selectable — these are display-only, so there is no text cursor and nothing
/// looks editable; copying goes through the button.
fn property_row(setting: &Setting) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&setting.key))
        .css_classes(["property"])
        .build();
    // Plain text — a `from` value carries `<addr>` that must not be markup.
    row.set_subtitle(&glib::markup_escape_text(&setting.value));

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
