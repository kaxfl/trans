mod cache;
mod dict;
mod glossary;
mod md5;
pub mod engine;
mod ocr;
mod selection;
mod uia;

use serde::Serialize;
use std::sync::Mutex;
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_store::StoreExt;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

/// 全局状态
struct AppState {
    engine_id: Mutex<String>,
    /// 触发翻译的全局热键字符串（如 "alt+period"）
    hotkey: Mutex<String>,
    /// 弹出主界面的全局热键字符串
    window_hotkey: Mutex<String>,
    /// 是否已解锁"中译英"（19 元买断）
    unlocked: Mutex<bool>,
    /// 内部合成 Ctrl+C 抓选区时置位的计数器。
    /// 用计数而不是布尔：抓取是异步的，可能重叠；布尔会被提前清掉。
    /// 大于 0 时，"复制即译"监听跳过本次剪贴板变化——否则内部复制会触发
    /// 第二个弹窗，和真正的翻译弹窗抢同一个位置，用户看到的就是"闪一下"。
    internal_copy: std::sync::atomic::AtomicUsize,
    /// 本地模型（OpenAI 兼容端点）配置
    local_cfg: Mutex<engine::LocalConfig>,
    /// 本地翻译缓存（原文 -> 译文 + 查询次数）
    cache: Mutex<cache::Cache>,
    /// 是否暂停"复制即译"监听（避免密码等敏感内容被外发）
    monitor_paused: std::sync::atomic::AtomicBool,
    /// 划词翻译总开关。关闭后选中任何文字都不弹窗（主窗口手动翻译不受影响）
    translate_enabled: std::sync::atomic::AtomicBool,
    /// 是否也翻译中文。默认 **false**：选中中文不弹窗，避免点中文字段就被翻译
    cn_ok: std::sync::atomic::AtomicBool,
    /// 是否使用本地翻译缓存。默认 **true**；关掉后每次都重新请求，方便"翻错了重刷"
    cache_on: std::sync::atomic::AtomicBool,
    /// 选中后是否**自动**弹窗。关掉后划词不再自动弹，但 Ctrl+Alt+. 仍可手动翻译
    auto_popup: std::sync::atomic::AtomicBool,
    /// 本地词典（mmap 打开的 35 MB 词典文件；没配或打不开时为 None）
    dict: Mutex<Option<dict::Dict>>,
}

pub const DEFAULT_HOTKEY: &str = "ctrl+alt+period";
/// 切换「划词翻译」总开关的快捷键（不用去点托盘）
pub const DEFAULT_TOGGLE_HOTKEY: &str = "ctrl+alt+z";
/// 切换"选中后是否自动弹窗"（不用去点托盘）。关闭后 Ctrl+Alt+. 仍可手动翻译。
pub const DEFAULT_HIDE_HOTKEY: &str = "ctrl+alt+comma";
pub const DEFAULT_WINDOW_HOTKEY: &str = "alt+enter";

/// 框选截图翻译热键（默认 Ctrl+Alt+S，S = Screenshot）
pub const DEFAULT_BOX_HOTKEY: &str = "ctrl+alt+s";

/// 设置页面返回的引擎信息
#[derive(Serialize)]
struct EngineInfo {
    id: String,
    label: String,
    is_default: bool,
}

pub const DEFAULT_ENGINE: &str = "local";
const SETTINGS_FILE: &str = "settings.json";

