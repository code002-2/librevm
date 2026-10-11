//! The images page.
//!
//! Two lists and a button. The first list is what can be fetched, from the mirrors in
//! `image::CATALOGUE`; the second is what has been deployed. Deploying does the whole job
//! in one call -- download, filesystem, unpack, key injection -- so the page's only real
//! work is to stay responsive while the daemon does it and to report what came back.
//!
//! The SSH part is deliberately separate. An image made before the key existed, or built
//! by hand, can have the key and the first-boot setup put into it without being rebuilt,
//! which is what the second button does.

use crate::i18n;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use librevm_core::store::Store;
use std::cell::RefCell;
use std::rc::Rc;

/// The images page, as a widget the window can drop into a stack.
pub struct ImagesPage {
    pub root: gtk4::Widget,
    list: gtk4::ListBox,
    deployed: gtk4::ListBox,
    status: gtk4::Label,
    /// The catalogue as last fetched, so the buttons know what they are acting on.
    entries: Rc<RefCell<Vec<CatalogEntry>>>,
    deployed_entries: Rc<RefCell<Vec<DeployedEntry>>>,
    /// Rows added to each list, so a refresh removes only those.
    ///
    /// Adwaita's own `remove_all` on a `ListBox` walks the children, which is what caused
    /// a runaway loop in the disks panel once. Tracking what was added avoids that class
    /// of bug entirely.
    shown: Rc<RefCell<Vec<gtk4::Widget>>>,
    shown_deployed: Rc<RefCell<Vec<gtk4::Widget>>>,
    store: Store,
    bridge: crate::daemon::Bridge,
}

#[derive(Clone, Debug)]
pub struct CatalogEntry {
    pub id: String,
    pub description: String,
    pub url: String,
    pub download_mib: u64,
    pub disk_mib: u64,
    pub cached: bool,
    pub mirrored: bool,
}

#[derive(Clone, Debug)]
pub struct DeployedEntry {
    pub name: String,
    pub path: String,
    pub bytes: u64,
    pub on_disk: u64,
}

impl ImagesPage {
    pub fn new(store: Store, bridge: crate::daemon::Bridge) -> Self {
        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let deployed = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();

        let status = gtk4::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();

        // -- what can be fetched ------------------------------------------
        let catalog_group = adw::PreferencesGroup::builder()
            .title(i18n::tr(i18n::Key::AvailableImages))
            .description(i18n::tr(i18n::Key::AvailableImagesHint))
            .build();
        catalog_group.add(&list);

        // -- what has been deployed ---------------------------------------
        let deployed_group = adw::PreferencesGroup::builder()
            .title(i18n::tr(i18n::Key::DeployedImages))
            .description(i18n::tr(i18n::Key::DeployedImagesHint))
            .build();
        deployed_group.add(&deployed);

        let ssh_note = gtk4::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label", "caption"])
            .label(i18n::tr(i18n::Key::SshDeployHint))
            .build();

        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);
        content.append(&catalog_group);
        content.append(&deployed_group);
        content.append(&ssh_note);
        content.append(&status);

