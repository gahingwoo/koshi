use adw::prelude::*;
use gtk::{gio, glib};

const APP_ID: &str = "moe.nikableh.Koshi";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    let status_page = adw::StatusPage::builder()
        .title("Koshi")
        .description("Welcome to Koshi")
        .icon_name("applications-system-symbolic")
        .build();

    let search_entry = build_search_entry();

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&build_header_bar(&search_entry));
    toolbar_view.set_content(Some(&status_page));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Koshi")
        .default_width(600)
        .default_height(400)
        .content(&toolbar_view)
        .build();

    setup_actions(app, &window, &search_entry);

    window.present();
}

fn build_search_entry() -> gtk::SearchEntry {
    gtk::SearchEntry::builder()
        .placeholder_text("Search lore, a Message-ID, or a lore.kernel.org URL")
        .tooltip_text("Search (Ctrl+L)")
        .hexpand(true)
        .build()
}

fn build_header_bar(search_entry: &gtk::SearchEntry) -> adw::HeaderBar {
    let header = adw::HeaderBar::new();

    let home_button = gtk::Button::builder()
        .icon_name("go-home-symbolic")
        .tooltip_text("Home")
        .build();
    header.pack_start(&home_button);

    let new_tab_button = gtk::Button::builder()
        .icon_name("tab-new-symbolic")
        .tooltip_text("New Tab")
        .build();
    header.pack_start(&new_tab_button);

    let clamp = adw::Clamp::builder()
        .maximum_size(600)
        .tightening_threshold(400)
        .hexpand(true)
        .child(search_entry)
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

fn build_account_button() -> gtk::Button {
    let avatar = adw::Avatar::new(24, None, false);
    let button = gtk::Button::builder()
        .child(&avatar)
        .tooltip_text("Account")
        .build();
    button.add_css_class("flat");
    button
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

    app.add_action_entries([preferences, shortcuts, about]);

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
