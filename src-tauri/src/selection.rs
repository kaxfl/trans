//! 全局选区抓取：**钩子 + 剪贴板 + 重试**，目标是"任何软件里选中文字都能翻"。
//!
//! 为什么不能只靠 UIA：
//! UIA 的 TextPattern 需要目标程序**主动暴露**文本框。Chrome、VSCode、IDEA
//! 默认都不暴露（要靠系统"屏幕阅读器"标志才打开），QQ、游戏、自绘界面则永远
//! 不暴露。所以只做 UIA 必然"很多 app 抓不到"。
//!
//! 有道那类词典的实际做法是**全局钩子 + 剪贴板合成复制**，而不是 UIA：
//!   1. 装全局低级鼠标钩子，检测"用户完成了一次选择"（双击选词 / 拖拽选段）；
//!   2. 合成 Ctrl+C，把选区复制到剪贴板；
//!   3. 读剪贴板拿到文本，然后**把剪贴板还原回去**，不污染用户剪贴板；
//!   4. 用"内容指纹 + 重试"判断复制是否真的成功。
//!
//! ## 架构要点（踩过的坑）
//!
//! **不要在钩子回调里用 `SetTimer` 做延迟**。第一版就是这么写的：钩子能识别出
//! 双击、也调了 `SetTimer`，但 `WM_TIMER` 从未被投递到钩子线程的消息循环里，
//! 结果"双击没反应"且完全没有报错——极难排查。
//! 现在改成：钩子回调只负责**判定**（纯计算、微秒级），判定成立就通过 channel
//! 把活丢给一个常驻工作线程，由它 sleep 一小会儿让选区稳定后再抓取。
//! 这样钩子回调永远不做重活（低级钩子回调阻塞会拖慢整个系统的鼠标输入）。

use std::sync::mpsc::{sync_channel, SyncSender};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    VK_C, VK_CONTROL, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetClassNameW, GetForegroundWindow, GetMessageW,
    GetWindowThreadProcessId, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HHOOK, MSG,
    WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEHWHEEL, WM_MOUSEWHEEL,
    WM_RBUTTONUP,
};

/// 选区被"确认"后回调（在工作线程里调用，可以放心做重活）。
pub type OnSelect = Box<dyn Fn(String) + Send + 'static>;

/// 两次点击算不算"双击"的时间窗
const DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);
/// 两次点击算不算"在同一处"的距离窗（像素）
const DOUBLE_CLICK_PX: i32 = 6;
/// 按下后单次移动超过这个距离，才算"疑似在拖拽"（还要连续多次+时间满足才算数）
const DRAG_PX: i32 = 5;
/// 进入拖拽前，必须连续几次移动都超过阈值。
///
/// 为什么需要：双击时鼠标几乎不可能零位移（手抖 1px），如果一次位移就判定拖拽，
/// 双击就会被误当成"拖拽选了一个字符"。实测用户日志里出现大量这种误判。
const DRAG_CONFIRM_MOVES: u32 = 2;
/// 按下到松开至少这么多毫秒，才算拖拽（双击的两击间隔远小于此）
const DRAG_MIN_MS: u64 = 160;
/// 复制后读剪贴板的重试次数与间隔
const COPY_RETRY: usize = 3;
const COPY_RETRY_MS: u64 = 120;
/// 两次抓取之间的最小间隔
const COOLDOWN: std::time::Duration = std::time::Duration::from_millis(500);
/// 判定成立后，等选区状态在目标程序里稳定下来的时间。
/// 这段是纯等待，直接计入弹窗延迟，所以压到最小；靠 UIA/Ctrl+C 的重试来兜稳定性。
const SETTLE_MS: u64 = 40;

/// 诊断日志开关：设置了 JFY_SELECTION_DEBUG 环境变量才写日志。
static DEBUG_LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// "正在内部合成 Ctrl+C"的深度计数。>0 时剪贴板监听应当忽略变化。
/// 用计数而非布尔，因为抓取过程可能重叠。
static INTERNAL_COPY_DEPTH: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// 供主程序查询：是否正在内部复制（用于跳过"复制即译"）。
pub fn internal_copy_active() -> bool {
    INTERNAL_COPY_DEPTH.load(std::sync::atomic::Ordering::Relaxed) > 0
}

/// 最近一次内部抓取到的文本。
///
/// 这是一道兜底保险：剪贴板监听是每 250ms 轮询的，可能在我们
/// "复制完 → 清掉内部标记"之间恰好跑一轮，于是把内部复制的文本
/// 当成用户复制，弹出第二个浮窗。主程序拿这个来识别"这是我们自己刚抓的"。
static LAST_INTERNAL_TEXT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 内部复制刚结束的时间点。
///
/// 为什么还需要这个：抓取流程是"保存剪贴板 → Ctrl+C → 读文本 → 还原剪贴板"。
/// 还原动作本身又是一次剪贴板变化；如果用户原来剪贴板里就有东西（比如刚复制的
/// 一段命令），监听会把"还原出来的旧内容"当成新复制，弹出莫名其妙的窗。
/// 所以复制结束后的一小段时间内，剪贴板的变化一律不当作用户行为。
static LAST_INTERNAL_AT: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);

