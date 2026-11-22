use crate::context::{HistoryLimit, HistorySource, Message, Speaker, merge_visible};
use anyhow::Result;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicIsize, Ordering::Relaxed},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT},
        Graphics::Gdi::ClientToScreen,
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize,
            },
            Ole::{
                SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound,
                SafeArrayUnaccessData,
            },
            Threading::{
                OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
                QueryFullProcessImageNameW,
            },
        },
        UI::{Accessibility::*, WindowsAndMessaging::*},
    },
    core::{BOOL, PWSTR},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub x: i32,
    pub y: i32,
    pub height: i32,
    pub left: i32,
    pub top: i32,
    pub right: i32,
}

#[derive(Clone)]
pub struct Draft {
    pub window: isize,
    pub focus: isize,
    pub conversation: String,
    pub conversation_title: Option<String>,
    pub text: String,
    pub anchor: Anchor,
    pub can_accept: bool,
    pub proof: Option<crate::visual::Proof>,
}

pub struct Observation {
    pub draft: Option<Draft>,
    pub history: Vec<Message>,
    pub status: &'static str,
}

pub struct Sample {
    pub window: isize,
    pub revision: u64,
    pub at: Instant,
    pub observation: Observation,
}

pub fn monitor(
    enabled: Arc<AtomicBool>,
    history_settings: Arc<Mutex<(HistoryLimit, HistorySource)>>,
    watch: Arc<AtomicIsize>,
) -> Receiver<Sample> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || unsafe {
        if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
            return;
        }
        {
            let mut reader =
                CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                    .ok()
                    .map(Reader::new);
            let mut visual = crate::visual::Reader::default();
            let mut was_enabled = false;
            let mut logged = false;
            loop {
                let is_enabled = enabled.load(Relaxed);
                if was_enabled && !is_enabled {
                    if let Some(reader) = &mut reader {
                        reader.last_window = 0;
                        reader.cache.clear();
                    }
                    visual.clear();
                }
                was_enabled = is_enabled;
                let target = watch.load(Relaxed);
                if target == 0 {
                    logged = false;
                }
                if is_enabled && target != 0 {
                    let window = GetForegroundWindow();
                    if window.0 as isize == target && is_wechat(window) {
                        let revision = crate::app::input_revision();
                        let (limit, source) = *history_settings.lock().unwrap();
                        let observation = reader
                            .as_mut()
                            .map(|reader| reader.observe(source))
                            .filter(|observed| observed.draft.is_some())
                            .unwrap_or_else(|| visual.observe(window, limit, source));
                        if !logged {
                            logged = true;
                            crate::app::trace(format!(
                                "sample window={} draft={} history={} uia={} status={}",
                                target,
                                observation.draft.is_some(),
                                observation.history.len(),
                                reader.is_some(),
                                observation.status
                            ));
                        }
                        if watch.load(Relaxed) == target
                            && crate::app::input_revision() == revision
                            && GetForegroundWindow() == window
                            && sender
                                .send(Sample {
                                    window: target,
                                    revision,
                                    at: Instant::now(),
                                    observation,
                                })
                                .is_err()
                        {
                            break;
                        }
                    }
                }
                // An idle helper does not read the WeChat window.
                thread::sleep(Duration::from_millis(80));
            }
        }
        CoUninitialize();
    });
    receiver
}

