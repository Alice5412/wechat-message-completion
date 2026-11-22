use crate::{
    api,
    context::{AttachedImage, HistoryLimit, HistorySource, Message, Speaker, parse_transcript},
    overlay::Overlay,
    settings::{Settings, wide},
    style::{PROMPT_LIMIT, Profile},
    wechat::{self, Anchor, Draft, Sample},
    weflow,
};
use anyhow::{Context, Result, ensure};
use std::{
    cell::RefCell,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicIsize, AtomicU64, AtomicUsize, Ordering::Relaxed},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{COLORREF, GlobalFree, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
        },
        Graphics::Gdi::{
            COLOR_WINDOW, CreateFontW, DeleteObject, GetSysColor, GetSysColorBrush, HDC, HFONT,
            HGDIOBJ, OPAQUE, SetBkColor, SetBkMode, SetTextColor, TRANSPARENT,
        },
        System::{
            DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
            LibraryLoader::GetModuleHandleW,
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
            Ole::CF_UNICODETEXT,
        },
        UI::{
            Controls::{EM_GETSEL, EM_REPLACESEL, EM_SCROLLCARET, EM_SETLIMITTEXT, EM_SETSEL},
            HiDpi::{
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
                SetProcessDpiAwarenessContext,
            },
            Input::{
                Ime::{GCS_COMPSTR, ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext},
                KeyboardAndMouse::*,
            },
            Shell::*,
            WindowsAndMessaging::*,
        },
    },
    core::{PCWSTR, w},
};

const APPLY: u32 = WM_APP + 1;
const TRAY: u32 = WM_APP + 2;
const PREDICT: u32 = WM_APP + 3;
const START_WEFLOW: u32 = WM_APP + 4;
const SAVE: usize = 101;
const PROBE: usize = 102;
const COPY: usize = 103;
const QUIT: usize = 104;
const ENABLE: usize = 105;
const EXAMPLE: usize = 106;
const PROBE_WEFLOW: usize = 107;
const SCREEN_HISTORY: usize = 108;
const CONTEXT_PAGE: usize = 109;
const MANUAL_PAGE: usize = 110;
const CONTEXT_TEXT: usize = 111;
const STYLE_PAGE: usize = 112;
const LEARN_STYLE: usize = 113;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}
static APP_WINDOW: AtomicIsize = AtomicIsize::new(0);
static DRAFT_FIELD: AtomicIsize = AtomicIsize::new(0);
static ARMED_WINDOW: AtomicIsize = AtomicIsize::new(0);
static ACTIVE_WINDOW: AtomicIsize = AtomicIsize::new(0);
static INPUT_REVISION: AtomicU64 = AtomicU64::new(0);
static TAB_HELD: AtomicBool = AtomicBool::new(false);
static PREDICTION_KEY_HELD: AtomicBool = AtomicBool::new(false);