/// 内部复制结束后多久，剪贴板变化仍然视为"我们造成的"。
const INTERNAL_SETTLE: std::time::Duration = std::time::Duration::from_millis(900);

/// 判断一段文本是不是我们刚内部抓取过的（用于剪贴板监听去重）。
pub fn is_internal_text(text: &str) -> bool {
    LAST_INTERNAL_TEXT
        .lock()
        .map(|g| g.as_deref() == Some(text))
        .unwrap_or(false)
}

/// 是否处在"内部复制刚结束"的静默窗口内。
pub fn in_internal_settle_window() -> bool {
    LAST_INTERNAL_AT
        .lock()
        .ok()
        .and_then(|g| *g)
        .map(|t| t.elapsed() < INTERNAL_SETTLE)
        .unwrap_or(false)
}

fn debug_on() -> bool {
    *DEBUG_LOG.get_or_init(|| std::env::var("JFY_SELECTION_DEBUG").is_ok())
}

fn dbg(msg: &str) {
    if !debug_on() {
        return;
    }
    use std::io::Write;
    let path = std::env::temp_dir().join("jishifanyi-selection.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "[{:?}] {}", std::time::SystemTime::now(), msg);
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Stage {
    Idle,
    /// 已按下左键，记录按下点和已确认的移动次数
    Down {
        at: std::time::Instant,
        pt: (i32, i32),
        moved: u32,
    },
    /// 正在拖拽（已超过 DRAG_PX）
    Drag { pt: (i32, i32) },
    /// 单击完成，等第二次点击（判断双击）
    WaitDouble { at: std::time::Instant, pt: (i32, i32) },
}

/// 选区矩形（物理像素）+ 该选区对应的文本，等鼠标移上来再翻译。
#[derive(Clone)]
struct Pending {
    /// 触发时 UIA 读到的文本（异步取回，可能为空）
    #[allow(dead_code)]
    text: String,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// 选区的屏幕矩形（物理像素）。
#[derive(Clone, Copy)]
pub struct SelRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// 待读取矩形的请求。ARM 时把 `remain` 置为 1，
/// 一个短命线程读完矩形后减回 0；读到 0 就写入 `slot`。
struct RectRequest {
    remain: usize,
    slot: Option<SelRect>,
}

static RECT_REQ: std::sync::Mutex<Option<RectRequest>> = std::sync::Mutex::new(None);

/// ARM 之后异步取一次选区矩形。
///
/// 为什么异步：UIA 读取是跨进程 COM 调用，几十毫秒起步，
/// **绝不能在钩子回调里做**（会把整个系统的鼠标输入拖慢）。
/// 所以这里起一个短命线程去读，读完写进共享槽位，钩子线程下一帧取走。
/// `remain` 用来保证「多次 ARM 只有最后一次的结果有效」。
fn request_rect() {
    {
        let mut g = match RECT_REQ.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        match g.as_mut() {
            Some(r) => {
                r.remain += 1;
                r.slot = None;
            }
            None => {
                *g = Some(RectRequest {
                    remain: 1,
                    slot: None,
                })
            }
        }
    }
    std::thread::spawn(|| {
        let rect = crate::uia::read_selection().ok().and_then(|sel| {
            if sel.w > 0 && sel.h > 0 {
                Some(SelRect {
                    x: sel.x,
                    y: sel.y,
                    w: sel.w,
                    h: sel.h,
                })
            } else {
                None
            }
        });
        let mut g = match RECT_REQ.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(r) = g.as_mut() {
            if r.remain <= 1 {
                // 我是最后一次请求 → 结果有效
                r.slot = rect;
                r.remain = 0;
            } else {
                r.remain -= 1;
            }
        }
        dbg(&format!(
            "异步取回选区矩形: {:?}",
            rect.map(|r| (r.x, r.y, r.w, r.h))
        ));
    });
}

/// 钩子线程调用：把已就绪的矩形取走（取完即清空，只消费一次）。
fn take_rect() -> Option<SelRect> {
    let mut g = RECT_REQ.lock().ok()?;
    let r = g.as_mut()?;
    if r.remain == 0 {
        r.slot.take()
    } else {
        None
    }
}

struct HookState {
    stage: Stage,
    own_pid: u32,
    last_capture: std::time::Instant,
    tx: SyncSender<()>,
    /// 已选中、等待"鼠标移上来"的选区
    pending: Option<Pending>,
    /// 鼠标是否已经进入过该选区
    inside: bool,
    /// 鼠标进入选区的时刻（用于判断停留时长）
    entered_at: Option<std::time::Instant>,
    /// 这次悬停是否已经翻译过了（移开时重置，所以"移开再放上"能再翻一次）
    translated: bool,
    /// 上次真正触发翻译的时刻。
    ///
    /// 为什么需要：实测用户拖选完一段后，后续的鼠标移动会被钩子**反复判定成
    /// 新的选中动作**，每次都重新武装并重置状态，于是翻译请求刷屏、弹窗被反复
    /// 重建，看起来就像"没弹出来"。用时间闸门挡掉这种连发。
    last_fired: Option<std::time::Instant>,
    /// 本次选区武装的时刻。用于"刚选中时别急着判离开"的宽限期。
    anchor_at: Option<std::time::Instant>,
    /// 左键当前是否处于按下状态。
    ///
    /// 为什么单独记：实测拖选结束后，后续的鼠标 MOVE 会因为状态残留被反复
    /// 判定成"拖拽结束"，从而连发 Armed。用物理按键状态卡住这条路径。
    lbtn_down: bool,
    /// 是否要求"鼠标移回选区上方才翻译"
    require_hover: bool,
    /// 鼠标离开选区时的回调（主程序用它来立刻收起弹窗）
    on_leave: Box<dyn Fn() + Send + 'static>,
    /// 在"空白处"（选区以外）点击时的回调，同样用于收起弹窗
    on_click_outside: Box<dyn Fn() + Send + 'static>,
}

/// 鼠标进入选区后停留多久才翻译。
/// 太长会显得"弹窗很慢"，太短容易误触发。60ms 是手感与稳定的折中。
const HOVER_DWELL: std::time::Duration = std::time::Duration::from_millis(60);
/// 鼠标进入选区的判定外扩（像素），避免边缘抖动导致进进出出
const HOVER_PAD: i32 = 3;
/// 判断"鼠标是否还在选区附近"的宽松半径。
/// UIA 给的矩形有时是 0（程序不提供），这时用它兜底。
const HOVER_FALLBACK_R: i32 = 120;
/// 两次触发翻译之间的最小间隔。挡掉"拖选后鼠标移动被反复判成新选中"导致的连发。
const FIRE_COOLDOWN: std::time::Duration = std::time::Duration::from_millis(1500);

/// 刚选中后的宽限期：这段时间内鼠标抖出选区不算"离开"，不关弹窗。
/// 选中一松手时手往往还在动，没有这个宽限期会出现"弹窗一闪就没 / 残留窗口"。
const ANCHOR_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

/// 设置"是否需要鼠标移回选区上方才翻译"。
pub fn set_require_hover(v: bool) {
    let ptr = std::ptr::addr_of_mut!(STATE);
    unsafe {
        if let Some(state) = (*ptr).as_mut() {
            state.require_hover = v;
            // 切换设置时清掉待定选区，避免旧状态残留
            state.pending = None;
            state.inside = false;
            state.entered_at = None;
            state.translated = false;
        }
    }
}

/// 当前是否要求"鼠标移回选区上方才翻译"。
pub fn require_hover() -> bool {
    let ptr = std::ptr::addr_of_mut!(STATE);
    unsafe { (*ptr).as_ref().map(|s| s.require_hover).unwrap_or(false) }
}

/// 全局可变状态。只在钩子回调里访问，且始终在同一个线程（装着钩子的那个线程），
/// 所以用一个 static 就够了。
static mut STATE: Option<HookState> = None;

fn hash_of(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// 前台窗口的 pid + 类名（诊断用）。
fn foreground_info(own_pid: u32) -> (u32, String, bool) {
    unsafe {
        let hwnd: HWND = GetForegroundWindow();
        if hwnd.0.is_null() {
            return (0, "<无前台窗口>".into(), false);
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let mut buf = [0u16; 256];
        let n = GetClassNameW(hwnd, &mut buf);
        let class = if n > 0 {
            String::from_utf16_lossy(&buf[..n as usize])
        } else {
            "<取不到类名>".into()
        };
        (pid, class, pid == own_pid)
    }
}

/// 合成一次 Ctrl+C。
fn send_ctrl_c() {
    let key = |vk: VIRTUAL_KEY, up: bool| INPUT {
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
    let seq = [
        key(VK_CONTROL, false),
        key(VK_C, false),
        key(VK_C, true),
        key(VK_CONTROL, true),
    ];
    unsafe {
        let _ = SendInput(&seq, std::mem::size_of::<INPUT>() as i32);
    }
}

/// 最近一次抓到的是"经剪贴板"还是"UIA 读取"。供主程序在结果不可信时换条路重抓。
static LAST_VIA_CLIPBOARD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 上一次抓取是否走了剪贴板路径。
pub fn last_via_clipboard() -> bool {
    LAST_VIA_CLIPBOARD.load(std::sync::atomic::Ordering::Relaxed)
}

/// 判断 UIA 读出来的文本和它给出的选区矩形是否"装得下"。
///
/// UIA 在高 DPI 下字符偏移会错位，实测同一个选区会读出
/// `immune` / `mmune` / `mmune t` / `immune t` 等多个版本（差一两个字符）。
/// 但如果框只有 1 个词的宽度、却读出十几个字符，或框挺宽却只读出 2 个字符，
/// 那就明显不对劲——这时应该换剪贴板路径重抓。
///
/// 阈值放宽一点：宁可漏判也不要误判（误判会让每次划词都多跑一趟剪贴板）。
fn rect_plausible(text: &str, w: i32) -> bool {
    if w <= 0 {
        return true; // 拿不到矩形就不判定
    }
    let n = text.chars().count();
    if n == 0 {
        return false;
    }
    // 每行粗估 5–40 px 宽（覆盖小字号到 CJK 宽字符）
    let max_chars = ((w as f64) / 5.0).ceil() as usize + 4;
    let min_chars = ((w as f64) / 40.0).floor() as usize;
    // 多行选区：矩形宽是"最宽一行"，实际字符数可能远超单行容量，所以放宽上限
    let lines = text.lines().count().max(1);
    if n > max_chars * lines + 4 {
        return false;
    }
    if n + 4 < min_chars {
        return false;
    }
    true
}

/// 抓取当前选区。
///
/// **优先走 UIA**（`uia::read_selection`）：它直接向目标程序要选区文本，
/// 完全不碰剪贴板——这样就不会和截图工具抢剪贴板。
///
/// **但 UIA 不可靠**：高 DPI 下字符偏移会错位（会多带前一个单词的尾字母、
/// 或漏掉首字母）。所以这里再用选区矩形做一次合理性检查；
/// 明显不合理就直接走剪贴板路径。主程序在"翻译结果不可信"时还会再兜一次底。
///
/// 返回 (文本, 是否走了剪贴板)。
pub fn grab_selection() -> Result<(String, bool), String> {
    // ① UIA：不碰剪贴板
    if let Ok(sel) = crate::uia::read_selection() {
        let t = sel.text.trim().to_string();
        if !t.is_empty() && rect_plausible(&t, sel.w) {
            if let Ok(mut g) = LAST_INTERNAL_TEXT.lock() {
                *g = Some(t.clone());
            }
            LAST_VIA_CLIPBOARD.store(false, std::sync::atomic::Ordering::Relaxed);
            return Ok((t, false));
        }
        dbg(&format!(
            "UIA 结果与选区矩形不符（文本 {} 字符 / 宽度 {}px），改用剪贴板路径",
            t.chars().count(),
            sel.w
        ));
    }
    // ② 兜底：合成 Ctrl+C
    grab_selection_via_clipboard()
}

/// 直接用剪贴板路径抓取（合成 Ctrl+C）。UIA 结果不可信时用它重试。
pub fn grab_selection_via_clipboard() -> Result<(String, bool), String> {
    let r = capture_selection()?;
    LAST_VIA_CLIPBOARD.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok((r, true))
}

/// 抓取当前选区：记下剪贴板旧内容 → 合成 Ctrl+C → 读新内容 → 还原剪贴板。
///
/// 不能因为"内容没变"就断定失败——用户可能选中的就是剪贴板里已有的同一段文字。
/// 所以用"重试 + 内容指纹"判定，且失败时不改动用户剪贴板。
///
/// **注意**：整段过程会被一个全局"内部复制"计数包住。因为合成的 Ctrl+C 会让
/// 剪贴板内容变化，若不标记，"复制即译"监听会以为用户复制了东西、弹出第二个
/// 窗口，和真正的翻译窗抢位置——用户看到的现象就是"浮窗闪一下"。
pub fn capture_selection() -> Result<String, String> {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            INTERNAL_COPY_DEPTH.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    INTERNAL_COPY_DEPTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _guard = Guard;

    let saved_text = arboard::Clipboard::new()
        .ok()
        .and_then(|mut c| c.get_text().ok());
    let saved_image = arboard::Clipboard::new()
        .ok()
        .and_then(|mut c| c.get_image().ok());
    let before_hash = saved_text.as_deref().map(hash_of);

    let mut got: Option<String> = None;
    for attempt in 0..COPY_RETRY {
        send_ctrl_c();
        std::thread::sleep(std::time::Duration::from_millis(COPY_RETRY_MS));
        if let Ok(t) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
            let t = t.trim().to_string();
            if t.is_empty() {
                continue;
            }
            let h = hash_of(&t);
            if Some(h) != before_hash {
                got = Some(t);
                break;
            }
            // 内容没变：可能是"选中的就是剪贴板原内容"，也可能是没复制成功。
            // 最后一次接受它，覆盖"连续翻译同一段文字"的场景。
            if attempt + 1 == COPY_RETRY {
                got = Some(t);
            }
        }
    }

    // 还原用户剪贴板
    if let Some(img) = saved_image {
        if let Ok(mut c) = arboard::Clipboard::new() {
            let _ = c.set_image(img);
        }
    } else if let Some(t) = saved_text {
        if let Ok(mut c) = arboard::Clipboard::new() {
            let _ = c.set_text(t);
        }
    }

    // 标记"内部复制刚结束"，让剪贴板监听在接下来一小段时间里忽略变化
    if let Ok(mut g) = LAST_INTERNAL_AT.lock() {
        *g = Some(std::time::Instant::now());
    }

    match got {
        Some(t) if t.is_empty() => Err("选区为空".into()),
        Some(t) => {
            // 记下来，供剪贴板监听识别"这是我们自己刚抓的"
            if let Ok(mut g) = LAST_INTERNAL_TEXT.lock() {
                *g = Some(t.clone());
            }
            Ok(t)
        }
        None => Err("没抓到选区（该软件可能不响应合成按键）".into()),
    }
}

/// 记录"用户刚选中了这个区域"，等鼠标移上来再翻译。
/// 只在钩子线程调用。
///
/// **这里绝不能调用 UIA**。低级鼠标钩子回调一旦阻塞，整个系统的鼠标输入都会被
/// 拖慢；而 UIA 读取要跨进程 COM 调用，几十毫秒起步。之前就是在这里读 UIA，
/// 导致用户反馈"移动鼠标很卡"。
/// 真正读文本交给工作线程（`grab_selection`），钩子里只记坐标。
/// 记录"用户刚选中了这个区域"。
/// 只在钩子线程调用。
///
/// **这里绝不能调用 UIA**。低级鼠标钩子回调一旦阻塞，整个系统的鼠标输入都会被
/// 拖慢；而 UIA 读取要跨进程 COM 调用，几十毫秒起步。之前就是在这里读 UIA，
/// 导致用户反馈"移动鼠标很卡"。
/// 真正读文本交给工作线程（`grab_selection`），钩子里只记坐标。
fn arm_pending(state: &mut HookState) {
    // 刚触发过翻译（冷却期内）就不要再重复触发：
    // 实测拖选后鼠标移动会被反复判成"新选中"，这里挡掉连发。
    if let Some(last) = state.last_fired {
        if last.elapsed() < FIRE_COOLDOWN {
            return;
        }
    }
    // 刷新矩形这件事不受冷却影响：用户每次选中新内容都应该更新判定区域，
    // 否则"移开就消失"会围着上一次的选区转。
    request_rect();

    let (cx, cy) = cursor_point();
    state.pending = Some(Pending {
        text: String::new(),
        x: cx,
        y: cy,
        // 0 x 0 = 暂时没有精确矩形；下一帧会从 take_rect() 补上
        w: 0,
        h: 0,
    });
    state.inside = false;
    state.entered_at = None;
    state.translated = false;
    state.anchor_at = Some(std::time::Instant::now());

    if state.require_hover {
        dbg(&format!("已武装选区锚点 @({cx},{cy})，等鼠标移上来"));
    } else {
        // 立即模式：选中就翻译，不等悬停。少一层等待，手感更快。
        // 仍然置 translated=true，这样鼠标留在选区上不会反复触发；
        // 鼠标一离开会被重置，移回来就能再翻一次。
        state.translated = true;
        state.last_fired = Some(std::time::Instant::now());
        let _ = state.tx.try_send(());
        dbg("立即模式：已通知工作线程去抓取");
    }
}

/// 收起弹窗。钩子线程调用。
///
/// **不维护"弹窗当前是否可见"的状态标记。**
/// 曾经加过 `popup_shown` 布尔量，但它必然和真实状态脱节——浮窗会自己超时关闭
/// （前端 6 秒定时器），钩子根本不知道；反过来也会出现"明明显示着却说没显示"
/// 而不发隐藏指令。隐藏窗口是幂等操作，直接调就行，记状态反而制造 bug。
fn hide_popup(state: &HookState, reason: &str) {
    dbg(&format!("收起弹窗（原因：{reason}）"));
    (state.on_leave)();
}

/// 鼠标移动时的"悬停判定"。返回 true 表示该翻译了。
///
/// 行为（按用户要求）：
///   鼠标移到选区上方 → 停留一下 → 翻译
///   鼠标移开         → 记为"未翻译"，不再打扰
///   再移回来         → 可以再翻一次
fn hover_check(pt: (i32, i32), state: &mut HookState) -> HoverOutcome {
    // 先看看异步线程有没有取回真正的选区矩形，补上（只消费一次）
    if let Some(rect) = take_rect() {
        if let Some(p) = state.pending.as_mut() {
            p.x = rect.x;
            p.y = rect.y;
            p.w = rect.w;
            p.h = rect.h;
            dbg(&format!(
                "已补上选区矩形 ({},{},{}x{})，整个选区都是悬停区域",
                rect.x, rect.y, rect.w, rect.h
            ));
        }
    }
    let Some(p) = state.pending.clone() else {
        return HoverOutcome::None;
    };

    // 矩形可用时判定"是否在选区内"；拿不到矩形才退化成锚点附近的宽松半径
    let inside = if p.w <= 0 || p.h <= 0 {
        (pt.0 - p.x).abs() <= HOVER_FALLBACK_R && (pt.1 - p.y).abs() <= HOVER_FALLBACK_R
    } else {
        let (x0, y0) = (p.x - HOVER_PAD, p.y - HOVER_PAD);
        let (x1, y1) = (p.x + p.w + HOVER_PAD, p.y + p.h + HOVER_PAD);
        pt.0 >= x0 && pt.0 <= x1 && pt.1 >= y0 && pt.1 <= y1
    };

    if !inside {
        let was_showing = state.inside || state.translated;
        state.inside = false;
        state.entered_at = None;
        if !was_showing {
            return HoverOutcome::None; // 本来就没显示，没什么可关的
        }
        // 刚选中时手还没停稳，鼠标很容易瞬间抖出选区。此刻先别关，
        // 否则会出现"弹窗刚出现就被隐藏、界面上却还残留一个窗口"的诡异现象。
        if let Some(t) = state.anchor_at {
            if t.elapsed() < ANCHOR_SETTLE {
                dbg(&format!(
                    "鼠标在选区外，但仍在选中宽限期内（{}ms < {}ms），暂不关",
                    t.elapsed().as_millis(),
                    ANCHOR_SETTLE.as_millis()
                ));
                return HoverOutcome::None;
            }
        }
        dbg(&format!(
            "鼠标离开选区 @({},{})，矩形 ({},{},{}x{}) → 收起弹窗",
            pt.0, pt.1, p.x, p.y, p.w, p.h
        ));
        state.translated = false;
        hide_popup(state, "leave");
        return HoverOutcome::Left;
    }

    if state.translated {
        return HoverOutcome::None; // 已经在显示了
    }
    // 进入选区（含"曾经离开、又移回来"的情况）→ 开始计时
    if !state.inside || state.entered_at.is_none() {
        state.inside = true;
        state.entered_at = Some(std::time::Instant::now());
        dbg("鼠标进入选区，开始停留计时");
        return HoverOutcome::None;
    }
    match state.entered_at {
        Some(t) if t.elapsed() >= HOVER_DWELL => {
            // 冷却闸门：距上次触发太近就不翻，避免连发
            if let Some(last) = state.last_fired {
                if last.elapsed() < FIRE_COOLDOWN {
                    return HoverOutcome::None;
                }
            }
            state.translated = true;
            state.last_fired = Some(std::time::Instant::now());
            dbg("悬停满足，触发翻译");
            HoverOutcome::Fire
        }
        _ => HoverOutcome::None,
    }
}

/// 钩子的判定结果。
#[derive(PartialEq, Clone, Copy, Debug)]
enum Verdict {
    /// 无关事件
    Ignore,
    /// 确认了"用户选中了一段文字"（可能是"按住拖拽结束"，也可能是"双击选词"）
    Armed,
    /// 悬停判定成立，应立刻翻译。
    /// 单独一个变体，是为了让它绕开外层针对"选中动作"的冷却判断——
    /// 悬停的冷却由 `hover_check` 内部负责，外层再判一次会把自己拦死。
    HoverFire,
    /// 鼠标离开了选区 → 主程序应立刻收起弹窗
    HoverLeft,
    /// 在选区以外的空白处点了鼠标 → 收起弹窗
    ClickOutside,
}

/// `hover_check` 的三态结果。
#[derive(PartialEq, Clone, Copy, Debug)]
enum HoverOutcome {
    /// 没变化
    None,
    /// 该翻译了
    Fire,
    /// 鼠标离开了选区
    Left,
}

/// 钩子状态机（纯逻辑，便于阅读和测试）。
fn handle_mouse(msg: u32, pt: (i32, i32), is_self: bool, state: &mut HookState) -> Verdict {
    let before = match state.stage {
        Stage::Idle => "Idle",
        Stage::Down { .. } => "Down",
        Stage::Drag { .. } => "Drag",
        Stage::WaitDouble { .. } => "WaitDouble",
    };
    let verdict = handle_mouse_inner(msg, pt, is_self, state);
    let msg_name = match msg {
        WM_LBUTTONDOWN => "DOWN",
        WM_LBUTTONUP => "UP",
        WM_MOUSEMOVE => "MOVE",
        WM_RBUTTONUP => "RUP",
        _ => "?",
    };
    // MOVE 太频繁，只在它带来状态变化（确认选中/进入拖拽）时记一行
    if msg != WM_MOUSEMOVE || verdict != Verdict::Ignore {
        dbg(&format!(
            "鼠标 {msg_name} @({},{}) is_self={is_self} {before} → {:?}",
            pt.0, pt.1, verdict
        ));
    }
    verdict
}

fn handle_mouse_inner(msg: u32, pt: (i32, i32), is_self: bool, state: &mut HookState) -> Verdict {
    match msg {
        WM_LBUTTONDOWN => {
            state.lbtn_down = true;
            if is_self {
                state.stage = Stage::Idle;
                return Verdict::Ignore;
            }
            // 先判断双击：如果上一次点击刚结束、位置和时间都接近，这就是"选中一个词"。
            //
            // 这一步**必须放在最前面**。否则双击的第一下会被后面的"点空白"逻辑
            // 当成普通点击、把 pending 清掉，第二次点击就不认双击了——
            // 表现就是"根本选不中词、弹窗再也不出现"。
            if let Stage::WaitDouble { at, pt: p } = state.stage {
                let close = (pt.0 - p.0).abs() <= DOUBLE_CLICK_PX
                    && (pt.1 - p.1).abs() <= DOUBLE_CLICK_PX;
                if close && at.elapsed() <= DOUBLE_CLICK_WINDOW {
                    state.stage = Stage::Idle;
                    return Verdict::Armed;
                }
            }

            // 判断这次点击是不是落在"刚选中的那段文字"上。
            //
            // 除了"点选区自己"（用户想重新选词），**其它任何点击都收起弹窗**。
            // 这里刻意不做"当前是否显示着"的前置判断：translated / inside 这些标记
            // 会因为超时、宽限期、坐标刷新等原因和真实状态脱节，一旦都是 false，
            // 点击就完全不响应——那正是之前"点空白关不掉"的病根。
            // 隐藏窗口是幂等操作，无脑调用没有代价。
            let on_selection = state
                .pending
                .as_ref()
                .map(|p| {
                    if p.w <= 0 || p.h <= 0 {
                        // 还没拿到精确矩形，用宽松半径兜底
                        (pt.0 - p.x).abs() <= HOVER_FALLBACK_R
                            && (pt.1 - p.y).abs() <= HOVER_FALLBACK_R
                    } else {
                        let (x0, y0) = (p.x - HOVER_PAD, p.y - HOVER_PAD);
                        let (x1, y1) = (p.x + p.w + HOVER_PAD, p.y + p.h + HOVER_PAD);
                        pt.0 >= x0 && pt.0 <= x1 && pt.1 >= y0 && pt.1 <= y1
                    }
                })
                .unwrap_or(false);

            // 必须先把状态推进到 Down，这样松开时才能进入 WaitDouble 等待双击。
            // 之前是"先 return ClickOutside、后置状态"，等于每次点击都把状态清掉。
            state.stage = Stage::Down {
                at: std::time::Instant::now(),
                pt,
                moved: 0,
            };

            // 点选区以外的地方 → 收起弹窗。
            // 注意**不清 pending**：矩形要留着，否则同一轮里"点在不在选区上"就没法判断了。
            if !on_selection {
                state.translated = false;
                state.inside = false;
                state.entered_at = None;
                return Verdict::ClickOutside;
            }
            Verdict::Ignore
        }
        WM_MOUSEMOVE => {
            // 先做悬停判定：鼠标是否在（或离开了）刚才选中的区域
            if state.require_hover {
                match hover_check(pt, state) {
                    HoverOutcome::Fire => return Verdict::HoverFire,
                    HoverOutcome::Left => return Verdict::HoverLeft,
                    HoverOutcome::None => {}
                }
            }
            // 按住左键移动超过阈值 → 进入拖拽。WaitDouble 也要能进，
            // 因为"单击一下再按住拖"是常见操作。
            // 但必须"连续多次移动都超阈值"才算——否则双击时手抖 1px 会被误判成拖拽。
            match state.stage {
                Stage::Down { at, pt: p, moved } => {
                    if (pt.0 - p.0).abs() > DRAG_PX || (pt.1 - p.1).abs() > DRAG_PX {
                        let moved = moved + 1;
                        // 移动次数够、且按下已有一小段时间 → 判定为拖拽
                        if moved >= DRAG_CONFIRM_MOVES
                            && at.elapsed().as_millis() as u64 >= DRAG_MIN_MS
                        {
                            state.stage = Stage::Drag { pt };
                            dbg(&format!("进入拖拽 @({},{})", pt.0, pt.1));
                        } else {
                            state.stage = Stage::Down { at, pt: p, moved };
                        }
                    }
                }
                Stage::WaitDouble { at, pt: p } => {
                    // 只有左键真的按着，才允许从"等待双击"转成"拖拽"。
                    // 否则鼠标单纯移动就会把状态推进到拖拽，埋下误触发的根。
                    if state.lbtn_down
                        && ((pt.0 - p.0).abs() > DRAG_PX || (pt.1 - p.1).abs() > DRAG_PX)
                    {
                        state.stage = Stage::Down {
                            at,
                            pt: p,
                            moved: 1,
                        };
                    }
                }
                _ => {}
            }
            Verdict::Ignore
        }
        WM_LBUTTONUP => {
            state.lbtn_down = false;
            let v = match state.stage {
                // 拖拽结束 → 选区已确定（必须是真的按着拖过）
                Stage::Drag { .. } => {
                    state.stage = Stage::Idle;
                    if is_self {
                        Verdict::Ignore
                    } else {
                        Verdict::Armed
                    }
                }
                // 普通单击 → 进入双击等待
                Stage::Down { at, pt: p, .. } => {
                    state.stage = Stage::WaitDouble { at, pt: p };
                    Verdict::Ignore
                }
                _ => {
                    state.stage = Stage::Idle;
                    Verdict::Ignore
                }
            };
            v
        }
        WM_RBUTTONUP => {
            state.stage = Stage::Idle;
            Verdict::Ignore
        }
        _ => Verdict::Ignore,
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let state_ptr = std::ptr::addr_of_mut!(STATE);
        if let Some(state) = (*state_ptr).as_mut() {
            let msg = wparam.0 as u32;
            // 消息白名单必须把滚轮也算进来。
            // 之前把滚轮处理写在白名单**里面**，而白名单里没有滚轮消息，
            // 于是那段代码永远执行不到——典型的"写了但没生效"。
            if matches!(
                msg,
                WM_LBUTTONDOWN
                    | WM_LBUTTONUP
                    | WM_MOUSEMOVE
                    | WM_RBUTTONUP
                    | WM_MOUSEWHEEL
                    | WM_MOUSEHWHEEL
            ) {
                let msll = &*(lparam.0
                    as *const windows::Win32::UI::WindowsAndMessaging::MSLLHOOKSTRUCT);
                // 忽略我们自己合成出来的鼠标事件，避免自激
                let injected = (msll.flags
                    & windows::Win32::UI::WindowsAndMessaging::LLMHF_INJECTED)
                    != 0;
                if !injected {
                    let pt = msll.pt;
                    let is_self = foreground_info(state.own_pid).2;

                    // 滚轮：选区在屏幕上的位置变了，之前记下的矩形和文本都作废。
                    // 必须主动处理——滚动时鼠标**并不移动**，收不到 WM_MOUSEMOVE，
                    // 否则会继续按旧矩形判定，表现为"滚动网页后弹出上次的翻译"。
                    if msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL {
                        state.pending = None;
                        state.inside = false;
                        state.entered_at = None;
                        state.translated = false;
                        state.stage = Stage::Idle;
                        dbg("滚轮滚动 → 作废选区并收起弹窗");
                        hide_popup(state, "leave");
                        return CallNextHookEx(None, code, wparam, lparam);
                    }

                    // 刚记录完选区时，如果鼠标**本来就在选区上**（双击选词后很常见），
                    // 那就直接开始计时，不必让用户"先移开再移回来"。
                    // 注：这里的返回值不用处理——本次不可能是"离开"（刚武装）。
                    if state.require_hover && state.pending.is_some() && !state.inside {
                        hover_check((pt.x, pt.y), state);
                    }
                    let verdict = handle_mouse(msg, (pt.x, pt.y), is_self, state);

                    match verdict {
                        // 悬停成立 → 直接放行（冷却由 hover_check 内部管）
                        Verdict::HoverFire => {
                            if state.tx.try_send(()).is_ok() {
                                dbg("悬停触发 → 已通知工作线程去抓取");
                            } else {
                                dbg("工作线程忙，忽略这次悬停触发");
                            }
                        }
                        // 鼠标离开选区
                        // （注意：hover_check 内部已经调过 hide_popup 并重置状态，
                        //  这里只做日志，避免重复发隐藏指令）
                        Verdict::HoverLeft => {
                            dbg("收到 HoverLeft");
                        }
                        // 在空白处点击 → 收起弹窗
                        Verdict::ClickOutside => {
                            dbg("在选区外点击 → 收起弹窗");
                            (state.on_click_outside)();
                        }
                        // 选中动作（拖拽结束 / 双击）
                        Verdict::Armed => {
                            if state.require_hover {
                                // 只记下选区，等鼠标移上来再翻
                                arm_pending(state);
                            } else if state.last_capture.elapsed() < COOLDOWN {
                                dbg("冷却期内，忽略这次触发");
                            } else if state.tx.try_send(()).is_ok() {
                                dbg("已通知工作线程去抓取");
                            } else {
                                dbg("工作线程忙，忽略这次触发");
                            }
                        }
                        Verdict::Ignore => {}
                    }
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// 将 HHOOK 转成 usize 便于跨线程传递（句柄本身可以跨线程使用）。
struct SendHook(usize);

/// 启动全局鼠标钩子。返回 false 表示安装失败（极少数环境会被安全软件拦）。
pub fn start(
    on_select: OnSelect,
    on_leave: Box<dyn Fn() + Send + 'static>,
    on_click_outside: Box<dyn Fn() + Send + 'static>,
) -> bool {
    // 工作线程与钩子之间只用"该抓了"这一个信号，容量 1，避免积压
    let (tx, rx) = sync_channel::<()>(1);

    unsafe {
        let own_pid = std::process::id();
        let state_ptr = std::ptr::addr_of_mut!(STATE);
        *state_ptr = Some(HookState {
            stage: Stage::Idle,
            own_pid,
            last_capture: std::time::Instant::now() - COOLDOWN * 10,
            tx,
            pending: None,
            inside: false,
            entered_at: None,
            translated: false,
            last_fired: None,
            anchor_at: None,
            lbtn_down: false,
            // 默认"选中即翻译"（立即模式）。想要"鼠标移回选区才翻译"的人
            // 可以在设置里把 require_hover 打开。
            require_hover: false,
            on_leave,
            on_click_outside,
        });

        let Ok(hook) = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0) else {
            eprintln!("[选区] 安装鼠标钩子失败，将只能靠复制即译");
            dbg("SetWindowsHookExW 失败");
            return false;
        };
        let hook_ptr = SendHook(hook.0 as usize);
        dbg(&format!("鼠标钩子已安装 (pid={own_pid})"));

        // ① 工作线程：负责"等待选区稳定 → 抓取 → 回调"
        std::thread::spawn(move || {
            dbg("工作线程已启动，等待抓取信号");
            let on_select = on_select;
            while rx.recv().is_ok() {
                dbg("工作线程收到信号，开始等待选区稳定");
                std::thread::sleep(std::time::Duration::from_millis(SETTLE_MS));
                dbg("开始抓取选区");
                let grabbed = grab_selection();
                match grabbed {
                    Ok((text, via_clipboard)) => {
                        dbg(&format!(
                            "抓到选区「{text}」（{}），准备交给主程序",
                            if via_clipboard { "经剪贴板" } else { "UIA·未碰剪贴板" }
                        ));
                        // 这里**不做"和上次相同就跳过"的去重**。
                        // 曾经加过，结果用户重新选同一段文字时被吞掉、不翻译了。
                        // 每次用户选择都是新意图，必须响应；防抖交给钩子侧的冷却窗口。
                        if let Some(state) = (*std::ptr::addr_of_mut!(STATE)).as_mut() {
                            state.last_capture = std::time::Instant::now();
                        }
                        dbg("调用回调，交给主程序翻译");
                        on_select(text);
                        dbg("回调已返回");
                    }
                    Err(e) => dbg(&format!("抓取失败: {e}")),
                }
            }
            dbg("工作线程退出（channel 关闭）");
        });

        // ② 钩子线程：必须自己跑消息循环，否则低级钩子收不到回调
        std::thread::spawn(move || {
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = UnhookWindowsHookEx(HHOOK(hook_ptr.0 as *mut core::ffi::c_void));
        });
        true
    }
}

/// 当前鼠标位置（物理像素），用作弹窗锚点。
pub fn cursor_point() -> (i32, i32) {    let mut pt = POINT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
    }
    (pt.x, pt.y)
}
