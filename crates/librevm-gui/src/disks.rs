//! The disks and shared-directory sections of the detail pane.
//!
//! Both edit the VM definition, and both need the same two things afterwards: save, and
//! tell the rest of the window that something changed. They are kept together here so
//! the detail pane stays about the machine as a whole.
//!
//! A note on what these can and cannot do: attaching a disk writes the definition, and
//! the definition reaches crosvm as `--block`. Whether the guest can *use* it is a
//! separate question -- on Gunyah the virtio block driver crashes crosvm the moment it
//! touches the queue, which is written up in docs/virtio-on-gunyah.md. The panel says
//! so rather than pretending, because otherwise the failure looks like a bug.

use crate::i18n;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use librevm_core::config::{DiskConfig, SharedDirConfig};
use librevm_core::disk;
use librevm_core::store::Store;
use librevm_core::vm;
use librevm_core::vm::State;
use std::path::PathBuf;
use std::rc::Rc;

    /// Everything the panel needs to act on behalf of one machine.
pub struct DisksPanel {
    group: adw::PreferencesGroup,
    store: Store,
    on_changed: Rc<dyn Fn()>,
    parent: Rc<std::cell::RefCell<Option<gtk4::Window>>>,
    /// The shares, in their own group, because an `adw::PreferencesGroup` cannot be
    /// nested inside another one.
    shares: Rc<std::cell::RefCell<Option<adw::PreferencesGroup>>>,
    /// The machine currently shown, so the action buttons know what to edit.
    current: Rc<std::cell::RefCell<Option<String>>>,
    /// Exactly the rows this panel put into `group`.
    ///
    /// Clearing by walking `first_child()` and calling `remove()` does not work:
    /// `adw::PreferencesGroup` keeps its own internal box, and a child it does not
    /// recognise makes `remove()` fail while leaving `first_child()` returning the same
    /// widget. The result is an infinite loop, and Adwaita prints a CRITICAL for each
    /// turn of it -- which is how a console log reached 95 GB and filled the disk.
    shown: Rc<std::cell::RefCell<Vec<gtk4::Widget>>>,
}

impl DisksPanel {
    pub fn new(store: Store, on_changed: Rc<dyn Fn()>) -> Rc<Self> {
        let group = adw::PreferencesGroup::builder()
            .title(i18n::tr(i18n::Key::Disks))
            .build();

        Rc::new(Self {
            group,
            store,
            on_changed,
            parent: Rc::new(std::cell::RefCell::new(None)),
            shares: Rc::new(std::cell::RefCell::new(None)),
            current: Rc::new(std::cell::RefCell::new(None)),
            shown: Rc::new(std::cell::RefCell::new(Vec::new())),
        })
    }

    /// Add a row and remember it, so it can be taken out again.
    fn add(&self, child: &impl IsA<gtk4::Widget>) {
        let widget = child.clone().upcast::<gtk4::Widget>();
        self.group.add(&widget);
        self.shown.borrow_mut().push(widget);
    }

    /// Take out everything this panel added.
    fn clear(&self) {
        for widget in self.shown.borrow_mut().drain(..) {
            // Only remove it if Adwaita still considers it a child; otherwise `remove`
            // fails and there is nothing to undo anyway.
            if widget.parent().is_some() {
                self.group.remove(&widget);
            }
        }
    }

