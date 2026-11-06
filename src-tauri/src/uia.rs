//! Windows UIA：读取当前焦点程序的选中文本及其屏幕坐标。
//! 阻塞调用，应在 spawn_blocking 里执行。仅适用于通过 UIA 暴露文本选区的程序
//! （浏览器、Office、编辑器、终端等）；游戏/自绘界面读不到。

use windows::Win32::Foundation::POINT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextRange,
    IUIAutomationTextPattern, UIA_TextPatternId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    VK_C, VK_CONTROL, VIRTUAL_KEY,
};
use std::mem::size_of;
use serde::Serialize;

#[derive(Serialize)]
pub struct Selection {
    pub text: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// 读取当前选中文本及其屏幕矩形（物理像素）。
/// 依次尝试：①鼠标光标下的元素 ②键盘焦点元素；并对每个元素**向上遍历祖先**，
/// 因为很多程序（IDEA/QQ 等）的 TextPattern 提供者在光标/焦点元素的上层（编辑器窗格/文档节点）。
pub fn read_selection() -> Result<Selection, String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("创建 UIA 失败: {e}"))?;
        // 遍历器为可选：创建失败则跳过祖先遍历，但仍先测光标/焦点元素的选区
        let walker = uia.ControlViewWalker().ok();
        let mut diag = String::new();

        let mut candidates: Vec<IUIAutomationElement> = Vec::new();
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_ok() {
            match uia.ElementFromPoint(pt) {
                Ok(e) => candidates.push(e),
                Err(_) => diag.push_str("光标元素查询失败; "),
            }
        }
        match uia.GetFocusedElement() {
            Ok(e) => candidates.push(e),
            Err(_) => diag.push_str("焦点元素查询失败; "),
        }

        for (idx, mut elem) in candidates.iter().cloned().enumerate() {
            let label = if idx == 0 { "光标" } else { "焦点" };
            for _ in 0..16 {
                if let Some(s) = try_selection_from(&elem) {
                    return Ok(s);
                }
                // 记录该元素：名称 + 是否支持文本模式，用于诊断
                let name = elem
                    .CurrentName()
                    .map(|n| n.to_string())
                    .unwrap_or_default();
                let has_tp = elem
                    .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
                    .is_ok();
                diag.push_str(&format!("[{label}][{name}]文本模式={has_tp}; "));
                let Some(w) = &walker else { break };
                match w.GetParentElement(&elem) {
                    Ok(p) => elem = p,
                    Err(_) => break,
                }
            }
        }

        Err(format!("未读到选区（{diag}）"))
    }
}

/// 尝试从单个元素读取 TextPattern 选区；成功则返回 Selection。
unsafe fn try_selection_from(elem: &IUIAutomationElement) -> Option<Selection> {
    let tp = elem
        .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
        .ok()?;
    let ranges = tp.GetSelection().ok()?;
    let count = ranges.Length().ok()?;
    if count <= 0 {
        return None;
    }
    let range = ranges.GetElement(0).ok()?;
    let text = range.GetText(-1).ok()?;
    let text: String = text.to_string();
    if text.trim().is_empty() {
        return None;
    }
    let (x, y, w, h) = unsafe { bounding_rect(&range) };
    Some(Selection { text, x, y, w, h })
}

/// 复制并读取当前选区，结束时**还原剪贴板**（不占用/不破坏用户的剪贴板）。
/// 用于 UIA 读不到选区（IDEA、部分编辑器等）时，Alt+X 内部自动复制。
pub fn copy_selection_and_read() -> Result<String, String> {
    /// 复制前先往剪贴板写这个哨兵。
    /// 复制完如果读到的还是它，说明 Ctrl+C 根本没生效——此时绝不能把
    /// "剪贴板里的旧内容"当成用户选中的文字（之前就是把一段旧调试日志
    /// 当成了选区去翻译，弹出来一坨莫名其妙的文本）。
    const SENTINEL: &str = "\u{1}JFY_NO_COPY\u{1}";

    // 保存当前剪贴板文本，用于之后还原
    let saved = arboard::Clipboard::new()
        .ok()
        .and_then(|mut c| c.get_text().ok());

    // 写入哨兵，等系统处理完再发 Ctrl+C
    if let Ok(mut c) = arboard::Clipboard::new() {
        let _ = c.set_text(SENTINEL);
    }
    std::thread::sleep(std::time::Duration::from_millis(60));

    tap(VK_CONTROL, false); // Ctrl 按下
    std::thread::sleep(std::time::Duration::from_millis(25));
    tap(VK_C, false); // C 按下
    std::thread::sleep(std::time::Duration::from_millis(15));
    tap(VK_C, true); // C 抬起
    std::thread::sleep(std::time::Duration::from_millis(15));
    tap(VK_CONTROL, true); // Ctrl 抬起
    std::thread::sleep(std::time::Duration::from_millis(260));
    let result = arboard::Clipboard::new().and_then(|mut c| c.get_text());

    // 还原剪贴板（写回之前内容）
    if let Some(s) = saved.clone() {
        if let Ok(mut c) = arboard::Clipboard::new() {
            let _ = c.set_text(s);
        }
    }

    match result {
        // 还是哨兵 → 没复制成功（软件抵抗模拟按键）
        Ok(t) if t == SENTINEL => Err(
            "没抓到选区：目标软件没有响应模拟的 Ctrl+C。请手动 Ctrl+C 后用「复制即译」，或选中后按快捷键重试。"
                .to_string(),
        ),
        Ok(t) => {
            let t = t.trim().to_string();
            if t.is_empty() {
                Err("选区是空的".to_string())
            } else {
                Ok(t)
            }
        }
        Err(e) => Err(format!("读取剪贴板失败: {e}")),
    }
}

/// 发送一次键盘事件（up 为抬起）。
fn tap(vk: VIRTUAL_KEY, up: bool) {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                time: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        let _ = SendInput(std::slice::from_ref(&input), size_of::<INPUT>() as i32);
    }
}

/// 文本范围的屏幕包围矩形（物理像素）。多行选区取所有行矩形的并集。
unsafe fn bounding_rect(range: &IUIAutomationTextRange) -> (i32, i32, i32, i32) {
    if let Ok(sa) = range.GetBoundingRectangles() {
        let sa = &*sa;
        let n = sa.rgsabound[0].cElements as usize;
        if n >= 4 {
            let p = sa.pvData as *const f64;
            let v = core::slice::from_raw_parts(p, n);
            let (mut minx, mut miny) = (f64::MAX, f64::MAX);
            let (mut maxx, mut maxy) = (f64::MIN, f64::MIN);
            for r in v.chunks(4) {
                if r.len() < 4 || r[2] <= 0.0 || r[3] <= 0.0 {
                    continue;
                }
                let (x, y, w, h) = (r[0], r[1], r[2], r[3]);
                minx = minx.min(x);
                miny = miny.min(y);
                maxx = maxx.max(x + w);
                maxy = maxy.max(y + h);
            }
            if minx <= maxx && miny <= maxy {
                return (minx as i32, miny as i32, (maxx - minx) as i32, (maxy - miny) as i32);
            }
        }
    }
    (0, 0, 0, 0)
}