pub fn run() -> Result<()> {
    let instance_suffix = std::env::var("WECHAT_COMPLETION_INSTANCE").unwrap_or_default();
    let instance_suffix = instance_suffix
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(30)
        .collect::<String>();
    let class_name = wide(&format!("WeChatCompletionInlineSettings{instance_suffix}"));
    unsafe {
        let class_name = PCWSTR(class_name.as_ptr());
        if let Ok(existing) = FindWindowW(class_name, None) {
            let _ = PostMessageW(Some(existing), START_WEFLOW, WPARAM(0), LPARAM(0));
            let _ = ShowWindow(existing, SW_RESTORE);
            let _ = SetForegroundWindow(existing);
            return Ok(());
        }
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = HINSTANCE(GetModuleHandleW(None)?.0);
        let cursor = LoadCursorW(None, IDC_ARROW)?;
        let icon = LoadIconW(None, IDI_APPLICATION)?;
        let class = WNDCLASSW {
            hInstance: instance,
            hCursor: cursor,
            hIcon: icon,
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            lpfnWndProc: Some(window_proc),
            lpszClassName: class_name,
            ..Default::default()
        };
        ensure!(RegisterClassW(&class) != 0, "无法注册微信补全窗口");
        let window = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class.lpszClassName,
            w!("微信消息补全"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            700,
            680,
            None,
            None,
            Some(instance),
            None,
        )?;
        let app = App::new(window, icon)?;
        let start_in_tray =
            instance_suffix.is_empty() && app.tray_available && app.settings.wechat_enabled;
        APP_WINDOW.store(window.0 as isize, Relaxed);
        DRAFT_FIELD.store(app.controls.draft.0 as isize, Relaxed);
        APP.with(|slot| slot.replace(Some(app)));
        let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), Some(instance), 0)?;
        let mouse = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(instance), 0) {
            Ok(hook) => hook,
            Err(error) => {
                let _ = UnhookWindowsHookEx(keyboard);
                return Err(error.into());
            }
        };
        SetTimer(Some(window), 1, 24, None);
        let _ = PostMessageW(Some(window), START_WEFLOW, WPARAM(0), LPARAM(0));
        if !start_in_tray {
            let _ = ShowWindow(window, SW_SHOW);
        }
        let mut message = MSG::default();
        loop {
            let result = GetMessageW(&mut message, None, 0, 0).0;
            if result <= 0 {
                break;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let _ = UnhookWindowsHookEx(mouse);
        let _ = UnhookWindowsHookEx(keyboard);
        APP.with(|slot| slot.replace(None));
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        if message == WM_CTLCOLORSTATIC {
            let dc = HDC(wparam.0 as _);
            if GetDlgCtrlID(HWND(lparam.0 as _)) == CONTEXT_TEXT as i32 {
                // Read-only edit controls send WM_CTLCOLORSTATIC, but still scroll their text.
                SetBkColor(dc, COLORREF(GetSysColor(COLOR_WINDOW)));
                SetBkMode(dc, OPAQUE);
            } else {
                SetBkMode(dc, TRANSPARENT);
            }
            SetTextColor(dc, COLORREF(0x00232932));
            return LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize);
        }
        let handled = APP.with(|slot| {
            let Ok(mut slot) = slot.try_borrow_mut() else {
                return false;
            };
            let Some(app) = slot.as_mut() else {
                return false;
            };
            match message {
                WM_TIMER => app.tick(),
                APPLY => app.apply(wparam.0 as u64),
                PREDICT => app.predict(lparam.0, wparam.0 as u64),
                START_WEFLOW => app.start_weflow(),
                WM_COMMAND if wparam.0 >> 16 == BN_CLICKED as usize => match wparam.0 & 0xffff {
                    SAVE => app.save(),
                    PROBE => app.test_connection(),
                    PROBE_WEFLOW => app.test_weflow(),
                    COPY => app.copy(),
                    EXAMPLE => app.example(),
                    ENABLE => app.update_enabled(),
                    CONTEXT_PAGE => app.controls.show_page(Page::Context),
                    MANUAL_PAGE => app.controls.show_page(Page::Manual),
                    STYLE_PAGE => app.controls.show_page(Page::Style),
                    LEARN_STYLE => app.learn_style(),
                    QUIT => {
                        let _ = DestroyWindow(window);
                    }
                    _ => return false,
                },
                TRAY if matches!(lparam.0 as u32, WM_LBUTTONUP | WM_RBUTTONUP) => {
                    let _ = ShowWindow(window, SW_RESTORE);
                    let _ = SetForegroundWindow(window);
                }
                WM_CLOSE => {
                    if app.tray_available {
                        let _ = ShowWindow(window, SW_HIDE);
                    } else {
                        let _ = DestroyWindow(window);
                    }
                }
                _ => return false,
            }
            true
        });
        if handled {
            LRESULT(0)
        } else {
            DefWindowProcW(window, message, wparam, lparam)
        }
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, event: WPARAM, data: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let key = &*(data.0 as *const KBDLLHOOKSTRUCT);
            if !key.flags.contains(LLKHF_INJECTED) || key.vkCode != VK_PACKET.0 as u32 {
                let going_down = matches!(event.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
                if key.vkCode == VK_SPACE.0 as u32 {
                    if going_down && PREDICTION_KEY_HELD.load(Relaxed) {
                        return LRESULT(1);
                    }
                    if !going_down && PREDICTION_KEY_HELD.swap(false, Relaxed) {
                        return LRESULT(1);
                    }
                    if going_down
                        && GetAsyncKeyState(VK_CONTROL.0 as i32) < 0
                        && GetAsyncKeyState(VK_MENU.0 as i32) < 0
                        && ![VK_SHIFT, VK_LWIN, VK_RWIN]
                            .iter()
                            .any(|key| GetAsyncKeyState(key.0 as i32) < 0)
                    {
                        let foreground = GetForegroundWindow();
                        if APP.with(|slot| {
                            slot.try_borrow().ok().is_some_and(|slot| {
                                slot.as_ref().is_some_and(|app| app.shortcut_in(foreground))
                            })
                        }) {
                            let revision = invalidate();
                            if PostMessageW(
                                Some(HWND(APP_WINDOW.load(Relaxed) as _)),
                                PREDICT,
                                WPARAM(revision as usize),
                                LPARAM(foreground.0 as isize),
                            )
                            .is_ok()
                            {
                                PREDICTION_KEY_HELD.store(true, Relaxed);
                                return LRESULT(1);
                            }
                        }
                    }
                }
                if key.vkCode == VK_TAB.0 as u32 {
                    if going_down && TAB_HELD.load(Relaxed) {
                        return LRESULT(1);
                    }
                    if !going_down && TAB_HELD.swap(false, Relaxed) {
                        return LRESULT(1);
                    }
                    if going_down
                        && ![VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN]
                            .iter()
                            .any(|key| GetAsyncKeyState(key.0 as i32) < 0)
                    {
                        let target = ARMED_WINDOW.swap(0, Relaxed);
                        if target != 0 && GetForegroundWindow().0 as isize == target {
                            let parent = APP_WINDOW.load(Relaxed);
                            let own_editor = target != parent
                                || GetFocus().0 as isize == DRAFT_FIELD.load(Relaxed);
                            if own_editor
                                && PostMessageW(
                                    Some(HWND(parent as _)),
                                    APPLY,
                                    WPARAM(INPUT_REVISION.load(Relaxed) as usize),
                                    LPARAM(0),
                                )
                                .is_ok()
                            {
                                TAB_HELD.store(true, Relaxed);
                                return LRESULT(1);
                            }
                        }
                    }
                }
                let modifier = matches!(
                    VIRTUAL_KEY(key.vkCode as u16),
                    VK_SHIFT
                        | VK_LSHIFT
                        | VK_RSHIFT
                        | VK_CONTROL
                        | VK_LCONTROL
                        | VK_RCONTROL
                        | VK_MENU
                        | VK_LMENU
                        | VK_RMENU
                        | VK_LWIN
                        | VK_RWIN
                );
                if going_down && !modifier && belongs_to_active_window() {
                    invalidate();
                }
            }
        }
        CallNextHookEx(None, code, event, data)
    }
}

unsafe extern "system" fn mouse_hook(code: i32, event: WPARAM, data: LPARAM) -> LRESULT {
    if code >= 0
        && matches!(
            event.0 as u32,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MOUSEWHEEL | WM_MBUTTONDOWN
        )
    {
        if belongs_to_active_window() {
            invalidate();
        }
    }
    unsafe { CallNextHookEx(None, code, event, data) }
}

fn belongs_to_active_window() -> bool {
    let active = ACTIVE_WINDOW.load(Relaxed);
    active != 0 && unsafe { GetForegroundWindow().0 as isize } == active
}

fn invalidate() -> u64 {
    ARMED_WINDOW.store(0, Relaxed);
    INPUT_REVISION.fetch_add(1, Relaxed) + 1
}

pub(crate) fn trace(message: impl AsRef<str>) {
    use std::io::Write;
    if std::env::var_os("WECHAT_COMPLETION_TRACE").is_none() {
        return;
    }
    let Some(base) = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from) else {
        return;
    };
    let dir = base.join("WeChatCompletion");
    let _ = std::fs::create_dir_all(&dir);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("trace.log"))
    {
        let _ = writeln!(file, "{stamp} {}", message.as_ref());
    }
}

pub fn input_revision() -> u64 {
    INPUT_REVISION.load(Relaxed)
}

#[derive(Clone)]
struct Input {
    draft: Draft,
    history: Vec<Message>,
}

struct Candidate {
    trigger: Trigger,
    input: Input,
    text: String,
}

#[derive(Clone, Copy)]
struct Trigger {
    window: isize,
    revision: u64,
}

impl Trigger {
    fn is_current(self) -> bool {
        self.revision == input_revision()
            && unsafe { GetForegroundWindow().0 as isize } == self.window
    }
}

enum Prediction {
    Reading(Trigger, Instant),
    Running(Trigger, Input),
    Showing(Candidate),
    Applying(Candidate, Instant),
}