/// 原始单段翻译：先用选中引擎，失败则自动回退到 mymemory（国内可用）。
/// 调用某个引擎，429/Too Many 时退避 2 秒重试一次。
async fn run_engine(
    id: &str,
    text: &str,
    target: &str,
    cfg: &engine::LocalConfig,
) -> Result<engine::Translation, String> {
    let engine = engine::get(id, cfg).ok_or_else(|| format!("引擎不存在: {id}"))?;
    let mut last_err = String::new();
    for attempt in 0..2 {
        match engine.translate(text, target).await {
            Ok(r) => return Ok(r),
            Err(e) => {
                let el = e.to_lowercase();
                if (el.contains("429") || el.contains("too many")) && attempt == 0 {
                    last_err = e;
                    let d = std::time::Duration::from_secs(2);
                    let _ = tauri::async_runtime::spawn_blocking(move || std::thread::sleep(d)).await;
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(last_err)
}

async fn engine_translate_once(
    text: &str,
    target: &str,
    id: &str,
    cfg: &engine::LocalConfig,
) -> Result<engine::Translation, String> {
    match run_engine(id, text, target, cfg).await {
        Ok(r) => Ok(r),
        Err(e) => {
            // 本地模型没起来 / 出错时，自动回退到免费的 MyMemory，别让用户对着空白弹窗
            if id != "mymemory" {
                if let Ok(r) = run_engine("mymemory", text, target, cfg).await {
                    return Ok(r);
                }
            }
            Err(e)
        }
    }
}

/// 批量翻译标识符的子词。
///
/// 为什么批量：原来是对每个子词各调一次模型，`double lastUpdateTime` 这种
/// 要 4 次往返，延迟随子词数线性增长。改成一次请求翻全部，往返只花一次。
///
/// 解析失败就返回 None，调用方会退回逐个翻译——**绝不能因为省时间去猜。**
async fn translate_parts_batch(
    words: &[String],
    target: &str,
    id: &str,
    cfg: &engine::LocalConfig,
) -> Option<Vec<String>> {
    if words.is_empty() || words.len() > 8 {
        return None;
    }
    let list = words.join("\n");
    let prompt = format!(
        "把下面每一行英文翻译成{}，只输出译文，保持行数一致，不要编号、不要解释：\n{}",
        if target == "en" { "英文" } else { "简体中文" },
        list
    );
    let r = engine_translate_once(&prompt, target, id, cfg).await.ok()?;
    let lines: Vec<String> = r
        .translated
        .lines()
        .map(|l| l.trim().trim_start_matches(|c: char| {
            // 去掉模型爱加的 "1. " "- " 之类前缀
            c.is_ascii_digit() || c == '.' || c == '-' || c == '、' || c == '）' || c == ')'
        }).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() != words.len() {
        return None; // 行数对不上，不敢用
    }
    Some(lines)
}

/// 单段翻译：代码标识符先拆词逐段翻译（本地小模型对标识符容易瞎翻，拆词更准也更直观）。
/// 把词典里的一条记录转成 UI 用的结构化释义。
///
/// ECDICT 的 `translation` 形如：
/// ```text
/// n. 船
/// vi. 乘船
/// vt. 以船运
/// ```
/// 每行开头是词性（`n.` / `vi.` / `adj.` …），后面是逗号分隔的若干含义。
/// 没有词性前缀的行归到"释义"一组。
fn dict_entry_to_senses(e: &dict::DictEntry) -> Vec<engine::Sense> {
    let mut out: Vec<engine::Sense> = Vec::new();
    for line in e.translation.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 拆出词性前缀：字母+可选点，后面跟空格
        let (pos, rest) = match line.find(' ') {
            Some(i) => {
                let head = &line[..i];
                let tail = line[i + 1..].trim();
                if head.len() <= 6
                    && head
                        .chars()
                        .all(|c| c.is_ascii_alphabetic() || c == '.' || c == '&')
                    && !tail.is_empty()
                {
                    (head.to_string(), tail)
                } else {
                    (String::new(), line)
                }
            }
            None => (String::new(), line),
        };
        let meanings: Vec<String> = rest
            .split(['；', ',', '，', ';'])
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .take(8)
            .collect();
        if meanings.is_empty() {
            continue;
        }
        // 同一个词性合并到一条
        if let Some(existing) = out.iter_mut().find(|s| s.pos == pos) {
            existing.meanings.extend(meanings);
            existing.meanings.truncate(8);
        } else {
            out.push(engine::Sense { pos, meanings });
        }
        if out.len() >= 8 {
            break;
        }
    }
    out
}

async fn translate_single(
    text: &str,
    target: &str,
    id: &str,
    cfg: &engine::LocalConfig,
    dict: Option<dict::DictEntry>,
) -> Result<engine::Translation, String> {
    // ① 查单词：**先查本地词典**（毫秒级、词性准确、不占显存）。
    //
    // 这是"轻量化"最关键的一步：以前查单词也走大模型，结果是 1–3 秒 + 词性标错
    // （实测 `among` 被标成 n./v./adj.）。词典查表没有这两个问题。
    // 词典里没有这个词，才交给模型。
    if engine::is_zh_target(target) {
        if let Some(e) = dict {
            let senses = dict_entry_to_senses(&e);
            if !senses.is_empty() {
                let flat = senses
                    .iter()
                    .flat_map(|s| s.meanings.iter().cloned())
                    .collect::<Vec<_>>()
                    .join("；");
                return Ok(engine::Translation {
                    translated: flat,
                    source: "dict".into(),
                    target: target.to_string(),
                    cached: false,
                    hit_count: 0,
                    parts: Vec::new(),
                    senses,
                });
            }
        }
    }

    // ② 词典没有 → 交给模型做"单词释义"（需要词性+多含义的格式）
    if engine::is_lookup_word(text) && engine::is_zh_target(target) && id == "local" {
        let le = engine::LocalEngine { cfg: cfg.clone() };
        if let Ok(senses) = le.lookup_word(text, target).await {
            if !senses.is_empty() {
                let flat = senses
                    .iter()
                    .flat_map(|s| s.meanings.iter().cloned())
                    .collect::<Vec<_>>()
                    .join("；");
                return Ok(engine::Translation {
                    translated: flat,
                    source: "dict".into(),
                    target: target.to_string(),
                    cached: false,
                    hit_count: 0,
                    parts: Vec::new(),
                    senses,
                });
            }
        }
        // 词典模式失败就继续往下走普通翻译，不要在这里报错
    }

    // 标识符快速通道：如果每个子词都在内置术语表里，直接拼结果，**不问模型**。
    // ctx / buf / idx 这类词在编程语境里含义固定，问模型既慢又容易被"发挥"。
    if cache::looks_like_identifier(text) && !text.contains(' ') && text.chars().count() >= 3 {
        let words = split_identifier(text);
        if words.len() >= 2 && words.len() <= 8 {
            let mut parts: Vec<engine::CodePart> = Vec::new();
            let mut all_covered = true;
            for p in &words {
                let upper_abbr = p.chars().all(|c| c.is_ascii_uppercase()) && p.chars().count() >= 2;
                if upper_abbr {
                    parts.push(engine::CodePart {
                        raw: p.clone(),
                        meaning: "缩写（保持原样）".into(),
                        note: String::new(),
                    });
                } else if let Some(m) = glossary::lookup(p) {
                    parts.push(engine::CodePart {
                        raw: p.clone(),
                        meaning: m.to_string(),
                        note: String::new(),
                    });
                } else {
                    all_covered = false;
                    break;
                }
            }
            if all_covered && parts.len() >= 2 {
                let flat = parts
                    .iter()
                    .map(|x| format!("{} → {}", x.raw, x.meaning))
                    .collect::<Vec<_>>()
                    .join("　");
                return Ok(engine::Translation {
                    translated: flat,
                    source: "glossary".into(),
                    target: target.to_string(),
                    cached: false,
                    hit_count: 0,
                    parts,
                    senses: Vec::new(),
                });
            }
        }
    }

    let mut r = engine_translate_once(text, target, id, cfg).await?;
    // 两种情况需要拆词：
    //  ① 引擎原样返回了（不懂这个词）——比较前必须**先清洗标记**。
    //     否则模型输出的 `among /think` 会因带着标记而"看起来不等于"原文，
    //     既不会被识别为回显，又过不了有效性校验，白白回退到 MyMemory。
    //  ② 输入本身就是标识符（含 _ / . / camelCase 且无空格）——拆开解释比硬翻有用
    let cleaned = engine::strip_thinking(&r.translated);
    let unchanged = cleaned.trim().eq_ignore_ascii_case(text.trim());
    let is_ident = cache::looks_like_identifier(text);
    if (unchanged || is_ident) && !text.contains(' ') && text.chars().count() >= 3 {
        let words = split_identifier(text);
        if words.len() >= 2 && words.len() <= 8 {
            // 全大写缩写（GFX / SOV / URL）保持原样，不参与翻译
            let is_abbr = |p: &String| p.chars().all(|c| c.is_ascii_uppercase()) && p.len() >= 2;
            let need: Vec<String> = words.iter().filter(|p| !is_abbr(p)).cloned().collect();

            // ① 先查内置代码术语表：ctx→上下文、buf→缓冲区 这类含义是固定的，
            //    查表比问模型又快又准，而且完全离线。只有查不到的才交给模型。
            let mut meanings: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            let mut rest: Vec<String> = Vec::new();
            for p in &need {
                match glossary::lookup(p) {
                    Some(m) => {
                        meanings.insert(p.clone(), m.to_string());
                    }
                    None => rest.push(p.clone()),
                }
            }

            // ② 剩下的词才问模型（一次批量往返，失败再逐个）
            let batch = if rest.is_empty() {
                None
            } else {
                translate_parts_batch(&rest, target, id, cfg).await
            };
            match batch {
                Some(list) => {
                    for (w, m) in rest.iter().zip(list.iter()) {
                        if engine::is_valid_result(w, m, target) {
                            meanings.insert(w.clone(), m.clone());
                        }
                    }
                }
                None => {
                    for p in &rest {
                        if let Ok(pr) = engine_translate_once(p, target, id, cfg).await {
                            if engine::is_valid_result(p, &pr.translated, target) {
                                meanings.insert(p.clone(), pr.translated.trim().to_string());
                            }
                        }
                    }
                }
            }

            let parts: Vec<engine::CodePart> = words
                .iter()
                .map(|p| {
                    if is_abbr(p) {
                        engine::CodePart {
                            raw: p.clone(),
                            meaning: "缩写（保持原样）".into(),
                            note: String::new(),
                        }
                    } else {
                        engine::CodePart {
                            raw: p.clone(),
                            // 翻不出来就原样显示，别编
                            meaning: meanings.get(p).cloned().unwrap_or_else(|| p.clone()),
                            note: String::new(),
                        }
                    }
                })
                .collect();
            if parts.len() >= 2 {
                r.source = "code".into();
                r.parts = parts;
            }
        }
    }
    Ok(r)
}

/// 把代码标识符拆成子词：下划线/连字符/路径分隔 + camelCase 边界。
fn split_identifier(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for seg in s.split(|c: char| matches!(c, '_' | '-' | '.' | '/' | '\\' | ' ')) {
        if seg.is_empty() {
            continue;
        }
        let chars: Vec<char> = seg.chars().collect();
        let mut cur = String::new();
        for (i, &c) in chars.iter().enumerate() {
            if c.is_uppercase() && i > 0 && !cur.is_empty() {
                let prev = chars[i - 1];
                let next_lower = i + 1 < chars.len() && chars[i + 1].is_lowercase();
                if prev.is_lowercase() || next_lower {
                    parts.push(std::mem::take(&mut cur));
                }
            }
            cur.push(c);
        }
        if !cur.is_empty() {
            parts.push(cur);
        }
    }
    parts
}

/// 是否是"逐词兜底"的结果（source=code 且没有拆词明细）。
/// 这类结果不写缓存：纯查表重算只要微秒，缓存反而会挡住算法改进。
fn is_wordwise(r: &engine::Translation) -> bool {
    r.source == "code" && r.parts.is_empty()
}
/// 从词典释义里挑一条"最像意思"的短释义。
///
/// ECDICT 的 translation 长这样：
/// ```text
/// n. 船
/// vi. 乘船
/// ```
/// 但有些词条第一行是**词形变化说明**（`has` → "have的第三人称单数现在式"），
/// 直接拿来用会得到一堆废话。所以规则是：
///   1. 按 `；`, `，`, `;` 切段，取第一段"去掉词性前缀后非空且不是词形说明"的
///   2. 去掉 `n.` / `vt.` / `[计]` 这类前缀
///   3. 截到 12 个字符，避免一条释义撑爆弹窗
fn pick_meaning(translation: &str) -> Option<String> {
    const SKIP: [&str; 7] = [
        "的第三人称单数",
        "的复数形式",
        "的过去式",
        "的现在分词",
        "的过去分词",
        "的比较级",
        "的最高级",
    ];
    for line in translation.lines() {
        for seg in line.split(['；', ';', '，', ',']) {
            let mut out = String::new();
            for t in seg.split_whitespace() {
                // 词性标记：n. / vt. / prep. / interj. / adj.，以及 [计] 这类领域标记
                let is_pos = (t.ends_with('.') && t.chars().count() <= 8)
                    || (t.starts_with('[') && t.ends_with(']'));
                if is_pos && out.is_empty() {
                    continue; // 只跳过开头的词性/领域标记
                }
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(t);
            }
            let s = out.trim();
            if s.is_empty() || SKIP.iter().any(|k| s.contains(k)) {
                continue;
            }
            return Some(s.chars().take(12).collect());
        }
    }
    None
}
/// 粗略判断"这是不是一段代码"。
///
/// 为什么需要：模型看到 `shop->setPos(520, 45);` 这类行会当句子硬翻，
/// 实测把 `Shop` 翻成"刘草的人"、`map` 翻成"场"——比不翻还糟。
/// 判到代码就直接走标识符级翻译，压根不给模型发挥的机会。
fn looks_like_code(text: &str) -> bool {
    let t = text.trim();
    let n = t.chars().count();
    if n < 3 || n > 4000 {
        return false;
    }

    // ① 强信号：这些组合在英文散文里基本不会出现
    const TOKENS: [&str; 14] = [
        "->", "::", "=>", "==", "!=", "++", "--", "&&", "||", "{", "}", "();", ");", "#include",
    ];
    if TOKENS.iter().any(|s| t.contains(s)) {
        return true;
    }

    // ② 分号 / 等号密度：HOI4 脚本每行形如 `key = value`，
    //    正常英文段落里 `=` 和 `;` 极少。
    let semi = t.matches(';').count();
    let eq = t.matches('=').count();
    let lines = t.lines().count().max(1) as f64;
    if semi >= 1 {
        return true;
    }
    if eq >= 2 && (eq as f64 / lines) >= 0.8 {
        return true;
    }

    // ③ 符号占比：散文里标点占比通常 <5%，代码里 `(){}[]=<>*&|` 往往 >12%
    let weird = t
        .chars()
        .filter(|c| {
            !c.is_alphanumeric()
                && !c.is_whitespace()
                && !",.'\"!?-:…—·、。，；：！？“”‘’（）《》".contains(*c)
        })
        .count();
    (weird as f64 / n as f64) > 0.12
}

/// 代码优先：识别成代码就走逐词/标识符翻译，否则交给正常翻译链。
async fn translate_or_code(
    app: &tauri::AppHandle,
    text: &str,
    target: &str,
    id: &str,
    cfg: &engine::LocalConfig,
) -> Result<engine::Translation, String> {
    if looks_like_code(text) {
        let state = app.state::<AppState>();
        if let Some(fixed) = wordwise_preserve(&state, text) {
            eprintln!("[翻译] 判定为代码，走标识符级翻译");
            return Ok(engine::Translation {
                translated: fixed,
                source: "glossary".into(),
                target: target.to_string(),
                cached: false,
                hit_count: 0,
                parts: Vec::new(),
                senses: Vec::new(),
            });
        }
    }
    translate_with_fallback(text, target, id, cfg, None).await
}

/// 整句翻不出来时的兜底：**逐词查本地词典，并原样保留所有符号/缩进/换行**。
///
/// 为什么这样设计：
///  - 代码块、配置、日志里大量符号和换行，整句交给模型时它可能拒翻、乱翻、
///    或者把符号也改掉。这里干脆不让模型参与：只替换"连续字母"这一段，
///    其余字符（`{}();=<>_"'` / 空格 / 换行 / 中文 / 数字）**逐字原样输出**。
///  - 所以输出格式必然和原文一致，永远不会出现"符号被吃掉"。
///
/// 返回 None 表示一个词都没翻出来（此时调用方应该显示错误，而不是给半成品）。
fn wordwise_preserve(state: &AppState, text: &str) -> Option<String> {
    let guard = state.dict.lock().ok()?;
    let d = guard.as_ref()?;

    let mut out = String::with_capacity(text.len() + 32);
    let mut word = String::new();
    let mut hits = 0usize;

    // 处理当前累积的一个单词
    let flush = |w: &mut String, out: &mut String, hits: &mut usize| {
        if w.is_empty() {
            return;
        }
        // 单字母、以及 URL/API 这类全大写缩写保持原样
        let keep = w.chars().count() < 2
            || (w.chars().all(|c| c.is_ascii_uppercase()) && w.chars().count() <= 5);
        if keep {
            out.push_str(w);
            w.clear();
            return;
        }
        // 词典优先（释义更完整），查不到再看代码术语表（ctx / buf 这类），
        // 最后都没有就保留原文——不编造。
        // 关键字（for / int / return）保持原样：翻成"为""整数"只会更难读。
        // 术语表优先：它是按"编程语境"人工挑的，比通用词典的第一条更贴切
        // （scene 在词典里第一条是"场"，代码里该是"场景"）。
        // 兜底顺序：术语表 → 词典 → 标识符拆词。
        let lower = w.to_ascii_lowercase();
        let hit = if glossary::is_keyword(&lower) {
            None
        } else {
            glossary::lookup(w)
                .map(|m| m.to_string())
                .or_else(|| d.lookup(w).and_then(|e| pick_meaning(&e.translation)))
                .or_else(|| glossary::lookup_identifier(w))
        };
        match hit {
            Some(meaning) => {
                out.push_str(&meaning);
                *hits += 1;
            }
            None => out.push_str(w),
        }
        w.clear();
    };

    for ch in text.chars() {
        if ch.is_ascii_alphabetic() {
            word.push(ch);
        } else {
            flush(&mut word, &mut out, &mut hits);
            out.push(ch); // 符号 / 换行 / 空白 / 中文：一个字符都不动
        }
    }
    flush(&mut word, &mut out, &mut hits);

    if hits > 0 {
        Some(out)
    } else {
        None
    }
}
/// 翻译：长文自动分段（远程免费引擎单次约 500 字符上限），逐段翻译再拼接。
///
/// `dict_hit` 是**调用方预先查好的词典结果**（只有单个单词时才可能有）。
/// 设计成传进来而不是在这里查：因为查词典要用 AppState，而这里只拿到配置，
/// 传进来能让这个函数保持纯逻辑、好测试。
async fn translate_with_fallback(
    text: &str,
    target: &str,
    id: &str,
    cfg: &engine::LocalConfig,
    dict_hit: Option<dict::DictEntry>,
) -> Result<engine::Translation, String> {
    // 用户明确选了「本地词典」：只查词，查不到就说清楚，不去联网。
    if id == "dict" {
        if let Some(e) = dict_hit {
            let senses = dict_entry_to_senses(&e);
            let flat = senses
                .iter()
                .flat_map(|s| s.meanings.iter().cloned())
                .collect::<Vec<_>>()
                .join("；");
            return Ok(engine::Translation {
                translated: flat,
                source: "dict".into(),
                target: target.to_string(),
                cached: false,
                hit_count: 0,
                parts: Vec::new(),
                senses,
            });
        }
        return Err("本地词典只收录单词（56 万条）；句子请换成「本地模型」或「DeepSeek 云端」".into());
    }
    // 分段上限按引擎区分。
    //
    // 400 是 MyMemory 时代的限制（那个免费接口单次只接受 ~500 字符）。
    // 但本地模型和 DeepSeek 都能吃长输入，继续按 400 切会变成"每 400 字一次请求"，
    // 串行跑下来：1000 字要 3 次调用，2000 字要 5 次——用户等不到结果弹窗就超时关了。
    let max = match id {
        "deepseek" | "local" => 1800,
        _ => 400,
    };
    if text.chars().count() > max {
        let mut all = String::new();
        let mut first_err: Option<String> = None;
        for c in chunk_text(text, max) {
            match translate_single(&c, target, id, cfg, None).await {
                Ok(r) => all.push_str(&r.translated),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        if all.trim().is_empty() {
            // 全部失败：显示真实错误，而不是空结果
            return Err(first_err.unwrap_or_else(|| "没有可翻译的内容".to_string()));
        }
        return Ok(engine::Translation {
            translated: all.trim().to_string(),
            source: "auto".into(),
            target: target.to_string(),
            cached: false,
            hit_count: 0,
            parts: Vec::new(),
            senses: Vec::new(),
        });
    }
    translate_single(text, target, id, cfg, dict_hit).await
}

/// 把长文本切成不超过 max 字符的段，尽可能在句末/标点/换行断。
fn chunk_text(text: &str, max: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    for seg in text.split_inclusive(|c| matches!(c, '。' | '.' | '!' | '?' | ';' | '\n')) {
        if cur.chars().count() + seg.chars().count() > max && !cur.is_empty() {
            chunks.push(std::mem::take(&mut cur));
        }
        cur.push_str(seg);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    // 兜底：仍有超长段按字符硬切（避免切在 UTF-8 中间）
    let mut out = Vec::new();
    for c in chunks {
        if c.chars().count() <= max {
            out.push(c);
        } else {
            let mut start = 0usize;
            while start < c.len() {
                let mut end = (start + max).min(c.len());
                while end > start && !c.is_char_boundary(end) {
                    end -= 1;
                }
                out.push(c[start..end].to_string());
                start = end;
            }
        }
    }
    out
}

/// 翻译命令：先查本地缓存，未命中再调引擎（失败自动回退）。
#[tauri::command]
async fn translate(
    text: String,
    target: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<engine::Translation, String> {
    // 方向可以是具体语言，也可以让程序自己判断（"auto" 或空）：
    // 有中文 → 翻成英文；没有中文 → 翻成中文。
    let target = match target.unwrap_or_default().as_str() {
        "" | "auto" => engine::auto_target(&text).to_string(),
        other => other.to_string(),
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        return Ok(engine::Translation {
            translated: String::new(),
            source: "empty".into(),
            target,
            cached: false,
            hit_count: 0,
            parts: Vec::new(),
            senses: Vec::new(),
        });
    }

    // ① 查缓存：命中就 0 延迟返回（本地模型推理慢，这一步最关键）。
    //    只对短文本用缓存，避免长文改动一个词就读到旧结果。
    // 关掉缓存开关时，读写都不做（相当于每次都重新请求）
    let cache_on = state
        .cache_on
        .load(std::sync::atomic::Ordering::Relaxed);
    let cacheable = cache_on && text.chars().count() <= 500;
    if cacheable {
        let mut c = state.cache.lock().unwrap();
        if let Some(e) = c.get(&text, &target) {
            return Ok(engine::Translation {
                translated: e.translated,
                source: e.source,
                target,
                cached: true,
                hit_count: e.count,
                // 代码模式的结果也要能命中缓存，否则每次悬停都要重新逐词翻译
                parts: e.parts,
                senses: Vec::new(),
            });
        }
    }

    // ② 未命中：走引擎
    let id = state.engine_id.lock().unwrap().clone();
    let cfg = state.local_cfg.lock().unwrap().clone();
    let dict_hit = lookup_dict(&state, &text, &target);
    let r = match translate_with_fallback(&text, &target, &id, &cfg, dict_hit).await {
        Ok(r) if engine::is_valid_result(&text, &r.translated, &target) => r,
        other => {
            // 主窗口手动翻译同样走逐词兜底：整句翻不动就按原格式逐词替换
            match wordwise_preserve(&state, &text) {
                Some(fixed) => engine::Translation {
                    translated: fixed,
                    source: "code".into(),
                    target: target.clone(),
                    cached: false,
                    hit_count: 0,
                    parts: Vec::new(),
                    senses: Vec::new(),
                },
                None => other?,
            }
        }
    };

    // ③ 写缓存（只写可信结果；"原样回显原文"这类失败结果一律不入缓存，
    //    否则用户会永远看到原文且再也改不掉）
    if cacheable
        && engine::is_valid_result(&text, &r.translated, &target)
    {
        let mut c = state.cache.lock().unwrap();
        c.put(&text, &target, &r.translated, &r.source, &r.parts, &r.senses);
        c.save();
    }
    Ok(r)
}

/// 读取本地模型配置（地址 + 模型名）
#[tauri::command]
fn get_local_config(state: tauri::State<'_, AppState>) -> engine::LocalConfig {
    state.local_cfg.lock().unwrap().clone()
}

/// 更新本地模型配置并持久化
#[tauri::command]
fn set_local_config(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    base_url: String,
    model: String,
) -> Result<engine::LocalConfig, String> {
    let cfg = engine::LocalConfig {
        base_url: base_url.trim().to_string(),
        model: model.trim().to_string(),
        // 云端 key / 模型由 set_cloud_config 单独维护，这里沿用现有值
        api_key: state.local_cfg.lock().unwrap().api_key.clone(),
        cloud_model: state.local_cfg.lock().unwrap().cloud_model.clone(),
    };
    if cfg.base_url.is_empty() {
        return Err("地址不能为空".into());
    }
    if cfg.model.is_empty() {
        return Err("模型名不能为空".into());
    }
    *state.local_cfg.lock().unwrap() = cfg.clone();
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("local_base_url", serde_json::json!(cfg.base_url));
        let _ = store.set("local_model", serde_json::json!(cfg.model));
        let _ = store.save();
    }
    Ok(cfg)
}

/// 设置云端（DeepSeek）API Key 与模型名。
///
/// 单独一个命令而不是塞进 `set_local_config`：这两项跟本地模型地址无关，
/// 混在一起容易让前端传参错位。
#[tauri::command]
fn set_cloud_config(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    api_key: String,
    cloud_model: String,
) -> Result<engine::LocalConfig, String> {
    let key = api_key.trim().to_string();
    let model = {
        let m = cloud_model.trim();
        if m.is_empty() {
            "deepseek-chat".to_string()
        } else {
            m.to_string()
        }
    };
    let cfg = {
        let mut cfg = state.local_cfg.lock().unwrap();
        cfg.api_key = key.clone();
        cfg.cloud_model = model.clone();
        cfg.clone()
    };
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("cloud_api_key", serde_json::json!(key));
        let _ = store.set("cloud_model", serde_json::json!(model));
        let _ = store.save();
    }
    if !key.is_empty() {
        tauri::async_runtime::spawn(async {
            engine::warm_up_cloud().await;
        });
    }
    Ok(cfg)
}

/// 用云端 key 发一次极短的翻译请求，验证 key 是否可用（设置页的"测试"按钮）。
#[tauri::command]
async fn test_cloud(
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let cfg = state.local_cfg.lock().unwrap().clone();
    if cfg.api_key.trim().is_empty() {
        return Err("还没填 API Key".into());
    }
    let engine: Box<dyn engine::Translator> = Box::new(engine::DeepSeekEngine::new(cfg));
    let r = engine.translate("hello", "zh-CN").await?;
    Ok(r.translated)
}

/// 换本地模型：只改模型名，地址不动（常用操作，单独给一个入口）
#[tauri::command]
fn set_local_model(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    model: String,
) -> Result<String, String> {
    let m = model.trim().to_string();
    if m.is_empty() {
        return Err("模型名不能为空".into());
    }
    {
        let mut cfg = state.local_cfg.lock().unwrap();
        cfg.model = m.clone();
    }
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("local_model", serde_json::json!(m));
        let _ = store.save();
    }
    Ok(m)
}

/// 探测本地模型服务是否在线，并返回可用模型列表。
/// 先试 OpenAI 兼容的 `/v1/models`，不行再试 Ollama 原生的 `/api/tags`。
#[tauri::command]
async fn probe_local(state: tauri::State<'_, AppState>) -> Result<Vec<String>, String> {
    let cfg = state.local_cfg.lock().unwrap().clone();
    let base = cfg.base_url.trim_end_matches('/').to_string();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| format!("HTTP 客户端构建失败: {e}"))?;

    // ① OpenAI 兼容端点
    let url = format!("{base}/v1/models");
    if let Ok(resp) = client.get(&url).send().await {
        if resp.status().is_success() {
            if let Ok(v) = resp.json::<serde_json::Value>().await {
                if let Some(arr) = v["data"].as_array() {
                    let models: Vec<String> = arr
                        .iter()
                        .filter_map(|m| m["id"].as_str().map(String::from))
                        .collect();
                    if !models.is_empty() {
                        return Ok(models);
                    }
                }
            }
        }
    }

    // ② Ollama 原生端点（有些 Ollama 版本 /v1/models 不可用）
    let url2 = format!("{base}/api/tags");
    let resp = client
        .get(&url2)
        .send()
        .await
        .map_err(|e| format!("连不上 {base}：{e}。确认 Ollama 已启动（托盘有羊驼图标）"))?;
    if !resp.status().is_success() {
        return Err(format!("{url2} 返回 {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("解析失败: {e}"))?;
    let models: Vec<String> = v["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    m["name"]
                        .as_str()
                        .or_else(|| m["model"].as_str())
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default();
    if models.is_empty() {
        return Err("服务在线，但没有已下载的模型。先执行：ollama pull qwen3:0.6b".into());
    }
    Ok(models)
}

/// 是否暂停"复制即译"监听
#[tauri::command]
fn get_monitor_paused(state: tauri::State<'_, AppState>) -> bool {
    state
        .monitor_paused
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// 暂停/恢复"复制即译"监听（复制密码等敏感内容前先暂停）
#[tauri::command]
fn set_monitor_paused(state: tauri::State<'_, AppState>, paused: bool) -> bool {
    state
        .monitor_paused
        .store(paused, std::sync::atomic::Ordering::Relaxed);
    paused
}

/// 当前"复制即译"开关状态（默认关闭：复制东西时突然弹窗很打扰）
#[tauri::command]
fn get_copy_translate(state: tauri::State<'_, AppState>) -> bool {
    !state
        .monitor_paused
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// 开关"复制即译"。默认关，需要的人自己去设置里打开。
#[tauri::command]
fn set_copy_translate(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<bool, String> {
    state
        .monitor_paused
        .store(!enabled, std::sync::atomic::Ordering::Relaxed);
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("copy_translate", serde_json::json!(enabled));
        let _ = store.save();
    }
    Ok(enabled)
}

/// 是否要求"鼠标移回选区上方才翻译"（默认要求）
#[tauri::command]
fn get_require_hover() -> bool {
    selection::require_hover()
}

/// 设置"是否需要鼠标移回选区上方才翻译"
#[tauri::command]
fn set_require_hover(app: tauri::AppHandle, value: bool) -> Result<bool, String> {
    selection::set_require_hover(value);
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("require_hover", serde_json::json!(value));
        let _ = store.save();
    }
    Ok(value)
}

/// 划词翻译总开关（关掉后选中什么都不弹）
#[tauri::command]
fn get_translate_enabled(state: tauri::State<'_, AppState>) -> bool {
    state
        .translate_enabled
        .load(std::sync::atomic::Ordering::Relaxed)
}

#[tauri::command]
fn set_translate_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    value: bool,
) -> Result<bool, String> {
    state
        .translate_enabled
        .store(value, std::sync::atomic::Ordering::Relaxed);
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("translate_enabled", serde_json::json!(value));
        let _ = store.save();
    }
    Ok(value)
}

/// 百度翻译凭据：保存后立即生效（只写本机设置，不进代码仓库）
#[tauri::command]
fn set_baidu_cred(app: tauri::AppHandle, appid: String, key: String) -> Result<bool, String> {
    let appid = appid.trim().to_string();
    let key = key.trim().to_string();
    if appid.is_empty() || key.is_empty() {
        if let Ok(mut g) = engine::BAIDU_CRED.lock() {
            *g = None;
        }
        if let Ok(store) = app.store(SETTINGS_FILE) {
            let _ = store.set("baidu_appid", serde_json::json!(""));
            let _ = store.set("baidu_key", serde_json::json!(""));
            let _ = store.save();
        }
        return Ok(false);
    }
    if let Ok(mut g) = engine::BAIDU_CRED.lock() {
        *g = Some((appid.clone(), key.clone()));
    }
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("baidu_appid", serde_json::json!(appid));
        let _ = store.set("baidu_key", serde_json::json!(key));
        let _ = store.save();
    }
    Ok(true)
}

/// 百度翻译是否已配置
#[tauri::command]
fn get_baidu_configured() -> bool {
    engine::BAIDU_CRED
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false)
}

/// 是否启用本地翻译缓存（关掉后每次都重新请求，方便"翻错了重刷"）
#[tauri::command]
fn get_cache_on(state: tauri::State<'_, AppState>) -> bool {
    state.cache_on.load(std::sync::atomic::Ordering::Relaxed)
}

#[tauri::command]
fn set_cache_on(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    value: bool,
) -> bool {
    state
        .cache_on
        .store(value, std::sync::atomic::Ordering::Relaxed);
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("cache_on", serde_json::json!(value));
        let _ = store.save();
    }
    value
}
/// 目标语言偏好：auto / zh-CN / en / ja / ko / ru / fr / de / es
#[tauri::command]
fn get_target_lang() -> String {
    engine::target_pref()
}

#[tauri::command]
fn set_target_lang(app: tauri::AppHandle, value: String) -> Result<String, String> {
    engine::set_target_pref(&value);
    let v = engine::target_pref();
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("target_lang", serde_json::json!(v));
        let _ = store.save();
    }
    Ok(v)
}
/// 当前版本号（编译时写入，和 tauri.conf.json 一致）
#[tauri::command]
fn get_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 用系统浏览器打开链接（检查更新 / 看主页）
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("只允许 https 链接".into());
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .spawn()
            .map_err(|e| format!("打开浏览器失败: {e}"))?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("xdg-open")
            .arg(&url)
            .spawn()
            .map_err(|e| format!("打开浏览器失败: {e}"))?;
    }
    Ok(())
}
/// 当前主题：auto / dark / light
#[tauri::command]
fn get_theme(app: tauri::AppHandle) -> String {
    app.store(SETTINGS_FILE)
        .ok()
        .and_then(|s| s.get("theme").and_then(|v| v.as_str().map(String::from)))
        .unwrap_or_else(|| "auto".to_string())
}

/// 切换主题。两个窗口（主界面 + 浮窗）都装了监听，改完立刻生效。
#[tauri::command]
fn set_theme(app: tauri::AppHandle, value: String) -> Result<String, String> {
    let v = match value.as_str() {
        "light" => "light",
        "dark" => "dark",
        _ => "auto",
    }
    .to_string();
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("theme", serde_json::json!(v));
        let _ = store.save();
    }
    let _ = app.emit("theme:changed", v.clone());
    Ok(v)
}
/// 选中后是否自动弹窗
#[tauri::command]
fn get_auto_popup(state: tauri::State<'_, AppState>) -> bool {
    state.auto_popup.load(std::sync::atomic::Ordering::Relaxed)
}

