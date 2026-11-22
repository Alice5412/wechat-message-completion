use crate::{
    context::{AttachedImage, HistoryLimit, Message, Speaker},
    payload::message_text,
    settings::Settings,
    style::{MESSAGES_PER_SESSION, Profile, SESSION_LIMIT as STYLE_SESSION_LIMIT, Samples},
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{
    StatusCode, Url,
    blocking::{Client, RequestBuilder, Response},
    redirect::Policy,
};
use serde::{Deserialize, de::DeserializeOwned};
use std::{
    collections::HashSet,
    env,
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    net::{SocketAddr, TcpStream},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use windows::Win32::{
    System::{Com::CoTaskMemFree, Threading::CREATE_NO_WINDOW},
    UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
};

const MAX_IMAGE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_IMAGES: usize = 3;
const LOCAL_PROTOCOL_VERSION: u32 = 2;

pub fn start_bundled_service(settings: &Settings) -> Result<Option<String>> {
    let Some(endpoint) = settings.weflow_endpoint()? else {
        return Ok(None);
    };
    let Some(bundle) = bundled_service_for(&endpoint)? else {
        return Ok(None);
    };
    let service = Service::new(settings, None)?;
    let port = endpoint
        .port_or_known_default()
        .context("本机历史服务端口无效")?;
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let listening = || TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok();
    if listening() {
        return check_running_bundle(&service, &bundle, false);
    }

    let python = python_windowed()?;
    let mut child = Command::new(python)
        .arg(bundle.directory.join("server.py"))
        .current_dir(&bundle.directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("无法启动本机历史服务")?;
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        if listening() {
            return check_running_bundle(&service, &bundle, true);
        }
        if child
            .try_wait()
            .context("无法检查本机历史服务进程")?
            .is_some()
        {
            if listening() {
                // The independent Windows startup entry may have won the bind race.
                return check_running_bundle(&service, &bundle, false);
            }
            bail!("本机历史服务启动失败，请查看服务日志");
        }
        thread::sleep(Duration::from_millis(200));
    }
    bail!("本机历史服务启动后暂未就绪，请稍后再试")
}

fn check_running_bundle(
    service: &Service,
    bundle: &Bundle,
    started: bool,
) -> Result<Option<String>> {
    let health = service.health()?;
    verify_account(&bundle.config, &health)?;
    if !health.current_protocol() {
        return Ok(Some("本机 WeFlow 正在运行旧版，请重启服务".into()));
    }
    if started {
        service.probe_messages()?;
        return Ok(Some("本机 WeFlow 服务已启动，消息可读".into()));
    }
    Ok(None)
}

fn bundled_directory(exe: &Path) -> Option<PathBuf> {
    let bin = exe.parent()?;
    let build_root = bin
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "target"))
        .and_then(Path::parent);
    [Some(bin), build_root]
        .into_iter()
        .flatten()
        .map(|root| root.join("weflow-local"))
        .find(|directory| {
            directory.join("server.py").is_file() && directory.join("config.json").is_file()
        })
}

struct Bundle {
    directory: PathBuf,
    config: BundleConfig,
}

#[derive(Deserialize)]
struct BundleConfig {
    port: Option<u16>,
    root: PathBuf,
    wxid: String,
}

fn bundled_service_for(endpoint: &Url) -> Result<Option<Bundle>> {
    if !matches!(endpoint.host_str(), Some("127.0.0.1" | "localhost")) {
        return Ok(None);
    }
    let exe = env::current_exe().context("无法确定补全程序所在目录")?;
    let Some(directory) = bundled_directory(&exe) else {
        return Ok(None);
    };
    let config = File::open(directory.join("config.json")).context("无法打开本机历史服务设置")?;
    let config: BundleConfig = serde_json::from_reader(config).context("本机历史服务设置无效")?;
    Ok(
        (endpoint.port_or_known_default() == Some(config.port.unwrap_or(5031)))
            .then_some(Bundle { directory, config }),
    )
}

fn verify_account(config: &BundleConfig, health: &Health) -> Result<()> {
    let home = env::var_os("USERPROFILE").map(PathBuf::from);
    let standard = home.as_ref().map(|home| home.join("xwechat_files"));
    verify_account_at(config, health, standard.as_deref())
}

