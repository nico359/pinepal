// SPDX-License-Identifier: GPL-3.0-or-later

use adw::prelude::*;
use gtk::{gio, glib};

use crate::log_collector;

/// Open a non-modal dialog showing collected diagnostic logs.
/// Works on both desktop and mobile (adw::Dialog adapts layout automatically).
pub fn show_log_viewer(parent: &impl IsA<gtk::Widget>) {
    // Capture the root window now, before the dialog is presented, so we can
    // use it as the parent for the save file chooser later.
    let parent_window = parent
        .as_ref()
        .root()
        .and_then(|r| r.downcast::<gtk::Window>().ok());

    let dialog = adw::Dialog::builder()
        .title("Diagnostic Logs")
        .content_width(600)
        .content_height(700)
        .build();

    let toolbar_view = adw::ToolbarView::new();

    let header_bar = adw::HeaderBar::new();
    let copy_btn = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("Copy all logs to clipboard")
        .build();
    let save_btn = gtk::Button::builder()
        .icon_name("document-save-symbolic")
        .tooltip_text("Save logs to file")
        .build();
    // Place buttons so they're reachable on narrow phone screens.
    header_bar.pack_end(&save_btn);
    header_bar.pack_end(&copy_btn);
    toolbar_view.add_top_bar(&header_bar);

    let text_view = gtk::TextView::builder()
        .editable(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::Char)
        .left_margin(8)
        .right_margin(8)
        .top_margin(8)
        .bottom_margin(8)
        .build();

    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&text_view)
        .build();
    toolbar_view.set_content(Some(&scroll));
    dialog.set_child(Some(&toolbar_view));

    // Populate from the in-memory ring buffer.
    let content = log_collector::get_logs();
    let buf = text_view.buffer();
    if content.is_empty() {
        buf.set_text("No log entries collected yet.");
    } else {
        buf.set_text(&content);
    }

    // Scroll to the bottom after the widget is realised.
    glib::idle_add_local_once({
        let tv = text_view.clone();
        move || {
            let b = tv.buffer();
            let end = b.end_iter();
            let mark = b.create_mark(None, &end, false);
            tv.scroll_to_mark(&mark, 0.0, true, 0.0, 1.0);
        }
    });

    // Copy button — puts the full log text on the clipboard.
    copy_btn.connect_clicked({
        let tv = text_view.clone();
        move |_| {
            let b = tv.buffer();
            let text = b.text(&b.start_iter(), &b.end_iter(), false);
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&text);
            }
        }
    });

    // Save button — opens a file chooser and writes the log as plain text.
    save_btn.connect_clicked({
        let tv = text_view.clone();
        move |_| {
            let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let file_dialog = gtk::FileDialog::builder()
                .initial_name(format!("pinepal-logs-{timestamp}.txt"))
                .build();

            let tv_c = tv.clone();
            let win = parent_window.clone();
            file_dialog.save(win.as_ref(), gio::Cancellable::NONE, move |result| {
                if let Ok(file) = result {
                    let b = tv_c.buffer();
                    let text = b.text(&b.start_iter(), &b.end_iter(), false);
                    if let Some(path) = file.path() {
                        if let Err(e) = std::fs::write(&path, text.as_bytes()) {
                            log::error!("Failed to save logs: {e}");
                        } else {
                            log::info!("Logs saved to {}", path.display());
                        }
                    }
                }
            });
        }
    });

    dialog.present(Some(parent.as_ref()));
}
