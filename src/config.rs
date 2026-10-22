use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use regex::Regex;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// 应用程序级设置（`RimeUserDictSync.ini`）。
///
/// 这里只放"每台设备各自的连接信息"，与同步内容无关：
/// 设备 ID / 密码派生串 / 目录布局的权威副本在 `RIME/.rime-sync/device.json`，
/// 同步文件列表在 `RIME/.rime-sync/sync-list.json`。
#[derive(Clone, Debug)]
pub struct Settings {
    pub user_data_dir: String,
    pub deployer_path: String,
    pub webdav_url: String,
    pub username: String,
    pub password: String,
    /// 后台纯搬运是否开启（§3）。
    pub background_enabled: bool,
    /// 后台轮次间隔，分钟；§10 风险 10 建议不低于 10 分钟。
    pub background_minutes: u32,
    /// v0.2.5 的 INI 同步列表，仅用于一次性迁移进 `sync-list.json`。
    pub legacy_sync_files: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            user_data_dir: String::new(),
            deployer_path: String::new(),
            webdav_url: String::new(),
            username: String::new(),
            password: String::new(),
            background_enabled: true,
            background_minutes: 10,
            legacy_sync_files: Vec::new(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            Self::default().save(path)?;
        }
        let map = parse_ini(&fs::read_to_string(path).context("读取配置文件失败")?);
        let get = |section: &str, key: &str| {
            map.get(&(section.to_owned(), key.to_owned()))
                .cloned()
                .unwrap_or_default()
        };
        let encoded = get("webdav", "password_base64");
        // 单个字段损坏时降级为空密码，避免整个配置加载失败后被默认空配置覆盖。
        let password = if encoded.is_empty() {
            get("webdav", "password")
        } else {
            STANDARD
                .decode(encoded)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .unwrap_or_default()
        };
        let minutes = get("background", "minutes")
            .trim()
            .parse::<u32>()
            .unwrap_or(10)
            .clamp(1, 24 * 60);
        Ok(Self {
            user_data_dir: nonempty(get("rime", "user_data_dir"))
                .or_else(|| nonempty(get("weasel", "user_data_dir")))
                .unwrap_or_default(),
            deployer_path: nonempty(get("rime", "deployer_path"))
                .or_else(|| nonempty(get("weasel", "deployer_path")))
                .unwrap_or_default(),
            webdav_url: get("webdav", "url"),
            username: get("webdav", "username"),
            password,
            background_enabled: !matches!(
                get("background", "enabled")
                    .trim()
                    .to_ascii_lowercase()
                    .as_str(),
                "0" | "false" | "no" | "off"
            ),
            background_minutes: minutes,
            legacy_sync_files: parse_legacy_sync_files(&get),
        })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut text = format!(
            "[rime]\nuser_data_dir={}\ndeployer_path={}\n\n[webdav]\nurl={}\nusername={}\npassword_base64={}\n\n[background]\nenabled={}\nminutes={}\n",
            self.user_data_dir,
            self.deployer_path,
            self.webdav_url,
            self.username,
            STANDARD.encode(self.password.as_bytes()),
            if self.background_enabled { "1" } else { "0" },
            self.background_minutes,
        );
        if !self.legacy_sync_files.is_empty() {
            text.push_str("\n[sync_files]\nfile=");
            text.push_str(&self.legacy_sync_files.join(";"));
            text.push('\n');
        }
        fs::write(path, text).context("保存配置文件失败")
    }

    pub fn webdav_ready(&self) -> bool {
        !self.webdav_url.trim().is_empty() && !self.username.trim().is_empty()
    }
}

fn nonempty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

/// 兼容 v0.2.5 的 `file=路径/union/Y;...` 与更早的 `union=`/`newest=` 两个键。
fn parse_legacy_sync_files(get: &impl Fn(&str, &str) -> String) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for value in [
        get("sync_files", "file"),
        get("sync_files", "union"),
        get("sync_files", "newest"),
    ] {
        for entry in value
            .split(';')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            entries.push(entry.to_owned());
        }
    }
    entries.dedup();
    entries
}

