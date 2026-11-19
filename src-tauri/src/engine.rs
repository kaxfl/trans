//! 翻译引擎层。
//!
//! 设计目标：**翻译源可切换、可本地、可离线**。
//!
//! - `LocalEngine`：走 OpenAI 兼容的 `POST {base}/v1/chat/completions`。
//!   Ollama / llama.cpp server / LM Studio / vLLM 都兼容这套接口，
//!   所以在"本地小模型"这件事上，换后端不用改代码，只改设置里的地址和模型名。
//! - `MyMemoryEngine`：免费公共 API，作为没配本地模型时的兜底（不需要 key）。
//! - `GoogleEngine`：逆向免费端点，**仅个人自用**。商用请勿启用（违反其条款）。
//!
//! 新增引擎只需实现 `Translator` trait 并在 `get` / `list` 登记。

use async_trait::async_trait;
use serde::Serialize;

/// MyMemory 注册邮箱（免费提额 key）。由设置写入，请求时带 `de` 参数。
pub static MYMEMORY_EMAIL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 本地引擎配置（OpenAI 兼容端点的地址与模型名）。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LocalConfig {
    /// 形如 `http://127.0.0.1:11434`（Ollama 默认）
    pub base_url: String,
    /// 形如 `qwen3:0.6b`
    pub model: String,
    /// DeepSeek 云端 API Key（`sk-...`）。空表示未配置。
    #[serde(default)]
    pub api_key: String,
    /// 云端模型名，默认 `deepseek-chat`
    #[serde(default = "default_cloud_model")]
    pub cloud_model: String,
}

fn default_cloud_model() -> String {
    "deepseek-chat".to_string()
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:11434".to_string(),
            // 默认用 1.7b：实测 0.6b 对短词的输出很不稳定（会把「思考」翻进译文、
            // 或原样回显），而 1.7b 在 6GB 显存上能全量跑 GPU，又快又准。
            // 用户没下这个模型时会自动回退到 MyMemory，不会白屏。
            model: "qwen3:1.7b".to_string(),
            api_key: String::new(),
            cloud_model: default_cloud_model(),
        }
    }
}

/// 全局复用的 HTTP 客户端。
///
/// 之前每次请求都 `Client::builder().build()`，等于每次都重建连接池和 TLS 会话。
/// 连续翻译时这一项能省几百毫秒——而且客户端本身是可复用的（reqwest 内部线程安全）。
/// 读取 Windows 系统代理（VPN 客户端通常只写注册表，不设环境变量）。
/// reqwest 默认只认环境变量，所以浏览器能打开的网站软件却连不上。
fn system_proxy() -> Option<String> {
    if let Ok(v) = std::env::var("HTTPS_PROXY").or_else(|_| std::env::var("https_proxy")) {
        if !v.trim().is_empty() {
            return Some(v);
        }
    }
    #[cfg(target_os = "windows")]
    {
        let out = std::process::Command::new("reg")
            .args([
                "query",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
                "/v",
                "ProxyServer",
            ])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Some(pos) = line.find("REG_SZ") {
                let v = line[pos + 6..].trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

fn http() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let mut b = reqwest::Client::builder()
            // 30 秒足够：正常 1–2 秒就回来；超时基本是网络/代理问题，
            // 与其让用户对着「翻译中…」干等两分钟，不如早点报错。
            .timeout(std::time::Duration::from_secs(30));
        if let Some(p) = system_proxy() {
            let url = if p.starts_with("http") { p } else { format!("http://{p}") };
            if let Ok(proxy) = reqwest::Proxy::all(&url) {
                b = b.proxy(proxy);
            }
        }
        b.build().expect("HTTP 客户端初始化失败")
    })
}

/// 标识符拆出来的一个子词：原文 + 译文 + 可选的简短说明（词性/注释）。
///
/// 这是"代码模式"的核心数据结构。给英语不好的初级程序员看代码时，
/// 一行通顺的译文其实没用——他们需要的是"这个名字由哪几个词组成、各是什么意思"。
#[derive(Serialize, serde::Deserialize, Clone, Debug, Default)]
pub struct CodePart {
    /// 原始子词，如 `last`、`Update`
    pub raw: String,
    /// 拆分后的中文含义
    pub meaning: String,
    /// 简短说明，如"类型""常量"，通常为空
    #[serde(default)]
    pub note: String,
}

/// 词典释义里的一条：词性 + 若干含义。
#[derive(Serialize, serde::Deserialize, Clone, Debug, Default)]
pub struct Sense {
    /// 词性缩写，如 `n.` `v.` `adj.` `adv.`；可能为空
    pub pos: String,
    /// 该词性下的若干含义（已按 `；` 拆开）
    pub meanings: Vec<String>,
}

/// 翻译结果
#[derive(Serialize, Clone)]
pub struct Translation {
    pub translated: String,
    pub source: String,
    pub target: String,
    /// 是否命中了本地缓存
    #[serde(default)]
    pub cached: bool,
    /// 该原文累计被查过几次（仅缓存命中时有意义）
    #[serde(default)]
    pub hit_count: u32,
    /// 代码模式：标识符被拆成的子词列表（非空时前端按结构化布局展示）
    #[serde(default)]
    pub parts: Vec<CodePart>,
    /// 词典模式：单词的多条释义（非空时前端按"词性 + 含义"排版）
    #[serde(default)]
    pub senses: Vec<Sense>,
}

