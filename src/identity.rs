use crate::webdav::WebDav;
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::Hmac;
use pbkdf2::pbkdf2;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::path::Path;

pub const PASSWORD_ALG: &str = "pbkdf2-sha256";
/// §7.1：PBKDF2-HMAC-SHA256(pw, salt, 600k) → 32B。
pub const PASSWORD_ITERATIONS: u32 = 600_000;
const SALT_BYTES: usize = 16;
const TOKEN_BYTES: usize = 32;

/// `installation.yaml` 与 `owner.json` 里保存的密码派生串（§7.4）。
///
/// 存的是 KDF 派生串而不是明文：验证只需"本地用远端 salt 重算 hash → 比对"，
/// 功能不受影响，但密码不会随 RIME 目录被其它同步工具搬走。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasswordHash {
    #[serde(default = "default_alg")]
    pub alg: String,
    #[serde(default = "default_iterations")]
    pub iter: u32,
    pub salt: String,
    pub hash: String,
}

fn default_alg() -> String {
    PASSWORD_ALG.to_owned()
}

fn default_iterations() -> u32 {
    PASSWORD_ITERATIONS
}

impl PasswordHash {
    /// 用新随机 salt 派生。
    pub fn derive(password: &str) -> Self {
        let mut salt = [0u8; SALT_BYTES];
        rand::thread_rng().fill_bytes(&mut salt);
        Self::derive_with(password, &salt, PASSWORD_ITERATIONS)
    }

    pub fn derive_with(password: &str, salt: &[u8], iter: u32) -> Self {
        let mut out = [0u8; 32];
        // Hmac<Sha256> 的输出长度固定，这里不会失败。
        let _ = pbkdf2::<Hmac<Sha256>>(password.as_bytes(), salt, iter.max(1), &mut out);
        Self {
            alg: PASSWORD_ALG.to_owned(),
            iter,
            salt: STANDARD.encode(salt),
            hash: STANDARD.encode(out),
        }
    }

    /// 用本条记录自己的 salt/iter 重算并比对。
    pub fn verify(&self, password: &str) -> bool {
        let Ok(salt) = STANDARD.decode(&self.salt) else {
            return false;
        };
        if !self.alg.eq_ignore_ascii_case(PASSWORD_ALG) {
            return false;
        }
        Self::derive_with(password, &salt, self.iter).hash == self.hash
    }

    /// `pbkdf2-sha256$600000$<salt_b64>$<hash_b64>`。
    pub fn to_storage(&self) -> String {
        format!("{}${}${}${}", self.alg, self.iter, self.salt, self.hash)
    }

    pub fn parse_storage(value: &str) -> Option<Self> {
        let mut parts = value.split('$');
        let alg = parts.next()?;
        let iter = parts.next()?.parse::<u32>().ok()?;
        let salt = parts.next()?;
        let hash = parts.next()?;
        if parts.next().is_some() || salt.is_empty() || hash.is_empty() {
            return None;
        }
        Some(Self {
            alg: alg.to_owned(),
            iter,
            salt: salt.to_owned(),
            hash: hash.to_owned(),
        })
    }
}

/// §7.2 第 3 步的接管留痕。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Takeover {
    #[serde(default)]
    pub old_machine_fp: String,
    #[serde(default)]
    pub at: String,
}

/// 远端 `<ID>/owner.json`（§7.1）。
///
/// 注意：`owner.json` 不写进 `<ID>/.index.json` 清单，后台搬运因此不会下载或覆盖别人的它。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Owner {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub app_ver: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub last_sync: String,
    #[serde(default)]
    pub machine_fp: String,
    #[serde(default)]
    pub session_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pwd: Option<PasswordHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pwd_set_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taken_over_from: Option<Takeover>,
}

