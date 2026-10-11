//! The main window: a sidebar of machines and a detail pane with a console.

use crate::console::ConsoleView;
use crate::i18n;
use crate::vm_view::VmView;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use librevm_core::store::Store;
use librevm_core::vm::{self, State};
use std::cell::RefCell;
use std::rc::Rc;

pub fn build(app: &adw::Application) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(i18n::tr(i18n::Key::WindowTitle))
        .default_width(1040)
        .default_height(700)
        .build();

    fill(&window);

    window.present();

    // If no window ever appears, this is the line that says whether the window was
    // built at all -- without it, a missing window is indistinguishable from a
    // failure earlier in startup.
    log::info!(
        "window built and presented (display={:?}, backend={:?})",
        std::env::var("WAYLAND_DISPLAY").or_else(|_| std::env::var("DISPLAY")),
        gtk4::gdk::Display::default().map(|d| d.type_().name().to_string()),
    );

    check_gunyah(&window);
}

/// Offer to install Gunyah when it is not there.
///
/// The device already boots under Qualcomm's hypervisor, so without these modules
/// nothing can be started at all. Finding that out only when the Start button fails
/// is a poor introduction, so the check happens as soon as the window appears.
fn check_gunyah(window: &adw::ApplicationWindow) {
    if librevm_core::backend::gunyah_ready() {
        log::info!("Gunyah is available");
        return;
    }

    log::warn!("Gunyah is not available; offering to install it");

    let installer = librevm_core::backend::gunyah_installer();
    log::info!(
        "installer: {}",
        installer
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "none found".to_string())
    );

    let dialog = adw::MessageDialog::builder()
        .transient_for(window)
        .heading(i18n::tr(i18n::Key::GunyahMissingTitle))
        .body(match installer {
            Some(_) => i18n::tr(i18n::Key::GunyahMissingBody).to_string(),
            None => match i18n::current() {
                i18n::Lang::En => {
                    "This machine boots under Qualcomm's hypervisor, but the modules \
                     that talk to its resource manager are not loaded, so no virtual \
                     machine can start.\n\n\
                     No installer was found on this machine either. Install librevm \
                     with scripts/install.sh, or copy install-gunyah onto the system."
                        .to_string()
                }
                i18n::Lang::Zh => {
                    "这台设备运行在 Qualcomm 的 hypervisor 之下，但和它的资源管理器\
                     通信所需的内核模块没有加载，因此无法启动任何虚拟机。\n\n\
                     这台机器上也没有找到安装脚本。请用 scripts/install.sh 安装 \
                     librevm，或者把 install-gunyah 复制到系统里。"
                        .to_string()
                }
            },
        })
        .build();

    dialog.add_response("later", i18n::tr(i18n::Key::Later));
    dialog.add_response("recheck", i18n::tr(i18n::Key::Recheck));
    if installer.is_some() {
        dialog.add_response("install", i18n::tr(i18n::Key::InstallNow));
        dialog.set_response_appearance("install", adw::ResponseAppearance::Suggested);
    }
    dialog.set_default_response(Some("recheck"));
    dialog.set_close_response("later");

    let window_for_result = window.clone();
    dialog.connect_response(None, move |d, response| {
        d.close();
        match response {
            "install" => {
                if let Some(path) = librevm_core::backend::gunyah_installer() {
                    run_installer(&window_for_result, &path);
                }
            }
            "recheck" => {
                if librevm_core::backend::gunyah_ready() {
                    let ok = adw::MessageDialog::builder()
                        .transient_for(&window_for_result)
                        .heading(i18n::tr(i18n::Key::GunyahNowAvailable))
                        .build();
                    ok.add_response("ok", i18n::tr(i18n::Key::Later));
                    ok.present();
                } else {
                    check_gunyah(&window_for_result);
                }
            }
            _ => {}
        }
    });

    dialog.present();
}

