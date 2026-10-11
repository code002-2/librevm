//! The privileged half of librevm.
//!
//! The store lives under `/var/lib/librevm`, which belongs to root, and starting a
//! machine needs `/dev/gunyah`. Without a daemon the window has to be launched through
//! sudo, which means the whole GUI runs as root: a GTK program, its toolkit, its
//! renderers and its image decoders, all privileged, all parsing files an operator
//! chose. That is a large amount of code to trust with root for the sake of writing a
//! TOML file and calling `kill`.
//!
//! So the privileged work moves here, behind a small, auditable interface on the system
//! bus, and the window goes back to running as the person who opened it.
//!
//! Every method returns a JSON string rather than a D-Bus struct. The interface is
//! young and the shape of a machine summary will change; a JSON document lets both
//! sides move without the wire format having to be versioned in lockstep. It also keeps
//! this file free of a parallel set of types.

use anyhow::{bail, Context, Result};
use librevm_core::config::{DiskConfig, SharedDirConfig, VmConfig};
use librevm_core::disk;
use librevm_core::hub;
use librevm_core::store::Store;
use librevm_core::vm::{self, State};
use serde::Serialize;
use zbus::interface;

/// The bus name. The policy file grants ownership of exactly this name.
const BUS_NAME: &str = "org.librevm.Daemon1";
/// Where the daemon serves the bus.
const OBJECT_PATH: &str = "/org/librevm/Daemon1";

/// A machine, flattened for the wire.
#[derive(Serialize)]
struct MachineView {
    name: String,
    state: String,
    pid: Option<i32>,
    running: bool,
    hypervisor: String,
    memory_mib: u64,
    guest_memory_mib: u64,
    cpus: u32,
    kernel: String,
    disks: Vec<DiskView>,
    shares: Vec<ShareView>,
}

#[derive(Serialize)]
struct DiskView {
    path: String,
    format: String,
    capacity: u64,
    capacity_human: String,
    allocated: u64,
    allocated_human: String,
    readonly: bool,
    root: bool,
    label: Option<String>,
    /// Whether the image file could be read at all. A definition can name a path that
    /// has since been deleted, and the window should say so rather than fail to draw.
    readable: bool,
}

#[derive(Serialize)]
struct ShareView {
    path: String,
    tag: String,
    readonly: bool,
}

/// The object the bus exposes.
struct Daemon {
    store: Store,
}

impl Daemon {
    /// Check a name before it reaches the filesystem.
    ///
    /// `Store::vm_dir` joins the name onto a path, so a name containing a separator or
    /// a `..` would let a caller reach outside the store. The CLI validates names when
    /// it writes a definition, but the daemon is reachable by anything in the `librevm`
    /// group and has to make its own check before touching a path.
    fn check_name(name: &str) -> Result<()> {
        if name.is_empty() || name.len() > 64 {
            bail!("a machine name must be 1 to 64 characters");
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            bail!("a machine name may only contain letters, digits, '-', '_' and '.'");
        }
        if name == "." || name == ".." {
            bail!("{name:?} is not a usable machine name");
        }
        Ok(())
    }