#[async_trait]
pub trait Translator: Send + Sync {
    async fn translate(&self, text: &str, target_lang: &str) -> Result<Translation, String>;
}

// ---------------- 本地模型（OpenAI 兼容端点） ----------------

pub struct LocalEngine {
    pub cfg: LocalConfig,
}

/// 给本地模型的系统提示词：只输出译文，代码标识符原样保留。
/// 小模型很容易"开始解说"，所以约束要硬。
///
/// 两条经验（都是实测出来的）：
/// ① **不要写 `/no_think`**。在提示词里加这个软开关，反而会让 0.6B 模型把
///    `/think` 当普通文本吐在译文末尾。关闭思维链用请求体的 `think: false`。
/// ② **要明确禁止输出"思考"二字**。Qwen3 的小尺寸版本即使关了思维链，
///    有时仍会把「思考」当成正文的一部分（例如 `among` → `在……中思考`）。
const LOCAL_SYSTEM_PROMPT: &str = "\
你是翻译引擎。把用户给的内容翻译成目标语言，只输出译文本身。

规则：
1. 不要解释、不要加引号、不要输出任何前后缀。
2. 绝对不要输出 /think、/no_think、think 这类标记，也不要写「思考」二字。
3. 代码标识符、函数名、变量名、路径、命令、URL 原样保留，不要翻译。
4. 代码块（``` ... ```）内部的内容不要翻译，原样输出。
5. 保持原有换行和标点结构。
6. 如果输入已经是目标语言，原样返回。";

/// 词典模式：查单个单词时用的提示词。
///
/// 输出格式被强行规定成 `词性|含义1；含义2；含义3` 一行一条，
/// 目的是**让解析变简单**——小模型自由发挥时格式千奇百怪，
/// 与其事后猜，不如把格式写死在提示里，解析失败就退回普通翻译。
const LOCAL_DICT_PROMPT: &str = "\
你是一本英汉词典。用户给你一个英文单词，你按词性列出它的中文含义。

输出格式（严格遵守）：
每行一条，格式为：词性|含义1；含义2；含义3
- 词性用 n. / v. / adj. / adv. / prep. / conj. / pron. 这类缩写
- 同一词性的多个含义用中文分号「；」隔开，最多 4 个
- 按常用程度从高到低排列
- 只输出这些行，不要编号、不要例句、不要解释、不要输出 /think 或「思考」

示例输出：
n.|船；小船；舟
v.|划船；乘船
";

/// 兜底清洗：裁掉模型吐出来的思考标记。
///
/// 踩过的坑：模型有时**只输出一个 ` /think` 结尾标记**（被 `think:false` 影响，
/// 把控制符当成了普通文本）。早期实现遇到"只有结尾标记"时会把标记**之前**的正文
/// 整段截掉，于是译文变成空串 → 被判为翻译失败 → 回退 MyMemory，
/// 表现出来就是"本地模型对单词和短句完全不可用"。
/// 所以这里对孤立的标记一律只做**删除**，绝不截断正文。
pub fn strip_thinking(s: &str) -> String {
    /// 把 `from` 位置起、标记自身连同前面紧邻的空白一起删掉。
    fn erase_at(out: &mut String, from: usize, len: usize) {
        let mut start = from;
        while start > 0 {
            let c = out.as_bytes()[start - 1] as char;
            if c == ' ' || c == '\t' {
                start -= 1;
            } else {
                break;
            }
        }
        out.replace_range(start..from + len, "");
    }

    let mut out = s.to_string();

    // ① 先整体删掉成对的思维链块。两种写法：
    //    /think ... /think      （Qwen3 软开关的写法）
    //    <think> ... </think>   （有些模板用尖括号）
    for (open, close) in [("/think", "/think"), ("<think>", "</think>")] {
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 16 {
                break; // 防御：异常输入别死循环
            }
            let Some(i) = out.find(open) else { break };
            let Some(j) = out[i + open.len()..].find(close) else { break };
            out.replace_range(i..i + open.len() + j + close.len(), "");
        }
    }

    // ② 再删掉孤立的标记。
    //
    // 顺序很关键：**先删长的、再删短的**，而且绝不能把 `<` / `>` 单独放进列表——
    // 那会把 `<think>` 拆成 `<` + `think` + `>`，标签是没了，可标签之间的思考内容
    // 会全部留在正文里（踩过的坑：`<think>嗯</think>你好` 曾变成 `嗯你好`）。
    //
    // `/no_think` 必须排在 `/think` 前面：两者有公共前缀 `/`，
    // 先删 `/think` 会把 `/no_think` 切成 `/no_` 残片留在译文里。
    for tok in [
        "</no_think>",
        "<no_think>",
        "/no_think",
        "</think>",
        "<think>",
        "</think",
        "<think",
        "/think",
    ] {
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 16 {
                break;
            }
            let Some(i) = out.find(tok) else { break };
            erase_at(&mut out, i, tok.len());
        }
    }
    // ③ 有些模型会加 "Translation:" 之类的前缀
    let t = out.trim();
    for p in ["Translation:", "译文：", "翻译：", "译文:"] {
        if let Some(rest) = t.strip_prefix(p) {
            return rest.trim().to_string();
        }
    }
    t.to_string()
}

