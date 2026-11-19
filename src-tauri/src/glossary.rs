//! 代码术语表：常见标识符用词 → 中文。
//!
//! 为什么需要它：模型翻 `ctx` / `buf` / `idx` 这类缩写经常给出"上下文管理器"之类的
//! 长解释，或者干脆编一个意思。这些词在编程语境里的含义是**固定的**，
//! 查表比问模型更快、更准，而且离线可用（毫秒级）。
//!
//! 匹配规则：先精确匹配，再尝试去掉复数 s / es 后匹配。

/// 查一个词在编程语境下的中文含义。
pub fn lookup(word: &str) -> Option<&'static str> {
    let w = word.to_ascii_lowercase();
    if let Some(m) = exact(&w) {
        return Some(m);
    }
    // 复数：flags → flag, boxes → box, indices → 不处理（已在表里）
    if let Some(stem) = w.strip_suffix("es") {
        if let Some(m) = exact(stem) {
            return Some(m);
        }
    }
    if let Some(stem) = w.strip_suffix('s') {
        if let Some(m) = exact(stem) {
            return Some(m);
        }
    }
    None
}

fn exact(w: &str) -> Option<&'static str> {
    Some(match w {
        // ---- 常见缩写 ----
        "arg" | "args" | "argv" => "参数",
        "param" | "params" => "参数",
        "buf" | "buffer" => "缓冲区",
        "ctx" | "context" => "上下文",
        "fn" | "func" | "function" => "函数",
        "impl" | "implementation" => "实现",
        "ret" => "返回",
        "val" | "value" => "值",
        "idx" | "index" => "索引",
        "tmp" | "temp" => "临时",
        "cfg" | "conf" | "config" => "配置",
        "src" | "source" => "来源",
        "dst" | "dest" | "target" => "目标",
        "msg" | "message" => "消息",
        "err" | "error" => "错误",
        "req" | "request" => "请求",
        "res" | "resp" | "response" => "响应",
        "iter" | "iterator" => "迭代器",
        "len" | "length" => "长度",
        "str" | "string" => "字符串",
        "num" | "number" => "数字",
        "ptr" | "pointer" => "指针",
        "rect" => "矩形",
        "wnd" | "window" => "窗口",
        "hwnd" => "窗口句柄",
        "handle" => "句柄",
        "cb" | "callback" => "回调",
        "evt" | "event" => "事件",
        "attr" | "attribute" => "属性",
        "prop" | "property" => "属性",

        // ---- 生命周期 / 状态 ----
        "init" | "initialize" => "初始化",
        "deinit" | "destroy" => "销毁",
        "create" | "new" => "创建",
        "delete" | "remove" | "drop" => "删除",
        "load" => "加载",
        "unload" => "卸载",
        "save" | "store" => "保存",
        "read" => "读取",
        "write" => "写入",
        "open" => "打开",
        "close" => "关闭",
        "start" => "启动",
        "stop" => "停止",
        "pause" => "暂停",
        "resume" => "恢复",
        "reset" => "重置",
        "update" => "更新",
        "refresh" => "刷新",
        "reload" => "重新加载",
        "sync" => "同步",
        "async" => "异步",
        "lock" => "加锁",
        "unlock" => "解锁",
        "wait" => "等待",
        "notify" => "通知",
        "emit" => "发出",
        "trigger" => "触发",
        "dispatch" => "分发",
        "subscribe" => "订阅",
        "publish" => "发布",
        "register" => "注册",
        "unregister" => "注销",
        "enable" => "启用",
        "disable" => "禁用",
        "toggle" => "切换",
        "check" => "检查",
        "verify" => "校验",
        "validate" => "验证",
        "parse" => "解析",
        "format" => "格式化",
        "encode" => "编码",
        "decode" => "解码",
        "serialize" => "序列化",
        "deserialize" => "反序列化",
        "compress" => "压缩",
        "decompress" => "解压",
        "encrypt" => "加密",
        "decrypt" => "解密",
        "hash" => "哈希",
        "scene" => "场景",
        "item" => "条目",
        "button" => "按钮",
        "sound" => "声音",
        "timer" => "计时器",
        "view" => "视图",
        "widget" => "控件",
        "label" => "标签",
        "background" => "背景",
        "zombie" => "僵尸",
        "plant" => "植物",
        "bullet" => "子弹",
        "damage" => "伤害",
        "health" => "生命值",
        "score" => "分数",
        "wave" => "波次",
        "enemy" => "敌人",
        "player" => "玩家",
        "coin" => "金币",
        "spawn" => "生成",
        "render" => "渲染",
        "draw" => "绘制",
        "paint" => "绘制",
        "layout" => "布局",
        "resize" => "调整尺寸",
        "scroll" => "滚动",
        "click" => "点击",
        "hover" => "悬停",
        "focus" => "焦点",
        "blur" => "失焦",
        "show" => "显示",
        "hide" => "隐藏",
        "visible" => "可见",
        "enabled" => "已启用",
        "disabled" => "已禁用",
        "pending" => "待处理",
        "running" => "运行中",
        "failed" => "失败",
        "success" => "成功",
        "retry" => "重试",
        "timeout" => "超时",
        "abort" | "cancel" => "取消",

        // ---- 数据结构 ----
        "list" => "列表",
        "array" | "arr" => "数组",
        "vec" | "vector" => "向量",
        "map" | "dict" | "dictionary" => "映射",
        "set" => "集合",
        "queue" => "队列",
        "stack" => "栈",
        "heap" => "堆",
        "tree" => "树",
        "graph" => "图",
        "node" => "节点",
        "edge" => "边",
        "leaf" => "叶子节点",
        "root" => "根",
        "key" => "键",
        "entry" => "条目",
        "element" | "elem" => "元素",
        "count" => "计数",
        "size" => "大小",
        "capacity" | "cap" => "容量",
        "offset" => "偏移",
        "pos" | "position" => "位置",
        "range" => "范围",
        "slice" => "切片",
        "chunk" => "数据块",
        "batch" => "批处理",

        // ---- 系统 / 网络 ----
        "thread" => "线程",
        "task" => "任务",
        "process" => "进程",
        "mutex" => "互斥锁",
        "semaphore" => "信号量",
        "channel" => "通道",
        "pipe" => "管道",
        "socket" => "套接字",
        "stream" => "流",
        "host" => "主机",
        "port" => "端口",
        "client" => "客户端",
        "server" => "服务端",
        "proxy" => "代理",
        "hook" => "钩子",
        "handler" => "处理器",
        "listener" => "监听器",
        "worker" => "工作线程",
        "pool" => "池",
        "connection" | "conn" => "连接",
        "session" => "会话",
        "token" => "令牌",
        "auth" => "鉴权",
        "permission" => "权限",
        "role" => "角色",
        "user" => "用户",
        "account" => "账号",
        "profile" => "配置档案",

        // ---- 存储 / 数据 ----
        "cache" => "缓存",
        "db" | "database" => "数据库",
        "query" => "查询",
        "row" => "行",
        "column" | "col" => "列",
        "table" => "表",
        "schema" => "模式",
        "field" => "字段",
        "record" => "记录",
        "migration" => "迁移",
        "transaction" => "事务",
        "commit" => "提交",
        "rollback" => "回滚",

        // ---- 工程 / 构建 ----
        "version" => "版本",
        "release" => "发布",
        "build" => "构建",
        "compile" => "编译",
        "deploy" => "部署",
        "install" => "安装",
        "uninstall" => "卸载",
        "test" => "测试",
        "debug" => "调试",
        "warn" | "warning" => "警告",
        "info" => "信息",
        "trace" => "追踪",
        "log" => "日志",
        "issue" => "问题",
        "bug" => "缺陷",
        "fix" => "修复",
        "patch" => "补丁",
        "feature" => "功能",
        "module" | "mod" => "模块",
        "package" | "pkg" => "包",
        "plugin" => "插件",
        "extension" | "ext" => "扩展",
        "dependency" | "dep" => "依赖",
        "license" => "许可证",
        "author" => "作者",
        "sample" | "example" => "示例",
        "default" => "默认",
        "option" | "opt" => "选项",
        "flag" => "标志",
        "state" => "状态",
        "status" => "状态",
        "mode" => "模式",
        "type" => "类型",
        "kind" => "种类",
        "level" => "级别",
        "title" => "标题",
        "text" => "文本",
        "line" => "行",
        "path" => "路径",
        "file" => "文件",
        "dir" | "directory" | "folder" => "目录",
        "name" => "名称",
        "id" => "编号",
        "time" => "时间",
        "date" => "日期",
        "duration" => "时长",
        "interval" => "间隔",
        "delay" => "延迟",
        "speed" => "速度",
        "ratio" => "比例",
        "scale" => "缩放",
        "width" => "宽度",
        "height" => "高度",
        "min" => "最小",
        "max" => "最大",
        "avg" | "average" => "平均",
        "total" => "总计",
        "sum" => "总和",
        "current" | "cur" => "当前",
        "prev" | "previous" => "上一个",
        "next" => "下一个",
        "first" => "第一个",
        "last" => "最后一个",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::lookup;

    #[test]
    fn common_code_words() {
        assert_eq!(lookup("ctx"), Some("上下文"));
        assert_eq!(lookup("Buffer"), Some("缓冲区"));
        assert_eq!(lookup("flags"), Some("标志"));
        assert_eq!(lookup("boxes"), None); // 表里没有 box："boxes" → "box" 查不到
        assert_eq!(lookup("userProfile"), None);
    }
}

/// 各语言共有的关键字 —— 这些**不该翻译**（`for` 翻成"为"只会更看不懂）。
pub fn is_keyword(w: &str) -> bool {
    matches!(
        w,
        "if" | "else" | "for" | "while" | "do" | "switch" | "case" | "default" | "break"
            | "continue" | "return" | "goto" | "try" | "catch" | "finally" | "throw" | "new"
            | "delete" | "this" | "self" | "super" | "class" | "struct" | "enum" | "union"
            | "interface" | "trait" | "impl" | "fn" | "def" | "func" | "function" | "public"
            | "private" | "protected" | "static" | "const" | "let" | "var" | "mut" | "auto"
            | "void" | "int" | "uint" | "long" | "short" | "float" | "double" | "char" | "bool"
            | "string" | "str" | "byte" | "unsigned" | "signed" | "size_t" | "nullptr" | "null"
            | "true" | "false" | "and" | "or" | "not" | "in" | "is" | "as" | "use" | "mod"
            | "pub" | "crate" | "extern" | "inline" | "virtual" | "override" | "final"
            | "namespace" | "using" | "typedef" | "template" | "typename" | "operator"
            | "await" | "async" | "yield" | "match" | "when" | "elif" | "lambda" | "pass"
            | "import" | "export" | "from" | "package" | "include" | "define"
    )
}

/// 标识符里的"动词用法"覆盖表。
///
/// 同一个词在类型位置和动词位置意思不同：`Set` 作类型是"集合"，
/// 作前缀（`setPos`）却是"设置"。这里优先按动词理解。
fn ident_word(part: &str) -> Option<&'static str> {
    match part.to_ascii_lowercase().as_str() {
        "set" => Some("设置"),
        "get" => Some("获取"),
        "add" => Some("添加"),
        "del" | "rm" => Some("删除"),
        "is" => Some("是"),
        "has" => Some("有"),
        "to" => Some("转成"),
        "on" => Some("当"),
        "max" => Some("最大"),
        "min" => Some("最小"),
        _ => lookup(part),
    }
}