    fn view(&self, name: &str) -> Result<MachineView> {
        let vm = self.store.load(name)?;
        let state = vm::state(&self.store, name);
        let (state_word, pid) = match state {
            State::Running { pid } => ("running", Some(pid)),
            State::Stopped => ("stopped", None),
            State::Stale { pid } => ("stale", Some(pid)),
        };

        let disks = vm
            .disks
            .iter()
            .map(|d| {
                let info = disk::open(&d.path).ok();
                DiskView {
                    path: d.path.display().to_string(),
                    format: info
                        .as_ref()
                        .map(|i| i.format.name().to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                    capacity: info.as_ref().map(|i| i.capacity).unwrap_or(0),
                    capacity_human: info
                        .as_ref()
                        .map(|i| disk::human(i.capacity))
                        .unwrap_or_else(|| "-".to_string()),
                    allocated: info.as_ref().map(|i| i.allocated).unwrap_or(0),
                    allocated_human: info
                        .as_ref()
                        .map(|i| disk::human(i.allocated))
                        .unwrap_or_else(|| "-".to_string()),
                    readonly: d.readonly,
                    root: d.root,
                    label: d.label.clone(),
                    readable: info.is_some(),
                }
            })
            .collect();

        let shares = vm
            .shared_dirs
            .iter()
            .map(|s| ShareView {
                path: s.path.display().to_string(),
                tag: s.tag.clone(),
                readonly: s.readonly,
            })
            .collect();

        Ok(MachineView {
            name: vm.name.clone(),
            state: state_word.to_string(),
            pid,
            running: state != State::Stopped,
            hypervisor: vm.hypervisor.backend.to_string(),
            memory_mib: vm.memory_mib,
            guest_memory_mib: vm.guest_memory_mib(),
            cpus: vm.cpus,
            kernel: vm.boot.kernel.display().to_string(),
            disks,
            shares,
        })
    }

    fn load_checked(&self, name: &str) -> Result<VmConfig> {
        Self::check_name(name)?;
        self.store.load(name)
    }
}

#[interface(name = "org.librevm.Daemon1")]
impl Daemon {
    /// Every machine in the store, as a JSON array.
    async fn list_machines(&self) -> String {
        let names = match self.store.list() {
            Ok(n) => n,
            Err(e) => return error_json(&e),
        };
        let views: Vec<MachineView> = names.iter().filter_map(|n| self.view(n).ok()).collect();
        to_json(&views)
    }

    /// One machine, or an error object.
    async fn machine(&self, name: String) -> String {
        match self.view(&name) {
            Ok(v) => to_json(&v),
            Err(e) => error_json(&e),
        }
    }

    /// What this host offers: hypervisors, crosvm, Gunyah readiness.
    async fn host_info(&self) -> String {
        to_json(&vm::host_info())
    }

    /// The crosvm command line a machine would run, without running it.
    async fn plan(&self, name: String) -> String {
        match self
            .load_checked(&name)
            .and_then(|vm| vm::plan(&self.store, &vm, true))
        {
            Ok(p) => to_json(&serde_json::json!({ "ok": true, "plan": p })),
            Err(e) => error_json(&e),
        }
    }

    /// Start a machine, detached, with its console hub.
    async fn start_machine(&self, name: String) -> String {
        let result = (|| -> Result<String> {
            let vm = self.load_checked(&name)?;
            vm.validate()?;

            // A machine already running is not an error worth shouting about; the
            // window asks to start because the operator clicked, and the answer they
            // want is "it is running", not a stack trace.
            if let State::Running { pid } = vm::state(&self.store, &name) {
                return Ok(format!("already running (pid {pid})"));
            }

            let exe = std::env::current_exe().context("finding my own path")?;
            hub::spawn(&self.store, &name, &exe)?;
            Ok(format!("started {name}"))
        })();
        match result {
            Ok(m) => to_json(&serde_json::json!({ "ok": true, "message": m })),
            Err(e) => error_json(&e),
        }
    }

    /// Stop a machine, politely first.
    async fn stop_machine(&self, name: String, grace_secs: u32) -> String {
        let result = (|| -> Result<String> {
            Self::check_name(&name)?;
            // The core works in milliseconds; the bus speaks in seconds because that is
            // what an operator thinks in.
            vm::stop(&self.store, &name, (grace_secs.max(1) as u64) * 1000)?;
            Ok(format!("stopped {name}"))
        })();
        match result {
            Ok(m) => to_json(&serde_json::json!({ "ok": true, "message": m })),
            Err(e) => error_json(&e),
        }
    }

    /// Create a machine from a TOML definition, which is what the CLI writes too.
    ///
    /// Taking the definition rather than a list of fields keeps both front ends from
    /// having to agree on a wire shape for something that already has a file format.
    async fn create_machine(&self, definition: String) -> String {
        let result = (|| -> Result<String> {
            let vm = VmConfig::from_toml(&definition)?;
            Self::check_name(&vm.name)?;
            if self.store.exists(&vm.name) {
                bail!("{} already exists", vm.name);
            }
            vm.validate()?;

            // Fill in the console paths the way the CLI does, so a machine created
            // here is indistinguishable from one created there.
            let mut vm = vm;
            if vm.console.socket.is_none() {
                vm.console.socket = Some(self.store.console_socket(&vm.name));
            }
            if vm.console.log.is_none() {
                vm.console.log = Some(self.store.console_log(&vm.name));
            }
            self.store.save(&vm)?;
            Ok(format!("created {}", vm.name))
        })();
        match result {
            Ok(m) => to_json(&serde_json::json!({ "ok": true, "message": m })),
            Err(e) => error_json(&e),
        }
    }

