//! Linux 专用目录/文件选择器（egui 自绘）。
//!
//! 为什么不用 `rfd`：它在 Linux 上走 xdg-desktop-portal / GTK，两个都没暴露
//! 「显示隐藏文件」的开关；而 RIME 用户目录天生躲在隐藏目录里
//! （`~/.local/share/fcitx5/rime`、`~/.config/ibus/rime`）。
//! 列表默认不列以 `.` 开头的项，用户连 `.local` 都看不见，只能手打整条路径。
//! portal 与 zenity 都不可用时，原生对话框会直接返回 `None`。
//!
//! 所以这里用 egui 画列表：隐藏项默认可见（也能一键收起）、可以粘贴路径跳转、
//! 顺带标出「看起来像 RIME 目录」的文件夹，省掉逐层试错。

use eframe::egui;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

/// 选择器回给调用方的结果。
pub enum PickerEvent {
    Accept(PathBuf),
    Files(Vec<PathBuf>),
    Cancel,
}

/// 列表里的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_hidden: bool,
    /// 目录里有 `installation.yaml`，多半就是 RIME 用户目录。
    pub looks_like_rime: bool,
}

pub struct DirPicker {
    cwd: PathBuf,
    /// 路径输入框的内容；回车或点「转到」才真正跳转。
    manual: String,
    /// 默认开：RIME 目录本身就是隐藏的，默认收起等于把用户挡在门外。
    show_hidden: bool,
    entries: Vec<Entry>,
    /// 当前目录里的隐藏项总数（不受开关影响），用来提示「还藏着几项」。
    hidden_total: usize,
    error: Option<String>,
    hint: String,
    select_files: bool,
    selected_files: BTreeSet<PathBuf>,
}

impl DirPicker {
    pub fn new(start: PathBuf, hint: impl Into<String>) -> Self {
        let mut picker = Self {
            cwd: PathBuf::from("/"),
            manual: String::new(),
            show_hidden: true,
            entries: Vec::new(),
            hidden_total: 0,
            error: None,
            hint: hint.into(),
            select_files: false,
            selected_files: BTreeSet::new(),
        };
        picker.navigate(start_dir(&start));
        picker
    }

    pub fn for_files(start: PathBuf, hint: impl Into<String>) -> Self {
        let mut picker = Self::new(start, hint);
        picker.select_files = true;
        picker
    }