fn parse_ini(text: &str) -> BTreeMap<(String, String), String> {
    let mut result = BTreeMap::new();
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_lowercase();
        } else if !line.is_empty()
            && !line.starts_with(';')
            && !line.starts_with('#')
            && let Some((key, value)) = line.split_once('=')
        {
            result.insert(
                (section.clone(), key.trim().to_lowercase()),
                value.trim().to_owned(),
            );
        }
    }
    result
}

/// 读一个可缺失的 YAML 标量（`installation.yaml` 的 `installation_id` / `sync_dir` / `Password`）。
pub fn yaml_scalar_opt(path: &Path, key: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let rx = Regex::new(&format!(
        r"(?m)^[\t ]*{}[\t ]*:[\t ]*([^\r\n]*)",
        regex::escape(key)
    ))
    .ok()?;
    let mut value = rx
        .captures(&text)
        .and_then(|captures| captures.get(1))
        .map(|matched| matched.as_str().trim().to_owned())?;
    if let Some(index) = value.find(" #") {
        value.truncate(index);
        value = value.trim().to_owned();
    }
    if value.len() >= 2
        && ((value.starts_with('\'') && value.ends_with('\''))
            || (value.starts_with('"') && value.ends_with('"')))
    {
        value = value[1..value.len() - 1].replace("''", "'");
    }
    if value.is_empty() { None } else { Some(value) }
}

/// 写回 YAML 标量：只替换第一处匹配，值统一用单引号包裹。
pub fn set_yaml_scalar(path: &Path, key: &str, value: &str) -> Result<()> {
    let text = fs::read_to_string(path)?;
    let escaped = value.replace('\'', "''");
    let rx = Regex::new(&format!(
        r"(?m)^([\t ]*{}[\t ]*:[\t ]*)[^\r\n]*",
        regex::escape(key)
    ))?;
    let updated = if let Some(captures) = rx.captures(&text) {
        // 手动拼接：避免值中的 $ 被当作捕获组引用。
        let whole = captures.get(0).context("正则匹配结果缺少整体")?;
        let prefix = captures.get(1).context("正则匹配结果缺少键前缀")?;
        let mut updated = String::with_capacity(text.len() + escaped.len() + 2);
        updated.push_str(&text[..whole.start()]);
        updated.push_str(prefix.as_str());
        if prefix.as_str().ends_with(':') {
            updated.push(' ');
        }
        updated.push('\'');
        updated.push_str(&escaped);
        updated.push('\'');
        updated.push_str(&text[whole.end()..]);
        updated
    } else {
        format!("{}\n{}: '{}'\n", text.trim_end(), key, escaped)
    };
    fs::write(path, updated)?;
    Ok(())
}

/// 配置文件文件名（Windows/Linux 便携布局下就在程序旁边）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const CONFIG_FILE: &str = "RimeUserDictSync.ini";

/// 配置目录回退到用户目录时的说明，供界面与命令行提示。
static APP_DIR_NOTE: OnceLock<Option<String>> = OnceLock::new();

/// 程序目录不可用时，配置与日志最终落在哪、为什么。
///
/// Linux 上把程序装到 `/opt`、`/usr/local/bin` 这类 root 所有的目录里很常见，
/// 此时普通用户既写不了 `RimeUserDictSync.ini` 也写不了 `RimeSync.log`：
/// 「指定 RIME 用户词库」点完保存失败，下次启动又回到自动探测，
/// 用户看到的现象就是「怎么都识别不到词库」。所以这里换成用户可写的目录，
/// 并把原因留给界面显示，而不是静默换掉。
pub fn app_dir_note() -> Option<String> {
    APP_DIR_NOTE.get().cloned().flatten()
}

fn remember_app_dir_note(note: Option<String>) -> Option<String> {
    // 只记第一次：后续调用（命令行、界面各算一次）不该覆盖真正的来源。
    let _ = APP_DIR_NOTE.set(note.clone());
    note
}

