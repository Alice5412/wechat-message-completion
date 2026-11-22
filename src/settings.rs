use crate::{
    context::{HistoryLimit, HistorySource},
    style::{PROMPT_LIMIT, Profile},
};
use anyhow::{Context, Result, anyhow, ensure};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use windows::{
    Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
        CredReadW, CredWriteW,
    },
    core::{PCWSTR, PWSTR},
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub base_url: String,
    pub model: String,
    pub rounds: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_messages: Option<usize>,
    pub wechat_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weflow_url: Option<String>,
    pub weflow_images: bool,
    pub screen_history: bool,
    pub style: Profile,
    pub use_style: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub credential_slots: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-flash".into(),
            rounds: 20,
            history_messages: None,
            wechat_enabled: false,
            weflow_url: None,
            weflow_images: true,
            screen_history: true,
            style: Profile::default(),
            use_style: true,
            credential_slots: BTreeMap::new(),
        }
    }
}

impl Settings {
    pub fn active_style(&self) -> Option<&str> {
        let prompt = self.style.prompt.trim();
        (self.use_style && !prompt.is_empty()).then_some(prompt)
    }

    pub fn history_source(&self) -> HistorySource {
        match (self.weflow_url.is_some(), self.screen_history) {
            (_, false) => HistorySource::WeFlowOnly,
            (true, true) => HistorySource::WeFlowWithFallback,
            (false, true) => HistorySource::Screen,
        }
    }

    pub fn history_limit(&self) -> HistoryLimit {
        match self.history_messages {
            Some(count) => HistoryLimit::Messages(count),
            None => HistoryLimit::Rounds(self.rounds),
        }
    }

    pub fn set_history_count(&mut self, count: usize) {
        if let Some(value) = &mut self.history_messages {
            *value = count;
        } else {
            self.rounds = count;
        }
    }

    pub fn load() -> Result<Self> {
        match fs::read(settings_path()?) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("设置文件格式无效")?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    fn validate(&self) -> Result<()> {
        self.endpoint()?;
        self.weflow_endpoint()?;
        ensure!(!self.model.trim().is_empty(), "请输入模型名");
        ensure!(
            self.style.prompt.chars().count() <= PROMPT_LIMIT,
            "风格提示词最多 {PROMPT_LIMIT} 字"
        );
        let limit = self.history_limit();
        ensure!(
            (1..=100).contains(&limit.count()),
            "上下文应在 1 到 100 {}之间",
            limit.unit()
        );
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        self.validate()?;
        let path = settings_path()?;
        fs::create_dir_all(path.parent().context("设置目录不可用")?)?;
        let temp = path.with_extension(format!("{}.tmp", new_revision()));
        let result = (|| -> Result<()> {
            let json = serde_json::to_vec_pretty(self)?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(&json)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, &path).context("无法提交设置")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }

    pub fn save_with_keys(&mut self, api_key: &str, weflow_key: &str) -> Result<()> {
        self.validate()?;
        let api_key = api_key.trim();
        let weflow_key = weflow_key.trim();
        ensure!(api_key.len() <= 2048, "API Key 长度无效");
        ensure!(weflow_key.len() <= 2048, "WeFlow 本地令牌长度无效");
        let mut pending = Vec::new();
        if !api_key.is_empty() {
            pending.push((self.api_scope()?, api_key));
        }
        if !weflow_key.is_empty() {
            let endpoint = self.weflow_endpoint()?.context("请先填写 WeFlow 地址")?;
            pending.push((format!("WeChatCompletion/WeFlow/{endpoint}"), weflow_key));
        }
        let mut updated = self.clone();
        let mut staged = Vec::new();
        let mut retired = Vec::new();
        let result = (|| -> Result<()> {
            for (scope, key) in pending {
                let previous = updated
                    .credential_slots
                    .insert(scope.clone(), new_revision());
                if let Some(previous) = previous {
                    retired.push(wide(&format!("{scope}#slot={previous}")));
                }
                let target = updated.credential_target(&scope);
                save_key(target.clone(), key)?;
                staged.push(target);
            }
            updated.save()?;
            Ok(())
        })();
        if let Err(error) = result {
            for target in staged {
                delete_slot(&target);
            }
            return Err(error);
        }
        *self = updated;
        for target in retired {
            delete_slot(&target);
        }
        Ok(())
    }

    pub fn endpoint(&self) -> Result<Url> {
        let mut url = Url::parse(self.base_url.trim()).context("API 地址格式无效")?;
        let host = url.host_str().context("API 地址缺少主机")?;
        let local = matches!(host, "localhost" | "127.0.0.1" | "[::1]");
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && local),
            "API 地址使用 HTTPS，本机服务可使用 HTTP"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "API 地址不能包含凭据、查询参数或片段"
        );
        let path = url.path().trim_end_matches('/');
        if !path.ends_with("/chat/completions") {
            url.set_path(&format!("{path}/chat/completions"));
        }
        Ok(url)
    }