    /// 当前所在目录（测试与调用方都用得到）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn showing_hidden(&self) -> bool {
        self.show_hidden
    }

    /// 让调用方把校验失败写进窗口里，而不是关窗再弹一个提示框。
    pub fn report_error(&mut self, message: String) {
        self.error = Some(message);
    }

    fn navigate(&mut self, dir: PathBuf) {
        match read_entries(&dir) {
            Ok((entries, hidden_total)) => {
                self.cwd = dir;
                self.entries = entries;
                self.hidden_total = hidden_total;
                self.error = None;
            }
            Err(error) => {
                // 失败时留在原地：跳到读不了的地方只会让用户彻底失去方向。
                self.error = Some(format!("无法读取 {}：{error}", dir.display()));
            }
        }
        self.manual = self.cwd.display().to_string();
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            self.navigate(parent);
        }
    }

    fn go_home(&mut self) {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            self.navigate(home);
        }
    }

    /// 处理输入框里的路径：支持 `~/`、`%VAR%`、相对写法（相对当前目录）。
    fn go_manual(&mut self) {
        let raw = self.manual.trim().to_string();
        if raw.is_empty() {
            return;
        }
        let target = resolve_typed_path(&raw, &self.cwd);
        if target.is_dir() {
            self.navigate(target);
        } else if target.is_file() {
            // 文件模式也支持直接粘贴文件路径，并在所在目录勾选它。
            let parent = target.parent().map(Path::to_path_buf);
            match parent {
                Some(dir) => {
                    self.navigate(dir);
                    if self.select_files {
                        self.selected_files.insert(target);
                    }
                }
                None => self.error = Some(format!("{} 不是目录", target.display())),
            }
        } else {
            self.error = Some(format!("{} 不存在", target.display()));
            self.manual = raw;
        }
    }

    /// 画窗口；返回 `None` 表示用户还在选。
    pub fn show(&mut self, ctx: &egui::Context, title: &str) -> Option<PickerEvent> {
        let mut open = true;
        let mut outcome: Option<PickerEvent> = None;
        egui::Window::new(title)
            .collapsible(false)
            .resizable(true)
            .default_size([760., 520.])
            .min_width(560.)
            .open(&mut open)
            .show(ctx, |ui| {
                outcome = self.body(ui);
            });
        if outcome.is_some() {
            return outcome;
        }
        // 右上角的关闭按钮等同于「取消」。
        if !open {
            return Some(PickerEvent::Cancel);
        }
        None
    }

    fn body(&mut self, ui: &mut egui::Ui) -> Option<PickerEvent> {
        let mut result = None;

        ui.horizontal(|ui| {
            if ui.button("⬆ 上一层").clicked() {
                self.go_up();
            }
            if ui.button("🏠 主目录").clicked() {
                self.go_home();
            }
            if ui.button("🔄 刷新").clicked() {
                let cwd = self.cwd.clone();
                self.navigate(cwd);
            }
            ui.separator();
            ui.checkbox(&mut self.show_hidden, "显示隐藏文件");
            if !self.show_hidden && self.hidden_total > 0 {
                ui.label(
                    egui::RichText::new(format!("（{} 个隐藏项被折叠）", self.hidden_total)).weak(),
                );
            }
        });

        ui.horizontal(|ui| {
            let width = (ui.available_width() - 90.).max(160.);
            let response = ui.add_sized(
                [width, 24.],
                egui::TextEdit::singleline(&mut self.manual)
                    .hint_text("可直接粘贴路径（支持 ~/ 与 %HOME%），回车跳转"),
            );
            let entered =
                response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            if ui.button("转到").clicked() || entered {
                self.go_manual();
            }
        });

        // 常见位置：把探测到的候选目录摆出来，省得用户自己找 .local 在哪。
        let quick: Vec<PathBuf> = crate::platform::rime_candidate_dirs()
            .into_iter()
            .filter(|dir| dir.is_dir())
            .take(6)
            .collect();
        if !quick.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("常见位置：").weak());
                for dir in quick {
                    if ui
                        .small_button(shorten_home(&dir))
                        .on_hover_text(dir.display().to_string())
                        .clicked()
                    {
                        self.navigate(dir);
                    }
                }
            });
        }

        ui.separator();
        let mut goto: Option<PathBuf> = None;
        egui::ScrollArea::vertical()
            .max_height(300.)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.entries.is_empty() {
                    ui.label(egui::RichText::new("（这个目录下没有条目）").weak());
                }
                for entry in &self.entries {
                    if entry.is_hidden && !self.show_hidden {
                        continue;
                    }
                    ui.horizontal(|ui| {
                        let text = if entry.is_dir {
                            egui::RichText::new(format!("📁 {}", entry.name))
                        } else {
                            egui::RichText::new(format!("📄 {}", entry.name))
                        };
                        // 隐藏项不弱化显示：RIME 的目录本身就藏在 `.local` 下，
                        // 这里正是用户要点的目标，用灰字反而碍事。
                        if self.select_files && !entry.is_dir {
                            let mut selected = self.selected_files.contains(&entry.path);
                            if ui.checkbox(&mut selected, text).changed() {
                                if selected {
                                    self.selected_files.insert(entry.path.clone());
                                } else {
                                    self.selected_files.remove(&entry.path);
                                }
                            }
                            return;
                        }
                        let sense = if entry.is_dir {
                            egui::Sense::click()
                        } else {
                            egui::Sense::hover()
                        };
                        let mut label = ui.add(egui::Label::new(text).sense(sense));
                        if entry.is_dir {
                            label = label.on_hover_cursor(egui::CursorIcon::PointingHand);
                        }
                        if entry.looks_like_rime {
                            ui.label(
                                egui::RichText::new("RIME")
                                    .small()
                                    .color(egui::Color32::from_rgb(0, 160, 70)),
                            );
                        }
                        if label.clicked() {
                            goto = Some(entry.path.clone());
                        }
                    });
                }
            });
        if let Some(dir) = goto {
            self.navigate(dir);
        }

        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label("当前目录：");
            ui.monospace(self.cwd.display().to_string());
        });
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::from_rgb(200, 120, 0), error);
        }
        if self.select_files {
            ui.label(format!(
                "已选 {} 个文件（可跨目录选择）",
                self.selected_files.len()
            ));
        }
        ui.horizontal(|ui| {
            if self.select_files {
                if ui
                    .add_enabled(
                        !self.selected_files.is_empty(),
                        egui::Button::new("添加所选文件"),
                    )
                    .clicked()
                {
                    result = Some(PickerEvent::Files(
                        self.selected_files.iter().cloned().collect(),
                    ));
                }
                if ui.button("清空选择").clicked() {
                    self.selected_files.clear();
                }
            } else if ui.button("选择此目录").clicked() {
                result = Some(PickerEvent::Accept(self.cwd.clone()));
            }
            if ui.button("取消").clicked() {
                result = Some(PickerEvent::Cancel);
            }
            if !self.hint.is_empty() {
                ui.label(egui::RichText::new(&self.hint).weak());
            }
        });

        result
    }
}

