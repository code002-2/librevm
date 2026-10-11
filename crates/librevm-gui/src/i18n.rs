//! Translations, and the language the window is drawn in.
//!
//! Deliberately a table in this crate rather than gettext. The build has to work on
//! an offline device with no `msgfmt` and no `libintl` development files, and the
//! number of strings here is small enough that a lookup table is genuinely simpler
//! than the tooling a `.po` workflow needs.
//!
//! Usage is `tr(Key::Start)`, with `Key` an enum so a missing translation is a
//! compile error rather than an English string slipping through at runtime.

use std::sync::atomic::{AtomicU8, Ordering};

/// The languages the window can be drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    En,
    Zh,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Zh => "zh",
        }
    }

    /// The name to show in the language menu, written in that language.
    pub fn endonym(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Zh => "中文",
        }
    }

    pub fn all() -> [Lang; 2] {
        [Lang::En, Lang::Zh]
    }

    pub fn from_code(s: &str) -> Option<Lang> {
        let s = s.trim().to_ascii_lowercase();
        if s.starts_with("zh") {
            Some(Lang::Zh)
        } else if s.starts_with("en") {
            Some(Lang::En)
        } else {
            None
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn current() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Lang::Zh,
        _ => Lang::En,
    }
}

pub fn set_current(lang: Lang) {
    CURRENT.store(
        match lang {
            Lang::En => 0,
            Lang::Zh => 1,
        },
        Ordering::Relaxed,
    );
}

/// Guess a language from the environment, so a Chinese desktop opens in Chinese
/// without anyone touching the menu.
pub fn detect() -> Lang {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(var) {
            if let Some(l) = Lang::from_code(&v) {
                return l;
            }
        }
    }
    Lang::En
}

/// Every piece of text the interface shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    // window and sidebar
    WindowTitle,
    NoMachinesTitle,
    NoMachinesBody,
    CreateVm,
    Refresh,
    Machines,
    ShowConsole,
    Details,
    Language,

    // detail pane
    NothingSelectedTitle,
    NothingSelectedBody,
    Start,
    Stop,
    ShowCommand,
    Console,
    Delete,
    MetricHypervisor,
    MetricMemory,
    MetricGuestMemory,
    MetricCpus,
    MetricKernel,
    MetricConsole,

    // create dialog
    NewVm,
    Name,
    KernelImage,
    MemoryMib,
    SwiotlbMib,
    Vcpus,
    KernelCmdline,
    MemoryProtection,
    ProtectionHint,
    GroupIdentity,
    GroupResources,
    GroupAdvanced,
    Create,
    Cancel,

    // console pane
    SendToGuest,
    NoMachineSelected,
    ConsoleHint,

    // state words
    StateRunning,
    StateStopped,
    StateStale,

    // messages
    CouldNotStart,
    CouldNotStop,
    CouldNotCreate,
    DeleteQuestion,
    CouldNotBuildCommand,
    CrosvmCommand,

    // language names in the menu are endonyms and need no translation
    LanguageHint,

    // first-run check
    GunyahMissingTitle,
    GunyahMissingBody,
    InstallNow,
    Later,
    Recheck,
    GunyahInstalling,
    GunyahStillMissing,
    GunyahNowAvailable,
    ShowDetails,

    // -- disks and shares -------------------------------------------------
    Disks,
    NoDisks,
    AttachDisk,
    CreateDisk,
    CreateDiskBody,
    Detach,
    RootDisk,
    ReadOnly,
    DiskImages,
    DiskPath,
    DiskSize,
    DiskFormat,
    SharedDirs,
    AddShare,
    RestartToApply,
    VirtioWarning,

    // -- the editor -------------------------------------------------------
    EditMachine,
    Save,
    Description,
    Initramfs,
    GroupBoot,
    GroupNetwork,
    GuestNetwork,
    GuestNetworkHint,
    NetworkIndex,
    NetworkIndexHint,
    SwiotlbTooBig,
    SwiotlbTooBigBody,
    CouldNotSave,
    Settings,

    // -- the images page --------------------------------------------------
    Images,
    AvailableImages,
    AvailableImagesHint,
    DeployedImages,
    DeployedImagesHint,
    Deploy,
    Redeploy,
    DeployImage,
    ImageName,
    AttachTo,
    Source,
    DeployNote,
    NoImagesYet,
    NoDaemon,
    MirrorDefault,
    DeploySsh,
    SshDeployHint,
}

