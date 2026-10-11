//! The console pane.
//!
//! It tails the guest's console log, interprets it as terminal output, and draws the
//! resulting screen. Reading the log rather than the socket means the pane also works
//! for a guest nothing is attached to, and survives detaching.
//!
//! The interpreting is done by `crate::ansi`. Without it a shell's colours arrive as
//! escape sequences and are shown literally, and anything that moves the cursor --
//! `clear`, a progress bar, `top` -- turns into a wall of text.

use crate::ansi::{Color, Pen, Terminal};
use crate::i18n;
use gtk4::prelude::*;
use librevm_core::store::Store;
use librevm_core::vm::{self, State};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;

/// The default palette. These are the usual xterm values, adjusted enough to stay
/// readable on a dark background.
const PALETTE: [(u8, u8, u8); 16] = [
    (0x1d, 0x1f, 0x21), // black
    (0xe0, 0x1b, 0x24), // red
    (0x26, 0xa4, 0x39), // green
    (0xc4, 0xa0, 0x00), // yellow
    (0x2a, 0x7d, 0xe9), // blue
    (0xa5, 0x6c, 0xcb), // magenta
    (0x26, 0xa4, 0xb8), // cyan
    (0xd0, 0xd0, 0xd0), // white
    (0x5c, 0x63, 0x6b), // bright black
    (0xf6, 0x61, 0x6c), // bright red
    (0x4e, 0xe0, 0x6a), // bright green
    (0xf9, 0xc4, 0x40), // bright yellow
    (0x62, 0xa0, 0xea), // bright blue
    (0xc0, 0x7a, 0xf0), // bright magenta
    (0x4e, 0xd8, 0xe8), // bright cyan
    (0xff, 0xff, 0xff), // bright white
];

/// A tag is identified by everything that affects how a run of text looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TagKey {
    fg: Color,
    bg: Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    reverse: bool,
    strike: bool,
}

impl TagKey {
    fn of(pen: Pen) -> Self {
        TagKey {
            fg: pen.fg,
            bg: pen.bg,
            bold: pen.bold,
            dim: pen.dim,
            italic: pen.italic,
            underline: pen.underline,
            reverse: pen.reverse,
            strike: pen.strike,
        }
    }

    /// The effective foreground and background, with reverse video applied and
    /// defaults filled in.
    fn colours(&self) -> (String, Option<String>) {
        let resolve = |c: Color, fallback: &str| -> String {
            match c {
                Color::Default => fallback.to_string(),
                Color::Indexed(i) => {
                    // 0-15 come from the palette; above that is the 6x6x6 cube plus
                    // a grey ramp, the standard 256-colour layout.
                    let (r, g, b) = if (i as usize) < 16 {
                        PALETTE[i as usize]
                    } else if i >= 232 {
                        let v = 8 + (i - 232) * 10;
                        (v, v, v)
                    } else {
                        let i = i - 16;
                        let steps = [0u8, 95, 135, 175, 215, 255];
                        (
                            steps[(i / 36) as usize],
                            steps[((i / 6) % 6) as usize],
                            steps[(i % 6) as usize],
                        )
                    };
                    format!("#{r:02x}{g:02x}{b:02x}")
                }
                Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            }
        };

        let fg_default = "#d0d0d0";
        let bg_default = "#1d1f21";
        let fg = resolve(self.fg, fg_default);
        let bg = resolve(self.bg, bg_default);
        if self.reverse {
            (bg, Some(fg))
        } else {
            // Only set a background when it differs from the terminal's, so runs of
            // ordinary text stay cheap.
            let bg = if bg == bg_default { None } else { Some(bg) };
            (fg, bg)
        }
    }
}

pub struct ConsoleView {
    root: gtk4::Box,
    view: gtk4::TextView,
    buffer: gtk4::TextBuffer,
    entry: gtk4::Entry,
    send: gtk4::Button,
    status: gtk4::Label,
    scrolled: gtk4::ScrolledWindow,

    store: Store,
    current: Rc<RefCell<Option<String>>>,
    /// How much of the log has been fed to the terminal.
    offset: Rc<RefCell<u64>>,
    /// The screen. `None` until a machine is followed.
    term: Rc<RefCell<Option<Terminal>>>,
    /// How many lines the text buffer already holds, so only new ones are appended.
    rendered: Rc<Cell<usize>>,
    /// Tags, built once per distinct appearance.
    tags: Rc<RefCell<HashMap<TagKey, gtk4::TextTag>>>,
    /// Follow the output, unless the operator has scrolled up.
    autoscroll: Rc<Cell<bool>>,

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
            .wrap_mode(gtk4::WrapMode::None)
            .top_margin(6)
            .bottom_margin(6)
            .left_margin(8)
            .right_margin(8)
            .css_classes(["console-view"])
            .build();