/// 起点不存在时逐级上溯到最近一个真实存在的目录，避免一开窗就是空白。
fn start_dir(start: &Path) -> PathBuf {
    let mut dir = start.to_path_buf();
    loop {
        if dir.is_dir() {
            return dir;
        }
        match dir.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => dir = parent.to_path_buf(),
            _ => {
                return std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/"));
            }
        }
    }
}

/// 读出目录内容：目录在前、文件在后，同类按名字排序；隐藏项一并返回并计数。
fn read_entries(dir: &Path) -> std::io::Result<(Vec<Entry>, usize)> {
    let mut dirs: Vec<Entry> = Vec::new();
    let mut files: Vec<Entry> = Vec::new();
    let mut hidden_total = 0usize;
    for entry in fs::read_dir(dir)? {
        // 单个条目出错（比如权限不足的软链）不该让整个列表失败。
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_hidden = name.starts_with('.');
        let file_type = entry.file_type();
        let is_dir = match &file_type {
            Ok(kind) if kind.is_dir() => true,
            // `~/.local` 偶尔是软链，只认 file_type 会判定成文件。
            Ok(kind) if kind.is_symlink() => entry.path().is_dir(),
            _ => false,
        };
        if is_hidden {
            hidden_total += 1;
        }
        let item = Entry {
            looks_like_rime: is_dir && entry.path().join("installation.yaml").is_file(),
            name,
            path: entry.path(),
            is_dir,
            is_hidden,
        };
        if is_dir {
            dirs.push(item);
        } else {
            files.push(item);
        }
    }
    let by_name = |a: &Entry, b: &Entry| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    };
    dirs.sort_by(by_name);
    files.sort_by(by_name);
    dirs.extend(files);
    Ok((dirs, hidden_total))
}

