# -*- coding: utf-8 -*-
"""
生成《划词翻译》参赛答辩 PPT（12 页，16:9）。
设计语言与产品一致：深蓝底 + 亮蓝发光，微软雅黑。
"""
import os
from pptx import Presentation
from pptx.util import Inches, Pt
from pptx.dml.color import RGBColor
from pptx.enum.text import PP_ALIGN, MSO_ANCHOR
from pptx.enum.shapes import MSO_SHAPE
from pptx.oxml.ns import qn
from PIL import Image

ASSETS = os.path.join(os.environ["TEMP"], "deck_assets")
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "划词翻译-答辩.pptx")

BG = "0B1220"
BG2 = "111C33"
ACCENT = "4A9EFF"
INK = "E8EEFC"
MUTED = "8B97AD"
DIM = "5D6A82"
LINE = "22314F"
FONT = "微软雅黑"

W, H = 13.333, 7.5
M = 0.72
CW = W - M * 2


def A(name):
    return os.path.join(ASSETS, name)


def rgb(h):
    return RGBColor.from_string(h)


def set_run(run, text, size=18, bold=False, color=INK, font=FONT):
    run.text = text
    f = run.font
    f.size = Pt(size)
    f.bold = bold
    f.color.rgb = rgb(color)
    f.name = font
    rPr = run._r.get_or_add_rPr()
    for tag in ("a:ea", "a:cs"):
        el = rPr.find(qn(tag))
        if el is None:
            el = rPr.makeelement(qn(tag), {})
            rPr.append(el)
        el.set("typeface", font)
    return run


def textbox(slide, left, top, width, height, align=PP_ALIGN.LEFT, anchor=MSO_ANCHOR.TOP):
    tb = slide.shapes.add_textbox(Inches(left), Inches(top), Inches(width), Inches(height))
    tf = tb.text_frame
    tf.word_wrap = True
    tf.margin_left = 0
    tf.margin_right = 0
    tf.margin_top = 0
    tf.margin_bottom = 0
    tf.vertical_anchor = anchor
    tf.paragraphs[0].alignment = align
    return tb, tf


def para(tf, text, size=18, bold=False, color=INK, first=False, space_before=0,
         space_after=0, align=PP_ALIGN.LEFT, line_spacing=None):
    p = tf.paragraphs[0] if first else tf.add_paragraph()
    p.alignment = align
    if space_before:
        p.space_before = Pt(space_before)
    if space_after:
        p.space_after = Pt(space_after)
    if line_spacing:
        p.line_spacing = line_spacing
    set_run(p.add_run(), text, size=size, bold=bold, color=color)
    return p


def slide_new(prs):
    s = prs.slides.add_slide(prs.slide_layouts[6])
    bg = s.shapes.add_shape(MSO_SHAPE.RECTANGLE, 0, 0, prs.slide_width, prs.slide_height)
    bg.fill.solid()
    bg.fill.fore_color.rgb = rgb(BG)
    bg.line.fill.background()
    bg.shadow.inherit = False
    return s


def band(slide, left, top, width, height, color=BG2, line=LINE):
    r = slide.shapes.add_shape(MSO_SHAPE.ROUNDED_RECTANGLE, Inches(left), Inches(top),
                               Inches(width), Inches(height))
    r.fill.solid()
    r.fill.fore_color.rgb = rgb(color)
    r.line.color.rgb = rgb(line)
    r.line.width = Pt(0.75)
    r.shadow.inherit = False
    r.adjustments[0] = 0.04
    return r


def accent_bar(slide, left, top, width=0.9, height=0.06, color=ACCENT):
    r = slide.shapes.add_shape(MSO_SHAPE.ROUNDED_RECTANGLE, Inches(left), Inches(top),
                               Inches(width), Inches(height))
    r.fill.solid()
    r.fill.fore_color.rgb = rgb(color)
    r.line.fill.background()
    r.shadow.inherit = False
    return r


