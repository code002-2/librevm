//! Feeding the host's touchscreen to a guest.
//!
//! Passing the event device to crosvm directly does not work on a machine with a desktop
//! already running. `--input multi-touch[path=/dev/input/eventN]` opens the node with
//! `EVIOCGRAB`, which takes it exclusively, and on the Xiaomi Pad 8 Pro `gnome-shell` holds
//! the panel. The grab fails and crosvm reports
//!
//! ```text
//! 0: failed to open event device '/dev/input/event4'
//! 1: Connection refused (os error 111)
//! ```
//!
//! which reads like a socket problem and is not one -- nothing in that message mentions the
//! grab.
//!
//! `--multi-touch <socket>` takes a different route: crosvm listens on a socket and reads
//! events from it, grabbing nothing. An evdev node may have any number of readers, so this
//! process reads the panel alongside the desktop without disturbing it, and translates as it
//! goes.
//!
//! The kernel hands out `input_event`:
//!
//! ```text
//! i64 seconds, i64 microseconds, u16 type, u16 code, s32 value     -- 24 bytes
//! ```
//!
//! and crosvm's socket wants `virtio_input_event`:
//!
//! ```text
//! u16 type, u16 code, u32 value                                    -- 8 bytes, little-endian
//! ```
//!
//! so each record becomes a shorter one with the timestamp dropped.

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The size of one `input_event` in the kernel's ABI.
const INPUT_EVENT_SIZE: usize = 24;

/// The size of one `virtio_input_event` on the wire.
const VIRTIO_EVENT_SIZE: usize = 8;

/// A touchscreen on the host that a guest could be given.
pub struct TouchDevice {
    /// The event node, such as `/dev/input/event4`.
    pub path: PathBuf,
    /// What the kernel calls it.
    pub name: String,
    /// The width and height its coordinates are reported in, when it says.
    pub size: Option<(u32, u32)>,
}

