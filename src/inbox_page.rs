use adw::prelude::*;
use gtk::glib;

use crate::thread_list_page::build_thread_list_page;

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

pub const INBOX_LIST_TITLE: &str = "Public Inboxes";

pub fn build_inbox_page(nav: &adw::NavigationView) -> adw::NavigationPage {
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

    list.connect_row_activated(glib::clone!(
        #[weak]
        nav,
        move |_, row| {
            let (name, description, _) = PLACEHOLDER_INBOXES[row.index() as usize];
            nav.push(&build_thread_list_page(name, description));
        }
    ));

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

    let scrolled = gtk::ScrolledWindow::builder().child(&clamp).build();

    adw::NavigationPage::new(&scrolled, INBOX_LIST_TITLE)
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
