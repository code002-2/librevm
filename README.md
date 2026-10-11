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
| 控制台里的终端仿真（颜色、光标、清屏） | ✅ 可用 |
| 磁盘镜像管理（创建 / 扩容 / 克隆 / 检查） | ✅ 可用 |
| 磁盘接入客户机（virtio-blk） | ✅ 可用 |
| 客户机网络（virtio-net） | ✅ 可用 |
| 共享目录（virtio-fs） | ✅ 可用 |
| D-Bus daemon（GUI 以普通用户运行） | ✅ 可用 |

### virtio 在 Gunyah 上可以用

**crosvm 默认会启用 `virtio-balloon`，而这个设备在本机上会让它收到 `SIGBUS`
（退出码 135）。加上 `--no-balloon`，一切正常。**

librevm 现在默认就带上它。加上之后实测：

* 512 MiB 的 ext4 镜像挂上去，客户机里 `mount` 成功、写入成功、读回成功，
  重启后文件还在；
* virtio-net 设备正常枚举，进程干净退出；
* virtio-rng、xHCI、性能计数器都不影响，也一并关掉了。

早前这个文件里写的是另一套：说是**所有** virtio 设备都不能用、原因是借出的
内存宿主读不了、要改 Qualcomm 的驱动。**那是错的**，判断依据是两个真实的观察
（virtio-net 一枚举就崩、没有驱动的客户机却能启动）推出了一个不成立的结论——
当时的测试里 balloon 一直开着。真正的定位方法是**一次只改一个开关**，而那本该
是第一步。详细记录见 [docs/virtio-on-gunyah.md](docs/virtio-on-gunyah.md)。

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
target/release/librevm-gui    图形界面（以你自己的账号运行）
target/release/librevm-daemon 特权部分，跑在系统总线上
```

## 安装

最简单的是装 deb：

```sh
sudo dpkg -i librevm_0.1.0_arm64.deb
sudo usermod -aG librevm $USER      # 然后注销重新登录
```

或者从源码树装到 `/usr/local`：

```sh
sudo ./scripts/install.sh
```

它会把三个程序、Gunyah 安装脚本、桌面项、udev 规则、D-Bus 策略和
systemd 单元都放好，并把你的账号加进 `librevm` 组。

自己一步步装也行：

```sh
sudo install -m755 target/release/librevm        /usr/local/bin/librevm
sudo install -m755 target/release/librevm-gui    /usr/local/bin/librevm-gui
sudo install -m755 target/release/librevm-daemon /usr/local/bin/librevm-daemon

sudo install -m755 scripts/install-gunyah.sh     /usr/local/sbin/install-gunyah
sudo install -m755 scripts/librevm-gui-session   /usr/local/bin/librevm-gui-session
sudo install -m755 scripts/librevm-get-gunyah.sh /usr/local/libexec/librevm-get-gunyah

sudo install -m644 packaging/librevm.desktop /usr/share/applications/librevm.desktop
sudo install -m644 packaging/60-gunyah.rules /etc/udev/rules.d/60-gunyah.rules
sudo install -m644 packaging/org.librevm.Daemon1.conf \
    /usr/share/dbus-1/system.d/org.librevm.Daemon1.conf
sudo install -m644 packaging/librevm-daemon.service \
    /usr/lib/systemd/system/librevm-daemon.service
sudo systemctl enable --now librevm-daemon
```

### 为什么有 daemon

窗口**以你自己的账号运行**，从不提权。

它需要读写三样属于 root 的东西：`/var/lib/librevm` 里的虚拟机定义、
`/dev/gunyah`，以及启动一个客户机。以前的做法是用 sudo 把**整个 GTK 程序**
提权——那意味着 GTK、它的渲染器、图片解码器，全都在 root 下解析你选的文件。
为了写一个 TOML 和调一次 `kill`，这个代价太大。

现在特权部分在 `librevm-daemon` 里，接口很小，摆在系统总线上，由 D-Bus 策略
限定只有 `librevm` 组够得着。窗口自己：

* **读** store 是直接读的（目录是组可读的，刷新时不必来回问总线）；
* **写和启动**走 daemon。

如果 daemon 不在（没装，或者你就想用 root 跑），窗口会回落到直接调用，
功能一样。日志里会写明它选了哪条路。

把账号加进组：

```sh
sudo usermod -aG librevm $USER
```

然后注销重新登录——组关系是登录时确定的。

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
