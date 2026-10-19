use crate::{
    config::{self, Settings},
    context::{self, Context, DeviceFile},
    daemon,
    device::{self, Claim, DeviceAction, DeviceSeed},
    engine, identity,
    paths::{self, Layout},
    platform,
    report::Reporter,
    state::SyncList,
    transport, webdav,
};
use anyhow::{Context as _, Result, bail};
use eframe::egui;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::Duration,
};
// macOS 的后台代理状态快照用；其他平台没有这段，导入会变成 unused。
#[cfg(target_os = "macos")]
use std::sync::Mutex;
// 服务状态轮询：Windows 查 SCM，macOS 查 launchd 是否加载了代理。
// 不单独 cfg 会让不含该逻辑的平台报 unused。
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::time::Instant;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const APP_TITLE: &str = concat!("RIME 用户词库同步工具 v", env!("CARGO_PKG_VERSION"));
const GITHUB_URL: &str = "https://github.com/small32/RimeUserDictSync";
const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/small32/RimeUserDictSync/releases/latest";
const CONFIG_BROKEN_NOTICE: &str =
    "配置文件读取失败，为避免覆盖原有配置已禁用保存；请手动修复或删除配置文件后重启程序。";
/// 后台服务/代理状态的轮询间隔。
///
/// Windows 查 SCM 很便宜，5 秒足够跟上提权安装的节奏；
/// macOS 每次要 fork 一个 launchctl，放宽到 30 秒，避免无谓的进程创建。
#[cfg(target_os = "windows")]
const SERVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);
#[cfg(target_os = "macos")]
const SERVICE_POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct UpdateInfo {
    version: String,
    url: String,
}

enum Event {
    Log(String),
    Progress(u8),
    /// 手动「同步词库」结束。
    Manual(Result<String, String>),
    /// 后台轮次结束。
    Round(Result<String, String>),
    Tested(Result<String, String>),
    /// 设备设置（抢占 / 接管 / 注销 / 导入）结束。
    Device(Result<String, String>),
    UpdateChecked(Option<UpdateInfo>),
}

struct ChannelReporter {
    tx: Sender<Event>,
}

impl Reporter for ChannelReporter {
    fn log(&self, text: &str) {
        let _ = self.tx.send(Event::Log(crate::report::timestamped(text)));
    }
    fn progress(&self, value: u8) {
        let _ = self.tx.send(Event::Progress(value));
    }
}

pub fn run() -> eframe::Result {
    let icon = load_icon();
    let min_width = if cfg!(target_os = "macos") {
        880.
    } else {
        780.
    };
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([880., 600.])
        .with_min_inner_size([min_width, 460.])
        .with_title(APP_TITLE);
    let viewport = if let Some(icon) = icon {
        viewport.with_icon(icon)
    } else {
        viewport
    };
    eframe::run_native(
        APP_TITLE,
        eframe::NativeOptions {
            viewport,
            ..Default::default()
        },
        Box::new(|cc| {
            set_fonts(&cc.egui_ctx);
            // 程序刚换过版本时，launchd 里记着的还是旧二进制的签名约束，
            // 后台任务会被系统按「Launch Constraint Violation」杀掉。
            // 放进后台线程，别拖慢首帧。
            #[cfg(target_os = "macos")]
            std::thread::spawn(crate::agent::heal_after_update);
            Ok(Box::new(App::new()))
        }),
    )
}

fn load_icon() -> Option<Arc<egui::IconData>> {
    #[cfg(target_os = "macos")]
    let bytes = include_bytes!("../macos/AppIcon.png");
    #[cfg(not(target_os = "macos"))]
    let bytes = include_bytes!("../weasel.ico");
    let image = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = image.dimensions();
    Some(Arc::new(egui::IconData {
        rgba: image.into_raw(),
        width: w,
        height: h,
    }))
}

fn set_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates: Vec<PathBuf> = if cfg!(windows) {
        vec![
            PathBuf::from(r"C:\Windows\Fonts\msyh.ttc"),
            PathBuf::from(r"C:\Windows\Fonts\simhei.ttf"),
        ]
    } else if cfg!(target_os = "macos") {
        vec![
            PathBuf::from("/System/Library/Fonts/Hiragino Sans GB.ttc"),
            PathBuf::from("/System/Library/Fonts/STHeiti Light.ttc"),
            PathBuf::from("/System/Library/Fonts/STHeiti Medium.ttc"),
            PathBuf::from("/System/Library/Fonts/PingFang.ttc"),
        ]
    } else {
        vec![PathBuf::from(
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        )]
    };
    if let Some((_, bytes)) = candidates
        .into_iter()
        .find_map(|path| fs::read(&path).ok().map(|bytes| (path, bytes)))
    {
        let font = egui::FontData::from_owned(bytes).tweak(egui::FontTweak {
            y_offset: 1.5,
            ..Default::default()
        });
        fonts.font_data.insert("cjk".into(), Arc::new(font));
        fonts
            .families
            .get_mut(&egui::FontFamily::Proportional)
            .unwrap()
            .insert(0, "cjk".into());
        fonts
            .families
            .get_mut(&egui::FontFamily::Monospace)
            .unwrap()
            .insert(0, "cjk".into());
    }
    ctx.set_fonts(fonts);
}

struct App {
    base: PathBuf,
    ini: PathBuf,
    log_path: PathBuf,
    settings: Settings,
    context: Option<Context>,
    context_error: Option<String>,
    logs: String,
    /// 已经显示过的日志字节数；界面靠它增量读取 `RimeSync.log`。
    ///
    /// 后台服务写的是同一个文件，所以日志窗口必须从文件读，
    /// 而不是只维护"本进程写了什么"。
    log_offset: u64,
    progress: u8,
    busy: bool,
    busy_flag: Arc<AtomicBool>,
    /// 后台服务是否在跑。服务接管搬运后，内建线程就让位，避免重复劳动。
    service_running: Arc<AtomicBool>,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    service_text: String,
    /// macOS：后台线程定期查 launchd，UI 只读快照。
    ///
    /// 直接在 UI 线程调 `agent::status()` 会 spawn `id` + 两次 `launchctl print`，
    /// 每 5 秒卡一下界面。
    #[cfg(target_os = "macos")]
    agent_snapshot: Arc<Mutex<AgentSnapshot>>,
    #[cfg(target_os = "windows")]
    shortcut_text: String,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    service_checked_at: Instant,
    config_broken: bool,

    show_webdav: bool,
    show_device: bool,
    show_sync_files: bool,
    show_about: bool,
    #[cfg(target_os = "macos")]
    show_config_files: bool,
    #[cfg(target_os = "macos")]
    config_dir_draft: String,

    dav_url: String,
    dav_username: String,
    dav_password: String,
    dav_password_visible: bool,
    capability_text: String,

    device_id_draft: String,
    device_password_draft: String,
    device_password_confirm: String,
    device_skip_password: bool,
    device_confirm_takeover: bool,
    device_info_text: String,
    exported_text: String,

    sync_files_draft: Vec<String>,

    cancel: Arc<AtomicBool>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    notice: Option<String>,
    available_update: Option<UpdateInfo>,
}

/// macOS 后台代理状态快照（由后台线程刷新，UI 只读）。
#[cfg(target_os = "macos")]
#[derive(Default)]
struct AgentSnapshot {
    text: String,
    /// 定时搬运任务是否已被 launchd 加载。
    loaded: bool,
}

