use adw::prelude::*;

/// Shared scaffold for list-style pages: a scrolled, clamped column holding a
/// title row (a title-1 heading plus optional trailing buttons), a dim
/// description and a boxed list below.
pub fn build_list_page(
    page_title: &str,
    heading: &str,
    description: &str,
    title_buttons: &[gtk::Widget],
    list: &impl IsA<gtk::Widget>,
) -> adw::NavigationPage {
    let title_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    title_row.append(
        &gtk::Label::builder()
            .label(heading)
            .halign(gtk::Align::Start)
            .hexpand(true)
            .css_classes(["title-1"])
            .build(),
    );
    for button in title_buttons {
        title_row.append(button);
    }

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(36)
        .margin_bottom(36)
        .margin_start(12)
        .margin_end(12)
        .spacing(12)
        .build();
    content.append(&title_row);
    content.append(
        &gtk::Label::builder()
            .label(description)
            .halign(gtk::Align::Start)
            .css_classes(["dim-label"])
            .build(),
    );
    content.append(list);

    let clamp = adw::Clamp::builder()
        .maximum_size(800)
        .tightening_threshold(600)
        .child(&content)
        .build();

    let scrolled = gtk::ScrolledWindow::builder().child(&clamp).build();

    adw::NavigationPage::new(&scrolled, page_title)
}