impl Prediction {
    fn trigger(&self) -> Trigger {
        match self {
            Self::Reading(trigger, _) | Self::Running(trigger, _) => *trigger,
            Self::Showing(candidate) | Self::Applying(candidate, _) => candidate.trigger,
        }
    }
}

enum ModelEvent {
    ServiceStarted(Result<Option<String>, String>),
    StyleProgress(usize, usize),
    StyleReady(Option<String>, Result<Profile, String>),
    Context(u64, RequestContext),
    End(u64, Result<Completion, String>),
    Test(String),
}

struct RequestContext {
    heading: String,
    transcript: String,
}

impl RequestContext {
    fn new(
        title: Option<&str>,
        source: &str,
        messages: &[Message],
        images: &[AttachedImage],
        draft: &str,
        uses_style: bool,
    ) -> Self {
        use std::fmt::Write;

        let title = title.unwrap_or("当前微信会话");
        let heading = format!(
            "{source} · {} 条 · {} 张附图 · {title}",
            messages.len(),
            images.len()
        );
        let mut transcript = String::new();
        if uses_style {
            transcript.push_str("本次使用了「我的回复风格」\n\n");
        }
        if !draft.is_empty() {
            let _ = writeln!(transcript, "触发时的草稿\n{draft}\n");
        }
        for (index, message) in messages.iter().enumerate() {
            let number = index + 1;
            let speaker = match message.speaker {
                Speaker::Me => "我",
                Speaker::Other => "对方",
            };
            let attachment = if images.iter().any(|image| image.message_number == number) {
                " · 随请求附图"
            } else {
                ""
            };
            let _ = writeln!(
                transcript,
                "{number}. {speaker}{attachment}\n{}\n",
                message.text
            );
        }
        if messages.is_empty() {
            transcript.push_str("这次请求没有聊天历史。");
        }
        Self {
            heading,
            transcript: transcript.replace("\r\n", "\n").replace('\n', "\r\n"),
        }
    }
}

struct Completion {
    text: String,
    history_status: String,
}

struct App {
    window: HWND,
    tray: NOTIFYICONDATAW,
    tray_available: bool,
    controls: Controls,
    overlay: Overlay,
    settings: Settings,
    enabled: Arc<AtomicBool>,
    history_settings: Arc<Mutex<(HistoryLimit, HistorySource)>>,
    watch: Arc<AtomicIsize>,
    observations: Receiver<Sample>,
    observed: Option<Sample>,
    events: Receiver<ModelEvent>,
    sender: Sender<ModelEvent>,
    workers: Arc<AtomicUsize>,
    prediction: Option<Prediction>,
    latest_wechat_revision: Option<u64>,
    starting_weflow: bool,
    learning_style: bool,
    status: String,
    status_until: Option<Instant>,
}

