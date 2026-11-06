const { invoke } = window.__TAURI__.core;

const dpr = window.devicePixelRatio || 1;
const sel = document.querySelector("#sel");
let sx = 0, sy = 0, dragging = false;

function draw(x0, y0, x1, y1) {
  const x = Math.min(x0, x1), y = Math.min(y0, y1);
  const w = Math.abs(x1 - x0), h = Math.abs(y1 - y0);
  Object.assign(sel.style, {
    left: x + "px",
    top: y + "px",
    width: w + "px",
    height: h + "px",
    display: "block",
  });
}

document.addEventListener("mousedown", (e) => {
  dragging = true;
  sx = e.clientX;
  sy = e.clientY;
});
document.addEventListener("mousemove", (e) => {
  if (dragging) draw(sx, sy, e.clientX, e.clientY);
});
document.addEventListener("mouseup", async (e) => {
  if (!dragging) return;
  dragging = false;
  const x = Math.min(sx, e.clientX), y = Math.min(sy, e.clientY);
  const w = Math.abs(e.clientX - sx), h = Math.abs(e.clientY - sy);
  if (w < 5 || h < 5) {
    await invoke("box_cancel");
    return;
  }
  try {
    // CSS 像素 → 物理像素（主显示器原点在 (0,0)）
    const text = await invoke("ocr_region", {
      x: Math.round(x * dpr),
      y: Math.round(y * dpr),
      width: Math.round(w * dpr),
      height: Math.round(h * dpr),
    });
    const res = await invoke("translate", { text, target: "zh-CN" });
    await invoke("box_done", { translated: res.translated, source: res.source, raw: text });
  } catch (err) {
    await invoke("box_done", { translated: "", source: "", raw: "", error: String(err) });
  }
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") invoke("box_cancel");
});
