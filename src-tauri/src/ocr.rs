//! Windows 原生 OCR：GDI 截取屏幕区域 → WinRT OcrEngine 识别。
//! 所有函数均为阻塞调用，应在 `tauri::async_runtime::spawn_blocking` 中执行。

use std::mem::size_of;
use windows::{
    core::HSTRING,
    Globalization::Language,
    Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap},
    Media::Ocr::OcrEngine,
    Storage::Streams::DataWriter,
    Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
        HGDIOBJ, SRCCOPY,
    },
};

/// 截取屏幕指定区域并 OCR，返回识别文本。
/// `x, y, w, h` 为物理像素坐标。
pub fn recognize(x: i32, y: i32, w: i32, h: i32) -> Result<String, String> {
    if w <= 0 || h <= 0 {
        return Err("区域无效".into());
    }
    let bgra = unsafe { capture_region_bgra(x, y, w, h) }?;
    let bitmap = make_bitmap(&bgra, w as u32, h as u32)?;
    let text = unsafe { ocr_bitmap(&bitmap) }?;
    Ok(text)
}

// 把 windows::core::Result 的错误转成 String
fn w<T>(r: windows::core::Result<T>) -> Result<T, String> {
    r.map_err(|e| e.to_string())
}

/// GDI 截取屏幕矩形区域，得到 BGRA8 像素（自上而下）。
#[allow(clippy::too_many_arguments)]
unsafe fn capture_region_bgra(x: i32, y: i32, w: i32, h: i32) -> Result<Vec<u8>, String> {
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(Some(screen));
    let bmp = CreateCompatibleBitmap(screen, w, h);
    let _prev = SelectObject(mem, HGDIOBJ::from(bmp));
    BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY).map_err(|e| format!("BitBlt: {e}"))?;

    let mut bmi = BITMAPINFO::default();
    let bmih = &mut bmi.bmiHeader;
    bmih.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bmih.biWidth = w;
    bmih.biHeight = -h; // 负值 = 自上而下
    bmih.biPlanes = 1;
    bmih.biBitCount = 32;
    bmih.biCompression = BI_RGB.0;

    let mut buf = vec![0u8; (w * h * 4) as usize];
    let got = GetDIBits(
        mem,
        bmp,
        0,
        h as u32,
        Some(buf.as_mut_ptr().cast()),
        &mut bmi,
        DIB_RGB_COLORS,
    );

    SelectObject(mem, _prev);
    let _ = DeleteObject(HGDIOBJ::from(bmp));
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);

    if got <= 0 {
        return Err("GetDIBits 返回 0，截屏失败".into());
    }
    Ok(buf)
}

/// 把 BGRA 像素塞进 SoftwareBitmap（用 DataWriter 正确设置 IBuffer 长度）。
fn make_bitmap(bgra: &[u8], width: u32, height: u32) -> Result<SoftwareBitmap, String> {
    let writer = DataWriter::new().map_err(|e| format!("DataWriter: {e}"))?;
    writer.WriteBytes(bgra).map_err(|e| format!("WriteBytes: {e}"))?;
    let buffer = writer.DetachBuffer().map_err(|e| format!("DetachBuffer: {e}"))?;
    w(SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        width as i32,
        height as i32,
    ))
    .map_err(|e| format!("SoftwareBitmap: {e}"))
}

/// 用 WinRT OcrEngine 识别 SoftwareBitmap，返回每行拼接的文本。
unsafe fn ocr_bitmap(bitmap: &SoftwareBitmap) -> Result<String, String> {
    let lang = Language::CreateLanguage(&HSTRING::from("en-US"))
        .map_err(|e| format!("创建语言: {e}"))?;
    let engine = w(OcrEngine::TryCreateFromLanguage(&lang)).map_err(|e| format!("OcrEngine: {e}"))?;
    let op = w(engine.RecognizeAsync(bitmap)).map_err(|e| format!("RecognizeAsync: {e}"))?;
    let result = w(op.join()).map_err(|e| format!("OCR join: {e}"))?;

    let mut out = String::new();
    let lines = w(result.Lines()).map_err(|e| format!("Lines: {e}"))?;
    let n = w(lines.Size()).map_err(|e| format!("Lines.Size: {e}"))?;
    for i in 0..n {
        let line = w(lines.GetAt(i)).map_err(|e| format!("Line.GetAt: {e}"))?;
        let words = w(line.Words()).map_err(|e| format!("Words: {e}"))?;
        let m = w(words.Size()).map_err(|e| format!("Words.Size: {e}"))?;
        let mut lt = String::new();
        for j in 0..m {
            let word = w(words.GetAt(j)).map_err(|e| format!("Word.GetAt: {e}"))?;
            let txt = w(word.Text()).map_err(|e| format!("Text: {e}"))?;
            lt.push_str(&txt.to_string());
            lt.push(' ');
        }
        out.push_str(lt.trim());
        out.push('\n');
    }
    Ok(out.trim().to_string())
}
