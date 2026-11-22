use crate::wechat::Anchor;
use anyhow::{Result, ensure};
use std::{ffi::c_void, ptr::null_mut};
use windows::{
    Win32::{
        Foundation::{COLORREF, HWND, POINT, SIZE},
        Graphics::Gdi::*,
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

pub struct Overlay {
    window: HWND,
    previous: Option<(Anchor, String)>,
}

impl Overlay {
    pub fn new() -> Result<Self> {
        unsafe {
            let window = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOPMOST,
                w!("STATIC"),
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )?;
            if std::env::var_os("WECHAT_COMPLETION_CAPTURABLE").is_none() {
                let _ = SetWindowDisplayAffinity(window, WDA_EXCLUDEFROMCAPTURE);
            }
            Ok(Self {
                window,
                previous: None,
            })
        }
    }

    pub fn hide(&mut self) {
        if self.previous.take().is_some() {
            unsafe {
                let _ = ShowWindow(self.window, SW_HIDE);
            }
        }
    }

    pub fn show(&mut self, anchor: Anchor, suggestion: &str) -> Result<bool> {
        if let Some((last_anchor, last_text)) = &self.previous
            && *last_anchor == anchor
            && last_text == suggestion
        {
            return Ok(true);
        }
        if suggestion.is_empty() {
            self.hide();
            return Ok(false);
        }
        match unsafe { self.paint(anchor, suggestion) } {
            Ok(()) => {
                self.previous = Some((anchor, suggestion.to_owned()));
                Ok(true)
            }
            Err(error) => {
                self.hide();
                Err(error)
            }
        }
    }

    unsafe fn paint(&self, anchor: Anchor, suggestion: &str) -> Result<()> {
        unsafe {
            let screen = GetDC(None);
            ensure!(!screen.is_invalid(), "无法获取屏幕绘制环境");
            let dc = CreateCompatibleDC(Some(screen));
            if dc.is_invalid() {
                ReleaseDC(None, screen);
                anyhow::bail!("无法创建灰字绘制环境");
            }
            let font = CreateFontW(
                -(anchor.height * 4 / 5).clamp(11, 36),
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
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
            let old_font = SelectObject(dc, HGDIOBJ(font.0));
            let drawn = (|| {
                let preview = Preview::layout(dc, anchor, suggestion)?;
                let width = preview.width;
                let height = preview.height;
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: width,
                        biHeight: -height,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut bits: *mut c_void = null_mut();
                let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
                let old_bitmap = SelectObject(dc, HGDIOBJ(bitmap.0));
                let pixel_count = (width * height) as usize;
                std::ptr::write_bytes(bits.cast::<u32>(), 0, pixel_count);
                SetTextColor(dc, COLORREF(0x00ffffff));
                SetBkMode(dc, TRANSPARENT);
                for (index, line) in preview.lines.iter().enumerate() {
                    let y = preview.padding + index as i32 * preview.line_height;
                    let _ = TextOutW(dc, preview.padding, y, line);
                }
                let _ = GdiFlush();
                let pixels = std::slice::from_raw_parts_mut(bits.cast::<u32>(), pixel_count);
                let background = GetPixel(
                    screen,
                    anchor.left + 3,
                    anchor.y + anchor.height.saturating_sub(2),
                );
                colorize(pixels, &preview, background.0 & 0xff < 90);
                let dimensions = SIZE {
                    cx: width,
                    cy: height,
                };
                let blend = BLENDFUNCTION {
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                    ..Default::default()
                };
                let result = UpdateLayeredWindow(
                    self.window,
                    Some(screen),
                    Some(&preview.point),
                    Some(&dimensions),
                    Some(dc),
                    Some(&POINT::default()),
                    COLORREF(0),
                    Some(&blend),
                    ULW_ALPHA,
                );
                SelectObject(dc, old_bitmap);
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                result?;
                SetWindowPos(
                    self.window,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                )?;
                Ok(())
            })();
            SelectObject(dc, old_font);
            let _ = DeleteObject(HGDIOBJ(font.0));
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            drawn
        }
    }
}

struct Preview {
    point: POINT,
    lines: Vec<Vec<u16>>,
    line_height: i32,
    padding: i32,
    width: i32,
    height: i32,
}

impl Preview {
    fn layout(dc: HDC, anchor: Anchor, text: &str) -> Result<Self> {
        let mut monitor = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let point = POINT {
            x: anchor.x,
            y: anchor.y,
        };
        unsafe {
            let display = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
            ensure!(
                GetMonitorInfoW(display, &mut monitor).as_bool(),
                "无法确定建议的显示区域"
            );
        }
        let work = monitor.rcWork;
        let right = anchor.right.min(work.right);
        let text_units: Vec<_> = text.encode_utf16().collect();
        let size = measure(dc, &text_units)?;
        if anchor.x >= work.left && anchor.x + size.cx + 4 <= right {
            return Ok(Self {
                point: POINT {
                    x: anchor.x + 1,
                    y: (anchor.y + (anchor.height - size.cy) / 2)
                        .clamp(work.top, (work.bottom - size.cy - 2).max(work.top)),
                },
                lines: vec![text_units],
                line_height: size.cy,
                padding: 0,
                width: size.cx + 3,
                height: size.cy + 2,
            });
        }

        let left = anchor.left.max(work.left);
        let padding = (size.cy / 3).max(6);
        let line_height = size.cy + 2;
        let content_width = (right - left).min(26 * size.cy) - 2 * padding;
        ensure!(content_width > 0, "当前输入框没有建议显示空间");
        let mut lines = Vec::new();
        let mut max_width = 0;
        let mut remaining = text;
        while !remaining.is_empty() {
            let mut line = Vec::new();
            let mut used = 0;
            let mut width = 0;
            for ch in remaining.chars() {
                let old_len = line.len();
                line.extend_from_slice(ch.encode_utf16(&mut [0; 2]));
                let next_width = measure(dc, &line)?.cx;
                if next_width > content_width {
                    line.truncate(old_len);
                    break;
                }
                width = next_width;
                used += ch.len_utf8();
            }
            ensure!(used > 0, "当前输入框没有建议显示空间");
            max_width = max_width.max(width);
            lines.push(line);
            remaining = &remaining[used..];
        }
        let width = max_width + 2 * padding;
        let height = line_height * lines.len() as i32 + 2 * padding;
        ensure!(height <= work.bottom - work.top, "当前屏幕无法显示完整建议");
        let gap = (size.cy / 5).max(4);
        let above = anchor.top - gap - height;
        let y = if above >= work.top {
            above
        } else {
            (anchor.y + anchor.height + gap).clamp(work.top, work.bottom - height)
        };
        Ok(Self {
            point: POINT {
                x: anchor.x.clamp(left, right - width),
                y,
            },
            lines,
            line_height,
            padding,
            width,
            height,
        })
    }
}

fn measure(dc: HDC, text: &[u16]) -> Result<SIZE> {
    let mut size = SIZE::default();
    let measured = unsafe { GetTextExtentPoint32W(dc, text, &mut size) };
    ensure!(measured.as_bool() && size.cy > 0, "无法测量建议文本");
    Ok(size)
}

fn colorize(pixels: &mut [u32], preview: &Preview, dark: bool) {
    for (index, pixel) in pixels.iter_mut().enumerate() {
        let coverage = *pixel & 0xff;
        let (alpha, gray) = if preview.padding == 0 {
            (coverage * 238 / 255, if dark { 172 } else { 148 })
        } else {
            let x = index % preview.width as usize;
            let y = index / preview.width as usize;
            let border = x == 0
                || y == 0
                || x + 1 == preview.width as usize
                || y + 1 == preview.height as usize;
            let ground = match (dark, border) {
                (true, false) => 38,
                (true, true) => 64,
                (false, false) => 247,
                (false, true) => 208,
            };
            let shade = if dark { 184 } else { 102 };
            (248, (coverage * shade + (255 - coverage) * ground) / 255)
        };
        let channel = alpha * gray / 255;
        *pixel = (alpha << 24) | (channel << 16) | (channel << 8) | channel;
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.window);
        }
    }
}
