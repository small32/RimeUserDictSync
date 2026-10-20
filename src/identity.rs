use crate::webdav::{MkcolOutcome, PutOutcome, WebDav};
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

/// 设备用户动作的远端互斥锁；心跳独立保存，不参与 owner 的读改写。
pub struct OwnerLock<'a> {
    dav: &'a WebDav,
    relative: String,
}

impl<'a> OwnerLock<'a> {
    pub fn acquire(dav: &'a WebDav, id: &str) -> Result<Self> {
        crate::paths::validate_installation_id(id)?;
        let relative = format!("__rime-owner-lock-{}", id.to_ascii_lowercase());
        match dav.mkcol(&relative)? {
            MkcolOutcome::Created => Ok(Self { dav, relative }),
            MkcolOutcome::AlreadyExists => bail!(
                "该设备的设置正在另一端修改，请稍后重试；若上次操作异常中断，请确认没有其它设置操作后清理远端 {relative}/"
            ),
        }
    }
}

impl Drop for OwnerLock<'_> {
    fn drop(&mut self) {
        let _ = self.dav.delete_collection(&self.relative);
    }
}

impl Owner {
    /// owner 与 ETag 必须来自同一个 GET 响应，才能执行条件更新。
    pub fn load_versioned(dav: &WebDav, id: &str) -> Result<Option<(Self, Option<String>)>> {
        let relative = format!("{id}/{}", crate::paths::OWNER_FILE);
        dav.try_get_bytes_with_etag(&relative)?
            .map(|(bytes, etag)| {
                let owner = serde_json::from_slice(&bytes)
                    .context("远端 owner.json 无法解析，未覆盖原文件")?;
                Ok((owner, etag))
            })
            .transpose()
    }

    pub fn save_if_match(&self, dav: &WebDav, id: &str, etag: &str) -> Result<()> {
        let relative = format!("{id}/{}", crate::paths::OWNER_FILE);
        match dav.put_bytes(&relative, serde_json::to_vec_pretty(self)?, Some(etag))? {
            PutOutcome::Uploaded { .. } => Ok(()),
            PutOutcome::PreconditionFailed => bail!("设备身份在操作期间已变化，请重新读取后重试"),
        }
    }

    pub fn save_if_absent(&self, dav: &WebDav, id: &str) -> Result<()> {
        let relative = format!("{id}/{}", crate::paths::OWNER_FILE);
        match dav.put_bytes_if_absent(&relative, serde_json::to_vec_pretty(self)?)? {
            PutOutcome::Uploaded { .. } => Ok(()),
            PutOutcome::PreconditionFailed => bail!("设备身份已被另一台设备建立，请重新读取后重试"),
        }
    }
    /// 读取远端 `owner.json`；不存在或无法解析时返回 `None`。
    pub fn load(dav: &WebDav, id: &str) -> Result<Option<Self>> {
        let relative = format!("{id}/{}", crate::paths::OWNER_FILE);
        match dav.try_get_bytes(&relative)? {
            Some(bytes) => {
                let Some(mut owner): Option<Self> = serde_json::from_slice(&bytes).ok() else {
                    return Ok(None);
                };
                // 心跳失败不应阻止读取身份；旧 token 的心跳不会影响新持有者。
                if let Ok(Some(bytes)) = dav.try_get_bytes(&owner.heartbeat_relative(id))
                    && let Ok(heartbeat) = serde_json::from_slice::<serde_json::Value>(&bytes)
                    && let Some(last_sync) =
                        heartbeat.get("last_sync").and_then(|value| value.as_str())
                    && last_sync > owner.last_sync.as_str()
                {
                    owner.last_sync = last_sync.to_owned();
                }
                Ok(Some(owner))
            }
            None => Ok(None),
        }
    }

    pub fn heartbeat_relative(&self, id: &str) -> String {
        format!(
            "{id}/.last-sync-{}.json",
            crate::hash::sha256_hex(self.session_token.as_bytes())
        )
    }

    /// 写入远端 `owner.json`（该 ID 的持有设备独占写入）。
    pub fn save(&self, dav: &WebDav, id: &str) -> Result<()> {
        let relative = format!("{id}/{}", crate::paths::OWNER_FILE);
        let bytes = serde_json::to_vec_pretty(self)?;
        match dav.put_bytes(&relative, bytes, None)? {
            PutOutcome::Uploaded { .. } => Ok(()),
            PutOutcome::PreconditionFailed => bail!("远端拒绝更新设备身份，未保存 owner.json"),
        }
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

/// 文件名/目录名用途的随机串：小写十六进制。
///
/// 不要拿 `new_session_token` 当名字用——它是 STANDARD base64，字母表里的 `/`、`+`
/// 会把路径拆成多段（§9 探测目录、§4.4 C 档临时文件都中招）。
pub fn new_path_token(count: usize) -> String {
    let mut bytes = vec![0u8; count.div_ceil(2)];
    rand::thread_rng().fill_bytes(&mut bytes);
    crate::hash::sha256_hex(&bytes)[..count].to_owned()
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
        match token.char_indices().nth(8) {
            Some((index, _)) => &token[..index],
            None => token,
        }
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