fn lang_name(target: &str) -> &'static str {
    match target {
        "en" => "英文",
        "ja" => "日文",
        _ => "简体中文",
    }
}

/// 一段文本是否"看起来像"目标语言。
///
/// 用途：判断模型是真的翻译了，还是把原文原样吐回来了。
/// 小模型有时会不听指令、直接把输入输出（尤其短词），如果我们把这个
/// 结果当成功缓存起来，用户就会永远看到"原文"，且这个错误结果再也改不掉。
fn looks_like_target(s: &str, target: &str) -> bool {
    match target {
        "en" | "fr" | "de" | "es" => s.chars().any(|c| c.is_ascii_alphabetic()),
        "ko" => s.chars().any(|c| matches!(c as u32, 0xAC00..=0xD7AF | 0x1100..=0x11FF)),
        "ru" => s.chars().any(|c| matches!(c as u32, 0x0400..=0x04FF)),
        "ja" => s.chars().any(|c| {
            matches!(c as u32, 0x3040..=0x30FF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF)
        }),
        // 默认目标当作中文：必须含汉字/中文标点，否则认为没翻
        _ => s.chars().any(|c| {
            matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3000..=0x303F | 0xFF00..=0xFFEF)
        }),
    }
}

/// 判断这次翻译结果是否可信。
///
/// 不可信的情况：
/// ① 结果为空（调用方另行处理）；
/// ② 结果和原文一模一样（模型直接回显）；
/// ③ 结果是另一种语言（该翻成中文却还是一串英文）。
///
/// 注意：不能简单地把"结果==原文"一律当失败——某些语言/内容确实不该变。
/// 所以这里结合"目标语言特征"来判断，只有明确没翻译时才判定失败。
pub fn is_valid_result(source: &str, translated: &str, target: &str) -> bool {
    let t = translated.trim();
    if t.is_empty() {
        return false;
    }
    // 目标语言特征不成立 → 没翻
    if !looks_like_target(t, target) {
        return false;
    }
    // 原样回显（忽略大小写与首尾空白）→ 没翻
    if t.eq_ignore_ascii_case(source.trim()) {
        return false;
    }
    true
}

/// 解析 OpenAI 兼容响应，取第一个 choice 的 message.content。
fn parse_openai_like(v: &serde_json::Value) -> Option<String> {
    // OpenAI / Ollama /v1 / llama.cpp server
    if let Some(s) = v["choices"]
        .get(0)
        .and_then(|c| c["message"]["content"].as_str())
    {
        return Some(s.to_string());
    }
    // 少数实现的非流式回包
    if let Some(s) = v["choices"].get(0).and_then(|c| c["text"].as_str()) {
        return Some(s.to_string());
    }
    // Ollama 原生 /api/chat
    if let Some(s) = v["message"]["content"].as_str() {
        return Some(s.to_string());
    }
    None
}

#[async_trait]
impl Translator for LocalEngine {
    async fn translate(&self, text: &str, target_lang: &str) -> Result<Translation, String> {
        let user = format!("翻译成{}：\n{}", lang_name(target_lang), text);
        let translated = self.chat(LOCAL_SYSTEM_PROMPT, &user).await?;

        // 模型有时会把原文原样吐回来（尤其短词）。这种结果绝不能被当成成功结果
        // 缓存下来——否则用户会永远看到"原文"，而且再也改不掉。
        if !is_valid_result(text, &translated, target_lang) {
            // 记日志，便于统计"到底是模型没翻，还是选区抓错了"
            if std::env::var("JFY_SELECTION_DEBUG").is_ok() {
                use std::io::Write;
                let path = std::env::temp_dir().join("jishifanyi-selection.log");
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(
                        f,
                        "[本地模型·未翻译] 输入「{}」→ 原样返回「{}」",
                        text.chars().take(60).collect::<String>(),
                        translated.chars().take(60).collect::<String>()
                    );
                }
            }
            return Err(format!(
                "模型没能翻译（原样返回了原文），请重试或换更大的模型。原文：{}",
                text.chars().take(40).collect::<String>()
            ));
        }

        Ok(Translation {
            translated,
            source: "auto".into(),
            target: target_lang.to_string(),
            cached: false,
            hit_count: 0,
            parts: Vec::new(),
            senses: Vec::new(),
        })
    }
}