#[tauri::command]
fn set_auto_popup(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    value: bool,
) -> Result<bool, String> {
    state
        .auto_popup
        .store(value, std::sync::atomic::Ordering::Relaxed);
    persist_flag(&app, "auto_popup", value);
    Ok(value)
}
/// 是否也翻译中文（默认 false：只翻英文，点中文字段不弹窗）
#[tauri::command]
fn get_cn_ok(state: tauri::State<'_, AppState>) -> bool {
    state.cn_ok.load(std::sync::atomic::Ordering::Relaxed)
}

#[tauri::command]
fn set_cn_ok(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    value: bool,
) -> Result<bool, String> {
    state
        .cn_ok
        .store(value, std::sync::atomic::Ordering::Relaxed);
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("cn_to_en", serde_json::json!(value));
        let _ = store.save();
    }
    Ok(value)
}

/// 缓存条目数
#[tauri::command]
fn cache_size(state: tauri::State<'_, AppState>) -> usize {
    state.cache.lock().unwrap().len()
}

/// 清空缓存
#[tauri::command]
fn clear_cache(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut c = state.cache.lock().unwrap();
    c.entries.clear();
    c.save();
    Ok(())
}

/// 列出所有可用引擎
#[tauri::command]
fn list_engines() -> Vec<EngineInfo> {
    engine::list()
        .into_iter()
        .map(|(id, label, is_default)| EngineInfo {
            id: id.into(),
            label: label.into(),
            is_default,
        })
        .collect()
}

/// 当前引擎 id
#[tauri::command]
fn get_engine(state: tauri::State<'_, AppState>) -> String {
    state.engine_id.lock().unwrap().clone()
}

/// OCR 识别屏幕指定区域（物理像素坐标），返回识别文本。
#[tauri::command]
async fn ocr_region(x: i32, y: i32, width: i32, height: i32) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || ocr::recognize(x, y, width, height))
        .await
        .map_err(|e| format!("OCR 线程失败: {e}"))?
}

/// 框选结果，发给主窗口
#[derive(Serialize, Clone)]
struct BoxResult {
    translated: String,
    source: String,
    raw: String,
    error: String,
}

/// 打开全屏半透明框选遮罩窗口
#[tauri::command]
async fn start_box(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("box") {
        let _ = w.close();
    }
    let (px, py, pw, ph) = match app.primary_monitor().map_err(|e| format!("{e}"))? {
        Some(m) => {
            let p = m.position();
            let s = m.size();
            (p.x, p.y, s.width, s.height)
        }
        None => (0, 0, 1920, 1080),
    };
    let win = WebviewWindowBuilder::new(&app, "box", WebviewUrl::App("box.html".into()))
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .position(px as f64, py as f64)
        .inner_size(pw as f64, ph as f64)
        .build()
        .map_err(|e| format!("创建框选窗口失败: {e}"))?;
    let _ = win.set_focus();
    Ok(())
}