def title(slide, main, sub=None, page=None):
    accent_bar(slide, M, 0.62)
    tb, tf = textbox(slide, M, 0.78, CW, 0.6)
    para(tf, main, size=29, bold=True, first=True)
    if sub:
        tb2, tf2 = textbox(slide, M, 1.42, CW, 0.4)
        para(tf2, sub, size=14, color=MUTED, first=True)
    ft, ftf = textbox(slide, M, H - 0.5, CW, 0.28)
    ftf.paragraphs[0].alignment = PP_ALIGN.RIGHT
    set_run(ftf.paragraphs[0].add_run(),
            f"U.N.Owen 译制组   ·   {page:02d}" if page else "U.N.Owen 译制组",
            size=10.5, color=DIM)


def fit_picture(slide, path, left, top, max_w, max_h, border=True, align="center"):
    with Image.open(path) as im:
        iw, ih = im.size
    scale = min(max_w / iw, max_h / ih)
    w, h = iw * scale, ih * scale
    x = left + (max_w - w) / 2 if align == "center" else left
    y = top + (max_h - h) / 2
    pic = slide.shapes.add_picture(path, Inches(x), Inches(y), Inches(w), Inches(h))
    if border:
        pic.line.color.rgb = rgb(LINE)
        pic.line.width = Pt(1)
    return pic


def bullets(slide, items, left, top, width, size=15, gap=16, height=4.5):
    """每条：标题一行 + 说明一行，两条之间留 gap。"""
    tb, tf = textbox(slide, left, top, width, height)
    first = True
    for it in items:
        if isinstance(it, tuple):
            head, desc = it
            p1 = tf.paragraphs[0] if first else tf.add_paragraph()
            first = False
            p1.line_spacing = 1.2
            p1.space_after = Pt(3)
            set_run(p1.add_run(), "▍ ", size=size, color=ACCENT, bold=True)
            set_run(p1.add_run(), head, size=size, bold=True, color=INK)
            p2 = tf.add_paragraph()
            p2.line_spacing = 1.3
            p2.space_after = Pt(gap)
            set_run(p2.add_run(), desc, size=size - 2, color=MUTED)
        else:
            p = tf.paragraphs[0] if first else tf.add_paragraph()
            first = False
            p.line_spacing = 1.3
            p.space_after = Pt(gap)
            set_run(p.add_run(), "▍ ", size=size, color=ACCENT, bold=True)
            set_run(p.add_run(), it, size=size, color=INK)
    return tf


# ============================================================
prs = Presentation()
prs.slide_width = Inches(W)
prs.slide_height = Inches(H)

# ---------- 01 封面 ----------
s = slide_new(prs)
glow = s.shapes.add_shape(MSO_SHAPE.OVAL, Inches(W / 2 - 4.2), Inches(-3.6), Inches(8.4), Inches(8.4))
glow.fill.solid()
glow.fill.fore_color.rgb = rgb("0F1A2E")
glow.line.fill.background()
glow.shadow.inherit = False
glow.element.getparent().remove(glow.element)
s.shapes._spTree.insert(2, glow.element)

logo = s.shapes.add_shape(MSO_SHAPE.ROUNDED_RECTANGLE, Inches(W / 2 - 0.78), Inches(1.02), Inches(1.56), Inches(1.56))
logo.fill.solid()
logo.fill.fore_color.rgb = rgb("101B31")
logo.line.color.rgb = rgb(ACCENT)
logo.line.width = Pt(1.25)
logo.shadow.inherit = False
logo.adjustments[0] = 0.24
ltf = logo.text_frame
ltf.word_wrap = False
ltf.vertical_anchor = MSO_ANCHOR.MIDDLE
ltf.paragraphs[0].alignment = PP_ALIGN.CENTER
set_run(ltf.paragraphs[0].add_run(), "> Trans_", size=17, bold=True, color=INK, font="Consolas")

tb, tf = textbox(s, 0.8, 2.95, W - 1.6, 1.0, align=PP_ALIGN.CENTER)
para(tf, "划词翻译", size=50, bold=True, first=True, align=PP_ALIGN.CENTER)

tb, tf = textbox(s, 0.8, 4.05, W - 1.6, 0.5, align=PP_ALIGN.CENTER)
para(tf, "在任意软件里选中文字，译文立刻出现在光标旁", size=19, color=MUTED,
     first=True, align=PP_ALIGN.CENTER)