        let clamp = adw::Clamp::builder().maximum_size(720).child(&content).build();
        let scrolled = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();

        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        let refresh = gtk4::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text(i18n::tr(i18n::Key::Refresh))
            .build();
        header.pack_end(&refresh);
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&scrolled));

        let page = Self {
            root: toolbar.upcast(),
            list,
            deployed,
            status,
            entries: Rc::new(RefCell::new(Vec::new())),
            deployed_entries: Rc::new(RefCell::new(Vec::new())),
            shown: Rc::new(RefCell::new(Vec::new())),
            shown_deployed: Rc::new(RefCell::new(Vec::new())),
            store,
            bridge,
        };

        {
            let page_state = page.entries.clone();
            let deployed_state = page.deployed_entries.clone();
            let list = page.list.clone();
            let deployed = page.deployed.clone();
            let status = page.status.clone();
            let bridge = page.bridge.clone();
            let store = page.store.clone();
            let shown = page.shown.clone();
            let shown_deployed = page.shown_deployed.clone();
            refresh.connect_clicked(move |_| {
                load(
                    &bridge,
                    &store,
                    &list,
                    &deployed,
                    &status,
                    &page_state,
                    &deployed_state,
                    &shown,
                    &shown_deployed,
                );
            });
        }

        page
    }

    /// Fetch the lists and fill the page.
    pub fn reload(&self) {
        load(
            &self.bridge,
            &self.store,
            &self.list,
            &self.deployed,
            &self.status,
            &self.entries,
            &self.deployed_entries,
            &self.shown,
            &self.shown_deployed,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn load(
    bridge: &crate::daemon::Bridge,
    store: &Store,
    list: &gtk4::ListBox,
    deployed: &gtk4::ListBox,
    status: &gtk4::Label,
    entries: &Rc<RefCell<Vec<CatalogEntry>>>,
    deployed_entries: &Rc<RefCell<Vec<DeployedEntry>>>,
    shown: &Rc<RefCell<Vec<gtk4::Widget>>>,
    shown_deployed: &Rc<RefCell<Vec<gtk4::Widget>>>,
) {
    let catalogue = match bridge.client() {
        Some(c) => c.image_catalogue(),
        None => Ok(serde_json::json!({
            "images": librevm_core::image::CATALOGUE.iter().map(|e| serde_json::json!({
                "id": e.id,
                "description": e.description,
                "url": e.url,
                "download_mib": e.download_mib,
                "disk_mib": e.disk_mib,
                "cached": false,
                "mirrored": false,
            })).collect::<Vec<_>>(),
            "mirrors": librevm_core::image::MIRRORS.iter()
                .map(|(n, u)| serde_json::json!({ "name": n, "url": u }))
                .collect::<Vec<_>>(),
        })),
    };

    let images_status = match bridge.client() {
        Some(c) => c.image_status(),
        None => Ok(serde_json::json!({ "cached": [], "images": [] })),
    };

    // -- catalogue --------------------------------------------------------
    for w in shown.borrow_mut().drain(..) {
        if w.parent().is_some() {
            list.remove(&w);
        }
    }
    let mut collected = Vec::new();
    if let Ok(v) = &catalogue {
        for e in v["images"].as_array().cloned().unwrap_or_default() {
            let entry = CatalogEntry {
                id: e["id"].as_str().unwrap_or("").to_string(),
                description: e["description"].as_str().unwrap_or("").to_string(),
                url: e["url"].as_str().unwrap_or("").to_string(),
                download_mib: e["download_mib"].as_u64().unwrap_or(0),
                disk_mib: e["disk_mib"].as_u64().unwrap_or(0),
                cached: e["cached"].as_bool().unwrap_or(false),
                mirrored: e["mirrored"].as_bool().unwrap_or(false),
            };
            let row = catalog_row(&entry, bridge, status, store);
            list.append(&row);
            shown.borrow_mut().push(row.upcast());
            collected.push(entry);
        }
    }
    *entries.borrow_mut() = collected;

    // -- deployed ---------------------------------------------------------
    for w in shown_deployed.borrow_mut().drain(..) {
        if w.parent().is_some() {
            deployed.remove(&w);
        }
    }
    let mut collected = Vec::new();
    if let Ok(v) = &images_status {
        for e in v["images"].as_array().cloned().unwrap_or_default() {
            let entry = DeployedEntry {
                name: e["name"].as_str().unwrap_or("").to_string(),
                path: e["path"].as_str().unwrap_or("").to_string(),
                bytes: e["bytes"].as_u64().unwrap_or(0),
                on_disk: e["on_disk"].as_u64().unwrap_or(0),
            };
            let row = deployed_row(&entry, bridge, status, store);
            deployed.append(&row);
            shown_deployed.borrow_mut().push(row.upcast());
            collected.push(entry);
        }
    }
    let deployed_count = collected.len();
    *deployed_entries.borrow_mut() = collected;

    if deployed_count == 0 {
        let label = gtk4::Label::builder()
            .label(i18n::tr(i18n::Key::NoImagesYet))
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();
        let row = gtk4::ListBoxRow::builder().activatable(false).child(&label).build();
        deployed.append(&row);
        shown_deployed.borrow_mut().push(row.upcast());
    }

    let mirror = catalogue
        .as_ref()
        .ok()
        .and_then(|v| v["mirror"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| i18n::tr(i18n::Key::MirrorDefault).to_string());
    status.set_label(&i18n::tr_images_status(
        entries.borrow().len(),
        deployed_count,
        &mirror,
    ));
}

/// A row for one catalogue entry, with a button that deploys it.
fn catalog_row(
    entry: &CatalogEntry,
    bridge: &crate::daemon::Bridge,
    status: &gtk4::Label,
    store: &Store,
) -> gtk4::ListBoxRow {
    let title = gtk4::Label::builder()
        .label(&entry.description)
        .xalign(0.0)
        .build();
    let subtitle = gtk4::Label::builder()
        .label(i18n::tr_image_row(entry.download_mib, entry.disk_mib))
        .xalign(0.0)
        .css_classes(["dim-label", "caption"])
        .build();
    let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.append(&title);
    text.append(&subtitle);

    let deploy = gtk4::Button::builder()
        .label(if entry.cached {
            i18n::tr(i18n::Key::Redeploy)
        } else {
            i18n::tr(i18n::Key::Deploy)
        })
        .valign(gtk4::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();

    let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    row_box.set_margin_top(10);
    row_box.set_margin_bottom(10);
    row_box.set_margin_start(12);
    row_box.set_margin_end(12);
    row_box.append(&text);
    row_box.append(&deploy);

    {
        let entry = entry.clone();
        let bridge = bridge.clone();
        let status = status.clone();
        let store = store.clone();
        let deploy_button = deploy.clone();
        deploy.connect_clicked(move |_| {
            // A default name from the id, so the dialog is one field and one decision.
            let dialog = deploy_dialog(&entry, &bridge, &status, &store);
            dialog.present();
            let _ = &deploy_button;
        });
    }

    gtk4::ListBoxRow::builder()
        .activatable(false)
        .child(&row_box)
        .build()
}

/// Ask for a name and a size, then do the deployment.
fn deploy_dialog(
    entry: &CatalogEntry,
    bridge: &crate::daemon::Bridge,
    status: &gtk4::Label,
    store: &Store,
) -> adw::Window {
    let window = adw::Window::builder()
        .title(i18n::tr(i18n::Key::DeployImage))
        .default_width(520)
        .modal(true)
        .build();

    let header = adw::HeaderBar::new();
    let go = gtk4::Button::builder()
        .label(i18n::tr(i18n::Key::Deploy))
        .css_classes(["suggested-action"])
        .build();
    header.pack_end(&go);

    let name_row = adw::EntryRow::builder()
        .title(i18n::tr(i18n::Key::ImageName))
        // A dot-free default: the name becomes the filename.
        .text(entry.id.replace('.', "-"))
        .build();

    let size_row = adw::EntryRow::builder()
        .title(i18n::tr(i18n::Key::DiskSize))
        .text(format!("{}M", entry.disk_mib))
        .build();

    let machine_row = adw::EntryRow::builder()
        .title(i18n::tr(i18n::Key::AttachTo))
        .text(
            store
                .list()
                .unwrap_or_default()
                .first()
                .cloned()
                .unwrap_or_default(),
        )
        .build();

    let as_root = adw::SwitchRow::builder()
        .title(i18n::tr(i18n::Key::RootDisk))
        .active(true)
        .build();

    let url = gtk4::Label::builder()
        .label(&entry.url)
        .xalign(0.0)
        .wrap(true)
        .selectable(true)
        .css_classes(["dim-label", "caption"])
        .build();

    let group = adw::PreferencesGroup::builder()
        .title(&entry.description)
        .build();
    group.add(&name_row);
    group.add(&size_row);
    group.add(&machine_row);
    group.add(&as_root);

    let source = adw::PreferencesGroup::builder()
        .title(i18n::tr(i18n::Key::Source))
        .build();
    source.add(&url);

    let note = gtk4::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label", "caption"])
        .label(i18n::tr(i18n::Key::DeployNote))
        .build();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.append(&group);
    content.append(&source);
    content.append(&note);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));

    {
        let window = window.clone();
        let status = status.clone();
        let bridge = bridge.clone();
        let name_row = name_row.clone();
        let size_row = size_row.clone();
        let machine_row = machine_row.clone();
        let as_root = as_root.clone();
        let id = entry.id.clone();
        go.connect_clicked(move |_| {
            let name = name_row.text().to_string();
            let size = parse_mib(&size_row.text());
            let machine = machine_row.text().to_string();

            // The call blocks for as long as the download takes, so say so first. A
            // spinner would be better, but the window would not repaint either way
            // without moving the work off this thread.
            status.set_label(&i18n::tr_deploying(&name));
            window.close();

            match bridge.client() {
                Some(client) => match client.deploy_image(
                    &id,
                    &name,
                    size,
                    &machine,
                    as_root.is_active(),
                ) {
                    Ok(v) => {
                        let path = v["path"].as_str().unwrap_or("").to_string();
                        let attached = v["attached"].as_bool().unwrap_or(false);
                        status.set_label(&i18n::tr_deployed(&path, attached, &machine));
                    }
                    Err(e) => status.set_label(&i18n::tr_deploy_failed(&format!("{e:#}"))),
                },
                None => status.set_label(i18n::tr(i18n::Key::NoDaemon)),
            }
        });
    }

    window
}

/// A row for a deployed image, with the operations that act on a disk file.
///
/// SSH setup is not here. It belongs to a machine rather than to a disk -- the key and the
/// first-boot script are per machine -- so it lives in that machine's settings.
fn deployed_row(
    entry: &DeployedEntry,
    bridge: &crate::daemon::Bridge,
    status: &gtk4::Label,
    _store: &Store,
) -> gtk4::ListBoxRow {
    let title = gtk4::Label::builder()
        .label(&entry.name)
        .xalign(0.0)
        .css_classes(["monospace"])
        .build();
    let subtitle = gtk4::Label::builder()
        .label(i18n::tr_image_usage(entry.bytes, entry.on_disk))
        .xalign(0.0)
        .css_classes(["dim-label", "caption"])
        .build();
    let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.append(&title);
    text.append(&subtitle);

    let resize = gtk4::Button::builder()
        .label(i18n::tr(i18n::Key::Resize))
        .valign(gtk4::Align::Center)
        .css_classes(["pill"])
        .build();
    let format = gtk4::Button::builder()
        .label(i18n::tr(i18n::Key::Format))
        .valign(gtk4::Align::Center)
        .css_classes(["pill"])
        .build();
    let delete = gtk4::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text(i18n::tr(i18n::Key::DeleteDisk))
        .valign(gtk4::Align::Center)
        .css_classes(["pill", "destructive-action"])
        .build();

    let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    buttons.set_valign(gtk4::Align::Center);
    buttons.append(&resize);
    buttons.append(&format);
    buttons.append(&delete);

    let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    row_box.set_margin_top(10);
    row_box.set_margin_bottom(10);
    row_box.set_margin_start(12);
    row_box.set_margin_end(12);
    row_box.append(&text);
    row_box.append(&buttons);

    // -- resize -----------------------------------------------------------
    {
        let path = entry.path.clone();
        let bridge = bridge.clone();
        let status = status.clone();
        let current = entry.bytes.max(1024 * 1024) / 1048576;
        resize.connect_clicked(move |button| {
            let window = button.root().and_downcast::<adw::Window>();
            let dialog = adw::MessageDialog::new(
                window.as_ref(),
                Some(&i18n::tr(i18n::Key::Resize)),
                None,
            );
            let row = adw::EntryRow::builder()
                .title(i18n::tr(i18n::Key::NewSize))
                .text(format!("{current}M"))
                .build();
            let group = adw::PreferencesGroup::new();
            group.add(&row);
            dialog.set_extra_child(Some(&group));
            dialog.add_response("cancel", &i18n::tr(i18n::Key::Cancel));
            dialog.add_response("go", &i18n::tr(i18n::Key::Resize));
            dialog.set_response_appearance("go", adw::ResponseAppearance::Suggested);
            let bridge = bridge.clone();
            let status = status.clone();
            let path = path.clone();
            dialog.connect_response(None, move |d, response| {
                if response == "go" {
                    let mib = parse_mib(&row.text());
                    if mib > 0 {
                        match bridge.client() {
                            Some(c) => match c.resize_disk(&path, mib) {
                                Ok(_) => status.set_label(&i18n::tr_done(
                                    i18n::Key::DiskResized,
                                    &format!("{mib} MiB"),
                                )),
                                Err(e) => {
                                    status.set_label(&i18n::tr_deploy_failed(&format!("{e:#}")))
                                }
                            },
                            None => status.set_label(i18n::tr(i18n::Key::NoDaemon)),
                        }
                    }
                }
                d.close();
            });
            dialog.present();
        });
    }

    // -- format -----------------------------------------------------------
    {
        let path = entry.path.clone();
        let bridge = bridge.clone();
        let status = status.clone();
        format.connect_clicked(move |button| {
            let window = button.root().and_downcast::<adw::Window>();
            let dialog = adw::MessageDialog::new(
                window.as_ref(),
                Some(&i18n::tr(i18n::Key::Format)),
                Some(&i18n::tr(i18n::Key::FormatWarning)),
            );
            // A dropdown rather than free text, because the only values that work are the
            // ones mkfs knows.
            let list = gtk4::StringList::new(&["ext4", "ext2", "vfat", "xfs", "btrfs"]);
            let dropdown = gtk4::DropDown::builder().model(&list).selected(0).build();
            let group = adw::PreferencesGroup::new();
            let row = adw::ActionRow::builder()
                .title(i18n::tr(i18n::Key::Filesystem))
                .build();
            row.add_suffix(&dropdown);
            group.add(&row);
            dialog.set_extra_child(Some(&group));
            dialog.add_response("cancel", &i18n::tr(i18n::Key::Cancel));
            dialog.add_response("go", &i18n::tr(i18n::Key::Format));
            dialog.set_response_appearance("go", adw::ResponseAppearance::Destructive);
            let bridge = bridge.clone();
            let status = status.clone();
            let path = path.clone();
            dialog.connect_response(None, move |d, response| {
                if response == "go" {
                    let fs = ["ext4", "ext2", "vfat", "xfs", "btrfs"]
                        [dropdown.selected().min(4) as usize]
                        .to_string();
                    match bridge.client() {
                        Some(c) => match c.format_disk(&path, &fs) {
                            Ok(_) => status.set_label(&i18n::tr_done(
                                i18n::Key::DiskFormatted,
                                &fs,
                            )),
                            Err(e) => status.set_label(&i18n::tr_deploy_failed(&format!("{e:#}"))),
                        },
                        None => status.set_label(i18n::tr(i18n::Key::NoDaemon)),
                    }
                }
                d.close();
            });
            dialog.present();
        });
    }

    // -- delete -----------------------------------------------------------
    {
        let path = entry.path.clone();
        let bridge = bridge.clone();
        let status = status.clone();
        let name = entry.name.clone();
        delete.connect_clicked(move |button| {
            let window = button.root().and_downcast::<adw::Window>();
            let dialog = adw::MessageDialog::new(
                window.as_ref(),
                Some(&i18n::tr(i18n::Key::ConfirmDelete)),
                Some(&i18n::tr(i18n::Key::ConfirmDeleteBody)),
            );
            dialog.set_body_use_markup(true);
            dialog.add_response("cancel", &i18n::tr(i18n::Key::Cancel));
            dialog.add_response("go", &i18n::tr(i18n::Key::DeleteDisk));
            dialog.set_response_appearance("go", adw::ResponseAppearance::Destructive);
            let bridge = bridge.clone();
            let status = status.clone();
            let path = path.clone();
            let name = name.clone();
            dialog.connect_response(None, move |d, response| {
                if response == "go" {
                    match bridge.client() {
                        // The daemon refuses if a machine still has it, and says which.
                        Some(c) => match c.delete_disk(&path) {
                            Ok(_) => status.set_label(&i18n::tr_done(i18n::Key::DiskDeleted, &name)),
                            Err(e) => status.set_label(&i18n::tr_deploy_failed(&format!("{e:#}"))),
                        },
                        None => status.set_label(i18n::tr(i18n::Key::NoDaemon)),
                    }
                }
                d.close();
            });
            dialog.present();
        });
    }

    gtk4::ListBoxRow::builder()
        .activatable(false)
        .child(&row_box)
        .build()
}

/// "4G", "4096M", "4g" all mean the same thing here.
fn parse_mib(text: &str) -> u64 {
    let t = text.trim();
    if t.is_empty() {
        return 0;
    }
    let (digits, unit) = match t.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => (&t[..t.len() - 1], c.to_ascii_lowercase()),
        _ => (t, 'm'),
    };
    let n: u64 = digits.trim().parse().unwrap_or(0);
    match unit {
        'g' => n * 1024,
        't' => n * 1024 * 1024,
        _ => n,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_read_in_the_units_people_write() {
        assert_eq!(parse_mib("4096M"), 4096);
        assert_eq!(parse_mib("4G"), 4096);
        assert_eq!(parse_mib("4g"), 4096);
        assert_eq!(parse_mib("1024"), 1024);
        assert_eq!(parse_mib(" 2048m "), 2048);
        assert_eq!(parse_mib("1T"), 1048576);
    }

    #[test]
    fn nonsense_sizes_are_zero_rather_than_a_panic() {
        assert_eq!(parse_mib(""), 0);
        assert_eq!(parse_mib("abc"), 0);
        assert_eq!(parse_mib("G"), 0);
    }
}