/// Run the installer and show what it said.
///
/// It has to be root to write /lib/modules, and this window is already root, so it is
/// run directly. The output is captured rather than shown live: it is the last lines
/// that matter, and the alternative is a terminal the operator did not ask for.
fn run_installer(window: &adw::ApplicationWindow, installer: &std::path::Path) {
    log::info!("running {}", installer.display());

    let output = std::process::Command::new(installer)
        .arg("--install")
        .env("QUIET", "no")
        .output();

    let text = match output {
        Ok(out) => {
            let mut s = String::new();
            s.push_str(&format!("$ {} --install\n\n", installer.display()));
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stdout.trim().is_empty() {
                s.push_str(&stdout);
            }
            if !stderr.trim().is_empty() {
                if !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str("--- stderr ---\n");
                s.push_str(&stderr);
            }
            s.push_str(&format!("\nexit status: {}\n", out.status));
            s
        }
        Err(e) => format!(
            "could not run {}:\n{e:#}\n\n\
             The GUI runs as root, so this is not a permission problem. Check that the \
             file is executable.\n",
            installer.display()
        ),
    };

    log::info!("installer finished:\n{text}");

    let ready = librevm_core::backend::gunyah_ready();
    let heading = if ready {
        i18n::tr(i18n::Key::GunyahNowAvailable)
    } else {
        i18n::tr(i18n::Key::GunyahStillMissing)
    };

    let result = adw::MessageDialog::builder()
        .transient_for(window)
        .heading(heading)
        .build();
    result.add_response("close", i18n::tr(i18n::Key::Later));
    result.add_response("details", i18n::tr(i18n::Key::ShowDetails));
    if !ready {
        result.add_response("recheck", i18n::tr(i18n::Key::Recheck));
    }
    result.set_default_response(Some("close"));
    result.set_close_response("close");

    let window_for_text = window.clone();
    let text_for_dialog = text.clone();
    let installer_for_retry = installer.to_path_buf();
    result.connect_response(None, move |d, response| {
        d.close();
        match response {
            "details" => crate::vm_view::show_text(
                Some(window_for_text.clone().upcast()),
                i18n::tr(i18n::Key::GunyahInstalling),
                &text_for_dialog,
            ),
            "recheck" => {
                if librevm_core::backend::gunyah_ready() {
                    let ok = adw::MessageDialog::builder()
                        .transient_for(&window_for_text)
                        .heading(i18n::tr(i18n::Key::GunyahNowAvailable))
                        .build();
                    ok.add_response("ok", i18n::tr(i18n::Key::Later));
                    ok.present();
                } else {
                    run_installer(&window_for_text, &installer_for_retry);
                }
            }
            _ => {}
        }
    });

    result.present();
}

/// Draw the window's contents again, in whatever language is now selected.
///
/// Replacing the content rather than opening a new window keeps the application
/// identity intact, which matters because the launcher icon's behaviour depends on
/// it. `Store` is rebuilt from the environment so nothing has to be threaded
/// through the signal handlers.
fn rebuild_in_place(window: &adw::ApplicationWindow) {
    fill(window);
}

