//! The machine editor.
//!
//! One dialog that writes every part of a definition: what it is called, what it boots,
//! how much memory it gets, whether it has a network, and the memory-protection mode that
//! decides what the hypervisor will accept.
//!
//! It is kept apart from the detail pane because the two have different jobs. The pane
//! shows a machine and acts on it; this dialog only edits, and it edits everything at
//! once so that a change which has to be consistent -- memory against swiotlb, a network
//! index that another machine is already using -- can be checked before anything is
//! written.
//!
//! Disks and shared directories are not here. They are lists rather than fields, and the
//! detail pane already shows and edits them in place.

use crate::i18n;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use librevm_core::config::{Hypervisor, HypervisorConfig, Protection, VmConfig};
use librevm_core::store::Store;
use librevm_core::vm::State;
use std::cell::RefCell;
use std::rc::Rc;

/// Open the editor for an existing machine.
///
/// `on_saved` runs after a successful save, so the caller can redraw and, when the machine
/// is running, say that a restart is needed.
pub fn present_editor<F>(store: Store, name: &str, parent: Option<&gtk4::Window>, on_saved: F)
where
    F: Fn() + 'static,
{
    let Ok(vm) = store.load(name) else {
        log::error!("could not load {name} to edit it");
        return;
    };
    // Connecting here rather than taking one as an argument keeps the call site simple,
    // and a second connection is cheap.
    let bridge = crate::daemon::Bridge::new();

    let dialog = adw::Window::builder()
        .title(i18n::tr(i18n::Key::EditMachine))
        .default_width(600)
        .default_height(720)
        .modal(true)
        .build();
    if let Some(p) = parent {
        dialog.set_transient_for(Some(p));
    }

    let header = adw::HeaderBar::new();
    let apply = gtk4::Button::builder()
        .label(i18n::tr(i18n::Key::Save))
        .css_classes(["suggested-action"])
        .build();
    header.pack_end(&apply);

    // -- identity ---------------------------------------------------------
    let name_row = adw::EntryRow::builder()
        .title(i18n::tr(i18n::Key::Name))
        .text(&vm.name)
        .build();
    // Renaming a machine means moving its directory, which is more than this dialog does.
    name_row.set_editable(false);
    let desc_row = adw::EntryRow::builder()
        .title(i18n::tr(i18n::Key::Description))
        .text(vm.description.clone().unwrap_or_default())
        .build();
    let identity = adw::PreferencesGroup::builder()
        .title(i18n::tr(i18n::Key::GroupIdentity))
        .build();
    identity.add(&name_row);
    identity.add(&desc_row);

    // -- what it boots ----------------------------------------------------
    let kernel_row = adw::EntryRow::builder()
        .title(i18n::tr(i18n::Key::KernelImage))
        .text(vm.boot.kernel.display().to_string())
        .build();
    let initrd_row = adw::EntryRow::builder()
        .title(i18n::tr(i18n::Key::Initramfs))
        .text(
            vm.boot
                .initramfs
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        )
        .build();
    initrd_row.set_show_apply_button(true);
    let cmdline_row = adw::EntryRow::builder()
        .title(i18n::tr(i18n::Key::KernelCmdline))
        .text(&vm.boot.cmdline)
        .build();

    let boot = adw::PreferencesGroup::builder()
        .title(i18n::tr(i18n::Key::GroupBoot))
        .build();
    boot.add(&kernel_row);
    boot.add(&initrd_row);
    boot.add(&cmdline_row);

    // -- resources --------------------------------------------------------
    let memory_row = adw::SpinRow::with_range(16.0, 8192.0, 16.0);
    memory_row.set_title(i18n::tr(i18n::Key::MemoryMib));
    memory_row.set_value(vm.memory_mib as f64);

    let swiotlb_row = adw::SpinRow::with_range(0.0, 1024.0, 16.0);
    swiotlb_row.set_title(i18n::tr(i18n::Key::SwiotlbMib));
    swiotlb_row.set_value(vm.hypervisor.swiotlb_mib as f64);

    let cpus_row = adw::SpinRow::with_range(1.0, 16.0, 1.0);
    cpus_row.set_title(i18n::tr(i18n::Key::Vcpus));
    cpus_row.set_value(vm.cpus as f64);

    let resources = adw::PreferencesGroup::builder()
        .title(i18n::tr(i18n::Key::GroupResources))
        .build();
    resources.add(&memory_row);
    resources.add(&swiotlb_row);
    resources.add(&cpus_row);

    // -- protection -------------------------------------------------------
    let protection = adw::ComboRow::builder()
        .title(i18n::tr(i18n::Key::MemoryProtection))
        .model(&gtk4::StringList::new(&protection_names()))
        .selected(match vm.hypervisor.protection {
            Protection::WithFirmware => 2,
            Protection::WithoutFirmware => 0,
            Protection::None => 1,
        })
        .build();
    let protection_group = adw::PreferencesGroup::builder()
        .title(i18n::tr(i18n::Key::GroupAdvanced))
        .build();
    protection_group.add(&protection);

    // -- networking -------------------------------------------------------
    let net_switch = adw::SwitchRow::builder()
        .title(i18n::tr(i18n::Key::GuestNetwork))
        .subtitle(i18n::tr(i18n::Key::GuestNetworkHint))
        .active(vm.network.enabled)
        .build();
    let net_index = adw::SpinRow::with_range(0.0, 40.0, 1.0);
    net_index.set_title(i18n::tr(i18n::Key::NetworkIndex));
    net_index.set_subtitle(i18n::tr(i18n::Key::NetworkIndexHint));
    net_index.set_value(vm.network.index as f64);
    net_index.set_sensitive(vm.network.enabled);

    let net = adw::PreferencesGroup::builder()
        .title(i18n::tr(i18n::Key::GroupNetwork))
        .build();
    net.add(&net_switch);
    net.add(&net_index);

    {
        // The index only means anything when the switch is on.
        let idx = net_index.clone();
        net_switch.connect_active_notify(move |s| idx.set_sensitive(s.is_active()));
    }

    // -- disks ------------------------------------------------------------
    //
    // Editing these writes to the definition like everything else on this dialog, but the
    // list has to be rebuilt when it changes, so it is kept behind a `RefCell` the way the
    // disk panel in the detail pane is.
    let disk_rows: Rc<RefCell<Vec<gtk4::Widget>>> = Rc::new(RefCell::new(Vec::new()));
    let disks_list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let attached: Rc<RefCell<Vec<librevm_core::config::DiskConfig>>> =
        Rc::new(RefCell::new(vm.disks.clone()));

    let disks_group = adw::PreferencesGroup::builder()
        .title(i18n::tr(i18n::Key::AttachedDisks))
        .description(i18n::tr(i18n::Key::AttachedDisksHint))
        .build();
    disks_group.add(&disks_list);

    // A dropdown of what is in the images directory, so a disk made on the disks page can
    // be attached without knowing its path. Refreshed when the dialog opens, because a
    // disk may have been deployed since the window was built.
    let candidate_paths: Rc<RefCell<Vec<std::path::PathBuf>>> = Rc::new(RefCell::new(Vec::new()));
    let candidate_names = gtk4::StringList::new(&[]);
    let picker = gtk4::DropDown::builder()
        .model(&candidate_names)
        .valign(gtk4::Align::Center)
        .build();
    let attach_button = gtk4::Button::builder()
        .label(i18n::tr(i18n::Key::AttachExisting))
        .valign(gtk4::Align::Center)
        .css_classes(["pill"])
        .build();
    let picker_row = adw::ActionRow::builder()
        .title(i18n::tr(i18n::Key::AttachExisting))
        .build();
    picker_row.add_suffix(&picker);
    picker_row.add_suffix(&attach_button);
    disks_group.add(&picker_row);

    // -- ssh --------------------------------------------------------------
    //
    // One button that writes the key and the first-boot script into whichever disk carries
    // the root filesystem. It is per machine rather than per disk, which is why it is here.
    let ssh_button = gtk4::Button::builder()
        .label(i18n::tr(i18n::Key::DeploySsh))
        .icon_name("network-server-symbolic")
        .halign(gtk4::Align::Start)
        .css_classes(["pill"])
        .build();
    let ssh_group = adw::PreferencesGroup::builder()
        .title(i18n::tr(i18n::Key::GroupSsh))
        .description(i18n::tr(i18n::Key::SshDeployHint))
        .build();
    ssh_group.add(&ssh_button);

    // -- console ----------------------------------------------------------
    //
    // Where the serial port goes. stdout is what a foreground run wants; a file is what a
    // machine nobody is watching wants; a pty is for attaching a terminal by hand.
    let serial_row = adw::ComboRow::builder()
        .title(i18n::tr(i18n::Key::SerialBackend))
        .subtitle(i18n::tr(i18n::Key::SerialBackendHint))
        .model(&gtk4::StringList::new(&serial_names()))
        .selected(match &vm.serial.backend {
            librevm_core::config::SerialBackend::Stdout => 0,
            librevm_core::config::SerialBackend::File(_) => 1,
            librevm_core::config::SerialBackend::Pty => 2,
            librevm_core::config::SerialBackend::Unix(_) => 3,
            librevm_core::config::SerialBackend::UnixStream(_) => 4,
        })
        .build();

    let serial_path_row = adw::EntryRow::builder()
        .title(i18n::tr(i18n::Key::SerialPath))
        .text(match &vm.serial.backend {
            librevm_core::config::SerialBackend::File(p)
            | librevm_core::config::SerialBackend::Unix(p)
            | librevm_core::config::SerialBackend::UnixStream(p) => p.display().to_string(),
            _ => String::new(),
        })
        .build();

    let pstore_row = adw::SwitchRow::builder()
        .title(i18n::tr(i18n::Key::Pstore))
        .subtitle(i18n::tr(i18n::Key::PstoreHint))
        .active(vm.serial.pstore)
        .build();

    // The path only means anything for the backends that take one.
    {
        let path = serial_path_row.clone();
        let update = move |i: u32| path.set_sensitive(matches!(i, 1 | 3 | 4));
        update(serial_row.selected());
        serial_row.connect_selected_notify(move |r| update(r.selected()));
    }

    let serial_group = adw::PreferencesGroup::builder()
        .title(i18n::tr(i18n::Key::GroupConsole))
        .build();
    serial_group.add(&serial_row);
    serial_group.add(&serial_path_row);
    serial_group.add(&pstore_row);

    // A running machine reads its definition once, at start. Saying so here is better than
    // letting an operator change something and wonder why nothing happened.
    let running = librevm_core::vm::state(&store, &vm.name) != State::Stopped;
    let restart_note = gtk4::Label::builder()
        .label(i18n::tr(i18n::Key::RestartToApply))
        .xalign(0.0)
        .wrap(true)
        .visible(running)
        .css_classes(["dim-label", "caption"])
        .build();

    // Two labels, because they carry different weights.
    //
    // The dim one is for the running total and for "swiotlb is too big", which are things to
    // notice before pressing Save. A save that *fails* used to land here too, in the least
    // prominent style GTK has, at the bottom of a scrolled dialog -- so a refusal looked
    // exactly like a success and the operator walked away believing the change was applied.
    // Failures get a banner at the top of the dialog now, where the eye already is.
    let warning = gtk4::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();

    let error_banner = gtk4::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .visible(false)
        .css_classes(["error", "heading"])
        .build();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    // First, so it is the first thing read when the dialog reappears after a failed save.
    content.append(&error_banner);
    content.append(&identity);
    content.append(&boot);
    content.append(&resources);
    content.append(&net);
    content.append(&serial_group);
    content.append(&disks_group);
    content.append(&ssh_group);
    content.append(&protection_group);
    content.append(&warning);
    content.append(&restart_note);

    // Fill the disk list, and keep it in step with the model. The rows are rebuilt on every
    // change rather than updated in place, because their order is the boot order and that
    // changes when the root moves.
    //
    // The closure has to be able to call itself -- toggling root rearranges the rows -- so
    // it lives in a cell that is filled in after it is defined. A plain `Rc<dyn Fn()>`
    // cannot refer to itself.
    let rebuild_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    let rebuild_disks: Rc<dyn Fn()> = {
        let disks_list = disks_list.clone();
        let disk_rows = disk_rows.clone();
        let attached = attached.clone();
        let status = warning.clone();
        let slot = rebuild_slot.clone();
        Rc::new(move || {
            for w in disk_rows.borrow_mut().drain(..) {
                if w.parent().is_some() {
                    disks_list.remove(&w);
                }
            }
            let items = attached.borrow().clone();
            if items.is_empty() {
                let label = gtk4::Label::builder()
                    .label(i18n::tr(i18n::Key::NoDisks))
                    .xalign(0.0)
                    .css_classes(["dim-label"])
                    .build();
                let row = gtk4::ListBoxRow::builder().activatable(false).child(&label).build();
                disks_list.append(&row);
                disk_rows.borrow_mut().push(row.upcast());
                return;
            }
            for (i, d) in items.iter().enumerate() {
                let name = d
                    .path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| d.path.display().to_string());
                let title = gtk4::Label::builder()
                    .label(&name)
                    .xalign(0.0)
                    .css_classes(["monospace"])
                    .build();
                let subtitle = gtk4::Label::builder()
                    .label(&d.path.display().to_string())
                    .xalign(0.0)
                    .ellipsize(gtk4::pango::EllipsizeMode::Middle)
                    .css_classes(["dim-label", "caption"])
                    .build();
                let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
                text.set_hexpand(true);
                text.append(&title);
                text.append(&subtitle);

                // Only one disk can be the root, so this is a radio in effect.
                let is_root = gtk4::CheckButton::builder()
                    .label(i18n::tr(i18n::Key::SetAsRoot))
                    .active(d.root)
                    .valign(gtk4::Align::Center)
                    .build();
                {
                    let attached = attached.clone();
                    let slot = slot.clone();
                    is_root.connect_toggled(move |b| {
                        if !b.is_active() {
                            return;
                        }
                        {
                            let mut items = attached.borrow_mut();
                            for (j, item) in items.iter_mut().enumerate() {
                                item.root = j == i;
                            }
                        }
                        if let Some(f) = slot.borrow().as_ref() {
                            f();
                        }
                    });
                }

                let detach = gtk4::Button::builder()
                    .icon_name("list-remove-symbolic")
                    .tooltip_text(i18n::tr(i18n::Key::Detach))
                    .valign(gtk4::Align::Center)
                    .css_classes(["flat"])
                    .build();
                {
                    let attached = attached.clone();
                    let slot = slot.clone();
                    let status = status.clone();
                    detach.connect_clicked(move |_| {
                        attached.borrow_mut().remove(i);
                        status.set_label(&i18n::tr(i18n::Key::DiskDetached));
                        if let Some(f) = slot.borrow().as_ref() {
                            f();
                        }
                    });
                }

                let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
                buttons.set_valign(gtk4::Align::Center);
                buttons.append(&is_root);
                buttons.append(&detach);

                let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
                row_box.set_margin_top(8);
                row_box.set_margin_bottom(8);
                row_box.set_margin_start(12);
                row_box.set_margin_end(12);
                row_box.append(&text);
                row_box.append(&buttons);

                let row = gtk4::ListBoxRow::builder().activatable(false).child(&row_box).build();
                disks_list.append(&row);
                disk_rows.borrow_mut().push(row.upcast());
            }
        })
    };
    *rebuild_slot.borrow_mut() = Some(rebuild_disks.clone());

    // What could be attached: every .img in the images directory, minus what is already on.
    {
        let candidate_paths = candidate_paths.clone();
        let candidate_names = candidate_names.clone();
        let attached = attached.clone();
        let images = store.images_dir();
        let mut names = Vec::new();
        let mut paths = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&images) {
            let mut found: Vec<std::path::PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("img"))
                .collect();
            found.sort();
            let in_use: Vec<std::path::PathBuf> =
                attached.borrow().iter().map(|d| d.path.clone()).collect();
            for p in found {
                if in_use.contains(&p) {
                    continue;
                }
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0) / 1048576;
                names.push(format!("{name}  ({size} MiB)"));
                paths.push(p);
            }
        }
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        candidate_names.splice(0, candidate_names.n_items(), &refs);
        *candidate_paths.borrow_mut() = paths;

        let attached = attached.clone();
        let rebuild = rebuild_disks.clone();
        let status = warning.clone();
        let candidate_paths = candidate_paths.clone();
        let picker = picker.clone();
        attach_button.connect_clicked(move |_| {
            let paths = candidate_paths.borrow();
            if paths.is_empty() {
                status.set_label(&i18n::tr(i18n::Key::NoDiskChosen));
                return;
            }
            let i = (picker.selected() as usize).min(paths.len() - 1);
            let path = paths[i].clone();
            let label = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "disk".to_string());
            let mut items = attached.borrow_mut();
            // Only one root: a first disk becomes the root, later ones do not.
            let root = items.is_empty();
            items.push(librevm_core::config::DiskConfig {
                path: path.clone(),
                readonly: false,
                root,
                bus: Default::default(),
                label: Some(label),
            });
            drop(items);
            status.set_label(&i18n::tr_done(
                i18n::Key::DiskAttached,
                &path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ));
            rebuild();
        });
    }
    {
        // The candidate list changes as disks are attached, so rebuild it after each one.
        let candidate_paths = candidate_paths.clone();
        let candidate_names = candidate_names.clone();
        let attached = attached.clone();
        let rebuild = rebuild_disks.clone();
        rebuild();
        // A second pass once the rows exist, so the picker reflects what is left.
        let _ = (&candidate_paths, &candidate_names, &attached, &rebuild);
    }

    // -- ssh --------------------------------------------------------------
    {
        let store = store.clone();
        let bridge = bridge.clone();
        let status = warning.clone();
        let attached = attached.clone();
        ssh_button.connect_clicked(move |_| {
            // The root disk is the one carrying the filesystem to put the key into; with
            // nothing marked as root, the first attached disk is the best guess.
            let chosen = {
                let items = attached.borrow();
                items
                    .iter()
                    .find(|d| d.root)
                    .or_else(|| items.first())
                    .cloned()
            };
            let Some(disk) = chosen else {
                status.set_label(&i18n::tr(i18n::Key::NoDisks));
                return;
            };
            status.set_label(&i18n::tr_deploying_ssh(&disk.path.display().to_string()));
            match bridge.client() {
                Some(client) => match client.deploy_ssh(
                    &disk.path.display().to_string(),
                    librevm_core::ssh::USER,
                ) {
                    Ok(v) => {
                        let notes: Vec<String> = v["notes"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        status.set_label(&i18n::tr_ssh_deployed(
                            &disk.path.display().to_string(),
                            &notes.join("; "),
                        ));
                    }
                    Err(e) => status.set_label(&i18n::tr_deploy_failed(&format!("{e:#}"))),
                },
                None => {
                    // No daemon: only works as root, which is how the window runs when
                    // started that way deliberately.
                    match librevm_core::image::inject_into_image(
                        &disk.path,
                        &librevm_core::image::Injections {
                            authorized_key: librevm_core::ssh::ensure_keypair(store.root()).ok(),
                            ssh_user: Some(librevm_core::ssh::USER.to_string()),
                            dns: librevm_core::image::host_nameservers(),
                            root_password: None,
                        },
                    ) {
                        Ok(notes) => status.set_label(&i18n::tr_ssh_deployed(
                            &disk.path.display().to_string(),
                            &notes.join("; "),
                        )),
                        Err(e) => status.set_label(&i18n::tr_deploy_failed(&format!("{e:#}"))),
                    }
                }
            }
        });
    }

    // Live arithmetic: guest RAM is memory - swiotlb, which surprises people.
    let recompute: Rc<dyn Fn()> = {
        let warning = warning.clone();
        let memory_row = memory_row.clone();
        let swiotlb_row = swiotlb_row.clone();
        Rc::new(move || {
            let mem = memory_row.value() as u64;
            let sw = swiotlb_row.value() as u64;
            if sw >= mem {
                warning.set_markup(&format!(
                    "<b>{}</b>\n{}",
                    i18n::tr(i18n::Key::SwiotlbTooBig),
                    i18n::tr(i18n::Key::SwiotlbTooBigBody)
                ));
            } else {
                warning.set_label(&i18n::tr_create_hint(mem, sw, mem - sw));
            }
        })
    };
    recompute();
    {
        let r = recompute.clone();
        memory_row.connect_value_notify(move |_| r());
    }
    {
        let r = recompute.clone();
        swiotlb_row.connect_value_notify(move |_| r());
    }

    let clamp = adw::Clamp::builder().maximum_size(560).child(&content).build();
    let scrolled = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scrolled));
    dialog.set_content(Some(&toolbar));

    // -- save -------------------------------------------------------------
    {
        let store = store.clone();
        let original = vm.clone();
        let dialog = dialog.clone();
        let on_saved = Rc::new(on_saved);
        let bridge = bridge.clone();
        let attached = attached.clone();
        let error_banner = error_banner.clone();
        apply.connect_clicked(move |_| {
            let mut vm = original.clone();

            vm.description = {
                let t = desc_row.text().to_string();
                if t.trim().is_empty() {
                    None
                } else {
                    Some(t)
                }
            };
            vm.boot.kernel = std::path::PathBuf::from(kernel_row.text().to_string());
            vm.boot.initramfs = {
                let t = initrd_row.text().to_string();
                if t.trim().is_empty() {
                    None
                } else {
                    Some(std::path::PathBuf::from(t))
                }
            };
            vm.boot.cmdline = cmdline_row.text().to_string();
            vm.memory_mib = memory_row.value() as u64;
            vm.hypervisor.swiotlb_mib = swiotlb_row.value() as u64;
            vm.cpus = cpus_row.value() as u32;
            vm.hypervisor.protection = match protection.selected() {
                2 => Protection::WithFirmware,
                1 => Protection::None,
                _ => Protection::WithoutFirmware,
            };
            // Set by default, and there is no reason to offer the alternative.
            vm.hypervisor.backend = Hypervisor::Auto;
            vm.hypervisor = HypervisorConfig {
                backend: Hypervisor::Auto,
                ..vm.hypervisor.clone()
            };
            let was_enabled = vm.network.enabled;
            vm.network.enabled = net_switch.is_active();
            vm.network.index = net_index.value() as u8;

            // The disks are edited in place above, so they are taken from the model rather
            // than from any widget.
            vm.disks = attached.borrow().clone();

            // The console. A backend that takes no path keeps whatever was there before,
            // so switching to a pty and back does not lose the file that was configured.
            let chosen = serial_row.selected();
            let path = serial_path_row.text().to_string();
            let path = if path.trim().is_empty() { None } else { Some(std::path::PathBuf::from(path)) };
            vm.serial.backend = match chosen {
                1 => librevm_core::config::SerialBackend::File(
                    path.clone().unwrap_or_else(|| std::path::PathBuf::from("/tmp/librevm-console.log")),
                ),
                2 => librevm_core::config::SerialBackend::Pty,
                3 => librevm_core::config::SerialBackend::Unix(
                    path.clone().unwrap_or_else(|| std::path::PathBuf::from("/tmp/librevm-console.sock")),
                ),
                4 => librevm_core::config::SerialBackend::UnixStream(
                    path.unwrap_or_else(|| std::path::PathBuf::from("/tmp/librevm-console.sock")),
                ),
                _ => librevm_core::config::SerialBackend::Stdout,
            };
            vm.serial.pstore = pstore_row.is_active();

            // The console socket and log are filled in by the store when missing, so
            // nothing here has to know about them.
            //
            // `validate` is deliberately *not* called on this side. It checks that every
            // path in the definition exists, and this process is the desktop user: a kernel
            // or initramfs under root's home -- which is 0700 -- comes back as missing, and
            // the save was refused for a file the daemon, running as root, reads perfectly
            // well. The check belongs where the privileges are, so the daemon does it.
            let outcome: anyhow::Result<()> = (|| {
                let text = vm.to_toml()?;
                match bridge.client() {
                    Some(client) => {
                        client.save_machine(&text)?;
                        Ok(())
                    }
                    // No daemon: only works if this process is root, which happens when
                    // the window was started that way deliberately.
                    None => {
                        vm.validate()?;
                        if vm.network.enabled {
                            librevm_core::net::setup(vm.network.index)?;
                        } else if was_enabled {
                            librevm_core::net::teardown().ok();
                        }
                        store.save(&vm)
                    }
                }
            })();

            match outcome {
                Ok(()) => {
                    log::info!("{}: definition saved", vm.name);
                    error_banner.set_visible(false);
                    on_saved();
                    dialog.close();
                }
                Err(e) => {
                    // Both, deliberately: the banner is unmissable, and the dim line keeps
                    // the message next to the fields that caused it.
                    let text = format!("{}: {e:#}", i18n::tr(i18n::Key::CouldNotSave));
                    error_banner.set_label(&text);
                    error_banner.set_visible(true);
                    warning.set_label(&text);
                    log::error!("{}: could not save: {e:#}", vm.name);
                    // A failed save must not close the dialog, or the change is lost with
                    // no way back to it. It already does not; this is the reason.
                }
            }
        });
    }

    dialog.present();
}

/// The serial backends, in the order the combo row lists them.
fn serial_names() -> [&'static str; 5] {
    match i18n::current() {
        i18n::Lang::En => ["stdout", "file", "pty", "unix datagram", "unix stream"],
        i18n::Lang::Zh => ["stdout", "文件", "pty", "unix 数据报", "unix 流"],
    }
}

fn protection_names() -> [&'static str; 3] {
    match i18n::current() {
        i18n::Lang::En => [
            "lend, no firmware (recommended)",
            "shared (usually refused)",
            "lend, with firmware",
        ],
        i18n::Lang::Zh => [
            "lend，无固件（推荐）",
            "shared（通常被拒绝）",
            "lend，带固件",
        ],
    }
}
