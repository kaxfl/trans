// 从 ECDICT 的 SQLite（811 MB / 340 万条）抽出"轻量词典"。
//
// 为什么不用现成的库：
//   ① 811 MB 太大，带一堆我们用不到的列（词形变化、音频、详细词典…）
//   ② 运行时不想背一个 SQLite 引擎
//
// 裁剪策略（这是"轻量化"的关键）：
//   - 只留单词（`word` 必须是纯字母），**丢掉短语和专有名词**：查代码注释够用
//   - 丢掉没有中文释义的条目
//   - 丢掉网络用语/地名/人名那类偏离主线的释义段
//   - 保留柯林斯星级和牛津核心词标记，UI 上可以显示"这是常用词"
//
// 输出格式（自定义：固定宽度索引 + 变长数据段，配合 mmap 二分查找）：
//   [0..8)   魔数 "ECDICT01"
//   [8..12)  条目数 u32
//   [12..16) 索引区偏移 u32（固定 20）
//   [16..20) 数据区偏移 u32
//   索引区：每条 8 字节（word_hash u32 + record_offset u32），按 hash 升序
//   数据区：每条 = [word_len u16][word][phon_len u16][phon][trans_len u16][trans][flags u8]
//           flags 低 3 位 = 柯林斯星级(0-5)，第 4 位 = 是否牛津核心词
//
// 用法: node build_dict.mjs <stardict.db> <输出文件>

import fs from "node:fs";
import { DatabaseSync } from "node:sqlite";

const [, , srcPath, outPath] = process.argv;
if (!srcPath || !outPath) {
  console.error("用法: node build_dict.mjs <stardict.db> <输出文件>");
  process.exit(1);
}

// FNV-1a 32 位；Rust 侧用同样的实现，保证 hash 一致
function hashWord(s) {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h >>> 0;
}

// 中文释义瘦身：只保留词性+释义，去掉网络/地名/人名等段落
const NOISE = /^\[(网络|地名|人名|医|化|法|机|电|纺|建|农|生|数|物|军|航海|航空|戏剧|俚语|古语|废语)\]/i;
function trimTranslation(raw) {
  if (!raw) return "";
  const lines = String(raw)
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter(Boolean);
  const keep = [];
  for (const line of lines) {
    if (NOISE.test(line)) continue;
    keep.push(line);
    if (keep.length >= 5) break;
  }
  const out = keep.join("；").replace(/\s+/g, " ").trim();
  return out.length > 200 ? out.slice(0, 200) : out;
}

console.log("打开源数据库…");
const db = new DatabaseSync(srcPath, { readOnly: true });

console.log("读取候选词条（只取单词 + 有中文释义）…");
// word GLOB 只能判断首字符；后面再用 JS 正则收紧
const rows = db
  .prepare(
    `SELECT word, phonetic, translation, collins, oxford, bnc, frq
     FROM stardict
     WHERE translation IS NOT NULL AND translation != ''
       AND word GLOB '[a-zA-Z]*'
       AND length(word) BETWEEN 1 AND 24`,
  )
  .all();
db.close();
console.log(`  候选: ${rows.length}`);

const isPureWord = /^[a-z]+$/i;
const entries = [];
for (const r of rows) {
  const word = String(r.word || "").trim().toLowerCase();
  if (!isPureWord.test(word)) continue; // 丢掉短语/带符号的
  const trans = trimTranslation(r.translation);
  if (!trans) continue;
  const collins = Number(r.collins) || 0;
  const oxford = Number(r.oxford) ? 1 : 0;
  const flags = (Math.min(collins, 7) & 0x07) | (oxford << 3);
  entries.push({
    word,
    phonetic: String(r.phonetic || "").trim().slice(0, 40),
    trans,
    flags,
  });
}
console.log(`  最终保留: ${entries.length} 个单词`);

// 去重（同一个 word 可能有多条，保留释义最长的）
const byWord = new Map();
for (const e of entries) {
  const old = byWord.get(e.word);
  if (!old || e.trans.length > old.trans.length) byWord.set(e.word, e);
}
const list = [...byWord.values()];
console.log(`  去重后: ${list.length}`);

console.log("排序并生成索引…");
list.sort((a, b) => {
  const ha = hashWord(a.word);
  const hb = hashWord(b.word);
  return ha === hb ? 0 : ha - hb;
});

const HEADER = 20;
const indexSize = list.length * 8;
const dataStart = HEADER + indexSize;

const indexBuf = Buffer.alloc(indexSize);
const chunks = [];
let offset = dataStart;

for (let i = 0; i < list.length; i++) {
  const e = list[i];
  const w = Buffer.from(e.word, "utf8");
  const p = Buffer.from(e.phonetic, "utf8");
  const t = Buffer.from(e.trans, "utf8");
  const rec = Buffer.alloc(2 + w.length + 2 + p.length + 2 + t.length + 1);
  let o = 0;
  rec.writeUInt16LE(w.length, o); o += 2; w.copy(rec, o); o += w.length;
  rec.writeUInt16LE(p.length, o); o += 2; p.copy(rec, o); o += p.length;
  rec.writeUInt16LE(t.length, o); o += 2; t.copy(rec, o); o += t.length;
  rec.writeUInt8(e.flags, o);
  chunks.push(rec);

  indexBuf.writeUInt32LE(hashWord(e.word), i * 8);
  indexBuf.writeUInt32LE(offset, i * 8 + 4);
  offset += rec.length;
}

const header = Buffer.alloc(HEADER);
header.write("ECDICT01", 0, "ascii");
header.writeUInt32LE(list.length, 8);
header.writeUInt32LE(HEADER, 12);
header.writeUInt32LE(dataStart, 16);

console.log("写出词典文件…");
const fd = fs.openSync(outPath, "w");
fs.writeSync(fd, header);
fs.writeSync(fd, indexBuf);
for (const c of chunks) fs.writeSync(fd, c);
fs.closeSync(fd);

const size = fs.statSync(outPath).size;
console.log(`✅ 完成: ${outPath}`);
console.log(`   单词 ${list.length} 条，体积 ${(size / 1024 / 1024).toFixed(1)} MB`);