fn verify_account_at(
    config: &BundleConfig,
    health: &Health,
    standard: Option<&Path>,
) -> Result<()> {
    ensure!(
        health.service.as_deref() == Some("weflow-local")
            && !config.wxid.trim().is_empty()
            && health.wxid.as_deref() == Some(&config.wxid),
        "本机服务的微信账号与配置不一致"
    );
    let root = fs::canonicalize(&config.root).context("找不到已配置的微信账号目录")?;
    if health.current_protocol() {
        let running_root = health
            .data_root
            .as_ref()
            .and_then(|path| fs::canonicalize(path).ok());
        ensure!(
            running_root.as_ref() == Some(&root),
            "本机服务正在读取其他微信数据目录，请核对配置并重启服务"
        );
    }
    let parent = root.parent().context("找不到微信账号目录的上级目录")?;
    let standard = standard
        .filter(|directory| directory.is_dir())
        .map(fs::canonicalize)
        .transpose()?;
    let directories =
        std::iter::once(parent).chain(standard.as_deref().filter(|dir| *dir != parent));
    let mut profiles = HashSet::new();
    for directory in directories {
        for profile in fs::read_dir(directory).context("无法检查本机微信账号目录")? {
            let path = profile?.path();
            if path.join("db_storage/session/session.db").is_file() {
                profiles.insert(fs::canonicalize(path)?);
            }
        }
    }
    ensure!(
        profiles.len() == 1 && profiles.contains(&root),
        "无法唯一确认本机微信数据账号，本次未读取聊天记录"
    );
    Ok(())
}

fn python_windowed() -> Result<PathBuf> {
    let path = unsafe {
        let folder = SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None)
            .context("无法定位本机 Python")?;
        let path = folder.to_string();
        CoTaskMemFree(Some(folder.0.cast()));
        path.context("本机 Python 路径无效")?
    };
    let python = PathBuf::from(path).join(r"Programs\Python\Python314\pythonw.exe");
    ensure!(python.is_file(), "找不到本机历史服务使用的 Python 3.14");
    Ok(python)
}

fn local_style(
    directory: &Path,
    mut progress: impl FnMut(usize, usize) -> bool,
) -> Result<Profile> {
    #[derive(Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum StyleRecord {
        Start { total: usize },
        Chat { index: usize },
        Text { text: String },
        End,
    }

    let mut child = Command::new(python_windowed()?.with_file_name("python.exe"))
        .arg(directory.join("server.py"))
        .arg("--style-samples")
        .current_dir(directory)
        .creation_flags(CREATE_NO_WINDOW.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("无法启动本机历史分析")?;
    let output = child.stdout.take().context("无法连接本机历史分析")?;
    let mut samples = Samples::default();
    let mut total = 0;
    let mut conversations = 0;
    let mut last_count = 0;
    let mut complete = false;
    for line in BufReader::new(output).lines() {
        let record: StyleRecord =
            serde_json::from_str(&line?).context("本机历史分析返回了无效状态")?;
        match record {
            StyleRecord::Start { total: count } => total = count,
            StyleRecord::Chat { index } => {
                conversations += usize::from(samples.count() > last_count);
                last_count = samples.count();
                ensure!(index < total && progress(index, total), "本次学习已取消");
            }
            StyleRecord::Text { text } => samples.observe(&text),
            StyleRecord::End => complete = true,
        }
    }
    let status = child.wait().context("无法确认本机历史分析已结束")?;
    ensure!(complete && status.success(), "本机历史分析未能读完");
    conversations += usize::from(samples.count() > last_count);
    let mut profile = samples.profile(conversations, 0)?;
    profile.archive = true;
    Ok(profile)
}

pub struct History {
    pub messages: Vec<Message>,
    pub images: Vec<AttachedImage>,
}

pub struct Service {
    endpoint: Url,
    token: Option<String>,
}

impl Service {
    pub fn new(settings: &Settings, token: Option<String>) -> Result<Self> {
        let endpoint = settings.weflow_endpoint()?.context("请填写 WeFlow 地址")?;
        let token = token.or_else(|| settings.weflow_key(&endpoint));
        Ok(Self { endpoint, token })
    }

    fn health(&self) -> Result<Health> {
        let status: Health = read_json(
            self.get(self.endpoint.join("api/v1/health")?)?
                .timeout(Duration::from_secs(2)),
        )?;
        ensure!(status.status == "ok", "WeFlow 服务未就绪");
        Ok(status)
    }