#[cfg(target_os = "macos")]
fn spawn_agent_poller() -> Arc<Mutex<AgentSnapshot>> {
    let snapshot = Arc::new(Mutex::new(AgentSnapshot::default()));
    let shared = snapshot.clone();
    std::thread::spawn(move || {
        loop {
            let (text, loaded) = crate::agent::status();
            if let Ok(mut guard) = shared.lock() {
                guard.text = text;
                guard.loaded = loaded;
            }
            std::thread::sleep(SERVICE_POLL_INTERVAL);
        }
    });
    snapshot
}

impl App {
    fn new() -> Self {
        let base = config::app_dir().unwrap_or_else(|_| PathBuf::from("."));
        let ini = base.join("RimeUserDictSync.ini");
        let legacy_ini = base.join("WeaselUserDictSync.ini");
        if !ini.exists() && legacy_ini.is_file() && fs::rename(&legacy_ini, &ini).is_err() {
            let _ = fs::copy(&legacy_ini, &ini);
        }
        let log_path = base.join("RimeSync.log");
        let mut logs = fs::read_to_string(&log_path).unwrap_or_default();
        let (settings, config_broken) = match Settings::load(&ini) {
            Ok(settings) => (settings, false),
            Err(error) => {
                let line = crate::report::timestamped(&format!(
                    "警告: 读取配置文件失败，已显示默认空配置且禁用保存，请修复或删除 {}: {error}",
                    ini.display()
                ));
                logs.push_str(&line);
                logs.push('\n');
                if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) {
                    let _ = writeln!(file, "{line}");
                }
                (Settings::default(), true)
            }
        };
        let (tx, rx) = mpsc::channel();
        check_for_update(tx.clone());
        let busy_flag = Arc::new(AtomicBool::new(false));
        let service_running = Arc::new(AtomicBool::new(false));
        spawn_background(
            base.clone(),
            ini.clone(),
            tx.clone(),
            busy_flag.clone(),
            service_running.clone(),
        );
        #[cfg(target_os = "macos")]
        let agent_snapshot = spawn_agent_poller();

