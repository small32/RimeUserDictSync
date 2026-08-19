use crate::{
    config::{self, Settings},
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
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([760., 520.])
        .with_min_inner_size([620., 420.])
        .with_title("RIME 用户词库同步工具");
    let viewport = if let Some(i) = icon {
        viewport.with_icon(i)
    } else {
        viewport
    };
    eframe::run_native(
        "RIME 用户词库同步工具",
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
    password_visible: bool,
    cancel: Arc<AtomicBool>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    notice: Option<String>,
}
impl App {
    fn new() -> Self {
        let base = config::app_dir().unwrap_or_else(|_| PathBuf::from("."));
        let ini = base.join("WeaselUserDictSync.ini");
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
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