/// Find the host's touchscreen.
///
/// Chosen by the kernel's own naming and by the `INPUT_PROP_DIRECT` property, which is what
/// distinguishes a panel from a touchpad: a touchpad reports absolute coordinates too, but it
/// is not the thing the operator is pointing at.
pub fn find_touchscreen() -> Option<TouchDevice> {
    let Ok(entries) = std::fs::read_dir("/sys/class/input") else {
        return None;
    };
    let mut candidates: Vec<(String, TouchDevice)> = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("event") {
            continue;
        }
        let dir = entry.path();
        let device = dir.join("device");

        let what = std::fs::read_to_string(device.join("name"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        // `properties` is a hex bitmask; bit 1 is INPUT_PROP_DIRECT.
        let props = std::fs::read_to_string(device.join("properties"))
            .ok()
            .and_then(|s| u64::from_str_radix(s.trim(), 16).ok())
            .unwrap_or(0);
        let direct = props & (1 << 1) != 0;

        let looks_like_touch = {
            let lower = what.to_lowercase();
            lower.contains("touch") && !lower.contains("touchpad")
        };
        if !looks_like_touch && !direct {
            continue;
        }

        // The extent is read from the device itself when the forwarder opens it; here only
        // the name matters.
        candidates.push((
            name.clone(),
            TouchDevice {
                path: PathBuf::from(format!("/dev/input/{name}")),
                name: what,
                size: None,
            },
        ));
    }

    // `event4` after `event10`, and prefer one that reports a size.
    candidates.sort_by(|a, b| {
        let key = |n: &str| n.trim_start_matches("event").parse::<u32>().unwrap_or(0);
        key(&a.0).cmp(&key(&b.0))
    });
    candidates
        .into_iter()
        .max_by_key(|(_, d)| d.size.is_some())
        .map(|(_, d)| d)
}

/// What an absolute axis reports, as the kernel describes it.
///
/// A panel does not report in pixels of anything: it reports in its own units, and on this
/// device they are 3199 by 2135 where the guest's screen is 1280 by 1024. Passing those
/// numbers through unchanged puts most of the screen out of reach -- everything past the
/// three-quarter mark clamps to an edge -- which reads as "the touch is offset" rather than
/// as a scaling mistake.
///
/// The kernel keeps the ranges in the device and exposes them only through `EVIOCGABS`, so
/// this asks for them rather than guessing.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

/// The extent of one absolute axis, as `(min, max)`.
fn axis_range(fd: std::os::fd::RawFd, code: u32) -> Option<(i32, i32)> {
    // EVIOCGABS(code) is _IOR('E', 0x40 + code, struct input_absinfo).
    const IOC_READ: u64 = 2;
    const IOC_NRSHIFT: u64 = 0;
    const IOC_TYPESHIFT: u64 = 8;
    const IOC_SIZESHIFT: u64 = 16;
    const IOC_DIRSHIFT: u64 = 30;
    let size = std::mem::size_of::<InputAbsInfo>() as u64;
    let request = (IOC_READ << IOC_DIRSHIFT)
        | ((b'E' as u64) << IOC_TYPESHIFT)
        | (((0x40 + code) as u64) << IOC_NRSHIFT)
        | (size << IOC_SIZESHIFT);

    let mut info = InputAbsInfo::default();
    // SAFETY: the request encodes the size of the struct being passed, and the pointer is to
    // a live local of exactly that type.
    let rc = unsafe { libc::ioctl(fd, request as libc::c_ulong, &mut info) };
    if rc != 0 {
        return None;
    }
    (info.maximum > info.minimum).then_some((info.minimum, info.maximum))
}

/// How to turn the panel's units into the guest's.
#[derive(Debug, Clone, Copy)]
pub struct Calibration {
    pub x_min: i32,
    pub x_max: i32,
    pub y_min: i32,
    pub y_max: i32,
    /// What the guest's screen is, which is what the values have to end up in.
    pub width: i32,
    pub height: i32,
    /// The part of the panel the window covers, as `(x, y, width, height)` in panel units.
    /// `None` means the whole panel, which is right only when the window fills the screen.
    pub area: Option<(i32, i32, i32, i32)>,
}

impl Calibration {
    /// Read the panel's ranges, falling back to the guest's own size when it will not say.
    ///
    /// A device that already reports in screen units needs no scaling, and saying so
    /// explicitly keeps the arithmetic honest rather than special-casing it later.
    pub fn read(
        fd: std::os::fd::RawFd,
        width: u32,
        height: u32,
        area: Option<[i32; 4]>,
    ) -> Self {
        // A window the size of nothing is worse than no window information at all, so a
        // degenerate rectangle is treated as absent.
        let area = area
            .filter(|a| a[2] > 0 && a[3] > 0)
            .map(|a| (a[0], a[1], a[2], a[3]));

        match (axis_range(fd, 0x35), axis_range(fd, 0x36)) {
            (Some((x_min, x_max)), Some((y_min, y_max))) => Self {
                x_min,
                x_max,
                y_min,
                y_max,
                width: width as i32,
                height: height as i32,
                area,
            },
            _ => Self {
                x_min: 0,
                x_max: width as i32,
                y_min: 0,
                y_max: height as i32,
                width: width as i32,
                height: height as i32,
                area,
            },
        }
    }

    /// Scale one coordinate, clamped, so a panel that reports slightly past its stated maximum
    /// cannot put the pointer off the screen.
    fn scale(value: i32, min: i32, max: i32, out: i32) -> i32 {
        if max <= min || out <= 0 {
            return 0;
        }
        let v = value.clamp(min, max) - min;
        let scaled = (v as i64 * out as i64) / (max - min) as i64;
        scaled.clamp(0, out as i64) as i32
    }

    /// Map a panel point into the guest's screen, or `None` when it is outside the window.
    ///
    /// Two limits apply in turn: first the window's rectangle within the panel, since a touch
    /// outside the window has no place in the guest, then the guest's own resolution.
    pub fn map(&self, px: i32, py: i32) -> Option<(i32, i32)> {
        let (x, y) = match self.area {
            Some((ax, ay, aw, ah)) => {
                if px < ax || py < ay || px >= ax + aw || py >= ay + ah {
                    return None;
                }
                (
                    Self::scale(px - ax, 0, aw, self.width),
                    Self::scale(py - ay, 0, ah, self.height),
                )
            }
            None => (
                Self::scale(px, self.x_min, self.x_max, self.width),
                Self::scale(py, self.y_min, self.y_max, self.height),
            ),
        };
        Some((x, y))
    }

    /// A short description, for the log.
    pub fn describe(&self) -> String {
        let area = match self.area {
            Some((x, y, w, h)) => format!("window at ({x},{y}) {w}x{h} in panel"),
            None => "whole panel (no window geometry given)".to_string(),
        };
        format!(
            "panel x[{}..{}] y[{}..{}] -> screen {}x{}; {area}",
            self.x_min, self.x_max, self.y_min, self.y_max, self.width, self.height
        )
    }
}

/// Run the forwarder until `stop` is set.
///
/// One connection at a time: crosvm connects once and stays. A second connection would mean a
/// second machine asking for the same panel, which cannot be shared meaningfully, so it is
/// served in turn.
pub fn serve(
    device: &Path,
    socket: &Path,
    screen: (u32, u32),
    area: Option<[i32; 4]>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    if socket.exists() {
        std::fs::remove_file(socket)
            .with_context(|| format!("removing the old socket at {}", socket.display()))?;
    }
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }

    let listener = UnixListener::bind(socket)
        .with_context(|| format!("listening on {}", socket.display()))?;
    listener
        .set_nonblocking(true)
        .context("making the touch socket non-blocking")?;

    // The socket has to be group writable for the same reason the console one does: the
    // daemon is root and the things that may want to watch it are not.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o660));
    }

    log::info!("touch: listening on {} for {}", socket.display(), device.display());

    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((conn, _)) => {
                log::info!("touch: a client connected");
                if let Err(e) = pump(device, conn, screen, area, &stop) {
                    log::warn!("touch: {e:#}");
                }
                log::info!("touch: the client went away");
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => bail!("accepting a touch connection: {e}"),
        }
    }

    let _ = std::fs::remove_file(socket);
    Ok(())
}