impl App {
    fn new(window: HWND, icon: HICON) -> Result<Self> {
        let settings = Settings::load()?;
        let overlay = Overlay::new()?;
        let controls = Controls::new(window, &settings)?;
        let mut tray = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            uID: 1,
            uCallbackMessage: TRAY,
            hIcon: icon,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            ..Default::default()
        };
        let tooltip = wide("微信消息补全");
        tray.szTip[..tooltip.len()].copy_from_slice(&tooltip);
        let tray_available = unsafe { Shell_NotifyIconW(NIM_ADD, &tray).as_bool() };
        let (sender, events) = mpsc::channel();
        let enabled = Arc::new(AtomicBool::new(settings.wechat_enabled));
        let history_settings = Arc::new(Mutex::new((
            settings.history_limit(),
            settings.history_source(),
        )));
        let watch = Arc::new(AtomicIsize::new(0));
        if settings.wechat_enabled {
            unsafe {
                SendMessageW(controls.enable, BM_SETCHECK, Some(WPARAM(1)), None);
            }
        }
        Ok(Self {
            window,
            tray,
            tray_available,
            controls,
            overlay,
            settings,
            enabled: enabled.clone(),
            history_settings: history_settings.clone(),
            watch: watch.clone(),
            observations: wechat::monitor(enabled, history_settings, watch),
            observed: None,
            events,
            sender,
            workers: Arc::new(AtomicUsize::new(0)),
            prediction: None,
            latest_wechat_revision: None,
            starting_weflow: false,
            learning_style: false,
            status: String::new(),
            status_until: None,
        })
    }

    fn shortcut_in(&self, window: HWND) -> bool {
        if window == self.window {
            unsafe { GetFocus() == self.controls.draft }
        } else {
            self.enabled.load(Relaxed) && wechat::is_wechat(window)
        }
    }

    fn predict(&mut self, window: isize, revision: u64) {
        let trigger = Trigger { window, revision };
        trace(format!(
            "predict window={window} revision={revision} current={} allowed={} enabled={}",
            trigger.is_current(),
            self.shortcut_in(HWND(window as _)),
            self.enabled.load(Relaxed)
        ));
        if !trigger.is_current() || !self.shortcut_in(HWND(window as _)) {
            return;
        }
        self.reset();
        ACTIVE_WINDOW.store(window, Relaxed);
        self.prediction = Some(Prediction::Reading(trigger, Instant::now()));
        if window != self.window.0 as isize {
            self.latest_wechat_revision = Some(revision);
            self.watch.store(window, Relaxed);
        }
        self.report("正在读取当前对话…".into());
    }

    fn tick(&mut self) {
        if self
            .prediction
            .as_ref()
            .is_some_and(|state| !state.trigger().is_current())
        {
            trace(format!(
                "tick reset: revision {} now {} foreground {}",
                self.prediction
                    .as_ref()
                    .map(|state| state.trigger().revision)
                    .unwrap_or(0),
                input_revision(),
                unsafe { GetForegroundWindow().0 as isize }
            ));
            let applying = matches!(self.prediction, Some(Prediction::Applying(..)));
            self.reset();
            if applying {
                self.report("焦点已变化，建议未填入".into());
            }
        }
        for sample in self.observations.try_iter() {
            if self.prediction.as_ref().is_some_and(|state| {
                let trigger = state.trigger();
                sample.window == trigger.window && sample.revision == trigger.revision
            }) {
                self.observed = Some(sample);
            }
        }
        if let Some(Prediction::Reading(trigger, at)) = self.prediction {
            if let Some(input) = self.read_input(trigger) {
                trace(format!(
                    "read_input ok draft_chars={} history={} title_detected={}",
                    input.draft.text.chars().count(),
                    input.history.len(),
                    input.draft.conversation_title.is_some()
                ));
                if input.draft.text.trim().is_empty()
                    && (trigger.window == self.window.0 as isize
                        || self.settings.history_source() == HistorySource::Screen)
                    && self
                        .settings
                        .history_limit()
                        .select(&input.history)
                        .0
                        .is_empty()
                {
                    self.reset();
                    self.report("当前会话没有可供参考的对话文字".into());
                } else if self.workers.load(Relaxed) >= 2 {
                    self.reset();
                    self.report("上一条请求还在结束，请稍后再按快捷键".into());
                } else {
                    self.start_request(trigger, input);
                }
            } else if at.elapsed() >= Duration::from_secs(7) {
                trace("read_input timeout");
                self.reset();
                self.report("请把光标放在聊天输入框，再按 Ctrl+Alt+空格".into());
            }
        }
        if let Some(Prediction::Applying(candidate, started)) = self
            .prediction
            .take_if(|state| matches!(state, Prediction::Applying(..)))
        {
            self.apply_candidate(candidate, started);
        }
        while let Ok(event) = self.events.try_recv() {
            match event {
                ModelEvent::ServiceStarted(result) => {
                    self.starting_weflow = false;
                    match result {
                        Ok(Some(message)) => self.report(message),
                        Ok(None) => {}
                        Err(error) => self.report(error),
                    }
                }
                ModelEvent::StyleProgress(done, total) => set_text(
                    self.controls.style_summary,
                    &format!("正在本机分析私聊 {}/{total}…", done + 1),
                ),
                ModelEvent::StyleReady(source, result) => self.finish_style(source, result),
                ModelEvent::Context(revision, context) => {
                    if self.latest_wechat_revision == Some(revision) {
                        self.controls.show_context(&context);
                    }
                }
                ModelEvent::End(revision, outcome) => self.show_result(revision, outcome),
                ModelEvent::Test(result) => self.report(result),
            }
        }
        if self
            .status_until
            .is_none_or(|until| Instant::now() >= until)
        {
            self.status_until = None;
            if unsafe { GetForegroundWindow() } == self.window {
                let mode = if unsafe { IsWindowVisible(self.controls.draft).as_bool() } {
                    "在草稿框按 Ctrl+Alt+空格测试"
                } else if self.enabled.load(Relaxed) {
                    "Ctrl+Alt+空格生成一条"
                } else {
                    "微信补全未开启"
                };
                self.display(mode.into());
            }
        }
    }

    fn read_input(&self, trigger: Trigger) -> Option<Input> {
        if !trigger.is_current() {
            return None;
        }
        if trigger.window == self.window.0 as isize {
            return self.controls.read_draft(self.window);
        }
        let sample = self.observed.as_ref().filter(|sample| {
            sample.window == trigger.window
                && sample.revision == trigger.revision
                && sample.at.elapsed() < Duration::from_millis(500)
        })?;
        let draft = sample
            .observation
            .draft
            .as_ref()
            .filter(|draft| draft.window == trigger.window && draft.can_accept)?;
        Some(Input {
            draft: draft.clone(),
            history: sample.observation.history.clone(),
        })
    }

    fn start_request(&mut self, trigger: Trigger, input: Input) {
        let workers = self.workers.clone();
        let sender = self.sender.clone();
        let settings = self.settings.clone();
        let limit = settings.history_limit();
        let history = limit.select(&input.history).0.to_vec();
        let draft = input.draft.text.clone();
        let focus = input.draft.focus;
        let title = input.draft.conversation_title.clone();
        let in_wechat = trigger.window != self.window.0 as isize;
        let history_source = settings.history_source();
        let use_weflow = in_wechat && history_source != HistorySource::Screen;
        if input.draft.proof.is_some() {
            // Screen observations can be verified directly before displaying or inserting.
            self.watch.store(0, Relaxed);
        }
        self.prediction = Some(Prediction::Running(trigger, input));
        self.report(if use_weflow {
            "正在查询本机 WeFlow 消息…".into()
        } else {
            "正在生成一条建议…".into()
        });
        workers.fetch_add(1, Relaxed);
        thread::spawn(move || {
            let outcome = (|| -> Result<Completion> {
                let is_current = || {
                    trigger.is_current()
                        && wechat::native_focus(HWND(trigger.window as _)) == Some(focus)
                };
                let mut images = Vec::new();
                let mut history = history;
                let mut source = "读屏";
                let mut local_problem = None;
                if use_weflow {
                    let local = (|| {
                        let service = weflow::Service::new(&settings, None)?;
                        let title = title.as_deref().context("未能确认当前微信会话标题")?;
                        let local =
                            service.history(title, limit, settings.weflow_images, is_current)?;
                        ensure!(!local.messages.is_empty(), "WeFlow 暂无当前会话的消息");
                        Ok(local)
                    })();
                    match local {
                        Ok(local) => {
                            source = "WeFlow";
                            history = local.messages;
                            images = local.images;
                            trace(format!(
                                "weflow ok messages={} images={}",
                                history.len(),
                                images.len()
                            ));
                        }
                        Err(error) if history_source == HistorySource::WeFlowOnly => {
                            trace(format!("weflow failed: {error}"));
                            return Err(error);
                        }
                        Err(error) => local_problem = Some(error.to_string()),
                    }
                }
                ensure!(is_current(), "本次预测已取消");
                ensure!(
                    !draft.trim().is_empty() || !history.is_empty(),
                    "{}",
                    local_problem
                        .as_deref()
                        .unwrap_or("当前会话没有可供参考的消息")
                );
                let history_status = if in_wechat {
                    let context = RequestContext::new(
                        title.as_deref(),
                        source,
                        &history,
                        &images,
                        &draft,
                        settings.active_style().is_some(),
                    );
                    let _ = sender.send(ModelEvent::Context(trigger.revision, context));
                    format!("{source} {} 条 · ", history.len())
                } else {
                    String::new()
                };
                let mut text = String::new();
                ensure!(is_current(), "本次预测已取消");
                api::stream(&settings, &history, &images, &draft, |suffix| {
                    text = suffix.to_owned();
                    is_current()
                })?;
                trace(format!(
                    "model completed suggestion_chars={} source={source}",
                    text.chars().count()
                ));
                Ok(Completion {
                    text,
                    history_status,
                })
            })()
            .map_err(|error| error.to_string());
            if let Err(problem) = &outcome {
                trace(format!("request failed: {problem}"));
            }
            workers.fetch_sub(1, Relaxed);
            let _ = sender.send(ModelEvent::End(trigger.revision, outcome));
        });
    }

    fn draft_state(&self, trigger: Trigger, input: &Input) -> crate::visual::DraftState {
        use crate::visual::DraftState;
        if !trigger.is_current()
            || wechat::native_focus(HWND(trigger.window as _)) != Some(input.draft.focus)
        {
            return DraftState::Changed;
        }
        if let Some(proof) = &input.draft.proof {
            return proof.verify(input.draft.anchor);
        }
        if self.read_input(trigger).is_some_and(|current| {
            current.draft.focus == input.draft.focus
                && current.draft.conversation == input.draft.conversation
                && current.draft.text == input.draft.text
                && current.draft.anchor == input.draft.anchor
        }) {
            DraftState::Confirmed
        } else {
            DraftState::Changed
        }
    }

    fn show_result(&mut self, revision: u64, outcome: Result<Completion, String>) {
        let Some(Prediction::Running(trigger, input)) = self.prediction.take_if(|state| {
            matches!(state, Prediction::Running(trigger, _) if trigger.revision == revision)
        }) else {
            return;
        };
        if self.draft_state(trigger, &input) == crate::visual::DraftState::Changed {
            trace("draft changed, suggestion dropped");
            self.reset();
            return;
        }
        let suggestion = match outcome {
            Ok(suggestion) => suggestion,
            Err(problem) => {
                trace(format!("show error: {problem}"));
                self.reset();
                if trigger.window != self.window.0 as isize
                    && self.settings.history_source() == HistorySource::WeFlowOnly
                    && self.tray_available
                {
                    let mut notice = self.tray;
                    notice.uFlags = NIF_INFO;
                    notice.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
                    for (buffer, value) in [
                        (&mut notice.szInfoTitle[..], "微信补全未生成"),
                        (&mut notice.szInfo[..], problem.as_str()),
                    ] {
                        let len = buffer.len() - 1;
                        for (slot, code) in buffer[..len].iter_mut().zip(value.encode_utf16()) {
                            *slot = code;
                        }
                    }
                    unsafe {
                        let _ = Shell_NotifyIconW(NIM_MODIFY, &notice);
                    }
                }
                self.report(problem);
                return;
            }
        };
        let displayed = self
            .overlay
            .show(input.draft.anchor, &suggestion.text)
            .unwrap_or(false);
        trace(format!(
            "overlay completed visible_chars={}",
            if displayed {
                suggestion.text.chars().count()
            } else {
                0
            }
        ));
        if !displayed || !trigger.is_current() {
            self.reset();
            self.report("本次没有可显示的建议".into());
            return;
        }
        self.prediction = Some(Prediction::Showing(Candidate {
            trigger,
            input,
            text: suggestion.text,
        }));
        ARMED_WINDOW.store(trigger.window, Relaxed);
        self.report(format!("{}按 Tab 填入建议", suggestion.history_status));
    }

    fn apply(&mut self, revision: u64) {
        // Verify the real draft without the preview covering part of the editor.
        self.overlay.hide();
        if let Some(Prediction::Showing(candidate)) = self.prediction.take()
            && candidate.trigger.revision == revision
        {
            self.apply_candidate(candidate, Instant::now());
        } else {
            self.reset();
        }
    }

    fn apply_candidate(&mut self, candidate: Candidate, started: Instant) {
        use crate::visual::DraftState;
        let result = match self.draft_state(candidate.trigger, &candidate.input) {
            DraftState::CaretHidden if started.elapsed() < Duration::from_millis(750) => {
                self.prediction = Some(Prediction::Applying(candidate, started));
                return;
            }
            DraftState::Changed | DraftState::CaretHidden => {
                Err(anyhow::anyhow!("无法确认当前草稿和光标，建议未填入"))
            }
            DraftState::Confirmed if candidate.trigger.window == self.window.0 as isize => {
                let text = wide(&candidate.text);
                unsafe {
                    SendMessageW(
                        self.controls.draft,
                        EM_REPLACESEL,
                        Some(WPARAM(1)),
                        Some(LPARAM(text.as_ptr() as isize)),
                    );
                }
                if get_text(self.controls.draft)
                    == format!("{}{}", candidate.input.draft.text, candidate.text)
                {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("未能确认建议已完整填入"))
                }
            }
            DraftState::Confirmed => insert_unicode(
                candidate.trigger,
                candidate.input.draft.focus,
                &candidate.text,
            ),
        };
        invalidate();
        self.reset();
        match result {
            Ok(()) => {
                trace("suggestion accepted");
                self.report("已提交建议到输入框".into());
            }
            Err(error) => {
                trace(format!("suggestion insertion failed: {error}"));
                self.report(error.to_string());
            }
        }
    }

    fn reset(&mut self) {
        self.prediction = None;
        self.observed = None;
        self.watch.store(0, Relaxed);
        ARMED_WINDOW.store(0, Relaxed);
        ACTIVE_WINDOW.store(0, Relaxed);
        self.overlay.hide();
    }

    fn save(&mut self) {
        let result = (|| -> Result<()> {
            let mut settings = self.settings.clone();
            settings.base_url = get_text(self.controls.base).trim().into();
            settings.model = get_text(self.controls.model).trim().into();
            let weflow_url = get_text(self.controls.weflow_url);
            let weflow_url = weflow_url.trim();
            settings.weflow_url = (!weflow_url.is_empty()).then(|| weflow_url.to_owned());
            settings.weflow_images = unsafe {
                SendMessageW(self.controls.weflow_images, BM_GETCHECK, None, None).0 == 1
            };
            settings.screen_history = unsafe {
                SendMessageW(self.controls.screen_history, BM_GETCHECK, None, None).0 == 1
            };
            settings.style.prompt = get_text(self.controls.style_prompt)
                .replace("\r\n", "\n")
                .trim()
                .into();
            settings.use_style = unsafe {
                SendMessageW(self.controls.style_enabled, BM_GETCHECK, None, None).0 == 1
            };
            settings.set_history_count(
                get_text(self.controls.history_count)
                    .trim()
                    .parse()
                    .context("请输入有效的历史消息数量")?,
            );
            let key = get_text(self.controls.key);
            let weflow_key = get_text(self.controls.weflow_key);
            settings.save_with_keys(&key, &weflow_key)?;
            set_text(self.controls.key, "");
            set_text(self.controls.weflow_key, "");
            *self.history_settings.lock().unwrap() =
                (settings.history_limit(), settings.history_source());
            self.settings = settings;
            self.reset();
            Ok(())
        })();
        self.report(
            result
                .err()
                .map_or_else(|| "设置已保存".into(), |error| error.to_string()),
        );
    }

    fn test_connection(&mut self) {
        self.report("正在用虚构对话测试连接…".into());
        let settings = self.settings.clone();
        let sender = self.sender.clone();
        thread::spawn(move || {
            let result = test_model(&settings);
            let message = result
                .err()
                .map_or_else(|| "API 已返回流式补全".into(), |error| error.to_string());
            let _ = sender.send(ModelEvent::Test(message));
        });
    }

    fn test_weflow(&mut self) {
        let mut settings = self.settings.clone();
        let address = get_text(self.controls.weflow_url);
        let token = get_text(self.controls.weflow_key);
        settings.weflow_url = Some(address.trim().to_owned()).filter(|value| !value.is_empty());
        let token = Some(token.trim().to_owned()).filter(|value| !value.is_empty());
        let sender = self.sender.clone();
        self.report("正在测试本机 WeFlow 接口…".into());
        thread::spawn(move || {
            let message = weflow::Service::new(&settings, token)
                .and_then(|service| service.test())
                .unwrap_or_else(|error| error.to_string());
            let _ = sender.send(ModelEvent::Test(message));
        });
    }

    fn start_weflow(&mut self) {
        if self.starting_weflow || self.settings.weflow_url.is_none() {
            return;
        }
        self.starting_weflow = true;
        let settings = self.settings.clone();
        let sender = self.sender.clone();
        thread::spawn(move || {
            let result =
                weflow::start_bundled_service(&settings).map_err(|error| error.to_string());
            let _ = sender.send(ModelEvent::ServiceStarted(result));
        });
    }

    fn learn_style(&mut self) {
        if self.learning_style {
            return;
        }
        self.learning_style = true;
        self.controls.set_style_learning(true);
        set_text(
            self.controls.style_summary,
            "正在检查本机 WeFlow 的私聊列表…",
        );
        let settings = self.settings.clone();
        let sender = self.sender.clone();
        thread::spawn(move || {
            let result = weflow::Service::new(&settings, None)
                .and_then(|service| {
                    service.learn_style(|done, total| {
                        sender.send(ModelEvent::StyleProgress(done, total)).is_ok()
                    })
                })
                .map_err(|error| error.to_string());
            let _ = sender.send(ModelEvent::StyleReady(settings.weflow_url, result));
        });
    }

    fn finish_style(&mut self, source: Option<String>, result: Result<Profile, String>) {
        self.learning_style = false;
        self.controls.set_style_learning(false);
        let result = result.and_then(|profile| {
            if source != self.settings.weflow_url {
                return Err("WeFlow 地址已更改，请重新生成风格提示词".into());
            }
            let mut settings = self.settings.clone();
            settings.style = profile;
            settings.save().map_err(|error| error.to_string())?;
            self.settings = settings;
            self.controls.show_style(&self.settings.style);
            Ok(())
        });
        match result {
            Ok(()) => self.report("回复风格提示词已保存".into()),
            Err(error) => {
                set_text(
                    self.controls.style_summary,
                    &self.settings.style.description(),
                );
                self.report(error);
            }
        }
    }

    fn update_enabled(&mut self) {
        let enabled = unsafe { SendMessageW(self.controls.enable, BM_GETCHECK, None, None).0 == 1 };
        self.enabled.store(enabled, Relaxed);
        self.settings.wechat_enabled = enabled;
        if let Err(error) = self.settings.save() {
            self.report(error.to_string());
        }
        self.reset();
    }

    fn copy(&mut self) {
        let text = get_text(self.controls.draft);
        if text.is_empty() {
            self.report("请先输入草稿".into());
        } else {
            let result = copy_to_clipboard(self.window, &text);
            self.report(
                result
                    .err()
                    .map_or_else(|| "草稿已复制".into(), |error| error.to_string()),
            );
        }
    }

    fn example(&mut self) {
        let history = if get_text(self.controls.history).is_empty() {
            "对方：几点到？\r\n我：快到了，你先找个位置\r\n对方：我在门口等，你还要多久？"
        } else {
            self.report("请清空历史框后再载入示例".into());
            return;
        };
        set_text(self.controls.history, history);
        if get_text(self.controls.draft).is_empty() {
            set_text(self.controls.draft, "我估计得");
            unsafe {
                let _ = SetFocus(Some(self.controls.draft));
                SendMessageW(
                    self.controls.draft,
                    EM_SETSEL,
                    Some(WPARAM(4)),
                    Some(LPARAM(4)),
                );
            }
        }
    }

    fn report(&mut self, status: String) {
        self.status_until = Some(Instant::now() + Duration::from_secs(5));
        self.display(status);
    }

    fn display(&mut self, status: String) {
        if self.status != status {
            set_text(self.controls.status, &status);
            self.status = status;
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        unsafe {
            if self.tray_available {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.tray);
            }
        }
    }
}

