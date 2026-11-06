//! 本地翻译缓存：原文 -> 译文，持久化到 app 数据目录的 cache.json。
//!
//! 两个作用：
//! ① 本地模型推理慢（CPU 上可能几百 ms 到数秒），命中缓存 = 0 延迟；
//! ② 记录每个词被查过的次数，UI 可以显示"查过 N 次"，让用户对重复出现的词有印象。
//!
//! 设计取舍：用单个 JSON 文件而不是 SQLite —— 依赖为零、实现简单、够用。
//! 条目数超过 MAX_ENTRIES 时按最近使用时间淘汰一半。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

const MAX_ENTRIES: usize = 5000;
const CACHE_FILE: &str = "cache.json";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    /// 译文
    pub translated: String,
    /// 源语言（引擎检测结果）
    pub source: String,
    /// 累计查询次数
    pub count: u32,
    /// 最近一次使用时间（unix 秒），用于淘汰
    pub last_used: u64,
    /// 代码模式的拆词结果（普通文本为空）。
    /// 必须缓存：拆词要对每个子词各调一次模型，不缓存的话每次悬停都要重算。
    #[serde(default)]
    pub parts: Vec<crate::engine::CodePart>,
    /// 词典模式的多条释义（普通文本与代码模式为空）。
    /// 同样要缓存：查词典要走一次模型，重复查同一个词应该秒出。
    #[serde(default)]
    pub senses: Vec<crate::engine::Sense>,
}

/// 缓存键：原文 + 翻译方向（同一段文字中译英和英译中要分开存）
pub fn key(text: &str, target: &str) -> String {
    format!("{target}\u{1}{text}")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Default, Serialize, Deserialize)]
pub struct Cache {
    pub entries: HashMap<String, Entry>,
    #[serde(skip)]
    path: Option<PathBuf>,
    #[serde(skip)]
    dirty: bool,
}

impl Cache {
    /// 从磁盘加载；文件不存在或损坏时返回空缓存（不报错，缓存不该阻塞启动）。
    pub fn load(dir: &std::path::Path) -> Self {
        let path = dir.join(CACHE_FILE);
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, Entry>>(&s).ok())
            .unwrap_or_default();
        Self {
            entries,
            path: Some(path),
            dirty: false,
        }
    }

    /// 查缓存。命中会累加计数并刷新 last_used。
    pub fn get(&mut self, text: &str, target: &str) -> Option<Entry> {
        let k = key(text, target);
        let e = self.entries.get_mut(&k)?;
        e.count = e.count.saturating_add(1);
        e.last_used = now_secs();
        let out = e.clone();
        self.dirty = true;
        Some(out)
    }

    /// 写入/更新缓存。
    pub fn put(
        &mut self,
        text: &str,
        target: &str,
        translated: &str,
        source: &str,
        parts: &[crate::engine::CodePart],
        senses: &[crate::engine::Sense],
    ) {
        let k = key(text, target);
        let count = self.entries.get(&k).map(|e| e.count).unwrap_or(0) + 1;
        self.entries.insert(
            k,
            Entry {
                translated: translated.to_string(),
                source: source.to_string(),
                count,
                last_used: now_secs(),
                parts: parts.to_vec(),
                senses: senses.to_vec(),
            },
        );
        if self.entries.len() > MAX_ENTRIES {
            self.evict();
        }
        self.dirty = true;
    }

    /// 淘汰：按 last_used 排序，丢掉最旧的一半。
    fn evict(&mut self) {
        let mut v: Vec<(String, u64)> = self
            .entries
            .iter()
            .map(|(k, e)| (k.clone(), e.last_used))
            .collect();
        v.sort_by_key(|(_, t)| *t);
        for (k, _) in v.into_iter().take(self.entries.len() / 2) {
            self.entries.remove(&k);
        }
    }

    /// 落盘。只在有改动时写。
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string(&self.entries) {
            let _ = std::fs::write(path, s);
            self.dirty = false;
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 清掉"明显无效"的历史条目。
    ///
    /// 早期版本会把"模型原样回显原文"这种失败结果也写进缓存，于是用户永久
    /// 看到原文。启动时跑一次体检，把这些坏条目删掉——否则修好了逻辑，
    /// 旧数据还在继续污染界面。
    ///
    /// 判定：译文与原文相同（忽略大小写/空白），或译文里完全没有目标语言特征。
    /// 注意 `target` 存在 key 里（形如 `zh-CN\u{1}原文`），这里直接对译文判定。
    pub fn purge_invalid(&mut self) -> usize {
        let before = self.entries.len();
        self.entries.retain(|k, e| {
            // key 形如 "<target>\u{1}<source>"
            let target = k.split('\u{1}').next().unwrap_or("zh-CN");
            let source = k.split('\u{1}').nth(1).unwrap_or("");
            super::engine::is_valid_result(source, &e.translated, target)
        });
        let removed = before - self.entries.len();
        if removed > 0 {
            self.dirty = true;
        }
        removed
    }
}

/// 判断一段文本是否"像代码"：无空格且含 _ / . / :: / camelCase 边界。
/// 用于给 UI 提示"这段按标识符拆词解释更合适"，以及决定是否走快速路径。
pub fn looks_like_identifier(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() || t.contains(char::is_whitespace) {
        return false;
    }
    if t.contains('_') || t.contains("::") || t.contains('.') {
        return true;
    }
    // camelCase / PascalCase：至少一个小写字母后面跟大写字母
    let chars: Vec<char> = t.chars().collect();
    chars
        .windows(2)
        .any(|w| w[0].is_lowercase() && w[1].is_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_counts_up() {
        let mut c = Cache::default();
        // 首次翻译完成时写入，count 记为 1 —— 也就是"你查过 1 次"
        c.put("hello", "zh-CN", "你好", "en", &[], &[]);
        let after_put = c.entries.get(&key("hello", "zh-CN")).map(|e| e.count);
        assert_eq!(after_put, Some(1), "首次写入应记为查过 1 次");
        // 之后再命中，逐次累加
        assert_eq!(c.get("hello", "zh-CN").unwrap().count, 2);
        assert_eq!(c.get("hello", "zh-CN").unwrap().count, 3);
        // 同一个词不同方向不串味
        assert!(c.get("hello", "en").is_none());
    }

    #[test]
    fn detects_identifiers() {
        assert!(looks_like_identifier("BUYOUT_SECRET"));
        assert!(looks_like_identifier("machineCode"));
        assert!(looks_like_identifier("std::vector"));
        assert!(!looks_like_identifier("hello world"));
        assert!(!looks_like_identifier("hello"));
    }
}
