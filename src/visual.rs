use crate::{
    context::{HistoryLimit, HistorySource, Message, Speaker, merge_visible, prepend_visible},
    wechat::{Anchor, Draft, Observation, is_wechat, native_focus},
};
use anyhow::{Context, Result};
use rusto::{DetectTextResult, InitializeConfig, OcrRunOptions, RustO, TextResult};
use std::{
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
    Graphics::Gdi::*,
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
            SetProcessDpiAwarenessContext,
        },
        Input::{Ime::ImmIsIME, KeyboardAndMouse::GetKeyboardLayout},
        WindowsAndMessaging::*,
    },
};

const MODEL: [(&str, &[u8]); 3] = [
    ("det.rten", include_bytes!("../assets/ocr/det.rten")),
    ("rec.rten", include_bytes!("../assets/ocr/rec.rten")),
    ("dict.txt", include_bytes!("../assets/ocr/dict.txt")),
];

#[derive(Clone)]
pub struct Proof {
    window: isize,
    bounds: Area,
    title: Area,
    input: Area,
    title_hash: u64,
    draft_hash: u64,
}

#[derive(PartialEq, Eq)]
pub enum DraftState {
    Changed,
    CaretHidden,
    Confirmed,
}

impl Proof {
    pub fn verify(&self, anchor: Anchor) -> DraftState {
        let window = HWND(self.window as _);
        if !available(window)
            || has_popup(window, anchor)
            || capture_bounds(window) != Some(self.bounds)
            || visible_surface(window, self.bounds).is_none_or(|area| area.w < self.input.right())
        {
            return DraftState::Changed;
        }
        let Some(title) = Screen::capture(self.title.offset(self.bounds.x, self.bounds.y)) else {
            return DraftState::Changed;
        };
        if title.hash(title.area.local(), false) != self.title_hash {
            return DraftState::Changed;
        }
        let Some(input) = Screen::capture(self.input.offset(self.bounds.x, self.bounds.y)) else {
            return DraftState::Changed;
        };
        let area = input.area.local();
        if input.hash(area, true) != self.draft_hash {
            return DraftState::Changed;
        }
        let dpi = dpi(window);
        match input.caret(area, dpi) {
            None => DraftState::CaretHidden,
            Some(caret)
                if (anchor.x - input.area.x - caret.0).abs() <= size(3, dpi)
                    && (anchor.y - input.area.y - caret.1).abs() <= size(3, dpi) =>
            {
                DraftState::Confirmed
            }
            Some(_) => DraftState::Changed,
        }
    }
}

#[derive(Default)]
pub struct Reader {
    ocr: Option<RustO>,
    initialization_failed: bool,
    current: Option<u64>,
    view: Option<(u64, Instant)>,
    revision: Option<u64>,
    attempted: HashSet<(u64, HistoryLimit)>,
    cache: HashMap<u64, Vec<Message>>,
    title: Option<(u64, String)>,
    caret: Option<(u64, (i32, i32, i32), Instant)>,
    draft: Option<(u64, EditorReading)>,
}

enum EditorReading {
    Blank,
    Ink(Vec<TextResult>),
}

impl Reader {
    pub fn clear(&mut self) {
        self.current = None;
        self.view = None;
        self.revision = None;
        self.attempted.clear();
        self.cache.clear();
        self.title = None;
        self.caret = None;
        self.draft = None;
    }

