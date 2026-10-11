//! Where VMs live on disk.
//!
//! One directory per VM, holding its definition and whatever runtime state it
//! accumulates. Keeping it this plain means a VM can be inspected, copied or
//! deleted with ordinary file tools.

use crate::config::VmConfig;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// The default store, used when nothing overrides it.
pub const DEFAULT_ROOT: &str = "/var/lib/librevm";

/// The default place for runtime files: pid files, console sockets.
pub const DEFAULT_RUN: &str = "/run/librevm";

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    run: PathBuf,
}

impl Default for Store {
    fn default() -> Self {
        Self::new(DEFAULT_ROOT, DEFAULT_RUN)
    }
}

impl Store {
    pub fn new(root: impl Into<PathBuf>, run: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), run: run.into() }
    }

    /// Honour `LIBREVM_ROOT` and `LIBREVM_RUN` so tests and unprivileged runs do
    /// not need the real system paths.
    pub fn from_env() -> Self {
        let root = std::env::var("LIBREVM_ROOT").unwrap_or_else(|_| DEFAULT_ROOT.to_string());
        let run = std::env::var("LIBREVM_RUN").unwrap_or_else(|_| DEFAULT_RUN.to_string());
        Self::new(root, run)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn run_dir(&self) -> &Path {
        &self.run
    }

    /// The directory holding a VM's files.
    pub fn vm_dir(&self, name: &str) -> PathBuf {
        self.root.join("vms").join(name)
    }

    /// The VM's definition file.
    pub fn config_path(&self, name: &str) -> PathBuf {
        self.vm_dir(name).join("vm.toml")
    }

    /// Where a VM's console output is kept.
    pub fn console_log(&self, name: &str) -> PathBuf {
        self.vm_dir(name).join("console.log")
    }

    /// The Unix socket a running VM's console is served on.
    pub fn console_socket(&self, name: &str) -> PathBuf {
        self.run.join(format!("{name}.console.sock"))
    }

    /// The pid file for a running VM.
    pub fn pid_file(&self, name: &str) -> PathBuf {
        self.run.join(format!("{name}.pid"))
    }

    /// Where the console hub keeps its own log.
    pub fn hub_log(&self, name: &str) -> PathBuf {
        self.vm_dir(name).join("hub.log")
    }

    pub fn disks_dir(&self, name: &str) -> PathBuf {
        self.vm_dir(name).join("disks")
    }

    /// Where images shared between machines live.
    ///
    /// Per-machine disks go in `disks_dir`; this is the place for images that
    /// outlive any one definition, and it is what `disk list` defaults to.
    pub fn images_dir(&self) -> PathBuf {
        self.root.join("images")
    }

    pub fn create_dirs(&self) -> Result<()> {
        for dir in [
            self.root.join("vms"),
            self.root.join("kernels"),
            self.root.join("images"),
            self.run.clone(),
        ] {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }

    /// Every VM name in the store, sorted.
    pub fn list(&self) -> Result<Vec<String>> {
        let dir = self.root.join("vms");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("listing {}", dir.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.path().join("vm.toml").is_file() {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn exists(&self, name: &str) -> bool {
        self.config_path(name).is_file()
    }

    pub fn load(&self, name: &str) -> Result<VmConfig> {
        let path = self.config_path(name);
        if !path.is_file() {
            bail!("no vm named {name:?} (looked for {})", path.display());
        }
        VmConfig::load(&path)
    }

    pub fn save(&self, vm: &VmConfig) -> Result<()> {
        self.create_dirs()?;
        let dir = self.vm_dir(&vm.name);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        std::fs::create_dir_all(self.disks_dir(&vm.name))?;
        vm.save(&self.config_path(&vm.name))
    }

    /// Remove a VM and everything under it.
    pub fn remove(&self, name: &str) -> Result<()> {
        let dir = self.vm_dir(name);
        if !dir.exists() {
            bail!("no vm named {name:?}");
        }
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("removing {}", dir.display()))?;
        let _ = std::fs::remove_file(self.console_socket(name));
        let _ = std::fs::remove_file(self.pid_file(name));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_derived_from_the_name() {
        let s = Store::new("/var/lib/librevm", "/run/librevm");
        assert_eq!(
            s.config_path("debian"),
            PathBuf::from("/var/lib/librevm/vms/debian/vm.toml")
        );
        assert_eq!(
            s.console_socket("debian"),
            PathBuf::from("/run/librevm/debian.console.sock")
        );
        assert_eq!(s.pid_file("debian"), PathBuf::from("/run/librevm/debian.pid"));
    }

    #[test]
    fn list_is_empty_when_the_store_does_not_exist() {
        let dir = std::env::temp_dir().join("librevm-nonexistent-store-test");
        let _ = std::fs::remove_dir_all(&dir);
        let s = Store::new(&dir, "/tmp");
        assert!(s.list().unwrap().is_empty());
    }
}
