//! The detail pane: what a VM is, and what you can do to it.
//!
//! Shared state lives in `Rc<RefCell<..>>` clones that the button closures capture.
//! That is the idiomatic shape for GTK4 Rust: the widgets own the closures, and
//! the closures own clones of the state, so nothing points back at the view.

use crate::i18n;
use anyhow::Context;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use librevm_core::config::{BootConfig, ConsoleConfig, HypervisorConfig, Protection, VmConfig};
use librevm_core::store::Store;
use librevm_core::vm::{self, State};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

/// State the action buttons share.
#[derive(Clone)]
struct Shared {
    store: Store,
    current: Rc<RefCell<Option<String>>>,
    on_changed: Rc<RefCell<Option<Rc<dyn Fn()>>>>,
    /// Called when the operator asks for the console, so the window can switch
    /// its stack to the console pane.
    on_console: Rc<RefCell<Option<Rc<dyn Fn()>>>>,
}

pub struct VmView {
    root: gtk4::Stack,
    title: gtk4::Label,
    subtitle: gtk4::Label,
    state_pill: gtk4::Label,
    m_hypervisor: gtk4::Label,
    m_memory: gtk4::Label,
    m_guest: gtk4::Label,
    m_cpus: gtk4::Label,
    m_kernel: gtk4::Label,
    m_console: gtk4::Label,
    start_button: gtk4::Button,
    stop_button: gtk4::Button,
    shared: Shared,
}

impl VmView {
    pub fn new(store: Store) -> Self {
        let stack = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .build();

        let empty = adw::StatusPage::builder()
            .icon_name("computer-symbolic")
            .title(i18n::tr(i18n::Key::NothingSelectedTitle))
            .description(i18n::tr(i18n::Key::NothingSelectedBody))
            .build();
        stack.add_named(&empty, Some("empty"));

        let title = gtk4::Label::builder().xalign(0.0).css_classes(["title-1"]).build();
        let subtitle = gtk4::Label::builder().xalign(0.0).css_classes(["dim-label"]).build();
        let state_pill = gtk4::Label::builder().css_classes(["state-pill", "state-stopped"]).build();

        let heading_text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        heading_text.set_hexpand(true);
        heading_text.append(&title);
        heading_text.append(&subtitle);

        let heading = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        heading.append(&heading_text);
        heading.append(&state_pill);

        let grid = gtk4::Grid::builder()
            .row_spacing(10)
            .column_spacing(24)
            .margin_top(20)
            .build();

        let mut labels = Vec::new();
        for key in [
            i18n::Key::MetricHypervisor,
            i18n::Key::MetricMemory,
            i18n::Key::MetricGuestMemory,
            i18n::Key::MetricCpus,
            i18n::Key::MetricKernel,
            i18n::Key::MetricConsole,
        ] {
            let l = gtk4::Label::builder()
                .label(i18n::tr(key))
                .xalign(0.0)
                .css_classes(["metric-label"])
                .build();
            let v = gtk4::Label::builder()
                .label("-")
                .xalign(0.0)
                .selectable(true)
                .wrap(true)
                .css_classes(["metric-value"])
                .build();
            grid.attach(&l, 0, labels.len() as i32, 1, 1);
            grid.attach(&v, 1, labels.len() as i32, 1, 1);
            labels.push(v);
        }
        let [m_hypervisor, m_memory, m_guest, m_cpus, m_kernel, m_console] =
            <[gtk4::Label; 6]>::try_from(labels).expect("six metric labels");

        let start_button = gtk4::Button::builder()
            .label(i18n::tr(i18n::Key::Start))
            .icon_name("media-playback-start-symbolic")
            .css_classes(["suggested-action", "pill"])
            .build();
        let stop_button = gtk4::Button::builder()
            .label(i18n::tr(i18n::Key::Stop))
            .icon_name("media-playback-stop-symbolic")
            .css_classes(["pill"])
            .build();
        let plan_button = gtk4::Button::builder()
            .label(i18n::tr(i18n::Key::ShowCommand))
            .icon_name("utilities-terminal-symbolic")
            .css_classes(["pill"])
            .build();
        // A prominent way into the console. The header-bar icon is easy to miss,
        // and connecting to the guest is the whole point.
        let console_button = gtk4::Button::builder()
            .label(i18n::tr(i18n::Key::Console))
            .icon_name("utilities-terminal-symbolic")
            .tooltip_text(i18n::tr(i18n::Key::ShowConsole))
            .css_classes(["pill"])
            .build();
        let delete_button = gtk4::Button::builder()
            .label(i18n::tr(i18n::Key::Delete))
            .icon_name("user-trash-symbolic")
            .css_classes(["destructive-action", "pill"])
            .build();

        let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        actions.set_margin_top(26);
        actions.append(&start_button);
        actions.append(&stop_button);
        actions.append(&console_button);
        actions.append(&plan_button);
        let actions_spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        actions_spacer.set_hexpand(true);
        actions.append(&actions_spacer);
        actions.append(&delete_button);

        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        content.set_margin_top(20);
        content.set_margin_bottom(20);
        content.set_margin_start(20);
        content.set_margin_end(20);
        content.append(&heading);
        content.append(&grid);
        content.append(&actions);

        let clamp = adw::Clamp::builder().maximum_size(760).child(&content).build();
        let scrolled = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();

        stack.add_named(&scrolled, Some("detail"));
        stack.set_visible_child_name("empty");

        let shared = Shared {
            store,
            current: Rc::new(RefCell::new(None)),
            on_changed: Rc::new(RefCell::new(None)),
            on_console: Rc::new(RefCell::new(None)),
        };

        let view = Self {
            root: stack,
            title,
            subtitle,
            state_pill,
            m_hypervisor,
            m_memory,
            m_guest,
            m_cpus,
            m_kernel,
            m_console,
            start_button,
            stop_button,
            shared,
        };

        view.wire_actions(plan_button, delete_button, console_button);
        view
    }

