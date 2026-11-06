const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const titleEl = document.querySelector("#title");
const srcEl = document.querySelector("#src");
const dstEl = document.querySelector("#dst");
const partsEl = document.querySelector("#parts");
const sensesEl = document.querySelector("#senses");
const errEl = document.querySelector("#err");
const hintEl = document.querySelector("#hint");
const wrapEl = document.querySelector("#wrap");
let hideTimer = null;

// 量出内容自然宽度/高度 → 让 Rust 侧把弹窗贴合到内容（短译窄框、长文宽框）
function maxLineWidth(text) {
  const meas = document.createElement("span");
  meas.style.cssText =
    "position:absolute;visibility:hidden;white-space:pre-wrap;word-break:break-word;width:max-content;font:inherit;";
  meas.textContent = text || "";
  document.body.appendChild(meas);
  const w = meas.offsetWidth;
  meas.remove();
  return w;
}

// 代码模式是"两列"布局，自然宽度要按各列之和估，不能只看最长的一行
function partsNaturalWidth() {
  let best = 0;
  for (const el of partsEl.querySelectorAll(".part")) {
    const raw = el.querySelector(".raw");
    const meaning = el.querySelector(".meaning");
    const w =
      (raw ? raw.offsetWidth : 0) +
      (meaning ? meaning.offsetWidth : 0) +
      40; /* 箭头 + 间距 */
    best = Math.max(best, w);
  }
  return best;
}

// 词典模式也是"两列"：词性列 + 含义列
function sensesNaturalWidth() {
  let best = 0;
  for (const el of sensesEl.querySelectorAll(".sense")) {
    const pos = el.querySelector(".pos");
    const meanings = el.querySelector(".meanings");
    const w = (pos ? pos.offsetWidth : 0) + (meanings ? meanings.offsetWidth : 0) + 24;
    best = Math.max(best, w);
  }
  return best;
}

function autoFit() {
  const natural = Math.max(
    maxLineWidth(titleEl.textContent),
    maxLineWidth(srcEl.textContent),
    maxLineWidth(dstEl.textContent),
    maxLineWidth(hintEl.textContent),
    maxLineWidth(errEl.textContent),
    partsEl.childElementCount ? partsNaturalWidth() : 0,
    sensesEl.childElementCount ? sensesNaturalWidth() : 0
  );
  const width = Math.min(Math.max(Math.ceil(natural) + 30, 150), 440);
  // 先按目标宽度撑一下，再量真实高度
  const savedMax = wrapEl.style.maxWidth;
  wrapEl.style.maxWidth = width + "px";
  const height = Math.min(Math.max(wrapEl.scrollHeight + 6, 52), 480);
  wrapEl.style.maxWidth = savedMax;
  // 窗口 = 卡片，不需要额外留白（不透明方案）
  invoke("popup_resize", { width, height }).catch(() => {});
}

function resetHide(ms) {
  if (hideTimer) clearTimeout(hideTimer);
  hideTimer = setTimeout(() => invoke("popup_hide"), ms || 6000);
}

function setHint(text) {
  hintEl.textContent = text || "";
  hintEl.style.display = text ? "block" : "none";
}

function show(el, on) {
  el.style.display = on ? "" : "none";
}

function renderParts(parts) {
  partsEl.innerHTML = "";
  for (const p of parts || []) {
    const row = document.createElement("div");
    row.className = "part";

    const raw = document.createElement("span");
    raw.className = "raw";
    raw.textContent = p.raw || "";

    const arrow = document.createElement("span");
    arrow.className = "arrow";
    arrow.textContent = "→";

    const meaning = document.createElement("span");
    meaning.className = "meaning";
    meaning.textContent = p.meaning || "";

    row.append(raw, arrow, meaning);

    if (p.note) {
      const note = document.createElement("span");
      note.className = "note";
      note.textContent = p.note;
      row.append(note);
    }
    partsEl.append(row);
  }
}

/// 词典模式：标题 = 单词，下面每行一个词性 + 它的若干含义。
/// 本地词典（ECDICT）还会带来音标和柯林斯星级，一并显示。
function renderSenses(word, senses, p) {
  sensesEl.innerHTML = "";
  const meta = [];
  if (p && p.phonetic) meta.push("[" + p.phonetic + "]");
  if (p && p.collins) meta.push("★".repeat(p.collins));
  if (p && p.oxford) meta.push("牛津核心");
  if (meta.length) {
    const m = document.createElement("div");
    m.className = "dict-meta";
    m.textContent = meta.join("   ");
    sensesEl.append(m);
  }
  for (const s of senses || []) {
    const row = document.createElement("div");
    row.className = "sense";

    if (s.pos) {
      const pos = document.createElement("span");
      pos.className = "pos";
      pos.textContent = s.pos;
      row.append(pos);
    }

    const meanings = document.createElement("span");
    meanings.className = "meanings";
    // 同一词性的多个含义用中文分号连着排，和有道那种词典的观感一致
    meanings.textContent = (s.meanings || []).join("；");
    row.append(meanings);

    sensesEl.append(row);
  }
}

