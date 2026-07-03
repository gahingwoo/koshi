use adw::prelude::*;
use gtk::glib;

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

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&adw::HeaderBar::new());
    toolbar_view.set_content(Some(&status_page));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Koshi")
        .default_width(600)
        .default_height(400)
        .content(&toolbar_view)
        .build();

    window.present();
}
