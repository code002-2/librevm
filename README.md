# librevm

A virtual machine manager for Linux, with first-class **Gunyah** support.

Written in Rust. The core is a library, the front ends are a CLI and a GTK4 /
libadwaita GUI, and both drive the same code — which is why a machine started
from the window and one started from a shell behave identically.

```
┌──────────────────────────────────────────────┐
│  librevm-gui          librevm (CLI)          │
│      │                     │                 │
│      └────────┬────────────┘                 │
│               ▼                              │
│        librevm-core                          │
│   config · store · backend · hub · vm        │
│               │                              │
│               ▼                              │
│     crosvm ── /dev/gunyah ── Gunyah RM       │
└──────────────────────────────────────────────┘
```

## 为什么做这个

在 Xiaomi Pad 8 Pro（`piano`，SM8750P）上让 Linux 跑起来之后，需要一种
**不用每次手打一长串 crosvm 参数**的办法。现有工具要么只面向 Android，
要么对 Gunyah 一无所知。

librevm 的目标是：**把「在这台机器上开一个虚拟机」变成点一下**，
并且把 Gunyah 那些反直觉的地方（lend 路径、swiotlb、BTI、门铃）
封装进代码和文档里。

## 功能现状

| 功能 | 状态 |
|---|---|
| 虚拟机定义（TOML） | ✅ 可用 |
| CLI：`create` / `list` / `start` / `stop` / `delete` / `plan` | ✅ 可用 |
| 控制台：双向串口（可 detach 重连） | ✅ 可用 |
| GTK4 GUI：侧栏 + 详情 + 控制台 | ✅ 可用 |
| 中英双语界面 + 语言切换 | ✅ 可用 |
| Gunyah 安装脚本（校验 vermagic 与 BTI） | ✅ 可用 |
| 首次启动检测 Gunyah 并引导安装 | ✅ 可用 |
| 客户机网络（virtio-net） | ❌ 未完成 |
| 磁盘（virtio-blk） | ❌ 未开始 |
| D-Bus daemon（GUI 以普通用户运行） | ❌ 未开始 |

## 构建

需要 Rust 1.85 或更新，以及 GTK4 和 libadwaita 的开发文件。

```sh
# Debian / Ubuntu
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev

cargo build --release
```

产出的二进制：

```
target/release/librevm        命令行
target/release/librevm-gui    图形界面
```

## 安装

```sh
sudo install -m755 target/release/librevm     /usr/local/bin/librevm
sudo install -m755 target/release/librevm-gui /usr/local/bin/librevm-gui

sudo install -m755 scripts/install-gunyah.sh        /usr/local/sbin/install-gunyah
sudo install -m755 scripts/librevm-gui-session      /usr/local/bin/librevm-gui-session
sudo install -m755 scripts/librevm-gui-session-root /usr/local/bin/librevm-gui-session-root
sudo install -m755 scripts/librevm-get-gunyah.sh    /usr/local/libexec/librevm-get-gunyah

sudo install -m644 packaging/librevm.desktop /usr/share/applications/librevm.desktop
sudo install -m644 packaging/60-gunyah.rules /etc/udev/rules.d/60-gunyah.rules
sudo install -m440 packaging/sudoers-librevm-gui /etc/sudoers.d/librevm-gui
```

或者直接：

```sh
sudo ./scripts/install.sh
```

## 用法

```sh
# 命令行
librevm create demo --kernel /var/lib/librevm/kernels/mini-Image
librevm start demo
librevm console demo        # Ctrl-C 断开，客户机继续跑
librevm stop demo

# 图形界面
librevm-gui-session
```

窗口里：左侧选机器，按 **启动**，自动切到控制台，底部输入框打字回车。

## Gunyah 模块

这台设备启动时就已经在 Qualcomm 的 hypervisor 之下，EL2 归它所有。
Linux 通过一组 Gunyah 模块和它的资源管理器通信。

```sh
sudo install-gunyah --status                          # 看现状
sudo install-gunyah --source /path/to/linux-7.2.6     # 从源码编译并安装
sudo install-gunyah --modules /path/to/prebuilt       # 装已编译好的
sudo install-gunyah --uninstall
```

脚本专门处理两个会浪费一整天的坑：

- **vermagic**。这台设备的内核 release 不是上游版本号
  （`7.2.6-piano-gnome-g453bb567f2fc`），装错只会得到一句
  `Exec format error`。脚本逐个校验，不匹配就拒绝并说明原因。
- **BTI**。内核是 `CONFIG_ARM64_BTI_KERNEL=y`，模块必须带
  branch-target landing pads。GCC 默认不生成，结果是第一次调用就
  `Oops - BTI`。脚本编译时强制 `-mbranch-protection=standard`，
  安装前用 `readelf -n` 验证（`objdump -p` 读不到这个 note）。

GUI 第一次启动时会检查 Gunyah 是否可用，缺失就询问是否现在安装。

## 状态放在哪

```
/var/lib/librevm/
├── gui.conf                  界面语言等偏好
├── kernels/                  内核镜像
└── vms/<name>/
    ├── vm.toml               虚拟机定义
    ├── console.log           客户机输出（GUI 显示的就是它）
    └── hub.log               hub 的调试日志，排查问题看这个

/run/librevm/
├── <name>.pid                crosvm 的 pid
└── <name>.console.sock       控制台 socket
```

## 控制台是怎么工作的

crosvm 的 `unix-stream` 串口是**客户端**：它连接到一个已经存在的监听者。
而请求启动的进程会立刻返回，没法持有那个监听。所以有一个 **hub** 进程：
它先监听，再启动 crosvm，然后在客户机和任意多个客户端之间转发字节，
并保留最近 64 KB 供新客户端回放。

这让「关掉窗口、客户机继续跑、随时重新接上」成为可能。

hub 的日志在 `/var/lib/librevm/vms/<name>/hub.log`，排查问题先看它：
它记录了谁连上、转发了多少字节、写成功没有，而 `console.log` 只有客户机的输出。

## 许可

GPL-3.0-or-later。见 [LICENSE](LICENSE)。