    pub fn widget(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    /// Remember the window, for file choosers and dialogs.
    pub fn set_parent(&self, parent: &impl IsA<gtk4::Window>) {
        *self.parent.borrow_mut() = Some(parent.clone().upcast());
    }

    /// Rebuild the rows for a machine. `None` clears the panel.
    pub fn render(self: &Rc<Self>, name: Option<&str>) {
        self.clear();
        *self.current.borrow_mut() = name.map(|s| s.to_string());
        *self.shares.borrow_mut() = None;
        let Some(name) = name else { return };

        let Ok(vm) = self.store.load(name) else { return };
        let running = vm::state(&self.store, name) != State::Stopped;

        // -- disks ----------------------------------------------------------
        if vm.disks.is_empty() {
            self.add(&note(&i18n::tr(i18n::Key::NoDisks)));
        }
        for (index, d) in vm.disks.iter().enumerate() {
            self.add(&self.disk_row(name, index, d));
        }

        // -- shared directories ---------------------------------------------
        if !vm.shared_dirs.is_empty() {
            let shares = adw::PreferencesGroup::builder()
                .title(i18n::tr(i18n::Key::SharedDirs))
                .build();
            for (index, s) in vm.shared_dirs.iter().enumerate() {
                shares.add(&self.share_row(name, index, s));
            }
            // The panel owns one group, so the shares live in a second group that
            // the detail pane adds separately.
            *self.shares.borrow_mut() = Some(shares);
        } else {
            *self.shares.borrow_mut() = None;
        }

        // -- the add row ------------------------------------------------------
        let add = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        add.set_margin_top(6);
        add.set_margin_bottom(6);
        add.set_margin_start(6);
        add.set_margin_end(6);

        let attach = gtk4::Button::builder()
            .label(i18n::tr(i18n::Key::AttachDisk))
            .icon_name("list-add-symbolic")
            .build();
        let create = gtk4::Button::builder()
            .label(i18n::tr(i18n::Key::CreateDisk))
            .icon_name("drive-harddisk-symbolic")
            .build();
        let add_share = gtk4::Button::builder()
            .label(i18n::tr(i18n::Key::AddShare))
            .icon_name("folder-symbolic")
            .build();

        {
            let me = self.clone();
            let name = name.to_string();
            attach.connect_clicked(move |_| me.choose_disk(&name));
        }
        {
            let me = self.clone();
            let name = name.to_string();
            create.connect_clicked(move |_| me.create_disk_dialog(&name));
        }
        {
            let me = self.clone();
            let name = name.to_string();
            add_share.connect_clicked(move |_| me.choose_share(&name));
        }

        add.append(&attach);
        add.append(&create);
        add.append(&add_share);

        let row = adw::PreferencesRow::builder().build();
        let boxed = gtk4::ListBox::new();
        boxed.set_selection_mode(gtk4::SelectionMode::None);
        boxed.append(&add);
        row.set_child(Some(&boxed));
        self.add(&row);

        if running {
            self.add(&note(&i18n::tr(i18n::Key::RestartToApply)));
        }
        self.add(&note(&i18n::tr(i18n::Key::VirtioWarning)));
    }

    /// The shares, built during `render`, if this machine has any.
    ///
    /// The detail pane adds this to its own holder box rather than nesting it inside the
    /// disks group, which Adwaita does not allow.
    pub fn shares(&self) -> Option<adw::PreferencesGroup> {
        self.shares.borrow().clone()
    }

    fn disk_row(self: &Rc<Self>, name: &str, index: usize, d: &DiskConfig) -> adw::ActionRow {
        let file = d.path.file_name().and_then(|n| n.to_str()).unwrap_or("disk");
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(file).to_string())
            .subtitle(self.describe(&d.path, d))
            .build();

        if d.root {
            row.add_prefix(&badge(&i18n::tr(i18n::Key::RootDisk)));
        }
        if d.readonly {
            row.add_prefix(&badge(&i18n::tr(i18n::Key::ReadOnly)));
        }

        let detach = gtk4::Button::builder()
            .icon_name("list-remove-symbolic")
            .tooltip_text(i18n::tr(i18n::Key::Detach))
            .valign(gtk4::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let me = self.clone();
            let name = name.to_string();
            detach.connect_clicked(move |_| me.detach(&name, index));
        }
        row.add_suffix(&detach);
        row
    }

