const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const sourceEl = document.querySelector("#source");
const resultEl = document.querySelector("#result");
const statusEl = document.querySelector("#status");
const btnEl = document.querySelector("#translate-btn");
const dirEl = document.querySelector("#direction");
const srcLangEl = document.querySelector("#src-lang");
const settingsBtn = document.querySelector("#settings-btn");
const backBtn = document.querySelector("#back-btn");
const mainView = document.querySelector("#main-view");
const settingsView = document.querySelector("#settings");
const engineEl = document.querySelector("#engine");
const boxBtn = document.querySelector("#box-btn");
const hotkeyEl = document.querySelector("#hotkey");
const hotkeySaveEl = document.querySelector("#hotkey-save");
const windowHotkeyEl = document.querySelector("#window-hotkey");
const windowHotkeySaveEl = document.querySelector("#window-hotkey-save");
const copyResultEl = document.querySelector("#copy-result");

async function doTranslate(text) {
  const t = (text || "").trim();
  if (!t) return;
  sourceEl.value = t;
  statusEl.textContent = "翻译中…";
  statusEl.classList.add("busy");
  try {
    const res = await invoke("translate", { text: t, target: dirEl.value });
    resultEl.value = res.translated;
    statusEl.classList.remove("busy");
    if (res.source === "empty") {
      statusEl.textContent = "没有内容";
      srcLangEl.textContent = "";
      return;
    }
    // 方向以"后端实际用的 target"为准：选了自动时，前端自己猜不准
    const dir = res.target === "en" ? "中译英" : "英译中";
    statusEl.textContent = `完成（${res.source} → ${dir}）`;
    srcLangEl.textContent = dir;
  } catch (e) {
    statusEl.classList.remove("busy");
    resultEl.value = "";
    statusEl.textContent = "翻译失败：" + e;
    srcLangEl.textContent = "";
  }
}

// ---- 设置：引擎列表 + 当前选择 ----
async function loadSettings() {
  const engines = await invoke("list_engines");
  engineEl.innerHTML = "";
  for (const e of engines) {
    const opt = document.createElement("option");
    opt.value = e.id;
    opt.textContent = e.label + (e.is_default ? "（默认）" : "");
    engineEl.appendChild(opt);
  }
  const current = await invoke("get_engine");
  if (current) engineEl.value = current;
}

settingsBtn.addEventListener("click", () => {
  mainView.style.display = "none";
  settingsView.hidden = false;
});
backBtn.addEventListener("click", () => {
  settingsView.hidden = true;
  mainView.style.display = "";
});
engineEl.addEventListener("change", async () => {
  const id = engineEl.value;
  try {
    const ok = await invoke("set_engine", { id });
    statusEl.textContent = `已切换翻译引擎：${ok}`;
  } catch (e) {
    statusEl.textContent = "切换引擎失败：" + e;
  }
});

// ---- 快捷键：录制 + 保存（翻译 / 弹主界面 两个）----
const HOTKEY_MODS = new Set(["Control", "Alt", "Shift", "Meta"]);
let recordingTarget = null; // 当前正在录制组合键的输入框

async function loadHotkey(field, cmd) {
  try {
    field.value = await invoke(cmd);
  } catch (e) {}
}

function bindRecorder(field, saveBtn, getCmd, setCmd) {
  field.addEventListener("focus", () => {
    recordingTarget = field;
    field.value = "";
    field.placeholder = "按下想用的组合键…";
  });
  field.addEventListener("blur", () => {
    if (recordingTarget === field) recordingTarget = null;
    field.placeholder = "点击后直接按组合键";
    if (!field.value) loadHotkey(field, getCmd);
  });
  saveBtn.addEventListener("click", async () => {
    const v = field.value.trim();
    if (!v) return;
    try {
      field.value = await invoke(setCmd, { hotkey: v });
      statusEl.textContent = `已更新快捷键：${field.value}`;
    } catch (e) {
      statusEl.textContent = "更新失败：" + e;
    }
  });
}

// 录制：捕获按下的组合键（忽略单独的修饰键）
document.addEventListener("keydown", (e) => {
  if (!recordingTarget) return;
  e.preventDefault();
  e.stopPropagation();
  if (HOTKEY_MODS.has(e.key)) return;
  const mods = [];
  if (e.ctrlKey) mods.push("ctrl");
  if (e.altKey) mods.push("alt");
  if (e.shiftKey) mods.push("shift");
  if (e.metaKey) mods.push("win");
  const key = keyName(e.key);
  if (!key) return;
  recordingTarget.value = [...mods, key].join("+");
  recordingTarget = null;
  recordingTarget.blur();
});

