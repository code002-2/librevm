# Graphics on Gunyah

What works, what does not, and the evidence for each. Written after a long afternoon of
bisection, so that the next person does not repeat it.

## Summary

A guest can have a working `virtio-gpu`. It gets `/dev/dri/card0`, a DRM driver, a
connected scanout with ten modes, and `fastfetch` reports `RedHat Virtio 1.0 GPU`. What has
not been demonstrated is a **window on the host** — crosvm opens the Wayland connection and
reports no error, but no toplevel ever appears.

```
                          device   driver   scanout   window
gpu off                     --       --        --        --
gpu on, old kernel          yes      no        no        no
gpu on, kernel with DRM     yes      yes       yes       no
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

Three things about the resulting command line are each worth an entry on their own.

### `displays=`, not `width=`/`height=`

Both parse. Only one works.

```
--gpu backend=2d,width=1280,height=1024
  WARN 'width' and 'height' in '--gpu' are deprecated; please use `displays=[...]`
  -> the Wayland connection is opened, no window is created, nothing is reported wrong

--gpu backend=2d,displays=[[mode=windowed[1280,1024]]]
  -> no warning
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
crosvm as root:              runs to a shell
```

This is true with or without `--gpu`, and with or without `--disable-sandbox`, which is how
the GPU was wrongly blamed for it for most of an afternoon. `--disable-sandbox` is still
needed as the desktop user, because minijail cannot mount `/proc` without root:

```
libminijail: cannot mount 'proc' as '/var/empty/proc' with flags 0xf: Operation not permitted
```

The daemon is root, so the daemon starts crosvm and passes it the user's session. Root can
reach the socket: it is `srwxrwxr-x`, and root is not stopped by the mode of `/run/user/1000`.

## What has not been made to work

**No window appears.** The evidence that this is not a setup mistake:

* `failed to open display` is absent, so the backend was selected and the socket opened.
* `deprecation warnings: 0`, so it is not the deprecated spelling.
* crosvm holds 61 open sockets, so it is connected to the compositor.
* The same compositor accepts a GUI program run as root (`gnome-text-editor` stayed up), so
  it is not refusing root clients.
* The guest's DRM is complete: `modetest -M virtio_gpu` lists 16 encoders, a `connected`
  connector, and ten modes including `1280x1024` as preferred.

What is left is that crosvm only creates the toplevel when the guest presents a scanout, and
plain writes to `/dev/fb0` may not count as a present. The next step is a real modeset from
inside the guest — `modetest -M virtio_gpu -s 40@41:1280x1024` — which failed here only
because the test disk's `libdrm.so.2` had been corrupted by repeated writes
(`Exec format error`).

## A note on the earlier conclusion

An earlier version of this repository recorded that "virtio as a whole is unusable on
Gunyah" and blamed a lent-memory mapping failure for `--gpu`. Both were wrong.

The first was wrong because a single device, `virtio-balloon`, kills crosvm on probe, and
turning it off with `--no-balloon` fixes block and network and sound and GPU together.

The second was wrong because the failure being attributed to the GPU was Gunyah refusing the
VM under a non-root caller. Vary one flag at a time, and check that the control case is
actually the same in every respect but that flag.