    fn api_scope(&self) -> Result<String> {
        let mut endpoint = self.endpoint()?;
        let path = endpoint
            .path()
            .trim_end_matches("/chat/completions")
            .to_owned();
        endpoint.set_path(&path);
        Ok(format!("WeChatCompletion/{}", endpoint.as_str()))
    }

    fn credential_target(&self, scope: &str) -> Vec<u16> {
        match self.credential_slots.get(scope) {
            Some(slot) => wide(&format!("{scope}#slot={slot}")),
            None => wide(scope),
        }
    }

    pub fn key(&self) -> Result<String> {
        read_key(&self.credential_target(&self.api_scope()?))
            .context("请在设置中填写该 API 地址的密钥")
    }

    pub fn save_key(&self, key: &str) -> Result<()> {
        save_key(self.credential_target(&self.api_scope()?), key)
    }

    pub fn weflow_endpoint(&self) -> Result<Option<Url>> {
        let Some(address) = self.weflow_url.as_deref() else {
            return Ok(None);
        };
        let url = Url::parse(address).context("WeFlow 地址格式无效")?;
        ensure!(
            url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
                && url.path() == "/"
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "WeFlow 地址须为本机 HTTP 地址，例如 http://127.0.0.1:5031"
        );
        Ok(Some(url))
    }

    pub fn weflow_key(&self, endpoint: &Url) -> Option<String> {
        read_key(&self.credential_target(&format!("WeChatCompletion/WeFlow/{endpoint}"))).ok()
    }
}

fn new_revision() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{}-{time:x}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn delete_slot(target: &[u16]) {
    unsafe {
        let _ = CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None);
    }
}

fn read_key(target: &[u16]) -> Result<String> {
    let mut credential = std::ptr::null_mut();
    unsafe {
        CredReadW(
            PCWSTR(target.as_ptr()),
            CRED_TYPE_GENERIC,
            None,
            &mut credential,
        )
        .map_err(|_| anyhow!("Windows 凭据管理器中没有该凭据"))?;
        let item = &*credential;
        let key = if item.CredentialBlobSize == 0 || item.CredentialBlob.is_null() {
            Err(anyhow!("该 API 地址的密钥为空"))
        } else {
            let bytes =
                std::slice::from_raw_parts(item.CredentialBlob, item.CredentialBlobSize as usize);
            std::str::from_utf8(bytes)
                .map(str::to_owned)
                .map_err(Into::into)
        };
        CredFree(credential.cast());
        key
    }
}

fn save_key(mut target: Vec<u16>, key: &str) -> Result<()> {
    let key = key.trim();
    ensure!(!key.is_empty() && key.len() <= 2048, "API Key 长度无效");
    let mut bytes = key.as_bytes().to_vec();
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        CredentialBlobSize: bytes.len() as u32,
        CredentialBlob: bytes.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    let result = unsafe { CredWriteW(&credential, 0) };
    bytes.fill(0);
    result.context("无法写入 Windows 凭据管理器")?;
    Ok(())
}

fn settings_path() -> Result<PathBuf> {
    Ok(
        PathBuf::from(env::var_os("LOCALAPPDATA").context("找不到本地应用数据目录")?)
            .join("WeChatCompletion")
            .join("settings.json"),
    )
}

pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_message_limit_preserves_legacy_round_settings() {
        let legacy: Settings = serde_json::from_str(r#"{"rounds":20}"#).unwrap();
        assert_eq!(legacy.history_limit(), HistoryLimit::Rounds(20));
        let mut messages: Settings =
            serde_json::from_str(r#"{"rounds":20,"history_messages":50}"#).unwrap();
        assert_eq!(messages.history_limit(), HistoryLimit::Messages(50));
        messages.set_history_count(45);
        assert_eq!(messages.history_limit(), HistoryLimit::Messages(45));
        assert_eq!(messages.rounds, 20);
    }

    #[test]
    fn screen_history_can_be_disabled_for_weflow() {
        let mut settings: Settings = serde_json::from_str("{}").unwrap();
        assert!(settings.history_source() == HistorySource::Screen);
        settings.weflow_url = Some("http://127.0.0.1:5031".into());
        assert!(settings.history_source() == HistorySource::WeFlowWithFallback);
        settings.screen_history = false;
        assert!(settings.history_source() == HistorySource::WeFlowOnly);
        settings.weflow_url = None;
        assert!(settings.history_source() == HistorySource::WeFlowOnly);
    }
}
