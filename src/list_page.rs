use adw::prelude::*;
use gtk::glib;

/// A right-click context menu for a list row: a popover of flat buttons, one
/// per `(label, action)`. Parented to `anchor` and unparented when it closes.
///
/// Plain buttons with direct click handlers are used rather than a
/// `gtk::PopoverMenu` backed by a `gio` action group: as a transient child of a
/// stock `adw::ActionRow`, the PopoverMenu's items do not reliably bind their
/// actions, so clicking them would silently do nothing. `label` uses `_` for
/// its mnemonic.
pub fn build_row_menu(
    anchor: &impl IsA<gtk::Widget>,
    actions: Vec<(&str, Box<dyn Fn()>)>,
) -> gtk::Popover {
    let items = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    let popover = gtk::Popover::builder()
        .has_arrow(false)
        .halign(gtk::Align::Start)
        .child(&items)
        .build();
    // The stock menu style: tighter padding and full-width row highlight.
    popover.add_css_class("menu");

    for (label, action) in actions {
        let button = gtk::Button::builder()
            .css_classes(["flat"])
            .child(
                &gtk::Label::builder()
                    .label(label)
                    .use_underline(true)
                    .xalign(0.0)
                    .hexpand(true)
                    .build(),
            )
            .build();
        button.connect_clicked(glib::clone!(
            #[weak]
            popover,
            move |_| {
                popover.popdown();
                action();
            }
        ));
        items.append(&button);
    }

    popover.set_parent(anchor);
    popover.connect_closed(|popover| popover.unparent());
    popover
}

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
    build_list_page_with_search(page_title, heading, description, title_buttons, list, None)
}

/// As [`build_list_page`], but pins an optional [`gtk::SearchBar`] above the
/// scrolled content. The bar reveals on demand (typing / Ctrl+F) and stays
/// fixed while the list scrolls beneath it.
pub fn build_list_page_with_search(
    page_title: &str,
    heading: &str,
    description: &str,
    title_buttons: &[gtk::Widget],
    list: &impl IsA<gtk::Widget>,
    search_bar: Option<&gtk::SearchBar>,
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
        // Cap the description at two wrapped lines and ellipsize the rest so a
        // long search query can't grow the header tall enough to push the
        // content below it (an empty/error status page) off the bottom of the
        // window. The full text stays available in a tooltip.
        &gtk::Label::builder()
            .label(description)
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .lines(2)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .tooltip_text(description)
            .css_classes(["dim-label"])
            .build(),
    );
    content.append(list);

    let clamp = adw::Clamp::builder()
        .maximum_size(800)
        .tightening_threshold(600)
        .child(&content)
        .build();

    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();

    let Some(search_bar) = search_bar else {
        return adw::NavigationPage::new(&scrolled, page_title);
    };

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    column.append(search_bar);
    column.append(&scrolled);

    adw::NavigationPage::new(&column, page_title)
}