impl Owner {
    /// 读取远端 `owner.json`；不存在或无法解析时返回 `None`。
    pub fn load(dav: &WebDav, id: &str) -> Result<Option<Self>> {
        let relative = format!("{id}/{}", crate::paths::OWNER_FILE);
        match dav.try_get_bytes(&relative)? {
            Some(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            None => Ok(None),
        }
    }

    /// 写入远端 `owner.json`（该 ID 的持有设备独占写入）。
    pub fn save(&self, dav: &WebDav, id: &str) -> Result<()> {
        let relative = format!("{id}/{}", crate::paths::OWNER_FILE);
        let bytes = serde_json::to_vec_pretty(self)?;
        dav.put_bytes(&relative, bytes, None)?;
        Ok(())
    }

    pub fn has_password(&self) -> bool {
        self.pwd.as_ref().is_some_and(|pwd| !pwd.hash.is_empty())
    }

    /// 用 owner.json 里的 salt/iter 校验密码（§7.2）。
    pub fn verify_password(&self, password: &str) -> bool {
        self.pwd.as_ref().is_some_and(|pwd| pwd.verify(password))
    }

    /// 展示给用户的设备信息（§7.2 弹窗）。
    pub fn describe(&self) -> String {
        let pwd_state = if self.has_password() {
            "已设置"
        } else {
            "未设置（老版本创建或手工建目录）"
        };
        format!(
            "主机名：{}\n首次注册：{}\n最后同步：{}\n设备指纹：{}\n密码状态：{}\n",
            or_unknown(&self.host),
            or_unknown(&self.created_at),
            or_unknown(&self.last_sync),
            crate::hash::short(&self.machine_fp),
            pwd_state,
        )
    }
}

fn or_unknown(value: &str) -> &str {
    if value.trim().is_empty() {
        "未知"
    } else {
        value
    }
}

/// 每次接管都签发新的 `session_token`，让旧机器下一轮自动停止同步（§7.2、§7.5）。
pub fn new_session_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    STANDARD.encode(bytes)
}

/// 本机指纹：随机 UUID 一次生成后长期保存，不做硬件探测（换硬件不该导致接管）。
pub fn new_machine_fp() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    crate::hash::sha256_hex(&bytes)
}

pub fn host_name() -> String {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(value) = std::env::var(key)
            && !value.trim().is_empty()
        {
            return value;
        }
    }
    "unknown-host".to_owned()
}

pub fn now_stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 日志/界面里展示用的短令牌。
pub fn short_token(token: &str) -> &str {
    if token.is_empty() {
        "(空)"
    } else {
        &token[..token.len().min(8)]
    }
}

/// 冲突副本文件名：`<name>.conflict-<ts>.<ext>`（§5.1）。
pub fn conflict_path(relative: &str) -> String {
    let stamp = chrono::Local::now().format("%Y%m%d%H%M%S").to_string();
    let path = Path::new(relative);
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| relative.to_owned());
    let extension = path
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| format!("{}/", parent.to_string_lossy().replace('\\', "/")))
        .unwrap_or_default();
    format!("{parent}{stem}.conflict-{stamp}{extension}")
}

/// §7.8 的设备配置导出格式（不含密码）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportedDevice {
    pub id: String,
    #[serde(default)]
    pub session_token: String,
    #[serde(default)]
    pub webdav_url: String,
    #[serde(default)]
    pub sync_files: Vec<String>,
    #[serde(default)]
    pub excluded: Vec<String>,
}

impl ExportedDevice {
    pub fn to_text(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn from_text(text: &str) -> Result<Self> {
        let parsed: Self = serde_json::from_str(text).context("设备配置不是有效的 JSON")?;
        crate::paths::validate_installation_id(&parsed.id).context("设备配置里的 ID 不合法")?;
        Ok(parsed)
    }
}

/// 断言密码满足最低要求（非空即可，长短由用户决定，但给出提示）。
pub fn validate_password_input(password: &str) -> Result<()> {
    if password.is_empty() {
        bail!("密码不能为空");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_string_round_trips() {
        let derived = PasswordHash::derive_with("secret", b"0123456789abcdef", 1000);
        let text = derived.to_storage();
        assert_eq!(PasswordHash::parse_storage(&text).unwrap(), derived);
        assert!(PasswordHash::parse_storage("plain-text").is_none());
    }

    #[test]
    fn verification_accepts_only_the_right_password() {
        // 用低迭代次数构造，测试不必跑满 60 万轮。
        let derived = PasswordHash::derive_with("s3cret", b"fixed-salt-16byt", 500);
        assert!(derived.verify("s3cret"));
        assert!(!derived.verify("s3cre"));
        assert!(!derived.verify(""));
    }

    #[test]
    fn conflict_copy_keeps_directory_and_extension() {
        let path = conflict_path("cn_dicts/base.dict.yaml");
        assert!(path.starts_with("cn_dicts/"));
        assert!(path.contains(".conflict-"));
        assert!(path.ends_with(".yaml"));
        assert_eq!(path.matches("conflict").count(), 1);
        assert!(!path.contains(".."));
    }

    #[test]
    fn exported_device_rejects_bad_id() {
        let text =
            r#"{"id":"../evil","session_token":"x","webdav_url":"","sync_files":[],"excluded":[]}"#;
        assert!(ExportedDevice::from_text(text).is_err());
    }
}