pub fn test_model(settings: &Settings) -> Result<String> {
    let settings = Settings {
        use_style: false,
        ..settings.clone()
    };
    let history =
        parse_transcript("对方：到哪了？\n我：快到了，你先进去吧\n对方：还需要一会儿吗？");
    let mut output = String::new();
    api::stream(&settings, &history, &[], "我估计得晚", |suffix| {
        output = suffix.to_owned();
        true
    })?;
    ensure!(!output.trim().is_empty(), "模型已连接，本次测试未返回补全");
    Ok(output)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Context,
    Manual,
    Style,
}

struct Controls {
    font: HFONT,
    base: HWND,
    model: HWND,
    key: HWND,
    history_count: HWND,
    weflow_url: HWND,
    weflow_key: HWND,
    weflow_images: HWND,
    screen_history: HWND,
    context_page: HWND,
    manual_page: HWND,
    style_page: HWND,
    context_heading: HWND,
    context_text: HWND,
    manual_controls: [HWND; 6],
    style_summary: HWND,
    style_prompt: HWND,
    style_enabled: HWND,
    style_learn: HWND,
    history: HWND,
    draft: HWND,
    enable: HWND,
    status: HWND,
}

impl Controls {
    fn new(window: HWND, settings: &Settings) -> Result<Self> {
        unsafe {
            let dpi = GetDpiForWindow(window).max(96) as i32;
            let scale = |value| value * dpi / 96;
            let _ = SetWindowPos(
                window,
                None,
                0,
                0,
                scale(700),
                scale(680),
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            let font = CreateFontW(
                -scale(15),
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                0,
                w!("Microsoft YaHei UI"),
            );
            let edit_class = w!("EDIT");
            let control = |class: PCWSTR,
                           title: &str,
                           id: usize,
                           style: WINDOW_STYLE,
                           rect: (i32, i32, i32, i32)|
             -> Result<HWND> {
                let edge = if class == edit_class {
                    WS_EX_CLIENTEDGE
                } else {
                    WINDOW_EX_STYLE(0)
                };
                let text = wide(title);
                let hwnd = CreateWindowExW(
                    edge,
                    class,
                    PCWSTR(text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | style,
                    scale(rect.0),
                    scale(rect.1),
                    scale(rect.2),
                    scale(rect.3),
                    Some(window),
                    (id != 0).then_some(HMENU(id as _)),
                    None,
                    None,
                )?;
                SendMessageW(
                    hwnd,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
                Ok(hwnd)
            };
            let label = |title: &str, rect| control(w!("STATIC"), title, 0, WINDOW_STYLE(0), rect);
            let edit = |title: &str, style: i32, rect| {
                control(
                    edit_class,
                    title,
                    0,
                    WS_TABSTOP | WINDOW_STYLE(style as u32),
                    rect,
                )
            };
            let button = |title: &str, id, rect| control(w!("BUTTON"), title, id, WS_TABSTOP, rect);
            label("模型连接", (24, 19, 200, 25))?;
            label("API 地址", (24, 55, 75, 25))?;
            let base = edit(&settings.base_url, ES_AUTOHSCROLL, (105, 50, 546, 28))?;
            label("模型", (24, 91, 75, 25))?;
            let model = edit(&settings.model, ES_AUTOHSCROLL, (105, 86, 326, 28))?;
            let limit = settings.history_limit();
            let history_label = match limit {
                HistoryLimit::Messages(_) => "消息条数",
                HistoryLimit::Rounds(_) => "对话轮数",
            };
            label(history_label, (461, 91, 85, 25))?;
            let history_count = edit(&limit.count().to_string(), ES_NUMBER, (558, 86, 93, 28))?;
            label("API Key", (24, 127, 75, 25))?;
            let key = edit("", ES_AUTOHSCROLL | ES_PASSWORD, (105, 122, 326, 28))?;
            button("保存设置", SAVE, (450, 121, 93, 29))?;
            button("测试连接", PROBE, (551, 121, 100, 29))?;
            let enable = control(
                w!("BUTTON"),
                "在微信中启用 Ctrl+Alt+空格（对话文字会发送至所选 API）",
                ENABLE,
                WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                (24, 164, 627, 30),
            )?;
            label("WeFlow 地址", (24, 209, 83, 25))?;
            let weflow_url = edit(
                settings.weflow_url.as_deref().unwrap_or(""),
                ES_AUTOHSCROLL,
                (112, 204, 321, 28),
            )?;
            let screen_history = control(
                w!("BUTTON"),
                "允许读屏获取聊天历史",
                SCREEN_HISTORY,
                WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                (450, 204, 201, 28),
            )?;
            SendMessageW(
                screen_history,
                BM_SETCHECK,
                Some(WPARAM(usize::from(settings.screen_history))),
                None,
            );
            label("本地令牌", (24, 245, 83, 25))?;
            let weflow_key = edit("", ES_AUTOHSCROLL | ES_PASSWORD, (112, 240, 220, 28))?;
            let weflow_images = control(
                w!("BUTTON"),
                "向模型发送图片和表情包",
                0,
                WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                (344, 239, 203, 29),
            )?;
            SendMessageW(
                weflow_images,
                BM_SETCHECK,
                Some(WPARAM(usize::from(settings.weflow_images))),
                None,
            );
            button("测试本地", PROBE_WEFLOW, (551, 239, 100, 29))?;
            let page = |title, id, rect| {
                control(
                    w!("BUTTON"),
                    title,
                    id,
                    WS_TABSTOP | WINDOW_STYLE((BS_PUSHLIKE | BS_RADIOBUTTON) as u32),
                    rect,
                )
            };
            let context_page = page("最近一次预测", CONTEXT_PAGE, (24, 286, 129, 29))?;
            let manual_page = page("手动测试", MANUAL_PAGE, (160, 286, 95, 29))?;
            let style_page = page("我的回复风格", STYLE_PAGE, (262, 286, 129, 29))?;
            let context_heading = label(
                "在微信按预测快捷键后，这里显示本次请求的上下文。",
                (24, 327, 627, 25),
            )?;
            let context_text = control(
                edit_class,
                "",
                CONTEXT_TEXT,
                WS_TABSTOP
                    | WS_VSCROLL
                    | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL | ES_READONLY) as u32),
                (24, 356, 627, 224),
            )?;
            let history_label = label(
                "对话历史 · 可粘贴「我：」和「对方：」开头的记录",
                (24, 327, 505, 25),
            )?;
            let example = button("填入示例", EXAMPLE, (551, 286, 100, 29))?;
            let history = edit(
                "",
                ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN | WS_VSCROLL.0 as i32,
                (24, 356, 627, 123),
            )?;
            let draft_label = label("草稿 · Ctrl+Alt+空格生成一次，Tab 填入", (24, 498, 500, 25))?;
            let draft = edit(
                "",
                ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN,
                (24, 525, 627, 55),
            )?;
            SendMessageW(draft, EM_SETLIMITTEXT, Some(WPARAM(2048)), None);
            SendMessageW(history, EM_SETLIMITTEXT, Some(WPARAM(200000)), None);
            let copy = button("复制草稿", COPY, (450, 591, 93, 29))?;
            let style_summary = label("", (24, 327, 627, 25))?;
            let style_prompt = edit(
                "",
                ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN | WS_VSCROLL.0 as i32,
                (24, 356, 627, 183),
            )?;
            SendMessageW(
                style_prompt,
                EM_SETLIMITTEXT,
                Some(WPARAM(PROMPT_LIMIT * 2)),
                None,
            );
            let style_enabled = control(
                w!("BUTTON"),
                "预测时使用 · 编辑后点保存设置",
                0,
                WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                (24, 550, 380, 29),
            )?;
            SendMessageW(
                style_enabled,
                BM_SETCHECK,
                Some(WPARAM(usize::from(settings.use_style))),
                None,
            );
            let style_learn = button("从聊天生成提示词", LEARN_STYLE, (482, 550, 169, 29))?;
            button("退出软件", QUIT, (551, 591, 100, 29))?;
            let status = label("准备就绪", (24, 597, 420, 24))?;
            let controls = Self {
                font,
                base,
                model,
                key,
                history_count,
                weflow_url,
                weflow_key,
                weflow_images,
                screen_history,
                context_page,
                manual_page,
                style_page,
                context_heading,
                context_text,
                manual_controls: [history_label, example, history, draft_label, draft, copy],
                style_summary,
                style_prompt,
                style_enabled,
                style_learn,
                history,
                draft,
                enable,
                status,
            };
            controls.show_style(&settings.style);
            controls.show_page(Page::Context);
            Ok(controls)
        }
    }

    fn show_page(&self, page: Page) {
        unsafe {
            for (button, button_page) in [
                (self.context_page, Page::Context),
                (self.manual_page, Page::Manual),
                (self.style_page, Page::Style),
            ] {
                SendMessageW(
                    button,
                    BM_SETCHECK,
                    Some(WPARAM(usize::from(page == button_page))),
                    None,
                );
            }
            for (controls, control_page) in [
                (&self.manual_controls[..], Page::Manual),
                (
                    &[self.context_heading, self.context_text][..],
                    Page::Context,
                ),
                (
                    &[
                        self.style_summary,
                        self.style_prompt,
                        self.style_enabled,
                        self.style_learn,
                    ][..],
                    Page::Style,
                ),
            ] {
                for &control in controls {
                    let _ = ShowWindow(
                        control,
                        if page == control_page {
                            SW_SHOW
                        } else {
                            SW_HIDE
                        },
                    );
                }
            }
        }
    }

    fn show_style(&self, profile: &Profile) {
        set_text(self.style_summary, &profile.description());
        set_text(self.style_prompt, &profile.prompt.replace("\n", "\r\n"));
    }

    fn set_style_learning(&self, learning: bool) {
        unsafe {
            let _ = EnableWindow(self.style_learn, !learning);
            let _ = EnableWindow(self.style_prompt, !learning);
        }
    }

    fn show_context(&self, context: &RequestContext) {
        set_text(self.context_heading, &context.heading);
        set_text(self.context_text, &context.transcript);
        unsafe {
            let end = GetWindowTextLengthW(self.context_text) as usize;
            SendMessageW(
                self.context_text,
                EM_SETSEL,
                Some(WPARAM(end)),
                Some(LPARAM(end as isize)),
            );
            SendMessageW(self.context_text, EM_SCROLLCARET, None, None);
        }
    }

    fn read_draft(&self, window: HWND) -> Option<Input> {
        unsafe {
            if GetFocus() != self.draft || in_composition(self.draft) {
                return None;
            }
            let text = get_text(self.draft);
            let mut start: u32 = 0;
            let mut end: u32 = 0;
            SendMessageW(
                self.draft,
                EM_GETSEL,
                Some(WPARAM((&mut start as *mut u32) as usize)),
                Some(LPARAM((&mut end as *mut u32) as isize)),
            );
            if start != end || end as usize != text.encode_utf16().count() {
                return None;
            }
            let (x, y, height) = wechat::native_caret(window)?;
            let mut field = RECT::default();
            GetWindowRect(self.draft, &mut field).ok()?;
            Some(Input {
                draft: Draft {
                    window: window.0 as isize,
                    focus: self.draft.0 as isize,
                    conversation: "panel".into(),
                    conversation_title: None,
                    text,
                    anchor: Anchor {
                        x,
                        y,
                        height,
                        left: field.left + 5,
                        top: field.top,
                        right: field.right - 5,
                    },
                    can_accept: true,
                    proof: None,
                },
                history: parse_transcript(&get_text(self.history)),
            })
        }
    }
}

impl Drop for Controls {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.font.0));
        }
    }
}

