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

    // Warnings go here, so an invalid combination is visible before Save rather than after.
    let warning = gtk4::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.append(&identity);
    content.append(&boot);
    content.append(&resources);
    content.append(&net);
    content.append(&protection_group);
    content.append(&warning);
    content.append(&restart_note);

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

            // The console socket and log are filled in by the store when missing, so
            // nothing here has to know about them.
            let outcome = vm.validate().and_then(|_| {
                let text = vm.to_toml()?;
                match bridge.client() {
                    // The store belongs to root and setting the tap up needs root, so the
                    // daemon does both. This is the path the window normally takes.
                    Some(client) => {
                        client.save_machine(&text)?;
                        Ok(())
                    }
                    // No daemon: only works if this process is root, which happens when
                    // the window was started that way deliberately.
                    None => {
                        if vm.network.enabled {
                            librevm_core::net::setup(vm.network.index)?;
                        } else if was_enabled {
                            librevm_core::net::teardown().ok();
                        }
                        store.save(&vm)
                    }
                }
            });

            match outcome {
                Ok(()) => {
                    log::info!("{}: definition saved", vm.name);
                    on_saved();
                    dialog.close();
                }
                Err(e) => {
                    warning.set_markup(&format!(
                        "<b>{}</b>\n{}",
                        i18n::tr(i18n::Key::CouldNotSave),
                        glib::markup_escape_text(&format!("{e:#}"))
                    ));
                    log::error!("{}: could not save: {e:#}", vm.name);
                }
            }
        });
    }

    dialog.present();
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
