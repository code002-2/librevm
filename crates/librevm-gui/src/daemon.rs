//! A client for the daemon.
//!
//! The window runs as whoever opened it. Everything that touches the store or starts a
//! machine belongs to root, so those calls go over the system bus to `librevm-daemon`
//! instead of being done here.
//!
//! Blocking rather than async on purpose: GTK is a single-threaded, callback-shaped
//! toolkit, and these calls are short. A `blocking::Connection` keeps the client free of
//! an executor and keeps the call sites looking like ordinary function calls.
//!
//! Falling back is a real path, not a formality. Running the window as root is still
//! allowed, and so is running it on a machine where the daemon is not installed; in
//! both cases the direct calls in `librevm-core` work and are used instead.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use zbus::blocking::Connection;

const BUS_NAME: &str = "org.librevm.Daemon1";
const OBJECT_PATH: &str = "/org/librevm/Daemon1";
const INTERFACE: &str = "org.librevm.Daemon1";

/// A connection to the daemon, if there is one and we are allowed to use it.
pub struct Client {
    conn: Connection,
}

impl Client {
    /// Connect to the system bus and check that the daemon is actually there.
    ///
    /// Returns `None` rather than an error for every reason a client might not be
    /// usable: no system bus, no daemon, or a policy that refuses us. In each case the
    /// caller falls back to doing the work itself, so a failure here is not something
    /// the operator needs to see.
    pub fn connect() -> Option<Client> {
        let conn = Connection::system().ok()?;

        // Asking for the name's owner is the cheapest liveness check: if nobody owns
        // it, the proxy would fail on the first call with a less helpful message.
        let owned: bool = conn
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "NameHasOwner",
                &(BUS_NAME),
            )
            .ok()
            .and_then(|reply| reply.body().deserialize::<bool>().ok())
            .unwrap_or(false);

