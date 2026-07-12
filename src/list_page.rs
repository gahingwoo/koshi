use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

/// A right-click context menu shared by every row of one list: a single stock
/// `gtk::PopoverMenu` popped at the pointer.
///
/// One popover per list, parented to `host` with its action group inserted
/// there too, so the menu items resolve their actions through real, permanent
/// ancestry — a popover conjured per press on a stock row has none, and its
/// items go inert. [`attach`](Self::attach) swaps the target row's handlers in
/// before popping, the same shape GNOME Resources uses for its process list.
///
/// `host` must be an ancestor of the rows, and must NOT be the `GtkListBox`
/// itself: ListBox's dispose remove()s its children in a loop, and remove()
/// rejects non-row children without unparenting them, so a popover child spins
/// that loop forever ("Tried to remove non-child" every iteration) and hangs
/// the app. A plain `gtk::Box` around or above the list is the right host —
/// its teardown unparents any child, and box layout skips popover children.
///
/// No strong host reference lives here (the host is reached as the popover's
/// parent): rows and list-signal closures capture the RowMenu, and a strong
/// handle would cycle the host alive.
#[derive(Clone)]
pub struct RowMenu {
    popover: gtk::PopoverMenu,
    handlers: Rc<RefCell<Vec<Rc<dyn Fn()>>>>,
}

impl RowMenu {
    /// One menu item per label (`_` marks the mnemonic), in the order the
    /// handlers are later passed to [`attach`](Self::attach).
    pub fn new(host: &impl IsA<gtk::Widget>, labels: &[&str]) -> Self {
        let menu = gio::Menu::new();
        let group = gio::SimpleActionGroup::new();
        let handlers: Rc<RefCell<Vec<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(Vec::new()));
        for (index, label) in labels.iter().enumerate() {
            let action = gio::SimpleAction::new(&format!("item{index}"), None);
            action.connect_activate(glib::clone!(
                #[strong]
                handlers,
                move |_, _| {
                    // Cloned out so the handler runs with the borrow released.
                    let handler = handlers.borrow().get(index).cloned();
                    if let Some(handler) = handler {
                        handler();
                    }
                }
            ));
            group.add_action(&action);
            menu.append(Some(label), Some(&format!("row-menu.item{index}")));
        }
        host.insert_action_group("row-menu", Some(&group));

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(host);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        // A stock host has no dispose hook for a manually parented child, so
        // detach the popover when the host is torn down; left attached it
        // would warn at finalize.
        host.connect_destroy(glib::clone!(
            #[weak]
            popover,
            move |_| popover.unparent()
        ));

        Self { popover, handlers }
    }

    /// Open the menu on `row` on right-click, with `row_handlers` (one per
    /// label, same order) as its actions.
    pub fn attach(&self, row: &impl IsA<gtk::Widget>, row_handlers: Vec<Rc<dyn Fn()>>) {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        gesture.connect_pressed(glib::clone!(
            #[weak(rename_to = row)]
            row.as_ref(),
            #[weak(rename_to = popover)]
            self.popover,
            #[strong(rename_to = handlers)]
            self.handlers,
            move |gesture, _, x, y| {
                let Some(host) = popover.parent() else {
                    return;
                };
                gesture.set_state(gtk::EventSequenceState::Claimed);
                *handlers.borrow_mut() = row_handlers.clone();
                // The popover is parented to the host, so the press point is
                // translated from row to host coordinates before pointing.
                let point = row
                    .compute_point(&host, &gtk::graphene::Point::new(x as f32, y as f32))
                    .unwrap_or_else(|| gtk::graphene::Point::new(x as f32, y as f32));
                popover.set_pointing_to(Some(&gdk::Rectangle::new(
                    point.x().round() as i32,
                    point.y().round() as i32,
                    1,
                    1,
                )));
                popover.popup();
            }
        ));
        row.add_controller(gesture);
    }
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
