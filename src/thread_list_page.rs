use adw::prelude::*;
use gtk::{gio, glib};

struct Thread {
    subject: &'static str,
    date: &'static str,
    time: &'static str,
    children: &'static [&'static str],
}

const PLACEHOLDER_THREADS: &[Thread] = &[
    Thread {
        subject: "[PATCH] media: mali-c55: Fix unaligned access of AEC histogram zone weights",
        date: "Jul 3",
        time: "9:44",
        children: &[],
    },
    Thread {
        subject: "[PATCH v3] Fix multiple issues in chcr driver:",
        date: "Jul 3",
        time: "9:30",
        children: &[],
    },
    Thread {
        subject: "[PATCH v3] ARM: breakpoint: CFI breakpoints only on demand",
        date: "Jul 3",
        time: "9:27",
        children: &[],
    },
    Thread {
        subject: "[PATCH v4 0/8] crypto: qce - Fix crypto self-test failures",
        date: "Jul 3",
        time: "9:23",
        children: &["[PATCH v4 2/8] crypto: qce - Fix HMAC self-test failures for empty messages"],
    },
    Thread {
        subject: "[PATCH stable] mm/khugepaged: write all dirty file folios when collapsing",
        date: "Jul 3",
        time: "9:20",
        children: &[],
    },
    Thread {
        subject: "[PATCH] perf trace: Refactor augmented_raw_syscalls using bpf_loop",
        date: "Jul 3",
        time: "8:59",
        children: &[],
    },
    Thread {
        subject: "[PATCH] drm/xe: Wait on external BO kernel fences in exec IOCTL",
        date: "Jul 3",
        time: "8:45",
        children: &[],
    },
    Thread {
        subject: "[PATCH] usb: gadget: f_ncm: validate datagram bounds in ncm_unwrap_ntb()",
        date: "Jul 3",
        time: "8:37",
        children: &[],
    },
    Thread {
        subject: "[PATCH net] macsec: don't read an unset MAC header in macsec_encrypt()",
        date: "Jul 3",
        time: "8:36",
        children: &[],
    },
];

pub fn build_thread_list_page(inbox_name: &str, inbox_description: &str) -> adw::NavigationPage {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(24)
        .margin_bottom(36)
        .margin_start(12)
        .margin_end(12)
        .spacing(12)
        .build();
    content.append(&build_title(inbox_name, inbox_description));
    content.append(&build_sort_row());
    content.append(&build_thread_list());

    let clamp = adw::Clamp::builder()
        .maximum_size(800)
        .tightening_threshold(600)
        .child(&content)
        .build();

    let scrolled = gtk::ScrolledWindow::builder().child(&clamp).build();

    adw::NavigationPage::new(&scrolled, inbox_name)
}

fn build_title(inbox_name: &str, inbox_description: &str) -> gtk::Box {
    let title_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();

    title_box.append(
        &gtk::Label::builder()
            .label(inbox_name)
            .halign(gtk::Align::Start)
            .css_classes(["title-1"])
            .build(),
    );
    title_box.append(
        &gtk::Label::builder()
            .label(inbox_description)
            .halign(gtk::Align::Start)
            .css_classes(["dim-label"])
            .build(),
    );

    title_box
}

fn build_sort_row() -> gtk::MenuButton {
    let sort_action =
        gio::SimpleAction::new_stateful("sort", Some(glib::VariantTy::STRING), &"date".into());
    sort_action.connect_activate(|action, param| {
        if let Some(param) = param {
            action.set_state(param);
        }
    });
    let group = gio::SimpleActionGroup::new();
    group.add_action(&sort_action);

    let menu = gio::Menu::new();
    menu.append(Some("Date"), Some("threads.sort::date"));
    menu.append(Some("Relevance"), Some("threads.sort::relevance"));

    let button = gtk::MenuButton::builder()
        .icon_name("view-sort-descending-symbolic")
        .menu_model(&menu)
        .tooltip_text("Sort")
        .halign(gtk::Align::End)
        .css_classes(["flat"])
        .build();
    button.insert_action_group("threads", Some(&group));
    button
}

fn build_thread_list() -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    for thread in PLACEHOLDER_THREADS {
        list.append(&build_thread_row(thread));
    }

    list
}

fn build_thread_row(thread: &Thread) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .build();

    let subjects = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(3)
        .hexpand(true)
        .build();
    subjects.append(
        &gtk::Label::builder()
            .label(thread.subject)
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build(),
    );
    for child in thread.children {
        subjects.append(
            &gtk::Label::builder()
                .label(format!("└ {child}"))
                .halign(gtk::Align::Start)
                .margin_start(12)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["dim-label"])
                .build(),
        );
    }
    row.append(&subjects);

    let timestamp = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .build();
    timestamp.append(
        &gtk::Label::builder()
            .label(thread.date)
            .halign(gtk::Align::End)
            .css_classes(["numeric", "caption"])
            .build(),
    );
    timestamp.append(
        &gtk::Label::builder()
            .label(thread.time)
            .halign(gtk::Align::End)
            .css_classes(["numeric", "caption", "dim-label"])
            .build(),
    );
    row.append(&timestamp);

    row
}