tb, tf = textbox(s, 0.8, 5.5, W - 1.6, 1.2, align=PP_ALIGN.CENTER)
para(tf, "U.N.Owen 译制组", size=18, bold=True, first=True, align=PP_ALIGN.CENTER)
para(tf, "2026 山东省首届大学生 AI 大模型创新传播大赛 · 赛道三", size=13,
     color=MUTED, align=PP_ALIGN.CENTER, space_before=10)
para(tf, "2026.11", size=12, color=DIM, align=PP_ALIGN.CENTER, space_before=5)

# ---------- 02 问题 ----------
s = slide_new(prs)
title(s, "读英文代码时，时间到底浪费在哪", "不是看不懂，是每查一个词都要中断一次", page=2)
fit_picture(s, A("anim_a1_c.png"), M, 1.95, 6.55, 4.0)
bullets(s, [
    ("切屏循环", "去查词，回来忘了读到哪"),
    ("重复劳动", "同一个词今天查了第五次"),
    ("场景受限", "游戏、图片、扫描件选不中"),
], 7.55, 2.05, 5.1, size=15, gap=24)

# ---------- 03 方案 ----------
s = slide_new(prs)
title(s, "选中即译：把中断压缩成一次选中", "全局鼠标钩子 + UIA + 剪贴板，三层取词降级", page=3)
fit_picture(s, A("anim_a2_c.png"), M, 1.9, 6.9, 4.1)
bullets(s, [
    ("任何软件都能用", "浏览器 / VSCode / IDEA / QQ / PDF"),
    ("移开就消失", "不挡视线，移回还能再翻一次"),
    ("三种触发", "悬停 · Ctrl+Alt+. · 框选"),
], 7.95, 2.0, 4.7, size=14, gap=20)

# ---------- 04 三级能力 ----------
s = slide_new(prs)
title(s, "同一个动作，按内容选不同的处理", "单词走词典、标识符走拆词、段落走模型", page=4)
for i, (img, label, cap) in enumerate([
    ("anim_a3_c.png", "单词 · 本地词典", "音标 + 词性 + 多条释义，26 微秒返回"),
    ("anim_a4_c.png", "标识符 · 拆词解释", "camelCase / snake_case 拆开逐词给含义"),
]):
    x = M + i * 6.02
    band(s, x, 1.9, 5.85, 4.35)
    fit_picture(s, A(img), x + 0.15, 2.0, 5.55, 3.15, border=True)
    tb, tf = textbox(s, x + 0.3, 5.35, 5.3, 0.8)
    para(tf, label, size=15, bold=True, first=True)
    para(tf, cap, size=11.5, color=MUTED, space_before=3)

# ---------- 05 代码块兜底 ----------
s = slide_new(prs)
title(s, "模型翻不动的时候，还有一层兜底", "自动降级为逐词查词典，符号与缩进一个字符都不改", page=5)
fit_picture(s, A("anim_a5_c.png"), M, 1.9, 7.3, 4.2)
bullets(s, [
    ("不是报错，是换招", "整句失败就逐词给结果"),
    ("符号零改动", "只有连续字母进入翻译流程"),
    ("查不到就留原文", "不编造释义"),
], 8.35, 2.0, 4.3, size=14, gap=22)

# ---------- 06 本地优先 ----------
s = slide_new(prs)
title(s, "本地优先：能离线就绝不上传", "四个翻译源，按可用性与场景自动调度", page=6)
rows = [
    ("本地词典", "ECDICT · 56.7 万词", "26 微秒", "离线"),
    ("本地模型", "Ollama · qwen3:1.7b", "0.5 秒 / 句", "离线"),
    ("DeepSeek 云端", "API（需 Key）", "约 1.0 秒 / 句", "需联网"),
    ("MyMemory", "免费接口", "1–3 秒 / 句", "兜底"),
]
shape = s.shapes.add_table(len(rows) + 1, 4, Inches(M), Inches(1.95), Inches(11.9), Inches(3.0))
table = shape.table
for i, wd in enumerate([3.0, 4.6, 2.3, 2.0]):
    table.columns[i].width = Inches(wd)
for j, htxt in enumerate(["翻译源", "具体实现", "单次耗时", "网络要求"]):
    cell = table.cell(0, j)
    cell.fill.solid()
    cell.fill.fore_color.rgb = rgb("16233D")
    cell.margin_left = Inches(0.14)
    cell.vertical_anchor = MSO_ANCHOR.MIDDLE
    set_run(cell.text_frame.paragraphs[0].add_run(), htxt, size=13, bold=True, color=ACCENT)