impl Key {
    fn en(self) -> &'static str {
        match self {
            Key::WindowTitle => "librevm",
            Key::NoMachinesTitle => "No machines yet",
            Key::NoMachinesBody => "Use the + button to create one.",
            Key::CreateVm => "Create a virtual machine",
            Key::Machines => "Machines",
            Key::ShowConsole => "Show the guest console",
            Key::Details => "Details",
            Key::Language => "Language",

            Key::NothingSelectedTitle => "Nothing selected",
            Key::NothingSelectedBody => "Pick a machine from the sidebar, or create one.",
            Key::Start => "Start",
            Key::Stop => "Stop",
            Key::ShowCommand => "Show command",
            Key::Console => "Console",
            Key::Delete => "Delete",
            Key::MetricHypervisor => "Hypervisor",
            Key::MetricMemory => "Total memory",
            Key::MetricGuestMemory => "Guest memory",
            Key::MetricCpus => "vCPUs",
            Key::MetricKernel => "Kernel",
            Key::MetricConsole => "Console",

            Key::NewVm => "New Virtual Machine",
            Key::Name => "Name",
            Key::KernelImage => "Kernel image",
            Key::MemoryMib => "Memory (MiB)",
            Key::SwiotlbMib => "swiotlb (MiB)",
            Key::Vcpus => "vCPUs",
            Key::KernelCmdline => "Kernel command line",
            Key::MemoryProtection => "Memory protection",
            Key::ProtectionHint => {
                "On Gunyah only the lend path is accepted by the resource manager"
            }
            Key::GroupIdentity => "Identity",
            Key::GroupResources => "Resources",
            Key::GroupAdvanced => "Advanced",
            Key::Create => "Create",
            Key::Cancel => "Cancel",

            Key::SendToGuest => "Send to the guest",
            Key::NoMachineSelected => "no machine selected",
            Key::ConsoleHint => "Type a command and press Enter.",

            Key::StateRunning => "running",
            Key::StateStopped => "stopped",
            Key::StateStale => "stale",

            Key::CouldNotStart => "Could not start",
            Key::CouldNotStop => "Could not stop",
            Key::CouldNotCreate => "Could not create the machine",
            Key::DeleteQuestion => "Delete",
            Key::CouldNotBuildCommand => "could not build the command",
            Key::CrosvmCommand => "crosvm command",

            Key::LanguageHint => "Applies to the whole window immediately",

            Key::GunyahMissingTitle => "Gunyah is not available",
            Key::GunyahMissingBody => {
                "This machine boots under Qualcomm's hypervisor, but the modules \
                 that talk to its resource manager are not loaded, so no virtual \
                 machine can start.\n\n\
                 librevm can fetch the installer and let it set them up. It needs \
                 the kernel tree this system was built from, or modules already \
                 built for it."
            }
            Key::InstallNow => "Install Gunyah",
            Key::Later => "Later",
            Key::Recheck => "Check again",
            Key::GunyahInstalling => "Installing Gunyah",
            Key::GunyahStillMissing => {
                "Gunyah is still not available. The installer's output is below."
            }
            Key::GunyahNowAvailable => "Gunyah is now available. Machines can be started.",
            Key::ShowDetails => "Details",

            Key::Disks => "Disks",
            Key::NoDisks => "No disks",
            Key::AttachDisk => "Attach",
            Key::CreateDisk => "New",
            Key::CreateDiskBody => {
                "Created sparse: an empty 4 GiB image occupies almost nothing."
            }
            Key::Detach => "Detach",
            Key::RootDisk => "root",
            Key::ReadOnly => "ro",
            Key::DiskImages => "Images",
            Key::DiskPath => "Path",
            Key::DiskSize => "Size",
            Key::DiskFormat => "Format",
            Key::SharedDirs => "Shares",
            Key::AddShare => "Add",
            Key::RestartToApply => "Restart to apply.",
            Key::VirtioWarning => {
                "virtio needs --no-balloon, which librevm passes by default."
            }

            Key::EditMachine => "Settings",
            Key::Save => "Save",
            Key::Description => "Description",
            Key::Initramfs => "Initramfs",
            Key::GroupBoot => "Boot",
            Key::GroupNetwork => "Network",
            Key::GuestNetwork => "Network",
            Key::GuestNetworkHint => {
                "Address on the host's subnet, forwarded by the host. Not NAT."
            }
            Key::NetworkIndex => "Index",
            Key::NetworkIndexHint => "Added to .201. Must differ per machine.",
            Key::SwiotlbTooBig => "swiotlb >= memory",
            Key::SwiotlbTooBigBody => "crosvm computes memory - swiotlb and panics.",
            Key::CouldNotSave => "Could not save",
            Key::Settings => "Settings",

            Key::Images => "Images",
            Key::AvailableImages => "Available",
            Key::AvailableImagesHint => {
                "Fetched from mirrors in mainland China. Deploying downloads one and turns \
                 it into a bootable disk."
            }
            Key::DeployedImages => "Deployed",
            Key::DeployedImagesHint => "Disks made from an image.",
            Key::Deploy => "Deploy",
            Key::Redeploy => "Deploy again",
            Key::DeployImage => "Deploy an image",
            Key::ImageName => "Name",
            Key::AttachTo => "Attach to",
            Key::Source => "Source",
            Key::DeployNote => {
                "The download happens on the daemon, so the window will not respond until \
                 it finishes. The SSH key is installed as the image is built."
            }
            Key::NoImagesYet => "Nothing deployed yet.",
            Key::NoDaemon => "no daemon on the bus",
            Key::MirrorDefault => "the catalogue's own mirrors",
            Key::DeploySsh => "Set up SSH",
            Key::SshDeployHint => {
                "Installs the key and a first-boot script that starts sshd. Needs no rebuild."
            }
            Key::Refresh => "Refresh",
        }
    }

    fn zh(self) -> &'static str {
        match self {
            Key::WindowTitle => "librevm",
            Key::NoMachinesTitle => "还没有虚拟机",
            Key::NoMachinesBody => "点上面的 + 号创建一个。",
            Key::CreateVm => "创建虚拟机",
            Key::Refresh => "刷新",
            Key::Machines => "虚拟机",
            Key::ShowConsole => "显示客户机控制台",
            Key::Details => "详情",
            Key::Language => "语言",

            Key::NothingSelectedTitle => "未选择",
            Key::NothingSelectedBody => "从左侧选一台虚拟机，或者新建一台。",
            Key::Start => "启动",
            Key::Stop => "停止",
            Key::ShowCommand => "查看命令",
            Key::Console => "控制台",
            Key::Delete => "删除",
            Key::MetricHypervisor => "Hypervisor",
            Key::MetricMemory => "总内存",
            Key::MetricGuestMemory => "客户机内存",
            Key::MetricCpus => "虚拟 CPU",
            Key::MetricKernel => "内核",
            Key::MetricConsole => "控制台",

            Key::NewVm => "新建虚拟机",
            Key::Name => "名称",
            Key::KernelImage => "内核镜像",
            Key::MemoryMib => "内存 (MiB)",
            Key::SwiotlbMib => "swiotlb (MiB)",
            Key::Vcpus => "虚拟 CPU",
            Key::KernelCmdline => "内核命令行",
            Key::MemoryProtection => "内存保护",
            Key::ProtectionHint => "在 Gunyah 上，资源管理器只接受 lend 方式",
            Key::GroupIdentity => "标识",
            Key::GroupResources => "资源",
            Key::GroupAdvanced => "高级",
            Key::Create => "创建",
            Key::Cancel => "取消",

            Key::SendToGuest => "发送到客户机",
            Key::NoMachineSelected => "未选择虚拟机",
            Key::ConsoleHint => "输入命令后按回车。",

            Key::StateRunning => "运行中",
            Key::StateStopped => "已停止",
            Key::StateStale => "残留",

            Key::CouldNotStart => "无法启动",
            Key::CouldNotStop => "无法停止",
            Key::CouldNotCreate => "无法创建虚拟机",
            Key::DeleteQuestion => "删除",
            Key::CouldNotBuildCommand => "无法生成命令",
            Key::CrosvmCommand => "crosvm 命令",

            Key::LanguageHint => "立即对整个窗口生效",

            Key::GunyahMissingTitle => "未检测到 Gunyah",
            Key::GunyahMissingBody => {
                "这台设备运行在 Qualcomm 的 hypervisor 之下，但和它的资源管理器\
                 通信所需的内核模块没有加载，因此无法启动任何虚拟机。\n\n\
                 librevm 可以拉取安装脚本并执行它。安装需要【这个系统所用的\
                 内核源码树】，或者【已经为该内核编译好的模块】。"
            }
            Key::InstallNow => "安装 Gunyah",
            Key::Later => "稍后",
            Key::Recheck => "重新检查",
            Key::GunyahInstalling => "正在安装 Gunyah",
            Key::GunyahStillMissing => "Gunyah 仍然不可用。下面是安装脚本的输出。",
            Key::GunyahNowAvailable => "Gunyah 现在可用了，可以启动虚拟机了。",
            Key::ShowDetails => "详情",

            Key::Disks => "磁盘",
            Key::NoDisks => "无磁盘",
            Key::AttachDisk => "挂载",
            Key::CreateDisk => "新建",
            Key::CreateDiskBody => "稀疏创建：空的 4 GiB 镜像几乎不占空间。",
            Key::Detach => "卸载",
            Key::RootDisk => "根",
            Key::ReadOnly => "只读",
            Key::DiskImages => "镜像",
            Key::DiskPath => "路径",
            Key::DiskSize => "容量",
            Key::DiskFormat => "格式",
            Key::SharedDirs => "共享",
            Key::AddShare => "添加",
            Key::RestartToApply => "重启后生效。",
            Key::VirtioWarning => {
                "virtio 需要 --no-balloon，librevm 已默认开启。"
            }

            Key::EditMachine => "虚拟机设置",
            Key::Save => "保存",
            Key::Description => "描述",
            Key::Initramfs => "initramfs",
            Key::GroupBoot => "启动",
            Key::GroupNetwork => "网络",
            Key::GuestNetwork => "网络",
            Key::GuestNetworkHint => "用宿主网段的地址，由宿主转发。非 NAT。",
            Key::NetworkIndex => "序号",
            Key::NetworkIndexHint => "加在 .201 上，各虚拟机不能相同。",
            Key::SwiotlbTooBig => "swiotlb >= 内存",
            Key::SwiotlbTooBigBody => "crosvm 用「内存 − swiotlb」算，减出负数会崩。",
            Key::CouldNotSave => "保存失败",
            Key::Settings => "设置",

            Key::Images => "镜像",
            Key::AvailableImages => "可获取",
            Key::AvailableImagesHint => "从国内镜像站拉取。部署会下载并做成可启动的磁盘。",
            Key::DeployedImages => "已部署",
            Key::DeployedImagesHint => "由镜像做成的磁盘。",
            Key::Deploy => "部署",
            Key::Redeploy => "重新部署",
            Key::DeployImage => "部署镜像",
            Key::ImageName => "名称",
            Key::AttachTo => "挂到",
            Key::Source => "来源",
            Key::DeployNote => {
                "下载在 daemon 上进行，期间窗口不会响应。\n\
                 构建镜像时会把 SSH 公钥一并装进去。"
            }
            Key::NoImagesYet => "还没有部署过。",
            Key::NoDaemon => "总线上没有 daemon",
            Key::MirrorDefault => "目录自带的镜像站",
            Key::DeploySsh => "部署 SSH",
            Key::SshDeployHint => "写入公钥和首启脚本（会启动 sshd），无需重建镜像。",
            Key::Refresh => "刷新",
        }
    }
}

