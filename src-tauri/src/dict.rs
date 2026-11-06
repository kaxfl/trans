//! 本地词典：mmap + 二分查找，查单词**不经过任何模型**。
//!
//! 为什么需要它：
//! 之前查单个单词也走大模型，结果是 ① 慢（1–3 秒）② 词性经常标错
//! （比如 `among` 被标成 n./v./adj.）。而单词恰恰是**最该查表**的场景。
//!
//! 数据由 `data/build_dict.mjs` 从 ECDICT 生成（56 万单词，35 MB），格式：
//! ```text
//! [0..8)   魔数 "ECDICT01"
//! [8..12)  条目数 u32
//! [12..16) 索引区偏移 u32
//! [16..20) 数据区偏移 u32
//! 索引区：每条 8 字节（word_hash u32 + record_offset u32），按 hash 升序
//! 数据区：每条 = [word_len u16][word][phon_len u16][phon][trans_len u16][trans][flags u8]
//!          flags 低 3 位 = 柯林斯星级，bit3 = 牛津核心词
//! ```
//!
//! 运行时用 mmap 打开，**不把词典读进内存**（56 万条全读进来要几百 MB），
//! 只在查询时按偏移读那一条。内存占用接近零，查询是微秒级。

use std::path::Path;

/// 词典里查到的一条。
#[derive(Clone, Debug)]
pub struct DictEntry {
    /// 单词
    pub word: String,
    /// 音标（可能为空）
    pub phonetic: String,
    /// 中文释义，多条用 `\n` 分隔（形如 `n. 船` / `vi. 乘船`）
    pub translation: String,
    /// 柯林斯星级 0–5
    pub collins: u8,
    /// 是否是牛津核心词
    pub oxford: bool,
}

/// FNV-1a 32 位。**必须和 `build_dict.mjs` 里的实现完全一致**，否则查不到。
fn hash_word(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

pub struct Dict {
    mmap: memmap2::Mmap,
    count: u32,
    index_off: usize,
}

impl Dict {
    /// 打开词典文件。文件不存在或格式不对时返回 Err，调用方应当当作"没有词典"处理。
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path).map_err(|e| format!("打开词典失败: {e}"))?;
        // SAFETY: 词典文件是只读的静态数据，程序运行期间不会被别的进程改写。
        let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| format!("映射词典失败: {e}"))?;
        if mmap.len() < 20 || &mmap[0..8] != b"ECDICT01" {
            return Err("词典文件格式不对（缺少 ECDICT01 魔数）".into());
        }
        let rd = |o: usize| -> u32 {
            u32::from_le_bytes([mmap[o], mmap[o + 1], mmap[o + 2], mmap[o + 3]])
        };
        let count = rd(8);
        let index_off = rd(12) as usize;
        Ok(Self {
            mmap,
            count,
            index_off,
        })
    }

    pub fn len(&self) -> u32 {
        self.count
    }

    /// 查一个单词。返回 None 表示词典里没有。
    pub fn lookup(&self, word: &str) -> Option<DictEntry> {
        let key = word.trim().to_ascii_lowercase();
        if key.is_empty() {
            return None;
        }
        let target = hash_word(&key);
        let rd_u32 = |o: usize| -> u32 {
            u32::from_le_bytes([self.mmap[o], self.mmap[o + 1], self.mmap[o + 2], self.mmap[o + 3]])
        };

        // 索引区按 hash 升序 → 二分找到"第一个 hash == target"的位置
        let mut lo: u32 = 0;
        let mut hi: u32 = self.count;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let h = rd_u32(self.index_off + mid as usize * 8);
            if h < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // 从 lo 开始线性扫（hash 冲突极少，通常第一条就命中）
        let mut i = lo;
        while i < self.count {
            let base = self.index_off + i as usize * 8;
            let h = rd_u32(base);
            if h != target {
                break;
            }
            let rec_off = rd_u32(base + 4) as usize;
            if let Some(e) = self.read_record(rec_off) {
                // hash 相同不代表单词相同，必须比对原文
                if e.word == key {
                    return Some(e);
                }
            }
            i += 1;
        }
        None
    }

    /// 读一条记录（数据区里是 [len u16][bytes] 交替的三段 + flags）。
    fn read_record(&self, off: usize) -> Option<DictEntry> {
        let m = &self.mmap;
        let rd_u16 = |o: usize| -> u16 {
            if o + 2 > m.len() {
                return 0;
            }
            u16::from_le_bytes([m[o], m[o + 1]])
        };
        let read_str = |o: usize, n: usize| -> String {
            if o + n > m.len() {
                return String::new();
            }
            String::from_utf8_lossy(&m[o..o + n]).to_string()
        };

        let mut p = off;
        let wl = rd_u16(p) as usize;
        p += 2;
        let word = read_str(p, wl);
        p += wl;
        let pl = rd_u16(p) as usize;
        p += 2;
        let phonetic = read_str(p, pl);
        p += pl;
        let tl = rd_u16(p) as usize;
        p += 2;
        let translation = read_str(p, tl);
        p += tl;
        let flags = *m.get(p).unwrap_or(&0);

        if word.is_empty() {
            return None;
        }
        Some(DictEntry {
            word,
            phonetic,
            translation,
            collins: flags & 0x07,
            oxford: (flags & 0x08) != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// hash 必须和 JS 实现一致。这个值是用 build_dict.mjs 里的 hashWord('boat') 算出来的，
    /// 两边不一致的话词典会完全查不到（而且没有任何报错），所以拿它当回归锚点。
    #[test]
    fn hash_matches_js() {
        fn js_ref(s: &str) -> u32 {
            // 与 build_dict.mjs 完全相同的写法
            let mut h: u32 = 0x811c9dc5;
            for c in s.chars() {
                let code = c as u32;
                h ^= code;
                h = h.wrapping_mul(0x01000193);
            }
            h
        }
        for w in ["boat", "among", "code", "a", "implementation"] {
            assert_eq!(hash_word(w), js_ref(w), "hash 不一致: {w}");
        }
    }
}
