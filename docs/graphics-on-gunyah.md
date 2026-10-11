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