/// 把一个标识符按 camelCase / snake_case 拆开，逐段查表；全部命中才返回拼接结果。
///
/// 例：`setPos` → "设置位置"、`addItem` → "添加条目"、`userProfileCache` → "用户配置档案缓存"。
/// 只要有一段落空就返回 None（宁可保留原文，也不给半成品）。
pub fn lookup_identifier(word: &str) -> Option<String> {
    // 纯大写缩写（URL / API）不动
    if word.chars().all(|c| c.is_ascii_uppercase()) && word.chars().count() >= 2 {
        return None;
    }
    let mut out = String::new();
    let mut any = false;
    for seg in word.split(['_', '-', '.', '/', '\\']) {
        if seg.is_empty() {
            continue;
        }
        // camelCase 边界
        let chars: Vec<char> = seg.chars().collect();
        let mut parts: Vec<String> = Vec::new();
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

        for part in parts {
            // 单字母前缀（Q、u8 里的 u）保留
            if part.chars().count() <= 1 {
                out.push_str(&part);
                continue;
            }
            if is_keyword(&part.to_ascii_lowercase()) {
                out.push_str(&part);
                any = true;
                continue;
            }
            match ident_word(&part) {
                Some(m) => {
                    out.push_str(m);
                    any = true;
                }
                None => return None,
            }
        }
    }
    if any {
        Some(out)
    } else {
        None
    }
}

#[cfg(test)]
mod ident_tests {
    use super::lookup_identifier;

    #[test]
    fn splits_camel_case() {
        assert_eq!(lookup_identifier("setPos").as_deref(), Some("设置位置"));
        assert_eq!(lookup_identifier("addItem").as_deref(), Some("添加条目"));
        assert_eq!(lookup_identifier("userProfileCache").as_deref(), Some("用户配置档案缓存"));
        // 有查不到的段落 → 整体放弃，保留原文
        assert_eq!(lookup_identifier("qqqqZzzz"), None);
    }
}
