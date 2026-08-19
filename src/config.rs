use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use regex::Regex;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Default)]
pub struct Settings {
    pub user_data_dir: String,
    pub deployer_path: String,
    pub webdav_url: String,
    pub username: String,
    pub password: String,
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
        Ok(Self {
            user_data_dir: get("rime", "user_data_dir")
                .or_else_nonempty(get("weasel", "user_data_dir")),
            deployer_path: get("rime", "deployer_path")
                .or_else_nonempty(get("weasel", "deployer_path")),
            webdav_url: get("webdav", "url"),
            username: get("webdav", "username"),
            password,
        })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = format!(
            "[rime]\nuser_data_dir={}\ndeployer_path={}\n\n[webdav]\nurl={}\nusername={}\npassword_base64={}\n",
            self.user_data_dir,
            self.deployer_path,
            self.webdav_url,
            self.username,
            STANDARD.encode(self.password.as_bytes())
        );
        fs::write(path, text).context("保存配置文件失败")
    }
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
    Ok(std::env::current_exe()?
        .parent()
        .context("无法确定程序目录")?
        .to_path_buf())
}