        if !owned {
            log::debug!("the daemon is not on the bus");
            return None;
        }
        Some(Client { conn })
    }

    fn call<B>(&self, method: &str, args: &B) -> Result<Value>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let reply = self
            .conn
            .call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), method, args)
            .with_context(|| format!("calling {method}"))?;

        let text: String = reply
            .body()
            .deserialize()
            .with_context(|| format!("{method} did not answer with a string"))?;

        let value: Value =
            serde_json::from_str(&text).with_context(|| format!("{method} answered {text:?}"))?;

        // Every method reports failure as an `error` field in the document rather than
        // a D-Bus error, so that the message survives the trip intact.
        if let Some(message) = value.get("error").and_then(|v| v.as_str()) {
            bail!("{message}");
        }
        Ok(value)
    }

    /// The `ok`/`message` shape the acting methods return.
    fn acted<B>(&self, method: &str, args: &B) -> Result<String>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let value = self.call(method, args)?;
        Ok(value
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("done")
            .to_string())
    }

    pub fn list_machines(&self) -> Result<Vec<Value>> {
        let value = self.call("ListMachines", &())?;
        value
            .as_array()
            .cloned()
            .context("ListMachines did not answer with an array")
    }

    pub fn machine(&self, name: &str) -> Result<Value> {
        self.call("Machine", &(name,))
    }

    pub fn host_info(&self) -> Result<Value> {
        self.call("HostInfo", &())
    }

    pub fn plan(&self, name: &str) -> Result<Value> {
        self.call("Plan", &(name,))
    }

    pub fn start_machine(&self, name: &str) -> Result<String> {
        self.acted("StartMachine", &(name,))
    }

    pub fn stop_machine(&self, name: &str, grace_secs: u32) -> Result<String> {
        self.acted("StopMachine", &(name, grace_secs))
    }

    pub fn create_machine(&self, definition: &str) -> Result<String> {
        self.acted("CreateMachine", &(definition,))
    }

    /// Replace an existing definition. The store belongs to root, so the settings editor
    /// goes through here rather than writing it as the desktop user.
    pub fn save_machine(&self, definition: &str) -> Result<String> {
        self.acted("SaveMachine", &(definition,))
    }

    /// Bring the host side of a guest network up, and get what the guest should use.
    pub fn net_setup(&self, index: u8) -> Result<Value> {
        self.call("NetSetup", &(index,))
    }

    pub fn net_teardown(&self) -> Result<String> {
        self.acted("NetTeardown", &())
    }

    pub fn net_status(&self) -> Result<Value> {
        self.call("NetStatus", &())
    }

    /// The images that can be fetched, and the mirrors available.
    pub fn image_catalogue(&self) -> Result<Value> {
        self.call("ImageCatalogue", &())
    }

    /// What is downloaded and what is deployed.
    pub fn image_status(&self) -> Result<Value> {
        self.call("ImageStatus", &())
    }

    /// Download an image and make a disk out of it. This is the long call: it does the
    /// whole download and unpack before returning.
    pub fn deploy_image(
        &self,
        id: &str,
        name: &str,
        size_mib: u64,
        attach_to: &str,
        as_root: bool,
    ) -> Result<Value> {
        self.call("DeployImage", &(id, name, size_mib, attach_to, as_root))
    }

    /// Install the SSH key and the first-boot sshd setup into an existing image.
    pub fn deploy_ssh(&self, image: &str, user: &str) -> Result<Value> {
        self.call("DeploySsh", &(image, user))
    }

    pub fn resize_disk(&self, path: &str, size_mib: u64) -> Result<Value> {
        self.call("ResizeDisk", &(path, size_mib))
    }

    pub fn format_disk(&self, path: &str, filesystem: &str) -> Result<Value> {
        self.call("FormatDisk", &(path, filesystem))
    }

    pub fn delete_disk(&self, path: &str) -> Result<Value> {
        self.call("DeleteDisk", &(path,))
    }

    /// Which machines have this disk attached.
    pub fn disk_users(&self, path: &str) -> Result<Value> {
        self.call("DiskUsers", &(path,))
    }

    /// Size, filesystem and users, in one call.
    pub fn disk_info(&self, path: &str) -> Result<Value> {
        self.call("DiskInfo", &(path,))
    }

    pub fn delete_machine(&self, name: &str) -> Result<String> {
        self.acted("DeleteMachine", &(name,))
    }

    /// The last `max_bytes` of console output.
    pub fn console_tail(&self, name: &str, max_bytes: u32) -> Result<Vec<u8>> {
        let reply = self
            .conn
            .call_method(
                Some(BUS_NAME),
                OBJECT_PATH,
                Some(INTERFACE),
                "ConsoleTail",
                &(name, max_bytes),
            )
            .with_context(|| format!("asking for {name}'s console"))?;
        reply
            .body()
            .deserialize::<Vec<u8>>()
            .context("ConsoleTail did not answer with bytes")
    }

    pub fn attach_disk(
        &self,
        name: &str,
        path: &str,
        readonly: bool,
        root: bool,
        label: &str,
    ) -> Result<String> {
        self.acted(
            "AttachDisk",
            &(name, path, readonly, root, label),
        )
    }

    pub fn detach_disk(&self, name: &str, path: &str) -> Result<String> {
        self.acted("DetachDisk", &(name, path))
    }

    pub fn add_share(&self, name: &str, path: &str, tag: &str, readonly: bool) -> Result<String> {
        self.acted("AddShare", &(name, path, tag, readonly))
    }

    pub fn remove_share(&self, name: &str, tag: &str) -> Result<String> {
        self.acted("RemoveShare", &(name, tag))
    }

    pub fn create_disk(
        &self,
        path: &str,
        size: &str,
        format: &str,
        attach_to: &str,
    ) -> Result<String> {
        self.acted("CreateDisk", &(path, size, format, attach_to))
    }

    pub fn list_disks(&self) -> Result<Vec<Value>> {
        let value = self.call("ListDisks", &())?;
        value
            .as_array()
            .cloned()
            .context("ListDisks did not answer with an array")
    }

    /// Read a field out of one of the JSON documents.
    pub fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
        value.get(key)
    }
}

/// The daemon's view of a machine, and whether one is reachable.
///
/// The window asks once at startup and keeps the answer. A daemon that appears later
/// would need a reconnect; a window that is already running can simply be reopened,
/// and pretending otherwise would mean polling the bus on every refresh.
#[derive(Default)]
pub struct Bridge {
    client: Option<Client>,
}

impl Clone for Bridge {
    fn clone(&self) -> Self {
        // A `zbus` blocking connection is a handle to a socket shared behind an `Arc`, so
        // cloning the client is cheap and both halves see the same bus.
        Bridge {
            client: self.client.as_ref().and_then(|_| Client::connect()),
        }
    }
}

impl Bridge {
    pub fn new() -> Self {
        let client = Client::connect();
        match &client {
            Some(_) => log::info!("talking to the daemon over the system bus"),
            None => log::info!("no daemon on the bus; managing the store directly"),
        }
        Bridge { client }
    }

    pub fn connected(&self) -> bool {
        self.client.is_some()
    }

    pub fn client(&self) -> Option<&Client> {
        self.client.as_ref()
    }
}