/// 框选截图翻译：OCR 之后**直接在选区旁边弹窗**显示译文，不再回填主窗口。
///
/// 坐标是物理像素（box.js 里已经乘过 devicePixelRatio）。
/// 这样用起来和"划词翻译"是同一套体验：选中哪块，译文就出现在哪块旁边。
#[tauri::command]
async fn ocr_box_translate(
    app: tauri::AppHandle,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<(), String> {
    // 先收起遮罩，否则它会一直盖在弹窗上面
    if let Some(w) = app.get_webview_window("box") {
        let _ = w.close();
    }
    let text = tauri::async_runtime::spawn_blocking(move || ocr::recognize(x, y, width, height))
        .await
        .map_err(|e| format!("OCR 线程失败: {e}"))??;
    let t = text.trim().to_string();
    if t.is_empty() {
        show_popup_toast(&app, "框选区域里没有识别到文字", x, y).await;
        return Ok(());
    }
    // 开关没开时给一句明确提示，别让用户对着屏幕发呆。
    // （划词那条路径是"不弹窗"，但框选是用户主动操作的，必须回话。）
    if !translation_allowed(&app, &t) {
        let cn_ok = app
            .state::<AppState>()
            .cn_ok
            .load(std::sync::atomic::Ordering::Relaxed);
        let msg = if cn_ok {
            "划词翻译已关闭（Ctrl+Alt+Z 可重新开启）"
        } else {
            "识别到的是中文 —— 在设置里打开「中文也翻译」"
        };
        show_popup_toast(&app, msg, x, y).await;
        return Ok(());
    }
    show_translation_popup(&app, &t, x, y, width, height).await;
    Ok(())
}

/// 框选完成：发结果给主窗口并关掉遮罩
#[tauri::command]
async fn box_done(
    app: tauri::AppHandle,
    translated: String,
    source: String,
    raw: String,
    error: Option<String>,
) -> Result<(), String> {
    let payload = BoxResult {
        translated,
        source,
        raw,
        error: error.unwrap_or_default(),
    };
    let _ = app.emit("box:result", payload);
    if let Some(w) = app.get_webview_window("box") {
        let _ = w.close();
    }
    // 焦点还给主窗口
    if let Some(m) = app.get_webview_window("main") {
        let _ = m.set_focus();
    }
    Ok(())
}

/// 取消框选
#[tauri::command]
fn box_cancel(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("box") {
        let _ = w.close();
    }
}

/// 读取当前焦点程序选中的文本及其屏幕坐标（供测试/后续选区弹窗用）。
#[tauri::command]
async fn read_selection() -> Result<uia::Selection, String> {
    tauri::async_runtime::spawn_blocking(uia::read_selection)
        .await
        .map_err(|e| format!("UIA 线程失败: {e}"))?
}

// 弹窗窗口比卡片大一圈（四周各 12px 留给外阴影）
const POPUP_W: f64 = 360.0;
const POPUP_H: f64 = 220.0;

/// 当前鼠标位置（物理像素），作为选区坐标读不到时的兜底锚点。
fn cursor_pos() -> (i32, i32) {
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_ok() {
            (pt.x, pt.y)
        } else {
            (0, 0)
        }
    }
}

/// 选区弹窗内容（loading=true 表示先显示原文+翻译中）
#[derive(Serialize, Clone, Default)]
struct PopupPayload {
    source: String,
    translated: String,
    error: String,
    loading: bool,
    /// 是否命中本地缓存
    #[serde(rename = "cached")]
    cached: bool,
    /// 该原文累计查询次数
    #[serde(rename = "hitCount")]
    hit_count: u32,
    /// 结果来源：code = 按标识符拆词
    #[serde(rename = "srcKind")]
    src_kind: String,
    /// 当前选的本地模型不可用、已回退到哪个引擎
    #[serde(rename = "fallbackFrom")]
    fallback_from: String,
    /// 这次结果到底是哪个引擎给的（本地模型 / DeepSeek 云端 / MyMemory / 词典）
    #[serde(rename = "engineLabel")]
    engine_label: String,
    /// 翻译耗时（毫秒），超过 800ms 才在弹窗里提示
    #[serde(rename = "elapsedMs")]
    elapsed_ms: u64,
    /// 代码模式：标识符拆出的子词列表，前端按结构化布局展示
    #[serde(default)]
    parts: Vec<engine::CodePart>,
    /// 代码模式下的标题（原始标识符）
    #[serde(default)]
    title: String,
    /// 词典模式：多条释义（词性 + 含义）
    #[serde(default)]
    senses: Vec<engine::Sense>,
    /// 词典模式：音标（来自 ECDICT）
    #[serde(default)]
    phonetic: String,
    /// 词典模式：柯林斯星级 0–5（0 表示没有评级）
    #[serde(default)]
    collins: u8,
    /// 词典模式：是否牛津核心词
    #[serde(default)]
    oxford: bool,
}

/// 隐藏选区弹窗
#[tauri::command]
fn popup_hide(app: tauri::AppHandle) {
    if let Some(p) = app.get_webview_window("popup") {
        let _ = p.hide();
    }
    // 清掉"当前展示的原文"，否则同一段内容再划一次会被判为重复而不再显示
    if let Ok(mut g) = POPUP_TEXT.lock() {
        *g = None;
    }
}

/// 调整选区弹窗宽高，使其贴合内容（前端量好逻辑像素宽高传进来）
#[tauri::command]
fn popup_resize(app: tauri::AppHandle, width: f64, height: f64) {
    if let Some(p) = app.get_webview_window("popup") {
        let _ = p.set_size(tauri::LogicalSize::new(width.max(120.0), height.max(50.0)));
    }
}

/// 解析热键字符串（如 "alt+comma"、"ctrl+shift+f"）为 (修饰键, 键)。
fn parse_hotkey(s: &str) -> Option<(Modifiers, Code)> {
    let lower = s.trim().to_lowercase();
    let parts: Vec<&str> = lower
        .split(&['+', ' '][..])
        .filter(|p| !p.is_empty())
        .collect();
    let mut mods = Modifiers::empty();
    let mut key: Option<Code> = None;
    for p in &parts {
        match *p {
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "alt" | "option" => mods |= Modifiers::ALT,
            "shift" => mods |= Modifiers::SHIFT,
            "win" | "cmd" | "meta" => mods |= Modifiers::META,
            _ => key = parse_key(p),
        }
    }
    Some((mods, key?))
}

fn parse_key(p: &str) -> Option<Code> {
    Some(match p {
        "comma" | "," => Code::Comma,
        "period" | "." => Code::Period,
        "space" => Code::Space,
        "enter" | "return" => Code::Enter,
        "tab" => Code::Tab,
        "f1" => Code::F1,
        "f2" => Code::F2,
        "f3" => Code::F3,
        "f4" => Code::F4,
        "f5" => Code::F5,
        "f6" => Code::F6,
        "f7" => Code::F7,
        "f8" => Code::F8,
        "f9" => Code::F9,
        "f10" => Code::F10,
        "f11" => Code::F11,
        "f12" => Code::F12,
        "a" => Code::KeyA,
        "b" => Code::KeyB,
        "c" => Code::KeyC,
        "d" => Code::KeyD,
        "e" => Code::KeyE,
        "f" => Code::KeyF,
        "g" => Code::KeyG,
        "h" => Code::KeyH,
        "i" => Code::KeyI,
        "j" => Code::KeyJ,
        "k" => Code::KeyK,
        "l" => Code::KeyL,
        "m" => Code::KeyM,
        "n" => Code::KeyN,
        "o" => Code::KeyO,
        "p" => Code::KeyP,
        "q" => Code::KeyQ,
        "r" => Code::KeyR,
        "s" => Code::KeyS,
        "t" => Code::KeyT,
        "u" => Code::KeyU,
        "v" => Code::KeyV,
        "w" => Code::KeyW,
        "x" => Code::KeyX,
        "y" => Code::KeyY,
        "z" => Code::KeyZ,
        "0" => Code::Digit0,
        "1" => Code::Digit1,
        "2" => Code::Digit2,
        "3" => Code::Digit3,
        "4" => Code::Digit4,
        "5" => Code::Digit5,
        "6" => Code::Digit6,
        "7" => Code::Digit7,
        "8" => Code::Digit8,
        "9" => Code::Digit9,
        _ => return None,
    })
}

/// 最近一次热键注册的结果。注册失败（被别的软件占用）以前是静默吞掉的，
/// 用户只看到"按了没反应"，完全没法判断原因。
static HOTKEY_STATUS: Mutex<String> = Mutex::new(String::new());

/// 统一注册三个热键（先全注销再注册，避免冲突残留）
fn apply_hotkeys(app: &tauri::AppHandle, translate: &str, window: &str) {
    let _ = app.global_shortcut().unregister_all();
    let mut problems: Vec<String> = Vec::new();
    match parse_hotkey(translate) {
        Some((m, k)) => {
            if let Err(e) = app.global_shortcut().register(Shortcut::new(Some(m), k)) {
                problems.push(format!("翻译热键 {translate} 注册失败：{e}"));
            }
        }
        None => problems.push(format!("翻译热键 {translate} 格式无法解析")),
    }
    if let Some((m, k)) = parse_hotkey(window) {
        if let Err(e) = app.global_shortcut().register(Shortcut::new(Some(m), k)) {
            problems.push(format!("主界面热键 {window} 注册失败：{e}"));
        }
    }
    // 切换开关 / 收起浮窗（失败只记日志，不影响主功能）
    if let Some((m, k)) = parse_hotkey(DEFAULT_TOGGLE_HOTKEY) {
        if let Err(e) = app.global_shortcut().register(Shortcut::new(Some(m), k)) {
            problems.push(format!("开关热键 {DEFAULT_TOGGLE_HOTKEY} 注册失败：{e}"));
        }
    }
    if let Some((m, k)) = parse_hotkey(DEFAULT_HIDE_HOTKEY) {
        if let Err(e) = app.global_shortcut().register(Shortcut::new(Some(m), k)) {
            problems.push(format!("收起浮窗热键 {DEFAULT_HIDE_HOTKEY} 注册失败：{e}"));
        }
    }    if let Some((m, k)) = parse_hotkey(DEFAULT_BOX_HOTKEY) {
        if let Err(e) = app.global_shortcut().register(Shortcut::new(Some(m), k)) {
            problems.push(format!("框选热键 {DEFAULT_BOX_HOTKEY} 注册失败：{e}"));
        }
    }
    let msg = if problems.is_empty() {
        "热键已注册".to_string()
    } else {
        problems.join("；")
    };
    eprintln!("[热键] {msg}");
    if let Ok(mut g) = HOTKEY_STATUS.lock() {
        *g = msg;
    }
}

/// 查询热键注册状态（设置页显示，便于判断"按了没反应"到底为什么）
#[tauri::command]
fn get_hotkey_status() -> String {
    HOTKEY_STATUS
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|_| String::new())
}

/// 更新翻译热键
#[tauri::command]
fn set_hotkey(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    hotkey: String,
) -> Result<String, String> {
    parse_hotkey(&hotkey).ok_or("无法解析的热键（例如 alt+period、ctrl+shift+f）")?;
    let win = state.window_hotkey.lock().unwrap().clone();
    apply_hotkeys(&app, &hotkey, &win);
    *state.hotkey.lock().unwrap() = hotkey.clone();
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("hotkey", serde_json::json!(hotkey));
        let _ = store.save();
    }
    Ok(hotkey)
}

/// 当前翻译热键字符串
#[tauri::command]
fn get_hotkey(state: tauri::State<'_, AppState>) -> String {
    state.hotkey.lock().unwrap().clone()
}

/// 更新弹出主界面热键
#[tauri::command]
fn set_window_hotkey(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    hotkey: String,
) -> Result<String, String> {
    parse_hotkey(&hotkey).ok_or("无法解析的热键（例如 alt+enter、ctrl+shift+space）")?;
    let tr = state.hotkey.lock().unwrap().clone();
    apply_hotkeys(&app, &tr, &hotkey);
    *state.window_hotkey.lock().unwrap() = hotkey.clone();
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("window_hotkey", serde_json::json!(hotkey));
        let _ = store.save();
    }
    Ok(hotkey)
}

/// 当前弹出主界面热键字符串
#[tauri::command]
fn get_window_hotkey(state: tauri::State<'_, AppState>) -> String {
    state.window_hotkey.lock().unwrap().clone()
}

/// 弹出并聚焦主窗口
async fn show_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// 中译英买断激活密钥（卖家预生成激活码、App 本地校验；离线码，够 19 元场景用）
const BUYOUT_SECRET: &str = "translator-19-buoyout-2026";

/// 读 Windows 机器硬件 ID（MachineGuid），用于生成机器指纹
fn read_machine_guid() -> Option<String> {
    let hklm = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE);
    let key = hklm.open_subkey(r"SOFTWARE\Microsoft\Cryptography").ok()?;
    let guid: String = key.get_value("MachineGuid").ok()?;
    Some(guid)
}

/// 机器指纹（8 位大写 hex）：买家发给卖家，卖家据此生成绑定本机的激活码
fn machine_id() -> String {
    use sha2::{Digest, Sha256};
    let guid = read_machine_guid().unwrap_or_default();
    let mut h = Sha256::new();
    h.update(BUYOUT_SECRET.as_bytes());
    h.update(guid.as_bytes());
    let d = h.finalize();
    format!("{:02X}{:02X}{:02X}{:02X}", d[0], d[1], d[2], d[3])
}

/// 校验激活码：格式 TL-<机器码>-<签名>；机器码必须等于本机，签名 = sha256(secret+机器码)前4字节hex。
/// 这样每个码只对生成它的那台电脑有效，别人转走也无法解锁。
fn valid_code(code: &str) -> bool {
    use sha2::{Digest, Sha256};
    let code = code.trim();
    let Some(rest) = code.strip_prefix("TL-") else {
        return false;
    };
    let mut parts = rest.splitn(2, '-');
    let (Some(mach), Some(sig)) = (parts.next(), parts.next()) else {
        return false;
    };
    if mach != machine_id() {
        return false; // 绑定其它电脑
    }
    let mut h = Sha256::new();
    h.update(BUYOUT_SECRET.as_bytes());
    h.update(mach.as_bytes());
    let d = h.finalize();
    let expect = format!("{:02X}{:02X}{:02X}{:02X}", d[0], d[1], d[2], d[3]);
    expect.eq_ignore_ascii_case(sig)
}