        let mut app = Self {
            base,
            ini,
            log_path,
            dav_url: settings.webdav_url.clone(),
            dav_username: settings.username.clone(),
            dav_password: settings.password.clone(),
            settings,
            context: None,
            context_error: None,
            log_offset: logs.len() as u64,
            logs,
            progress: 0,
            busy: false,
            busy_flag,
            service_running,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            service_text: String::new(),
            #[cfg(target_os = "macos")]
            agent_snapshot,
            #[cfg(target_os = "windows")]
            shortcut_text: String::new(),
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            service_checked_at: Instant::now() - SERVICE_POLL_INTERVAL,
            config_broken,
            show_webdav: false,
            show_device: false,
            show_sync_files: false,
            show_about: false,
            #[cfg(target_os = "macos")]
            show_config_files: false,
            #[cfg(target_os = "macos")]
            config_dir_draft: String::new(),
            dav_password_visible: false,
            capability_text: String::new(),
            device_id_draft: String::new(),
            device_password_draft: String::new(),
            device_password_confirm: String::new(),
            device_skip_password: false,
            device_confirm_takeover: false,
            device_info_text: String::new(),
            exported_text: String::new(),
            sync_files_draft: Vec::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            tx,
            rx,
            notice: None,
            available_update: None,
        };
        app.startup();
        Self::trim_display(&mut app.logs);
        app
    }

    /// 启动时：迁移旧布局（`Sync/WebDAV/`、`webdav-sync.zip`），然后构建上下文并记录提示。
    fn startup(&mut self) {
        let ini = self.ini.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let reporter = ChannelReporter { tx: tx.clone() };
            let settings = Settings::load(&ini).unwrap_or_default();
            let rime_dir = context::resolve_rime_dir(&settings);
            let existing_id =
                config::yaml_scalar_opt(&rime_dir.join("installation.yaml"), "installation_id")
                    .filter(|id| paths::validate_installation_id(id).is_ok());
            let dav = build_webdav(&settings).ok().flatten();
            if let Some(id) = existing_id {
                let layout = Layout::new(&rime_dir, &id);
                let _ = layout.ensure_dirs();
                // 迁移会读写 Sync/，必须和后台上传/手动同步共用同一把锁。
                match crate::state::LocalLock::acquire(&layout.lock_file()) {
                    Ok(_lock) => {
                        match context::migrate_legacy_layout(&layout, dav.as_ref(), &|line| {
                            reporter.log(line)
                        }) {
                            Ok(notices) => {
                                for notice in notices {
                                    reporter.log(&notice);
                                }
                            }
                            Err(error) => reporter.log(&format!("旧版布局迁移失败：{error}")),
                        }
                    }
                    Err(error) => {
                        reporter.log(&format!("跳过旧版布局迁移（{error}），下次启动会重试。"))
                    }
                }
            }
        });
        self.refresh_context();
    }

    /// 重新构建同步上下文（保存 WebDAV / 设备设置后调用）。
    fn refresh_context(&mut self) {
        match build_context(&self.base, &self.ini) {
            Ok(context) => {
                let notices = context.notices().to_vec();
                self.device_id_draft = context.device.installation_id.clone();
                self.context = Some(context);
                self.context_error = None;
                for notice in notices {
                    self.append(&crate::report::timestamped(&notice));
                }
            }
            Err(error) => {
                self.context = None;
                self.context_error = Some(error.to_string());
            }
        }
    }

    /// 追加一行日志。
    ///
    /// 界面不再自己维护"显示缓冲"，而是统一从 `RimeSync.log` 增量读取：
    /// 后台服务写的是同一个文件，这样服务的搬运动作也会出现在窗口里。
    fn append(&mut self, line: &str) {
        let written = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .and_then(|mut file| writeln!(file, "{line}"))
            .is_ok();
        if written {
            self.tail_log();
            return;
        }
        // 写不了文件（权限、被杀软锁住等）时，至少让界面看得到这一行。
        self.logs.push_str(line);
        self.logs.push('\n');
        Self::trim_display(&mut self.logs);
    }

    /// 从上次读到的位置继续读日志，只消费到最后一个换行，
    /// 避免把别的进程正在写的半行读进来。
    fn tail_log(&mut self) {
        let Ok(metadata) = fs::metadata(&self.log_path) else {
            return;
        };
        let length = metadata.len();
        if length < self.log_offset {
            // 文件被截断或轮转过了
            self.log_offset = 0;
        }
        if length == self.log_offset {
            return;
        }
        let Ok(mut file) = fs::File::open(&self.log_path) else {
            return;
        };
        if file.seek(SeekFrom::Start(self.log_offset)).is_err() {
            return;
        }
        let mut buffer = Vec::new();
        if file.read_to_end(&mut buffer).is_err() {
            return;
        }
        let Some(cut) = buffer.iter().rposition(|byte| *byte == b'\n') else {
            return;
        };
        let consumed = cut + 1;
        self.log_offset += consumed as u64;
        let text = String::from_utf8_lossy(&buffer[..consumed]).into_owned();
        self.logs.push_str(&text);
        Self::trim_display(&mut self.logs);
    }

    /// 只保留末尾一段，避免长时间运行后界面缓冲无限增长。
    /// 切点一定取在换行之后，所以不会切坏 UTF-8。
    fn trim_display(logs: &mut String) {
        const MAX_BYTES: usize = 512 * 1024;
        if logs.len() <= MAX_BYTES {
            return;
        }
        let cut = logs.len() - MAX_BYTES;
        if let Some(position) = logs
            .as_bytes()
            .get(cut..)
            .and_then(|tail| tail.iter().position(|byte| *byte == b'\n'))
        {
            logs.drain(..cut + position + 1);
        }
    }

    fn poll(&mut self) {
        // 服务往同一个日志文件里写，这里每帧顺带把新增行捞进窗口。
        self.tail_log();
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Log(text) => self.append(&text),
                Event::Progress(value) => self.progress = value,
                Event::Manual(result) => {
                    self.set_busy(false);
                    self.notice = Some(match result {
                        Ok(message) => message,
                        Err(error) => {
                            self.append(&crate::report::timestamped(&format!("失败: {error}")));
                            error
                        }
                    });
                    self.refresh_context();
                }
                Event::Round(result) => match result {
                    Ok(message) => self.append(&crate::report::timestamped(&message)),
                    Err(error) => self.append(&crate::report::timestamped(&format!(
                        "后台搬运未完成: {error}"
                    ))),
                },
                Event::Tested(result) => {
                    self.set_busy(false);
                    match result {
                        Ok(text) => {
                            self.capability_text = text.clone();
                            self.notice = Some(format!("连接测试通过（含写入测试）。\n{text}"));
                        }
                        Err(error) => self.notice = Some(format!("测试失败：{error}")),
                    }
                }
                Event::Device(result) => {
                    self.set_busy(false);
                    self.notice = Some(match result {
                        Ok(message) => message,
                        Err(error) => format!("设备设置失败：{error}"),
                    });
                    self.refresh_context();
                }
                Event::UpdateChecked(update) => self.available_update = update,
            }
        }
        self.refresh_service_state();
    }

    /// 每 5 秒问一次 SCM：服务装没装、跑没跑。
    #[cfg(target_os = "windows")]
    fn refresh_service_state(&mut self) {
        if self.service_checked_at.elapsed() < SERVICE_POLL_INTERVAL {
            return;
        }
        self.service_checked_at = Instant::now();
        let text = crate::service::describe();
        let running = crate::service::status() == crate::service::Status::Running;
        // 状态变化才写日志，避免每 5 秒刷一行。
        if !self.service_text.is_empty() && self.service_text != text {
            self.append(&crate::report::timestamped(&format!(
                "后台服务状态变化：{text}"
            )));
        }
        self.service_text = text;
        self.shortcut_text = crate::shortcut::describe(&self.base);
        self.service_running.store(running, Ordering::Relaxed);
    }

    /// macOS：读后台线程刷新的快照；加载了就让 GUI 内建线程让位。
    #[cfg(target_os = "macos")]
    fn refresh_service_state(&mut self) {
        if self.service_checked_at.elapsed() < SERVICE_POLL_INTERVAL {
            return;
        }
        self.service_checked_at = Instant::now();
        let (text, loaded) = match self.agent_snapshot.lock() {
            Ok(guard) if !guard.text.is_empty() => (guard.text.clone(), guard.loaded),
            // 后台线程还没跑完第一轮，下一帧再读。
            _ => return,
        };
        if !self.service_text.is_empty() && self.service_text != text {
            self.append(&crate::report::timestamped(&format!(
                "后台代理状态变化：{text}"
            )));
        }
        self.service_text = text;
        self.service_running.store(loaded, Ordering::Relaxed);
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    fn refresh_service_state(&mut self) {
        self.service_running.store(false, Ordering::Relaxed);
    }

    /// 请后台服务立刻跑一轮；没装服务时由内建线程消费，效果一样。
    fn request_background_round(&self) {
        if let Some(context) = &self.context
            && let Err(error) = daemon::request_wake(&context.layout.meta)
        {
            let _ = error;
        }
    }

    /// 安装 / 卸载服务需要管理员权限，先尝试直接执行，被拒则弹 UAC 重新拉起自己。
    #[cfg(target_os = "windows")]
    fn run_service_action(&mut self, uninstall: bool) {
        if self.busy {
            return;
        }
        let mut arguments = vec![if uninstall {
            "--uninstall-service".to_owned()
        } else {
            "--install-service".to_owned()
        }];
        arguments.push("--app-dir".to_owned());
        arguments.push(self.base.to_string_lossy().into_owned());
        match crate::service::request_elevated(&arguments) {
            Ok(()) => {
                self.append(&crate::report::timestamped(if uninstall {
                    "已请求管理员权限卸载后台服务。"
                } else {
                    "已请求管理员权限安装后台服务。"
                }));
                self.notice = Some(
                    "请在管理员权限提示（UAC）中确认；完成后本窗口会自动刷新服务状态。".to_owned(),
                );
                // 把检查时间拨回一个周期之前，下一次刷新才会立刻发生
                // （refresh_service_state 在 elapsed() < SERVICE_POLL_INTERVAL 时直接返回）。
                self.service_checked_at = Instant::now() - SERVICE_POLL_INTERVAL;
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    /// 创建 / 删除「同步词库」快捷方式；不需要管理员权限。
    #[cfg(target_os = "windows")]
    fn run_shortcut_action(&mut self, uninstall: bool) {
        let result = if uninstall {
            crate::shortcut::uninstall(&self.base)
        } else {
            crate::shortcut::install(&self.base)
        };
        match result {
            Ok(message) => {
                self.append(&crate::report::timestamped(&message));
                self.notice = Some(message);
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
        self.shortcut_text = crate::shortcut::describe(&self.base);
    }

    /// macOS：生成/删除 AppleScript applet 形式的「同步词库」。
    #[cfg(target_os = "macos")]
    fn run_shortcut_action(&mut self, uninstall: bool) {
        let result = if uninstall {
            crate::shortcut_macos::uninstall(&self.base)
        } else {
            crate::shortcut_macos::install(&self.base)
        };
        match result {
            Ok(message) => {
                self.append(&crate::report::timestamped(&message));
                self.notice = Some(message);
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    /// macOS：安装/卸载 launchd 后台代理。
    #[cfg(target_os = "macos")]
    fn run_agent_action(&mut self, uninstall: bool) {
        let result = if uninstall {
            crate::agent::uninstall()
        } else {
            crate::agent::install(&self.base, self.settings.background_minutes)
        };
        match result {
            Ok(message) => {
                self.append(&crate::report::timestamped(&message));
                self.notice = Some(message);
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
        // 立刻刷新状态，下一帧就能看到「已加载 / 未安装」。
        self.service_checked_at = Instant::now() - SERVICE_POLL_INTERVAL;
        self.refresh_service_state();
    }

    fn set_busy(&mut self, value: bool) {
        self.busy = value;
        self.busy_flag.store(value, Ordering::Relaxed);
    }

    fn start_manual_sync(&mut self) {
        if self.busy {
            return;
        }
        if self.context.is_none() {
            self.notice = Some(format!(
                "无法开始同步：{}",
                self.context_error.clone().unwrap_or_default()
            ));
            return;
        }
        self.set_busy(true);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.progress = 0;
        let base = self.base.clone();
        let ini = self.ini.clone();
        let cancel = self.cancel.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let reporter = ChannelReporter { tx: tx.clone() };
            let result = (|| -> Result<String> {
                let context = build_context(&base, &ini)?;
                let outcome = engine::manual_sync(&context, &cancel, &reporter)?;
                let mut message = format!(
                    "同步完成：合并 {} 个文件，上传 {} 项。",
                    outcome.merged, outcome.uploaded
                );
                if !outcome.conflicts.is_empty() {
                    message.push_str(&format!(
                        "\n有 {} 个文件存在双端修改冲突，远端版本已另存为副本：\n{}",
                        outcome.conflicts.len(),
                        outcome.conflicts.join("\n")
                    ));
                }
                Ok(message)
            })()
            .map_err(|error| error.to_string());
            let _ = tx.send(Event::Manual(result));
        });
    }

    fn test_webdav(&mut self) {
        if self.busy {
            return;
        }
        self.set_busy(true);
        let url = self.dav_url.trim().to_owned();
        let username = self.dav_username.trim().to_owned();
        let password = self.dav_password.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<String> {
                let dav = webdav::WebDav::new(&url, &username, &password)?;
                dav.test_write()?;
                let capabilities = dav.probe_capabilities();
                Ok(format!(
                    "服务端能力：{}\n上传校验档位：{}",
                    capabilities.describe(),
                    capabilities.tier().label()
                ))
            })()
            .map_err(|error: anyhow::Error| error.to_string());
            let _ = tx.send(Event::Tested(result));
        });
    }

    fn save_webdav(&mut self) -> Result<()> {
        if self.config_broken {
            bail!("{CONFIG_BROKEN_NOTICE}");
        }
        self.settings.webdav_url = self.dav_url.trim().to_owned();
        self.settings.username = self.dav_username.trim().to_owned();
        self.settings.password = self.dav_password.clone();
        self.settings.save(&self.ini)?;
        self.append(&crate::report::timestamped("已保存 WebDAV 设置。"));
        self.refresh_context();
        self.request_background_round();
        Ok(())
    }

    fn choose_rime(&mut self) {
        if self.config_broken {
            self.notice = Some(CONFIG_BROKEN_NOTICE.into());
            return;
        }
        let start = if self.settings.user_data_dir.trim().is_empty() {
            platform::default_user_dir()
        } else {
            // 用户可能填 %APPDATA%\Rime 这类环境变量写法，与 resolve_rime_dir 保持一致。
            PathBuf::from(platform::expand_env(&self.settings.user_data_dir))
        };
        let Some(dir) = rfd::FileDialog::new().set_directory(start).pick_folder() else {
            return;
        };
        let installation = dir.join("installation.yaml");
        if !installation.is_file() {
            self.notice = Some(format!(
                "{} 中没有 installation.yaml，请选择真正的 RIME 用户目录。",
                dir.display()
            ));
            return;
        }
        self.settings.user_data_dir = dir.to_string_lossy().into_owned();
        if let Err(error) = self.settings.save(&self.ini) {
            self.notice = Some(error.to_string());
            return;
        }
        // sync_dir 由 device.json 权威接管，这里只提示一次。
        if let Some(id) = config::yaml_scalar_opt(&installation, "installation_id") {
            self.append(&crate::report::timestamped(&format!(
                "已指定 RIME 用户词库，installation_id 读作 {id}；sync_dir 将指向其中的 Sync 目录。"
            )));
        } else {
            self.append(&crate::report::timestamped(
                "已指定 RIME 用户词库；installation.yaml 缺少 installation_id，请在「设备设置」中注册。",
            ));
        }
        self.refresh_context();
    }

    fn open_sync_files(&mut self) {
        let Some(context) = &self.context else {
            self.notice =
                Some("请先完成设备设置，同步列表保存在 RIME/.rime-sync/sync-list.json。".into());
            return;
        };
        match context.load_list() {
            Ok(list) => {
                self.sync_files_draft = list.files.iter().map(|file| file.path.clone()).collect();
                self.show_sync_files = true;
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn add_sync_files(&mut self) {
        let user_dir = context::resolve_rime_dir(&self.settings);
        let Some(files) = rfd::FileDialog::new().set_directory(&user_dir).pick_files() else {
            return;
        };
        let canonical_root = user_dir.canonicalize().unwrap_or(user_dir);
        for file in files {
            let canonical_file = file.canonicalize().unwrap_or(file);
            let Ok(relative) = canonical_file.strip_prefix(&canonical_root) else {
                self.notice = Some("同步文件必须位于当前 RIME 用户词库目录内。".into());
                continue;
            };
            let candidate = relative.to_string_lossy().replace('\\', "/");
            match paths::validate_sync_file_path(&candidate) {
                Ok(relative) => {
                    if relative.to_ascii_lowercase().ends_with(".userdb.txt") {
                        self.notice = Some(
                            "userdb 快照不能加入同步列表：它由 RIME 的 /sync 与 Sync/<ID>/ 负责。"
                                .into(),
                        );
                        continue;
                    }
                    if !self.sync_files_draft.iter().any(|item| item == &relative) {
                        self.sync_files_draft.push(relative);
                    }
                }
                Err(error) => self.notice = Some(error.to_string()),
            }
        }
    }

    fn save_sync_files(&mut self) -> Result<()> {
        let context = self.context.as_ref().context("请先完成设备设置")?;
        let mut list = context.load_list()?;
        let removed: Vec<String> = list
            .files
            .iter()
            .map(|file| file.path.clone())
            .filter(|path| !self.sync_files_draft.contains(path))
            .collect();
        // §6：移除 = 写进本设备排除集，绝不删除 Others/ 里的公共文件。
        for path in removed {
            list.remove(&path);
        }
        let mut fresh = SyncList {
            files: Vec::new(),
            excluded: list.excluded.clone(),
        };
        for path in &self.sync_files_draft {
            fresh.register(path);
        }
        let kept: Vec<String> = fresh.files.iter().map(|file| file.path.clone()).collect();
        fresh.excluded.retain(|item| !kept.contains(item));
        context.save_list(&fresh)?;
        self.append(&crate::report::timestamped(&format!(
            "已保存同步文件选择（{} 项）；移除的文件只影响本设备，Others/ 中的公共副本保持不变。",
            fresh.files.len()
        )));
        self.request_background_round();
        Ok(())
    }

    fn submit_device_action(&mut self, action: DeviceAction) {
        if self.busy {
            return;
        }
        let ini = self.ini.clone();
        let tx = self.tx.clone();
        self.set_busy(true);
        std::thread::spawn(move || {
            let result = run_device_action(&ini, action).map_err(|error| error.to_string());
            let _ = tx.send(Event::Device(result));
        });
    }

    #[cfg(target_os = "macos")]
    fn save_config_dir(&mut self) -> Result<()> {
        if self.config_broken {
            bail!("{CONFIG_BROKEN_NOTICE}");
        }
        let target = PathBuf::from(self.config_dir_draft.trim());
        if !target.is_absolute() {
            bail!("配置目录必须是绝对路径");
        }
        fs::create_dir_all(&target)?;
        let source = self
            .base
            .canonicalize()
            .unwrap_or_else(|_| self.base.clone());
        let target = target.canonicalize()?;
        if target != source && target.starts_with(&source) {
            bail!("配置目录不能放在当前配置目录的子目录中");
        }
        if target != source {
            fs::create_dir_all(target.join("Sync"))?;
            if self.log_path.is_file() {
                fs::copy(&self.log_path, target.join("RimeSync.log"))?;
            }
        }
        let ini = target.join("RimeUserDictSync.ini");
        self.settings.save(&ini)?;
        config::set_app_dir(&target)?;
        self.base = target.clone();
        self.ini = ini;
        self.log_path = target.join("RimeSync.log");
        self.config_dir_draft = target.to_string_lossy().into_owned();
        Ok(())
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.poll();
        ctx.request_repaint_after(Duration::from_millis(200));

        egui::TopBottomPanel::top("status_panel")
            .exact_height(64.)
            .show(ctx, |ui| {
                ui.add_space(8.);
                ui.add(
                    egui::ProgressBar::new(self.progress as f32 / 100.)
                        .show_percentage()
                        .animate(self.busy),
                );
                ui.horizontal(|ui| match (&self.context, &self.context_error) {
                    (Some(context), _) => {
                        ui.label(format!(
                            "设备 ID：{}　共享目录：{}",
                            context.device.installation_id,
                            context.layout.others.display()
                        ));
                    }
                    (None, Some(error)) => {
                        ui.colored_label(egui::Color32::from_rgb(200, 120, 0), error);
                    }
                    _ => {
                        ui.label("尚未配置。");
                    }
                });
            });

        egui::TopBottomPanel::bottom("actions_panel")
            .exact_height(178.)
            .show(ctx, |ui| {
                ui.add_space(6.);
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("指定 RIME 用户词库"))
                        .clicked()
                    {
                        self.choose_rime();
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("WebDAV 设置"))
                        .clicked()
                    {
                        self.show_webdav = true;
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("设备设置"))
                        .clicked()
                    {
                        self.show_device = true;
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("同步文件选择"))
                        .clicked()
                    {
                        self.open_sync_files();
                    }
                    #[cfg(target_os = "macos")]
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("配置文件"))
                        .clicked()
                    {
                        self.config_dir_draft = self.base.to_string_lossy().into_owned();
                        self.show_config_files = true;
                    }
                    if ui.button("关于").clicked() {
                        self.show_about = true;
                    }
                });
                ui.horizontal(|ui| {
                    let mut enabled = self.settings.background_enabled;
                    if ui
                        .add_enabled(
                            !self.busy,
                            egui::Checkbox::new(&mut enabled, "Rime词库同步"),
                        )
                        .changed()
                    {
                        self.settings.background_enabled = enabled;
                        if self.config_broken {
                            self.notice = Some(CONFIG_BROKEN_NOTICE.into());
                        } else {
                            let _ = self.settings.save(&self.ini);
                        }
                        // 开 → 拉起菜单栏图标；关 → 图标立即消失。
                        #[cfg(target_os = "macos")]
                        {
                            let _ = crate::agent::apply_background_switch(enabled);
                            self.service_checked_at = Instant::now() - SERVICE_POLL_INTERVAL;
                        }
                    }
                    ui.add_enabled(
                        !self.busy && self.settings.background_enabled,
                        egui::Slider::new(&mut self.settings.background_minutes, 1..=240)
                            .text("分钟"),
                    );
                    if ui.button("保存间隔").clicked() {
                        if self.config_broken {
                            self.notice = Some(CONFIG_BROKEN_NOTICE.into());
                        } else {
                            let _ = self.settings.save(&self.ini);
                            // macOS 的代理间隔写在 plist 里，改完设置要同步过去。
                            #[cfg(target_os = "macos")]
                            match crate::agent::refresh_interval(
                                &self.base,
                                self.settings.background_minutes,
                            ) {
                                Ok(true) => self.append(&crate::report::timestamped(
                                    "已按新的间隔更新后台代理。",
                                )),
                                Ok(false) => {}
                                Err(error) => self.notice = Some(error.to_string()),
                            }
                            self.request_background_round();
                        }
                    }
                    if ui.button("立刻搬运一轮").clicked() {
                        self.request_background_round();
                    }
                    ui.separator();
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(!self.busy, egui::Button::new("同步词库"))
                            .clicked()
                        {
                            self.start_manual_sync();
                        }
                        if ui
                            .add_enabled(self.busy, egui::Button::new("停止同步"))
                            .clicked()
                        {
                            self.cancel.store(true, Ordering::Relaxed);
                        }
                    });
                });
                #[cfg(target_os = "windows")]
                ui.horizontal(|ui| {
                    if self.service_text.is_empty() {
                        self.refresh_service_state();
                    }
                    ui.label(&self.service_text);
                    let installed =
                        crate::service::status() != crate::service::Status::NotInstalled;
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("安装 / 更新后台服务"))
                        .clicked()
                    {
                        self.run_service_action(false);
                    }
                    if ui
                        .add_enabled(!self.busy && installed, egui::Button::new("卸载后台服务"))
                        .clicked()
                    {
                        self.run_service_action(true);
                    }
                });
                #[cfg(target_os = "windows")]
                ui.horizontal(|ui| {
                    if self.shortcut_text.is_empty() {
                        self.refresh_service_state();
                    }
                    ui.label(&self.shortcut_text);
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("创建 / 更新快捷方式"))
                        .clicked()
                    {
                        self.run_shortcut_action(false);
                    }
                    if ui
                        .add_enabled(
                            !self.busy && crate::shortcut::exists(&self.base),
                            egui::Button::new("删除快捷方式"),
                        )
                        .clicked()
                    {
                        self.run_shortcut_action(true);
                    }
                });
                #[cfg(target_os = "macos")]
                ui.horizontal(|ui| {
                    if self.service_text.is_empty() {
                        self.refresh_service_state();
                    }
                    ui.label(&self.service_text);
                    let installed = crate::agent::installed();
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("开启 / 更新 Rime词库同步"))
                        .clicked()
                    {
                        self.run_agent_action(false);
                    }
                    if ui
                        .add_enabled(
                            !self.busy && installed,
                            egui::Button::new("关闭 Rime词库同步"),
                        )
                        .clicked()
                    {
                        self.run_agent_action(true);
                    }
                });
                #[cfg(target_os = "macos")]
                ui.horizontal(|ui| {
                    let created = crate::shortcut_macos::exists(&self.base);
                    ui.label(if created {
                        "快捷方式：已创建"
                    } else {
                        "快捷方式：未创建"
                    })
                    .on_hover_text(crate::shortcut_macos::describe(&self.base));
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("创建 / 更新快捷方式"))
                        .clicked()
                    {
                        self.run_shortcut_action(false);
                    }
                    if ui
                        .add_enabled(!self.busy && created, egui::Button::new("删除快捷方式"))
                        .clicked()
                    {
                        self.run_shortcut_action(true);
                    }
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::both()
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let size = ui.available_size();
                    ui.add_sized(
                        size,
                        egui::TextEdit::multiline(&mut self.logs)
                            .font(egui::TextStyle::Monospace)
                            .interactive(false),
                    );
                });
        });

        self.webdav_window(ctx);
        self.device_window(ctx);
        self.sync_files_window(ctx);
        #[cfg(target_os = "macos")]
        self.config_dir_window(ctx);
        self.about_window(ctx);
        self.update_window(ctx);
        self.notice_window(ctx);
    }
}