impl LocalEngine {
    /// 向本地模型发一次请求并取回文本（翻译和查词典共用）。
    ///
    /// **优先走 Ollama 原生 `/api/chat`，失败再退回 OpenAI 兼容的
    /// `/v1/chat/completions`。** 这不是风格问题，是实测出来的 20 倍差距：
    ///
    /// | 端点 | 翻 "hello world" | 生成 token |
    /// |---|---|---|
    /// | `/v1/chat/completions` + `think:false` | 3805 ms | 186 |
    /// | `/api/chat` + `think:false` | **187 ms** | **3** |
    ///
    /// 原因：Ollama 的 OpenAI 兼容层**不认 `think:false`**，照样生成整条思维链，
    /// 只是把它塞进 `message.reasoning` 字段（正文看着正常，时间全花在思考上）。
    /// 原生端点才真正关闭思维链。
    ///
    /// 保留 `/v1` 回退是为了兼容 llama.cpp server、LM Studio、vLLM——
    /// 它们没有 `/api/chat`，但都提供 OpenAI 兼容接口。
    async fn chat(&self, system: &str, user: &str) -> Result<String, String> {
        let base = self.cfg.base_url.trim_end_matches('/');
        if base.is_empty() {
            return Err("本地模型地址为空，请在设置里填写（如 http://127.0.0.1:11434）".into());
        }

        // ⓪ 服务没开就试着拉起来。
        //
        // 用户不该被要求"记得先启动 Ollama"。只有真的要用模型时才做这件事，
        // 而且放到阻塞线程池里——启动+等待端口最多十几秒，不能卡住异步运行时。
        let base_owned = base.to_string();
        let _ = tauri::async_runtime::spawn_blocking(move || ensure_local_server(&base_owned)).await;

        // ① Ollama 原生端点（能真正关掉思维链）
        match self.post_chat(base, true, system, user).await {
            Ok(t) => return Ok(t),
            Err(native_err) => {
                // ② 退回 OpenAI 兼容端点（非 Ollama 的实现）
                match self.post_chat(base, false, system, user).await {
                    Ok(t) => Ok(t),
                    // 两个都失败时，报原生端点的错误——它对 Ollama 用户更有信息量
                    Err(_) => Err(native_err),
                }
            }
        }
    }

    /// 发一次请求。`native = true` 走 Ollama 原生 `/api/chat`，
    /// `false` 走 OpenAI 兼容的 `/v1/chat/completions`。
    async fn post_chat(
        &self,
        base: &str,
        native: bool,
        system: &str,
        user: &str,
    ) -> Result<String, String> {
        // 注意：这里**不要**限制 max_tokens。
        //
        // 曾经为了压延迟加过 `max_tokens ≈ 输入长度 × 3`，结果短词只分到 64~81 的
        // 预算，模型还没开始输出正文就被截断，返回空串 → 被判为翻译失败 →
        // 回退到 MyMemory。表现出来就是"单词和短句本地模型完全不可用"。
        let (url, body) = if native {
            (
                format!("{base}/api/chat"),
                serde_json::json!({
                    "model": self.cfg.model,
                    "stream": false,
                    // 关键：原生端点认这个开关
                    "think": false,
                    "options": { "temperature": 0.1 },
                    "messages": [
                        { "role": "system", "content": system },
                        { "role": "user", "content": user }
                    ]
                }),
            )
        } else {
            (
                format!("{base}/v1/chat/completions"),
                serde_json::json!({
                    "model": self.cfg.model,
                    "stream": false,
                    "temperature": 0.1,
                    "think": false,
                    "messages": [
                        { "role": "system", "content": system },
                        { "role": "user", "content": user }
                    ]
                }),
            )
        };

        let resp = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| format!("HTTP 客户端构建失败: {e}"))?
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    format!(
                        "连不上本地模型 {url}。请确认服务已启动（如 `ollama serve`），模型已拉取（如 `ollama pull {}`）",
                        self.cfg.model
                    )
                } else if e.is_timeout() {
                    format!(
                        "本地模型超时（>120s）。模型 `{}` 可能太大，换更小的模型试试",
                        self.cfg.model
                    )
                } else {
                    format!("本地模型请求失败: {e}")
                }
            })?;

        if !resp.status().is_success() {
            let code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "本地模型返回 {code}: {}",
                body.chars().take(200).collect::<String>()
            ));
        }

        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("本地模型响应解析失败: {e}"))?;
        let raw = parse_openai_like(&v).ok_or_else(|| {
            format!(
                "本地模型响应里找不到文本字段: {}",
                v.to_string().chars().take(200).collect::<String>()
            )
        })?;
        let cleaned = strip_thinking(&raw);
        if cleaned.trim().is_empty() {
            return Err("本地模型返回了空内容（可能模型不支持这个语言方向）".into());
        }
        Ok(cleaned)
    }

    /// 查词典：让模型按 `词性|含义1；含义2` 的格式输出多条释义。
    ///
    /// 只在"输入确实是一个英文单词"时调用。解析失败就返回 Err，
    /// 调用方会退回普通翻译——**宁可少给信息，也不能给错信息。**
    pub async fn lookup_word(&self, word: &str, target: &str) -> Result<Vec<Sense>, String> {
        let user = format!("{word}");
        let out = self.chat(LOCAL_DICT_PROMPT, &user).await?;
        let senses = parse_senses(&out);
        if senses.is_empty() {
            return Err("词典格式解析失败".into());
        }
        // 至少要有一个含义是目标语言，否则认为没翻
        let has_target = senses
            .iter()
            .any(|s| s.meanings.iter().any(|m| looks_like_target(m, target)));
        if !has_target {
            return Err("词典结果里没有目标语言".into());
        }
        Ok(senses)
    }
}