    /// Replace an existing machine's definition.
    ///
    /// The window edits definitions as the desktop user, and the store belongs to root, so
    /// this is the path the settings editor uses. It also puts the host side of the guest
    /// network up or down to match, which needs root for the same reason.
    async fn save_machine(&self, definition: String) -> String {
        let result = (|| -> Result<String> {
            let mut vm = VmConfig::from_toml(&definition)?;
            Self::check_name(&vm.name)?;
            if !self.store.exists(&vm.name) {
                bail!("{} does not exist", vm.name);
            }

            // The console paths belong to the store, not to whatever the caller sent.
            let existing = self.store.load(&vm.name)?;
            vm.console = existing.console.clone();
            vm.validate()?;

            let was_enabled = existing.network.enabled;
            let now_enabled = vm.network.enabled;

            // A machine reads its definition once, at start, so nothing here has to stop
            // it; the caller is told to restart instead.
            if now_enabled {
                librevm_core::net::setup(vm.network.index)?;
            } else if was_enabled {
                librevm_core::net::teardown().ok();
            }

            self.store.save(&vm)?;
            Ok(format!("saved {}", vm.name))
        })();
        match result {
            Ok(m) => to_json(&serde_json::json!({ "ok": true, "message": m })),
            Err(e) => error_json(&e),
        }
    }

    /// Bring the host side of the guest network up, and say what the guest should use.
    async fn net_setup(&self, index: u8) -> String {
        match librevm_core::net::setup(index) {
            Ok(guest) => to_json(&serde_json::json!({
                "ok": true,
                "interface": guest.interface,
                "address": guest.address.to_string(),
                "prefix": guest.prefix,
                "gateway": guest.gateway.to_string(),
                "dns": guest.dns.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
                "script": guest.script(),
            })),
            Err(e) => error_json(&e),
        }
    }

    /// Take the host side down.
    async fn net_teardown(&self) -> String {
        match librevm_core::net::teardown() {
            Ok(()) => to_json(&serde_json::json!({ "ok": true, "message": "the tap is gone" })),
            Err(e) => error_json(&e),
        }
    }

    /// Whether the tap is there, and what it is addressed.
    async fn net_status(&self) -> String {
        let up = librevm_core::net::is_up();
        let read = |name: &str| {
            std::fs::read_to_string(format!("/sys/class/net/{}/{name}", librevm_core::net::TAP_NAME))
                .map(|s| s.trim().to_string())
                .ok()
        };
        to_json(&serde_json::json!({
            "ok": true,
            "tap": librevm_core::net::TAP_NAME,
            "present": up,
            "mac": read("address"),
            "mtu": read("mtu"),
        }))
    }

    /// The images that can be fetched, with whichever mirror is in force.
    async fn image_catalogue(&self) -> String {
        let chosen = librevm_core::image::chosen_mirror();
        let entries: Vec<serde_json::Value> = librevm_core::image::CATALOGUE
            .iter()
            .map(|e| {
                let effective = match &chosen {
                    Some(m) => librevm_core::image::with_mirror(e, m),
                    None => e.clone(),
                };
                serde_json::json!({
                    "id": e.id,
                    "description": e.description,
                    "url": effective.url,
                    "original_url": e.url,
                    "arch": e.arch,
                    "download_mib": e.download_mib,
                    "disk_mib": e.disk_mib,
                    "mirrored": effective.url != e.url,
                    "cached": librevm_core::image::cache_path(&self.store.images_dir(), &effective).is_file(),
                })
            })
            .collect();
        to_json(&serde_json::json!({
            "ok": true,
            "mirror": chosen,
            "mirrors": librevm_core::image::MIRRORS
                .iter()
                .map(|(n, u)| serde_json::json!({ "name": n, "url": u }))
                .collect::<Vec<_>>(),
            "images": entries,
        }))
    }

