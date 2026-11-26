use std::result::Result::Ok;
use std::{
    io, ptr, slice,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM},
        Graphics::{Gdi::*, GdiPlus::*},
        UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
    },
    core::{PCWSTR, w},
};

use crate::win::{os_error, wide};

const CLASS: PCWSTR = w!("MSIToolkit.Osd.v1");
const PARGB32: i32 = 0x000e_200b;
pub(crate) const HIDE_TIMER: usize = 1;

pub(crate) struct Osd {
    owner: HWND,
    instance: HINSTANCE,
    registered: bool,
    visible: Option<(Window, Instant)>,
}

impl Osd {
    pub(crate) fn new(owner: HWND, instance: HINSTANCE) -> Self {
        Self {
            owner,
            instance,
            registered: false,
            visible: None,
        }
    }

    pub(crate) fn show(&mut self, title: &str, detail: &str, duration_ms: u16) -> io::Result<()> {
        self.hide();
        if !self.registered {
            let class = WNDCLASSW {
                lpfnWndProc: Some(osd_proc),
                hInstance: self.instance,
                lpszClassName: CLASS,
                ..Default::default()
            };
            if unsafe { RegisterClassW(&class) } == 0 {
                return Err(io::Error::last_os_error());
            }
            self.registered = true;
        }
        let monitor = unsafe { MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY) };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            return Err(io::Error::last_os_error());
        }
        let work = info.rcWork;
        let window = Window(
            unsafe {
                CreateWindowExW(
                    WS_EX_LAYERED
                        | WS_EX_TRANSPARENT
                        | WS_EX_TOPMOST
                        | WS_EX_NOACTIVATE
                        | WS_EX_TOOLWINDOW,
                    CLASS,
                    w!("MSI Toolkit"),
                    WS_POPUP,
                    work.left,
                    work.top,
                    0,
                    0,
                    None,
                    None,
                    Some(self.instance),
                    None,
                )
            }
            .map_err(os_error)?,
        );
        let scale = f32::from(unsafe { GetDpiForWindow(window.0) }.clamp(96, 288) as u16) / 96.0;
        let px = |value: f32| (value * scale).round() as i32;
        let (width, height) = (px(312.0), px(68.0));
        let dib = Dib::new(width, height)?;
        dib.paint(width, height, scale, title, detail)?;
        let dst = POINT {
            x: (work.right - width - px(20.0)).max(work.left),
            y: (work.bottom - height - px(24.0)).max(work.top),
        };
        let size = SIZE {
            cx: width,
            cy: height,
        };
        let origin = POINT::default();
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            UpdateLayeredWindow(
                window.0,
                None,
                Some(&dst),
                Some(&size),
                Some(dib.dc),
                Some(&origin),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
        }
        .map_err(os_error)?;
        // User32 owns the composited pixels after UpdateLayeredWindow; the DIB can be released already.
        drop(dib);
        let expires = Instant::now() + Duration::from_millis(u64::from(duration_ms));
        if unsafe { SetTimer(Some(self.owner), HIDE_TIMER, u32::from(duration_ms), None) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let _ = unsafe { ShowWindow(window.0, SW_SHOWNOACTIVATE) };
        self.visible = Some((window, expires));
        Ok(())
    }

    pub(crate) fn expire(&mut self) {
        if self
            .visible
            .as_ref()
            .is_some_and(|(_, until)| Instant::now() >= *until)
        {
            self.hide();
        }
    }

    pub(crate) fn hide(&mut self) {
        if self.visible.take().is_some() {
            unsafe {
                let _ = KillTimer(Some(self.owner), HIDE_TIMER);
            }
        }
    }
}

impl Drop for Osd {
    fn drop(&mut self) {
        self.hide();
        if self.registered {
            unsafe {
                let _ = UnregisterClassW(CLASS, Some(self.instance));
            }
        }
    }
}

struct Window(HWND);

unsafe extern "system" fn osd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    pixels: *mut u32,
}