function keyName(k) {
  if (/^[a-z]$/i.test(k)) return k.toLowerCase();
  if (/^[0-9]$/.test(k)) return k;
  if (/^f\d{1,2}$/i.test(k)) return k.toLowerCase();
  switch (k) {
    case ",": return "comma";
    case ".": return "period";
    case " ": return "space";
    case "Enter": return "enter";
    case "Tab": return "tab";
    default: return null;
  }
}

bindRecorder(hotkeyEl, hotkeySaveEl, "get_hotkey", "set_hotkey");
bindRecorder(windowHotkeyEl, windowHotkeySaveEl, "get_window_hotkey", "set_window_hotkey");

// ---- 云端模型（DeepSeek）----
const cloudKeyEl = document.querySelector("#cloud-key");
const cloudModelEl = document.querySelector("#cloud-model");
const cloudSaveEl = document.querySelector("#cloud-save");
const cloudTestEl = document.querySelector("#cloud-test");
const cloudStatusEl = document.querySelector("#cloud-status");

async function loadCloudConfig() {
  try {
    const cfg = await invoke("get_local_config");
    // key 不回显明文太长的内容时也照实填，方便用户确认有没有存上
    cloudKeyEl.value = cfg.api_key || "";
    cloudModelEl.value = cfg.cloud_model || "deepseek-chat";
  } catch (e) {}
}

cloudSaveEl.addEventListener("click", async () => {
  try {
    await invoke("set_cloud_config", {
      apiKey: cloudKeyEl.value.trim(),
      cloudModel: cloudModelEl.value.trim(),
    });
    cloudStatusEl.textContent = "✅ 已保存。到上面「翻译引擎」切换成 DeepSeek 云端即可。";
  } catch (e) {
    cloudStatusEl.textContent = "保存失败：" + e;
  }
});

cloudTestEl.addEventListener("click", async () => {
  cloudStatusEl.textContent = "测试中…";
  try {
    const out = await invoke("test_cloud");
    cloudStatusEl.textContent = `✅ 可用，测试翻译结果：${out}`;
  } catch (e) {
    cloudStatusEl.textContent = "❌ " + e;
  }
});

// ---- 外观主题：auto / dark / light ----
const themeBtns = ["theme-auto", "theme-dark", "theme-light"].map((id) => document.querySelector("#" + id));

function applyTheme(t) {
  const light =
    t === "light" ||
    (t === "auto" && window.matchMedia("(prefers-color-scheme: light)").matches);
  document.documentElement.dataset.theme = light ? "light" : "dark";
  themeBtns.forEach((b) => {
    if (!b) return;
    const v = b.id.replace("theme-", "");
    const active = v === t;
    b.classList.toggle("on", active);
  });
}

async function loadTheme() {
  try {
    applyTheme(await invoke("get_theme"));
  } catch (e) {}
}

themeBtns.forEach((b) => {
  if (!b) return;
  b.addEventListener("click", async () => {
    const v = b.id.replace("theme-", "");
    try {
      await invoke("set_theme", { value: v });
      applyTheme(v);
    } catch (e) {}
  });
});

// 跟随系统时，系统切换深浅色要实时响应
if (window.matchMedia) {
  window.matchMedia("(prefers-color-scheme: light)").addEventListener?.("change", async () => {
    try {
      applyTheme(await invoke("get_theme"));
    } catch (e) {}
  });
}
// ---- 热键注册状态（注册失败时明确告诉用户，而不是"按了没反应"）----
const hotkeyStatusEl = document.querySelector("#hotkey-status");
async function refreshHotkeyStatus() {
  try {
    const msg = await invoke("get_hotkey_status");
    if (!hotkeyStatusEl) return;
    if (msg && msg !== "热键已注册") {
      hotkeyStatusEl.textContent = "⚠ " + msg + "（多半被别的软件占用了，换一个组合键）";
    } else if (msg) {
      hotkeyStatusEl.textContent = "✅ " + msg;
    }
  } catch (e) {}
}