    /// Download an image and turn it into a disk, optionally attaching it to a machine.
    ///
    /// This is the long one. It runs on the bus in a blocking way, so a large download
    /// makes the caller wait; the GUI shows its own progress state and treats the call as
    /// the unit of work.
    async fn deploy_image(
        &self,
        id: String,
        name: String,
        size_mib: u64,
        attach_to: String,
        as_root: bool,
    ) -> String {
        let result = (|| -> Result<serde_json::Value> {
            let entry = librevm_core::image::find(&id)
                .with_context(|| format!("no image called {id}"))?;
            let effective = match librevm_core::image::chosen_mirror() {
                Some(m) => librevm_core::image::with_mirror(entry, &m),
                None => entry.clone(),
            };

            // The key has to exist before the image is built, because it goes into it.
            let key = librevm_core::ssh::ensure_keypair(self.store.root())?;
            let dns = librevm_core::image::host_nameservers();

            let deployed = librevm_core::image::deploy(
                &self.store.images_dir(),
                &effective,
                &name,
                if size_mib > 0 { Some(size_mib) } else { None },
                &librevm_core::image::Injections {
                    authorized_key: Some(key),
                    ssh_user: Some(librevm_core::ssh::USER.to_string()),
                    dns,
                    root_password: None,
                },
                |_, _| {},
            )?;

            let mut attached = false;
            if !attach_to.is_empty() && attach_to != "-" {
                let mut vm = self.store.load(&attach_to)?;
                vm.disks.push(librevm_core::config::DiskConfig {
                    path: deployed.path.clone(),
                    readonly: false,
                    root: as_root,
                    label: Some(name.clone()),
                });
                vm.validate()?;
                self.store.save(&vm)?;
                attached = true;
            }

            Ok(serde_json::json!({
                "ok": true,
                "path": deployed.path.display().to_string(),
                "size_mib": deployed.size_mib,
                "notes": deployed.notes,
                "attached": attached,
                "machine": if attached { attach_to } else { String::new() },
            }))
        })();
        match result {
            Ok(v) => to_json(&v),
            Err(e) => error_json(&e),
        }
    }

    /// Install an SSH key and the first-boot sshd setup into an existing disk image.
    ///
    /// This is the one-click part: an image that was made elsewhere, or one deployed
    /// before the key existed, gets the same treatment without being rebuilt.
    async fn deploy_ssh(&self, image: String, user: String) -> String {
        let result = (|| -> Result<serde_json::Value> {
            let path = std::path::PathBuf::from(&image);
            if !path.is_file() {
                bail!("{} is not a file", path.display());
            }
            let user = if user.is_empty() {
                librevm_core::ssh::USER.to_string()
            } else {
                user
            };
            let key = librevm_core::ssh::ensure_keypair(self.store.root())?;
            let dns = librevm_core::image::host_nameservers();

            let injections = librevm_core::image::Injections {
                authorized_key: Some(key),
                ssh_user: Some(user.clone()),
                dns,
                root_password: None,
            };

            let notes = librevm_core::image::inject_into_image(&path, &injections)?;
            Ok(serde_json::json!({
                "ok": true,
                "image": path.display().to_string(),
                "user": user,
                "notes": notes,
            }))
        })();
        match result {
            Ok(v) => to_json(&v),
            Err(e) => error_json(&e),
        }
    }

    /// What has been downloaded, and what is deployed.
    async fn image_status(&self) -> String {
        let dir = librevm_core::image::cache_dir(&self.store.images_dir());
        let mut cached = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if let Ok(m) = e.metadata() {
                    if m.is_file() {
                        cached.push(serde_json::json!({
                            "name": e.file_name().to_string_lossy(),
                            "bytes": m.len(),
                        }));
                    }
                }
            }
        }