pub fn diagnose() -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let result = (|| {
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            let mut windows = Vec::new();
            EnumWindows(
                Some(collect_window),
                LPARAM((&mut windows as *mut Vec<isize>) as isize),
            )?;
            if windows.is_empty() {
                println!("No visible Windows WeChat window");
            }
            for raw in windows {
                let window = HWND(raw as _);
                let root = uia.ElementFromHandle(window)?;
                let elements = root.FindAll(TreeScope_Descendants, &uia.CreateTrueCondition()?)?;
                let mut inputs = 0;
                let mut messages = 0;
                for index in 0..elements.Length()?.min(1000) {
                    if let Ok(element) = elements.GetElement(index) {
                        let id = element
                            .CurrentAutomationId()
                            .unwrap_or_default()
                            .to_string();
                        inputs += usize::from(is_input(&id));
                        messages += usize::from(is_message_list(&id));
                    }
                }
                println!(
                    "WeChat UIA: {} descendants, {inputs} chat inputs, {messages} message lists",
                    elements.Length()?
                );
            }
            Ok(())
        })();
        CoUninitialize();
        result
    }
}

unsafe extern "system" fn collect_window(window: HWND, data: LPARAM) -> BOOL {
    unsafe {
        if IsWindowVisible(window).as_bool() && is_wechat(window) {
            let windows = &mut *(data.0 as *mut Vec<isize>);
            windows.push(window.0 as isize);
        }
    }
    true.into()
}

pub fn is_wechat(window: HWND) -> bool {
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(window, Some(&mut pid));
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut path = [0u16; 1024];
        let mut len = path.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_FORMAT(0),
            PWSTR(path.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        if result.is_err() {
            return false;
        }
        let file = String::from_utf16_lossy(&path[..len as usize]);
        let name = file.rsplit('\\').next().unwrap_or_default();
        name.eq_ignore_ascii_case("Weixin.exe") || name.eq_ignore_ascii_case("WeChat.exe")
    }
}

struct Reader {
    uia: IUIAutomation,
    last_window: isize,
    history_at: Instant,
    conversation: String,
    cache: HashMap<String, Vec<Message>>,
}

impl Reader {
    fn new(uia: IUIAutomation) -> Self {
        Self {
            uia,
            last_window: 0,
            history_at: Instant::now(),
            conversation: String::new(),
            cache: HashMap::new(),
        }
    }

    fn observe(&mut self, source: HistorySource) -> Observation {
        let window = unsafe { GetForegroundWindow() };
        let raw = window.0 as isize;
        let mut observation = Observation {
            draft: None,
            history: Vec::new(),
            status: "微信窗口未在前台",
        };
        if window.is_invalid() || !is_wechat(window) {
            self.last_window = 0;
            return observation;
        }
        observation.status = "微信未提供可用的聊天输入控件";
        let Ok(root) = (unsafe { self.uia.ElementFromHandle(window) }) else {
            return observation;
        };
        let Ok(focused) = (unsafe { self.uia.GetFocusedElement() }) else {
            return observation;
        };
        if !is_input(
            &unsafe { focused.CurrentAutomationId() }
                .unwrap_or_default()
                .to_string(),
        ) {
            return observation;
        }
        let (title, identity) = self.read_identity(&root).unwrap_or_default();
        if identity.is_empty() {
            observation.status = "正在确认当前联系人与消息";
            return observation;
        }
        let conversation = format!("{raw}:{identity}");
        if source != HistorySource::WeFlowOnly {
            if raw != self.last_window
                || conversation != self.conversation
                || self.history_at.elapsed() >= Duration::from_millis(650)
            {
                self.last_window = raw;
                self.history_at = Instant::now();
                self.conversation.clone_from(&conversation);
                if let Ok(messages) = self.read_history(&root) {
                    if self.cache.len() >= 24 && !self.cache.contains_key(&conversation) {
                        self.cache.clear();
                    }
                    merge_visible(
                        self.cache.entry(conversation.clone()).or_default(),
                        &messages,
                    );
                }
            }
            observation.history = self.cache.get(&conversation).cloned().unwrap_or_default();
        }
        observation.status = "等待输入框末尾的可用光标";
        observation.draft = read_draft(focused, window, &conversation, title).ok();
        if observation.draft.is_some() {
            observation.status = "微信输入框已连接";
        }
        observation
    }