/// 输入框里的路径：`~`、`%VAR%`、相对路径都要认。
fn resolve_typed_path(raw: &str, cwd: &Path) -> PathBuf {
    let expanded = crate::platform::expand_env(raw);
    let expanded = if expanded == "~" {
        std::env::var("HOME").unwrap_or_else(|_| "~".to_owned())
    } else if let Some(rest) = expanded.strip_prefix("~/") {
        format!("{}/{}", std::env::var("HOME").unwrap_or_default(), rest)
    } else {
        expanded
    };
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// 把 `/home/alice/.local/share/fcitx5/rime` 缩成 `~/.local/share/fcitx5/rime`。
fn shorten_home(dir: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) => match dir.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => dir.display().to_string(),
        },
        None => dir.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    #[test]
    fn entries_list_dirs_first_then_files_regardless_of_hidden_state() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".hidden-dir")).unwrap();
        fs::create_dir_all(dir.path().join("visible-dir")).unwrap();
        touch(&dir.path().join(".hidden-file"));
        touch(&dir.path().join("visible-file"));

        let (entries, hidden_total) = read_entries(dir.path()).unwrap();
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(
            names,
            vec![".hidden-dir", "visible-dir", ".hidden-file", "visible-file"]
        );
        assert_eq!(hidden_total, 2, "隐藏项要被计数，折叠时才提示得出数量");
        assert!(entries[0].is_hidden && entries[0].is_dir);
    }

    #[test]
    fn entries_mark_directories_that_look_like_a_rime_user_dir() {
        let dir = tempfile::tempdir().unwrap();
        let fcitx5 = dir.path().join(".local/share/fcitx5");
        let rime = fcitx5.join("rime");
        fs::create_dir_all(&rime).unwrap();
        fs::write(rime.join("installation.yaml"), "installation_id: pc01\n").unwrap();
        fs::create_dir_all(fcitx5.join("other")).unwrap();

        // 在 fcitx5 这一层看：rime 带 installation.yaml，other 是空壳。
        let (entries, _) = read_entries(&fcitx5).unwrap();
        let marked: Vec<&str> = entries
            .iter()
            .filter(|entry| entry.looks_like_rime)
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(marked, vec!["rime"]);
    }

    #[test]
    fn picker_starts_with_hidden_entries_visible() {
        // RIME 的用户目录本身是隐藏目录，默认收起就把用户挡在门外。
        let dir = tempfile::tempdir().unwrap();
        let picker = DirPicker::new(dir.path().to_path_buf(), "");
        assert!(picker.showing_hidden());
        assert_eq!(picker.cwd(), dir.path());
    }

    #[test]
    fn file_picker_accepts_hidden_typed_files_and_keeps_choices_between_directories() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join(".hidden/custom.yaml");
        let second = dir.path().join("other/words.dict.yaml");
        touch(&first);
        touch(&second);
        let mut picker = DirPicker::for_files(dir.path().to_path_buf(), "");
        assert!(picker.showing_hidden());
        picker.manual = first.display().to_string();
        picker.go_manual();
        assert_eq!(picker.cwd(), first.parent().unwrap());
        picker.manual = second.display().to_string();
        picker.go_manual();
        assert_eq!(picker.cwd(), second.parent().unwrap());
        assert_eq!(picker.selected_files, BTreeSet::from([first, second]));
        picker.manual = dir.path().join("missing.yaml").display().to_string();
        picker.go_manual();
        assert!(picker.error.is_some());
        assert_eq!(picker.selected_files.len(), 2);

        let picker = std::cell::RefCell::new(picker);
        egui::__run_test_ui(|ui| {
            let _ = picker.borrow_mut().body(ui);
        });
    }

    #[test]
    fn picker_falls_back_to_the_nearest_existing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no/such/rime");
        let picker = DirPicker::new(missing, "");
        assert_eq!(
            picker.cwd(),
            dir.path(),
            "起点不存在时要停在最近的真实父目录"
        );
    }

    #[test]
    fn typed_paths_accept_tilde_and_relative_forms() {
        let cwd = PathBuf::from("/tmp/somewhere");
        assert_eq!(resolve_typed_path("/a/b", &cwd), PathBuf::from("/a/b"));
        assert_eq!(resolve_typed_path("child", &cwd), cwd.join("child"));
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            assert_eq!(resolve_typed_path("~/rime", &cwd), home.join("rime"));
        }
    }

    #[test]
    fn shorten_home_hides_the_prefix() {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let dir = home.join(".local/share/fcitx5/rime");
            assert_eq!(shorten_home(&dir), "~/.local/share/fcitx5/rime");
            assert_eq!(shorten_home(&home), "~");
        }
        assert_eq!(shorten_home(Path::new("/opt/x")), "/opt/x");
    }

    /// 没有真实窗口（CI、无 X 会话）时也要能确认这套界面代码画得出来。
    #[test]
    fn body_renders_without_a_window() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".hidden-dir")).unwrap();
        fs::create_dir_all(dir.path().join("visible-dir")).unwrap();
        touch(&dir.path().join(".hidden-file"));
        touch(&dir.path().join("visible-file"));

        let picker = std::cell::RefCell::new(DirPicker::new(dir.path().to_path_buf(), "提示"));
        egui::__run_test_ui(|ui| {
            let _ = picker.borrow_mut().body(ui);
        });
        // 折叠隐藏项的分支同样要走一遍。
        picker.borrow_mut().show_hidden = false;
        egui::__run_test_ui(|ui| {
            let _ = picker.borrow_mut().body(ui);
        });
    }

    #[test]
    fn report_error_is_shown_instead_of_closing_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let mut picker = DirPicker::new(dir.path().to_path_buf(), "");
        picker.report_error("没有 installation.yaml".to_owned());
        let picker = std::cell::RefCell::new(picker);
        egui::__run_test_ui(|ui| {
            let _ = picker.borrow_mut().body(ui);
        });
        assert!(
            picker
                .borrow()
                .error
                .as_deref()
                .unwrap()
                .contains("installation.yaml")
        );
    }
}