    fn probe_messages(&self) -> Result<()> {
        let sessions = self.sessions(&[("limit", "20")])?;
        for session in sessions
            .sessions
            .iter()
            .filter(|item| is_private(&item.username))
            .take(3)
        {
            if !self.messages(&session.username, 1, false)?.is_empty() {
                return Ok(());
            }
        }
        bail!("WeFlow 已连接，但近期私聊没有返回可读取的消息")
    }

    pub fn test(&self) -> Result<String> {
        let health = self.health()?;
        let bundle = bundled_service_for(&self.endpoint)?;
        if let Some(bundle) = &bundle {
            verify_account(&bundle.config, &health)?;
        }
        self.probe_messages()?;
        if bundle.is_some() && !health.current_protocol() {
            return Ok("WeFlow 消息可读 · 正在运行旧版，请重启服务".into());
        }
        Ok("WeFlow 已连接，私聊消息可读".into())
    }

    pub fn learn_style(&self, mut progress: impl FnMut(usize, usize) -> bool) -> Result<Profile> {
        if let Some(bundle) = bundled_service_for(&self.endpoint)? {
            verify_account(&bundle.config, &self.health()?)?;
            return local_style(&bundle.directory, progress);
        }
        let sessions = self.sessions(&[("limit", &STYLE_SESSION_LIMIT.to_string())])?;
        let mut seen = HashSet::new();
        let private = sessions
            .sessions
            .into_iter()
            .map(|session| session.username)
            .filter(|name| is_private(name) && seen.insert(name.clone()))
            .take(STYLE_SESSION_LIMIT)
            .collect::<Vec<_>>();
        ensure!(!private.is_empty(), "本机服务没有返回可用于学习的私聊");

        let total = private.len();
        let mut samples = Samples::default();
        let mut conversations = 0;
        let mut skipped = 0;
        let mut first_error = None;
        for (index, talker) in private.iter().enumerate() {
            ensure!(progress(index, total), "本次学习已取消");
            let before = samples.count();
            match self.messages(talker, MESSAGES_PER_SESSION, false) {
                Ok(rows) => {
                    for row in rows {
                        if let Some(text) = row.into_style_text() {
                            samples.observe(&text);
                        }
                    }
                    conversations += usize::from(samples.count() > before);
                }
                Err(error) => {
                    skipped += 1;
                    first_error.get_or_insert(error);
                }
            }
        }
        if skipped == total
            && let Some(error) = first_error
        {
            return Err(error.context("无法读取私聊历史"));
        }
        samples.profile(conversations, skipped)
    }

    pub fn history(
        &self,
        title: &str,
        limit: HistoryLimit,
        include_images: bool,
        is_current: impl Fn() -> bool,
    ) -> Result<History> {
        const SESSION_LIMIT: &str = "100";
        ensure!(is_current(), "本次预测已取消");
        if let Some(bundle) = bundled_service_for(&self.endpoint)? {
            verify_account(&bundle.config, &self.health()?)?;
        }
        ensure!(is_current(), "本次预测已取消");
        let sessions = self.sessions(&[
            ("title", title),
            ("keyword", title),
            ("limit", SESSION_LIMIT),
        ])?;
        ensure!(sessions.sessions.len() < 100, "WeFlow 的会话匹配结果过多");
        let talker = resolve_session(&sessions, title)?;
        ensure!(is_current(), "本次预测已取消");

        let count = match limit {
            HistoryLimit::Messages(count) => count.min(100),
            HistoryLimit::Rounds(_) => 200,
        };
        let mut rows = self.messages(talker, count, include_images)?;
        rows.sort_by_key(|m| (m.create_time, m.sort_seq.unwrap_or(m.local_id), m.local_id));
        let (messages, media): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .filter_map(MessageRow::into_message)
            .unzip();
        let (recent, _) = limit.select(&messages);
        let first = messages.len() - recent.len();
        let mut images = Vec::new();
        if include_images {
            for (index, url) in media[first..]
                .iter()
                .enumerate()
                .rev()
                .filter_map(|(index, url)| Some((index, url.as_ref()?)))
                .take(MAX_IMAGES)
            {
                ensure!(is_current(), "本次预测已取消");
                if let Ok(data_url) = self.image(url) {
                    images.push(AttachedImage {
                        message_number: index + 1,
                        data_url,
                    });
                }
            }
        }
        images.reverse();
        Ok(History {
            messages: recent.to_vec(),
            images,
        })
    }