impl App {
    fn webdav_window(&mut self, ctx: &egui::Context) {
        if !self.show_webdav {
            return;
        }
        let mut open = true;
        egui::Window::new("WebDAV 设置")
            .collapsible(false)
            .resizable(false)
            .fixed_size([760., 330.])
            .open(&mut open)
            .show(ctx, |ui| {
                egui::Grid::new("dav").show(ui, |ui| {
                    ui.label("地址");
                    ui.add_sized(
                        [620., 26.],
                        egui::TextEdit::singleline(&mut self.dav_url).desired_width(f32::INFINITY),
                    );
                    ui.end_row();
                    ui.label("用户名");
                    ui.add_sized(
                        [620., 26.],
                        egui::TextEdit::singleline(&mut self.dav_username)
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();
                    ui.label("密码");
                    ui.add_sized(
                        [620., 26.],
                        egui::TextEdit::singleline(&mut self.dav_password)
                            .desired_width(f32::INFINITY)
                            .password(!self.dav_password_visible),
                    );
                    ui.end_row();
                });
                ui.checkbox(&mut self.dav_password_visible, "显示密码");
                ui.label("测试连接会执行 MKCOL + PUT + GET + DELETE，并探测服务端能力档位。");
                if !self.capability_text.is_empty() {
                    ui.label(&self.capability_text);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("测试连接"))
                        .clicked()
                    {
                        self.test_webdav();
                    }
                    if ui.button("保存").clicked() {
                        if let Err(error) = self.save_webdav() {
                            self.notice = Some(error.to_string());
                        } else {
                            self.show_webdav = false;
                        }
                    }
                    if ui.button("取消").clicked() {
                        self.show_webdav = false;
                    }
                });
            });
        if !open {
            self.show_webdav = false;
        }
    }

    fn device_window(&mut self, ctx: &egui::Context) {
        if !self.show_device {
            return;
        }
        let mut open = true;
        egui::Window::new("设备设置")
            .collapsible(false)
            .resizable(false)
            .fixed_size([820., 560.])
            .open(&mut open)
            .show(ctx, |ui| {
                if let Some(context) = &self.context {
                    ui.label(format!("当前设备 ID：{}", context.device.installation_id));
                    ui.label(format!("共享目录：{}", context.layout.others.display()));
                    ui.label(format!(
                        "session_token：{}…",
                        identity::short_token(&context.device.session_token)
                    ));
                } else {
                    ui.label("尚未完成设备设置。请先指定 RIME 用户词库并配置 WebDAV。");
                }
                ui.separator();
                egui::Grid::new("device_fields").show(ui, |ui| {
                    ui.label("设备 ID");
                    ui.add_sized(
                        [620., 26.],
                        egui::TextEdit::singleline(&mut self.device_id_draft)
                            .hint_text("3~32 位，仅限字母/数字/下划线/连字符，例如 MacBookAir")
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();
                    ui.label("密码");
                    ui.add_sized(
                        [620., 26.],
                        egui::TextEdit::singleline(&mut self.device_password_draft)
                            .password(true)
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();
                    ui.label("确认密码");
                    ui.add_sized(
                        [620., 26.],
                        egui::TextEdit::singleline(&mut self.device_password_confirm)
                            .password(true)
                            .hint_text("仅「强制接管」「修改密码」需要")
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();
                });
                ui.checkbox(
                    &mut self.device_skip_password,
                    "首次注册时跳过设置密码（不推荐）",
                );
                ui.checkbox(
                    &mut self.device_confirm_takeover,
                    "我已确认：接管会让该 ID 的旧设备在下一轮自动停止同步",
                );

                if !self.device_info_text.is_empty() {
                    ui.separator();
                    ui.label("远端 owner.json 信息：");
                    let mut info = self.device_info_text.clone();
                    ui.add(
                        egui::TextEdit::multiline(&mut info)
                            .desired_rows(4)
                            .interactive(false),
                    );
                }
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("注册 / 验证并接管"))
                        .clicked()
                    {
                        let skip = self.device_skip_password;
                        self.submit_device_action(DeviceAction::Claim {
                            id: self.device_id_draft.trim().to_owned(),
                            password: Some(self.device_password_draft.clone()),
                            skip_password: skip,
                            confirm: self.device_confirm_takeover,
                        });
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("强制接管"))
                        .clicked()
                    {
                        self.submit_device_action(DeviceAction::TakeoverForced {
                            id: self.device_id_draft.trim().to_owned(),
                            new_password: self.device_password_confirm.clone(),
                            confirm: self.device_confirm_takeover,
                        });
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("修改密码"))
                        .clicked()
                    {
                        self.submit_device_action(DeviceAction::ChangePassword {
                            old_password: self.device_password_draft.clone(),
                            new_password: self.device_password_confirm.clone(),
                        });
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("注销此设备"))
                        .clicked()
                    {
                        self.submit_device_action(DeviceAction::Retire {
                            password: self.device_password_draft.clone(),
                        });
                    }
                    if ui.button("导出配置").clicked() {
                        self.submit_device_action(DeviceAction::Export);
                    }
                    if ui.button("导入配置").clicked() {
                        self.submit_device_action(DeviceAction::Import {
                            text: self.exported_text.clone(),
                        });
                    }
                });
                ui.separator();
                ui.label("设备配置（导出/导入用，不含密码）：");
                ui.add(
                    egui::TextEdit::multiline(&mut self.exported_text)
                        .desired_rows(6)
                        .font(egui::TextStyle::Monospace),
                );
            });
        if !open {
            self.show_device = false;
        }
    }

    fn sync_files_window(&mut self, ctx: &egui::Context) {
        if !self.show_sync_files {
            return;
        }
        let mut open = true;
        let mut remove = None;
        egui::Window::new("同步文件选择")
            .collapsible(false)
            .resizable(false)
            .fixed_size([860., 420.])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("合并规则按文件类型自动判定：*.dict.yaml 词条并集；*.txt 文本并集（custom_phrase 这类三列表按「文本+编码」，多个拼法都会保留）；*.yaml/*.json/*.lua 按时间。");
                ui.label("移除某个文件只影响本设备（写进本机排除集），不会删除 Others/ 里的公共副本。");
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(240.)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Grid::new("sync_files_grid")
                            .striped(true)
                            .show(ui, |ui| {
                                for (index, path) in self.sync_files_draft.iter().enumerate() {
                                    ui.add_sized(
                                        [320., 22.],
                                        egui::Label::new(path).truncate(),
                                    );
                                    let mode = crate::merge::MergeMode::for_path(path)
                                        .map(|mode| mode.label())
                                        .unwrap_or("不可同步");
                                    ui.label(mode);
                                    if ui.button("删除").clicked() {
                                        remove = Some(index);
                                    }
                                    ui.end_row();
                                }
                            });
                    });
                if let Some(index) = remove {
                    self.sync_files_draft.remove(index);
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("添加文件").clicked() {
                            self.add_sync_files();
                        }
                        if ui.button("保存").clicked() {
                            match self.save_sync_files() {
                                Ok(()) => self.show_sync_files = false,
                                Err(error) => self.notice = Some(error.to_string()),
                            }
                        }
                        if ui.button("取消").clicked() {
                            self.show_sync_files = false;
                        }
                    });
                    ui.label(format!("文件总数：{}", self.sync_files_draft.len()));
                });
            });
        if !open {
            self.show_sync_files = false;
        }
    }

    #[cfg(target_os = "macos")]
    fn config_dir_window(&mut self, ctx: &egui::Context) {
        if !self.show_config_files {
            return;
        }
        let mut open = true;
        egui::Window::new("配置文件")
            .collapsible(false)
            .resizable(false)
            .fixed_size([720., 190.])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("该目录用于保存 RimeUserDictSync.ini 和 RimeSync.log。");
                ui.label(format!("当前生效目录：{}", self.base.display()));
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [560., 26.],
                        egui::TextEdit::singleline(&mut self.config_dir_draft)
                            .desired_width(f32::INFINITY),
                    );
                    if ui.button("选择目录").clicked()
                        && let Some(directory) = rfd::FileDialog::new()
                            .set_directory(if self.config_dir_draft.is_empty() {
                                self.base.clone()
                            } else {
                                PathBuf::from(&self.config_dir_draft)
                            })
                            .pick_folder()
                    {
                        self.config_dir_draft = directory.to_string_lossy().into_owned();
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("打开配置").clicked()
                        && let Err(error) =
                            std::process::Command::new("open").arg(&self.base).spawn()
                    {
                        self.notice = Some(format!("打开配置目录失败：{error}"));
                    }
                    if ui.button("保存").clicked() {
                        match self.save_config_dir() {
                            Ok(()) => {
                                let directory = self.base.display().to_string();
                                self.append(&crate::report::timestamped(&format!(
                                    "已切换 macOS 配置目录：{directory}"
                                )));
                                self.show_config_files = false;
                                self.notice = Some(format!("配置目录已切换到：\n{directory}"));
                            }
                            Err(error) => self.notice = Some(error.to_string()),
                        }
                    }
                    if ui.button("取消").clicked() {
                        self.show_config_files = false;
                    }
                });
            });
        if !open {
            self.show_config_files = false;
        }
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        let mut open = true;
        egui::Window::new("关于 RIME 用户词库同步工具")
            .collapsible(false)
            .resizable(false)
            .fixed_size([620., 280.])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.heading("关于 RIME 用户词库同步工具");
                ui.label(format!("版本：v{APP_VERSION}"));
                ui.add_space(6.);
                ui.label(
                    "后台纯文件搬运（不打断输入），手动「同步词库」时才合并并重新部署。支持 Windows 小狼毫与 macOS 鼠须管。",
                );
                ui.add_space(6.);
                ui.label("数据分层：Sync/<ID>/ 为各设备 userdb 快照，Sync/Others/ 为文本词典的公共合并结果。");
                ui.hyperlink_to(GITHUB_URL, GITHUB_URL);
                ui.label("许可证：GNU General Public License v3.0");
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    if ui.button("确定").clicked() {
                        self.show_about = false;
                    }
                });
            });
        if !open {
            self.show_about = false;
        }
    }

    fn update_window(&mut self, ctx: &egui::Context) {
        let Some(update) = self.available_update.clone() else {
            return;
        };
        let mut open = true;
        egui::Window::new("发现新版本")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("当前版本：v{APP_VERSION}"));
                ui.label(format!("最新版本：v{}", update.version));
                ui.add_space(8.);
                ui.label("发现新版本，是否前往 GitHub Release 页面下载？");
                ui.add_space(8.);
                ui.horizontal(|ui| {
                    ui.hyperlink_to("前往下载", &update.url);
                    if ui.button("稍后").clicked() {
                        self.available_update = None;
                    }
                });
            });
        if !open {
            self.available_update = None;
        }
    }

    fn notice_window(&mut self, ctx: &egui::Context) {
        if let Some(text) = self.notice.clone() {
            let mut open = true;
            egui::Window::new("提示")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(text);
                    if ui.button("确定").clicked() {
                        self.notice = None;
                    }
                });
            if !open {
                self.notice = None;
            }
        }
    }
}

