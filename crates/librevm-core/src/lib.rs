//! librevm-core: the parts of librevm that do not care how they are driven.
//!
//! The CLI and the daemon both sit on this. It knows how to describe a VM, how to
//! find a workable hypervisor, how to turn a definition into a crosvm command
//! line, and how to start, inspect and stop one.

pub mod backend;
pub mod config;
pub mod disk;
pub mod hub;
pub mod store;
pub mod vm;

pub use config::{
    BootConfig, ConsoleConfig, Hypervisor, HypervisorConfig, Protection, VmConfig,
};
pub use store::Store;
pub use vm::{State, Summary};

/// The version of the crate, for `--version` and the daemon's D-Bus property.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Log a short banner when something starts up.
pub fn log_startup(program: &str) {
    log::info!("{program} {VERSION}");
    let info = vm::host_info();
    if info.under_hypervisor {
        log::info!("this kernel is running under a hypervisor");
    }
    for h in &info.hypervisors {
        log::info!("hypervisor available: {h}");
    }
    match &info.crosvm {
        Some(p) => log::info!("crosvm: {}", p.display()),
        None => log::warn!(
            "crosvm not found: {}",
            info.crosvm_error.as_deref().unwrap_or("unknown reason")
        ),
    }
}