    pub fn observe(
        &mut self,
        window: HWND,
        limit: HistoryLimit,
        source: HistorySource,
    ) -> Observation {
        let mut result = Observation {
            draft: None,
            history: Vec::new(),
            status: "等待微信聊天窗口",
        };
        if !available(window) {
            self.caret = None;
            self.revision = None;
            return result;
        }
        let Some(bounds) = capture_bounds(window) else {
            result.status = "微信窗口无法截图";
            return result;
        };
        let Some(frame) = visible_surface(window, bounds).and_then(Screen::capture) else {
            result.status = "微信聊天区域被遮挡";
            return result;
        };
        let dpi = dpi(window);
        let Some(layout) = frame.layout(dpi) else {
            self.caret = None;
            result.status = "未找到微信聊天布局";
            crate::app::trace(format!(
                "layout miss area={}x{} at {}x{} dpi={}",
                frame.area.w, frame.area.h, frame.area.x, frame.area.y, dpi
            ));
            return result;
        };
        let title_hash = frame.hash(layout.title, false);
        let revision = crate::app::input_revision();
        if self.current != Some(title_hash) {
            self.current = Some(title_hash);
            self.view = None;
            self.draft = None;
            self.caret = None;
            self.revision = None;
        }
        if self.revision != Some(revision) {
            self.revision = Some(revision);
            self.caret = None;
            if source == HistorySource::WeFlowWithFallback {
                self.view = None;
            }
        }
        let draft_hash = frame.hash(layout.input, true);
        if let Some(caret) = frame.caret(layout.input, dpi) {
            self.caret = Some((draft_hash, caret, Instant::now()));
        }
        if self.ocr.is_none() && !self.initialization_failed {
            match prepare_model() {
                Ok(ocr) => self.ocr = Some(ocr),
                Err(_) => self.initialization_failed = true,
            }
        }
        let Some(ocr) = self.ocr.as_mut() else {
            result.status = "本地读屏模型无法启动";
            return result;
        };

        if source != HistorySource::WeFlowOnly {
            let body_hash = frame.hash_sparse(layout.body);
            if self.view.is_none_or(|(last, at)| {
                body_hash != last && at.elapsed() >= Duration::from_millis(450)
            }) {
                self.view = Some((body_hash, Instant::now()));
                if self.cache.len() >= 24 && !self.cache.contains_key(&title_hash) {
                    self.cache.clear();
                    self.attempted.clear();
                }
                let visible = self.cache.entry(title_hash).or_default();
                let messages = frame
                    .ocr(ocr, layout.body, false)
                    .map(|lines| frame.messages(&lines, layout.body, dpi))
                    .unwrap_or_default();
                if source == HistorySource::WeFlowWithFallback {
                    *visible = messages;
                } else {
                    merge_visible(visible, &messages);
                }
            }
            result.history = self.cache.get(&title_hash).cloned().unwrap_or_default();
        }
        let caret = self.caret.filter(|(hash, _, at)| {
            *hash == draft_hash && at.elapsed() < Duration::from_millis(850)
        });
        result.status = "微信读屏已连接，请将光标放入输入框";
        let Some((_, (x, y, height), _)) = caret else {
            return result;
        };
        let anchor = Anchor {
            x: frame.area.x + x,
            y: frame.area.y + y,
            height,
            left: frame.area.x + layout.input.x,
            top: frame.area.y + layout.input.y,
            right: frame.area.x + layout.input.right(),
        };
        if has_popup(window, anchor) {
            return result;
        }
        if self
            .draft
            .as_ref()
            .is_none_or(|(key, _)| *key != draft_hash)
        {
            let background = frame.rgb(layout.input.right() - 3, layout.input.bottom() - 3);
            let reading = if frame.editor_ink(layout.input, background).is_none() {
                EditorReading::Blank
            } else {
                EditorReading::Ink(frame.ocr(ocr, layout.input, true).unwrap_or_default())
            };
            self.draft = Some((draft_hash, reading));
        }
        let Some((_, reading)) = &self.draft else {
            return result;
        };
        let text = match reading {
            EditorReading::Blank => String::new(),
            EditorReading::Ink(lines) => {
                let Some(text) = draft_at_end(lines, layout.input, (x, y, height), dpi) else {
                    return result;
                };
                text
            }
        };
        if text.ends_with(|ch: char| ch.is_ascii_alphabetic())
            && unsafe {
                ImmIsIME(GetKeyboardLayout(GetWindowThreadProcessId(window, None))).as_bool()
            }
        {
            return result;
        }
        if source == HistorySource::Screen
            && !result.history.is_empty()
            && limit.select(&result.history).1 < limit.count()
            && self.attempted.insert((title_hash, limit))
            && let Some(history) = self.cache.get_mut(&title_hash)
        {
            prefetch(
                window, bounds, &frame, &layout, revision, limit, ocr, history,
            );
            result.history = history.clone();
        }
        let conversation_title = if source != HistorySource::Screen {
            if self
                .title
                .as_ref()
                .is_none_or(|(hash, _)| *hash != title_hash)
            {
                let title = frame.ocr(ocr, layout.title, false).ok().and_then(|lines| {
                    let [line] = lines.as_slice() else {
                        return None;
                    };
                    let text = line.text.trim();
                    (line.score >= 0.9
                        && !text.is_empty()
                        && !text.ends_with(['…', '.'])
                        && line.frame.left + line.frame.width
                            < (layout.title.w - size(2, dpi)) as f32)
                        .then(|| text.to_owned())
                });
                self.title = title.map(|name| (title_hash, name));
            }
            self.title.as_ref().map(|(_, title)| title.clone())
        } else {
            None
        };
        let Some(focus) = native_focus(window) else {
            return result;
        };
        result.draft = Some(Draft {
            window: window.0 as isize,
            focus,
            conversation: format!("screen:{}:{title_hash}", window.0 as isize),
            conversation_title,
            text,
            anchor,
            can_accept: true,
            proof: Some(Proof {
                window: window.0 as isize,
                bounds,
                title: layout.title,
                input: layout.input,
                title_hash,
                draft_hash,
            }),
        });
        result.status = "微信输入框已连接";
        result
    }
}