// ---- 翻译开关：总开关 + 中文也翻译 ----
const enabledToggleEl = document.querySelector("#enabled-toggle");
const cnToggleEl = document.querySelector("#cn-toggle");
const autoToggleEl = document.querySelector("#auto-toggle");

async function refreshSwitches() {
  try {
    const en = await invoke("get_translate_enabled");
    enabledToggleEl.classList.toggle("on", en);
    enabledToggleEl.textContent = en ? "划词翻译：已开启" : "划词翻译：已关闭";
    const cn = await invoke("get_cn_ok");
    cnToggleEl.classList.toggle("on", cn);
    cnToggleEl.textContent = cn ? "中文也翻译：已开启" : "中文也翻译：已关闭";
    const au = await invoke("get_auto_popup");
    autoToggleEl.classList.toggle("on", au);
    autoToggleEl.textContent = au ? "选中自动弹窗：已开启" : "选中自动弹窗：已关闭";
  } catch (e) {}
}

enabledToggleEl.addEventListener("click", async () => {
  try {
    const cur = await invoke("get_translate_enabled");
    await invoke("set_translate_enabled", { value: !cur });
    refreshSwitches();
    loadTheme();
    loadVersion();
    loadTargetLang();
    refreshCacheToggle();
    refreshBaidu();
    loadQuickEngine();
    loadDirection();
    refreshStartup();
    refreshHotkeyStatus();
  } catch (e) {}
});

autoToggleEl.addEventListener("click", async () => {
  try {
    const cur = await invoke("get_auto_popup");
    await invoke("set_auto_popup", { value: !cur });
    refreshSwitches();
  } catch (e) {}
});

cnToggleEl.addEventListener("click", async () => {
  try {
    const cur = await invoke("get_cn_ok");
    await invoke("set_cn_ok", { value: !cur });
    refreshSwitches();
    loadTheme();
    loadVersion();
    loadTargetLang();
    refreshCacheToggle();
    refreshBaidu();
    loadQuickEngine();
    loadDirection();
    refreshStartup();
    refreshHotkeyStatus();
  } catch (e) {}
});

// ---- 本地模型（OpenAI 兼容端点）----
const localUrlEl = document.querySelector("#local-url");
const localModelEl = document.querySelector("#local-model");
const localSaveEl = document.querySelector("#local-save");
const localProbeEl = document.querySelector("#local-probe");
const localStatusEl = document.querySelector("#local-status");
const modelPickEl = document.querySelector("#local-model-pick");
const localModelUseEl = document.querySelector("#local-model-use");

async function loadLocalConfig() {
  try {
    const cfg = await invoke("get_local_config");
    localUrlEl.value = cfg.base_url || "";
    localModelEl.value = cfg.model || "";
  } catch (e) {}
}

localSaveEl.addEventListener("click", async () => {
  try {
    const cfg = await invoke("set_local_config", {
      baseUrl: localUrlEl.value.trim(),
      model: localModelEl.value.trim(),
    });
    localStatusEl.textContent = `已保存：${cfg.base_url} / ${cfg.model}`;
  } catch (e) {
    localStatusEl.textContent = "保存失败：" + e;
  }
});

localProbeEl.addEventListener("click", async () => {
  localStatusEl.textContent = "测试中…";
  try {
    const models = await invoke("probe_local");
    // 把已下载的模型填进下拉框，省得手打模型名
    modelPickEl.innerHTML = "";
    for (const m of models || []) {
      const opt = document.createElement("option");
      opt.value = m;
      opt.textContent = m;
      modelPickEl.appendChild(opt);
    }
    if (!models || models.length === 0) {
      localStatusEl.textContent = "服务在线，但没有已下载的模型。先执行：ollama pull qwen3:1.7b";
    } else {
      const want = localModelEl.value.trim();
      const has = models.includes(want);
      localStatusEl.textContent = has
        ? `✅ 连接成功，模型「${want}」可用。共 ${models.length} 个模型。`
        : `连接成功，但没有模型「${want}」。可用：${models.slice(0, 5).join("、")}`;
    }
  } catch (e) {
    localStatusEl.textContent =
      "❌ 连不上。确认已安装并启动 Ollama（或在设置里填对地址）。详情：" + e;
  }
});