fn get_text(window: HWND) -> String {
    unsafe {
        let length = GetWindowTextLengthW(window).max(0) as usize;
        let mut text = vec![0; length + 1];
        let count = GetWindowTextW(window, &mut text).max(0) as usize;
        String::from_utf16_lossy(&text[..count])
    }
}

fn set_text(window: HWND, text: &str) {
    let text = wide(text);
    unsafe {
        let _ = SetWindowTextW(window, PCWSTR(text.as_ptr()));
    }
}

fn in_composition(window: HWND) -> bool {
    unsafe {
        let context = ImmGetContext(window);
        if context.is_invalid() {
            return false;
        }
        let length = ImmGetCompositionStringW(context, GCS_COMPSTR, None, 0);
        let _ = ImmReleaseContext(window, context);
        length > 0
    }
}

fn insert_unicode(trigger: Trigger, focus: isize, text: &str) -> Result<()> {
    let inputs: Vec<_> = text
        .encode_utf16()
        .flat_map(|unit| {
            [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP].map(|flags| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wScan: unit,
                        dwFlags: flags,
                        ..Default::default()
                    },
                },
            })
        })
        .collect();
    ensure!(
        trigger.is_current() && wechat::native_focus(HWND(trigger.window as _)) == Some(focus),
        "焦点已变化，建议未填入"
    );
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) } as usize;
    ensure!(sent > 0, "Windows 未接受填入请求");
    ensure!(sent == inputs.len(), "Windows 只接受了部分输入，请检查草稿");
    Ok(())
}

fn copy_to_clipboard(window: HWND, text: &str) -> Result<()> {
    let text = wide(text);
    unsafe {
        OpenClipboard(Some(window)).context("剪贴板正在使用")?;
        let result = (|| {
            let memory = GlobalAlloc(GMEM_MOVEABLE, text.len() * 2)?;
            let data = GlobalLock(memory);
            if data.is_null() {
                let _ = GlobalFree(Some(memory));
                anyhow::bail!("无法分配剪贴板内容");
            }
            std::ptr::copy_nonoverlapping(text.as_ptr(), data.cast::<u16>(), text.len());
            let _ = GlobalUnlock(memory);
            let handed_off = EmptyClipboard().and_then(|_| {
                SetClipboardData(
                    CF_UNICODETEXT.0 as u32,
                    Some(windows::Win32::Foundation::HANDLE(memory.0)),
                )
            });
            if handed_off.is_err() {
                let _ = GlobalFree(Some(memory));
            }
            handed_off?;
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}