    pub fn widget(&self) -> gtk4::Widget {
        self.root.clone().upcast()
    }

    pub fn on_changed<F: Fn() + 'static>(&self, f: F) {
        *self.shared.on_changed.borrow_mut() = Some(Rc::new(f));
    }

    /// Called when the operator asks to see the console.
    pub fn on_console<F: Fn() + 'static>(&self, f: F) {
        *self.shared.on_console.borrow_mut() = Some(Rc::new(f));
    }

    fn request_console(&self) {
        if let Some(f) = self.shared.on_console.borrow().as_ref() {
            f();
        }
    }

    fn notify_changed(&self) {
        if let Some(f) = self.shared.on_changed.borrow().as_ref() {
            f();
        }
    }

    pub fn clear(&self) {
        *self.shared.current.borrow_mut() = None;
        self.root.set_visible_child_name("empty");
    }

    pub fn show(&self, name: &str) {
        let store = &self.shared.store;
        let Ok(vm) = store.load(name) else {
            self.clear();
            return;
        };
        let Ok(summary) = vm::summary(store, name) else {
            self.clear();
            return;
        };

        *self.shared.current.borrow_mut() = Some(name.to_string());

        self.title.set_label(&vm.name);
        self.subtitle
            .set_label(vm.description.as_deref().unwrap_or("no description"));

        self.m_hypervisor.set_label(&vm.hypervisor.backend.to_string());
        self.m_memory.set_label(&format!("{} MiB", vm.memory_mib));
        self.m_guest.set_label(&format!("{} MiB", vm.guest_memory_mib()));
        self.m_cpus.set_label(&vm.cpus.to_string());
        self.m_kernel.set_label(&vm.boot.kernel.display().to_string());
        self.m_console
            .set_label(&store.console_socket(name).display().to_string());

        self.update_state(&summary.state);
        self.root.set_visible_child_name("detail");
    }

    pub fn update_state(&self, state: &State) {
        let (word, class) = match state {
            State::Running { .. } => ("running", "state-running"),
            State::Stopped => ("stopped", "state-stopped"),
            State::Stale { .. } => ("stale", "state-stale"),
        };
        self.state_pill.set_label(word);
        self.state_pill.set_css_classes(&["state-pill", class]);

        let running = matches!(state, State::Running { .. });
        self.start_button.set_sensitive(!running);
        self.stop_button.set_sensitive(running);
    }

    fn wire_actions(
        &self,
        plan_button: gtk4::Button,
        delete_button: gtk4::Button,
        console_button: gtk4::Button,
    ) {
        let window_of = {
            let root = self.root.clone();
            move || -> Option<gtk4::Window> {
                root.root().and_then(|r| r.downcast::<gtk4::Window>().ok())
            }
        };

        // Start
        {
            let shared = self.shared.clone();
            let notify = {
                let view_state = self.shared.clone();
                move || {
                    if let Some(f) = view_state.on_changed.borrow().as_ref() {
                        f();
                    }
                }
            };
            let window_of = window_of.clone();
            self.start_button.connect_clicked(move |_| {
                let Some(name) = shared.current.borrow().clone() else { return };

                // A machine is very often already running -- the window was simply
                // closed and reopened. That is not an error: go to the console.
                let already = match vm::state(&shared.store, &name) {
                    vm::State::Running { .. } => true,
                    vm::State::Stale { pid } => {
                        log::warn!("{name}: clearing stale pid {pid}");
                        let _ = std::fs::remove_file(shared.store.pid_file(&name));
                        false
                    }
                    vm::State::Stopped => false,
                };
                if already {
                    log::info!("{name} was already running; showing its console");
                    if let Some(f) = shared.on_console.borrow().as_ref() {
                        f();
                    }
                    notify();
                    return;
                }

                // Start under a hub: crosvm's console is a client of a socket that
                // has to exist first, and this process cannot hold it.
                match std::env::current_exe()
                    .context("finding our own path")
                    .and_then(|exe| librevm_core::hub::spawn(&shared.store, &name, &exe))
                {
                    Ok(pid) => {
                        log::info!("{name} started (pid {pid})");
                        // Starting a machine is almost always followed by wanting
                        // to use it.
                        if let Some(f) = shared.on_console.borrow().as_ref() {
                            f();
                        }
                    }
                    Err(e) => alert(
                        window_of(),
                        i18n::tr(i18n::Key::CouldNotStart),
                        &format!("{e:#}"),
                    ),
                }
                notify();
            });
        }

        // Console
        {
            let shared = self.shared.clone();
            console_button.connect_clicked(move |_| {
                if let Some(f) = shared.on_console.borrow().as_ref() {
                    f();
                }
            });
        }

        // Stop
        {
            let shared = self.shared.clone();
            let notify = {
                let view_state = self.shared.clone();
                move || {
                    if let Some(f) = view_state.on_changed.borrow().as_ref() {
                        f();
                    }
                }
            };
            let window_of = window_of.clone();
            self.stop_button.connect_clicked(move |_| {
                let Some(name) = shared.current.borrow().clone() else { return };
                if let Err(e) = vm::stop(&shared.store, &name, 5000) {
                    alert(
                        window_of(),
                        i18n::tr(i18n::Key::CouldNotStop),
                        &format!("{e:#}"),
                    );
                }
                notify();
            });
        }

        // Show the crosvm command line
        {
            let shared = self.shared.clone();
            let window_of = window_of.clone();
            plan_button.connect_clicked(move |_| {
                let Some(name) = shared.current.borrow().clone() else { return };
                let text = match shared
                    .store
                    .load(&name)
                    .and_then(|vm| vm::plan(&shared.store, &vm, true))
                {
                    Ok(plan) => {
                        let mut s = format!("# {}\n\n", plan.hypervisor);
                        s.push_str(&plan.argv.join(" \\\n  "));
                        s
                    }
                    Err(e) => {
                        format!("{}:\n{e:#}", i18n::tr(i18n::Key::CouldNotBuildCommand))
                    }
                };
                show_text(
                    window_of(),
                    &format!("{name}: {}", i18n::tr(i18n::Key::CrosvmCommand)),
                    &text,
                );
            });
        }

        // Delete
        {
            let shared = self.shared.clone();
            let notify = {
                let view_state = self.shared.clone();
                move || {
                    if let Some(f) = view_state.on_changed.borrow().as_ref() {
                        f();
                    }
                }
            };
            let window_of = window_of.clone();
            delete_button.connect_clicked(move |_| {
                let Some(name) = shared.current.borrow().clone() else { return };
                let Some(window) = window_of() else { return };

                let d = adw::MessageDialog::builder()
                    .transient_for(&window)
                    .heading(i18n::tr_delete_question(&name))
                    .body(i18n::tr_delete_body())
                    .build();
                d.add_response("cancel", i18n::tr(i18n::Key::Cancel));
                d.add_response("delete", i18n::tr(i18n::Key::Delete));
                d.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
                d.set_default_response(Some("cancel"));
                d.set_close_response("cancel");

                let store = shared.store.clone();
                let name = name.clone();
                let notify = notify.clone();
                d.connect_response(None, move |_, resp| {
                    if resp == "delete" {
                        match store.remove(&name) {
                            Ok(()) => log::info!("deleted {name}"),
                            Err(e) => log::error!("could not delete {name}: {e:#}"),
                        }
                        notify();
                    }
                });
                d.present();
            });
        }
    }
}