/// 解析 `词性|含义1；含义2` 形式的词典输出。解析不出来的行直接丢掉。
fn parse_senses(s: &str) -> Vec<Sense> {
    let mut out: Vec<Sense> = Vec::new();
    for line in s.lines() {
        let line = line.trim().trim_start_matches(|c: char| {
            c.is_ascii_digit() || c == '.' || c == '-' || c == '*' || c == ' '
        });
        if line.is_empty() {
            continue;
        }
        let Some((pos, rest)) = line.split_once('|') else {
            continue;
        };
        let meanings: Vec<String> = rest
            .split(['；', ';', ',', '，'])
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .take(4)
            .collect();
        if meanings.is_empty() {
            continue;
        }
        out.push(Sense {
            pos: pos.trim().to_string(),
            meanings,
        });
        if out.len() >= 5 {
            break;
        }
    }
    out
}

/// 判断输入是不是"一个可以查词典的英文单词"。
///
/// 要求：单个片段、只有字母、长度 2~24。
/// 带空格的是短语、带下划线/点的是标识符，都不走词典模式。
pub fn is_lookup_word(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() || t.chars().count() > 24 {
        return false;
    }
    if !t.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    t.chars().count() >= 2
}

// ---------------- 自动判断翻译方向 ----------------

/// 文本里是否含中文字符（CJK 统一表意文字）。
pub fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        let u = c as u32;
        (0x4E00..=0x9FFF).contains(&u)        // 基本区
            || (0x3400..=0x4DBF).contains(&u) // 扩展 A
            || (0xF900..=0xFAFF).contains(&u) // 兼容表意文字
            || (0x3000..=0x303F).contains(&u) // 中文标点
    })
}

/// 用户选的目标语言（"auto" 表示按内容自动判断）。
///
/// 放在全局静态里是为了少改调用链：翻译入口有七八处，
/// 都通过 `auto_target()` 取方向，改一处即可全部生效。
static TARGET_PREF: std::sync::OnceLock<std::sync::RwLock<String>> = std::sync::OnceLock::new();

fn pref_cell() -> &'static std::sync::RwLock<String> {
    TARGET_PREF.get_or_init(|| std::sync::RwLock::new("auto".to_string()))
}

/// 设置目标语言偏好（"auto" / "zh-CN" / "en" / "ja" / "ko" / "ru" / "fr" / "de" / "es"）
pub fn set_target_pref(v: &str) {
    let v = normalize_lang(v);
    if let Ok(mut g) = pref_cell().write() {
        *g = v;
    }
}

pub fn target_pref() -> String {
    pref_cell()
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|_| "auto".to_string())
}

/// 只接受已知的语言代码，其余一律当 auto（防止设置文件被改坏）
fn normalize_lang(v: &str) -> String {
    match v {
        "zh-CN" | "zh-TW" | "en" | "ja" | "ko" | "ru" | "fr" | "de" | "es" => v.to_string(),
        _ => "auto".to_string(),
    }
}

/// 目标是不是中文（只有中文目标才该走本地词典）
pub fn is_zh_target(target: &str) -> bool {
    target.starts_with("zh")
}

/// 按字符区段猜源语言。只做"够用"的判断，不引入语言识别库。
pub fn detect_source(text: &str) -> &'static str {
    let (mut han, mut kana, mut hangul, mut cyr, mut latin) = (0, 0, 0, 0, 0);
    for c in text.chars() {
        let u = c as u32;
        match u {
            0x3040..=0x30FF => kana += 1,
            0xAC00..=0xD7AF => hangul += 1,
            0x4E00..=0x9FFF => han += 1,
            0x0400..=0x04FF => cyr += 1,
            0x0041..=0x007A => latin += 1,
            _ => {}
        }
    }
    if kana > 0 {
        "ja"
    } else if hangul > 0 {
        "ko"
    } else if han > 0 {
        "zh-CN"
    } else if cyr > 0 {
        "ru"
    } else if latin > 0 {
        "en"
    } else {
        "en"
    }
}

/// 判定翻译方向。
///
/// 规则：
///  1. 用户指定了目标语言 → 用它；但源语言和目标相同时换个方向，
///     否则"中译中"会退化成原文回显。
///  2. 用户选"自动" → 源是中文就翻英文；源是英/日/韩/俄 → 翻中文。
pub fn auto_target(text: &str) -> &'static str {
    let pref = target_pref();
    let src = detect_source(text);
    if pref == "auto" {
        return if is_zh_target(src) { "en" } else { "zh-CN" };
    }
    let pref_static = match pref.as_str() {
        "zh-CN" => "zh-CN",
        "zh-TW" => "zh-TW",
        "en" => "en",
        "ja" => "ja",
        "ko" => "ko",
        "ru" => "ru",
        "fr" => "fr",
        "de" => "de",
        "es" => "es",
        _ => "zh-CN",
    };
    if pref_static == src || (is_zh_target(pref_static) && is_zh_target(src)) {
        return if is_zh_target(src) { "en" } else { "zh-CN" };
    }
    pref_static
}

