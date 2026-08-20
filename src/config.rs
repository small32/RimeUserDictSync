use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use regex::Regex;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncFileMode {
    Union,
    Newest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncFile {
    pub path: String,
    pub mode: SyncFileMode,
    pub writeback: bool,
}

#[derive(Clone, Default)]
pub struct Settings {
    pub user_data_dir: String,
    pub deployer_path: String,
    pub webdav_url: String,
    pub username: String,
    pub password: String,
    pub sync_files: Vec<SyncFile>,
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            Self::default().save(path)?;
        }
        let map = parse_ini(&fs::read_to_string(path).context("读取配置文件失败")?);
        let get = |section: &str, key: &str| {
            map.get(&(section.into(), key.into()))
                .cloned()
                .unwrap_or_default()
        };
        let encoded = get("webdav", "password_base64");
        let password = if encoded.is_empty() {
            get("webdav", "password")
        } else {
            String::from_utf8(
                STANDARD
                    .decode(encoded)
                    .context("WebDAV 密码 Base64 无效")?,
            )?
        };
        let mut sync_files = parse_sync_file_entries(&get("sync_files", "file"));
        if sync_files.is_empty() {
            sync_files = parse_legacy_sync_files(&get);
        }
        Ok(Self {
            user_data_dir: get("rime", "user_data_dir")
                .or_else_nonempty(get("weasel", "user_data_dir")),
            deployer_path: get("rime", "deployer_path")
                .or_else_nonempty(get("weasel", "deployer_path")),
            webdav_url: get("webdav", "url"),
            username: get("webdav", "username"),
            password,
            sync_files,
        })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let files = join_sync_file_entries(&self.sync_files);
        let text = format!(
            "[rime]\nuser_data_dir={}\ndeployer_path={}\n\n[webdav]\nurl={}\nusername={}\npassword_base64={}\n\n[sync_files]\nfile={}\n",
            self.user_data_dir,
            self.deployer_path,
            self.webdav_url,
            self.username,
            STANDARD.encode(self.password.as_bytes()),
            files,
        );
        fs::write(path, text).context("保存配置文件失败")
    }
}

fn parse_sync_file_entries(value: &str) -> Vec<SyncFile> {
    value
        .split(';')
        .map(str::trim)
        .filter_map(|entry| {
            let (path, mode, writeback) =
                if let Some((path, writeback)) = entry.rsplit_once("/union/") {
                    (path, SyncFileMode::Union, writeback)
                } else if let Some((path, writeback)) = entry.rsplit_once("/newest/") {
                    (path, SyncFileMode::Newest, writeback)
                } else {
                    return None;
                };
            if path.is_empty() {
                return None;
            }
            Some(SyncFile {
                path: path.to_owned(),
                mode,
                // 兼容曾短暂使用“回写目录”的测试版：非 N 参数均视为回写。
                writeback: !writeback.eq_ignore_ascii_case("N"),
            })
        })
        .collect()
}

fn join_sync_file_entries(files: &[SyncFile]) -> String {
    files
        .iter()
        .map(|file| {
            let mode = match file.mode {
                SyncFileMode::Union => "union",
                SyncFileMode::Newest => "newest",
            };
            let writeback = if file.writeback { "Y" } else { "N" };
            format!("{}/{mode}/{writeback}", file.path)
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn parse_legacy_sync_files(get: &impl Fn(&str, &str) -> String) -> Vec<SyncFile> {
    let mut files = parse_legacy_file_list(&get("sync_files", "union"), SyncFileMode::Union);
    let newest = parse_legacy_file_list(&get("sync_files", "newest"), SyncFileMode::Newest);
    files.retain(|union| {
        !newest
            .iter()
            .any(|item| item.path.eq_ignore_ascii_case(&union.path))
    });
    files.extend(newest);
    files
}

fn parse_legacy_file_list(value: &str, mode: SyncFileMode) -> Vec<SyncFile> {
    value
        .split(';')
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(|path| SyncFile {
            path: path.to_owned(),
            mode,
            writeback: false,
        })
        .collect()
}

trait NonEmpty {
    fn or_else_nonempty(self, other: String) -> String;
}
impl NonEmpty for String {
    fn or_else_nonempty(self, other: String) -> String {
        if self.is_empty() { other } else { self }
    }
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

pub fn yaml_scalar(path: &Path, key: &str) -> Result<String> {
    let text = fs::read_to_string(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    let rx = Regex::new(&format!(r"(?m)^\s*{}\s*:\s*(.*?)\s*$", regex::escape(key)))?;
    let mut value = rx
        .captures(&text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_owned())
        .with_context(|| format!("installation.yaml 缺少 {key}"))?;
    if let Some(i) = value.find(" #") {
        value.truncate(i);
        value = value.trim().to_owned();
    }
    if value.len() >= 2
        && ((value.starts_with('\'') && value.ends_with('\''))
            || (value.starts_with('"') && value.ends_with('"')))
    {
        value = value[1..value.len() - 1].to_owned();
    }
    if value.is_empty() {
        bail!("installation.yaml 的 {key} 为空");
    }
    Ok(value)
}

pub fn set_yaml_scalar(path: &Path, key: &str, value: &str) -> Result<()> {
    let text = fs::read_to_string(path)?;
    let escaped = value.replace('\'', "''");
    let rx = Regex::new(&format!(r"(?m)^(\s*{}\s*:\s*).*$", regex::escape(key)))?;
    let updated = if rx.is_match(&text) {
        rx.replace(&text, format!("${{1}}'{escaped}'")).into_owned()
    } else {
        format!("{}\n{}: '{}'\n", text.trim_end(), key, escaped)
    };
    fs::write(path, updated)?;
    Ok(())
}

pub fn validate_installation_id(value: &str) -> Result<()> {
    if value.trim().is_empty()
        || matches!(value, "." | "..")
        || value.eq_ignore_ascii_case("WebDAV")
        || value.contains(['/', '\\'])
    {
        bail!("installation_id 不能安全地用作同步文件夹名称: {value}");
    }
    Ok(())
}

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
    Ok(std::env::current_exe()?
        .parent()
        .context("无法确定程序目录")?
        .to_path_buf())
}

#[cfg(target_os = "macos")]
pub fn default_app_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("无法确定用户主目录")?;
    Ok(PathBuf::from(home).join("Library/Application Support/RimeUserDictSync"))
}

#[cfg(target_os = "macos")]
pub fn set_app_dir(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("配置目录必须是绝对路径");
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

    #[test]
    fn sync_files_round_trip_as_semicolon_lists() {
        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("RimeUserDictSync.ini");
        let settings = Settings {
            sync_files: vec![
                SyncFile {
                    path: "custom/a.dict.yaml".into(),
                    mode: SyncFileMode::Union,
                    writeback: true,
                },
                SyncFile {
                    path: "wanxiang-lts-zh-hans.gram".into(),
                    mode: SyncFileMode::Newest,
                    writeback: false,
                },
            ],
            ..Default::default()
        };

        settings.save(&ini).unwrap();
        let text = fs::read_to_string(&ini).unwrap();
        assert!(
            text.contains("file=custom/a.dict.yaml/union/Y;wanxiang-lts-zh-hans.gram/newest/N")
        );
        assert_eq!(
            Settings::load(&ini).unwrap().sync_files,
            settings.sync_files
        );
    }
}
