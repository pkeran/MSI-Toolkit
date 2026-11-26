//! Screen-independent Slint software rendering and a lossless RGBA PNG writer.
use std::io::{self, Write};
use std::path::Path;
use std::rc::Rc;
use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{PhysicalSize, Rgba8Pixel, SharedPixelBuffer};

struct Headless { window: Rc<MinimalSoftwareWindow> }
impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> { Ok(self.window.clone()) }
    fn duration_since_start(&self) -> std::time::Duration { std::time::Duration::ZERO }
}

pub fn install() -> Result<Rc<MinimalSoftwareWindow>, String> {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless { window: window.clone() })).map_err(|e| e.to_string())?;
    Ok(window)
}

pub fn render(window: &MinimalSoftwareWindow, width: u32, height: u32) -> Result<SharedPixelBuffer<Rgba8Pixel>, String> {
    window.set_size(PhysicalSize::new(width, height));
    window.request_redraw();
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    let mut target = vec![PremultipliedRgbaColor::default(); width as usize * height as usize];
    let drawn = window.draw_if_needed(|renderer| { renderer.render(&mut target, width as usize); });
    if !drawn { return Err("软件渲染器没有生成帧".into()); }
    for (to, from) in pixels.make_mut_slice().iter_mut().zip(target) {
        let straight = |value: u8| if from.alpha == 0 { 0 } else { (u32::from(value) * 255 / u32::from(from.alpha)).min(255) as u8 };
        *to = Rgba8Pixel::new(straight(from.red), straight(from.green), straight(from.blue), from.alpha);
    }
    if pixels.as_slice().iter().all(|pixel| *pixel == pixels.as_slice()[0]) { return Err("软件渲染输出为空白".into()); }
    Ok(pixels)
}

pub fn save(path: &Path, pixels: &SharedPixelBuffer<Rgba8Pixel>) -> io::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    // Readers may open the final path as soon as it appears.
    let mut staging = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut file = io::BufWriter::new(staging.as_file_mut());
        write_png(&mut file, pixels.width(), pixels.height(), pixels.as_bytes())?;
        file.flush()?;
    }
    staging.persist(path)?;
    Ok(())
}

fn write_png(out: &mut impl Write, width: u32, height: u32, rgba: &[u8]) -> io::Result<()> {
    let stride = width as usize * 4;
    if width == 0 || height == 0 || rgba.len() != stride * height as usize { return Err(io::Error::new(io::ErrorKind::InvalidInput, "无效 RGBA 尺寸")); }
    out.write_all(b"\x89PNG\r\n\x1a\n")?;
    let mut ihdr = Vec::from(width.to_be_bytes());
    ihdr.extend(height.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]);
    chunk(out, b"IHDR", &ihdr)?;
    let mut raw = Vec::with_capacity(rgba.len() + height as usize);
    for line in rgba.chunks_exact(stride) { raw.push(0); raw.extend_from_slice(line); }
    let mut zlib = vec![0x78, 0x01];
    let blocks = raw.chunks(65535);
    let count = blocks.len();
    for (index, block) in blocks.enumerate() {
        zlib.push(u8::from(index + 1 == count));
        let len = block.len() as u16;
        zlib.extend(len.to_le_bytes());
        zlib.extend((!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &raw { a = (a + u32::from(*byte)) % 65521; b = (b + a) % 65521; }
    zlib.extend(((b << 16) | a).to_be_bytes());
    chunk(out, b"IDAT", &zlib)?;
    chunk(out, b"IEND", &[])
}

fn chunk(out: &mut impl Write, kind: &[u8; 4], data: &[u8]) -> io::Result<()> {
    out.write_all(&(data.len() as u32).to_be_bytes())?;
    out.write_all(kind)?;
    out.write_all(data)?;
    let mut crc = !0u32;
    for byte in kind.iter().chain(data) {
        crc ^= u32::from(*byte);
        for _ in 0..8 { crc = (crc >> 1) ^ (0xedb88320 & (0u32.wrapping_sub(crc & 1))); }
    }
    out.write_all(&(!crc).to_be_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn png_has_rgba_header_and_standard_crc() {
        let mut bytes = Vec::new();
        write_png(&mut bytes, 1, 1, &[12, 34, 56, 255]).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&bytes[16..24], &[0, 0, 0, 1, 0, 0, 0, 1]);
        assert_eq!(&bytes[bytes.len()-12..], b"\0\0\0\0IEND\xaeB`\x82");
    }
    #[test]
    fn png_rejects_inconsistent_pixel_counts() {
        assert!(write_png(&mut Vec::new(), 2, 1, &[0; 4]).is_err());
    }

    #[test]
    fn failed_export_preserves_the_previous_png_and_leaves_no_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        let mut valid = Vec::new();
        write_png(&mut valid, 1, 1, &[12, 34, 56, 255]).unwrap();
        std::fs::write(&path, &valid).unwrap();
        let empty = SharedPixelBuffer::<Rgba8Pixel>::new(0, 0);
        assert!(save(&path, &empty).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), valid);
        assert!(save(&dir.path().join("new.png"), &empty).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn successful_export_publishes_the_complete_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("shot.png");
        let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(1, 1);
        for value in [42, 84] {
            pixels.make_mut_bytes().copy_from_slice(&[value, 34, 56, 255]);
            let mut expected = Vec::new();
            write_png(&mut expected, 1, 1, pixels.as_bytes()).unwrap();
            save(&path, &pixels).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), expected);
        }
    }
}