pub fn diagnose() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let until = Instant::now() + Duration::from_secs(8);
    let mut reader = Reader::default();
    let mut last = None;
    while Instant::now() < until {
        let window = unsafe { GetForegroundWindow() };
        if is_wechat(window) {
            let result = reader.observe(window, HistoryLimit::Messages(0), HistorySource::Screen);
            let details = (
                result.status,
                result.history.len(),
                result.draft.as_ref().map_or(0, |d| d.text.chars().count()),
            );
            if last != Some(details) {
                println!(
                    "{}; visible messages: {}; draft characters: {}",
                    details.0, details.1, details.2
                );
                last = Some(details);
            }
        }
        std::thread::sleep(Duration::from_millis(75));
    }
    if last.is_none() {
        println!("No foreground WeChat window");
    }
    Ok(())
}

fn prefetch(
    window: HWND,
    bounds: Area,
    initial: &Screen,
    layout: &Layout,
    revision: u64,
    limit: HistoryLimit,
    ocr: &mut RustO,
    history: &mut Vec<Message>,
) {
    let title = initial.hash(layout.title, false);
    let previously_visible = initial.hash_sparse(layout.body);
    let dpi = dpi(window);
    let x = initial.area.x + layout.body.x + layout.body.w / 2;
    let y = initial.area.y + layout.body.bottom() - size(100, dpi);
    let hit = unsafe { WindowFromPoint(POINT { x, y }) };
    if hit.is_invalid() || unsafe { GetAncestor(hit, GA_ROOT) } != window {
        return;
    }
    let coords = LPARAM(((y as u16 as usize) << 16 | x as u16 as usize) as isize);
    let scroll = |delta: i32| unsafe {
        PostMessageW(
            Some(hit),
            WM_MOUSEWHEEL,
            WPARAM((delta as u16 as usize) << 16),
            coords,
        )
        .is_ok()
    };
    let current = || {
        if !available(window)
            || capture_bounds(window) != Some(bounds)
            || crate::app::input_revision() != revision
        {
            return None;
        }
        let visible = visible_surface(window, bounds)?;
        if visible != initial.area {
            return None;
        }
        let frame = Screen::capture(visible)?;
        (frame.hash(layout.title, false) == title).then_some(frame)
    };
    const PAGE_STEP: i32 = 240;
    let started = Instant::now();
    let (budget, pages) = match limit {
        HistoryLimit::Messages(_) => (Duration::from_secs(4), 16),
        HistoryLimit::Rounds(_) => (Duration::from_secs(2), 12),
    };
    let mut ticks = 0;
    let mut last_view = previously_visible;
    for _ in 0..pages {
        if started.elapsed() >= budget
            || limit.select(history).1 >= limit.count()
            || current().is_none()
            || !scroll(PAGE_STEP)
        {
            break;
        }
        ticks += 1;
        std::thread::sleep(Duration::from_millis(180));
        let Some(frame) = current() else {
            break;
        };
        let view = frame.hash_sparse(layout.body);
        if view == last_view {
            break;
        }
        last_view = view;
        let Ok(lines) = frame.ocr(ocr, layout.body, false) else {
            break;
        };
        let older = frame.messages(&lines, layout.body, dpi);
        if !prepend_visible(history, &older) {
            break;
        }
    }
    if ticks > 0 && current().is_some() {
        let _ = scroll(-ticks * PAGE_STEP);
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn prepare_model() -> Result<RustO> {
    let directory =
        std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").context("找不到本地数据目录")?)
            .join("WeChatCompletion")
            .join("ocr-v6");
    std::fs::create_dir_all(&directory)?;
    for (name, bytes) in MODEL {
        let path = directory.join(name);
        if std::fs::metadata(&path).map_or(0, |meta| meta.len()) != bytes.len() as u64 {
            std::fs::write(path, bytes)?;
        }
    }
    rusto::init();
    let mut config = InitializeConfig::ppv6(
        directory.join("det.rten"),
        directory.join("rec.rten"),
        directory.join("dict.txt"),
    );
    config.det.limit_type = "max".into();
    config.det.limit_side_len = 1024;
    Ok(RustO::initialize(config)?)
}

fn available(window: HWND) -> bool {
    unsafe {
        if window.is_invalid() || window != GetForegroundWindow() || IsIconic(window).as_bool() {
            return false;
        }
        let mut affinity = 0;
        GetWindowDisplayAffinity(window, &mut affinity).is_ok() && affinity == WDA_NONE.0
    }
}

fn capture_bounds(window: HWND) -> Option<Area> {
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(window, &mut rect).ok()?;
    }
    let bounds = Area::new(
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    );
    let dpi = dpi(window);
    (bounds.w >= size(580, dpi)
        && bounds.h >= size(375, dpi)
        && u64::try_from(bounds.w).ok()? * u64::try_from(bounds.h).ok()? <= 18_000_000)
        .then_some(bounds)
}

fn visible_surface(window: HWND, bounds: Area) -> Option<Area> {
    let dpi = dpi(window);
    let first = size(330, dpi);
    let stride = size(13, dpi) as usize;
    let mut right = first;
    for x in (first..bounds.w - size(4, dpi)).step_by(stride) {
        let belongs = [17, 38, 57, 74, 87].iter().all(|percent| {
            let point = POINT {
                x: bounds.x + x,
                y: bounds.y + bounds.h * percent / 100,
            };
            unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) == window }
        });
        if !belongs {
            break;
        }
        right = x;
    }
    if right < size(550, dpi) {
        return None;
    }
    Some(Area::new(bounds.x, bounds.y, right, bounds.h))
}