fn fill(window: &adw::ApplicationWindow) {
    let store = Store::from_env();

    // ---- sidebar --------------------------------------------------------
    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::Single)
        .css_classes(["navigation-sidebar"])
        .build();

    // A second list under the machines, for the pages that are not about one machine.
    // A header toggle was the first attempt and it was too easy to miss.
    let pages = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::Single)
        .css_classes(["navigation-sidebar"])
        .build();
    let disks_row = gtk4::ListBoxRow::builder()
        .child(
            &adw::ActionRow::builder()
                .title(i18n::tr(i18n::Key::DisksPage))
                .subtitle(i18n::tr(i18n::Key::DisksPageHint))
                .build(),
        )
        .build();
    pages.append(&disks_row);

    let sidebar_body = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    sidebar_body.append(&list);
    sidebar_body.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    sidebar_body.append(&pages);

    let sidebar_scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&sidebar_body)
        .build();

    let empty = adw::StatusPage::builder()
        .icon_name("computer-symbolic")
        .title(i18n::tr(i18n::Key::NoMachinesTitle))
        .description(i18n::tr(i18n::Key::NoMachinesBody))
        .build();

    let sidebar_stack = gtk4::Stack::builder().build();
    sidebar_stack.add_named(&sidebar_scroll, Some("list"));
    sidebar_stack.add_named(&empty, Some("empty"));

    let new_button = gtk4::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text(i18n::tr(i18n::Key::CreateVm))
        .css_classes(["flat"])
        .build();
    let refresh_button = gtk4::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text(i18n::tr(i18n::Key::Refresh))
        .css_classes(["flat"])
        .build();

    // The language menu. Switching rebuilds the window rather than rebinding every
    // label: there are few enough strings that a fresh window is simpler, and it
    // cannot leave a stale one behind.
    let language_button = gtk4::MenuButton::builder()
        .icon_name("preferences-desktop-locale-symbolic")
        .tooltip_text(i18n::tr(i18n::Key::Language))
        .css_classes(["flat"])
        .build();

    {
        let boxed = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        boxed.set_margin_top(6);
        boxed.set_margin_bottom(6);
        boxed.set_margin_start(6);
        boxed.set_margin_end(6);

        let heading = gtk4::Label::builder()
            .label(i18n::tr(i18n::Key::Language))
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .margin_start(6)
            .margin_bottom(4)
            .build();
        boxed.append(&heading);

        let current = i18n::current();
        for lang in i18n::Lang::all() {
            let row = gtk4::Button::builder()
                .css_classes(["flat"])
                .build();
            let inner = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            let tick = gtk4::Image::from_icon_name(if lang == current {
                "object-select-symbolic"
            } else {
                "window-close-symbolic"
            });
            if lang != current {
                // keep the column aligned without showing a mark
                tick.set_opacity(0.0);
            }
            inner.append(&tick);
            inner.append(&gtk4::Label::new(Some(lang.endonym())));
            row.set_child(Some(&inner));

            let window_for_switch = window.clone();
            row.connect_clicked(move |btn| {
                if lang == i18n::current() {
                    if let Some(p) = btn
                        .ancestor(gtk4::Popover::static_type())
                        .and_then(|w| w.downcast::<gtk4::Popover>().ok())
                    {
                        p.popdown();
                    }
                    return;
                }
                log::info!("switching language to {}", lang.code());
                i18n::set_current(lang);
                i18n::save(lang);
                if let Some(p) = btn
                    .ancestor(gtk4::Popover::static_type())
                    .and_then(|w| w.downcast::<gtk4::Popover>().ok())
                {
                    p.popdown();
                }
                rebuild_in_place(&window_for_switch);
            });
            boxed.append(&row);
        }

        let popover = gtk4::Popover::new();
        popover.set_child(Some(&boxed));
        language_button.set_popover(Some(&popover));
    }

    let sidebar_header = adw::HeaderBar::builder()
        .title_widget(&adw::WindowTitle::new(
            i18n::tr(i18n::Key::Machines),
            i18n::tr(i18n::Key::WindowTitle),
        ))
        .build();
    sidebar_header.pack_start(&refresh_button);
    sidebar_header.pack_end(&new_button);
    sidebar_header.pack_end(&language_button);

    let sidebar_toolbar = adw::ToolbarView::new();
    sidebar_toolbar.add_top_bar(&sidebar_header);
    sidebar_toolbar.set_content(Some(&sidebar_stack));

    let sidebar_page = adw::NavigationPage::builder()
        .title(i18n::tr(i18n::Key::Machines))
        .child(&sidebar_toolbar)
        .build();

    // ---- detail ---------------------------------------------------------
    let view = Rc::new(VmView::new(store.clone()));
    let console = Rc::new(ConsoleView::new(store.clone()));
    // The images page needs the daemon: deploying is a download and a filesystem, which is
    // not something the window should be doing with its own privileges.
    let images = Rc::new(crate::images::ImagesPage::new(
        store.clone(),
        crate::daemon::Bridge::new(),
    ));

    let detail_stack = gtk4::Stack::builder().vexpand(true).build();
    detail_stack.add_named(&view.widget(), Some("detail"));
    detail_stack.add_named(&console.widget(), Some("console"));
    detail_stack.add_named(&images.root, Some("images"));
    detail_stack.set_visible_child_name("detail");

    let console_toggle = gtk4::ToggleButton::builder()
        .icon_name("utilities-terminal-symbolic")
        .tooltip_text(i18n::tr(i18n::Key::ShowConsole))
        .css_classes(["flat"])
        .build();

    // Shared with the callbacks below, which drive this toggle and are driven by it.
    let switching = Rc::new(std::cell::Cell::new(false));

    let detail_header = adw::HeaderBar::builder().build();
    detail_header.pack_end(&console_toggle);

    // Selecting a machine in the sidebar leaves the disks page.
    //
    // Both of these used to reach into the other list and clear it, and to flip the other
    // toggle. That is a closed loop -- clearing a selection emits `row-selected`, and
    // changing a toggle's active state from inside a handler for that same toggle trips a
    // GTK assertion about its accounting -- and it took the window down when the disks
    // entry was clicked. Driving only the stack and leaving each list to its own handler
    // breaks the loop: a selection in one list does not need the other one cleared, because
    // the stack can only show one page anyway.
    {
        let detail_stack = detail_stack.clone();
        let images = images.clone();
        list.connect_row_selected(move |_, row| {
            if row.is_some() && detail_stack.visible_child_name().as_deref() == Some("images") {
                detail_stack.set_visible_child_name("detail");
            }
            // The page is reloaded when it is shown, not when a machine is picked.
            let _ = &images;
        });
    }

    {
        let detail_stack = detail_stack.clone();
        let images = images.clone();
        pages.connect_row_selected(move |_, row| {
            if row.is_some() {
                detail_stack.set_visible_child_name("images");
                images.reload();
            }
        });
    }

    let detail_toolbar = adw::ToolbarView::new();
    detail_toolbar.add_top_bar(&detail_header);
    detail_toolbar.set_content(Some(&detail_stack));

    let detail_page = adw::NavigationPage::builder()
        .title(i18n::tr(i18n::Key::Details))
        .child(&detail_toolbar)
        .build();

    let split = adw::NavigationSplitView::builder()
        .sidebar(&sidebar_page)
        .content(&detail_page)
        .build();
    split.set_collapsed(false);

    window.set_content(Some(&split));

    // ---- shared bits the callbacks need ---------------------------------
    let selected: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    // Rebuild the sidebar from the store.
    let repopulate = {
        let store = store.clone();
        let list = list.clone();
        let sidebar_stack = sidebar_stack.clone();
        let selected = selected.clone();
        let view = view.clone();
        let console = console.clone();
        Rc::new(move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }

            let all = vm::list(&store).unwrap_or_default();
            sidebar_stack.set_visible_child_name(if all.is_empty() { "empty" } else { "list" });

            for summary in &all {
                list.append(&build_row(summary));
            }

            // keep the selection if it still exists, otherwise take the first
            let keep = selected.borrow().clone();
            let index = keep
                .as_ref()
                .and_then(|name| all.iter().position(|s| &s.name == name))
                .or(if all.is_empty() { None } else { Some(0) });

            match index {
                Some(i) => {
                    if let Some(row) = list.row_at_index(i as i32) {
                        list.select_row(Some(&row));
                    }
                    let name = all[i].name.clone();
                    selected.replace(Some(name.clone()));
                    view.show(&name);
                    console.follow(&name);
                }
                None => {
                    selected.replace(None);
                    view.clear();
                    console.clear();
                }
            }
        })
    };

    // ---- wiring ---------------------------------------------------------
    {
        let store = store.clone();
        let selected = selected.clone();
        let view = view.clone();
        let console = console.clone();
        let split = split.clone();
        list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let all = vm::list(&store).unwrap_or_default();
            let Some(summary) = all.get(row.index() as usize) else { return };
            selected.replace(Some(summary.name.clone()));
            view.show(&summary.name);
            console.follow(&summary.name);
            split.set_show_content(true);
        });
    }

    {
        let detail_stack = detail_stack.clone();
        let switching = switching.clone();
        console_toggle.connect_toggled(move |b| {
            // Set by the callback above while it is driving this toggle, so that the round
            // trip through here does not look like a fresh user action.
            if switching.get() {
                return;
            }
            detail_stack.set_visible_child_name(if b.is_active() { "console" } else { "detail" });
        });
    }

    // The detail pane's Console button switches this same stack, so both ways in land on
    // the console view. Guarded against re-entry: `set_active` runs the toggled handler
    // above, which sets the stack again, and GTK objects if a toggle's state is changed
    // while it is already handling a change of that state.
    {
        let detail_stack = detail_stack.clone();
        let console_toggle = console_toggle.clone();
        let switching = Rc::new(std::cell::Cell::new(false));
        view.on_console(move || {
            if switching.get() {
                return;
            }
            switching.set(true);
            detail_stack.set_visible_child_name("console");
            console_toggle.set_active(true);
            switching.set(false);
        });
    }

    {
        let store = store.clone();
        let repopulate = repopulate.clone();
        view.on_changed(move || {
            let _ = &store;
            repopulate();
        });
    }

    {
        let repopulate = repopulate.clone();
        refresh_button.connect_clicked(move |_| repopulate());
    }

    {
        let window = window.clone();
        let store = store.clone();
        let repopulate = repopulate.clone();
        new_button.connect_clicked(move |_| {
            crate::vm_view::present_create_dialog(&window, &store, {
                let repopulate = repopulate.clone();
                move || repopulate()
            });
        });
    }

    repopulate();

    // Poll for state changes and console output. A signal-based design would be
    // tidier, but the guest's output arrives on a socket the CLI may also be
    // attached to, so the log file is the one reliable channel.
    {
        let store = store.clone();
        let list = list.clone();
        let selected = selected.clone();
        let view = view.clone();
        let console = console.clone();
        glib::timeout_add_seconds_local(1, move || {
            let all = vm::list(&store).unwrap_or_default();

            let mut child = list.first_child();
            let mut i = 0usize;
            while let Some(row) = child {
                if let Some(list_row) = row.downcast_ref::<gtk4::ListBoxRow>() {
                    if let Some(summary) = all.get(i) {
                        update_row_state(list_row, &summary.state);
                    }
                }
                child = row.next_sibling();
                i += 1;
            }

            if let Some(name) = selected.borrow().clone() {
                if let Ok(s) = vm::summary(&store, &name) {
                    view.update_state(&s.state);
                }
                console.refresh_if_running(&name);
            }

            glib::ControlFlow::Continue
        });
    }
}