/// Read the panel and write the guest's format, until the connection or the stop flag ends it.
fn pump(
    device: &Path,
    mut conn: UnixStream,
    screen: (u32, u32),
    area: Option<[i32; 4]>,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let file = std::fs::File::open(device)
        .with_context(|| format!("opening {}", device.display()))?;

    // The panel reports in its own units and they are not the screen's, so read the ranges
    // off the device and scale into the guest's resolution.
    use std::os::fd::AsRawFd;
    let calibration = Calibration::read(file.as_raw_fd(), screen.0, screen.1, area);
    log::info!("touch: {}", calibration.describe());
    let mut file = file;

    // crosvm writes status updates back -- LED state and so on. Nothing here acts on them,
    // but if nobody reads them the socket buffer fills and crosvm blocks on write.
    let drain = conn.try_clone().context("cloning the touch connection")?;
    let draining = stop.clone();
    std::thread::spawn(move || {
        let mut sink = std::io::sink();
        let mut socket = drain;
        socket
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .ok();
        let mut buf = [0u8; 1024];
        while !draining.load(Ordering::Relaxed) {
            match socket.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    let _ = sink.write_all(&[]);
                }
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => break,
            }
        }
    });

    let mut raw = vec![0u8; INPUT_EVENT_SIZE * 64];
    let mut pending: Vec<u8> = Vec::new();
    // The pair of position values most recently seen, and whether the current frame belongs
    // to a touch that is inside the window.
    let mut pending_x: Option<i32> = None;
    let mut pending_y: Option<i32> = None;
    let mut outside = false;

    while !stop.load(Ordering::Relaxed) {
        let read = match file.read(&mut raw) {
            Ok(0) => break,
            Ok(n) => n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("reading the touchscreen"),
        };

        pending.extend_from_slice(&raw[..read]);
        let mut out: Vec<u8> = Vec::with_capacity(pending.len() / 3);
        let complete = pending.len() / INPUT_EVENT_SIZE * INPUT_EVENT_SIZE;
        for record in pending[..complete].chunks_exact(INPUT_EVENT_SIZE) {
            // i64 seconds, i64 microseconds, u16 type, u16 code, s32 value -- all native
            // endian, which on this platform is little.
            let typ = u16::from_ne_bytes([record[16], record[17]]);
            let code = u16::from_ne_bytes([record[18], record[19]]);
            let mut value = i32::from_ne_bytes([record[20], record[21], record[22], record[23]]);

            // A position has to be translated together with its pair, and whether it is
            // inside the window is only known once both are in hand, so the position events
            // are held back and the frame is closed with the translated pair. Everything else
            // -- slots, tracking ids, the button -- is a code rather than a coordinate and is
            // passed through untouched.
            if typ == 3 && (code == 0x35 || code == 0x00) {
                pending_x = Some(value);
                continue;
            }
            if typ == 3 && (code == 0x36 || code == 0x01) {
                pending_y = Some(value);
                continue;
            }

            // A frame boundary is where the held positions are flushed, so the guest never
            // sees half a coordinate.
            if typ == 0 && code == 0 {
                if let (Some(px), Some(py)) = (pending_x, pending_y) {
                    let mapped = calibration.map(px, py);
                    // One line per frame, at debug: this process is the only one that sees
                    // both the panel's units and the guest's, so it is where the mapping can
                    // be checked without a second listener on either side.
                    match mapped {
                        Some((x, y)) => log::debug!("touch: panel ({px},{py}) -> guest ({x},{y})"),
                        None => log::debug!("touch: panel ({px},{py}) is outside the window"),
                    }
                    if let Some((x, y)) = mapped {
                        out.extend_from_slice(&3u16.to_le_bytes());
                        out.extend_from_slice(&0x35u16.to_le_bytes());
                        out.extend_from_slice(&(x as u32).to_le_bytes());
                        out.extend_from_slice(&3u16.to_le_bytes());
                        out.extend_from_slice(&0x36u16.to_le_bytes());
                        out.extend_from_slice(&(y as u32).to_le_bytes());
                        outside = false;
                    } else {
                        // Outside the window: the whole frame goes, or the guest would see a
                        // position with no valid pair.
                        outside = true;
                    }
                    pending_x = None;
                    pending_y = None;
                } else if outside {
                    pending_x = None;
                    pending_y = None;
                }
            }
            out.extend_from_slice(&typ.to_le_bytes());
            out.extend_from_slice(&code.to_le_bytes());
            // virtio_input_event carries the value unsigned; the kernel's negative values
            // are only used for axis inversion, which a panel does not do.
            out.extend_from_slice(&(value as u32).to_le_bytes());
        }
        pending.drain(..complete);

        if out.is_empty() {
            continue;
        }
        if let Err(e) = conn.write_all(&out) {
            return Err(e).context("writing to the guest's touch socket");
        }
    }

    Ok(())
}

/// Where a machine's touch socket lives.
pub fn socket_path(run_dir: &Path, name: &str) -> PathBuf {
    run_dir.join(format!("{name}.touch.sock"))
}

/// The size to describe the touchscreen with, given what the guest's display is.
///
/// The kernel does not expose an absolute axis range as a file, so the display's own size is
/// the best answer and is right for a panel that fills the screen, which is what this is.
pub fn fallback_size(vm: &crate::config::VmConfig) -> (u32, u32) {
    if vm.gpu.width > 0 && vm.gpu.height > 0 {
        (vm.gpu.width, vm.gpu.height)
    } else {
        (1280, 1024)
    }
}