        // Disks in the images directory, with whether they look like a root filesystem.
        let mut images = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.store.images_dir()) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("img") {
                    continue;
                }
                let meta = e.metadata().ok();
                images.push(serde_json::json!({
                    "name": e.file_name().to_string_lossy(),
                    "path": p.display().to_string(),
                    "bytes": meta.as_ref().map(|m| m.len()).unwrap_or(0),
                    "on_disk": meta.as_ref().map(|m| {
                        #[cfg(unix)]
                        { use std::os::unix::fs::MetadataExt; m.blocks() * 512 }
                        #[cfg(not(unix))]
                        { m.len() }
                    }).unwrap_or(0),
                }));
            }
        }
        images.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

        to_json(&serde_json::json!({
            "ok": true,
            "cached": cached,
            "images": images,
            "ssh_key": librevm_core::ssh::exists(self.store.root()),
        }))
    }

    /// Grow or shrink a disk file.
    async fn resize_disk(&self, path: String, size_mib: u64) -> String {
        let result = (|| -> Result<serde_json::Value> {
            let p = std::path::PathBuf::from(&path);
            if !p.is_file() {
                bail!("{} is not a file", p.display());
            }
            // Growing is safe; shrinking needs the filesystem inside to have been shrunk
            // first, which is the caller's business, so it is allowed here.
            librevm_core::disk::resize(&p, size_mib * 1024 * 1024, true)?;
            let len = std::fs::metadata(&p)?.len();
            Ok(serde_json::json!({
                "ok": true,
                "path": p.display().to_string(),
                "size_mib": len / (1024 * 1024),
            }))
        })();
        match result {
            Ok(v) => to_json(&v),
            Err(e) => error_json(&e),
        }
    }

    /// Put a filesystem on a disk file, replacing whatever was there.
    async fn format_disk(&self, path: String, filesystem: String) -> String {
        let result = (|| -> Result<serde_json::Value> {
            let p = std::path::PathBuf::from(&path);
            if !p.is_file() {
                bail!("{} is not a file", p.display());
            }
            let fs = if filesystem.is_empty() {
                "ext4".to_string()
            } else {
                filesystem
            };
            librevm_core::disk::make_filesystem(&p, &fs)?;
            Ok(serde_json::json!({
                "ok": true,
                "path": p.display().to_string(),
                "filesystem": fs,
            }))
        })();
        match result {
            Ok(v) => to_json(&v),
            Err(e) => error_json(&e),
        }
    }

    /// Remove a disk file, refusing if a machine still refers to it.
    async fn delete_disk(&self, path: String) -> String {
        let result = (|| -> Result<serde_json::Value> {
            let p = std::path::PathBuf::from(&path);
            if !p.is_file() {
                bail!("{} is not a file", p.display());
            }
            // A machine with this disk attached would fail to start afterwards, so say
            // which one rather than leaving that to be discovered later.
            let mut users = Vec::new();
            for name in self.store.list()? {
                if let Ok(vm) = self.store.load(&name) {
                    if vm.disks.iter().any(|d| d.path == p) {
                        users.push(name);
                    }
                }
            }
            if !users.is_empty() {
                bail!("still attached to {}", users.join(", "));
            }
            std::fs::remove_file(&p)?;
            Ok(serde_json::json!({
                "ok": true,
                "path": p.display().to_string(),
            }))
        })();
        match result {
            Ok(v) => to_json(&v),
            Err(e) => error_json(&e),
        }
    }

    /// Which machines have a given disk attached.
    async fn disk_users(&self, path: String) -> String {
        let p = std::path::PathBuf::from(&path);
        let mut users = Vec::new();
        for name in self.store.list().unwrap_or_default() {
            if let Ok(vm) = self.store.load(&name) {
                if vm.disks.iter().any(|d| d.path == p) {
                    users.push(serde_json::json!({
                        "name": name,
                        "root": vm.disks.iter().any(|d| d.path == p && d.root),
                    }));
                }
            }
        }
        to_json(&serde_json::json!({ "ok": true, "users": users }))
    }

    /// Everything the images page needs about one disk in a single call.
    ///
    /// The page shows a disk's size, its filesystem and who has it attached; asking three
    /// times for one row would be three round trips.
    async fn disk_info(&self, path: String) -> String {
        let p = std::path::PathBuf::from(&path);
        if !p.is_file() {
            return error_json(&anyhow::anyhow!("{} is not a file", p.display()));
        }
        let bytes = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);

        // The filesystem, from blkid, which reads the superblock without mounting.
        let fs = std::process::Command::new("blkid")
            .args(["-o", "value", "-s", "TYPE", &p.display().to_string()])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty());

        let mut users = Vec::new();
        for name in self.store.list().unwrap_or_default() {
            if let Ok(vm) = self.store.load(&name) {
                if let Some(d) = vm.disks.iter().find(|d| d.path == p) {
                    users.push(serde_json::json!({ "name": name, "root": d.root }));
                }
            }
        }

        to_json(&serde_json::json!({
            "ok": true,
            "path": p.display().to_string(),
            "bytes": bytes,
            "filesystem": fs,
            "users": users,
        }))
    }

    /// Remove a machine and everything it owns.
    async fn delete_machine(&self, name: String) -> String {
        let result = (|| -> Result<String> {
            Self::check_name(&name)?;
            if !self.store.exists(&name) {
                bail!("{name} does not exist");
            }
            // Stopping first means a deleted machine cannot leave a hub behind holding
            // a socket for a definition that no longer exists.
            if vm::state(&self.store, &name) != State::Stopped {
                vm::stop(&self.store, &name, 5000).ok();
            }
            self.store.remove(&name)?;
            Ok(format!("deleted {name}"))
        })();
        match result {
            Ok(m) => to_json(&serde_json::json!({ "ok": true, "message": m })),
            Err(e) => error_json(&e),
        }
    }

    /// The last `max_bytes` of a machine's console log, as raw bytes.
    ///
    /// Bytes rather than a string because the console carries terminal output, which is
    /// not required to be valid UTF-8 part way through a multi-byte character.
    async fn console_tail(&self, name: String, max_bytes: u32) -> Vec<u8> {
        if Self::check_name(&name).is_err() {
            return Vec::new();
        }
        let path = self.store.console_log(&name);
        let Ok(data) = std::fs::read(&path) else {
            return Vec::new();
        };
        let limit = (max_bytes as usize).clamp(1024, 8 * 1024 * 1024);
        if data.len() <= limit {
            return data;
        }
        data[data.len() - limit..].to_vec()
    }

    /// Attach a disk image.
    async fn attach_disk(
        &self,
        name: String,
        path: String,
        readonly: bool,
        root: bool,
        label: String,
    ) -> String {
        let result = (|| -> Result<String> {
            let mut vm = self.load_checked(&name)?;
            let path = std::path::PathBuf::from(path);
            if !path.is_file() {
                bail!("{} does not exist", path.display());
            }
            if vm.disks.iter().any(|d| d.path == path) {
                bail!("{} is already attached", path.display());
            }
            if root && vm.disks.iter().any(|d| d.root) {
                bail!("{name} already has a root disk");
            }
            if root && readonly {
                bail!("the root disk cannot be read-only");
            }
            vm.disks.push(DiskConfig {
                path: path.clone(),
                readonly,
                root,
                label: if label.is_empty() { None } else { Some(label) },
            });
            vm.validate()?;
            self.store.save(&vm)?;
            Ok(format!("attached {}", path.display()))
        })();
        match result {
            Ok(m) => to_json(&serde_json::json!({ "ok": true, "message": m })),
            Err(e) => error_json(&e),
        }
    }

    /// Detach a disk by path.
    async fn detach_disk(&self, name: String, path: String) -> String {
        let result = (|| -> Result<String> {
            let mut vm = self.load_checked(&name)?;
            let wanted = std::path::PathBuf::from(&path);
            let Some(index) = vm.disks.iter().position(|d| d.path == wanted) else {
                bail!("{name} has no disk {path}");
            };
            let removed = vm.disks.remove(index);
            self.store.save(&vm)?;
            Ok(format!("detached {}", removed.path.display()))
        })();
        match result {
            Ok(m) => to_json(&serde_json::json!({ "ok": true, "message": m })),
            Err(e) => error_json(&e),
        }
    }

    /// Share a host directory with a machine.
    async fn add_share(
        &self,
        name: String,
        path: String,
        tag: String,
        readonly: bool,
    ) -> String {
        let result = (|| -> Result<String> {
            let mut vm = self.load_checked(&name)?;
            let dir = std::path::PathBuf::from(&path);
            if !dir.is_dir() {
                bail!("{} is not a directory", dir.display());
            }
            let tag = if tag.is_empty() {
                dir.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("share")
                    .to_string()
            } else {
                tag
            };
            if vm.shared_dirs.iter().any(|s| s.tag == tag) {
                bail!("{name} already shares something tagged {tag:?}");
            }
            vm.shared_dirs.push(SharedDirConfig {
                path: dir.clone(),
                tag: tag.clone(),
                readonly,
            });
            vm.validate()?;
            self.store.save(&vm)?;
            Ok(format!("sharing {} as {tag}", dir.display()))
        })();
        match result {
            Ok(m) => to_json(&serde_json::json!({ "ok": true, "message": m })),
            Err(e) => error_json(&e),
        }
    }

    /// Stop sharing a tagged directory.
    async fn remove_share(&self, name: String, tag: String) -> String {
        let result = (|| -> Result<String> {
            let mut vm = self.load_checked(&name)?;
            let Some(index) = vm
                .shared_dirs
                .iter()
                .position(|s| s.tag == tag || s.path.display().to_string() == tag)
            else {
                bail!("{name} shares nothing tagged {tag:?}");
            };
            vm.shared_dirs.remove(index);
            self.store.save(&vm)?;
            Ok(format!("stopped sharing {tag:?}"))
        })();
        match result {
            Ok(m) => to_json(&serde_json::json!({ "ok": true, "message": m })),
            Err(e) => error_json(&e),
        }
    }

    /// Make a disk image, and optionally attach it.
    async fn create_disk(
        &self,
        path: String,
        size: String,
        format: String,
        attach_to: String,
    ) -> String {
        let result = (|| -> Result<String> {
            let path = std::path::PathBuf::from(path);
            let size = disk::parse_size(&size)?;
            let format = disk::Format::parse(&format)?;
            let made = disk::create(&path, size, format)?;

            if !attach_to.is_empty() {
                let mut vm = self.load_checked(&attach_to)?;
                if !vm.disks.iter().any(|d| d.path == path) {
                    vm.disks.push(DiskConfig {
                        path: path.clone(),
                        readonly: false,
                        root: false,
                        label: None,
                    });
                    vm.validate()?;
                    self.store.save(&vm)?;
                }
            }

            Ok(format!(
                "{} ({}, {} on disk)",
                made.path.display(),
                disk::human(made.capacity),
                disk::human(made.allocated)
            ))
        })();
        match result {
            Ok(m) => to_json(&serde_json::json!({ "ok": true, "message": m })),
            Err(e) => error_json(&e),
        }
    }

    /// Disk images in the shared images directory.
    async fn list_disks(&self) -> String {
        let dir = self.store.images_dir();
        match disk::list(&dir) {
            Ok(images) => {
                let views: Vec<serde_json::Value> = images
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "path": d.path.display().to_string(),
                            "name": d.path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                            "format": d.format.name(),
                            "capacity": d.capacity,
                            "capacity_human": disk::human(d.capacity),
                            "allocated": d.allocated,
                            "allocated_human": disk::human(d.allocated),
                        })
                    })
                    .collect();
                to_json(&views)
            }
            Err(e) => error_json(&e),
        }
    }

    /// The version, so a client can tell what it is talking to.
    #[zbus(property)]
    fn version(&self) -> String {
        librevm_core::VERSION.to_string()
    }
}

fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| {
        format!(r#"{{"error":"could not serialise the answer: {e}"}}"#)
    })
}

fn error_json(e: &anyhow::Error) -> String {
    serde_json::json!({ "error": format!("{e:#}") }).to_string()
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("librevm_daemon=info,warn"),
    )
    .init();

    // The daemon is the privileged side; running it as anyone else would silently
    // build a store the operator cannot see. Refusing early is kinder than a confusing
    // permission error later.
    if unsafe { libc::geteuid() } != 0 {
        bail!("the daemon manages a root-owned store and must run as root");
    }

    let store = Store::from_env();
    store.create_dirs().context("preparing the store")?;

    let daemon = Daemon { store };
    let _connection = zbus::connection::Builder::system()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, daemon)?
        .build()
        .await
        .context("taking the bus name")?;

    log::info!("serving {BUS_NAME} at {OBJECT_PATH}");

    // Nothing else to do: the connection serves in the background. Wait for a signal so
    // the process stays alive and shuts down cleanly under systemd.
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => log::info!("interrupted"),
        _ = term.recv() => log::info!("asked to stop"),
    }

    Ok(())
}