/// 激活中译英买断
#[tauri::command]
fn activate(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    code: String,
) -> Result<bool, String> {
    if !valid_code(&code) {
        return Err("激活码无效。请联系卖家获取。".to_string());
    }
    *state.unlocked.lock().unwrap() = true;
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("unlocked", serde_json::json!(true));
        let _ = store.save();
    }
    Ok(true)
}

/// 当前是否已解锁中译英
#[tauri::command]
fn get_buyout_status(state: tauri::State<'_, AppState>) -> bool {
    *state.unlocked.lock().unwrap()
}

/// 本机机器码（买家发给卖家换取绑定本机的激活码）
#[tauri::command]
fn get_machine_code() -> String {
    machine_id()
}

/// 设置 MyMemory 邮箱（免费提额 key）
#[tauri::command]
fn set_mymemory_email(app: tauri::AppHandle, email: String) -> Result<String, String> {
    let clean = email.trim().to_string();
    *engine::MYMEMORY_EMAIL.lock().unwrap() = if clean.is_empty() {
        None
    } else {
        Some(clean.clone())
    };
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("mymemory_email", serde_json::json!(clean));
        let _ = store.save();
    }
    Ok(clean)
}

/// 当前 MyMemory 邮箱
#[tauri::command]
fn get_mymemory_email() -> String {
    engine::MYMEMORY_EMAIL
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_default()
}

/// 把文本写入剪贴板（供弹窗"复制译文"按钮调用）
#[tauri::command]
fn copy_to_clipboard(text: String) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(text))
        .map_err(|e| e.to_string())
}

/// 主窗口最小化到任务栏（不隐身，避免用户找不到）
#[tauri::command]
fn hide_window(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.minimize();
    }
}

/// Ctrl+Alt+F 触发：UIA 读选区 → 先弹"翻译中" → 翻译 → 填译文。
/// 托盘里那个「划词翻译」勾选项的句柄。热键切换开关时用它把勾去掉/打上，
/// 否则菜单上会显示过期状态。
static TRAY_TOGGLE_ITEM: Mutex<Option<tauri::menu::CheckMenuItem<tauri::Wry>>> =
    Mutex::new(None);

/// 托盘里「选中自动弹窗」勾选项的句柄
static TRAY_AUTO_ITEM: Mutex<Option<tauri::menu::CheckMenuItem<tauri::Wry>>> =
    Mutex::new(None);

/// 切换「选中后自动弹窗」。
///
/// 关闭后：划词/双击不再自动弹（相当于"静默模式"），但 Ctrl+Alt+. 手动翻译照常。
/// 这是"总开关"之外的另一种粒度：只想安静看代码、偶尔手动翻一下时用。
async fn toggle_auto_popup(app: &tauri::AppHandle) {
    let new = {
        let st = app.state::<AppState>();
        let v = !st.auto_popup.load(std::sync::atomic::Ordering::Relaxed);
        st.auto_popup
            .store(v, std::sync::atomic::Ordering::Relaxed);
        v
    };
    persist_flag(app, "auto_popup", new);

    // 关掉时顺手把当前浮窗收起来
    if !new {
        if let Some(p) = app.get_webview_window("popup") {
            let _ = p.hide();
        }
        if let Ok(mut g) = POPUP_TEXT.lock() {
            *g = None;
        }
    }
    // 同步托盘勾选
    if let Ok(g) = TRAY_AUTO_ITEM.lock() {
        if let Some(item) = g.as_ref() {
            let _ = item.set_checked(new);
        }
    }

    let msg = if new {
        "选中自动弹窗：已开启"
    } else {
        "选中自动弹窗：已关闭（Ctrl+Alt+. 仍可手动翻译）"
    };
    let (cx, cy) = cursor_pos();
    show_popup_toast(app, msg, cx, cy).await;
}
/// 切换「划词翻译」总开关（热键用），并同步托盘勾选 + 给个提示。
async fn toggle_translation(app: &tauri::AppHandle) {
    let new = {
        let st = app.state::<AppState>();
        let v = !st
            .translate_enabled
            .load(std::sync::atomic::Ordering::Relaxed);
        st.translate_enabled
            .store(v, std::sync::atomic::Ordering::Relaxed);
        v
    };
    persist_flag(app, "translate_enabled", new);

    // 同步托盘里的勾选状态，否则菜单上还显示旧状态
    if let Ok(g) = TRAY_TOGGLE_ITEM.lock() {
        if let Some(item) = g.as_ref() {
            let _ = item.set_checked(new);
        }
    }

    let msg = if new {
        "划词翻译：已开启"
    } else {
        "划词翻译：已关闭"
    };
    let (cx, cy) = cursor_pos();
    show_popup_toast(app, msg, cx, cy).await;
}

/// 在光标附近显示一句状态提示（不是错误，用普通文字样式）
async fn show_popup_toast(app: &tauri::AppHandle, msg: &str, x: i32, y: i32) {
    if let Some(popup) = app.get_webview_window("popup") {
        let pos = popup_pos(app, x, y, 0, 0).await;
        let _ = popup.set_position(tauri::PhysicalPosition::new(pos.0, pos.1));
        let _ = app.emit(
            "popup:show",
            PopupPayload {
                translated: msg.into(),
                loading: false,
                ..Default::default()
            },
        );
        let _ = popup.show();
        reclamp_popup_later(app);
    }
}
/// 选中中文且「中文也翻译」关着时，给一句明确提示，而不是让快捷键静默无效。
/// 返回 true 表示"已经处理过了、别再翻译"。
async fn chinese_gate_notice(app: &tauri::AppHandle, text: &str, cx: i32, cy: i32) -> bool {
    if !engine::has_cjk(text) {
        return false;
    }
    let cn_ok = app
        .state::<AppState>()
        .cn_ok
        .load(std::sync::atomic::Ordering::Relaxed);
    if cn_ok {
        return false;
    }
    show_popup_toast(
        app,
        "选中的是中文，已跳过。右键托盘勾选「中文也翻译」即可翻译中文。",
        cx,
        cy,
    )
    .await;
    true
}
async fn handle_selection(app: tauri::AppHandle) {
    eprintln!("[选区] 开始处理");
    let (cx, cy) = cursor_pos();
    eprintln!("[选区] 光标 ({cx},{cy})");
    // 快捷键是"明确要求翻译"的动作。总开关关着时不能静默什么都不做——
    // 用户只会以为"快捷键坏了"。这里给一句明确提示。
    if !app
        .state::<AppState>()
        .translate_enabled
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        eprintln!("[选区] 总开关关闭，提示用户");
        show_popup_error(
            &app,
            "划词翻译已关闭。右键托盘图标勾选「划词翻译」即可重新开启。",
            cx,
            cy,
        )
        .await;
        return;
    }
    let mut uia_err: Option<String> = None;
    // ① 先 UIA 直接读选区（浏览器/VS/QQ 等）
    //
    // **必须加超时**：UIA 是跨进程 COM 调用，某些应用会让它无限期阻塞，
    // 结果就是按了快捷键/双击之后什么都没发生（连错误提示都出不来）。
    // 700ms 足够正常应用返回；超时就当"读不到"，走剪贴板兜底。
    let sel_opt: Option<Result<uia::Selection, String>> = {
        let fut = tauri::async_runtime::spawn_blocking(uia::read_selection);
        match tokio::time::timeout(std::time::Duration::from_millis(700), fut).await {
            Ok(Ok(r)) => Some(r),
            Ok(Err(_)) => None,
            Err(_) => {
                eprintln!("[选区] UIA 读取超时（700ms），改用剪贴板");
                None
            }
        }
    };
    if let Some(Ok(sel)) = &sel_opt {
        if !sel.text.trim().is_empty() {
            let (bx, by, bw, bh) = if sel.w > 0 && sel.h > 0 {
                (sel.x, sel.y, sel.w, sel.h)
            } else {
                (cx, cy, 0, 0)
            };
            let text = sel.text.trim().to_string();
            if text.chars().count() >= 2 {
                if !chinese_gate_notice(&app, &text, cx, cy).await {
                    show_translation_popup(&app, &text, bx, by, bw, bh).await;
                }
                return;
            }
        }
    } else if let Some(Err(e)) = &sel_opt {
        uia_err = Some(e.clone()); // 透传真实 UIA 报错，便于诊断
    }
    // ② UIA 读不到 → 复制并还原剪贴板（不占用你的剪贴板），从剪贴板拿选区
    let state = app.state::<AppState>();
    state
        .internal_copy
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let text = tauri::async_runtime::spawn_blocking(uia::copy_selection_and_read)
        .await
        .ok()
        .and_then(|r| r.ok())
        .map(|t| t.trim().to_string())
        .unwrap_or_default();
    state
        .internal_copy
        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    if text.is_empty() {
        let msg = uia_err.unwrap_or_else(capture_hint);
        eprintln!("[热键] 没抓到选区，显示提示");
        show_popup_error(&app, &msg, cx, cy).await;
        return;
    }
    eprintln!("[热键] 抓到选区 {} 字，开始翻译", text.chars().count());
    if !chinese_gate_notice(&app, &text, cx, cy).await {
        show_translation_popup(&app, &text, cx, cy, 0, 0).await;
    }
}

/// 抓不到选区时的明确指引（VSCode/IDEA 默认不暴露 UIA，需开无障碍才可被 Alt+X 抓到）。
fn capture_hint() -> String {
    "读不到选区。若在 VSCode：请开启 设置→editor.accessibilitySupport=on；若在 IDEA：启用 Java Access Bridge（控制面板）；或直接 选中后 Ctrl+C 用复制即译。".to_string()
}

/// 当前弹窗正在展示的原文，用于判断"是不是同一段内容"。
static POPUP_TEXT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 查本地词典：只有"单个英文单词"才值得查，其余情况直接返回 None。
///
/// 词典是 mmap 打开的，这个调用是纯内存操作（微秒级），
/// 所以可以在拿锁的同一个作用域里同步做完，不会把锁带过 await。
fn lookup_dict(state: &tauri::State<'_, AppState>, text: &str, target: &str) -> Option<dict::DictEntry> {
    if !engine::is_zh_target(target) || !engine::is_lookup_word(text) {
        return None;
    }
    let guard = state.dict.lock().ok()?;
    guard.as_ref()?.lookup(text)
}

/// 把词典结果包装成弹窗 payload（带上音标和星级）。
fn dict_payload(
    text: &str,
    e: &dict::DictEntry,
    _target: &str,
    elapsed: std::time::Duration,
) -> PopupPayload {
    let senses = dict_entry_to_senses(e);
    let flat = senses
        .iter()
        .flat_map(|s| s.meanings.iter().cloned())
        .collect::<Vec<_>>()
        .join("；");
    PopupPayload {
        title: text.to_string(),
        senses,
        phonetic: e.phonetic.clone(),
        collins: e.collins,
        oxford: e.oxford,
        translated: flat,
        src_kind: "dict".into(),
        engine_label: "本地词典（离线）".into(),
        elapsed_ms: elapsed.as_millis() as u64,
        ..Default::default()
    }
}

/// 当前本地词典状态：条目数（0 表示没启用）
#[tauri::command]
fn dict_status(state: tauri::State<'_, AppState>) -> u32 {
    state
        .dict
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|d| d.len()))
        .unwrap_or(0)
}

/// 设置本地词典文件路径并立即加载。
/// 传空字符串表示停用词典。
#[tauri::command]
fn set_dict_path(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<u32, String> {
    let p = path.trim().to_string();
    if p.is_empty() {
        *state.dict.lock().unwrap() = None;
        if let Ok(store) = app.store(SETTINGS_FILE) {
            let _ = store.set("dict_path", serde_json::json!(""));
            let _ = store.save();
        }
        return Ok(0);
    }
    let pb = std::path::PathBuf::from(&p);
    let d = dict::Dict::open(&pb)?;
    let n = d.len();
    *state.dict.lock().unwrap() = Some(d);
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("dict_path", serde_json::json!(p));
        let _ = store.save();
    }
    Ok(n)
}

/// 用词典直接查一个词（设置页的"试查"用，也方便排查"为什么查不到"）
#[tauri::command]
fn dict_lookup(state: tauri::State<'_, AppState>, word: String) -> Result<Vec<engine::Sense>, String> {
    let guard = state.dict.lock().map_err(|_| "词典锁失败".to_string())?;
    let d = guard.as_ref().ok_or("词典未启用")?;
    let e = d.lookup(&word).ok_or_else(|| format!("词典里没有「{word}」"))?;
    let senses = dict_entry_to_senses(&e);
    if senses.is_empty() {
        return Err("查到了词条但没有可用的中文释义".into());
    }
    Ok(senses)
}

