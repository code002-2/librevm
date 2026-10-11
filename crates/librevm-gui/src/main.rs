//! librevm-gui: a GTK4 front end for librevm.
//!
//! It drives the same core crate the CLI does, so a VM created here is the same
//! TOML file and the same crosvm command line. Nothing is duplicated except the
//! presentation.

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

mod console;
mod i18n;
mod vm_view;
mod window;

const APP_ID: &str = "org.librevm.Gui";

fn main() -> glib::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // The saved choice wins, then the locale, then English.
    let lang = i18n::initial();
    i18n::set_current(lang);
    log::info!("interface language: {}", lang.code());

    // A normal, single-instance application.
    //
    // This is deliberate, and it is what makes the launcher icon work every time.
    // The shell's flow is: look up the id on the session bus; if something owns it,
    // send an activate request; otherwise start the process. Activate is wired to
    // open a window, so clicking the icon while a window is already open raises a
    // new one rather than starting a second copy.
    //
    // Making this NON_UNIQUE breaks that flow: nothing owns the id, so the shell has
    // no one to activate, and while it still believes the application is running --
    // which it decides from the window's app id, and which lags the window closing
    // by a moment -- a second click does nothing at all.
    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_startup(|_| {
        load_css();
        librevm_core::log_startup("librevm-gui");
    });

    // Activate opens a window. When the shell sends an activate request to an
    // already-running instance this is what responds, so clicking the icon while
    // the application is up gives the operator a window instead of nothing.
    app.connect_activate(window::build);

    app.run()
}

/// Load the small stylesheet that gives the state pills their colours.
fn load_css() {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(
        r#"
        .state-running  { background: alpha(@success_color, 0.18); color: @success_color; }
        .state-stopped  { background: alpha(@window_fg_color, 0.08); }
        .state-stale    { background: alpha(@warning_color, 0.18); color: @warning_color; }
        .state-pill     { border-radius: 999px; padding: 1px 10px; font-size: 0.85em; font-weight: bold; }
        .vm-title       { font-weight: bold; }
        .vm-subtitle    { font-size: 0.85em; opacity: 0.65; }
        .console-view   { font-family: monospace; font-size: 0.9em; }
        .metric-value   { font-weight: bold; }
        .metric-label   { font-size: 0.85em; opacity: 0.65; }
        "#,
    );
    gtk4::style_context_add_provider_for_display(
        &gtk4::gdk::Display::default().expect("a display to attach the stylesheet to"),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