// 从下拉框直接切换模型，不用手打、也不用重新构建
localModelUseEl.addEventListener("click", async () => {
  const m = modelPickEl.value;
  if (!m) {
    localStatusEl.textContent = "先点「测试连接」拉取模型列表";
    return;
  }
  try {
    const ok = await invoke("set_local_model", { model: m });
    localModelEl.value = ok;
    localStatusEl.textContent = `✅ 已切换到模型「${ok}」，下一次翻译生效`;
  } catch (e) {
    localStatusEl.textContent = "切换失败：" + e;
  }
});

// ---- 划词/复制行为开关 ----
const hoverToggleEl = document.querySelector("#hover-toggle");
const copyToggleEl = document.querySelector("#copy-toggle");

async function refreshToggles() {
  try {
    const hover = await invoke("get_require_hover");
    hoverToggleEl.classList.toggle("on", hover);
  hoverToggleEl.textContent = hover
      ? "触发方式：鼠标移回选区才翻译"
      : "触发方式：选中即翻译（推荐）";
  } catch (e) {}
  try {
    const copy = await invoke("get_copy_translate");
    copyToggleEl.classList.toggle("on", copy);
  copyToggleEl.textContent = copy ? "复制即译：已开启" : "复制即译：已关闭";
  } catch (e) {}
}

hoverToggleEl.addEventListener("click", async () => {
  try {
    const cur = await invoke("get_require_hover");
    await invoke("set_require_hover", { value: !cur });
    refreshToggles();
  } catch (e) {}
});

copyToggleEl.addEventListener("click", async () => {
  try {
    const cur = await invoke("get_copy_translate");
    await invoke("set_copy_translate", { enabled: !cur });
    refreshToggles();
    refreshMonitor();
  } catch (e) {}
});

// ---- 复制即译监听 + 缓存 ----
const monitorToggleEl = document.querySelector("#monitor-toggle");
const cacheClearEl = document.querySelector("#cache-clear");
const cacheStatusEl = document.querySelector("#cache-status");

async function refreshMonitor() {
  try {
    const paused = await invoke("get_monitor_paused");
    monitorToggleEl.classList.toggle("on", !paused);
  monitorToggleEl.textContent = paused ? "已暂停（点此恢复）" : "监听中（点此暂停）";
    const n = await invoke("cache_size");
    cacheStatusEl.textContent = paused
      ? `⏸ 监听已暂停（复制不会翻译）。本地缓存 ${n} 条。`
      : `复制任何文字都会自动翻译。复制密码等敏感内容前请先「暂停监听」。本地缓存 ${n} 条。`;
  } catch (e) {}
}

monitorToggleEl.addEventListener("click", async () => {
  try {
    const paused = await invoke("get_monitor_paused");
    await invoke("set_monitor_paused", { paused: !paused });
    refreshMonitor();
  } catch (e) {}
});

cacheClearEl.addEventListener("click", async () => {
  try {
    await invoke("clear_cache");
    refreshMonitor();
    statusEl.textContent = "翻译缓存已清空";
  } catch (e) {}
});

// ---- MyMemory 邮箱（免费提额）----
const mmEmailEl = document.querySelector("#mm-email");
const mmEmailSaveEl = document.querySelector("#mm-email-save");
async function loadMmEmail() {
  try {
    mmEmailEl.value = await invoke("get_mymemory_email");
  } catch (e) {}
}
mmEmailSaveEl.addEventListener("click", async () => {
  try {
    const ok = await invoke("set_mymemory_email", { email: mmEmailEl.value.trim() });
    mmEmailEl.value = ok;
    statusEl.textContent = "已保存 MyMemory 邮箱（额度已提升）";
  } catch (e) {
    statusEl.textContent = "保存失败：" + e;
  }
});

// 主窗口译文复制按钮
copyResultEl.addEventListener("click", async () => {
  const t = resultEl.value || "";
  if (!t) return;
  try {
    await invoke("copy_to_clipboard", { text: t });
    copyResultEl.textContent = "已复制 ✓";
    setTimeout(() => (copyResultEl.textContent = "复制"), 1200);
  } catch (e) {
    copyResultEl.textContent = "复制失败";
  }
});

// ---- 框选翻译 ----
boxBtn.addEventListener("click", async () => {
  try {
    await invoke("start_box");
  } catch (e) {
    statusEl.textContent = "打开框选失败：" + e;
  }
});

