// 独立验证 dict.bin：用和 Rust 完全相同的算法（FNV-1a hash + 索引二分）查一批词。
// 目的：在编译 GUI 之前就确认"词典文件和索引是好的"，把问题挡在应用层之外。
//
// 用法: node verify_dict.mjs [dict.bin] [词1 词2 ...]

import fs from "node:fs";

const dictPath = process.argv[2] || "dict.bin";
const words = process.argv.slice(3);
const probes = words.length ? words : ["boat", "among", "code", "implementation", "refactor", "compile"];

const buf = fs.readFileSync(dictPath);
if (buf.length < 20 || buf.toString("ascii", 0, 8) !== "ECDICT01") {
  console.error("❌ 魔数不对，这不是我们的词典文件");
  process.exit(1);
}
const count = buf.readUInt32LE(8);
const indexOff = buf.readUInt32LE(12);
const dataOff = buf.readUInt32LE(16);
console.log(`词典: ${dictPath}`);
console.log(`  条目数 ${count.toLocaleString()}   索引区 @${indexOff}   数据区 @${dataOff}   文件 ${(buf.length / 1024 / 1024).toFixed(1)} MB`);

function hashWord(s) {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h >>> 0;
}

function lookup(word) {
  const key = word.trim().toLowerCase();
  const target = hashWord(key);
  let lo = 0;
  let hi = count;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    const h = buf.readUInt32LE(indexOff + mid * 8);
    if (h < target) lo = mid + 1;
    else hi = mid;
  }
  for (let i = lo; i < count; i++) {
    const base = indexOff + i * 8;
    const h = buf.readUInt32LE(base);
    if (h !== target) break;
    let p = buf.readUInt32LE(base + 4);
    const wl = buf.readUInt16LE(p); p += 2;
    const w = buf.toString("utf8", p, p + wl); p += wl;
    const pl = buf.readUInt16LE(p); p += 2;
    const phon = buf.toString("utf8", p, p + pl); p += pl;
    const tl = buf.readUInt16LE(p); p += 2;
    const trans = buf.toString("utf8", p, p + tl); p += tl;
    const flags = buf.readUInt8(p);
    if (w === key) {
      return { word: w, phonetic: phon, translation: trans, collins: flags & 7, oxford: (flags & 8) !== 0 };
    }
  }
  return null;
}

let hit = 0;
for (const w of probes) {
  const t0 = process.hrtime.bigint();
  const r = lookup(w);
  const t1 = process.hrtime.bigint();
  const us = Number(t1 - t0) / 1000;
  if (r) {
    hit++;
    const star = "★".repeat(r.collins) || "-";
    console.log(`\n✅ ${w}  [${us.toFixed(0)}μs]  ${r.phonetic ? "/" + r.phonetic + "/" : ""}  ${star}${r.oxford ? " 牛津核心" : ""}`);
    console.log("   " + r.translation.replace(/\n/g, "\n   "));
  } else {
    console.log(`\n❌ ${w}  [${us.toFixed(0)}μs]  词典里没有`);
  }
}
console.log(`\n命中 ${hit}/${probes.length}`);