/// 配置/日志所在目录：Windows 为 EXE 所在目录，macOS 默认用户资料库。
///
/// Linux 上优先用 EXE 所在目录（便携版，拷走整个目录即可带走配置）；
/// 该目录不可写时退到 `$XDG_DATA_HOME/rime-user-dict-sync`，
/// 这样系统级安装（`/opt`）也不会丢失设置。
pub fn app_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let default = default_app_dir()?;
        fs::create_dir_all(&default).context("无法创建 macOS 应用数据目录")?;
        let marker = default.join("config_dir.txt");
        let configured = fs::read_to_string(marker).unwrap_or_default();
        let dir = if configured.trim().is_empty() {
            default
        } else {
            let path = PathBuf::from(configured.trim());
            if path.is_absolute() { path } else { default }
        };
        fs::create_dir_all(&dir).context("无法创建 macOS 应用数据目录")?;
        Ok(dir)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let exe_dir = std::env::current_exe()?
            .parent()
            .context("无法确定程序目录")?
            .to_path_buf();
        #[cfg(target_os = "linux")]
        {
            if is_writable(&exe_dir) {
                remember_app_dir_note(None);
                Ok(exe_dir)
            } else {
                let fallback = user_data_app_dir()?;
                fs::create_dir_all(&fallback).context("无法创建用户级配置目录")?;
                inherit_settings(&exe_dir, &fallback);
                remember_app_dir_note(Some(format!(
                    "程序目录 {} 不可写，已回退到此",
                    exe_dir.display()
                )));
                Ok(fallback)
            }
        }
        #[cfg(not(target_os = "linux"))]
        Ok(exe_dir)
    }
}

/// 目录是否真的可写。
///
/// 用「建一个临时文件再删掉」来判定，而不是看权限位：只读挂载、ACL、
/// immutable 属性这些都能让 `mode` 看起来正常却写入失败。
#[cfg(target_os = "linux")]
fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".rime-sync-probe-{}", std::process::id()));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Linux 的用户级配置目录：`$XDG_DATA_HOME/rime-user-dict-sync`。
#[cfg(target_os = "linux")]
fn user_data_app_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("无法确定用户主目录")?;
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from(&home).join(".local/share"));
    Ok(base.join("rime-user-dict-sync"))
}

/// 回退时把程序目录里已有的 INI 继承过来：系统级安装升级后不该丢设置。
#[cfg(target_os = "linux")]
fn inherit_settings(from: &Path, to: &Path) {
    let source = from.join(CONFIG_FILE);
    let target = to.join(CONFIG_FILE);
    if target.is_file() || !source.is_file() {
        return;
    }
    let _ = fs::copy(&source, &target);
}

#[cfg(target_os = "macos")]
pub fn default_app_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("无法确定用户主目录")?;
    Ok(PathBuf::from(home).join("Library/Application Support/RimeUserDictSync"))
}