// ---------------- 自动拉起本地模型服务 ----------------
//
// 设计目标：用户不需要"记得先打开 Ollama"。
// 只有本地模型地址指向本机时才会尝试启动（远程服务器不归我们管），
// 而且**整个进程只尝试一次**——失败了就交给上层回退到 MyMemory，
// 不要在每条请求上都等十几秒。

use std::sync::atomic::{AtomicBool, Ordering};

static AUTO_START_TRIED: AtomicBool = AtomicBool::new(false);

/// `base` 是否指向本机；是则返回 (host, port)。
fn local_target(base: &str) -> Option<(String, u16)> {
    let rest = base
        .strip_prefix("http://")
        .or_else(|| base.strip_prefix("https://"))?;
    let hostport = rest.split('/').next()?;
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().ok()?),
        None => (hostport.to_string(), 80),
    };
    let is_local =
        host == "127.0.0.1" || host == "localhost" || host == "::1" || host == "[::1]";
    if !is_local {
        return None;
    }
    Some((host, port))
}

fn tcp_ready(host: &str, port: u16, timeout_ms: u64) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Ok(addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    for addr in addrs {
        if TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(timeout_ms)).is_ok() {
            return true;
        }
    }
    false
}

/// 找到 Ollama 的可执行文件。
fn find_ollama_exe() -> Option<std::path::PathBuf> {
    let mut cands: Vec<std::path::PathBuf> = Vec::new();
    for var in ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(p) = std::env::var(var) {
            let root = std::path::PathBuf::from(p);
            cands.push(root.join("Programs/Ollama/ollama.exe"));
            cands.push(root.join("Ollama/ollama.exe"));
        }
    }
    for c in &cands {
        if c.exists() {
            return Some(c.clone());
        }
    }
    // 再试 PATH（有些用户用 winget / scoop 装）
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if let Ok(out) = std::process::Command::new("where")
            .arg("ollama")
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                if let Some(line) = text.lines().next() {
                    let p = std::path::PathBuf::from(line.trim());
                    if p.exists() {
                        return Some(p);
                    }
                }
            }
        }
    }
    None
}