impl Dib {
    fn new(width: i32, height: i32) -> io::Result<Self> {
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        let mut dib = Self {
            dc,
            bitmap: HBITMAP::default(),
            old: HGDIOBJ::default(),
            pixels: ptr::null_mut(),
        };
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
        let mut bits = ptr::null_mut();
        dib.bitmap =
            unsafe { CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) }
                .map_err(os_error)?;
        dib.old = unsafe { SelectObject(dc, dib.bitmap.into()) };
        if dib.old.is_invalid() || bits.is_null() {
            return Err(io::Error::last_os_error());
        }
        dib.pixels = bits.cast();
        Ok(dib)
    }

    fn paint(
        &self,
        width: i32,
        height: i32,
        scale: f32,
        title: &str,
        detail: &str,
    ) -> io::Result<()> {
        let pixels = unsafe { slice::from_raw_parts_mut(self.pixels, (width * height) as usize) };
        let radius = 9.0 * scale;
        for y in 0..height {
            for x in 0..width {
                let cx = (radius - (x.min(width - 1 - x) as f32 + 0.5)).max(0.0);
                let cy = (radius - (y.min(height - 1 - y) as f32 + 0.5)).max(0.0);
                let coverage = (radius + 0.5 - cx.hypot(cy)).clamp(0.0, 1.0);
                let accent = (14.0 * scale..18.0 * scale).contains(&(x as f32))
                    && (16.0 * scale..52.0 * scale).contains(&(y as f32));
                pixels[(y * width + x) as usize] = if accent {
                    0xffed_6070
                } else {
                    premultiply((238.0 * coverage) as u8, 28, 28, 31)
                };
            }
        }
        let session = GdiPlus::start()?;
        let mut bitmap = ptr::null_mut();
        gdip(unsafe {
            GdipCreateBitmapFromScan0(
                width,
                height,
                width * 4,
                PARGB32,
                Some(self.pixels.cast()),
                &mut bitmap,
            )
        })?;
        let bitmap = Resource(bitmap, |p| unsafe {
            let _ = GdipDisposeImage(p.cast());
        });
        let mut graphics = ptr::null_mut();
        gdip(unsafe { GdipGetImageGraphicsContext(bitmap.0.cast(), &mut graphics) })?;
        let graphics = Resource(graphics, |p| unsafe {
            let _ = GdipDeleteGraphics(p);
        });
        gdip(unsafe { GdipSetTextRenderingHint(graphics.0, TextRenderingHintAntiAliasGridFit) })?;
        let mut family = ptr::null_mut();
        gdip(unsafe {
            GdipCreateFontFamilyFromName(w!("Segoe UI"), ptr::null_mut(), &mut family)
        })?;
        let family = Resource(family, |p| unsafe {
            let _ = GdipDeleteFontFamily(p);
        });
        for (text, size, top, color, style) in [
            (title, 13.0, 10.0, 0xffd0_d0d5, FontStyleRegular),
            (detail, 19.0, 28.0, 0xfffa_fafc, FontStyleBold),
        ] {
            let mut font = ptr::null_mut();
            gdip(unsafe { GdipCreateFont(family.0, size * scale, style.0, UnitPixel, &mut font) })?;
            let font = Resource(font, |p| unsafe {
                let _ = GdipDeleteFont(p);
            });
            let mut brush = ptr::null_mut();
            gdip(unsafe { GdipCreateSolidFill(color, &mut brush) })?;
            let brush = Resource(brush, |p| unsafe {
                let _ = GdipDeleteBrush(p.cast());
            });
            let text = wide(text);
            let rect = RectF {
                X: 26.0 * scale,
                Y: top * scale,
                Width: width as f32 - 38.0 * scale,
                Height: 29.0 * scale,
            };
            gdip(unsafe {
                GdipDrawString(
                    graphics.0,
                    PCWSTR(text.as_ptr()),
                    -1,
                    font.0,
                    &rect,
                    ptr::null(),
                    brush.0.cast(),
                )
            })?;
        }
        drop((family, graphics, bitmap, session));
        Ok(())
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            if !self.old.is_invalid() {
                let _ = SelectObject(self.dc, self.old);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap.into());
            }
            let _ = DeleteDC(self.dc);
        }
    }
}

fn premultiply(a: u8, r: u8, g: u8, b: u8) -> u32 {
    let channel = |v: u8| (u32::from(v) * u32::from(a) + 127) / 255;
    u32::from(a) << 24 | channel(r) << 16 | channel(g) << 8 | channel(b)
}

struct Resource<T>(*mut T, fn(*mut T));
impl<T> Drop for Resource<T> {
    fn drop(&mut self) {
        (self.1)(self.0);
    }
}

struct GdiPlus(usize);
impl GdiPlus {
    fn start() -> io::Result<Self> {
        let mut token = 0;
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            SuppressExternalCodecs: true.into(),
            ..Default::default()
        };
        gdip(unsafe { GdiplusStartup(&mut token, &input, ptr::null_mut()) })?;
        Ok(Self(token))
    }
}

impl Drop for GdiPlus {
    fn drop(&mut self) {
        unsafe {
            GdiplusShutdown(self.0);
        }
    }
}

fn gdip(status: Status) -> io::Result<()> {
    if status.0 == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!("GDI+ status {}", status.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_dib_contains_glyphs_and_premultiplied_alpha() {
        let (w, h) = (312, 68);
        let dib = Dib::new(w, h).unwrap();
        dib.paint(w, h, 1.0, "Refresh rate", "240 Hz").unwrap();
        let pixels = unsafe { slice::from_raw_parts(dib.pixels, (w * h) as usize) };
        assert_eq!(pixels[0], 0);
        assert!(
            pixels.iter().any(|p| p & 0x00ff_ffff == 0x00fa_fafc),
            "GDI+ text must be drawn into the DIB"
        );
        for &pixel in pixels {
            let alpha = pixel >> 24;
            assert!(
                [16, 8, 0]
                    .iter()
                    .all(|shift| (pixel >> shift) & 255 <= alpha)
            );
        }
    }
}
