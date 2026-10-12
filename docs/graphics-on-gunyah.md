# Graphics on Gunyah

What works, what does not, and the evidence for each. Written after a long afternoon of
bisection, so that the next person does not repeat it.

## Summary

A guest gets a working `virtio-gpu`, and **a window appears on the host** showing its
framebuffer — a black panel with the kernel's Tux logo, which is fbcon's boot screen. The
guest has `/dev/dri/card0`, a DRM driver, a connected scanout with ten modes, and `fastfetch`
reports `librevm virtio gpu`.

```
                          device   driver   scanout   window
gpu off                     --       --        --        --
gpu on, old kernel          yes      no        no        no
gpu on, kernel with DRM     yes      yes       yes       yes
```

## What the kernel needs

The guest boots from an initramfs with **no module tree** in it, so a driver built as a
module is simply absent. arm64 defconfig has what is needed as `=m`:

```
CONFIG_DRM=m
CONFIG_DRM_VIRTIO_GPU=m
```

That has to become `=y`, along with the pieces it selects:

```sh
./scripts/config \
  --enable DRM --enable DRM_VIRTIO_GPU --enable DRM_KMS_HELPER \
  --enable DRM_GEM_SHMEM_HELPER --enable DRM_FBDEV_EMULATION \
  --enable FB --enable DRM_SIMPLEDRM \
  --enable VIRTIO --enable VIRTIO_PCI --enable VIRTIO_BLK --enable VIRTIO_NET \
  --enable VIRTIO_INPUT --enable VIRTIO_CONSOLE
yes "" | make ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- olddefconfig
```

`kernels/mini-gpu2-Image` is built this way; `mini-blk-Image` and `mini-gpu-Image` are not,
and a machine pointing at either will enumerate the PCI device and bind nothing to it. The
symptom is a machine that looks fine and has no `/sys/class/drm`.

## What the command needs

`VmConfig` carries a `[gpu]` section:

```toml
[gpu]
enabled = true
backend = "2d"     # 2d | virglrenderer | gfxstream
width = 1280       # both or neither
height = 1024
# wayland_sock = "/run/user/1000/wayland-0"   # found automatically when left out
```

which becomes:

```
--gpu backend=2d,displays=[[mode=windowed[1280,1024]]]
--wayland-sock /run/user/1000/wayland-0
```

Three things about that command line are each worth an entry on their own.

### `displays=`, not `width=`/`height=`

Both parse. Only one works.

```
--gpu backend=2d,width=1280,height=1024
  WARN 'width' and 'height' in '--gpu' are deprecated; please use `displays=[...]`
  -> the Wayland connection is opened, no window is created, nothing is reported wrong

--gpu backend=2d,displays=[[mode=windowed[1280,1024]]]
  -> no warning, window appears
```

The syntax was found by bisection and is not guessable from the help text:

| value | result |
|---|---|
| `displays=[windowed]` | `expected ',' at position 21` |
| `displays=[[windowed]]` | `expected '[' at position 30` |
| `displays=[windowed[1280,1024]]` | `expected ',' at position 21` |
| `displays=[[mode=windowed]]` | `expected '[' at position 35` |
| `displays=[[mode=windowed[1280,1024]]]` | **accepted** |
| `displays=[[mode=borderless_full_screen]]` | `unknown variant`, this build only has `windowed` |

So: two levels of list, a `mode=` key, and the size bracketed onto the mode — and the size
is **not optional**.

### `--wayland-sock`, not just the environment

crosvm needs the argument *and* `XDG_RUNTIME_DIR`/`WAYLAND_DISPLAY`. Giving only the
environment leaves it on its default backend:

```
ERROR devices::virtio::gpu] failed to open display: unsupported by the implementation
```

That message reads like a missing feature. It means a missing argument. `wayland_socket()`
in `vm.rs` finds the session and both `backend.rs` and `vm.rs` use it, so the argument and
the environment cannot drift apart.

### crosvm has to run as root

Not for the GPU's sake — for Gunyah's.