/// GUI 内建的后台搬运线程（未安装 Windows 服务时的兜底）。
///
/// 与服务的调度完全同构：每 [`daemon::TICK`] 醒一次，判断 `DueAt` 是否到期，
/// 并消费 `.rime-sync/wake` 以响应「立刻搬运一轮」。装了服务就让位。
fn spawn_background(
    base: PathBuf,
    ini: PathBuf,
    tx: Sender<Event>,
    busy: Arc<AtomicBool>,
    service_running: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut due = daemon::DueAt::now();
        let mut last_error = String::new();
        loop {
            std::thread::sleep(daemon::TICK);
            let settings = match Settings::load(&ini) {
                Ok(settings) => settings,
                Err(_) => continue,
            };
            if busy.load(Ordering::Relaxed) || service_running.load(Ordering::Relaxed) {
                continue;
            }
            let schedule = daemon::Schedule::from_settings(&settings);
            let wake = daemon::take_wake_for_settings(&settings);
            if !wake && (!schedule.enabled || !due.is_due()) {
                continue;
            }
            due.reschedule(schedule.interval);
            let reporter = ChannelReporter { tx: tx.clone() };
            let cancel = AtomicBool::new(false);
            let result = match build_context(&base, &ini) {
                Ok(context) => transport::round(&context, &cancel, &reporter)
                    .map(|outcome| {
                        format!(
                            "后台搬运完成：下载 {} 项，上传 {} 项。",
                            outcome.downloaded, outcome.uploaded
                        )
                    })
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            match result {
                Ok(message) => {
                    last_error.clear();
                    let _ = tx.send(Event::Round(Ok(message)));
                }
                Err(error) => {
                    // 同一错误只打印一次，避免每轮刷屏。
                    if error != last_error {
                        last_error = error.clone();
                        let _ = tx.send(Event::Round(Err(error)));
                    }
                }
            }
        }
    });
}