for i, row in enumerate(rows, start=1):
    for j, val in enumerate(row):
        cell = table.cell(i, j)
        cell.fill.solid()
        cell.fill.fore_color.rgb = rgb("0E1729" if i % 2 else "101B31")
        cell.margin_left = Inches(0.14)
        cell.vertical_anchor = MSO_ANCHOR.MIDDLE
        set_run(cell.text_frame.paragraphs[0].add_run(), val, size=13,
                bold=(j == 0), color=INK if j == 0 else MUTED)

tb, tf = textbox(s, M, 5.35, CW, 1.5)
para(tf, "设计原则：本地能力优先 —— 查词不联网、不占显存；只有整句翻译才调模型，且优先本地模型。",
     size=14, color=MUTED, first=True)
para(tf, "隐私：本地模型模式下内容不出本机；「复制即译」默认关闭，避免复制密码时被外发。",
     size=14, color=MUTED, space_before=9)

# ---------- 07 框选 OCR ----------
s = slide_new(prs)
title(s, "选不中的文字，用框选解决", "Ctrl + Alt + S 唤起遮罩，拖一个矩形：识别 → 翻译 → 回填", page=7)
fit_picture(s, A("ocr.png"), M, 1.95, 7.35, 4.1)
bullets(s, [
    ("游戏 / 图片 / 扫描件", "文字是画出来的，本来选不中"),
    ("系统 OCR 引擎", "WinRT 识别，中英文都支持"),
    ("一键闭环", "框完自动翻译并回填"),
], 8.4, 2.05, 4.2, size=13.5, gap=20)

# ---------- 08 产品完成度 ----------
s = slide_new(prs)
title(s, "这不只是一个演示原型", "完整安装包、深浅双主题、托盘与快捷键体系", page=8)
fit_picture(s, A("ui_main_c.png"), M, 1.9, 3.6, 4.25)
fit_picture(s, A("ui_set_c.png"), M + 3.9, 1.9, 3.6, 4.25)
bullets(s, [
    ("一键安装", "16.8 MB，内置词典，装完即用"),
    ("双主题", "深色 / 浅色 / 跟随系统"),
    ("快捷键", "翻译 · 总开关 · 自动弹窗 · 框选"),
], 8.55, 2.0, 4.1, size=14, gap=22)

# ---------- 09 工程验证 ----------
s = slide_new(prs)
title(s, "我们验证过它能被别人装上并用起来", "干净 Windows 11 虚拟机实测 + 关键指标", page=9)
cards = [
    ("16.8 MB", "安装包体积", "词典 35 MB 已内置"),
    ("0 配置", "装完即用", "不装 Ollama 也能查词"),
    ("56.7 万", "本地词条", "查词完全离线"),
    ("26 μs", "查词延迟", "对比调模型 1–3 秒"),
]
for i, (big, mid, small) in enumerate(cards):
    x = M + i * 3.02
    band(s, x, 1.9, 2.82, 1.7)
    tb, tf = textbox(s, x + 0.22, 2.06, 2.4, 1.45)
    para(tf, big, size=24, bold=True, color=ACCENT, first=True)
    para(tf, mid, size=12.5, bold=True, space_before=3)
    para(tf, small, size=10.5, color=MUTED, space_before=3)

tb, tf = textbox(s, M, 3.9, CW, 2.5)
para(tf, "工程细节", size=15, bold=True, color=ACCENT, first=True)
for t in [
    "安装包带 NSIS 脚本：自动创建桌面快捷方式，卸载时一并清理",
    "在干净 Windows 11 虚拟机里完整走通：安装 → 启动 → 查词 → 翻译",
    "热键注册失败会在设置页明确提示，而不是「按了没反应」",
    "UIA 取词加 700 ms 超时、剪贴板加哨兵值校验，避免卡死与误翻旧内容",
]:
    para(tf, "· " + t, size=13, color=MUTED, space_before=8)

