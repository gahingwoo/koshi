use adw::prelude::*;
use gtk::{gio, glib};

const APP_ID: &str = "moe.nikableh.Koshi";

const PLACEHOLDER_INBOXES: &[(&str, &str, &str)] = &[
    ("all", "Every list archived on lore.kernel.org", "3.1M msgs"),
    ("live-patching", "Kernel live patching (klp)", "4.2k msgs"),
    ("kernel-janitors", "Trivial fixes and cleanups", "18k msgs"),
    ("dpdk-dev", "DPDK data-plane development", "210k msgs"),
    ("bpf", "BPF core, verifier and tooling", "96k msgs"),
    ("linux-rtc", "Real-time clock subsystem", "12k msgs"),
    ("linux-mm", "Memory management", "480k msgs"),
    ("netdev", "Networking stack", "1.2M msgs"),
    ("workflows", "Kernel development process & tooling", "9.8k msgs"),
    ("linux-doc", "Documentation", "40k msgs"),
];

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| load_css());
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    let search_entry = build_search_entry();

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&build_header_bar(&search_entry));
    toolbar_view.set_content(Some(&build_inbox_page()));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Koshi")
        .default_width(1000)
        .default_height(625)
        .content(&toolbar_view)
        .build();

    setup_actions(app, &window, &search_entry);

    window.present();
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        ".keycap-hint { padding: 1px 6px; border-radius: 6px; \
         border: 1px solid alpha(currentColor, 0.25); font-size: 0.8em; }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn build_search_entry() -> gtk::SearchEntry {
    gtk::SearchEntry::builder()
        .placeholder_text("Search lore, a Message-ID, or a lore.kernel.org URL")
        .hexpand(true)
        .build()
}

fn build_search_overlay(search_entry: &gtk::SearchEntry) -> gtk::Overlay {
    let badge = gtk::Label::builder()
        .label("Ctrl+L")
        .halign(gtk::Align::End)
        .valign(gtk::Align::Center)
        .margin_end(8)
        .can_target(false)
        .css_classes(["dim-label", "keycap-hint"])
        .build();

    let update_badge = glib::clone!(
        #[weak]
        search_entry,
        #[weak]
        badge,
        move |focused: bool| {
            badge.set_visible(!focused && search_entry.text().is_empty());
        }
    );

    search_entry.connect_changed(glib::clone!(
        #[strong]
        update_badge,
        move |entry| {
            update_badge(entry.state_flags().contains(gtk::StateFlags::FOCUS_WITHIN));
        }
    ));

    let focus_controller = gtk::EventControllerFocus::new();
    focus_controller.connect_enter(glib::clone!(
        #[strong]
        update_badge,
        move |_| update_badge(true)
    ));
    focus_controller.connect_leave(move |_| update_badge(false));
    search_entry.add_controller(focus_controller);

    let overlay = gtk::Overlay::builder().child(search_entry).build();
    overlay.add_overlay(&badge);
    overlay
}

fn build_header_bar(search_entry: &gtk::SearchEntry) -> adw::HeaderBar {
    let header = adw::HeaderBar::new();

    let back_button = gtk::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Back")
        .build();
    header.pack_start(&back_button);

    let new_tab_button = gtk::Button::builder()
        .icon_name("tab-new-symbolic")
        .tooltip_text("New Tab")
        .build();
    header.pack_start(&new_tab_button);

    let clamp = adw::Clamp::builder()
        .maximum_size(600)
        .tightening_threshold(400)
        .hexpand(true)
        .child(&build_search_overlay(search_entry))
        .build();
    header.set_title_widget(Some(&clamp));

    header.pack_end(&build_primary_menu_button());
    header.pack_end(&build_account_button());

    header
}

fn build_primary_menu_button() -> gtk::MenuButton {
    let menu = gio::Menu::new();
    menu.append(Some("_Preferences"), Some("app.preferences"));
    menu.append(Some("_Keyboard Shortcuts"), Some("app.shortcuts"));
    menu.append(Some("_About Koshi"), Some("app.about"));

    gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .primary(true)
        .tooltip_text("Main Menu")
        .build()
}

fn build_account_button() -> gtk::MenuButton {
    let menu = gio::Menu::new();
    menu.append(Some("Sign In"), Some("app.sign-in"));
    menu.append(Some("Manage Accounts"), Some("app.manage-accounts"));

    let button = gtk::MenuButton::builder()
        .icon_name("avatar-default-symbolic")
        .menu_model(&menu)
        .tooltip_text("Account")
        .build();
    button.add_css_class("flat");
    button
}