fn build_context(base: &Path, ini: &Path) -> Result<Context> {
    let mut settings = Settings::load(ini)?;
    Context::build(base, ini, &mut settings)
}

/// 设备设置的全部网络动作都在后台线程执行。
fn run_device_action(ini: &Path, action: DeviceAction) -> Result<String> {
    let mut settings = Settings::load(ini)?;
    if !settings.webdav_ready() {
        bail!("请先在「WebDAV 设置」中填写地址与用户名并测试连接。");
    }
    let rime_dir = context::resolve_rime_dir(&settings);
    let installation = rime_dir.join("installation.yaml");
    if !installation.is_file() {
        bail!("请先点击「指定 RIME 用户词库」选择包含 installation.yaml 的目录。");
    }
    let dav = webdav::WebDav::new(
        settings.webdav_url.trim(),
        settings.username.trim(),
        &settings.password,
    )?;
    let snapshot_dir = rime_dir.join(paths::SYNC_DIR_NAME);
    let mut device = load_device_file(&rime_dir).unwrap_or_default();
    let seed = DeviceSeed::from_device(&device, snapshot_dir.to_string_lossy().into_owned());

    match action {
        DeviceAction::Claim {
            id,
            password,
            skip_password,
            confirm,
        } => {
            if id.is_empty() {
                bail!("请先填写设备 ID。");
            }
            let password = password.filter(|value| !value.is_empty());
            if password.is_none() && !skip_password {
                bail!("请填写 ID 密码，或显式勾选「跳过设置密码」。");
            }
            match device::claim(&dav, &id, password.as_deref(), &seed)? {
                Claim::Claimed(claimed) => {
                    let mut claimed = *claimed;
                    claimed.sync_dir = seed.sync_dir.clone();
                    persist(&rime_dir, &mut settings, ini, &claimed)?;
                    Ok(format!(
                        "已在远端抢占 ID「{id}」并写入 owner.json（{}）。",
                        if password.is_some() {
                            "已设置密码"
                        } else {
                            "未设置密码"
                        }
                    ))
                }
                Claim::Occupied(owner) => {
                    let owner = *owner;
                    let info = describe_owner(&owner, &dav, &id);
                    if !confirm {
                        bail!(
                            "远端已存在此 ID，且不会自动占用它。\n{info}\n\
                             若确认这是你自己的设备：填写密码并勾选确认框后重试「注册 / 验证并接管」；\
                             忘记密码则使用「强制接管」。"
                        );
                    }
                    let Some(password) = password else {
                        bail!("验证接管需要密码；忘记密码请使用「强制接管」。\n{info}");
                    };
                    let owner = owner.context(
                        "远端存在此 ID 的目录，但没有 owner.json（老版本或手工创建）；请使用「强制接管」。",
                    )?;
                    let layout = Layout::new(&rime_dir, &id);
                    layout.ensure_dirs()?;
                    let (mut taken, notices) =
                        device::takeover_verified(&dav, &id, &owner, &password, &seed, &layout)?;
                    taken.sync_dir = seed.sync_dir.clone();
                    persist(&rime_dir, &mut settings, ini, &taken)?;
                    Ok(format!(
                        "已接管 ID「{id}」（密码验证通过）。\n{}",
                        notices.join("\n")
                    ))
                }
            }
        }
        DeviceAction::TakeoverForced {
            id,
            new_password,
            confirm,
        } => {
            if id.is_empty() {
                bail!("请先填写设备 ID。");
            }
            identity::validate_password_input(&new_password)?;
            if !confirm {
                bail!("强制接管会让该 ID 的旧设备立即停止同步，请先勾选确认框。");
            }
            let owner = identity::Owner::load(&dav, &id)?.unwrap_or_default();
            let layout = Layout::new(&rime_dir, &id);
            layout.ensure_dirs()?;
            let (mut taken, notices) =
                device::takeover_forced(&dav, &id, &owner, &new_password, &seed, &layout)?;
            taken.sync_dir = seed.sync_dir.clone();
            persist(&rime_dir, &mut settings, ini, &taken)?;
            Ok(format!(
                "已强制接管 ID「{id}」并重设密码。\n{}",
                notices.join("\n")
            ))
        }
        DeviceAction::ChangePassword {
            old_password,
            new_password,
        } => {
            let id = current_id(&device)?;
            device::change_password(&dav, &id, &old_password, &new_password, &mut device)?;
            persist(&rime_dir, &mut settings, ini, &device)?;
            Ok(format!("已更新 ID「{id}」的密码（session_token 未轮换）。"))
        }
        DeviceAction::Retire { password } => {
            let id = current_id(&device)?;
            let layout = Layout::new(&rime_dir, &id);
            let notices = device::retire(&dav, &id, &password, &layout)?;
            // 本地设备配置也一并清掉，避免下次启动还指向已注销的 ID。
            let _ = fs::remove_file(layout.device_file());
            device.session_token.clear();
            Ok(notices.join("\n"))
        }
        DeviceAction::Export => {
            let id = current_id(&device)?;
            let layout = Layout::new(&rime_dir, &id);
            let list = SyncList::load(&layout.list_file())?;
            let exported = device::export(&device, settings.webdav_url.trim(), &list);
            Ok(format!(
                "已生成设备配置，请复制保存到网盘或 U 盘：\n{}",
                exported.to_text()?
            ))
        }
        DeviceAction::Import { text } => {
            if text.trim().is_empty() {
                bail!("请先把导出的设备配置粘贴到文本框里。");
            }
            let exported = identity::ExportedDevice::from_text(&text)?;
            let layout = Layout::new(&rime_dir, &exported.id);
            layout.ensure_dirs()?;
            // 不要先删 device.json：它与 ID 无关（固定为 .rime-sync/device.json），
            // persist 随后会整体覆盖；一旦 import 校验失败，反而会把旧设备身份也一起弄丢。
            let notices = device::import(&exported, &mut device, &mut settings, &layout)?;
            persist(&rime_dir, &mut settings, ini, &device)?;
            Ok(format!(
                "{}\n导入后请直接点「同步词库」，ID 与 token 一致，无需接管。",
                notices.join("\n")
            ))
        }
    }
}

