//! The console pane.
//!
//! It tails the guest's console log and shows it in a monospace view, with a
//! command entry that writes into the guest through the console socket. Reading
//! the log rather than the socket means the pane also works for a guest that is
//! not currently attached anywhere, and survives detaching.

use crate::i18n;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use librevm_core::store::Store;
use librevm_core::vm::{self, State};
use std::cell::RefCell;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;

pub struct ConsoleView {
    root: gtk4::Box,
    view: gtk4::TextView,
    buffer: gtk4::TextBuffer,
    entry: gtk4::Entry,
    send: gtk4::Button,
    status: gtk4::Label,

    store: Store,
    current: Rc<RefCell<Option<String>>>,
    /// How much of the log we have already shown.
    offset: Rc<RefCell<u64>>,
    /// The connection used to type into the guest.
    ///
    /// It is kept open rather than reopened per line. The hub forwards input only
    /// while a client stays connected, and a client that connects, writes and
    /// immediately closes can lose that write to the hub's history replay.
    link: Rc<RefCell<Option<UnixStream>>>,
    /// Which guest `link` belongs to, so switching machines reconnects.
    link_for: Rc<RefCell<Option<String>>>,
}

impl ConsoleView {
    pub fn new(store: Store) -> Self {
        let buffer = gtk4::TextBuffer::new(None);

        let view = gtk4::TextView::builder()
            .buffer(&buffer)
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk4::WrapMode::Char)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(10)
            .right_margin(10)
            .css_classes(["console-view"])
            .build();