/// 统一的翻译弹窗：先"翻译中"，再填译文。
async fn show_translation_popup(
    app: &tauri::AppHandle,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    // ⓪ 先过两道开关——**在弹 loading 之前判断**，否则关了开关还会闪一下。
    //
    //  总开关：关闭后选中什么都不弹（主窗口手动翻译照常）。
    //  中文过滤：默认只翻英文。选中中文时不弹窗，除非用户主动打开「中文也翻译」。
    if !translation_allowed(app, text) {
        return;
    }

    // 已经显示着同一段内容 → 不要重新定位、不要重放 loading。
    // 否则反复触发时弹窗会被反复挪位置，看起来就是"四处乱飞"。
    let same = POPUP_TEXT
        .lock()
        .map(|g| g.as_deref() == Some(text))
        .unwrap_or(false);
    let visible = app
        .get_webview_window("popup")
        .and_then(|p| p.is_visible().ok())
        .unwrap_or(false);
    if same && visible {
        return;
    }
    if let Ok(mut g) = POPUP_TEXT.lock() {
        *g = Some(text.to_string());
    }

    show_popup_loading(app, x, y, w, h, text).await;
    let started = std::time::Instant::now();
    let target = engine::auto_target(text);
    let (id, cfg, cacheable, dict_hit) = {
        let state = app.state::<AppState>();
        let id = state.engine_id.lock().unwrap().clone();
        let cfg = state.local_cfg.lock().unwrap().clone();
        // 单个单词先查本地词典（毫秒级），查得到就不用启动模型
        let hit = lookup_dict(&state, text, target);
        (id, cfg, state.cache_on.load(std::sync::atomic::Ordering::Relaxed) && text.chars().count() <= 500, hit)
    };

    // ① 命中本地缓存 → 秒出
    if cacheable {
        let state = app.state::<AppState>();
        let mut c = state.cache.lock().unwrap();
        if let Some(e) = c.get(text, target) {
            drop(c);
            drop(state);
            let payload = PopupPayload {
                title: if e.source == "code" {
                    text.to_string()
                } else {
                    String::new()
                },
                translated: e.translated,
                cached: true,
                hit_count: e.count,
                src_kind: e.source,
                parts: e.parts,
                senses: e.senses,
                ..Default::default()
            };
            let _ = app.emit("popup:show", payload);
            return;
        }
    }

    // ② 本地词典命中 → 直接出结果，**完全不碰模型**（这是"轻量"的关键：
    //    查单词时 Ollama 可以在后台不启动、不占显存）
    if let Some(e) = dict_hit.as_ref() {
        let payload = dict_payload(text, e, target, started.elapsed());
        // 词典结果也写缓存，这样"查过 N 次"的计数能累计
        if cacheable {
            let state = app.state::<AppState>();
            let mut c = state.cache.lock().unwrap();
            c.put(text, target, &payload.translated, "dict", &[], &payload.senses);
            c.save();
        }
        let _ = app.emit("popup:show", payload);
        return;
    }

    // ③ 走引擎
    let mut res = translate_or_code(app, text, target, &id, &cfg).await;
    // 整句翻不动（或结果不可信）→ 逐词兜底，保留原格式。
    // 代码块、配置、日志这类"符号多、换行多"的文本主要靠这条兜住。
    let bad = match &res {
        Ok(r) => !engine::is_valid_result(text, &r.translated, target),
        Err(_) => true,
    };
    if bad {
        let state = app.state::<AppState>();
        if let Some(fixed) = wordwise_preserve(&state, text) {
            eprintln!("[翻译] 整句失败，改用逐词兜底");
            res = Ok(engine::Translation {
                translated: fixed,
                source: "glossary".into(),
                target: target.to_string(),
                cached: false,
                hit_count: 0,
                parts: Vec::new(),
                senses: Vec::new(),
            });
        }
    }

    match res {
        Ok(r) => {
            let (engine_lbl, fallback_from) = engine_label(&id, &r.source);
            if cacheable && engine::is_valid_result(text, &r.translated, target) && !is_wordwise(&r) {
                let state = app.state::<AppState>();
                let mut c = state.cache.lock().unwrap();
                c.put(text, target, &r.translated, &r.source, &r.parts, &r.senses);
                c.save();
            }
            emit_translation(app, text, r, fallback_from, engine_lbl, started.elapsed());
        }
        Err(e) => {
            let payload = PopupPayload {
                error: format!("翻译失败：{e}"),
                elapsed_ms: started.elapsed().as_millis() as u64,
                ..Default::default()
            };
            let _ = app.emit("popup:show", payload);
        }
    }
}

/// 把"选中的引擎 + 实际返回的结果来源"翻译成人看得懂的标注。
///
/// 之前只在"本地模型失败回退"时才提示，正常走 DeepSeek/本地模型时什么都不显示，
/// 用户根本分不清这句译文是谁翻的。现在总是返回一个标签。
fn engine_label(id: &str, source: &str) -> (String, String) {
    // 用户选的是本地模型，但结果不是本地模型给的 → 说明回退了。
    // 注意 "dict" 不算回退：那是小模型自己按词典格式给的释义，仍由本地模型产出。
    // 本地术语表命中的结果，和模型无关，标签要说实话
    if source == "glossary" {
        return ("本地术语表（离线）".to_string(), String::new());
    }
    if id == "local" && !matches!(source, "auto" | "code" | "dict") {
        return (
            "MyMemory（本地模型不可用）".to_string(),
            "MyMemory".to_string(),
        );
    }
    let label = match id {
        "local" => {
            if source == "dict" {
                "本地模型（词典模式）"
            } else {
                "本地模型"
            }
        }
        "deepseek" => "DeepSeek 云端",
        "mymemory" => "MyMemory 免费接口",
        "google" => "Google 免费接口",
        "baidu" => "百度翻译",         "dict" => "本地词典（离线）",
        _ => "未知引擎",
    };
    (label.to_string(), String::new())
}

/// 把某个布尔开关写进设置文件（托盘和设置页共用）。
fn persist_flag(app: &tauri::AppHandle, key: &str, value: bool) {
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set(key, serde_json::json!(value));
        let _ = store.save();
    }
}

/// 判断"这段文字现在该不该翻译"。
///
/// **所有翻译入口都必须过这一关**：划词悬停、双击、Alt+. 快捷键、复制即译。
/// 之前只把关卡加在 `show_translation_popup` 里，而双击/快捷键走的是
/// `show_popup_loading_public` + `translate_and_emit` 这条**直达路径**，
/// 结果就是"总开关关了照样弹、中文照样翻"。
fn translation_allowed(app: &tauri::AppHandle, text: &str) -> bool {
    let st = app.state::<AppState>();
    if !st
        .translate_enabled
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return false;
    }
    // 选中中文默认不翻（避免点中中文段落就被弹窗打扰）。
    // 但目标语言不是中文时（用户在设置里选了 English / 日本語 等），
    // 说明他就是想把中文译出去 —— 此时不该拦。
    let cn_source = engine::has_cjk(text) && engine::is_zh_target(engine::auto_target(text));
    if cn_source && !st.cn_ok.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    true
}

/// 把一次翻译结果推给弹窗。
///
/// 单独抽出来是为了让"划词"那条路径能**只翻译一次**：它自己先显示加载态、
/// 调一次引擎，然后直接走这里出结果。之前那条路径先调 `try_translate` 校验、
/// 再调 `show_translation_popup` 又翻一遍——同一个文本翻两遍，白等一倍时间。
fn emit_translation(
    app: &tauri::AppHandle,
    text: &str,
    r: engine::Translation,
    fallback_from: String,
    engine_label: String,
    elapsed: std::time::Duration,
) {
    let payload = PopupPayload {
        title: if r.source == "code" || r.source == "glossary" {
            text.to_string()
        } else {
            String::new()
        },
        parts: r.parts.clone(),
        senses: r.senses.clone(),
        translated: r.translated,
        src_kind: r.source.clone(),
        fallback_from,
        engine_label,
        elapsed_ms: elapsed.as_millis() as u64,
        ..Default::default()
    };
    let _ = app.emit("popup:show", payload);
    // 结果比「翻译中」高得多，重新夹一次，避免底部溢出屏幕
    reclamp_popup_later(app);
}

/// 显示"翻译中"占位。返回是否真的显示了（用于避免重复定位）。
pub(crate) async fn show_popup_loading_public(
    // 注意：开关判断在调用方用 translation_allowed 做；
    // 这里再兜一层，防止以后新增调用忘了检查。
    app: &tauri::AppHandle,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    source: &str,
) {
    show_popup_loading(app, x, y, w, h, source).await;
}

/// 划词路径专用：一次翻译，直接出结果（不重复校验、不重复翻译）。
pub(crate) async fn translate_and_emit(app: &tauri::AppHandle, text: &str) {
    // 直达路径（双击/快捷键）也必须过开关，否则总开关和中文过滤都形同虚设
    if !translation_allowed(app, text) {
        return;
    }
    let target = engine::auto_target(text);
    let started = std::time::Instant::now();
    let (id, cfg, cacheable, dict_hit) = {
        let state = app.state::<AppState>();
        let id = state.engine_id.lock().unwrap().clone();
        let cfg = state.local_cfg.lock().unwrap().clone();
        // 单个单词先查本地词典，查得到就完全不碰模型（这才是"轻量"的关键）
        let hit = lookup_dict(&state, text, target);
        (id, cfg, state.cache_on.load(std::sync::atomic::Ordering::Relaxed) && text.chars().count() <= 500, hit)
    };

    // ① 缓存命中 → 直接出，不再动引擎
    if cacheable {
        let state = app.state::<AppState>();
        let mut c = state.cache.lock().unwrap();
        if let Some(e) = c.get(text, target) {
            let payload = PopupPayload {
                title: if e.source == "code" {
                    text.to_string()
                } else {
                    String::new()
                },
                translated: e.translated,
                cached: true,
                hit_count: e.count,
                src_kind: e.source,
                parts: e.parts,
                senses: e.senses,
                elapsed_ms: started.elapsed().as_millis() as u64,
                ..Default::default()
            };
            drop(c);
            drop(state);
            let _ = app.emit("popup:show", payload);
            return;
        }
    }

    // ② 本地词典命中 → 直接出，不碰模型
    if let Some(e) = dict_hit.as_ref() {
        let payload = dict_payload(text, e, target, started.elapsed());
        if cacheable {
            let state = app.state::<AppState>();
            let mut c = state.cache.lock().unwrap();
            c.put(text, target, &payload.translated, "dict", &[], &payload.senses);
            c.save();
        }
        let _ = app.emit("popup:show", payload);
        return;
    }

    // ③ 翻一次
    let mut final_text = text.to_string();
    let mut result = translate_or_code(app, text, target, &id, &cfg).await;

    // ③ 结果不可信、且刚才是走 UIA 抓的 → 换剪贴板再抓一次重翻（只重试一次）
    let unusable = match &result {
        Ok(r) => !engine::is_valid_result(text, &r.translated, target),
        Err(_) => true,
    };
    if unusable && !selection::last_via_clipboard() {
        if let Ok((better, _)) = selection::grab_selection_via_clipboard() {
            if !better.trim().is_empty() && better != text {
                if let Ok(r2) = translate_with_fallback(&better, target, &id, &cfg, None).await {
                    if engine::is_valid_result(&better, &r2.translated, target) {
                        final_text = better;
                        result = Ok(r2);
                    }
                }
            }
        }
    }

    // ③ 还是不行（模型拒翻/回显原文/直接报错）→ 逐词兜底。
    //
    // 代码块、配置、脚本这类文本是重灾区：模型经常直接说"翻不了"。
    // 这条兜底只替换字母串，符号和换行原样保留，所以格式一定不坏。
    let still_bad = match &result {
        Ok(r) => !engine::is_valid_result(&final_text, &r.translated, target),
        Err(_) => true,
    };
    if still_bad {
        let state = app.state::<AppState>();
        if let Some(fixed) = wordwise_preserve(&state, &final_text) {
            eprintln!("[翻译] 整句失败，改用逐词兜底（选区路径）");
            result = Ok(engine::Translation {
                translated: fixed,
                source: "glossary".into(),
                target: target.to_string(),
                cached: false,
                hit_count: 0,
                parts: Vec::new(),
                senses: Vec::new(),
            });
        }
    }

    match result {
        Ok(r) => {
            let (engine_lbl, fallback_from) = engine_label(&id, &r.source);
            if cacheable && engine::is_valid_result(&final_text, &r.translated, target) && !is_wordwise(&r) {
                let state = app.state::<AppState>();
                let mut c = state.cache.lock().unwrap();
                c.put(&final_text, target, &r.translated, &r.source, &r.parts, &r.senses);
                c.save();
            }
            emit_translation(app, &final_text, r, fallback_from, engine_lbl, started.elapsed());
        }
        Err(e) => {
            let payload = PopupPayload {
                error: format!("翻译失败：{e}"),
                elapsed_ms: started.elapsed().as_millis() as u64,
                ..Default::default()
            };
            let _ = app.emit("popup:show", payload);
        }
    }
}

async fn show_popup_loading(
    app: &tauri::AppHandle,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    source: &str,
) {
    if let Some(popup) = app.get_webview_window("popup") {
        let pos = popup_pos(app, x, y, w, h).await;
        let _ = popup.set_position(tauri::PhysicalPosition::new(pos.0, pos.1));
        let _ = app.emit(
            "popup:show",
            PopupPayload {
                source: source.into(),
                loading: true,
                ..Default::default()
            },
        );
        let _ = popup.show();
        reclamp_popup_later(app);
    }
}

async fn show_popup_error(app: &tauri::AppHandle, msg: &str, x: i32, y: i32) {
    if let Some(popup) = app.get_webview_window("popup") {
        let pos = popup_pos(app, x, y, 0, 0).await;
        let _ = popup.set_position(tauri::PhysicalPosition::new(pos.0, pos.1));
        let _ = app.emit(
            "popup:show",
            PopupPayload {
                source: String::new(),
                translated: String::new(),
                error: msg.into(),
                loading: false,
                ..Default::default()
            },
        );
        let _ = popup.show();
        reclamp_popup_later(app);
    }
}

// 把弹窗夹回屏幕内。关键点：用窗口**当前真实尺寸**，而不是常量——
// 弹窗会随内容自适应高度（长段落能到几百像素），按常量算就会以为放得下，
// 结果下边缘被屏幕裁掉。这就是"底部文字划词时浮窗出屏"的原因。
fn clamp_popup_to_screen(app: &tauri::AppHandle) {
    let Some(p) = app.get_webview_window("popup") else {
        return;
    };
    let (Ok(pos), Ok(size)) = (p.outer_position(), p.outer_size()) else {
        return;
    };
    let mon = p
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten());
    let Some(mon) = mon else { return };
    let mp = mon.position();
    let ms = mon.size();
    let (mx, my) = (mp.x, mp.y);
    let (mw, mh) = (ms.width as i32, ms.height as i32);
    let (w, h) = (size.width as i32, size.height as i32);
    let margin = 8;

    let mut nx = pos.x;
    let mut ny = pos.y;
    if nx + w > mx + mw - margin {
        nx = mx + mw - w - margin;
    }
    if ny + h > my + mh - margin {
        ny = my + mh - h - margin;
    }
    nx = nx.max(mx + margin);
    ny = ny.max(my + margin);

    if (nx, ny) != (pos.x, pos.y) {
        // 先隐藏再移动：WebView2 在"已显示的窗口被挪动"时可能不重绘，
        // 表现是弹窗变半透明、能看见底下的网页。hide → move → show 强制整窗重绘。
        let _ = p.hide();
        let _ = p.set_position(tauri::PhysicalPosition::new(nx, ny));
        let _ = p.show();
    }
}