fn current_id(device: &DeviceFile) -> Result<String> {
    if device.installation_id.trim().is_empty() {
        bail!("本机还没有设备 ID，请先注册或导入设备配置。");
    }
    Ok(device.installation_id.clone())
}

fn load_device_file(rime_dir: &Path) -> Option<DeviceFile> {
    DeviceFile::load(&rime_dir.join(paths::META_DIR_NAME).join(paths::DEVICE_FILE)).ok()
}

fn persist(
    rime_dir: &Path,
    settings: &mut Settings,
    ini: &Path,
    device: &DeviceFile,
) -> Result<()> {
    let notices = context::persist_device(rime_dir, device)?;
    for notice in notices {
        let _ = notice;
    }
    settings.legacy_sync_files.clear();
    settings.save(ini)?;
    Ok(())
}

fn describe_owner(owner: &Option<identity::Owner>, dav: &webdav::WebDav, id: &str) -> String {
    let count = device::count_remote_snapshots(dav, id);
    match owner {
        Some(owner) => {
            let mut text = owner.describe();
            text.push_str(&format!("快照文件数：{count}\n"));
            if let Some(minutes) = device::minutes_since_last_sync(owner)
                && (0..24 * 60).contains(&minutes)
            {
                text.push_str(&format!(
                    "⚠ 该 ID 的设备似乎仍在使用（最后同步 {minutes} 分钟前），接管会使其立即停止同步。请确认这是你重装后的同一台机器。\n"
                ));
            }
            text
        }
        None => format!(
            "该 ID 在远端已存在目录，但没有 owner.json（老版本创建或手工建目录）。快照文件数：{count}\n"
        ),
    }
}