# ---------- 10 传播 ----------
s = slide_new(prs)
title(s, "传播：评委打开链接就能看到全部", "在线主页 + 110 秒演示动画 + 开源仓库", page=10)
fit_picture(s, A("landing.png"), M, 1.9, 6.6, 4.2)
tb, tf = textbox(s, 7.65, 1.95, 5.0, 4.2)
para(tf, "kaxfl.github.io/trans", size=16, bold=True, color=ACCENT, first=True)
para(tf, "手机、电脑都能直接打开", size=11.5, color=MUTED, space_before=3)
para(tf, "动画：9 章节，自动循环播放", size=14, space_before=18)
para(tf, "不依赖真人操作，答辩现场不会翻车", size=11.5, color=MUTED, space_before=3)
para(tf, "仓库：源码 + 安装包 Release", size=14, space_before=16)
para(tf, "github.com/kaxfl/trans", size=11.5, color=MUTED, space_before=3)

# ---------- 11 技术架构 ----------
s = slide_new(prs)
title(s, "技术选型：为什么是 Rust + Tauri", "16 MB 的桌面程序，要做到毫秒级响应", page=11)
layers = [
    ("前端", "WebView2 · 原生 HTML/CSS/JS", "无框架依赖，包体小"),
    ("后端", "Rust · Tauri 2", "全局钩子、UIA、OCR、词典全部原生"),
    ("取词", "WH_MOUSE_LL + UIA + 剪贴板", "三层降级，取不到就换下一种"),
    ("引擎", "词典 / Ollama / DeepSeek / MyMemory", "统一接口，按可用性自动回退"),
]
for i, (k, tech, note) in enumerate(layers):
    y = 1.9 + i * 1.16
    band(s, M, y, 7.9, 1.0)
    tb, tf = textbox(s, M + 0.3, y + 0.12, 7.3, 0.8)
    p = tf.paragraphs[0]
    set_run(p.add_run(), k, size=13.5, bold=True, color=ACCENT)
    set_run(p.add_run(), "   " + tech, size=12.5, color=INK)
    para(tf, note, size=11, color=MUTED, space_before=3)

tb, tf = textbox(s, 9.0, 1.9, 3.6, 4.6)
para(tf, "关键取舍", size=15, bold=True, color=ACCENT, first=True)
for t in ["词典用 mmap + 哈希 + 二分，查词 26 微秒",
          "词典不放资源内嵌，改为运行时加载",
          "长文自动分段，避免单次超限",
          "逐词兜底不写缓存，方便算法迭代"]:
    para(tf, "· " + t, size=12, color=MUTED, space_before=10)

# ---------- 12 结尾 ----------
s = slide_new(prs)
glow2 = s.shapes.add_shape(MSO_SHAPE.OVAL, Inches(W / 2 - 3.6), Inches(4.5), Inches(7.2), Inches(5.2))
glow2.fill.solid()
glow2.fill.fore_color.rgb = rgb("101B31")
glow2.line.fill.background()
glow2.shadow.inherit = False

tb, tf = textbox(s, 0.8, 1.8, W - 1.6, 0.9, align=PP_ALIGN.CENTER)
para(tf, "接下来", size=30, bold=True, first=True, align=PP_ALIGN.CENTER)

items = [
    ("可发现", "版本号与检查更新：远端 JSON + 语义化版本比较"),
    ("可扩展", "术语表：模组与领域词汇的固定译法"),
    ("可传播", "更多翻译源接入，进一步降低使用门槛"),
]
for i, (k, v) in enumerate(items):
    y = 3.0 + i * 0.82
    tb, tf = textbox(s, 2.7, y, 8.0, 0.65)
    p = tf.paragraphs[0]
    set_run(p.add_run(), k, size=15, bold=True, color=ACCENT)
    set_run(p.add_run(), "    " + v, size=13.5, color=MUTED)

tb, tf = textbox(s, 0.8, 6.0, W - 1.6, 0.95, align=PP_ALIGN.CENTER)
para(tf, "U.N.Owen 译制组", size=18, bold=True, first=True, align=PP_ALIGN.CENTER)
para(tf, "名字来自一首歌，歌名是个问题：U.N.Owen 是她吗？", size=11.5,
     color=DIM, align=PP_ALIGN.CENTER, space_before=7)

prs.save(OUT)
print("saved:", OUT)