/// Look up a string in the current language.
pub fn tr(key: Key) -> &'static str {
    match current() {
        Lang::En => key.en(),
        Lang::Zh => key.zh(),
    }
}

/// "4 MiB down, 1024 MiB disk".
pub fn tr_image_row(download_mib: u64, disk_mib: u64) -> String {
    match current() {
        Lang::En => format!("{download_mib} MiB download, {disk_mib} MiB disk"),
        Lang::Zh => format!("下载 {download_mib} MiB，磁盘 {disk_mib} MiB"),
    }
}

/// "1024 MiB, 42 MiB on disk".
pub fn tr_image_usage(bytes: u64, on_disk: u64) -> String {
    let mib = |b: u64| b as f64 / 1048576.0;
    match current() {
        Lang::En => format!("{:.0} MiB, {:.0} MiB on disk", mib(bytes), mib(on_disk)),
        Lang::Zh => format!("{:.0} MiB，占盘 {:.0} MiB", mib(bytes), mib(on_disk)),
    }
}

pub fn tr_images_status(available: usize, deployed: usize, mirror: &str) -> String {
    match current() {
        Lang::En => format!("{available} available, {deployed} deployed. Mirror: {mirror}"),
        Lang::Zh => format!("可获取 {available} 个，已部署 {deployed} 个。镜像站：{mirror}"),
    }
}

