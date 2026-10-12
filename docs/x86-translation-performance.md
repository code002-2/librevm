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

## Which desktop CPU this is closest to

A raw MIPS number does not answer that on its own, because this SoC is allowed to run at very
different clocks depending on load. Dividing by the clock removes that variable and leaves the
core's efficiency, which is the part that can be compared across architectures.

Measured, three runs each, with the clock sampled while each run was in progress:

| | Tot | average clock | MIPS per GHz |
|---|---|---|---|
| native aarch64 | 8454 | 3.851 GHz | 2196 |
| native aarch64 | 8375 | 3.765 GHz | 2224 |
| native aarch64 | 8243 | 3.744 GHz | 2202 |
| x86-64 through FEX | 6666 | 3.558 GHz | 1873 |
| x86-64 through FEX | 6568 | 3.485 GHz | 1884 |
| x86-64 through FEX | 6487 | 3.470 GHz | 1869 |

So: **2207 MIPS/GHz natively, 1875 through FEX**, a ratio of 0.850, which is the same 1.18x the
earlier runs gave at other clocks. sysbench agrees on the ratio at 1217 events/sec/GHz native.

Published anchors for the same benchmark, converted to the same units:

| core | MIPS per GHz | how |
|---|---|---|
| Ryzen 5 3600 (Zen 2) | ~2750 | ~11000 MIPS at 4.0 GHz |
| i5-10400 (Comet Lake) | ~2350 | ~10000 MIPS at 4.25 GHz |
| **this machine, native** | **2207** | measured |
| i7-4770 (Haswell) | ~2060 | ~7400 MIPS at 3.6 GHz |
| **this machine, through FEX** | **1875** | measured |
| Cortex-A76 (Raspberry Pi 5) | ~1375 | ~3300 MIPS at 2.4 GHz |

The conclusion, stated as a judgement rather than a range:

* **Per clock, this is a modern desktop-class core.** 2207 MIPS/GHz is about 80 percent of
  Zen 2 and about 94 percent of Comet Lake. Oryon is not a cut-down core.
* **In absolute single-thread terms it sits between an i7-4770 and an i5-10400**, roughly
  80 percent of the latter. It is not a fast desktop chip, but it is a real one, not a mobile
  part punching above its weight.
* **Through FEX it is about 91 percent of an i7-4770**, or about 68 percent of an i5-10400.
* **Multi-core is a different story.** Eight threads give 2.8 times one thread, not eight,
  because the sustained clock falls to 1.7-1.9 GHz and the six smaller cores are not equal to
  the two prime ones. Multithreaded, it behaves like a four-core desktop chip of Haswell vintage.

The limitation is not the core. It is the power budget: 3.28 GHz sustained on a prime core
against 4.0-4.5 GHz on a desktop, with the all-core clock held near 1.7 GHz by a thermal
governor that keeps the die just under 95 C.