listen("popup:show", (ev) => {
  const p = ev.payload || {};
  const isCode = Array.isArray(p.parts) && p.parts.length > 0;
  const isDict = Array.isArray(p.senses) && p.senses.length > 0;

  if (p.loading) {
    show(errEl, false);
    errEl.textContent = "";
    show(partsEl, false);
    show(sensesEl, false);
    show(titleEl, false);
    show(dstEl, true);
    show(srcEl, true);
    srcEl.textContent = p.source || "";
    dstEl.textContent = "翻译中…";
    dstEl.style.color = "var(--muted)"; // 灰色而不是醒目的蓝色，减少跳动感
    setHint("");
    // 长文分多次请求时可能超过 8 秒。之前这里也是 8 秒，
    // 结果"翻译中"还没结束弹窗就自己关了，用户以为没反应。
    resetHide(45000);
    setTimeout(autoFit, 30);
    return;
  }

  dstEl.style.color = "var(--text)";

  if (p.error && p.error.trim()) {
    // 报错时清掉上一轮的旧内容，避免"脏文本"
    show(srcEl, false);
    srcEl.textContent = "";
    show(titleEl, false);
    show(partsEl, false);
    show(sensesEl, false);
    show(dstEl, false);
    dstEl.textContent = "";
    show(errEl, true);
    errEl.textContent = p.error;
    setHint("");
  } else if (isDict) {
    // 词典模式：标题 = 单词，下面每行一个词性 + 若干含义
    show(srcEl, false);
    srcEl.textContent = "";
    show(errEl, false);
    errEl.textContent = "";
    show(partsEl, false);
    show(dstEl, false);
    dstEl.textContent = "";
    show(titleEl, true);
    titleEl.textContent = p.title || "";
    show(sensesEl, true);
    renderSenses(p.title, p.senses, p);
    setHint(dictHint(p));
  } else if (isCode) {
    // 代码模式：标题 = 原始标识符，下面按"子词 → 含义"列出来
    show(srcEl, false);
    srcEl.textContent = "";
    show(errEl, false);
    errEl.textContent = "";
    show(sensesEl, false);
    show(dstEl, false);
    dstEl.textContent = "";
    show(titleEl, true);
    titleEl.textContent = p.title || "";
    show(partsEl, true);
    renderParts(p.parts);
    setHint(codeHint(p));
  } else {
    show(srcEl, false);
    srcEl.textContent = "";
    show(errEl, false);
    errEl.textContent = "";
    show(titleEl, false);
    show(partsEl, false);
    partsEl.innerHTML = "";
    show(sensesEl, false);
    show(dstEl, true);
    dstEl.textContent = p.translated || "（无结果）";
    setHint(textHint(p));
  }

  resetHide(6000);
  setTimeout(autoFit, 30);
});

/// 词典模式的底部提示
function dictHint(p) {
  const parts = [p.engineLabel || "本地词典（离线）"];
  if (p.cached) parts.push(`⚡ 查过 ${p.hitCount || 1} 次`);
  if (p.elapsedMs > 800) parts.push(`耗时 ${(p.elapsedMs / 1000).toFixed(1)}s`);
  return parts.join(" · ");
}

/// 普通翻译：**永远告诉用户这句是谁翻的**。
/// 之前只有"回退"时才提示，正常走本地模型/DeepSeek 时一片空白，
/// 用户根本分不清是本地算的还是调用了云端。
function textHint(p) {
  const parts = [];
  if (p.cached) {
    parts.push(`⚡ 缓存命中（查过 ${p.hitCount || 1} 次）`);
  } else if (p.engineLabel) {
    parts.push(`翻译源：${p.engineLabel}`);
  }
  if (p.elapsedMs > 800) parts.push(`耗时 ${(p.elapsedMs / 1000).toFixed(1)}s`);
  return parts.join(" · ");
}

function codeHint(p) {
  const parts = ["按标识符拆词解释"];
  if (p.cached) parts.push(`⚡ 缓存命中（查过 ${p.hitCount || 1} 次）`);
  else if (p.engineLabel) parts.push(`翻译源：${p.engineLabel}`);
  if (p.elapsedMs > 800) parts.push(`耗时 ${(p.elapsedMs / 1000).toFixed(1)}s`);
  return parts.join(" · ");
}

document.addEventListener("mousedown", () => invoke("popup_hide"));

// 注：这里曾经加过"鼠标一动就收起浮窗"，但用户反馈那样不好——
// 正确的心智模型是"选中文字后，鼠标移回选区上方才弹翻译"，
// 弹出来之后应该老老实实待着让人看完。关闭方式：鼠标离开选区、
// 点击空白处、点击弹窗自身、或超时。

// ---- 主题：和主窗口共用同一份设置，切换后立刻生效 ----
function applyPopupTheme(t) {
  const light =
    t === "light" ||
    (t === "auto" && window.matchMedia("(prefers-color-scheme: light)").matches);
  document.documentElement.dataset.theme = light ? "light" : "dark";
}
(async () => {
  try {
    applyPopupTheme(await invoke("get_theme"));
  } catch (e) {}
})();
listen("theme:changed", (ev) => applyPopupTheme(ev.payload));