pub fn tr_deploying(name: &str) -> String {
    match current() {
        Lang::En => format!("downloading and building {name}..."),
        Lang::Zh => format!("正在下载并构建 {name}…"),
    }
}

pub fn tr_deployed(path: &str, attached: bool, machine: &str) -> String {
    match current() {
        Lang::En if attached => format!("built {path}, attached to {machine}"),
        Lang::Zh if attached => format!("已构建 {path}，已挂到 {machine}"),
        Lang::En => format!("built {path}"),
        Lang::Zh => format!("已构建 {path}"),
    }
}

pub fn tr_deploy_failed(why: &str) -> String {
    match current() {
        Lang::En => format!("failed: {why}"),
        Lang::Zh => format!("失败：{why}"),
    }
}

pub fn tr_deploying_ssh(name: &str) -> String {
    match current() {
        Lang::En => format!("putting the key into {name}..."),
        Lang::Zh => format!("正在写入公钥到 {name}…"),
    }
}

pub fn tr_ssh_deployed(name: &str, notes: &str) -> String {
    match current() {
        Lang::En => format!("{name}: {notes}"),
        Lang::Zh => format!("{name}：{notes}"),
    }
}

// A few strings need values spliced in. Rather than a format-string table these are
// built here, so the word order stays under the translator's control.