fn build_inbox_page() -> gtk::ScrolledWindow {
    let title = gtk::Label::builder()
        .label("Open a public inbox")
        .halign(gtk::Align::Start)
        .css_classes(["title-1"])
        .build();

    let subtitle = gtk::Label::builder()
        .label("Every list mirrored on lore.kernel.org. Pick one to open it in this tab.")
        .halign(gtk::Align::Start)
        .css_classes(["dim-label"])
        .build();

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    for &(name, description, count) in PLACEHOLDER_INBOXES {
        list.append(&build_inbox_row(name, description, count));
    }

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(36)
        .margin_bottom(36)
        .margin_start(12)
        .margin_end(12)
        .spacing(12)
        .build();
    content.append(&title);
    content.append(&subtitle);
    content.append(&list);

    let clamp = adw::Clamp::builder()
        .maximum_size(800)
        .tightening_threshold(600)
        .child(&content)
        .build();

    gtk::ScrolledWindow::builder().child(&clamp).build()
}

fn build_inbox_row(name: &str, description: &str, count: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(name))
        .subtitle(glib::markup_escape_text(description))
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("mail-unread-symbolic"));
    row.add_suffix(
        &gtk::Label::builder()
            .label(count)
            .css_classes(["dim-label", "numeric"])
            .build(),
    );
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    row
}

fn setup_actions(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    search_entry: &gtk::SearchEntry,
) {
    let focus_search = gio::ActionEntry::builder("focus-search")
        .activate(glib::clone!(
            #[weak]
            search_entry,
            move |_win: &adw::ApplicationWindow, _, _| {
                search_entry.grab_focus();
            }
        ))
        .build();
    window.add_action_entries([focus_search]);

    let preferences = gio::ActionEntry::builder("preferences")
        .activate(|app: &adw::Application, _, _| {
            show_preferences(app);
        })
        .build();

    let shortcuts = gio::ActionEntry::builder("shortcuts")
        .activate(|app: &adw::Application, _, _| {
            show_shortcuts(app);
        })
        .build();

    let about = gio::ActionEntry::builder("about")
        .activate(|app: &adw::Application, _, _| {
            show_about(app);
        })
        .build();

    // Stubs for the account menu so its items are not rendered insensitive.
    let sign_in = gio::ActionEntry::builder("sign-in")
        .activate(|_: &adw::Application, _, _| {})
        .build();
    let manage_accounts = gio::ActionEntry::builder("manage-accounts")
        .activate(|_: &adw::Application, _, _| {})
        .build();

    app.add_action_entries([preferences, shortcuts, about, sign_in, manage_accounts]);

    app.set_accels_for_action("win.focus-search", &["<Control>l"]);
    app.set_accels_for_action("app.preferences", &["<Control>comma"]);
    app.set_accels_for_action("app.shortcuts", &["<Control>question"]);
}

fn show_preferences(app: &adw::Application) {
    let page = adw::PreferencesPage::builder()
        .title("General")
        .icon_name("emblem-system-symbolic")
        .build();
    page.add(&adw::PreferencesGroup::builder().title("General").build());

    let dialog = adw::PreferencesDialog::new();
    dialog.add(&page);
    dialog.present(app.active_window().as_ref());
}

fn show_shortcuts(app: &adw::Application) {
    let section = adw::ShortcutsSection::new(Some("General"));
    section.add(adw::ShortcutsItem::from_action(
        "Focus search",
        "win.focus-search",
    ));
    section.add(adw::ShortcutsItem::from_action(
        "Preferences",
        "app.preferences",
    ));
    section.add(adw::ShortcutsItem::from_action(
        "Keyboard shortcuts",
        "app.shortcuts",
    ));

    let dialog = adw::ShortcutsDialog::new();
    dialog.add(section);
    dialog.present(app.active_window().as_ref());
}

fn show_about(app: &adw::Application) {
    let about = adw::AboutDialog::builder()
        .application_name("Koshi")
        .application_icon(APP_ID)
        .developer_name("Nika")
        .version(env!("CARGO_PKG_VERSION"))
        .build();
    about.present(app.active_window().as_ref());
}