        let scrolled = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Automatic)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .vexpand(true)
            .hexpand(true)
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
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["metric-label"])
            .build();

        let input_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        input_row.set_margin_top(6);
        input_row.set_margin_bottom(6);
        input_row.set_margin_start(8);
        input_row.set_margin_end(8);
        input_row.append(&entry);
        input_row.append(&send);

        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.append(&scrolled);
        root.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
        root.append(&status);
        root.append(&input_row);

        let console = Self {
            root,
            view: view.clone(),
            buffer: buffer.clone(),
            entry: entry.clone(),
            send: send.clone(),
            status,
            scrolled: scrolled.clone(),
            store,
            current: Rc::new(RefCell::new(None)),
            offset: Rc::new(RefCell::new(0)),
            term: Rc::new(RefCell::new(None)),
            rendered: Rc::new(Cell::new(0)),
            tags: Rc::new(RefCell::new(HashMap::new())),
            autoscroll: Rc::new(Cell::new(true)),
            link: Rc::new(RefCell::new(None)),
            link_for: Rc::new(RefCell::new(None)),
        };

        console.wire(entry, send, buffer, view, scrolled);
        console
    }

    pub fn widget(&self) -> gtk4::Widget {
        self.root.clone().upcast()
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
        *self.offset.borrow_mut() = 0;
        *self.term.borrow_mut() = Some(Terminal::new(24, 80));
        self.rendered.set(0);
        self.tags.borrow_mut().clear();
        self.buffer.set_text("");
        self.autoscroll.set(true);
        self.status
            .set_label(&i18n::tr_waiting_for_output(name));
        self.pump(name);
    }

    pub fn clear(&self) {
        *self.link.borrow_mut() = None;
        *self.link_for.borrow_mut() = None;
        *self.current.borrow_mut() = None;
        *self.term.borrow_mut() = None;
        self.rendered.set(0);
        self.buffer.set_text("");
        self.status
            .set_label(i18n::tr(i18n::Key::NoMachineSelected));
    }

    /// Read anything new from the log.
    pub fn refresh_if_running(&self, name: &str) {
        if self.current.borrow().as_deref() != Some(name) {
            return;
        }
        self.pump(name);
    }

    // -- wiring -------------------------------------------------------------

    fn wire(
        &self,
        entry: gtk4::Entry,
        send: gtk4::Button,
        buffer: gtk4::TextBuffer,
        view: gtk4::TextView,
        scrolled: gtk4::ScrolledWindow,
    ) {
        let current = self.current.clone();
        let store = self.store.clone();
        let link = self.link.clone();
        let link_for = self.link_for.clone();
        let entry_for_send = entry.clone();
        let send_line = move || {
            let Some(name) = current.borrow().clone() else { return };
            let text = entry_for_send.text().to_string();
            if text.is_empty() {
                return;
            }
            if send_keys(&store, &link, &link_for, &name, format!("{text}\n").as_bytes()) {
                entry_for_send.set_text("");
            }
        };

        {
            let send_line = send_line.clone();
            entry.connect_activate(move |_| send_line());
        }
        send.connect_clicked(move |_| send_line());

        // Keep the newest output visible, but stop following as soon as the operator
        // scrolls up to read something; start again when they return to the bottom.
        {
            let autoscroll = self.autoscroll.clone();
            let adjustment = scrolled.vadjustment();
            adjustment.connect_value_changed(move |a| {
                let at_bottom = a.value() + a.page_size() >= a.upper() - 1.0;
                autoscroll.set(at_bottom);
            });
        }

        // Ctrl+Shift+C copies, which is the convention for a terminal.
        {
            let buffer = buffer.clone();
            let view_for_copy = view.clone();
            let controller = gtk4::EventControllerKey::new();
            controller.connect_key_pressed(move |_, key, _, state| {
                if state.contains(gtk4::gdk::ModifierType::CONTROL_MASK)
                    && state.contains(gtk4::gdk::ModifierType::SHIFT_MASK)
                    && key == gtk4::gdk::Key::C
                {
                    if let Some((start, end)) = buffer.selection_bounds() {
                        let text = buffer.text(&start, &end, false).to_string();
                        view_for_copy.clipboard().set_text(&text);
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            view.add_controller(controller);
        }

        // A monospace font, so the grid stays aligned.
        let provider = gtk4::CssProvider::new();
        provider.load_from_string(
            r#"
            .console-view {
                font-family: monospace;
                font-size: 13px;
                background: #1d1f21;
                color: #d0d0d0;
            }
            "#,
        );
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().expect("a display"),
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    // -- reading ------------------------------------------------------------

    fn pump(&self, name: &str) {
        let path = self.store.console_log(name);
        let Ok(mut file) = std::fs::File::open(&path) else {
            return;
        };

        // If the log shrank, the machine was recreated; start over.
        let mut at = *self.offset.borrow();
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        if size < at {
            at = 0;
            *self.offset.borrow_mut() = 0;
            *self.term.borrow_mut() = Some(Terminal::new(24, 80));
            self.rendered.set(0);
            self.buffer.set_text("");
        }
        if size == at {
            self.update_status(name, size);
            return;
        }

        if file.seek(SeekFrom::Start(at)).is_err() {
            return;
        }
        let mut chunk = Vec::new();
        if file.take(1 << 20).read_to_end(&mut chunk).is_err() {
            return;
        }
        *self.offset.borrow_mut() = at + chunk.len() as u64;

        // Feed the terminal, and answer anything it wants to say back.
        let replies = {
            let mut term = self.term.borrow_mut();
            match term.as_mut() {
                Some(t) => t.feed(&chunk),
                None => Vec::new(),
            }
        };
        for reply in replies {
            match reply {
                crate::ansi::Reply::Bytes(bytes) => {
                    send_keys(&self.store, &self.link, &self.link_for, name, &bytes);
                }
            }
        }

        self.render();
        self.update_status(name, size);
    }

    /// Draw the terminal into the text buffer.
    ///
    /// Only lines that are new are appended; when the terminal scrolls, the line
    /// count grows by one and one line is added. A resize rewrites everything.
    fn render(&self) {
        let term = self.term.borrow();
        let Some(term) = term.as_ref() else { return };

        let total = term.total_lines();
        let already = self.rendered.get();

        if total < already {
            // the screen was resized smaller: redraw from scratch
            self.buffer.set_text("");
            self.rendered.set(0);
            return self.render();
        }
        if total == already {
            // the cursor may still have moved within the last line; redraw it
            if already == 0 {
                return;
            }
            let last = already - 1;
            self.replace_line(last);
            return;
        }

        let mut end = self.buffer.end_iter();
        for i in already..total {
            let Some(line) = term.line(i) else { continue };
            self.append_line(&mut end, line);
        }
        self.rendered.set(total);

        if self.autoscroll.get() {
            let mut end = self.buffer.end_iter();
            self.view.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
        }
    }

    fn append_line(&self, end: &mut gtk4::TextIter, line: &[crate::ansi::Cell]) {
        self.write_line(end, line);
        self.buffer.insert(end, "\n");
    }

    /// Rewrite one line in place, used when only the cursor moved.
    fn replace_line(&self, index: usize) {
        let term = self.term.borrow();
        let Some(term) = term.as_ref() else { return };
        let Some(line) = term.line(index) else { return };

        let start = self.buffer.iter_at_line(index as i32);
        let end = self.buffer.iter_at_line((index + 1) as i32);
        let (Some(start), Some(end)) = (start, end) else { return };
        self.buffer.delete(&mut start.clone(), &mut end.clone());

        let mut at = self.buffer.iter_at_line(index as i32).unwrap_or_else(|| {
            self.buffer.end_iter()
        });
        self.write_line(&mut at, line);
    }

    /// Write one line's cells, grouped into runs with the same appearance so the
    /// number of tags stays small.
    fn write_line(&self, at: &mut gtk4::TextIter, line: &[crate::ansi::Cell]) {
        // trim trailing blanks so lines do not carry invisible padding
        let last = line
            .iter()
            .rposition(|c| c.ch != ' ' || c.pen.bg != Color::Default)
            .map(|i| i + 1)
            .unwrap_or(0);
        let line = &line[..last];

        let mut i = 0;
        while i < line.len() {
            let key = TagKey::of(line[i].pen);
            let mut j = i + 1;
            while j < line.len() && TagKey::of(line[j].pen) == key {
                j += 1;
            }
            let text: String = line[i..j].iter().map(|c| c.ch).collect();

            let tag = self.tag_for(key);
            match tag {
                Some(tag) => self.buffer.insert_with_tags(at, &text, &[&tag]),
                None => self.buffer.insert(at, &text),
            }
            i = j;
        }
    }

    /// The tag for a given appearance, created on first use.
    fn tag_for(&self, key: TagKey) -> Option<gtk4::TextTag> {
        if let Some(t) = self.tags.borrow().get(&key) {
            return Some(t.clone());
        }

        let tag = gtk4::TextTag::new(None);
        let (fg, bg) = key.colours();
        tag.set_foreground(Some(&fg));
        if let Some(bg) = bg {
            tag.set_background(Some(&bg));
        }
        // pango takes a numeric weight, not the enum, in this binding
        tag.set_weight(if key.bold { 700 } else { 400 });
        tag.set_style(if key.italic {
            gtk4::pango::Style::Italic
        } else {
            gtk4::pango::Style::Normal
        });
        if key.underline {
            tag.set_underline(gtk4::pango::Underline::Single);
        }
        if key.strike {
            tag.set_strikethrough(true);
        }
        if key.dim {
            // no alpha on tags, so dim by blending towards the background
            tag.set_foreground(Some(&dim(&fg)));
        }

        self.buffer.tag_table().add(&tag);
        self.tags.borrow_mut().insert(key, tag.clone());
        Some(tag)
    }

    fn update_status(&self, name: &str, bytes: u64) {
        let state = vm::state(&self.store, name);
        let word = match state {
            State::Running { pid } => i18n::tr_running_pid(pid),
            State::Stopped => i18n::tr(i18n::Key::StateStopped).to_string(),
            State::Stale { pid } => i18n::tr_stale_pid(pid),
        };
        let (cx, cy) = self
            .term
            .borrow()
            .as_ref()
            .map(|t| t.cursor())
            .unwrap_or((0, 0));
        let lines = self.rendered.get();
        self.status.set_label(&format!(
            "{name}: {word} - {} - {lines} lines - cursor {cx},{cy}",
            i18n::tr_bytes_logged(bytes)
        ));
    }
}

/// Write to the guest, reconnecting once if the link is stale.
///
/// The usual cause of a stale link is that the machine was restarted, so the hub the
/// connection belonged to is gone. Without the retry the first line typed after every
/// restart would vanish.
fn send_keys(
    store: &Store,
    link: &Rc<RefCell<Option<UnixStream>>>,
    link_for: &Rc<RefCell<Option<String>>>,
    name: &str,
    bytes: &[u8],
) -> bool {
    for attempt in 0..2 {
        let needs_connect = {
            let same = link_for.borrow().as_deref() == Some(name);
            !same || link.borrow().is_none()
        };
        if needs_connect {
            let socket = store.console_socket(name);
            match UnixStream::connect(&socket) {
                Ok(s) => {
                    *link.borrow_mut() = Some(s);
                    *link_for.borrow_mut() = Some(name.to_string());
                }
                Err(e) => {
                    log::warn!("cannot reach {}: {e}", socket.display());
                    return false;
                }
            }
        }

        let mut guard = link.borrow_mut();
        let Some(s) = guard.as_mut() else { return false };
        match s.write_all(bytes).and_then(|_| s.flush()) {
            Ok(()) => return true,
            Err(e) => {
                log::warn!("the console link broke ({e}); reconnecting");
                *guard = None;
                drop(guard);
                if attempt == 1 {
                    log::warn!("could not deliver {} bytes", bytes.len());
                    return false;
                }
            }
        }
    }
    false
}

/// Blend a colour towards the terminal's background, for the dim attribute.
fn dim(hex: &str) -> String {
    let parse = |s: &str| u8::from_str_radix(s, 16).unwrap_or(0);
    if hex.len() != 7 {
        return hex.to_string();
    }
    let (r, g, b) = (
        parse(&hex[1..3]) as f32,
        parse(&hex[3..5]) as f32,
        parse(&hex[5..7]) as f32,
    );
    let mix = |v: f32, bg: f32| ((v + bg) / 2.0).round().clamp(0.0, 255.0) as u8;
    let (br, bg_, bb) = (0x1d as f32, 0x1f as f32, 0x21 as f32);
    format!(
        "#{:02x}{:02x}{:02x}",
        mix(r, br),
        mix(g, bg_),
        mix(b, bb)
    )
}