/// One row in the sidebar: name, a state pill, and a one-line summary.
fn build_row(summary: &vm::Summary) -> gtk4::ListBoxRow {
    let name = gtk4::Label::builder()
        .label(&summary.name)
        .xalign(0.0)
        .css_classes(["vm-title"])
        .build();

    let pill = gtk4::Label::builder()
        .label(state_word(&summary.state))
        .css_classes(["state-pill", state_class(&summary.state)])
        .build();

    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);

    let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    top.append(&name);
    top.append(&spacer);
    top.append(&pill);

    let sub = gtk4::Label::builder()
        .label(match i18n::current() {
            i18n::Lang::En => format!(
                "{} - {} MiB to the guest, {} vCPU{}",
                summary.hypervisor,
                summary.guest_memory_mib,
                summary.cpus,
                if summary.cpus == 1 { "" } else { "s" }
            ),
            i18n::Lang::Zh => format!(
                "{} — 客户机 {} MiB，{} 个虚拟 CPU",
                summary.hypervisor, summary.guest_memory_mib, summary.cpus
            ),
        })
        .xalign(0.0)
        .css_classes(["vm-subtitle"])
        .build();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    content.append(&top);
    content.append(&sub);

    let row = gtk4::ListBoxRow::builder().activatable(true).build();
    row.set_child(Some(&content));
    row
}

/// Find the state pill inside a row and update it.
fn update_row_state(row: &gtk4::ListBoxRow, state: &State) {
    let Some(content) = row.child().and_then(|c| c.downcast::<gtk4::Box>().ok()) else {
        return;
    };
    let Some(top) = content.first_child().and_then(|c| c.downcast::<gtk4::Box>().ok()) else {
        return;
    };

    let mut child = top.first_child();
    while let Some(w) = child {
        if let Some(label) = w.downcast_ref::<gtk4::Label>() {
            if label.css_classes().iter().any(|c| c == "state-pill") {
                label.set_label(state_word(state));
                label.set_css_classes(&["state-pill", state_class(state)]);
                break;
            }
        }
        child = w.next_sibling();
    }
}

fn state_word(state: &State) -> &'static str {
    match state {
        State::Running { .. } => i18n::tr(i18n::Key::StateRunning),
        State::Stopped => i18n::tr(i18n::Key::StateStopped),
        State::Stale { .. } => i18n::tr(i18n::Key::StateStale),
    }
}

fn state_class(state: &State) -> &'static str {
    match state {
        State::Running { .. } => "state-running",
        State::Stopped => "state-stopped",
        State::Stale { .. } => "state-stale",
    }
}