    fn share_row(self: &Rc<Self>, name: &str, index: usize, s: &SharedDirConfig) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&s.tag))
            .subtitle(s.path.display().to_string())
            .build();
        if s.readonly {
            row.add_prefix(&badge(&i18n::tr(i18n::Key::ReadOnly)));
        }

        let remove = gtk4::Button::builder()
            .icon_name("list-remove-symbolic")
            .tooltip_text(i18n::tr(i18n::Key::Detach))
            .valign(gtk4::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let me = self.clone();
            let name = name.to_string();
            remove.connect_clicked(move |_| me.remove_share(&name, index));
        }
        row.add_suffix(&remove);
        row
    }

    /// A one-line description of an image, read from the file itself so it is right even
    /// if the image was replaced behind our back.
    fn describe(&self, path: &std::path::Path, d: &DiskConfig) -> String {
        match disk::open(path) {
            Ok(info) => {
                let mut parts = vec![
                    info.format.name().to_string(),
                    disk::human(info.capacity),
                ];
                if info.allocated * 4 < info.capacity {
                    parts.push(format!("{} on disk", disk::human(info.allocated)));
                }
                if let Some(label) = &d.label {
                    parts.push(label.clone());
                }
                parts.join(" - ")
            }
            Err(e) => format!("{}: {e:#}", path.display()),
        }
    }

    // -- actions ------------------------------------------------------------

    fn choose_disk(self: &Rc<Self>, name: &str) {
        let chooser = gtk4::FileDialog::builder()
            .title(i18n::tr(i18n::Key::AttachDisk))
            .modal(true)
            .build();

        let me = self.clone();
        let name = name.to_string();
        chooser.open(self.parent.borrow().as_ref(), gtk4::gio::Cancellable::NONE, move |result| {
            let Ok(file) = result else { return };
            let Some(path) = file.path() else { return };
            me.attach(&name, path);
        });
    }

    fn attach(self: &Rc<Self>, name: &str, path: PathBuf) {
        let result = (|| -> anyhow::Result<()> {
            let mut vm = self.store.load(name)?;
            if vm.disks.iter().any(|d| d.path == path) {
                anyhow::bail!("{} is already attached", path.display());
            }
            vm.disks.push(DiskConfig {
                path: path.clone(),
                readonly: false,
                root: false,
                label: None,
            });
            vm.validate()?;
            self.store.save(&vm)?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                log::info!("{name}: attached {}", path.display());
                self.refresh(name);
            }
            Err(e) => self.complain(self.parent.borrow().as_ref(), &i18n::tr(i18n::Key::AttachDisk), &format!("{e:#}")),
        }
    }

    fn detach(self: &Rc<Self>, name: &str, index: usize) {
        let result = (|| -> anyhow::Result<()> {
            let mut vm = self.store.load(name)?;
            if index >= vm.disks.len() {
                anyhow::bail!("no disk {index}");
            }
            vm.disks.remove(index);
            self.store.save(&vm)?;
            Ok(())
        })();
        match result {
            Ok(()) => self.refresh(name),
            Err(e) => self.complain(self.parent.borrow().as_ref(), &i18n::tr(i18n::Key::Detach), &format!("{e:#}")),
        }
    }

    fn remove_share(self: &Rc<Self>, name: &str, index: usize) {
        let result = (|| -> anyhow::Result<()> {
            let mut vm = self.store.load(name)?;
            if index >= vm.shared_dirs.len() {
                anyhow::bail!("no share {index}");
            }
            vm.shared_dirs.remove(index);
            self.store.save(&vm)?;
            Ok(())
        })();
        match result {
            Ok(()) => self.refresh(name),
            Err(e) => self.complain(self.parent.borrow().as_ref(), &i18n::tr(i18n::Key::Detach), &format!("{e:#}")),
        }
    }

    fn choose_share(self: &Rc<Self>, name: &str) {
        let chooser = gtk4::FileDialog::builder()
            .title(i18n::tr(i18n::Key::AddShare))
            .modal(true)
            .build();

        let me = self.clone();
        let name = name.to_string();
        chooser.select_folder(
            self.parent.borrow().as_ref(),
            gtk4::gio::Cancellable::NONE,
            move |result| {
                let Ok(file) = result else { return };
                let Some(path) = file.path() else { return };
                let tag = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("share")
                    .to_string();

                let result = (|| -> anyhow::Result<()> {
                    let mut vm = me.store.load(&name)?;
                    if vm.shared_dirs.iter().any(|d| d.tag == tag) {
                        anyhow::bail!("{name} already shares something tagged {tag:?}");
                    }
                    vm.shared_dirs.push(SharedDirConfig {
                        path: path.clone(),
                        tag: tag.clone(),
                        readonly: false,
                    });
                    vm.validate()?;
                    me.store.save(&vm)?;
                    Ok(())
                })();

                match result {
                    Ok(()) => {
                        log::info!("{name}: sharing {} as {tag}", path.display());
                        me.refresh(&name);
                    }
                    Err(e) => me.complain(
                        me.parent.borrow().as_ref(),
                        &i18n::tr(i18n::Key::AddShare),
                        &format!("{e:#}"),
                    ),
                }
            },
        );
    }

    /// Ask for a size, then make an image and attach it.
    fn create_disk_dialog(self: &Rc<Self>, name: &str) {
        let dir = self.store.images_dir();
        std::fs::create_dir_all(&dir).ok();
        let default_path = dir.join(format!("{name}-disk.img"));

        let dialog = adw::MessageDialog::builder()
            .title(i18n::tr(i18n::Key::CreateDisk))
            .body(i18n::tr(i18n::Key::CreateDiskBody))
            .build();

        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
        content.set_margin_top(6);
        content.set_margin_bottom(6);
        content.set_margin_start(6);
        content.set_margin_end(6);

        let path_row = gtk4::Entry::builder()
            .text(default_path.display().to_string())
            .hexpand(true)
            .build();
        let size_row = gtk4::Entry::builder().text("4G").hexpand(true).build();
        let format_row = gtk4::DropDown::from_strings(&["raw", "sparse"]);

        for (label, widget) in [
            (i18n::tr(i18n::Key::DiskPath), path_row.clone().upcast::<gtk4::Widget>()),
            (i18n::tr(i18n::Key::DiskSize), size_row.clone().upcast()),
            (i18n::tr(i18n::Key::DiskFormat), format_row.clone().upcast()),
        ] {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
            let l = gtk4::Label::builder().label(label).xalign(0.0).width_chars(10).build();
            row.append(&l);
            row.append(&widget);
            content.append(&row);
        }
        dialog.set_extra_child(Some(&content));

        dialog.add_response("cancel", &i18n::tr(i18n::Key::Cancel));
        dialog.add_response("create", &i18n::tr(i18n::Key::CreateDisk));
        dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("create"));
        dialog.set_close_response("cancel");

        let me = self.clone();
        let name = name.to_string();
        dialog.connect_response(None, move |d, response| {            d.close();
            if response != "create" {
                return;
            }

            let path = PathBuf::from(path_row.text().to_string());
            let size_text = size_row.text().to_string();
            let format = match format_row.selected() {
                1 => disk::Format::Sparse,
                _ => disk::Format::Raw,
            };

            let result = (|| -> anyhow::Result<String> {
                let size = disk::parse_size(&size_text)?;
                let made = disk::create(&path, size, format)?;
                let mut vm = me.store.load(&name)?;
                vm.disks.push(DiskConfig {
                    path: path.clone(),
                    readonly: false,
                    root: false,
                    label: None,
                });
                vm.validate()?;
                me.store.save(&vm)?;
                Ok(format!(
                    "{} ({}, {} on disk)",
                    made.path.display(),
                    disk::human(made.capacity),
                    disk::human(made.allocated)
                ))
            })();

            match result {
                Ok(what) => {
                    log::info!("{name}: created and attached {what}");
                    me.refresh(&name);
                }
                Err(e) => me.complain(
                    me.parent.borrow().as_ref(),
                    &i18n::tr(i18n::Key::CreateDisk),
                    &format!("{e:#}"),
                ),
            }
        });

        dialog.present();
    }

    fn refresh(self: &Rc<Self>, name: &str) {
        self.render(Some(name));
        (self.on_changed)();
    }

    fn complain(&self, parent: Option<&gtk4::Window>, heading: &str, body: &str) {
        let dialog = adw::MessageDialog::builder()
            .title(heading)
            .body(body)
            .build();
        dialog.add_response("ok", &i18n::tr(i18n::Key::Later));
        dialog.set_close_response("ok");
        // A message dialog attaches itself to whatever window is focused; the parent is
        // only needed when there is more than one.
        let _ = parent;
        dialog.present();
    }
}

fn note(text: &str) -> gtk4::Label {
    let label = gtk4::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(6)
        .margin_end(6)
        .css_classes(["dim-label", "caption"])
        .build();
    label.upcast()
}

fn badge(text: &str) -> gtk4::Label {
    let label = gtk4::Label::builder()
        .label(text)
        .valign(gtk4::Align::Center)
        .css_classes(["badge"])
        .build();
    label.upcast()
}
