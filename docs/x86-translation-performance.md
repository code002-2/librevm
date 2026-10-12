# Running x86-64 software on the Piano (SM8750P)

What FEX-Emu costs, what Wine costs, and what clock the machine actually runs at. Every number
here was measured on the device itself, not in a guest.

## The clock is managed in three tiers

The first conclusion was wrong, and the correction matters. Writing `cpuinfo_max_freq` into
`scaling_max_freq` is accepted and then reverted within 0.2 s, which looks like a firmware pin;
but the ceiling is not fixed at all. It moves with load, and the SCMI log names the mechanism:

```
arm-scmi arm-scmi.0.auto: SCMI Protocol v2.0 'Qualcomm:' Firmware version 0x20000
arm-scmi arm-scmi.0.auto: Enabling SCMI Quirk [quirk_perf_level_get_fc_force]
```

The firmware's performance level is authoritative, and it is dynamic:

| state | policy0 (cpu0-5) | policy6 (cpu6-7) | temperature |
|---|---|---|---|
| idle | 3.53 GHz | 4.32 GHz | 54-60 C |
| one busy thread | 2.40 GHz | 3.07-3.28 GHz | 64-79 C |
| all cores busy | 1.55-2.00 GHz | 1.69-1.96 GHz | 87-91 C |

The all-core figure is enforced by the kernel thermal governor, which drives the cooling
devices to 11-12 of 15 and holds the temperature just under the 95 C passive trip. The
single-thread figure is not thermal at all: it settles at 3.28 GHz with the die at 64-70 C.

A heatsink on the battery does not change any of this; the limit is a power budget applied by
the CPUCP, not a temperature limit on the battery.

## FEX-Emu

FEX is not in Debian and publishes source only; there is no apt repository and the GitHub
releases carry no binaries. It builds from source in about four minutes on this machine with
`-j2` and clang, once past three requirements:

```
-DBUILD_FEXCONFIG=False     otherwise it demands Qt5 for its GUI front-end
apt install nasm            unittests/ASM needs it
apt install clang-tools-19  ninja's dependency scanner needs clang-scan-deps
```

A dynamic x86-64 binary also needs an x86-64 RootFS, which FEXRootFSFetcher downloads. It
insists on a TTY UI (`--force-ui=tty`, not `cli`) and on `squashfs-tools` being present, both of
which it reports clearly once asked. Fedora 44 is 813 MB compressed and 2.8 GB unpacked.

## What translation costs

Two runs of the same 7-Zip, single-threaded on a prime core, natively and through FEX:

| clock | native aarch64 | x86-64 through FEX | ratio |
|---|---|---|---|
| 2.84 GHz | 6919 MIPS | 5731 MIPS | 0.83 |
| 3.28 GHz | 7974 MIPS | 6391 MIPS | 0.80 |

Both sets are three runs each and vary by less than 0.1 percent within a set, so the ratio is
**1.20 to 1.25 times**. A syscall-heavy shell loop is worse at 1.91x, and a sha256 comparison
reads 9.5x because the native build uses the ARMv8 SHA instructions and the x86-64 build cannot.

For anything compute-bound with an assembly path, 1.2x is the number.

## Wine

Wine has to be an x86-64 Wine running through FEX, which is how it is normally hosted on arm64.
Fedora 44 ships 11.0. Installing it is the awkward part:

* `dnf install wine-core` under FEX spends more than twelve minutes parsing metadata in
  translated C++ and never downloads a byte. dnf5 is too heavy for this.
* Fetching the rpms directly and running `rpm -Uvh --nodeps` works. Note that FEX does not
  chroot, so rpm writes into the *host's* filesystem; the files land in `/usr/bin` and are
  usable, but the rpm database is never updated because Debian has none.
* Only `wine-core` was needed in the end; `wine-common` and `wine-filesystem` add the wrapper
  scripts and shared data.

What works and what does not:

| | result |
|---|---|
| `wine --version` | `wine-11.0 (Staging)`, 0.04 s |
| `wineboot --init` | builds an 804 MB Windows tree, 24 s |
| `wine cmd /c echo hi` | prints `hi`; 9.7 s cold, 4.9 s with wineserver already up |
| 7zr.exe (32-bit) | ShellExecuteEx fails |
| 7z2408-x64.exe | ShellExecuteEx fails, 0.48 s |
| anything Vulkan | Turnip rejects the Adreno: `chip_id = E844050001 is unsupported` |

The Wine startup cost is dominated by starting wineserver per command. With one kept alive the
cost roughly halves. The Vulkan failure is a GPU driver gap in this SoC, not a Wine or FEX one,
and it means DXVK and anything needing Vulkan will not run.

## Where this lands against desktop CPUs

7-Zip's MIPS rating is comparable across machines for the same build.

| | rating |
|---|---|
| native aarch64, this machine, at 3.28 GHz | 6391-7974 |
| x86-64 through FEX, same conditions | 5656-6391 |

Published single-thread figures for orientation, from various runs of various 7-Zip builds:

* Cortex-A76 at 2.4 GHz (Raspberry Pi 5 class): 3000-3500
* this machine, native: around 7000-8000
* Apple M1: 9000-10000
* Ryzen 5 3600 / i5-10400: 11000-13000

Those are order-of-magnitude anchors, not a ranking. What the measurements support is that this
machine performs like a fast laptop core and roughly half a desktop Zen 2 or Comet Lake core,
and that x86-64 software through FEX gives up about a fifth of that.

The all-core picture is much weaker than the single-thread one: eight threads produce 2.8 times
the throughput of one, not eight, because the sustained clock falls to about 1.7 GHz.