    fn messages(
        &self,
        talker: &str,
        count: usize,
        include_images: bool,
    ) -> Result<Vec<MessageRow>> {
        let mut url = self.endpoint.join("api/v1/messages")?;
        url.query_pairs_mut()
            .append_pair("talker", talker)
            .append_pair("limit", &count.to_string());
        if include_images {
            url.query_pairs_mut().extend_pairs([
                ("media", "1"),
                ("image", "1"),
                ("emoji", "1"),
                ("voice", "0"),
                ("video", "0"),
            ]);
        }
        let response: MessagesResponse = read_json(self.get(url)?)?;
        ensure!(
            response.success && response.talker == talker,
            "WeFlow 未返回当前会话的消息"
        );
        Ok(response.messages)
    }

    fn sessions(&self, query: &[(&str, &str)]) -> Result<SessionsResponse> {
        let mut url = self.endpoint.join("api/v1/sessions")?;
        url.query_pairs_mut().extend_pairs(query.iter().copied());
        let response: SessionsResponse = read_json(self.get(url)?)?;
        ensure!(response.success, "WeFlow 无法读取会话列表");
        Ok(response)
    }

    fn image(&self, media_url: &str) -> Result<String> {
        let url = self.endpoint.join(media_url)?;
        ensure!(
            url.origin() == self.endpoint.origin()
                && url.path().starts_with("/api/v1/media/")
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none(),
            "图片地址不属于本机 WeFlow 服务"
        );
        let mut bytes = Vec::new();
        send(self.get(url)?.timeout(Duration::from_millis(900)))?
            .take(MAX_IMAGE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= MAX_IMAGE_BYTES, "图片超过大小限制");
        let mime_type = image_mime_type(&bytes).context("WeFlow 图片格式不支持")?;
        Ok(format!(
            "data:{mime_type};base64,{}",
            STANDARD.encode(bytes)
        ))
    }

    fn get(&self, url: Url) -> Result<RequestBuilder> {
        let request = client()?.get(url);
        Ok(match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        })
    }
}

fn resolve_session<'a>(response: &'a SessionsResponse, title: &str) -> Result<&'a str> {
    let mut matching = response
        .sessions
        .iter()
        .filter(|session| response.exact_title || session.display_name.trim() == title.trim());
    let session = matching
        .next()
        .context("WeFlow 未找到与微信标题匹配的会话")?;
    ensure!(matching.next().is_none(), "WeFlow 中存在同名会话");
    ensure!(
        is_private(&session.username),
        "当前会话不是 WeFlow 支持的私聊"
    );
    Ok(&session.username)
}

fn is_private(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('@')
        && !name.starts_with("gh_")
        && !matches!(
            name,
            "filehelper" | "weixin" | "fmessage" | "newsapp" | "notifymessage"
        )
}

fn client() -> Result<&'static Client> {
    static CLIENT: std::sync::OnceLock<Result<Client, reqwest::Error>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .no_proxy()
                .redirect(Policy::none())
                .connect_timeout(Duration::from_millis(500))
                .timeout(Duration::from_secs(4))
                .build()
        })
        .as_ref()
        .map_err(|_| anyhow::anyhow!("无法初始化 WeFlow 本地客户端"))
}

fn send(request: RequestBuilder) -> Result<Response> {
    let response = request.send().map_err(|error| {
        if error.is_timeout() {
            anyhow::anyhow!("WeFlow 本地接口请求超时")
        } else {
            anyhow::anyhow!("无法连接本机 WeFlow 服务")
        }
    })?;
    match response.status() {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            bail!("请检查 WeFlow 本地接口令牌")
        }
        StatusCode::CONFLICT => bail!("WeFlow 中存在匹配该标题的多个联系人"),
        status if !status.is_success() => bail!("WeFlow 返回 HTTP {}", status.as_u16()),
        _ => Ok(response),
    }
}

fn read_json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T> {
    serde_json::from_reader(send(request)?.take(8 * 1024 * 1024)).context("WeFlow 接口响应格式无效")
}

fn image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [137, b'P', b'N', b'G', 13, 10, 26, 10, ..] => Some("image/png"),
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    }
}

#[derive(Deserialize)]
struct Health {
    status: String,
    service: Option<String>,
    wxid: Option<String>,
    version: Option<u32>,
    #[serde(rename = "dataRoot")]
    data_root: Option<PathBuf>,
}