```
crosvm as the desktop user:  WARN hypervisor::gunyah] unknown gh exit reason: 3
                             ERROR vcpu] vcpu hit unknown error: Invalid argument (os error 22)
crosvm as root:              runs to a shell, and draws
```

This is true with or without `--gpu`, and with or without `--disable-sandbox`, which is how
the GPU was wrongly blamed for it for most of an afternoon. `--disable-sandbox` is still
needed as the desktop user, because minijail cannot mount `/proc` without root:

```
libminijail: cannot mount 'proc' as '/var/empty/proc' with flags 0xf: Operation not permitted
```

The daemon is root, so the daemon starts crosvm and passes it the user's session. Root can
reach the socket: it is `srwxrwxr-x`, and root is not stopped by the mode of `/run/user/1000`.

## How to tell the window is there

Not through the accessibility bus. crosvm is not a GTK application, so it has no AT-SPI node,
and a search of the accessibility tree reports no window while one is plainly on screen. That
cost an afternoon: the conclusion "no window appears" was reached and written down here, and
was wrong.

Look at the screen, or at what the window is showing. The first thing a guest draws is
fbcon's boot logo — a black panel with the Tux penguin — and that is a scanout being
presented, so a penguin on screen is proof the whole path works.

## Buffers: what works and what does not

`modetest` can list the device but cannot set a mode:

```
# modetest -M virtio_gpu
opened device `virtio GPU` on driver `virtio_gpu` (version 0.1.0 at 0)
   ... 16 encoders, connector 40 "connected", ten modes including 1280x1024

# modetest -M virtio_gpu -s 40@41:1280x1024
failed to find CRTC for pipe
failed to create dumb buffer: Invalid argument
```

`DRM_IOCTL_MODE_CREATE_DUMB` fails with `EINVAL`. A dumb buffer wants physically contiguous
guest memory, and a protected VM's memory is lent to Gunyah rather than owned by crosvm, so
that allocation cannot be handed over. This is the lent-memory limit that was suspected at
the very start of the investigation, arriving somewhere quite different from where it was
expected.

It does not stop the console from being visible, because fbcon does not need a dumb buffer:
it writes through the driver's own memory and the virtio-gpu device takes it from there. A
full desktop inside the guest would need a compositor, and a compositor will want a dumb
buffer or a blob, so that is where the next attempt should start — the device advertises
`+resource_blob +host_visible`, and `VIRTIO_GPU_BLOB_MEM_GUEST` is the path that avoids
needing a contiguous region of the host's own.

## A desktop in the guest

`librevm desktop install <name>` puts XFCE on a machine, and `desktop start` runs it. The
window then shows the desktop, drawn by the guest and presented by crosvm.

```
librevm desktop install demo     # a few hundred MiB from the distribution's mirrors
librevm desktop start demo
librevm desktop status demo
librevm desktop stop demo
```

The machine needs `[gpu] enabled = true` and a kernel with the graphics drivers built in; the
command says so if either is missing.

### The three things that have to be right

**One: no `xorg.conf`.** This is the counter-intuitive one. A configuration that names a
driver makes X treat the GPU as a secondary device:

```
with a hand-written xorg.conf:   (II) modeset(G0): using drv /dev/dri/card0
                                 (EE) No devices detected.
with no configuration at all:    (II) modeset(0): using drv /dev/dri/card0
                                 ... and it works
```

`modeset(G0)` is the "G" for a GPU that is not the primary, and X then finds no screens at
all. Left to probe, it picks `modeset(0)`.

**Two: `xf86-video-fbdev` is not the answer.** The plan was to have X write to `/dev/fb0` and
so avoid the DRM dumb buffers that cannot be allocated here. It loads, and then:

```
(II) Loading /usr/lib/xorg/modules/drivers/fbdev_drv.so
(II) FBDEV: driver for framebuffer: fbdev
(II) Loading sub module "fbdevhw"
(EE) No devices detected.
```

X 21.1's fbdev driver only claims PCI and platform framebuffers, and this one is a DRM client
device, so it matches nothing. The modesetting driver takes the DRM node directly and is what
actually runs — and, contrary to the expectation, it does not need a dumb buffer to start.