#[cfg(target_os = "macos")]
pub fn set_app_dir(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("配置目录必须是绝对路径");
    }
    let default = default_app_dir()?;
    fs::create_dir_all(&default)?;
    fs::create_dir_all(path)?;
    fs::write(
        default.join("config_dir.txt"),
        path.to_string_lossy().as_bytes(),
    )
    .context("保存 macOS 配置目录失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 系统级安装（/opt）回退到用户目录时，程序旁边那份 INI 要被继承过来，
    /// 否则升级一次就等于把所有 WebDAV / 设备设置丢掉。
    #[cfg(target_os = "linux")]
    #[test]
    fn settings_are_inherited_when_config_dir_falls_back() {
        let from = tempfile::tempdir().unwrap();
        let to = tempfile::tempdir().unwrap();
        let original = "[webdav]\nurl=https://dav.example.com/rime/\n";
        fs::write(from.path().join(CONFIG_FILE), original).unwrap();

        inherit_settings(from.path(), to.path());
        assert_eq!(
            fs::read_to_string(to.path().join(CONFIG_FILE)).unwrap(),
            original
        );

        // 回退目录已有配置时不能被程序旁边那份覆盖。
        fs::write(from.path().join(CONFIG_FILE), "[webdav]\nurl=别的\n").unwrap();
        inherit_settings(from.path(), to.path());
        assert_eq!(
            fs::read_to_string(to.path().join(CONFIG_FILE)).unwrap(),
            original
        );
    }

    /// 可写判定要真去建文件：只读挂载上权限位看着是 7，写照样失败。
    #[cfg(target_os = "linux")]
    #[test]
    fn writable_probe_reports_a_writable_dir_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_writable(dir.path()));
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert!(leftovers.is_empty(), "探测文件应当被删掉：{leftovers:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn writable_probe_rejects_a_missing_dir() {
        assert!(!is_writable(Path::new("/nonexistent/rime-sync-probe")));
    }

    #[test]
    fn settings_round_trip_keeps_webdav_and_background() {
        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("RimeUserDictSync.ini");
        let settings = Settings {
            user_data_dir: r"C:\Users\x\AppData\Roaming\Rime".into(),
            webdav_url: "https://dav.example.com/rime/".into(),
            username: "user".into(),
            password: "p@ss".into(),
            background_enabled: false,
            background_minutes: 15,
            ..Default::default()
        };
        settings.save(&ini).unwrap();
        let loaded = Settings::load(&ini).unwrap();
        assert_eq!(loaded.user_data_dir, settings.user_data_dir);
        assert_eq!(loaded.webdav_url, settings.webdav_url);
        assert_eq!(loaded.username, "user");
        assert_eq!(loaded.password, "p@ss");
        assert!(!loaded.background_enabled);
        assert_eq!(loaded.background_minutes, 15);
    }

    #[test]
    fn legacy_sync_file_entries_are_read_for_migration() {
        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("RimeUserDictSync.ini");
        fs::write(
            &ini,
            "[sync_files]\nfile=default.custom.yaml/newest/N;custom/a.dict.yaml/union/Y\n",
        )
        .unwrap();
        assert_eq!(Settings::load(&ini).unwrap().legacy_sync_files.len(), 2);
    }

    #[test]
    fn set_yaml_scalar_keeps_dollar_literal_and_first_match_only() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("installation.yaml");
        fs::write(&yaml, "sync_dir: first\nsync_dir: second\n").unwrap();
        set_yaml_scalar(&yaml, "sync_dir", r"C:\data$1\Sync").unwrap();
        assert_eq!(
            fs::read_to_string(&yaml).unwrap(),
            "sync_dir: 'C:\\data$1\\Sync'\nsync_dir: second\n"
        );
    }

    #[test]
    fn optional_scalar_reports_missing_password() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("installation.yaml");
        fs::write(&yaml, "installation_id: pc01\nPassword: 'a$b'\n").unwrap();
        assert_eq!(
            yaml_scalar_opt(&yaml, "installation_id").as_deref(),
            Some("pc01")
        );
        assert_eq!(yaml_scalar_opt(&yaml, "Password").as_deref(), Some("a$b"));
        assert_eq!(yaml_scalar_opt(&yaml, "sync_dir"), None);
    }

    #[test]
    fn optional_scalar_strips_trailing_comments_and_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("installation.yaml");
        fs::write(&yaml, "sync_dir: /tmp/Sync # 说明\nother: \"quoted\"\n").unwrap();
        assert_eq!(
            yaml_scalar_opt(&yaml, "sync_dir").as_deref(),
            Some("/tmp/Sync")
        );
        assert_eq!(yaml_scalar_opt(&yaml, "other").as_deref(), Some("quoted"));
    }

    #[test]
    fn empty_yaml_scalar_never_consumes_the_next_field() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("installation.yaml");
        fs::write(&yaml, "sync_dir:\r\ninstallation_id: pc01\r\n").unwrap();
        assert_eq!(yaml_scalar_opt(&yaml, "sync_dir"), None);
        set_yaml_scalar(&yaml, "sync_dir", "C:/Rime/Sync").unwrap();
        assert_eq!(
            fs::read_to_string(&yaml).unwrap(),
            "sync_dir: 'C:/Rime/Sync'\r\ninstallation_id: pc01\r\n"
        );
        assert_eq!(
            yaml_scalar_opt(&yaml, "installation_id").as_deref(),
            Some("pc01")
        );
    }
}