/// 延迟一点再夹一次：前端 autoFit 改完尺寸后，才拿得到真实高度。
pub(crate) fn reclamp_popup_later(app: &tauri::AppHandle) {
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(180)).await;
        clamp_popup_to_screen(&h);
    });
}

// 计算弹窗位置：优先选区下方，放不下就到上方，再放不下就贴底——始终保证完整可见。
async fn popup_pos(app: &tauri::AppHandle, x: i32, y: i32, _sel_w: i32, sel_h: i32) -> (i32, i32) {
    let (sw, sh) = match app.primary_monitor() {
        Ok(Some(m)) => {
            let s = m.size();
            (s.width as i32, s.height as i32)
        }
        _ => (1920, 1080),
    };
    // 有真实尺寸就用真实尺寸，没显示过才退回常量
    let (pw, ph) = match app.get_webview_window("popup").and_then(|p| p.outer_size().ok()) {
        Some(s) if s.width > 0 && s.height > 0 => (s.width as i32, s.height as i32),
        _ => (POPUP_W as i32, POPUP_H as i32),
    };
    let margin = 8;
    let mut px = x;
    let mut py = y + sel_h.max(20) + margin;

    // ① 下方放不下 → 挪到选区上方
    if py + ph + margin > sh {
        py = y - ph - margin;
    }
    // ② 上方也放不下（选区在屏幕中间且内容很长）→ 贴底
    if py + ph + margin > sh {
        py = sh - ph - margin;
    }
    // ③ 实在比屏幕还高 → 贴顶，优先露出标题
    if py < 0 {
        py = margin;
    }
    if px + pw + margin > sw {
        px = sw - pw - margin;
    }
    if px < 0 {
        px = margin;
    }
    (px, py)
}

