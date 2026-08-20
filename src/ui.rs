use crate::{
    config::{self, Settings, SyncFile, SyncFileMode},
    sync::{self, Reporter},
    webdav::WebDav,
};
use eframe::egui;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
};

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const APP_TITLE: &str = concat!("RIME 用户词库同步工具 v", env!("CARGO_PKG_VERSION"));
const GITHUB_URL: &str = "https://github.com/small32/RimeUserDictSync";

enum Event {
    Log(String),
    Progress(u8),
    Finished(Result<(), String>),
    Tested(Result<(), String>),
}
struct ChannelReporter {
    tx: Sender<Event>,
}
impl Reporter for ChannelReporter {
    fn log(&self, t: &str) {
        let _ = self.tx.send(Event::Log(sync::timestamped(t)));
    }
    fn progress(&self, v: u8) {
        let _ = self.tx.send(Event::Progress(v));
    }
}

pub fn run() -> eframe::Result {
    let icon = load_icon();
    let min_width = if cfg!(target_os = "macos") {
        760.
    } else {
        620.
    };
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([760., 520.])
        .with_min_inner_size([min_width, 420.])
        .with_title(APP_TITLE);
    let viewport = if let Some(i) = icon {
        viewport.with_icon(i)
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
            Ok(Box::new(App::new()))
        }),
    )
}
fn load_icon() -> Option<Arc<egui::IconData>> {
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
    let mut f = egui::FontDefinitions::default();
    let candidates: Vec<PathBuf> = if cfg!(windows) {
        vec![
            PathBuf::from(r"C:\Windows\Fonts\msyh.ttc"),
            PathBuf::from(r"C:\Windows\Fonts\simhei.ttf"),
        ]
    } else if cfg!(target_os = "macos") {
        vec![
            // PingFang is not stored at a stable public path on every macOS release.
            // Hiragino Sans GB and STHeiti are system CJK fonts available on both
            // Intel and Apple Silicon macOS installations.
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
        .find_map(|p| fs::read(&p).ok().map(|b| (p, b)))
    {
        let font = egui::FontData::from_owned(bytes).tweak(egui::FontTweak {
            y_offset: 1.5,
            ..Default::default()
        });
        f.font_data.insert("cjk".into(), Arc::new(font));
        f.families
            .get_mut(&egui::FontFamily::Proportional)
            .unwrap()
            .insert(0, "cjk".into());
        f.families
            .get_mut(&egui::FontFamily::Monospace)
            .unwrap()
            .insert(0, "cjk".into());
    }
    ctx.set_fonts(f);
}

struct App {
    base: PathBuf,
    ini: PathBuf,
    log_path: PathBuf,
    settings: Settings,
    logs: String,
    progress: u8,
    busy: bool,
    show_webdav: bool,
    show_about: bool,
    show_sync_files: bool,
    sync_files_draft: Vec<SyncFile>,
    sync_files_selected: Vec<bool>,
    #[cfg(target_os = "macos")]
    show_config_files: bool,
    #[cfg(target_os = "macos")]
    config_dir_draft: String,
    password_visible: bool,
    cancel: Arc<AtomicBool>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    notice: Option<String>,
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
        let settings = Settings::load(&ini).unwrap_or_default();
        let logs = fs::read_to_string(&log_path).unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        fs::create_dir_all(base.join("Sync/WebDAV")).ok();
        Self {
            base,
            ini,
            log_path,
            settings,
            logs,
            progress: 0,
            busy: false,
            show_webdav: false,
            show_about: false,
            show_sync_files: false,
            sync_files_draft: Vec::new(),
            sync_files_selected: Vec::new(),
            #[cfg(target_os = "macos")]
            show_config_files: false,
            #[cfg(target_os = "macos")]
            config_dir_draft: String::new(),
            password_visible: false,
            cancel: Arc::new(AtomicBool::new(false)),
            tx,
            rx,
            notice: None,
        }
    }
    fn append(&mut self, line: &str) {
        self.logs.push_str(line);
        self.logs.push('\n');
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
    fn poll(&mut self) {
        while let Ok(e) = self.rx.try_recv() {
            match e {
                Event::Log(x) => self.append(&x),
                Event::Progress(x) => self.progress = x,
                Event::Finished(r) => {
                    self.busy = false;
                    self.notice = Some(match r {
                        Ok(_) => "同步完成。".into(),
                        Err(e) => {
                            let x = sync::timestamped(&format!("失败: {e}"));
                            self.append(&x);
                            e
                        }
                    })
                }
                Event::Tested(r) => {
                    self.busy = false;
                    self.notice = Some(match r {
                        Ok(_) => "连接成功，上传和下载测试均已通过。".into(),
                        Err(e) => format!("测试失败：{e}"),
                    });
                }
            }
        }
    }
    fn start(&mut self) {
        if self.busy {
            return;
        }
        self.cancel = Arc::new(AtomicBool::new(false));
        self.progress = 0;
        self.busy = true;
        let base = self.base.clone();
        let settings = self.settings.clone();
        let cancel = self.cancel.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let reporter = ChannelReporter { tx: tx.clone() };
            let result = sync::run(&base, settings, cancel, &reporter).map_err(|e| e.to_string());
            let _ = tx.send(Event::Finished(result));
        });
    }
    fn choose_rime(&mut self) {
        if let Some(dir) = rfd::FileDialog::new()
            .set_directory(if self.settings.user_data_dir.is_empty() {
                crate::platform::default_user_dir()
            } else {
                PathBuf::from(&self.settings.user_data_dir)
            })
            .pick_folder()
        {
            let yaml = dir.join("installation.yaml");
            let result = (|| -> anyhow::Result<String> {
                let id = config::yaml_scalar(&yaml, "installation_id")?;
                config::validate_installation_id(&id)?;
                let root = self.base.join("Sync");
                config::set_yaml_scalar(&yaml, "sync_dir", root.to_string_lossy().as_ref())?;
                self.settings.user_data_dir = dir.to_string_lossy().into_owned();
                self.settings.save(&self.ini)?;
                fs::create_dir_all(root.join(&id))?;
                fs::create_dir_all(root.join("WebDAV"))?;
                Ok(id)
            })();
            match result{Ok(id)=>self.append(&sync::timestamped(&format!("已指定 RIME 用户词库，installation_id 保持为 {id}，sync_dir 已指向程序目录下 Sync。"))),Err(e)=>self.notice=Some(e.to_string())}
        }
    }
    fn test_webdav(&mut self) {
        if self.busy {
            return;
        }
        self.busy = true;
        let s = self.settings.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let r = (|| WebDav::new(&s.webdav_url, &s.username, &s.password)?.test())()
                .map_err(|e: anyhow::Error| e.to_string());
            let _ = tx.send(Event::Tested(r));
        });
    }
    fn choose_sync_files(&mut self) {
        let user_dir = if self.settings.user_data_dir.trim().is_empty() {
            crate::platform::default_user_dir()
        } else {
            PathBuf::from(&self.settings.user_data_dir)
        };
        let Some(files) = rfd::FileDialog::new().set_directory(&user_dir).pick_files() else {
            return;
        };
        for file in files {
            let Ok(relative) = file.strip_prefix(&user_dir) else {
                self.notice = Some("同步文件必须位于当前 RIME 用户词库目录内。".into());
                continue;
            };
            let path = relative.to_string_lossy().replace('\\', "/");
            if path.is_empty() || path.contains(';') {
                self.notice = Some("同步文件名不能为空，也不能包含分号。".into());
                continue;
            }
            if !self.sync_files_draft.iter().any(|item| item.path == path) {
                self.sync_files_draft.push(SyncFile {
                    path,
                    mode: SyncFileMode::Newest,
                    writeback: true,
                });
                self.sync_files_selected.push(false);
            }
        }
    }
    #[cfg(target_os = "macos")]
    fn save_config_dir(&mut self) -> anyhow::Result<()> {
        let target = PathBuf::from(self.config_dir_draft.trim());
        if !target.is_absolute() {
            anyhow::bail!("配置目录必须是绝对路径");
        }
        fs::create_dir_all(&target)?;
        let source = self
            .base
            .canonicalize()
            .unwrap_or_else(|_| self.base.clone());
        let target = target.canonicalize()?;
        if target != source && target.starts_with(&source) {
            anyhow::bail!("配置目录不能放在当前配置目录的子目录中");
        }
        if target != source {
            copy_tree_if_present(&source.join("Sync"), &target.join("Sync"))?;
            if self.log_path.is_file() {
                fs::copy(&self.log_path, target.join("RimeSync.log"))?;
            }
        }
        let ini = target.join("RimeUserDictSync.ini");
        self.settings.save(&ini)?;
        if !self.settings.user_data_dir.trim().is_empty() {
            let installation =
                PathBuf::from(&self.settings.user_data_dir).join("installation.yaml");
            if installation.is_file() {
                config::set_yaml_scalar(
                    &installation,
                    "sync_dir",
                    target.join("Sync").to_string_lossy().as_ref(),
                )?;
            }
        }
        config::set_app_dir(&target)?;
        let resolved = config::app_dir()?.canonicalize()?;
        if resolved != target {
            anyhow::bail!("配置目录保存后校验失败，程序仍指向旧目录");
        }
        self.base = target.clone();
        self.ini = ini;
        self.log_path = target.join("RimeSync.log");
        self.config_dir_draft = target.to_string_lossy().into_owned();
        if target != source {
            let old_sync = source.join("Sync");
            if old_sync.is_dir() {
                fs::remove_dir_all(old_sync)?;
            }
            for name in ["RimeUserDictSync.ini", "RimeSync.log"] {
                let old_file = source.join(name);
                if old_file.is_file() {
                    fs::remove_file(old_file)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn copy_tree_if_present(source: &std::path::Path, target: &std::path::Path) -> anyhow::Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree_if_present(&entry.path(), &destination)?;
        } else {
            fs::copy(entry.path(), &destination)?;
            if let Ok(modified) = entry.metadata()?.modified() {
                filetime::set_file_mtime(
                    destination,
                    filetime::FileTime::from_system_time(modified),
                )?;
            }
        }
    }
    Ok(())
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.poll();
        ctx.request_repaint_after(std::time::Duration::from_millis(150));
        egui::TopBottomPanel::top("progress_panel")
            .exact_height(44.)
            .show(ctx, |ui| {
                ui.add_space(8.);
                ui.add(
                    egui::ProgressBar::new(self.progress as f32 / 100.)
                        .show_percentage()
                        .animate(self.busy),
                );
            });
        egui::TopBottomPanel::bottom("actions_panel")
            .exact_height(52.)
            .show(ctx, |ui| {
                ui.add_space(8.);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("指定RIME用户词库"))
                        .clicked()
                    {
                        self.choose_rime();
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("WebDAV设置"))
                        .clicked()
                    {
                        self.show_webdav = true;
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("同步文件选择"))
                        .clicked()
                    {
                        self.sync_files_draft = self.settings.sync_files.clone();
                        self.sync_files_selected = vec![false; self.sync_files_draft.len()];
                        self.show_sync_files = true;
                    }
                    #[cfg(target_os = "macos")]
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("配置文件"))
                        .clicked()
                    {
                        self.config_dir_draft = self.base.to_string_lossy().into_owned();
                        self.show_config_files = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("关于").clicked() {
                            self.show_about = true;
                        }
                        if ui
                            .add_enabled(self.busy, egui::Button::new("停止同步"))
                            .clicked()
                        {
                            self.cancel.store(true, Ordering::Relaxed);
                        }
                        if ui
                            .add_enabled(!self.busy, egui::Button::new("开始同步"))
                            .clicked()
                        {
                            self.start();
                        }
                    });
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
        if self.show_webdav {
            let mut open = true;
            egui::Window::new("WebDAV设置")
                .collapsible(false)
                .resizable(false)
                .fixed_size([720., 280.])
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.set_min_size(egui::vec2(690., 240.));
                    egui::Grid::new("dav").show(ui, |ui| {
                        ui.label("地址");
                        ui.add_sized(
                            [610., 28.],
                            egui::TextEdit::singleline(&mut self.settings.webdav_url)
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();
                        ui.label("用户名");
                        ui.add_sized(
                            [610., 28.],
                            egui::TextEdit::singleline(&mut self.settings.username)
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();
                        ui.label("密码");
                        ui.add_sized(
                            [610., 28.],
                            egui::TextEdit::singleline(&mut self.settings.password)
                                .desired_width(f32::INFINITY)
                                .password(!self.password_visible),
                        );
                        ui.end_row();
                    });
                    ui.checkbox(&mut self.password_visible, "显示密码");
                    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(!self.busy, egui::Button::new("测试连接"))
                                .clicked()
                            {
                                self.test_webdav();
                            }
                            if ui.button("保存").clicked() {
                                match self.settings.save(&self.ini) {
                                    Ok(_) => {
                                        self.append(&sync::timestamped("已保存 WebDAV 设置。"));
                                        self.show_webdav = false;
                                    }
                                    Err(e) => self.notice = Some(e.to_string()),
                                }
                            }
                            if ui.button("取消").clicked() {
                                self.show_webdav = false;
                            }
                        });
                    });
                });
            if !open {
                self.show_webdav = false;
            }
        }
        if self.show_sync_files {
            let mut open = true;
            egui::Window::new("同步文件选择")
                .collapsible(false)
                .resizable(false)
                .fixed_size([960., 360.])
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.set_min_size(egui::vec2(930., 320.));
                    ui.label("可多选 RIME 用户目录内的文件；按并集同步仅适用于 UTF-8 文本文件。");
                    ui.separator();
                    let mut remove = None;
                    egui::ScrollArea::vertical()
                        .max_height(220.)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            egui::Grid::new("sync_files_grid")
                                .striped(true)
                                .show(ui, |ui| {
                                    for (index, file) in
                                        self.sync_files_draft.iter_mut().enumerate()
                                    {
                                        ui.checkbox(&mut self.sync_files_selected[index], "选择");
                                        ui.add_sized(
                                            [260., 24.],
                                            egui::Label::new(&file.path).truncate(),
                                        );
                                        let mut union = file.mode == SyncFileMode::Union;
                                        if ui.checkbox(&mut union, "按并集").clicked() {
                                            file.mode = SyncFileMode::Union;
                                        }
                                        let mut newest = file.mode == SyncFileMode::Newest;
                                        if ui.checkbox(&mut newest, "按时间").clicked() {
                                            file.mode = SyncFileMode::Newest;
                                        }
                                        ui.checkbox(&mut file.writeback, "回写");
                                        if ui.button("删除").clicked() {
                                            remove = Some(index);
                                        }
                                        ui.end_row();
                                    }
                                });
                        });
                    if let Some(index) = remove {
                        self.sync_files_draft.remove(index);
                        self.sync_files_selected.remove(index);
                    }
                    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("添加文件").clicked() {
                                self.choose_sync_files();
                            }
                            if ui
                                .add_enabled(
                                    !self.sync_files_draft.is_empty(),
                                    egui::Button::new("清空"),
                                )
                                .clicked()
                            {
                                self.sync_files_draft.clear();
                                self.sync_files_selected.clear();
                            }
                            if ui.button("保存").clicked() {
                                self.settings.sync_files = self.sync_files_draft.clone();
                                match self.settings.save(&self.ini) {
                                    Ok(_) => {
                                        self.append(&sync::timestamped("已保存同步文件选择。"));
                                        self.show_sync_files = false;
                                    }
                                    Err(e) => self.notice = Some(e.to_string()),
                                }
                            }
                            if ui.button("取消").clicked() {
                                self.show_sync_files = false;
                            }
                        });
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("全选").clicked() {
                                self.sync_files_selected.fill(true);
                            }
                            if ui.button("取消选择").clicked() {
                                self.sync_files_selected.fill(false);
                            }
                            let has_selected =
                                self.sync_files_selected.iter().any(|selected| *selected);
                            if ui
                                .add_enabled(has_selected, egui::Button::new("批量按并集"))
                                .clicked()
                            {
                                for (file, selected) in self
                                    .sync_files_draft
                                    .iter_mut()
                                    .zip(&self.sync_files_selected)
                                {
                                    if *selected {
                                        file.mode = SyncFileMode::Union;
                                    }
                                }
                            }
                            if ui
                                .add_enabled(has_selected, egui::Button::new("批量按时间"))
                                .clicked()
                            {
                                for (file, selected) in self
                                    .sync_files_draft
                                    .iter_mut()
                                    .zip(&self.sync_files_selected)
                                {
                                    if *selected {
                                        file.mode = SyncFileMode::Newest;
                                    }
                                }
                            }
                            if ui
                                .add_enabled(has_selected, egui::Button::new("批量回写"))
                                .clicked()
                            {
                                for (file, selected) in self
                                    .sync_files_draft
                                    .iter_mut()
                                    .zip(&self.sync_files_selected)
                                {
                                    if *selected {
                                        file.writeback = true;
                                    }
                                }
                            }
                            if ui
                                .add_enabled(has_selected, egui::Button::new("批量不回写"))
                                .clicked()
                            {
                                for (file, selected) in self
                                    .sync_files_draft
                                    .iter_mut()
                                    .zip(&self.sync_files_selected)
                                {
                                    if *selected {
                                        file.writeback = false;
                                    }
                                }
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
        if self.show_config_files {
            let mut open = true;
            egui::Window::new("配置文件")
                .collapsible(false)
                .resizable(false)
                .fixed_size([720., 190.])
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.set_min_size(egui::vec2(690., 150.));
                    ui.label("该目录用于保存 Sync、RimeUserDictSync.ini 和 RimeSync.log。");
                    ui.label(format!("当前生效目录：{}", self.base.display()));
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [570., 28.],
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
                    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("打开配置").clicked()
                                && let Err(error) =
                                    std::process::Command::new("open").arg(&self.base).spawn()
                            {
                                self.notice = Some(format!("打开配置目录失败：{error}"));
                            }
                            if ui.button("保存").clicked() {
                                match self.save_config_dir() {
                                    Ok(_) => {
                                        let directory = self.base.display().to_string();
                                        self.append(&sync::timestamped(&format!(
                                            "已移动并切换 macOS 配置目录：{directory}"
                                        )));
                                        self.show_config_files = false;
                                        self.notice =
                                            Some(format!("配置目录已移动并切换到：\n{directory}"));
                                    }
                                    Err(error) => self.notice = Some(error.to_string()),
                                }
                            }
                            if ui.button("取消").clicked() {
                                self.show_config_files = false;
                            }
                        });
                    });
                });
            if !open {
                self.show_config_files = false;
            }
        }
        if self.show_about {
            let mut open = true;
            egui::Window::new("关于 RIME 用户词库同步工具")
                .collapsible(false)
                .resizable(false)
                .fixed_size([620., 260.])
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.heading("关于 RIME 用户词库同步工具");
                    ui.add_space(6.);
                    ui.label(format!("版本：v{APP_VERSION}"));
                    ui.add_space(8.);
                    ui.label(
                        "RIME 用户词库同步工具是一款支持 Windows 和 macOS 的 RIME 用户数据 WebDAV 同步工具，适用于小狼毫和鼠须管。",
                    );
                    ui.add_space(8.);
                    ui.label(
                        "本项目为开源软件。源代码、使用说明、版本更新和问题反馈请访问 GitHub：",
                    );
                    ui.hyperlink_to(GITHUB_URL, GITHUB_URL);
                    ui.add_space(8.);
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
        if let Some(text) = self.notice.clone() {
            let mut open = true;
            egui::Window::new("RIME 用户词库同步工具")
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