**Three: udev has to be running before X.** X enumerates input through udev. A bare Alpine
rootfs ships mdev, so the virtio touchscreen and keyboard exist in the kernel and are
invisible to X, which looks exactly like the hypervisor not providing them:

```
with mdev only, X starts with:   ⎡ Virtual core pointer
                                 ⎜   ↳ Virtual core XTEST pointer
with eudev running first:        ⎜   ↳ Virtual core XTEST pointer
                                 ⎜   ↳ Crosvm Virtio Multitouch Touchscreen
```

So the launcher starts `udevd`, waits for `/run/udev/data`, runs `udevadm trigger --action=add`
and `udevadm settle`, and only then starts X. The order matters, which is why the launcher is
a file with comments in it rather than a line of shell in the installer.

## Input

crosvm forwards nothing from the display window unless asked, so a desktop without
`--display-window-keyboard` and `--display-window-mouse` is a picture: visible and completely
unusable. Both are emitted whenever a GPU is enabled.

Those two flags also make crosvm create a `Crosvm Virtio Multitouch Touchscreen` in the guest.
It appears in X and is bound by libinput, but delivers nothing — crosvm's window does not
appear to listen for `wl_touch`, and GNOME does not synthesise pointer events from a
touchscreen unless asked to. The device is present and silent.

### The forwarder

`[gpu] touch = true` makes librevm read the panel itself and feed crosvm over a socket:

```
librevm reads /dev/input/event4  ->  translates  ->  /run/librevm/<name>.touch.sock
crosvm reads that socket (--multi-touch path=...)
```

The socket form exists because the direct one does not work here. `--input
multi-touch[path=/dev/input/eventN]` opens the device with `EVIOCGRAB`, which takes it
exclusively, and `gnome-shell` already holds the panel:

```
/dev/input/event4:   root  1      systemd
                     root  12085  systemd-logind
                     piano 12485  gnome-shell
crosvm: 0: failed to open event device '/dev/input/event4'
        1: Connection refused (os error 111)
```

A socket needs no exclusive access, and an evdev node may have any number of readers, so the
forwarder reads the panel alongside the desktop without disturbing it.

The wire format is `virtio_input_event`, 8 bytes little-endian — `u16 type, u16 code, u32
value` — where the kernel gives `input_event`, 24 bytes with a timestamp in front. crosvm's
source says so directly: *"it handles `virtio_input_event` instead of `input_event`
structures"*.

### Coordinates

A window is not the screen, and this cost several rounds.

```
panel, from EVIOCGABS:        3199 x 2135
screen, from DisplayConfig:   3200 x 2136
scaling factor:               2
window, from the a11y tree:   x=0 y=0 1040x700   (logical)
                            = 2080x1400           (panel units)
```

Mapping the whole panel onto the guest's 1280x1024 puts a touch two thirds of the way across
the window three quarters of the way across the screen. The forwarder now reads the panel's
real ranges with `EVIOCGABS` rather than being told them, and takes the window's rectangle
from `[gpu] touch_area = [x, y, w, h]` in panel units. A touch outside that rectangle is
dropped, since it has no corresponding place in the guest.

`librevm desktop touch-area <name>` reads the rectangle off the accessibility bus — the
compositor will not list its windows to an ordinary client, but a window is an object and its
extents are part of the tree — and prints the value to use:

```
$ librevm desktop touch-area demo
window in logical pixels: x=0 y=0 w=1040 h=700
scaling factor to check: `gsettings get org.gnome.desktop.interface scaling-factor`
touch_area = [0, 0, 1040, 700]   # multiply the four numbers above by the factor
```

**Still open.** Whether the scaling factor is the right conversion between the accessibility
tree's logical pixels and the panel's own units has not been confirmed by touching the screen:
the arithmetic gives 2080x1400 and every piece is in place, but a touch in the corner of the
window has not been shown to land in the corner of the screen. If it is wrong, the rectangle
is the one number to change, and `touch_area` exists so that changing it does not need a
rebuild.

## One window, from power-on to desktop