listen("box:result", (event) => {
  const r = event?.payload || {};
  sourceEl.value = r.raw || "";
  resultEl.value = r.translated || "";
  if (r.error) {
    statusEl.textContent = "框选翻译失败：" + r.error;
    srcLangEl.textContent = "";
  } else {
    const dir2 = r.target === "en" ? "中译英" : "英译中";
    statusEl.textContent = r.source ? `框选完成（${r.source} → ${dir2}）` : "框选完成";
    srcLangEl.textContent = r.target === "en" ? "中译英" : "外语→中文";
  }
});

// ---- 手动输入 ----
btnEl.addEventListener("click", () => doTranslate(sourceEl.value));

// 原文框里直接输入时：Ctrl+Enter 翻译，裸 Enter 保留换行（多行文本很常见）
sourceEl.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
    e.preventDefault();
    doTranslate(sourceEl.value);
  }
});

// ---- 清除输入（翻下一句不用退格删）----
const clearBtn = document.querySelector("#clear-btn");
clearBtn.addEventListener("click", () => {
  sourceEl.value = "";
  resultEl.value = "";
  srcLangEl.textContent = "";
  statusEl.textContent = "";
  sourceEl.focus();
});

// ---- 点击主界面空白处最小化窗口 ----
mainView.addEventListener("click", (e) => {
  const tag = e.target.tagName.toLowerCase();
  if (["input", "button", "textarea", "select", "label"].includes(tag)) return;
  invoke("hide_window").catch(() => {});
});

// init
loadSettings().catch((e) => {
  engineEl.innerHTML = `<option>加载设置失败: ${e}</option>`;
});
loadHotkey(hotkeyEl, "get_hotkey");
loadHotkey(windowHotkeyEl, "get_window_hotkey");
loadMmEmail();
loadLocalConfig();
loadCloudConfig();
refreshMonitor();
refreshToggles();
refreshSwitches();
    loadTheme();
    loadVersion();
    loadTargetLang();
    refreshCacheToggle();
    refreshBaidu();
    loadQuickEngine();
    loadDirection();
    refreshStartup();
refreshHotkeyStatus();

// ---- 关于：版本号 + 检查更新 ----
async function loadVersion() {
  try {
    const v = await invoke("get_version");
    const pill = document.querySelector("#version-pill");
    if (pill) pill.textContent = "版本 " + v;
  } catch (e) {}
}

document.querySelector("#check-update")?.addEventListener("click", async () => {
  try {
    await invoke("open_url", { url: "https://github.com/kaxfl/trans/releases/latest" });
  } catch (e) {
    statusEl.textContent = "打开浏览器失败：" + e;
  }
});

// ---- 目标语言 ----
async function loadTargetLang() {
  const sel = document.querySelector("#target-lang");
  if (!sel) return;
  try {
    sel.value = await invoke("get_target_lang");
  } catch (e) {}
}

document.querySelector("#target-lang")?.addEventListener("change", async (e) => {
  try {
    await invoke("set_target_lang", { value: e.target.value });
  } catch (err) {}
});

// ---- 本地缓存开关 ----
async function refreshCacheToggle() {
  const btn = document.querySelector("#cache-toggle");
  if (!btn) return;
  try {
    const on = await invoke("get_cache_on");
    btn.classList.toggle("on", on);
    btn.textContent = on ? "本地缓存：开启" : "本地缓存：关闭";
  } catch (e) {}
}

document.querySelector("#cache-toggle")?.addEventListener("click", async () => {
  const btn = document.querySelector("#cache-toggle");
  const cur = btn.classList.contains("on");
  try {
    const next = await invoke("set_cache_on", { value: !cur });
    btn.classList.toggle("on", next);
    btn.textContent = next ? "本地缓存：开启" : "本地缓存：关闭";
  } catch (e) {}
});

// ---- 百度翻译凭据 ----
async function refreshBaidu() {
  try {
    const ok = await invoke("get_baidu_configured");
    const st = document.querySelector("#baidu-status");
    if (st) st.textContent = ok ? "已配置" : "未配置";
  } catch (e) {}
}

document.querySelector("#baidu-save")?.addEventListener("click", async () => {
  const appid = document.querySelector("#baidu-appid").value.trim();
  const key = document.querySelector("#baidu-key").value.trim();
  const st = document.querySelector("#baidu-status");
  try {
    const ok = await invoke("set_baidu_cred", { appid, key });
    if (st) st.textContent = ok ? "已保存，可在「翻译引擎」里选百度翻译" : "已清空";
  } catch (e) {
    if (st) st.textContent = "保存失败：" + e;
  }
});