        let scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Automatic)
            .vexpand(true)
            .child(&view)
            .build();

        let entry = gtk4::Entry::builder()
            .placeholder_text(i18n::tr(i18n::Key::ConsoleHint))
            .hexpand(true)
            .build();

        let send = gtk4::Button::builder()
            .icon_name("go-next-symbolic")
            .tooltip_text(i18n::tr(i18n::Key::SendToGuest))
            .build();

        let status = gtk4::Label::builder()
            .label(i18n::tr(i18n::Key::NoMachineSelected))
            .xalign(0.0)
            .css_classes(["metric-label"])
            .build();

        let input_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        input_row.set_margin_top(6);
        input_row.set_margin_bottom(6);
        input_row.set_margin_start(10);
        input_row.set_margin_end(10);
        input_row.append(&entry);
        input_row.append(&send);

        let status_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        status_row.set_margin_start(10);
        status_row.set_margin_end(10);
        status_row.set_margin_bottom(6);
        status_row.append(&status);

        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.append(&scroll);
        root.append(&status_row);
        root.append(&input_row);

        let console = Self {
            root,
            view: view.clone(),
            buffer: buffer.clone(),
            entry: entry.clone(),
            send: send.clone(),
            status,
            store,
            current: Rc::new(RefCell::new(None)),
            offset: Rc::new(RefCell::new(0)),
            link: Rc::new(RefCell::new(None)),
            link_for: Rc::new(RefCell::new(None)),
        };

        console.wire(entry, send, buffer, view);
        console
    }

    pub fn widget(&self) -> gtk4::Widget {
        self.root.clone().upcast()
    }

    fn wire(
        &self,
        entry: gtk4::Entry,
        send: gtk4::Button,
        buffer: gtk4::TextBuffer,
        view: gtk4::TextView,
    ) {
        let current = self.current.clone();
        let store = self.store.clone();
        let link = self.link.clone();
        let link_for = self.link_for.clone();
        // the closure needs its own handle, since the entry is used again below
        let entry_for_send = entry.clone();
        let send_line = move || {
            let Some(name) = current.borrow().clone() else { return };
            let text = entry_for_send.text().to_string();
            if text.is_empty() {
                return;
            }

            let line = format!("{text}\n");

            // Write, and if the link turns out to be dead, reconnect and try once
            // more. The usual cause is that the machine was restarted, so the hub
            // this connection belonged to is gone; without the retry the first line
            // after every restart would vanish silently.
            let mut attempts = 0;
            loop {
                attempts += 1;

                let needs_connect = {
                    let same = link_for.borrow().as_deref() == Some(name.as_str());
                    !same || link.borrow().is_none()
                };
                if needs_connect {
                    let socket = store.console_socket(&name);
                    match UnixStream::connect(&socket) {
                        Ok(s) => {
                            *link.borrow_mut() = Some(s);
                            *link_for.borrow_mut() = Some(name.clone());
                        }
                        Err(e) => {
                            log::warn!("cannot reach {}: {e}", socket.display());
                            return;
                        }
                    }
                }

                let mut guard = link.borrow_mut();
                let Some(s) = guard.as_mut() else { return };
                match s.write_all(line.as_bytes()).and_then(|_| s.flush()) {
                    Ok(()) => break,
                    Err(e) => {
                        log::warn!("the console link broke ({e}); reconnecting");
                        *guard = None;
                        drop(guard);
                        if attempts >= 2 {
                            log::warn!("could not deliver {text:?}");
                            return;
                        }
                        // loop round, reconnect, and send it again
                    }
                }
            }

            log::debug!("sent {text:?} to {name}");
            entry_for_send.set_text("");
        };

        {
            let send_line = send_line.clone();
            entry.connect_activate(move |_| send_line());
        }
        send.connect_clicked(move |_| send_line());

        // keep the newest line visible
        let view_for_scroll = view.clone();
        let buffer_for_scroll = buffer.clone();
        buffer.connect_changed(move |_| {
            let end = buffer_for_scroll.end_iter();
            view_for_scroll.scroll_to_iter(&mut end.clone(), 0.0, false, 0.0, 0.0);
        });
    }

    /// Point the pane at a VM and show whatever it has logged so far.
    pub fn follow(&self, name: &str) {
        if self.current.borrow().as_deref() == Some(name) {
            return;
        }
        // A different machine means a different hub, so the old connection is
        // meaningless even if it is still open.
        *self.link.borrow_mut() = None;
        *self.link_for.borrow_mut() = None;
        *self.current.borrow_mut() = Some(name.to_string());
        self.buffer.set_text("");
        *self.offset.borrow_mut() = 0;
        self.status.set_label(&i18n::tr_waiting_for_output(name));
    }

    pub fn clear(&self) {
        *self.link.borrow_mut() = None;
        *self.link_for.borrow_mut() = None;
        *self.current.borrow_mut() = None;
        self.buffer.set_text("");
        *self.offset.borrow_mut() = 0;
        self.status.set_label(i18n::tr(i18n::Key::NoMachineSelected));
    }

    /// Read anything new from the log.
    pub fn refresh_if_running(&self, name: &str) {
        if self.current.borrow().as_deref() != Some(name) {
            return;
        }
        self.pump(name);
    }

    fn pump(&self, name: &str) {
        let path = self.store.console_log(name);
        let Ok(mut file) = std::fs::File::open(&path) else {
            return;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let mut offset = self.offset.borrow_mut();
        if len < *offset {
            // the log was truncated by a new run
            *offset = 0;
            self.buffer.set_text("");
        }
        if file.seek(SeekFrom::Start(*offset)).is_err() {
            return;
        }
        let mut text = String::new();
        if file.read_to_string(&mut text).is_err() {
            return;
        }
        *offset = len;

        if !text.is_empty() {
            let end = self.buffer.end_iter();
            self.buffer.insert(&mut end.clone(), &text);
        }

        let state = vm::state(&self.store, name);
        let word = match state {
            State::Running { pid } => i18n::tr_running_pid(pid),
            State::Stopped => i18n::tr(i18n::Key::StateStopped).to_string(),
            State::Stale { pid } => i18n::tr_stale_pid(pid),
        };
        self.status.set_label(&i18n::tr_console_status(name, &word, len, true));
    }
}