pub fn tr_running_pid(pid: i32) -> String {
    match current() {
        Lang::En => format!("running, pid {pid}"),
        Lang::Zh => format!("运行中，pid {pid}"),
    }
}

pub fn tr_stale_pid(pid: i32) -> String {
    match current() {
        Lang::En => format!("stale, pid {pid}"),
        Lang::Zh => format!("残留，pid {pid}"),
    }
}

pub fn tr_bytes_logged(bytes: u64) -> String {
    match current() {
        Lang::En => format!("{bytes} bytes logged"),
        Lang::Zh => format!("已记录 {bytes} 字节"),
    }
}

pub fn tr_waiting_for_output(name: &str) -> String {
    match current() {
        Lang::En => format!("{name}: waiting for output"),
        Lang::Zh => format!("{name}：等待输出"),
    }
}

pub fn tr_console_status(name: &str, word: &str, bytes: u64, following: bool) -> String {
    let follow = match (current(), following) {
        (Lang::En, true) => "following",
        (Lang::En, false) => "not following",
        (Lang::Zh, true) => "正在跟随",
        (Lang::Zh, false) => "未跟随",
    };
    match current() {
        Lang::En => format!("{name}: {word} - {} - {follow}", tr_bytes_logged(bytes)),
        Lang::Zh => format!("{name}：{word} — {} — {follow}", tr_bytes_logged(bytes)),
    }
}

pub fn tr_delete_question(name: &str) -> String {
    match current() {
        Lang::En => format!("Delete {name}?"),
        Lang::Zh => format!("删除 {name}？"),
    }
}

pub fn tr_delete_body() -> &'static str {
    match current() {
        Lang::En => "The definition is removed. A running guest is stopped first.",
        Lang::Zh => "会删掉虚拟机定义；如果正在运行，会先停止它。",
    }
}

pub fn tr_mib(value: u64) -> String {
    match current() {
        Lang::En => format!("{value} MiB"),
        Lang::Zh => format!("{value} MiB"),
    }
}

pub fn tr_create_hint(memory: u64, swiotlb: u64, guest: u64) -> String {
    match current() {
        Lang::En => format!(
            "{memory} MiB total minus {swiotlb} MiB of swiotlb leaves {guest} MiB for the guest"
        ),
        Lang::Zh => format!(
            "总共 {memory} MiB，减去 {swiotlb} MiB 的 swiotlb，客户机可用 {guest} MiB"
        ),
    }
}

pub fn tr_protection_name(name: &str) -> String {
    // the config stores stable identifiers; show them translated
    match (current(), name) {
        (Lang::Zh, "WithoutFirmware") => "无固件（lend）".to_string(),
        (Lang::Zh, "WithFirmware") => "带固件".to_string(),
        (Lang::Zh, "None") => "无".to_string(),
        _ => name.to_string(),
    }
}

/// Where the chosen language is remembered.
///
/// Kept beside the VM definitions so it travels with the rest of librevm's state,
/// and written world-readable so it also works when the GUI hands over to root.
fn config_path() -> std::path::PathBuf {
    let root = std::env::var("LIBREVM_ROOT").unwrap_or_else(|_| "/var/lib/librevm".to_string());
    std::path::Path::new(&root).join("gui.conf")
}

/// Remember the language, ignoring failure: not being able to persist a preference
/// is never a reason to interrupt the operator.
pub fn save(lang: Lang) {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = format!("# librevm GUI preferences\nlanguage = \"{}\"\n", lang.code());
    if std::fs::write(&path, text).is_ok() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666));
    }
}

/// Load the remembered language, if there is one.
pub fn load() -> Option<Lang> {
    let text = std::fs::read_to_string(config_path()).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("language") {
            let value = rest.trim_start_matches([' ', '=']).trim().trim_matches('"');
            if let Some(l) = Lang::from_code(value) {
                return Some(l);
            }
        }
    }
    None
}

/// The language to start in: the saved choice, else the environment, else English.
pub fn initial() -> Lang {
    load().unwrap_or_else(detect)
}