// ---- 顶栏：翻译方向 + 翻译源 ----


function syncDirection(value) {
  if (dirEl && dirEl.value !== value) dirEl.value = value;
  const tl = document.querySelector("#target-lang");
  if (tl && tl.value !== value) tl.value = value;
}

async function loadQuickEngine() {
  const quickEngineEl = document.querySelector("#engine-quick");
  if (!quickEngineEl) return;
  let engines = [];
  try {
    engines = await invoke("list_engines");
  } catch (e) {
    engines = [];
  }
  if (!Array.isArray(engines) || engines.length === 0) {
    engines = [
      { id: "baidu", label: "百度翻译" },
      { id: "local", label: "本地模型" },
      { id: "deepseek", label: "DeepSeek" },
      { id: "dict", label: "本地词典" },
      { id: "mymemory", label: "MyMemory" },
    ];
  }
  quickEngineEl.innerHTML = "";
  for (const e of engines) {
    const opt = document.createElement("option");
    opt.value = e.id;
    opt.textContent =
      String(e.label || e.id).replace(/\s*[（(].*?[)）]\s*/g, "").trim() || e.id;
    quickEngineEl.appendChild(opt);
  }
  // 必须有选中项，否则下拉框显示成空白
  let cur = "";
  try {
    cur = await invoke("get_engine");
  } catch (e) {}
  if (cur && engines.some((e) => e.id === cur)) {
    quickEngineEl.value = cur;
  } else if (quickEngineEl.options.length > 0) {
    quickEngineEl.selectedIndex = 0;
  }
}

document.querySelector("#engine-quick")?.addEventListener("change", async (ev) => {
  const quickEngineEl = ev.target;
  const id = quickEngineEl.value;
  try {
    await invoke("set_engine", { id });
    if (engineEl) engineEl.value = id;
    statusEl.textContent = `翻译源：${quickEngineEl.selectedOptions[0].textContent}`;
  } catch (e) {
    statusEl.textContent = "切换翻译源失败：" + e;
  }
});

// 方向：顶栏和设置页共用同一个 target_lang
dirEl?.addEventListener("change", async () => {
  syncDirection(dirEl.value);
  try {
    await invoke("set_target_lang", { value: dirEl.value });
  } catch (e) {}
});

document.querySelector("#target-lang")?.addEventListener("change", async (e) => {
  syncDirection(e.target.value);
  try {
    await invoke("set_target_lang", { value: e.target.value });
  } catch (err) {}
});

async function loadDirection() {
  try {
    const v = await invoke("get_target_lang");
    syncDirection(v || "auto");
  } catch (e) {}
}

// ---- 启动：开机自启 + 最小化到托盘 ----
async function refreshStartup() {
  const a = document.querySelector("#autostart-toggle");
  const m = document.querySelector("#minimized-toggle");
  try {
    if (a) {
      const on = await invoke("get_autostart");
      a.classList.toggle("on", on);
      a.textContent = "开机自动启动：" + (on ? "开启" : "关闭");
    }
  } catch (e) {}
  try {
    if (m) {
      const on = await invoke("get_start_minimized");
      m.classList.toggle("on", on);
      m.textContent = "启动时最小化窗口：" + (on ? "开启" : "关闭");
    }
  } catch (e) {}
}

document.querySelector("#autostart-toggle")?.addEventListener("click", async (ev) => {
  const btn = ev.target;
  const cur = btn.classList.contains("on");
  try {
    const next = await invoke("set_autostart", { value: !cur });
    btn.classList.toggle("on", next);
    btn.textContent = "开机自动启动：" + (next ? "开启" : "关闭");
  } catch (e) {
    statusEl.textContent = "设置开机自启失败：" + e;
  }
});

document.querySelector("#minimized-toggle")?.addEventListener("click", async (ev) => {
  const btn = ev.target;
  const cur = btn.classList.contains("on");
  try {
    const next = await invoke("set_start_minimized", { value: !cur });
    btn.classList.toggle("on", next);
    btn.textContent = "启动时最小化窗口：" + (next ? "开启" : "关闭");
  } catch (e) {}
});