/// 本地模型服务没在监听时，尝试把它拉起来（只试一次）。
///
/// 这个函数是阻塞的（最多等十几秒），调用方必须放到 blocking 线程池。
pub fn ensure_local_server(base: &str) {
    let Some((host, port)) = local_target(base) else {
        return;
    };
    // 已经在跑 → 什么都不做
    if tcp_ready(&host, port, 250) {
        return;
    }
    // 本进程已经试过启动 → 不再重试（避免每条请求都卡住）
    if AUTO_START_TRIED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(exe) = find_ollama_exe() else {
        return;
    };

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new(&exe)
            .arg("serve")
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new(&exe).arg("serve").spawn();
    }

    // 等端口就绪（Ollama 冷启动通常 1–3 秒）
    for _ in 0..60 {
        if tcp_ready(&host, port, 250) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

// ---------------- MyMemory 公开免费 API（兜底，国内可用） ----------------

pub struct MyMemoryEngine;

// MyMemory 需显式源语言，按目标语言推断（对应 英译中/中译英 方向）
fn pair_from_target(target_lang: &str) -> (String, String) {
    if target_lang == "en" {
        ("zh-CN".to_string(), "en".to_string())
    } else {
        ("en".to_string(), "zh-CN".to_string())
    }
}

#[async_trait]
impl Translator for MyMemoryEngine {
    async fn translate(&self, text: &str, target_lang: &str) -> Result<Translation, String> {
        let (src, tgt) = pair_from_target(target_lang);
        let langpair = format!("{}|{}", src, tgt);
        let email = MYMEMORY_EMAIL.lock().unwrap().clone().unwrap_or_default();
        let mut params: Vec<(&str, &str)> = vec![("q", text), ("langpair", langpair.as_str())];
        if !email.trim().is_empty() {
            params.push(("de", email.as_str()));
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| format!("HTTP 客户端构建失败: {e}"))?;
        let resp = client
            .get("https://api.mymemory.translated.net/get")
            .query(&params)
            .send()
            .await
            .map_err(|e| format!("网络错误: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("MyMemory 返回 {}", resp.status()));
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| format!("解析失败: {e}"))?;
        let translated = v["responseData"]["translatedText"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if translated.is_empty() && v["responseStatus"].as_i64() != Some(200) {
            return Err(format!(
                "MyMemory: {}",
                v["responseDetails"].as_str().unwrap_or("未知错误")
            ));
        }
        Ok(Translation {
            translated,
            source: src,
            target: tgt,
            cached: false,
            hit_count: 0,
            parts: Vec::new(),
            senses: Vec::new(),
        })
    }
}

// ---------------- Google 免费端点（逆向，仅个人自用） ----------------

pub struct GoogleEngine;

fn parse_google(body: &str) -> (String, String) {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let mut out = String::new();
    if let Some(segs) = v.get(0).and_then(|a| a.as_array()) {
        for seg in segs {
            if let Some(t) = seg.get(0).and_then(|s| s.as_str()) {
                out.push_str(t);
            }
        }
    }
    let source = v
        .get(2)
        .and_then(|s| s.as_str())
        .unwrap_or("auto")
        .to_string();
    (out, source)
}

#[async_trait]
impl Translator for GoogleEngine {
    async fn translate(&self, text: &str, target_lang: &str) -> Result<Translation, String> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| format!("HTTP 客户端构建失败: {e}"))?;
        let resp = client
            .get("https://translate.googleapis.com/translate_a/single")
            .query(&[
                ("client", "gtx"),
                ("sl", "auto"),
                ("tl", target_lang),
                ("dt", "t"),
                ("q", text),
            ])
            .send()
            .await
            .map_err(|e| format!("网络错误: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("Google 返回 {}", resp.status()));
        }
        let body = resp.text().await.map_err(|e| format!("读取失败: {e}"))?;
        let (translated, source) = parse_google(&body);
        Ok(Translation {
            translated,
            source,
            target: target_lang.to_string(),
            cached: false,
            hit_count: 0,
            parts: Vec::new(),
            senses: Vec::new(),
        })
    }
}

// ---------------- DeepSeek 云端（OpenAI 兼容） ----------------
//
// 为什么加它：本地模型要装 Ollama、要显存，普通用户走不完这套流程。
// DeepSeek 的 API 兼容 OpenAI 格式，接进来只需要一个 key，
// 成本极低（一次短句翻译不到一分钱），而且质量明显好于 1.7B 的小模型。
//
// 定位：**付费但零配置**的选项——介于"免费云端 MyMemory（质量一般）"
// 和"本地模型（免费但要折腾）"之间。

pub struct DeepSeekEngine {
    cfg: LocalConfig,
}

impl DeepSeekEngine {
    pub fn new(cfg: LocalConfig) -> Self {
        Self { cfg }
    }
}

/// 预热到 DeepSeek 的连接（DNS + TLS + 连接池）。
///
/// 为什么要这个：第一次请求要付"建连"的一次性成本。实测第一次能到 7 秒，
/// 之后复用连接只要 1 秒。用户第一次划词就吃这个亏，体验很差。
///
/// 这里只发一个 GET 到根路径——服务端会返回 4xx，但**连接已经建立了**，
/// 且不消耗任何 token 额度。失败也无所谓（没网/没 key 时静默忽略）。
pub async fn warm_up_cloud() {
    let _ = http()
        .get("https://api.deepseek.com/")
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await;
}

#[async_trait]
impl Translator for DeepSeekEngine {
    async fn translate(&self, text: &str, target_lang: &str) -> Result<Translation, String> {
        let key = self.cfg.api_key.trim();
        if key.is_empty() {
            return Err(
                "未配置 DeepSeek API Key。请在 ⚙ 设置 → 云端模型 里粘贴你的 key（sk-…）".into(),
            );
        }
        let model = {
            let m = self.cfg.cloud_model.trim();
            if m.is_empty() {
                "deepseek-chat"
            } else {
                m
            }
        };
        let user = format!("翻译成{}：\n{}", lang_name(target_lang), text);
        let body = serde_json::json!({
            "model": model,
            "stream": false,
            "temperature": 0.1,
            "messages": [
                { "role": "system", "content": LOCAL_SYSTEM_PROMPT },
                { "role": "user", "content": user }
            ]
        });

        let resp = http()
            .post("https://api.deepseek.com/chat/completions")
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    "DeepSeek 请求超时（>120s），检查网络或稍后重试".to_string()
                } else if e.is_connect() {
                    "连不上 api.deepseek.com，请检查网络/代理".to_string()
                } else {
                    format!("DeepSeek 请求失败: {e}")
                }
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let hint = match status.as_u16() {
                401 => "（API Key 无效或已过期）",
                402 => "（账户余额不足）",
                429 => "（请求过于频繁或额度用尽，稍后再试）",
                _ => "",
            };
            return Err(format!(
                "DeepSeek 返回 {status}{hint}: {}",
                body.chars().take(200).collect::<String>()
            ));
        }

        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("DeepSeek 响应解析失败: {e}"))?;
        let raw = v["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| {
                format!(
                    "DeepSeek 响应里找不到文本: {}",
                    v.to_string().chars().take(200).collect::<String>()
                )
            })?;

        let translated = strip_thinking(raw);
        if !is_valid_result(text, &translated, target_lang) {
            return Err("DeepSeek 没能翻译这段内容（返回了原文或语言不对），请重试".into());
        }
        Ok(Translation {
            translated,
            source: "auto".into(),
            target: target_lang.to_string(),
            cached: false,
            hit_count: 0,
            parts: Vec::new(),
            senses: Vec::new(),
        })
    }
}