    fn read_identity(&self, root: &IUIAutomationElement) -> Result<(String, String)> {
        let mut title = String::new();
        let mut selected = None;
        unsafe {
            let walker = self.uia.RawViewWalker()?;
            let mut pending = vec![(root.clone(), 0)];
            let mut count = 0;
            while let Some((item, depth)) = pending.pop() {
                count += 1;
                if count > 300 {
                    break;
                }
                let id = item.CurrentAutomationId().unwrap_or_default().to_string();
                if is_title(&id) {
                    let name = item.CurrentName().unwrap_or_default().to_string();
                    if !name.trim().is_empty() {
                        title = name;
                    }
                }
                if id.ends_with("session_list") {
                    if let Ok(pattern) = item.GetCurrentPatternAs::<IUIAutomationSelectionPattern>(
                        UIA_SelectionPatternId,
                    ) {
                        if let Ok(items) = pattern.GetCurrentSelection() {
                            if items.Length().unwrap_or(0) == 1 {
                                selected = items
                                    .GetElement(0)
                                    .ok()
                                    .and_then(|el| el.CurrentAutomationId().ok())
                                    .map(|id| id.to_string())
                                    .filter(|id| !id.is_empty());
                            }
                        }
                    }
                    continue;
                }
                if depth < 13 && !is_message_list(&id) {
                    if let Ok(mut child) = walker.GetFirstChildElement(&item) {
                        loop {
                            pending.push((child.clone(), depth + 1));
                            match walker.GetNextSiblingElement(&child) {
                                Ok(next) => child = next,
                                Err(_) => break,
                            }
                        }
                    }
                }
            }
        }
        let identity = if title.is_empty() {
            String::new()
        } else {
            selected.map_or_else(|| title.clone(), |id| format!("{title}:{id}"))
        };
        Ok((title, identity))
    }

    fn read_history(&self, root: &IUIAutomationElement) -> Result<Vec<Message>> {
        unsafe {
            let all = root.FindAll(TreeScope_Descendants, &self.uia.CreateTrueCondition()?)?;
            for index in 0..all.Length()?.min(1600) {
                let item = all.GetElement(index)?;
                let id = item.CurrentAutomationId().unwrap_or_default().to_string();
                if is_message_list(&id) {
                    return read_bubbles(&self.uia, &item);
                }
            }
        }
        Ok(Vec::new())
    }
}

fn is_input(id: &str) -> bool {
    id.ends_with("chat_input_field")
        || id.ends_with("chat_input")
        || id.ends_with("chat_input_edit")
}

fn is_title(id: &str) -> bool {
    [
        "chat_title",
        "chat_name",
        "chat_title_label",
        "chat_title_name",
    ]
    .iter()
    .any(|ending| id.ends_with(ending))
}

fn is_message_list(id: &str) -> bool {
    [
        "chat_message_list",
        "message_list",
        "msg_list",
        "chat_msg_list",
        "chat_content_list",
    ]
    .iter()
    .any(|ending| id.ends_with(ending))
}

fn read_bubbles(uia: &IUIAutomation, list: &IUIAutomationElement) -> Result<Vec<Message>> {
    let mut messages = Vec::new();
    unsafe {
        let region = list.CurrentBoundingRectangle()?;
        let children = list.FindAll(TreeScope_Children, &uia.CreateTrueCondition()?)?;
        for index in 0..children.Length()?.min(1000) {
            let item = children.GetElement(index)?;
            let name = item.CurrentName().unwrap_or_default().to_string();
            if name.is_empty() || item.CurrentIsOffscreen()?.as_bool() {
                continue;
            }
            let rect = item.CurrentBoundingRectangle()?;
            let width = region.right - region.left;
            if width <= 0 || rect.right <= rect.left {
                continue;
            }
            // A full-width or centered row has no reliable speaker classification.
            let left = rect.left - region.left;
            let right = region.right - rect.right;
            let speaker = if right - left > width / 7 {
                Speaker::Other
            } else if left - right > width / 7 {
                Speaker::Me
            } else {
                continue;
            };
            messages.push(Message {
                speaker,
                text: name,
            });
        }
    }
    Ok(messages)
}