fn dpi(window: HWND) -> i32 {
    unsafe { GetDpiForWindow(window).max(96) as i32 }
}

fn size(value: i32, dpi: i32) -> i32 {
    (value * dpi + 48) / 96
}

fn has_popup(window: HWND, anchor: Anchor) -> bool {
    let dpi = dpi(window);
    [10, 38, 75].iter().any(|&offset| {
        let point = POINT {
            x: anchor.x + size(offset, dpi),
            y: anchor.y + anchor.height + size(9, dpi),
        };
        if point.x >= anchor.right {
            return false;
        }
        unsafe {
            let hit = WindowFromPoint(point);
            !hit.is_invalid() && GetAncestor(hit, GA_ROOT) != window
        }
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Area {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Area {
    fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }
    fn right(self) -> i32 {
        self.x + self.w
    }
    fn bottom(self) -> i32 {
        self.y + self.h
    }
    fn local(self) -> Self {
        Self::new(0, 0, self.w, self.h)
    }
    fn offset(self, x: i32, y: i32) -> Self {
        Self::new(x + self.x, y + self.y, self.w, self.h)
    }
}

struct Layout {
    title: Area,
    body: Area,
    input: Area,
}

struct Screen {
    area: Area,
    pixels: Vec<u32>,
}

impl Screen {
    fn capture(area: Area) -> Option<Self> {
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return None;
            }
            let memory = CreateCompatibleDC(Some(screen));
            let result = (|| {
                if memory.is_invalid() {
                    return None;
                }
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biPlanes: 1,
                        biBitCount: 32,
                        biWidth: area.w,
                        biHeight: -area.h,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut bytes = std::ptr::null_mut();
                let bitmap =
                    CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bytes, None, 0)
                        .ok()?;
                let original = SelectObject(memory, HGDIOBJ(bitmap.0));
                let copied = BitBlt(
                    memory,
                    0,
                    0,
                    area.w,
                    area.h,
                    Some(screen),
                    area.x,
                    area.y,
                    SRCCOPY,
                );
                let pixels = copied.ok().map(|()| {
                    let _ = GdiFlush();
                    std::slice::from_raw_parts(bytes.cast::<u32>(), (area.w * area.h) as usize)
                        .to_vec()
                });
                SelectObject(memory, original);
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                pixels.map(|pixels| Self { area, pixels })
            })();
            if !memory.is_invalid() {
                let _ = DeleteDC(memory);
            }
            ReleaseDC(None, screen);
            result
        }
    }

    fn rgb(&self, x: i32, y: i32) -> [u8; 3] {
        let x = x.clamp(0, self.area.w - 1);
        let y = y.clamp(0, self.area.h - 1);
        let pixel = self.pixels[(y * self.area.w + x) as usize];
        [(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]
    }

    fn layout(&self, dpi: i32) -> Option<Layout> {
        let (w, h) = (self.area.w, self.area.h);
        let samples = (h / 5..h * 3 / 4).step_by(7).collect::<Vec<_>>();
        let left = (size(159, dpi)..size(400, dpi).min(w * 3 / 5))
            .map(|x| {
                (
                    x,
                    samples
                        .iter()
                        .filter(|&&y| gap(self.rgb(x - 1, y), self.rgb(x, y)) >= 12)
                        .count(),
                )
            })
            .max_by_key(|&(x, score)| (score, std::cmp::Reverse(x)))
            .filter(|&(_, score)| score * 100 >= samples.len() * 72)?
            .0;

        let ruler_start = left + size(38, dpi);
        let ruler_end = (w - size(36, dpi)).min(left + size(395, dpi));
        let samples = (ruler_start..ruler_end).step_by(8).collect::<Vec<_>>();
        if samples.len() < 20 {
            return None;
        }
        let divider = (h * 56 / 100..h - size(70, dpi))
            .map(|y| {
                (
                    y,
                    samples
                        .iter()
                        .filter(|&&x| gap(self.rgb(x, y - 1), self.rgb(x, y)) >= 12)
                        .count(),
                )
            })
            .max_by_key(|&(y, score)| (score, y))
            .filter(|&(_, score)| score * 100 >= samples.len() * 75)?
            .0;
        // The conversation panel can stop before a docked sidebar.
        let mut end_of_chat = ruler_end;
        let mut missed = 0;
        for x in (ruler_start..w - size(4, dpi)).step_by(8) {
            if (divider - 3..=divider + 3).any(|y| gap(self.rgb(x, y - 1), self.rgb(x, y)) >= 10) {
                missed = 0;
                end_of_chat = x;
            } else {
                missed += 1;
                if missed >= 5 && x > ruler_end {
                    break;
                }
            }
        }
        let right = (end_of_chat + size(11, dpi)).min(w);

        let title_x = left + size(10, dpi);
        let title = Area::new(
            title_x,
            size(38, dpi),
            size(266, dpi).min(right - title_x - size(50, dpi)),
            size(40, dpi),
        );
        let body_y = size(82, dpi);
        let body = Area::new(
            left + size(8, dpi),
            body_y,
            right - left - size(15, dpi),
            divider - body_y - size(7, dpi),
        );
        let input_y = divider + size(5, dpi);
        let input = Area::new(
            left + size(11, dpi),
            input_y,
            right - left - size(27, dpi),
            h - size(63, dpi) - input_y,
        );
        (input.h > size(20, dpi) && title.w >= size(65, dpi)).then_some(Layout {
            title,
            body,
            input,
        })
    }

    fn hash(&self, area: Area, ignore_caret: bool) -> u64 {
        let mut hash = DefaultHasher::new();
        let bg = self.rgb(area.right() - 2, area.bottom() - 3);
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let p = self.rgb(x, y);
                if ignore_caret && caret_color(p) {
                    bg.hash(&mut hash);
                } else {
                    p.hash(&mut hash);
                }
            }
        }
        hash.finish()
    }

    fn hash_sparse(&self, area: Area) -> u64 {
        let mut hash = DefaultHasher::new();
        for y in (area.y..area.bottom()).step_by(4) {
            for x in (area.x..area.right()).step_by(4) {
                self.rgb(x, y).hash(&mut hash);
            }
        }
        hash.finish()
    }

    fn caret(&self, region: Area, dpi: i32) -> Option<(i32, i32, i32)> {
        let mut best = None;
        for x in region.x..region.right() - size(3, dpi) {
            let background = self.rgb(x - 1, region.y);
            if caret_color(background) {
                continue;
            }
            let mut y = region.y;
            while y < region.bottom() {
                if !caret_color(self.rgb(x, y)) || caret_color(self.rgb(x - 1, y)) {
                    y += 1;
                    continue;
                }
                let start = y;
                while y < region.bottom()
                    && caret_color(self.rgb(x, y))
                    && !caret_color(self.rgb(x - 1, y))
                {
                    y += 1;
                }
                let h = y - start;
                let mid = start + h / 2;
                if h >= size(12, dpi)
                    && h <= size(29, dpi)
                    && !caret_color(self.rgb(x + size(4, dpi), mid))
                    && best.is_none_or(|(_, _, height)| h > height)
                {
                    best = Some((x, start, h));
                }
            }
        }
        best
    }

    fn ocr(&self, ocr: &mut RustO, area: Area, ignore_caret: bool) -> Result<Vec<TextResult>> {
        let background = self.rgb(area.right() - 3, area.bottom() - 3);
        let cropped = if ignore_caret {
            self.editor_ink(area, background)
        } else {
            Some(area)
        };
        let Some(cropped) = cropped else {
            return Ok(Vec::new());
        };
        let mut pixels = Vec::with_capacity((cropped.w * cropped.h * 3) as usize);
        let dark = luminance(background) < 128;
        for y in cropped.y..cropped.bottom() {
            for x in cropped.x..cropped.right() {
                let p = self.rgb(x, y);
                let gray = luminance(if ignore_caret && caret_color(p) {
                    background
                } else {
                    p
                });
                let normalized = if dark { 255 - gray } else { gray };
                pixels.extend([normalized; 3]);
            }
        }
        match ocr.detect_text_rgb(
            &pixels,
            cropped.w as u32,
            cropped.h as u32,
            (cropped.w * 3) as usize,
            &OcrRunOptions::default(),
        )? {
            DetectTextResult::Structured(mut lines) => {
                for line in &mut lines {
                    line.frame.left += (cropped.x - area.x) as f32;
                    line.frame.top += (cropped.y - area.y) as f32;
                }
                Ok(lines)
            }
            DetectTextResult::Spatial(_) => Ok(Vec::new()),
        }
    }

    fn editor_ink(&self, area: Area, background: [u8; 3]) -> Option<Area> {
        let mut left = area.right();
        let mut right = area.x;
        let mut top = area.bottom();
        let mut bottom = area.y;
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let pixel = self.rgb(x, y);
                if !caret_color(pixel) && luminance(pixel).abs_diff(luminance(background)) >= 60 {
                    left = left.min(x);
                    right = right.max(x);
                    top = top.min(y);
                    bottom = bottom.max(y);
                }
            }
        }
        if right <= left || bottom <= top {
            return None;
        }
        let left = (left - 9).max(area.x);
        let top = (top - 9).max(area.y);
        let right = (right + 10).min(area.right());
        let bottom = (bottom + 10).min(area.bottom());
        Some(Area::new(left, top, right - left, bottom - top))
    }

    fn messages(&self, lines: &[TextResult], area: Area, dpi: i32) -> Vec<Message> {
        let bg = self.rgb(area.x + 2, area.y + 8);
        let mut ordered = lines
            .iter()
            .filter(|line| line.score >= 0.68 && !line.text.trim().is_empty())
            .collect::<Vec<_>>();
        ordered.sort_by(|first, second| first.frame.top.total_cmp(&second.frame.top));
        let mut messages: Vec<Message> = Vec::new();
        let mut previous: Option<(Speaker, i32, i32)> = None;
        for line in ordered {
            let x = line.frame.left.round() as i32;
            let end = (line.frame.left + line.frame.width).round() as i32;
            let y = line.frame.top.round() as i32;
            let bottom = (line.frame.top + line.frame.height).round() as i32;
            let probe = self.rgb(area.x + x - size(4, dpi), area.y + (y + bottom) / 2);
            let aligned_left = (size(39, dpi)..=size(136, dpi)).contains(&x);
            let aligned_right = end >= area.w - size(120, dpi);
            let speaker = if green_bubble(probe) && end >= area.w / 2 {
                Speaker::Me
            } else if aligned_right
                && green_bubble(self.rgb(area.x + end + size(4, dpi), area.y + (y + bottom) / 2))
            {
                Speaker::Me
            } else if aligned_left && gap(probe, bg) >= 13 && !green_bubble(probe) {
                Speaker::Other
            } else {
                continue;
            };
            if previous.is_some_and(|(last, last_x, last_bottom)| {
                last == speaker
                    && (last_x - x).abs() <= size(6, dpi)
                    && (0..size(8, dpi)).contains(&(y - last_bottom))
            }) {
                if let Some(message) = messages.last_mut() {
                    message.text.push_str(line.text.trim());
                }
            } else {
                messages.push(Message {
                    speaker,
                    text: line.text.trim().into(),
                });
            }
            previous = Some((speaker, x, bottom));
        }
        messages
    }
}