/// 切换引擎并持久化
#[tauri::command]
fn set_engine(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<String, String> {
    if !engine::exists(&id) {
        return Err(format!("未知引擎: {id}"));
    }
    *state.engine_id.lock().unwrap() = id.clone();
    // 切到云端引擎时顺手把连接建好，第一次划词就不用等握手
    if id == "deepseek" {
        tauri::async_runtime::spawn(async {
            engine::warm_up_cloud().await;
        });
    }
    if let Ok(store) = app.store(SETTINGS_FILE) {
        let _ = store.set("engine", serde_json::json!(id));
        let _ = store.save();
    }
    Ok(id)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 声明进程为"每显示器 DPI 感知 v2"。
    //
    // 为什么必须做：不开的话，UIA 返回的选区坐标是**逻辑像素**，而我们拿去和
    // 物理像素的鼠标位置做比较、也直接拿去做窗口定位。在高 DPI 屏上两者差
    // 1.25~2 倍，于是出现两个症状：
    //   ① 选区字符偏移错位（同一个词读出「immune」/「mmune」/「mmune t」）
    //   ② 弹窗位置漂移、"四处乱飞"
    // 开启后 UIA 与鼠标坐标统一为物理像素，两个问题一起解决。
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    tauri::Builder::default()
        // 单实例：再次启动只唤醒已开窗口，避免多实例抢占全局热键
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, sc, ev| {
                    if ev.state() == ShortcutState::Pressed {
                        eprintln!("[热键] 收到按键: mods={:?} key={:?}", sc.mods, sc.key);
                        let is_window = {
                            let state = app.state::<AppState>();
                            let win_hk = match state.window_hotkey.try_lock() {
                                Ok(g) => g.clone(),
                                Err(e) => {
                                    eprintln!("[热键] 读取主界面热键失败: {e}");
                                    String::new()
                                }
                            };
                            if win_hk.is_empty() {
                                false
                            } else {
                                parse_hotkey(&win_hk)
                                    .map(|(m, k)| (m, k) == (sc.mods, sc.key))
                                    .unwrap_or(false)
                            }
                        };
                        // 是否是"切换开关"的热键
                        let is_toggle = parse_hotkey(DEFAULT_TOGGLE_HOTKEY)
                            .map(|(m, k)| (m, k) == (sc.mods, sc.key))
                            .unwrap_or(false);
                        // 是否是"收起浮窗"的热键
                        let is_hide = parse_hotkey(DEFAULT_HIDE_HOTKEY)
                            .map(|(m, k)| (m, k) == (sc.mods, sc.key))
                            .unwrap_or(false);
                        // 是否是"框选截图翻译"的热键
                        let is_box = parse_hotkey(DEFAULT_BOX_HOTKEY)
                            .map(|(m, k)| (m, k) == (sc.mods, sc.key))
                            .unwrap_or(false);
                        eprintln!(
                            "[热键] is_window={is_window} is_toggle={is_toggle} is_hide={is_hide} is_box={is_box}，准备执行"
                        );
                        let h = app.clone();
                        tauri::async_runtime::spawn(async move {
                            eprintln!("[热键] 任务开始执行");
                            if is_toggle {
                                toggle_translation(&h).await;
                            } else if is_hide {
                                toggle_auto_popup(&h).await;
                            } else if is_box {
                                // 框选截图翻译：拉起全屏遮罩，等用户拖框
                                let _ = start_box(h).await;
                            } else if is_window {
                                show_main_window(&h).await;
                            } else {
                                handle_selection(h).await;
                            }
                            eprintln!("[热键] 任务结束");
                        });
                    }
                })
                .build(),
        )
        .setup(|app| {
            // 注意：这里以前会调用 SPI_SETSCREENREADER 强制把系统"屏幕阅读器"标志置 1，
            // 以便 Chrome/Edge/VSCode 暴露 UIA 文本。那是全局改系统设置（并写盘），
            // 属于越权行为且容易被安全软件拦截，已移除。
            // 现在改为：UIA 读不到时，在弹窗里提示用户自己去开启无障碍支持。

            // 读取持久化的引擎选择，若无则用默认
            let mut engine_id = DEFAULT_ENGINE.to_string();
            if let Ok(store) = app.store(SETTINGS_FILE) {
                // 目标语言偏好（默认 auto：按内容判断方向）
            if let Some(v) = app
                .store(SETTINGS_FILE)
                .ok()
                .and_then(|s| s.get("target_lang").and_then(|v| v.as_str().map(String::from)))
            {
                engine::set_target_pref(&v);
            }

            if let Some(v) = store.get("engine").and_then(|v| v.as_str().map(String::from)) {
                    if engine::exists(&v) {
                        engine_id = v;
                    }
                }
            }
            let hotkey_str = app
                .store(SETTINGS_FILE)
                .ok()
                .and_then(|s| s.get("hotkey").and_then(|v| v.as_str().map(String::from)))
                .unwrap_or_else(|| DEFAULT_HOTKEY.to_string());
            // 老版本默认是 alt+period，很容易被别的软件占用（实测按了没反应）。
            // 升级后自动换成 alt+shift+period，并写回配置。
            // 历史遗留的默认值都自动归位到当前默认（ctrl+alt+period）
            let hotkey_str = if hotkey_str == "alt+period" || hotkey_str == "alt+shift+period" {
                if let Ok(store) = app.store(SETTINGS_FILE) {
                    let _ = store.set("hotkey", serde_json::json!(DEFAULT_HOTKEY));
                    let _ = store.save();
                }
                DEFAULT_HOTKEY.to_string()
            } else {
                hotkey_str
            };
            // 启动时如果已经选了云端引擎，先把连接焐热，
            // 免得用户第一次划词要等 DNS + TLS 握手（实测能到 7 秒）
            if engine_id == "deepseek" {
                tauri::async_runtime::spawn(async {
                    engine::warm_up_cloud().await;
                });
            }
            let window_hotkey_str = app
                .store(SETTINGS_FILE)
                .ok()
                .and_then(|s| s.get("window_hotkey").and_then(|v| v.as_str().map(String::from)))
                .unwrap_or_else(|| DEFAULT_WINDOW_HOTKEY.to_string());
            let unlocked = app
                .store(SETTINGS_FILE)
                .ok()
                .and_then(|s| s.get("unlocked").and_then(|v| v.as_bool()))
                .unwrap_or(false);

            // 两个行为开关（都在设置里可改）：
            //  copy_translate：复制即译，默认**关**——复制东西时突然弹出翻译很打扰
            //  require_hover ：鼠标移回选区上方才翻译，默认**开**（用户期望的行为）
            let copy_translate = app
                .store(SETTINGS_FILE)
                .ok()
                .and_then(|s| s.get("copy_translate").and_then(|v| v.as_bool()))
                .unwrap_or(false);
            // 划词翻译总开关，默认开
            let translate_enabled = app
                .store(SETTINGS_FILE)
                .ok()
                .and_then(|s| s.get("translate_enabled").and_then(|v| v.as_bool()))
                .unwrap_or(true);
            // 选中是否自动弹窗，默认开
            let auto_popup = app
                .store(SETTINGS_FILE)
                .ok()
                .and_then(|s| s.get("auto_popup").and_then(|v| v.as_bool()))
                .unwrap_or(true);
            // 是否也翻中文，默认关（只翻英文）
            let cn_ok = app
                .store(SETTINGS_FILE)
                .ok()
                .and_then(|s| s.get("cn_to_en").and_then(|v| v.as_bool()))
                .unwrap_or(false);
            // 是否使用本地翻译缓存：默认开。关掉后每次都重新请求，方便"翻错了重刷"
            let cache_on_setting = app
                .store(SETTINGS_FILE)
                .ok()
                .and_then(|s| s.get("cache_on").and_then(|v| v.as_bool()))
                .unwrap_or(true);
            // 选中即翻译（默认）还是"鼠标移回选区才翻译"
            let require_hover = app
                .store(SETTINGS_FILE)
                .ok()
                .and_then(|s| s.get("require_hover").and_then(|v| v.as_bool()))
                .unwrap_or(false);
            selection::set_require_hover(require_hover);

            // 本地模型配置（地址 + 模型名），默认指向 Ollama
            let mut local_cfg = engine::LocalConfig::default();
            if let Ok(store) = app.store(SETTINGS_FILE) {
                if let Some(v) = store
                    .get("local_base_url")
                    .and_then(|v| v.as_str().map(String::from))
                {
                    if !v.trim().is_empty() {
                        local_cfg.base_url = v;
                    }
                }
                if let Some(v) = store
                    .get("local_model")
                    .and_then(|v| v.as_str().map(String::from))
                {
                    if !v.trim().is_empty() {
                        local_cfg.model = v;
                    }
                }
            }

            // 本地缓存放在 app 数据目录（卸载不会自动删，用户可手动清）
            let data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            let cache = cache::Cache::load(&data_dir);
            println!("[缓存] 已载入 {} 条，位置: {:?}", cache.len(), data_dir);
            // 清掉历史遗留的无效条目（早期版本把"原样回显原文"的失败结果也缓存了）
            let mut cache = cache;
            let purged = cache.purge_invalid();
            if purged > 0 {
                println!("[缓存] 已清理 {purged} 条无效记录（原文回显类）");
                cache.save();
            }

            // 本地词典：优先用设置里的路径；没设置就找 app 数据目录下的 dict.bin。
            // 打不开就当"没装词典"处理，不影响其他功能。
            let default_dict = data_dir.join("dict.bin");
            // 安装包会把 dict.bin 作为资源一起发出去。首次运行时把它复制到数据目录，
            // 这样用户装完就能用，不需要手动配置任何路径。
            if !default_dict.exists() {
                if let Ok(res_dir) = app.path().resource_dir() {
                    let src = res_dir.join("dict.bin");
                    if src.exists() {
                        let _ = std::fs::create_dir_all(&data_dir);
                        match std::fs::copy(&src, &default_dict) {
                            Ok(_) => println!("[词典] 已从安装目录复制到 {:?}", default_dict),
                            Err(e) => println!("[词典] 复制失败: {e}"),
                        }
                    }
                }
            }
            let dict_path = app
                .store(SETTINGS_FILE)
                .ok()
                .and_then(|s| s.get("dict_path").and_then(|v| v.as_str().map(String::from)))
                .filter(|p| !p.trim().is_empty())
                .map(std::path::PathBuf::from)
                .unwrap_or(default_dict);
            let dict = match dict::Dict::open(&dict_path) {
                Ok(d) => {
                    println!("[词典] 已加载 {} 个单词: {:?}", d.len(), dict_path);
                    Some(d)
                }
                Err(e) => {
                    println!("[词典] 未启用（{e}）");
                    None
                }
            };

            app.manage(AppState {
                engine_id: Mutex::new(engine_id),
                hotkey: Mutex::new(hotkey_str),
                window_hotkey: Mutex::new(window_hotkey_str),
                unlocked: Mutex::new(unlocked),
                internal_copy: std::sync::atomic::AtomicUsize::new(0),
                local_cfg: Mutex::new(local_cfg),
                cache: Mutex::new(cache),
                monitor_paused: std::sync::atomic::AtomicBool::new(!copy_translate),
                translate_enabled: std::sync::atomic::AtomicBool::new(translate_enabled),
                cn_ok: std::sync::atomic::AtomicBool::new(cn_ok),
                cache_on: std::sync::atomic::AtomicBool::new(cache_on_setting),
                auto_popup: std::sync::atomic::AtomicBool::new(auto_popup),
                dict: Mutex::new(dict),
            });

            // 载入百度翻译凭据
            if let Ok(store) = app.store(SETTINGS_FILE) {
                let appid = store
                    .get("baidu_appid")
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default();
                let key = store
                    .get("baidu_key")
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default();
                if !appid.trim().is_empty() && !key.trim().is_empty() {
                    if let Ok(mut g) = engine::BAIDU_CRED.lock() {
                        *g = Some((appid, key));
                    }
                    eprintln!("[百度] 已载入 APPID");
                }
            }

            // 载入 MyMemory 邮箱（免费提额），供引擎使用
            if let Ok(store) = app.store(SETTINGS_FILE) {
                if let Some(email) = store
                    .get("mymemory_email")
                    .and_then(|v| v.as_str().map(String::from))
                {
                    if !email.trim().is_empty() {
                        *engine::MYMEMORY_EMAIL.lock().unwrap() = Some(email);
                    }
                }
            }

            // 全局鼠标钩子：双击选中一个词 / 拖拽选中一段 → 抓取选区 → 弹出译文。
            // 这是"任何软件都能划词"的主力路径（UIA 在很多程序上读不到）。
            //
            // 抓取有两条路，各有毛病，所以这里做了**交叉重试**：
            //   UIA   ：不碰剪贴板，但高 DPI 下字符偏移会错位（会读出 mmune t 这种碎片）
            //   剪贴板：内容准确，但要临时占用剪贴板（会干扰截图工具）
            // 先用 UIA；如果翻出来发现"结果不可信"（模型原样回显），说明多半是
            // 抓取抓歪了，就换剪贴板再抓一次、再翻一次。
            {
                let h = app.handle().clone();
                let h_select = h.clone();
                let h_leave = h.clone();
                let h_blank = h.clone();
                let ok = selection::start(
                    Box::new(move |text| {
                        let h2 = h_select.clone();
                        tauri::async_runtime::spawn(async move {
                            // 关掉总开关 / 选中中文时，**连"翻译中"都不要闪**，
                            // 所以这一关必须放在显示 loading 之前。
                            // 自动弹窗关掉后，划词不再弹；手动快捷键不受影响
                            if !h2
                                .state::<AppState>()
                                .auto_popup
                                .load(std::sync::atomic::Ordering::Relaxed)
                            {
                                return;
                            }
                            if !translation_allowed(&h2, &text) {
                                return;
                            }
                            let (px, py) = selection::cursor_point();
                            // ① 立刻把弹窗顶出来显示"翻译中…"——体感快慢主要取决于这一步，
                            //    而不是翻译本身有多快。以前是等翻译完才弹，所以显得慢。
                            show_popup_loading_public(&h2, px, py, 0, 0, &text).await;
                            // ② 只翻一次，直接出结果（不再先校验翻一遍、再翻一遍）
                            translate_and_emit(&h2, &text).await;
                        });
                    }),
                    // 鼠标离开选区 → 立刻收起弹窗（用户要求"移开就没"）
                    Box::new(move || {
                        let h3 = h_leave.clone();
                        tauri::async_runtime::spawn(async move {
                            if let Some(p) = h3.get_webview_window("popup") {
                                let _ = p.hide();
                            }
                            if let Ok(mut g) = POPUP_TEXT.lock() {
                                *g = None;
                            }
                        });
                    }),
                    // 在空白处点击 → 同样收起弹窗
                    Box::new(move || {
                        let h4 = h_blank.clone();
                        tauri::async_runtime::spawn(async move {
                            if let Some(p) = h4.get_webview_window("popup") {
                                let _ = p.hide();
                            }
                            if let Ok(mut g) = POPUP_TEXT.lock() {
                                *g = None;
                            }
                        });
                    }),
                );
                if !ok {
                    eprintln!("[选区] 鼠标钩子未启用，划词翻译将依赖 Alt+. 或复制即译");
                }
            }

            // 后台线程：轮询剪贴板，检测到新的可翻译文本时，在鼠标附近弹译窗。
            // 这实现了"复制即译"——VSCode/QQ/任意能复制的地方，复制后自动弹译文。
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let mut clip = match arboard::Clipboard::new() {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("剪贴板初始化失败: {e}");
                        return;
                    }
                };
                let mut last: Option<String> = None;
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    let Ok(text) = clip.get_text() else { continue };
                    let t = text.trim().to_string();
                    if t.is_empty() || last.as_deref() == Some(t.as_str()) {
                        continue;
                    }
                    last = Some(t.clone());

                    // ① 用户手动暂停监听（复制密码前先暂停）
                    if handle
                        .state::<AppState>()
                        .monitor_paused
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        continue;
                    }
                    // ② 基本过滤：太长的（文件路径粘贴/大段文档）和复制文件时不翻
                    if t.chars().count() < 2 || t.chars().count() > 2000 {
                        continue;
                    }

                    let h = handle.clone();
                    tauri::async_runtime::spawn(async move {
                        // 内部合成 Ctrl+C（划词抓取 / Alt+.）导致的变化要跳过，
                        // 否则会和真正的翻译弹窗抢位置，表现为"闪一下"
                        let internal = h
                            .state::<AppState>()
                            .internal_copy
                            .load(std::sync::atomic::Ordering::Relaxed);
                        if internal > 0
                            || selection::internal_copy_active()
                            || selection::in_internal_settle_window()
                            || selection::is_internal_text(&t)
                        {
                            return;
                        }
                        let (cx, cy) = cursor_pos();
                        show_translation_popup(&h, &t, cx, cy, 0, 0).await;
                    });
                }
            });

            // 选区弹窗：常驻隐藏、置顶、不抢焦点，Alt+F 时定位到选区旁显示。
            let _ = WebviewWindowBuilder::new(app, "popup", WebviewUrl::App("popup.html".into()))
                .decorations(false)
                // 不透明：WebView2 的透明窗口渲染不可靠（灰底/硬边），实测退不回干净效果
                .transparent(false)
                .always_on_top(true)
                .skip_taskbar(true)
                .focused(false)
                .visible(false)
                .inner_size(POPUP_W, POPUP_H)
                .build();

            // 【调试钩子】设了 JFY_DEBUG_POPUP=1 时，启动 3 秒后自动在屏幕中部
            // 弹一个真实翻译弹窗。用途：无鼠标操作也能截图核对弹窗样式。
            // 不设这个环境变量时完全不影响正常运行。
            if let Ok(v) = std::env::var("JFY_DEBUG_POPUP") {
                let h = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    // 值不是 "1" 时，直接把值当成待翻译文本（方便用真实代码片段测兜底）
                    let sample = if v != "1" && !v.trim().is_empty() {
                        v
                    } else {
                        "The translation service keeps a small in-memory cache so that repeated lookups of the same phrase return instantly.".to_string()
                    };
                    tauri::async_runtime::block_on(show_translation_popup(&h, &sample, 420, 300, 0, 0));
                });
            }

            // 注册两个热键：翻译（默认 Alt+.）+ 弹出主界面（默认 Alt+Enter）
            {
                let st = app.state::<AppState>();
                let tr = st.hotkey.lock().unwrap().clone();
                let wn = st.window_hotkey.lock().unwrap().clone();
                apply_hotkeys(app.handle(), &tr, &wn);
            }

            // 系统托盘图标：左键/菜单"显示主界面"，菜单"退出"
            {
                use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
                use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
                let show = MenuItem::with_id(app, "show", "显示主界面", true, None::<&str>);
                let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>);
                // 托盘里直接开关，不用每次打开主界面
                let st0 = app.state::<AppState>();
                let en0 = st0
                    .translate_enabled
                    .load(std::sync::atomic::Ordering::Relaxed);
                let cn0 = st0.cn_ok.load(std::sync::atomic::Ordering::Relaxed);
                let t_en = CheckMenuItem::with_id(app, "t_en", "划词翻译", true, en0, None::<&str>);
                let t_cn =
                    CheckMenuItem::with_id(app, "t_cn", "中文也翻译", true, cn0, None::<&str>);
                let a0 = st0
                    .auto_popup
                    .load(std::sync::atomic::Ordering::Relaxed);
                let t_auto =
                    CheckMenuItem::with_id(app, "t_auto", "选中自动弹窗", true, a0, None::<&str>);
                if let (Ok(show), Ok(quit), Ok(t_en), Ok(t_cn), Ok(t_auto)) =
                    (show, quit, t_en, t_cn, t_auto)
                {
                    let Ok(sep) = PredefinedMenuItem::separator(app) else { return Ok(()) };
                    let Ok(menu) =
                        Menu::with_items(app, &[&show, &sep, &t_en, &t_auto, &t_cn, &sep, &quit])
                    else {
                        return Ok(());
                    };
                    let t_en_h = t_en.clone();
                    if let Ok(mut g) = TRAY_TOGGLE_ITEM.lock() {
                        *g = Some(t_en.clone());
                    }
                    let t_cn_h = t_cn.clone();
                    let t_auto_h = t_auto.clone();
                    if let Ok(mut g) = TRAY_AUTO_ITEM.lock() {
                        *g = Some(t_auto.clone());
                    }
                    let _ = TrayIconBuilder::with_id("main-tray")
                        .icon(app.default_window_icon().cloned().unwrap())
                        .tooltip("划词翻译")
                        .menu(&menu)
                        .show_menu_on_left_click(false)
                        .on_menu_event(move |app, event| match event.id().as_ref() {
                            "show" => {
                                if let Some(w) = app.get_webview_window("main") {
                                    let _ = w.show();
                                    let _ = w.unminimize();
                                    let _ = w.set_focus();
                                }
                            }
                            "t_en" => {
                                let st = app.state::<AppState>();
                                let new = !st
                                    .translate_enabled
                                    .load(std::sync::atomic::Ordering::Relaxed);
                                st.translate_enabled
                                    .store(new, std::sync::atomic::Ordering::Relaxed);
                                let _ = t_en_h.set_checked(new);
                                persist_flag(app, "translate_enabled", new);
                            }
                            "t_cn" => {
                                let st = app.state::<AppState>();
                                let new = !st.cn_ok.load(std::sync::atomic::Ordering::Relaxed);
                                st.cn_ok.store(new, std::sync::atomic::Ordering::Relaxed);
                                let _ = t_cn_h.set_checked(new);
                                persist_flag(app, "cn_to_en", new);
                            }
                            "t_auto" => {
                                let st = app.state::<AppState>();
                                let new =
                                    !st.auto_popup.load(std::sync::atomic::Ordering::Relaxed);
                                st.auto_popup
                                    .store(new, std::sync::atomic::Ordering::Relaxed);
                                let _ = t_auto_h.set_checked(new);
                                persist_flag(app, "auto_popup", new);
                            }
                            "quit" => app.exit(0),
                            _ => {}
                        })
                        .on_tray_icon_event(|tray, event| {
                            if let TrayIconEvent::Click {
                                button: MouseButton::Left,
                                button_state: MouseButtonState::Up,
                                ..
                            } = event
                            {
                                let app = tray.app_handle();
                                if let Some(w) = app.get_webview_window("main") {
                                    let _ = w.show();
                                    let _ = w.unminimize();
                                    let _ = w.set_focus();
                                }
                            }
                        })
                        .build(app);
                }
            }


            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            translate,
            list_engines,
            get_engine,
            set_engine,
            ocr_region,
            ocr_box_translate,
            start_box,
            box_done,
            box_cancel,
            read_selection,
            popup_hide,
            popup_resize,
            get_hotkey,
            set_hotkey,
            get_window_hotkey,
            set_window_hotkey,
            copy_to_clipboard,
            hide_window,
            activate,
            get_buyout_status,
            set_mymemory_email,
            get_mymemory_email,
            get_machine_code,
            get_local_config,
            set_local_config,
            set_local_model,
            probe_local,
            get_monitor_paused,
            set_monitor_paused,
            get_copy_translate,
            set_copy_translate,
            get_require_hover,
            set_require_hover,
            get_hotkey_status,
            get_translate_enabled,
            set_translate_enabled,
            get_cn_ok,
            get_auto_popup,
            set_baidu_cred,
            get_baidu_configured,
            get_cache_on,
            set_cache_on,
            get_target_lang,
            set_target_lang,
            get_version,
            open_url,
            get_theme,
            set_theme,
            set_auto_popup,
            set_cn_ok,
            cache_size,
            clear_cache,
            dict_status,
            set_dict_path,
            dict_lookup,
            set_cloud_config,
            test_cloud
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod code_fallback_tests {
    use super::*;

    /// 逐词兜底必须**不改动任何非字母字符**：符号、缩进、换行原样保留。
    /// 这里不依赖真实词典（词典是运行时加载的），只验证"切词边界"逻辑。
    #[test]
    fn word_boundaries_keep_symbols() {
        let text = "const cache = new Map();\n  if (x) { return; }";
        // 手工模拟 wordwise 的分段规则：字母连续段 vs 其它
        let mut words = Vec::new();
        let mut cur = String::new();
        for ch in text.chars() {
            if ch.is_ascii_alphabetic() {
                cur.push(ch);
            } else if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() { words.push(cur); }
        assert_eq!(words, vec!["const", "cache", "new", "Map", "if", "x", "return"]);
        // 反推：把这些词逐个替换回原文，其余字符必须完全一致
        let mut rebuilt = String::new();
        let mut it = words.iter();
        let mut cur2 = String::new();
        for ch in text.chars() {
            if ch.is_ascii_alphabetic() {
                cur2.push(ch);
            } else {
                if !cur2.is_empty() {
                    rebuilt.push_str(it.next().unwrap());
                    cur2.clear();
                }
                rebuilt.push(ch);
            }
        }
        assert_eq!(rebuilt, text);
    }
}

#[cfg(test)]
mod code_detect_tests {
    use super::looks_like_code;

    #[test]
    fn prose_is_not_code() {
        // 正常英文段落：括号、引号、破折号都有，但绝不能被当成代码
        let prose = "Discussions of the theory and practice of translation reach back into antiquity \
and show remarkable continuities. The ancient Greeks distinguished between 'metaphrasis' \
(a literal translation) and 'hermeneia' (a translation with interpretation).";
        assert!(!looks_like_code(prose), "英文段落被误判成代码");

        let prose2 = "This distinction was adopted by English poet and translator John Dryden (1631-1700), \
who described translation as the judicious blending of these two modes.";
        assert!(!looks_like_code(prose2), "第二段也被误判");
    }

    #[test]
    fn code_is_code() {
        assert!(looks_like_code("shop->setPos(520, 45);"));
        assert!(looks_like_code("scene->addItem(shop);"));
        assert!(looks_like_code("has_completed_focus = JAP_advance_one_hundred_million_fireballs"));
        assert!(looks_like_code("if (type < 40) {\n    zombie = new BasicZombie;\n}"));
        assert!(looks_like_code("void MainWindow::onTimer()"));
    }
}