fn alert(parent: Option<gtk4::Window>, heading: &str, body: &str) {
    log::error!("{heading}: {body}");
    let Some(window) = parent else { return };
    let d = adw::MessageDialog::builder()
        .transient_for(&window)
        .heading(heading)
        .body(body)
        .build();
    d.add_response("ok", "OK");
    d.set_default_response(Some("ok"));
    d.present();
}

pub(crate) fn show_text(parent: Option<gtk4::Window>, heading: &str, body: &str) {
    let Some(window) = parent else { return };
    let label = gtk4::Label::builder()
        .label(body)
        .xalign(0.0)
        .yalign(0.0)
        .selectable(true)
        .wrap(true)
        .css_classes(["console-view"])
        .build();
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Automatic)
        .min_content_height(320)
        .min_content_width(640)
        .child(&label)
        .build();
    let d = adw::MessageDialog::builder()
        .transient_for(&window)
        .heading(heading)
        .extra_child(&scroll)
        .build();
    d.add_response("close", "Close");
    d.set_default_response(Some("close"));
    d.present();
}

/// The create dialog.
pub fn present_create_dialog<F: Fn() + 'static>(
    parent: &adw::ApplicationWindow,
    store: &Store,
    on_created: F,
) {
    let dialog = adw::Dialog::builder()
        .title(i18n::tr(i18n::Key::NewVm))
        .content_width(540)
        .content_height(640)
        .build();

    let name_entry = adw::EntryRow::builder()
        .title(i18n::tr(i18n::Key::Name))
        .build();
    let kernel_entry = adw::EntryRow::builder()
        .title(i18n::tr(i18n::Key::KernelImage))
        .text("/var/lib/librevm/kernels/mini-Image")
        .build();

    let memory_row = adw::SpinRow::with_range(16.0, 4096.0, 16.0);
    memory_row.set_title(i18n::tr(i18n::Key::MemoryMib));
    memory_row.set_value(128.0);

    let swiotlb_row = adw::SpinRow::with_range(0.0, 1024.0, 16.0);
    swiotlb_row.set_title(i18n::tr(i18n::Key::SwiotlbMib));
    swiotlb_row.set_value(64.0);

    let cpus_row = adw::SpinRow::with_range(1.0, 32.0, 1.0);
    cpus_row.set_title(i18n::tr(i18n::Key::Vcpus));
    cpus_row.set_value(1.0);

    let cmdline_entry = adw::EntryRow::builder()
        .title(i18n::tr(i18n::Key::KernelCmdline))
        .text("console=ttyS0 earlycon loglevel=7")
        .build();

    let protection = adw::ComboRow::builder()
        .title(i18n::tr(i18n::Key::MemoryProtection))
        .subtitle(i18n::tr(i18n::Key::ProtectionHint))
        .model(&gtk4::StringList::new(&match i18n::current() {
            i18n::Lang::En => [
                "lend, no firmware (recommended)",
                "shared (may be refused)",
                "lend, with firmware",
            ],
            i18n::Lang::Zh => [
                "lend，无固件（推荐）",
                "shared（可能被拒绝）",
                "lend，带固件",
            ],
        }))
        .selected(0)
        .build();

    // Live arithmetic: guest RAM is memory - swiotlb, which surprises people.
    let hint = gtk4::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();
    let recompute = {
        let hint = hint.clone();
        let memory_row = memory_row.clone();
        let swiotlb_row = swiotlb_row.clone();
        move || {
            let mem = memory_row.value() as u64;
            let sw = swiotlb_row.value() as u64;
            if sw >= mem {
                hint.set_markup(match i18n::current() {
                    i18n::Lang::En => {
                        "<b>swiotlb must be smaller than memory.</b>\n\
                         crosvm computes guest RAM as memory - swiotlb and panics when \
                         that underflows."
                    }
                    i18n::Lang::Zh => {
                        "<b>swiotlb 必须小于内存。</b>\n\
                         crosvm 用「内存 − swiotlb」算客户机 RAM，\n\
                         减出负数时会直接崩溃。"
                    }
                });
            } else {
                hint.set_label(&i18n::tr_create_hint(mem, sw, mem - sw));
            }
        }
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

    let identity = adw::PreferencesGroup::builder()
        .title(i18n::tr(i18n::Key::GroupIdentity))
        .build();
    identity.add(&name_entry);
    identity.add(&kernel_entry);

    let resources = adw::PreferencesGroup::builder()
        .title(i18n::tr(i18n::Key::GroupResources))
        .build();
    resources.add(&memory_row);
    resources.add(&swiotlb_row);
    resources.add(&cpus_row);

    let advanced = adw::PreferencesGroup::builder()
        .title(i18n::tr(i18n::Key::GroupAdvanced))
        .build();
    advanced.add(&protection);
    advanced.add(&cmdline_entry);

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.append(&identity);
    content.append(&resources);
    content.append(&hint);
    content.append(&advanced);

    let clamp = adw::Clamp::builder().maximum_size(560).child(&content).build();
    let scrolled = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();

    let create_button = gtk4::Button::builder()
        .label(i18n::tr(i18n::Key::Create))
        .css_classes(["suggested-action", "pill"])
        .build();

    let header = adw::HeaderBar::builder().build();
    header.pack_end(&create_button);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scrolled));
    dialog.set_child(Some(&toolbar));

    {
        let dialog = dialog.clone();
        // an adw::Dialog is not a gtk4::Window, so nested dialogs attach to the
        // application window instead
        let parent = parent.clone();
        let store = store.clone();
        let on_created = Rc::new(on_created);
        create_button.connect_clicked(move |_| {
            let name = name_entry.text().to_string();
            let protection = match protection.selected() {
                0 => Protection::WithoutFirmware,
                1 => Protection::None,
                _ => Protection::WithFirmware,
            };

            let vm = VmConfig {
                name: name.clone(),
                cpus: cpus_row.value() as u32,
                memory_mib: memory_row.value() as u64,
                hypervisor: HypervisorConfig {
                    protection,
                    swiotlb_mib: swiotlb_row.value() as u64,
                    ..Default::default()
                },
                boot: BootConfig {
                    kernel: PathBuf::from(kernel_entry.text().to_string()),
                    initramfs: None,
                    cmdline: cmdline_entry.text().to_string(),
                },
                console: ConsoleConfig { socket: None, log: None },
                description: None,
            };

            match vm.validate().and_then(|_| store.save(&vm)) {
                Ok(()) => {
                    log::info!("{name} created");
                    on_created();
                    dialog.close();
                }
                Err(e) => {
                    let d = adw::MessageDialog::builder()
                        .transient_for(&parent)
                        .heading(i18n::tr(i18n::Key::CouldNotCreate))
                        .body(format!("{e:#}"))
                        .build();
                    d.add_response("ok", "OK");
                    d.set_default_response(Some("ok"));
                    d.present();
                }
            }
        });
    }

    dialog.present(Some(parent));
}