fn read_draft(
    element: IUIAutomationElement,
    window: HWND,
    conversation: &str,
    title: String,
) -> Result<Draft> {
    unsafe {
        let pattern: IUIAutomationTextPattern2 = element.GetCurrentPatternAs(UIA_TextPattern2Id)?;
        let mut active = BOOL(0);
        let caret = pattern.GetCaretRange(&mut active)?;
        let document = pattern.DocumentRange()?;
        anyhow::ensure!(
            active.as_bool()
                && caret.CompareEndpoints(
                    TextPatternRangeEndpoint_Start,
                    &document,
                    TextPatternRangeEndpoint_End
                )? == 0,
            "光标不在草稿末尾",
        );
        let text = document.GetText(2048)?.to_string();
        let field = element.CurrentBoundingRectangle()?;
        let (x, y, height) = caret_rect(&caret)
            .or_else(|| native_caret(window))
            .ok_or_else(|| anyhow::anyhow!("没有可靠的光标位置"))?;
        anyhow::ensure!(
            x >= field.left && x <= field.right && y >= field.top - 4 && y <= field.bottom,
            "光标位置不可用"
        );
        let composition = element
            .GetCurrentPatternAs::<IUIAutomationTextEditPattern>(UIA_TextEditPatternId)
            .is_ok_and(|p| p.GetActiveComposition().is_ok());
        Ok(Draft {
            window: window.0 as isize,
            focus: native_focus(window).ok_or_else(|| anyhow::anyhow!("无法确认输入焦点"))?,
            conversation: conversation.into(),
            conversation_title: Some(title),
            text,
            anchor: Anchor {
                x,
                y,
                height,
                left: field.left + 5,
                top: field.top,
                right: field.right - 5,
            },
            can_accept: !composition,
            proof: None,
        })
    }
}

fn caret_rect(range: &IUIAutomationTextRange) -> Option<(i32, i32, i32)> {
    unsafe {
        let array = range.GetBoundingRectangles().ok()?;
        if array.is_null() {
            return None;
        }
        let result = (|| {
            if SafeArrayGetUBound(array, 1).ok()? - SafeArrayGetLBound(array, 1).ok()? != 3 {
                return None;
            }
            let mut data = std::ptr::null_mut();
            SafeArrayAccessData(array, &mut data).ok()?;
            let rect = std::slice::from_raw_parts(data.cast::<f64>(), 4);
            let point = (
                rect[0].round() as i32,
                rect[1].round() as i32,
                rect[3].round() as i32,
            );
            let _ = SafeArrayUnaccessData(array);
            (point.2 >= 8).then_some(point)
        })();
        let _ = SafeArrayDestroy(array);
        result
    }
}

pub fn native_caret(window: HWND) -> Option<(i32, i32, i32)> {
    let gui = gui_info(window)?;
    unsafe {
        if gui.hwndCaret.is_invalid() {
            return None;
        }
        let RECT {
            left, top, bottom, ..
        } = gui.rcCaret;
        let mut point = POINT { x: left, y: top };
        ClientToScreen(gui.hwndCaret, &mut point)
            .as_bool()
            .then_some((point.x, point.y, bottom - top))
    }
}

pub fn native_focus(window: HWND) -> Option<isize> {
    let focus = gui_info(window)?.hwndFocus;
    (!focus.is_invalid() && unsafe { GetAncestor(focus, GA_ROOT) } == window)
        .then_some(focus.0 as isize)
}

fn gui_info(window: HWND) -> Option<GUITHREADINFO> {
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        let thread = GetWindowThreadProcessId(window, None);
        if thread == 0 {
            return None;
        }
        GetGUIThreadInfo(thread, &mut info).ok()?;
    }
    Some(info)
}