fn draft_at_end(
    lines: &[TextResult],
    editor: Area,
    caret: (i32, i32, i32),
    dpi: i32,
) -> Option<String> {
    let mut lines = lines
        .iter()
        .filter(|line| !line.text.trim().is_empty())
        .collect::<Vec<_>>();
    lines.sort_by_key(|line| {
        (
            (line.frame.top as i32 + size(8, dpi)) / size(16, dpi),
            line.frame.left as i32,
        )
    });
    let last = lines.last()?;
    let right = editor.x + (last.frame.left + last.frame.width) as i32;
    let center = editor.y + (last.frame.top + last.frame.height * 0.5) as i32;
    if (caret.0 - right).abs() > size(15, dpi)
        || (caret.1 + caret.2 / 2 - center).abs() > size(10, dpi)
        || lines.iter().any(|line| line.score < 0.68)
    {
        return None;
    }
    let text = lines
        .iter()
        .map(|line| line.text.trim())
        .collect::<Vec<_>>()
        .join("\n");
    Some(text)
}

fn gap(first: [u8; 3], second: [u8; 3]) -> i32 {
    first
        .into_iter()
        .zip(second)
        .map(|(a, b)| i32::from(a.abs_diff(b)))
        .sum()
}

fn luminance([r, g, b]: [u8; 3]) -> u8 {
    ((u16::from(r) * 3 + u16::from(g) * 6 + u16::from(b)) / 10) as u8
}

fn caret_color([r, g, b]: [u8; 3]) -> bool {
    r < 48
        && (130..=219).contains(&g)
        && (55..=143).contains(&b)
        && g.saturating_sub(r) > 96
        && g.saturating_sub(b) >= 31
}

fn green_bubble([r, g, b]: [u8; 3]) -> bool {
    g.saturating_sub(r) >= 14 && g.saturating_sub(b) >= 10
}