fn check_for_update(tx: Sender<Event>) {
    std::thread::spawn(move || {
        let update = fetch_latest_release().ok().flatten();
        let _ = tx.send(Event::UpdateChecked(update));
    });
}

fn fetch_latest_release() -> Result<Option<UpdateInfo>> {
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("RimeUserDictSync/", env!("CARGO_PKG_VERSION")))
        .build()?
        .get(LATEST_RELEASE_API)
        .send()?
        .error_for_status()?;
    let json: serde_json::Value = serde_json::from_str(&response.text()?)?;
    let tag = json["tag_name"]
        .as_str()
        .context("GitHub Release 缺少 tag_name")?;
    let url = json["html_url"]
        .as_str()
        .context("GitHub Release 缺少 html_url")?;
    available_release(tag, url, APP_VERSION)
}

fn available_release(tag: &str, url: &str, current: &str) -> Result<Option<UpdateInfo>> {
    let version_text = tag.strip_prefix(['v', 'V']).unwrap_or(tag);
    let latest = semver::Version::parse(version_text)?;
    let current = semver::Version::parse(current)?;
    Ok((latest > current).then(|| UpdateInfo {
        version: latest.to_string(),
        url: url.to_owned(),
    }))
}

/// `test_webdav` 之外的地方只需要一个只读的 WebDAV 探测入口，这里给出统一构造点。
fn build_webdav(settings: &Settings) -> Result<Option<webdav::WebDav>> {
    if !settings.webdav_ready() {
        return Ok(None);
    }
    Ok(Some(webdav::WebDav::new(
        settings.webdav_url.trim(),
        settings.username.trim(),
        &settings.password,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_newer_stable_release_is_available() {
        assert!(
            available_release("v0.3.1", "https://example.test", "0.3.0")
                .unwrap()
                .is_some()
        );
        assert!(
            available_release("v0.3.0", "https://example.test", "0.3.0")
                .unwrap()
                .is_none()
        );
        assert!(
            available_release("v0.2.9", "https://example.test", "0.3.0")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn owner_description_mentions_recent_activity() {
        let owner = identity::Owner {
            host: "mac".into(),
            created_at: "2026-01-01 00:00:00".into(),
            last_sync: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            machine_fp: "abcd1234".into(),
            ..Default::default()
        };
        let text = owner.describe();
        assert!(text.contains("mac"));
        assert!(text.contains("密码状态：未设置（老版本创建或手工建目录）"));
        assert!(device::minutes_since_last_sync(&owner).unwrap() <= 1);
    }
}