A machine with a GPU is given the framebuffer console as well as the serial one, so the
window shows the whole boot the way a real machine does: the kernel log, then the initramfs,
then the console, then the desktop, all in the same window.

```
--params console=ttyS0 earlycon loglevel=7 console=tty0
```

**Why `console=tty0`.** `tty0` is the current virtual terminal, which is where fbcon draws,
which is the framebuffer, which is what crosvm's window shows. Without it the kernel has only
the serial port to talk to, and the window stays on the boot logo:

```
/proc/consoles:   ttyS0    -W- (E   p a)   4:64          <- the serial port only
with console=tty0:
/proc/consoles:   tty0     -WU (EC     )   4:1
                  ttyS0    -W- (E   p a)   4:64
[    0.012929] printk: legacy console [tty0] enabled
[    5.845118] printk: legacy console [ttyS0] enabled
[    6.441363] Console: switching to colour frame buffer device 160x64
```

**Why the order is `ttyS0` then `tty0`.** printk broadcasts to every console, so the kernel
log reaches both either way. But `/dev/console` is always major 5 minor 1, and the kernel
routes that to the *last* console registered — which is what the initramfs writes to. Ending
the list with `tty0` puts the initramfs's own output on the framebuffer too:

```
$ echo probe > /dev/console
(absent from the serial capture, so it went to the framebuffer)
```

Reversing them would put the initramfs on the serial port and leave the window blank until
the console took over. Keeping `ttyS0` in the list at all is what keeps `librevm console`
working; it captures 362 lines of a boot.

librevm appends `console=tty0` itself when a machine has a GPU, and does not when it does
not — a machine with no framebuffer console has nowhere to send it. A `console=tty0` already
in the definition is left alone.

**X still takes the display.** fbcon does not hand the framebuffer over — `/sys/class/vtconsole/vtcon1/bind`
stays `1` — but it does not need to: X binds the DRM node directly and the console stops being
drawn once X owns the scanout.

```
(II) modeset(0): using drv /dev/dri/card0
(II) modeset(0): Depth 24, (==) framebuffer bpp 32
xdpyinfo: dimensions: 1280x1024 pixels
```

The framebuffer is 1280x1024 and the console comes up as 160 columns by 64 rows, so the
console text is drawn at 8x16 pixels a character. On a window of that size the boot log is
readable and the shell prompt is usable; a larger font would need `fbcon=font:` on the
command line.

## Naming the machine

A guest reports the device tree crosvm generates, so `fastfetch` says `CPU: dummy-virt`, and
it names the GPU from the PCI ids database, so it says `GPU: RedHat Virtio 1.0 GPU`. Neither
is wrong and neither says anything about the machine the operator made.

`fastfetch` takes a per-module format string, and reads `/etc/fastfetch/config.jsonc` for
every user, so one file in the image relabels both:

```jsonc
{
  "modules": [
    { "type": "cpu", "format": "librevm cpu (sm8750)" },
    { "type": "gpu", "format": "librevm virtio gpu" }
  ]
}
```

Only those two modules are named, so everything else keeps the image's own defaults. The
name is written at deploy time by `Branding::librevm()`, which reads the host's own SoC —
`/proc/device-tree/compatible` on this device says `qcom,sun-mtp`, and Qualcomm's device
trees carry codenames rather than part numbers, so `sun` and `sunp` are translated to
`sm8750` and `sm8750p` and anything else is passed through unchanged rather than guessed at.

Note that an explicit `--structure` on the command line overrides the config, so
`fastfetch --structure CPU:GPU` prints the raw names. Plain `fastfetch` is what shows the
branding.

## A note on the earlier conclusion

An earlier version of this repository recorded that "virtio as a whole is unusable on
Gunyah" and blamed a lent-memory mapping failure for `--gpu`. Both were wrong.

The first was wrong because a single device, `virtio-balloon`, kills crosvm on probe, and
turning it off with `--no-balloon` fixes block and network and sound and GPU together.

The second was wrong because the failure being attributed to the GPU was Gunyah refusing the
VM under a non-root caller. Vary one flag at a time, and check that the control case is
actually the same in every respect but that flag.