impl Health {
    fn current_protocol(&self) -> bool {
        self.version
            .is_some_and(|version| version >= LOCAL_PROTOCOL_VERSION)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionsResponse {
    success: bool,
    #[serde(default)]
    exact_title: bool,
    sessions: Vec<Session>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    username: String,
    display_name: String,
}

#[derive(Deserialize)]
struct MessagesResponse {
    success: bool,
    talker: String,
    messages: Vec<MessageRow>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Direction {
    Number(i64),
    Flag(bool),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageRow {
    create_time: i64,
    local_id: i64,
    sort_seq: Option<i64>,
    local_type: Option<i64>,
    is_send: Option<Direction>,
    content: Option<String>,
    parsed_content: Option<String>,
    media_type: Option<String>,
    media_url: Option<String>,
}

impl MessageRow {
    fn into_style_text(self) -> Option<String> {
        if !matches!(
            self.is_send,
            Some(Direction::Number(1) | Direction::Flag(true))
        ) || self.local_type.is_some_and(|kind| kind as u32 != 1)
            || self.media_type.is_some()
        {
            return None;
        }
        Some(message_text(
            self.local_type,
            self.parsed_content,
            self.content,
        ))
    }

    fn into_message(self) -> Option<(Message, Option<String>)> {
        let speaker = match self.is_send? {
            Direction::Number(1) | Direction::Flag(true) => Speaker::Me,
            Direction::Number(0) | Direction::Flag(false) => Speaker::Other,
            Direction::Number(_) => return None,
        };
        let (label, media) = match self.media_type.as_deref() {
            Some("image") => (Some("[图片]"), self.media_url),
            Some("emoji") => (Some("[表情包]"), self.media_url),
            Some("voice") => (Some("[语音]"), None),
            Some("video") => (Some("[视频]"), None),
            _ => (None, None),
        };
        let text = label
            .map(str::to_owned)
            .unwrap_or_else(|| message_text(self.local_type, self.parsed_content, self.content));
        Some((Message { speaker, text }, media))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_only_a_unique_exact_private_chat() {
        let response: SessionsResponse = serde_json::from_value(serde_json::json!({
            "success": true,
            "sessions": [
                {"username": "wxid_one", "displayName": "小明"},
                {"username": "wxid_two", "displayName": "小明"},
                {"username": "wxid_three", "displayName": "小明显"},
                {"username": "456@chatroom", "displayName": "项目群"}
            ]
        }))
        .unwrap();
        assert!(resolve_session(&response, "小明").is_err());
        assert!(resolve_session(&response, "小").is_err());
        assert!(resolve_session(&response, "项目群").is_err());
        assert_eq!(resolve_session(&response, "小明显").unwrap(), "wxid_three");
    }

    #[test]
    fn accepts_a_unique_server_verified_alias() {
        let mut response: SessionsResponse = serde_json::from_value(serde_json::json!({
            "success": true,
            "exactTitle": true,
            "sessions": [{"username": "wxid_one", "displayName": "备注名"}]
        }))
        .unwrap();
        assert_eq!(resolve_session(&response, "昵称").unwrap(), "wxid_one");
        response.sessions.push(Session {
            username: "wxid_two".into(),
            display_name: "昵称".into(),
        });
        assert!(resolve_session(&response, "昵称").is_err());
    }

    #[test]
    fn rejects_other_source_and_multiple_local_profiles() -> Result<()> {
        let temp = env::temp_dir().join(format!("wx-profile-fixture-{}", std::process::id()));
        fs::create_dir(&temp)?;
        let run = (|| -> Result<()> {
            let profile = temp.join("profile");
            let db_dir = profile.join("db_storage/session");
            fs::create_dir_all(&db_dir)?;
            File::create(db_dir.join("session.db"))?;
            let config = BundleConfig {
                port: None,
                root: profile.clone(),
                wxid: "profile".into(),
            };
            let mut health = Health {
                status: "ok".into(),
                service: Some("weflow-local".into()),
                wxid: Some(config.wxid.clone()),
                version: Some(LOCAL_PROTOCOL_VERSION),
                data_root: Some(profile.clone()),
            };
            verify_account_at(&config, &health, None)?;
            health.data_root = Some(temp.clone());
            assert!(verify_account_at(&config, &health, None).is_err());
            health.data_root = Some(profile);
            let other = temp.join("other/db_storage/session");
            fs::create_dir_all(&other)?;
            File::create(other.join("session.db"))?;
            assert!(verify_account_at(&config, &health, None).is_err());
            Ok(())
        })();
        fs::remove_dir_all(&temp)?;
        run
    }
}