// ---------------- 注册表 ----------------

/// 按 id 构造引擎实例。`cfg` 只在 local / deepseek 引擎时使用。
pub fn get(id: &str, cfg: &LocalConfig) -> Option<Box<dyn Translator>> {
    match id {
        "local" => Some(Box::new(LocalEngine { cfg: cfg.clone() })),
        "deepseek" => Some(Box::new(DeepSeekEngine { cfg: cfg.clone() })),
        "mymemory" => Some(Box::new(MyMemoryEngine)),
        "google" => Some(Box::new(GoogleEngine)),
        _ => None,
    }
}

/// 引擎清单：(id, 显示名, 是否推荐默认)
pub fn list() -> Vec<(&'static str, &'static str, bool)> {
    vec![
        ("local", "本地模型 (OpenAI 兼容)", true),
        ("deepseek", "DeepSeek 云端 (需 API Key)", false),
        ("mymemory", "MyMemory (免费/国内可用)", false),
        ("google", "Google 免费 (逆向·勿商用)", false),
    ]
}

/// 引擎 id 是否合法
pub fn exists(id: &str) -> bool {
    matches!(id, "local" | "deepseek" | "mymemory" | "google")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_echo_and_wrong_language() {
        // 原样回显 → 无效
        assert!(!is_valid_result("except", "except", "zh-CN"));
        assert!(!is_valid_result("immune", "mmune t", "zh-CN"));
        // 该翻成中文却还是英文 → 无效
        assert!(!is_valid_result("hello world", "hello world!", "zh-CN"));
        // 正常译文 → 有效
        assert!(is_valid_result("except", "除……之外", "zh-CN"));
        assert!(is_valid_result("hello", "你好", "zh-CN"));
        // 英文目标：中文输入翻成英文才算有效
        assert!(is_valid_result("你好", "hello", "en"));
        assert!(!is_valid_result("你好", "你好", "en"));
    }

    #[test]
    fn strips_thinking() {
        assert_eq!(strip_thinking("/think 想一下 /think你好"), "你好");
        assert_eq!(strip_thinking("Translation: 你好"), "你好");
        assert_eq!(strip_thinking("  你好  "), "你好");
        // 关键回归用例：模型只吐了个结尾标记，正文必须保住
        assert_eq!(strip_thinking("双精度最后更新时间 /think"), "双精度最后更新时间");
        assert_eq!(strip_thinking("敏捷的棕色狐狸 /think"), "敏捷的棕色狐狸");
        assert_eq!(strip_thinking("/think"), "");
        // 中译英时模型偶尔会把 /no_think 甩在句尾（实测：`return /no_think`）
        assert_eq!(strip_thinking("return /no_think"), "return");
        assert_eq!(strip_thinking("/no_think 你好"), "你好");
        // 长标记先删，不能留下 /no_ 残片
        assert_eq!(strip_thinking("ok /no_think"), "ok");
        // 成对思维链块整体去掉，保留后面的正文
        assert_eq!(
            strip_thinking("<think>嗯，让我想想</think>你好"),
            "你好"
        );
    }

    /// 自动方向：有中文翻英文，没中文翻中文。
    #[test]
    fn auto_target_by_script() {
        assert_eq!(auto_target("immune"), "zh-CN");
        assert_eq!(auto_target("The boat moves fast"), "zh-CN");
        assert_eq!(auto_target("你好"), "en");
        assert_eq!(auto_target("这是一段中文，里面夹着 code"), "en");
        // 只有中文标点也算中文（用户可能只选了个「，」周围）
        assert_eq!(auto_target("好的。"), "en");
        // 空串按英文处理，翻成中文
        assert_eq!(auto_target(""), "zh-CN");
    }

    /// 只解析本机地址；远程地址不该被我们尝试启动。
    #[test]
    fn local_target_only_matches_loopback() {
        assert_eq!(
            local_target("http://127.0.0.1:11434"),
            Some(("127.0.0.1".to_string(), 11434))
        );
        assert_eq!(
            local_target("http://localhost:11434/v1"),
            Some(("localhost".to_string(), 11434))
        );
        assert_eq!(local_target("http://192.168.1.10:11434"), None);
        assert_eq!(local_target("https://api.example.com"), None);
        assert_eq!(local_target("not-a-url"), None);
    }

    /// 真实环境验证：Ollama 关着的时候，能不能被自动拉起来。
    ///
    /// 这个测试会**真的启动一个 Ollama 进程**，所以标了 #[ignore]，
    /// 只在手动验证时用：
    ///   cargo test --lib -- --ignored auto_start_spawns
    #[test]
    #[ignore]
    fn auto_start_spawns_local_server() {
        let (host, port) = local_target("http://127.0.0.1:11434").unwrap();
        assert!(
            !tcp_ready(&host, port, 200),
            "测试前提不成立：11434 已经在监听，请先停掉 Ollama"
        );
        ensure_local_server("http://127.0.0.1:11434");
        assert!(
            tcp_ready(&host, port, 800),
            "自动启动失败：11434 仍不可达"
        );
    }
}
