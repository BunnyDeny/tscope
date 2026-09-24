//! watch 子命令：像 Keil Watch 窗口一样实时刷新变量值。
//!
//! GUI 形态（与 plot 一致，全项目统一）：
//! - 独立子命令 `tscope watch <组>` / `var --watch`：打开自己的探针，
//!   采样线程独占 Session，值文本经 mpsc 送进 [`tscope_plot::WatchApp`]
//!   （名称 + 格式化值表格，变化整行黄色高亮）；
//! - debug 会话里 `watch <组>`：异步窗口——探针唯一属主是调试主循环，
//!   GUI 跑在派生的 `tscope watch --feed` 子进程（winit 每进程只允许
//!   一个 EventLoop，子进程方案让关窗后重开可行），值经 stdin 行协议馈送。
//!   **与 plot 窗口可同时打开**，两个窗口独立采样、独立关窗。
//!
//! 复合类型（数组/结构体/联合体）**展开成子项行**（`buf[0]`、`s.member`…），
//! 最多显示 max_elems 项（默认 8），超出显示「…余N」汇总行；每个基础变量
//! 每采样周期只做**一次**内存读（子项行共享同一份缓冲区），采样期间
//! 目标 CPU 不暂停。
//!
//! # 馈送行协议（debug 父进程 → `watch --feed` 子进程的 stdin）
//!
//! 每行一条，`\n` 结尾（读端兼容 `\r\n`）：
//! - `T <标题>`：窗口标题（行剩余部分）；
//! - `N <行数>`：监视行总数；
//! - `C <行标签>`：每行一个，共 N 行（顺序即行号）；
//! - `V <行号> <转义值文本>`：该行的新值。转义：`\\` `\n` `\r` `\t`
//!   （值可含空格，解析时按第一个空格切行号）。
//!
//! 子进程窗口关闭（✕）即正常退出；stdin EOF（父进程断开）也直接退出，
//! 不留无主空窗口。父端靠轮询子进程退出状态检测关窗。

use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use probe_rs::{Core, Session};
use tscope_plot::{run_watch_app, WatchApp};

use crate::config::{ToolConfig, WatchGroup};
use crate::feed::FeedChild;
use crate::session;
use crate::symbol::{prepare_symbol, PreparedSymbol, TypeDesc};

/// 一行监视项（debug 主循环侧使用）
pub(crate) struct WatchRow {
    /// 表格第一列显示的文字（普通行 = 表达式，数组元素行 = `expr[i]`）
    label: String,
    prepared: Result<PreparedSymbol, String>,
    /// 数组元素行：相对字段的偏移 + 元素类型；普通行 None
    elem: Option<(usize, TypeDesc)>,
    /// 「…余N」汇总行：值为 None
    summary: Option<String>,
    last_value: String,
    last_error: Option<String>,
    /// 上次已发给窗口的显示文本（变化才发；首拍全发）
    pub(crate) last_sent: Option<String>,
}

impl WatchRow {
    /// 当前该行的显示文本（值 / 错误 / 汇总）
    pub(crate) fn display_text(&self) -> String {
        if let Err(e) = &self.prepared {
            return e.clone();
        }
        if let Some(s) = &self.summary {
            return s.clone();
        }
        if let Some(e) = &self.last_error {
            return e.clone();
        }
        self.last_value.clone()
    }
}

/// 一次监视会话的完整准备（纯 CPU，不碰探针）：
/// 行构建 + 符号解析（数组按 max_elems 展开成子项行）
pub struct PreparedWatch {
    pub rows: Vec<WatchRow>,
    pub interval_ms: u64,
}

impl PreparedWatch {
    /// 窗口行标签（顺序即行号）
    pub fn labels(&self) -> Vec<String> {
        self.rows.iter().map(|r| r.label.clone()).collect()
    }
}

/// 列出配置里定义的所有监视组（`tscope watch` 不带参数时）
pub fn list_groups(config: &ToolConfig) -> Result<()> {
    if config.watch.is_empty() {
        println!("（配置里没有定义 watch 节）");
        return Ok(());
    }
    println!("配置里定义的监视组：");
    for (name, g) in &config.watch {
        println!(
            "  {name:<16} 每 {} ms 采样，{} 个符号，复合类型最多展开 {} 项",
            g.interval_ms,
            g.symbols.len(),
            g.max_elems
        );
    }
    println!("\n用法：tscope watch <组名>");
    Ok(())
}

/// 解析 yaml 里的监视组（纯 CPU；debug 的 watch 命令第一步调用它——
/// 任何配置/符号错误在此拦下，调试会话与探针不受影响）
pub fn prepare_watch(config: &ToolConfig, group: &str) -> Result<PreparedWatch> {
    let g = config.watch.get(group).ok_or_else(|| {
        let names: Vec<String> = config.watch.keys().cloned().collect();
        if names.is_empty() {
            anyhow!(
                "配置里没有定义 watch 节（在 tscope.yaml 里加 watch 段，格式见模板 tscope.yaml）"
            )
        } else {
            anyhow!(
                "配置里没有名为 {group} 的监视组；可用组：{}",
                names.join(", ")
            )
        }
    })?;
    let elf = config.firmware_image()?;
    Ok(PreparedWatch {
        rows: build_rows(g, elf),
        interval_ms: g.interval_ms,
    })
}

/// 临时监视（var --watch 用）：不依赖配置文件里的监视组，就地组装一组
pub fn prepare_watch_adhoc(
    elf: &std::path::Path,
    symbols: Vec<String>,
    interval_ms: u64,
    max_elems: usize,
) -> Result<PreparedWatch> {
    let g = WatchGroup {
        interval_ms,
        max_elems: max_elems.max(1),
        symbols,
    };
    Ok(PreparedWatch {
        rows: build_rows(&g, elf),
        interval_ms: g.interval_ms,
    })
}

/// 行构建（解析阶段，一次性）：单个符号解析失败只影响那一行，不中断整体。
/// 数组符号在此展开成元素行（封顶 max_elems）。
fn build_rows(g: &WatchGroup, elf: &std::path::Path) -> Vec<WatchRow> {
    let mut rows: Vec<WatchRow> = Vec::new();
    for expr in &g.symbols {
        match prepare_symbol(elf, expr) {
            Err(e) => rows.push(WatchRow {
                label: expr.clone(),
                prepared: Err(format!("{e:#}")),
                elem: None,
                summary: None,
                last_value: String::new(),
                last_error: None,
                last_sent: None,
            }),
            Ok(p) => match p.child_rows(g.max_elems) {
                Some((rows_elems, remaining)) => {
                    for (lab, off, ty) in rows_elems {
                        rows.push(WatchRow {
                            label: format!("{expr}{lab}"),
                            prepared: Ok(p.clone()),
                            elem: Some((off, ty)),
                            summary: None,
                            last_value: String::new(),
                            last_error: None,
                            last_sent: None,
                        });
                    }
                    if remaining > 0 {
                        rows.push(WatchRow {
                            label: expr.clone(),
                            prepared: Ok(p),
                            elem: None,
                            summary: Some(format!(
                                "… 其余 {remaining} 项未显示（调大 max_elems 或直接监视具体成员/下标）"
                            )),
                            last_value: String::new(),
                            last_error: None,
                            last_sent: None,
                        });
                    }
                }
                None => rows.push(WatchRow {
                    label: expr.clone(),
                    prepared: Ok(p),
                    elem: None,
                    summary: None,
                    last_value: String::new(),
                    last_error: None,
                    last_sent: None,
                }),
            },
        }
    }
    rows
}

/// 采样一个节拍：填每行的 last_value / last_error。
/// 同一个基础变量（如整块数组）只读一次内存，其元素行共享缓冲区。
/// 单行读失败只标记该基础变量的子项行，不中断整体。
pub(crate) fn sample_rows_core(core: &mut Core, rows: &mut [WatchRow]) -> Result<()> {
    let mut cache_key: Option<(u64, usize)> = None;
    let mut cache_buf: Option<Vec<u8>> = None;

    for row in rows.iter_mut() {
        if row.prepared.is_err() || row.summary.is_some() {
            continue;
        }
        let p = row.prepared.as_ref().unwrap();

        let key = p.base_key();
        if cache_key != Some(key) {
            match p.read_base(core) {
                Ok(buf) => {
                    cache_key = Some(key);
                    cache_buf = Some(buf);
                }
                Err(e) => {
                    cache_key = Some(key);
                    cache_buf = None;
                    row.last_error = Some(format!("{e:#}"));
                    continue;
                }
            }
        }
        let Some(buf) = cache_buf.as_ref() else {
            row.last_error = Some("读取失败".to_string());
            continue;
        };

        let value = match &row.elem {
            Some((off, ty)) => p.render_at(buf, *off, ty),
            None => p.render_self(buf),
        };
        row.last_value = value;
        row.last_error = None;
    }
    Ok(())
}

/// 采样并送出变化行（首拍全送；失败保持旧值）。tx 断开 = 窗口已关闭。
fn sample_and_send(
    core: &mut Core,
    rows: &mut [WatchRow],
    tx: &mpsc::Sender<(usize, String)>,
) -> bool {
    let _ = sample_rows_core(core, rows); // 读失败保持旧值
    for (i, row) in rows.iter_mut().enumerate() {
        let text = row.display_text();
        if row.last_sent.as_deref() != Some(text.as_str()) {
            if tx.send((i, text.clone())).is_err() {
                return false; // 窗口已关闭
            }
            row.last_sent = Some(text);
        }
    }
    true
}

/// 独立子命令入口：打开自己的探针，显示 yaml 配置的监视组（GUI 窗口）
pub fn run(config: &ToolConfig, group: &str) -> Result<()> {
    let prep = prepare_watch(config, group)?;
    let mut session = session::open_session(&config.probe, &config.chip)?;
    // 预检：attach 失败立即报错退出，不弹空窗口（采样线程内部会重新 attach）
    session.core(0).context("attach 内核失败")?;
    run_gui_with_session(&mut session, prep, &format!("tscope watch [{group}]"))
}

/// `var --watch`：单符号临时监视（GUI 窗口），复用调用方已打开的会话
pub fn run_adhoc(
    session: &mut Session,
    elf: &std::path::Path,
    title: &str,
    symbols: Vec<String>,
    interval_ms: u64,
    max_elems: usize,
) -> Result<()> {
    let prep = prepare_watch_adhoc(elf, symbols, interval_ms, max_elems)?;
    run_gui_with_session(session, prep, title)
}

/// 复用已有会话跑 GUI：采样线程独占（借用）Session 按节拍读符号、
/// 格式化后经 mpsc 送 WatchApp；本线程阻塞在窗口主循环，关窗返回。
fn run_gui_with_session(session: &mut Session, prep: PreparedWatch, title: &str) -> Result<()> {
    let interval = Duration::from_millis(prep.interval_ms.max(10));
    let (tx, rx) = mpsc::channel::<(usize, String)>();
    let mut app = WatchApp::new(prep.labels());
    app.set_inbox(rx);
    let mut rows = prep.rows;

    std::thread::scope(|s| -> Result<()> {
        // 显式重借用：move 闭包只搬走 &mut 引用，Session 本体留在这里
        let session_ref = &mut *session;
        s.spawn(move || sampler_watch_loop(session_ref, &mut rows, interval, tx));
        run_watch_app(app, title).map_err(|e| anyhow!("GUI 运行失败：{e}"))
    })?;
    Ok(())
}

/// 采样循环：独占（借用）Session，按固定节拍读符号 → 格式化 → mpsc。
/// UI 关闭（接收端 drop）后 send 失败，循环退出、线程结束。
fn sampler_watch_loop(
    session: &mut Session,
    rows: &mut [WatchRow],
    interval: Duration,
    tx: mpsc::Sender<(usize, String)>,
) {
    // 挂接带退避重试：J-Link 的 USB 传输偶发超时（环境问题，与工具逻辑无关）
    let mut attempt: u32 = 0;
    let core = 'attach: loop {
        attempt += 1;
        match session.core(0) {
            Ok(c) => break 'attach Some(c),
            Err(e) if attempt < 5 => {
                eprintln!("采样线程挂接失败（第 {attempt}/5 次）：{e:#}，重试中…");
                std::thread::sleep(Duration::from_millis(300 * u64::from(attempt)));
            }
            Err(e) => {
                eprintln!("采样线程挂接失败（第 5/5 次）：{e:#}，放弃采样（监视窗口将无数据）");
                break 'attach None;
            }
        }
    };
    let Some(mut core) = core else {
        return;
    };
    let mut next = Instant::now();
    loop {
        if !sample_and_send(&mut core, rows, &tx) {
            break; // UI 已关闭
        }
        // 固定节拍补偿：读取耗时不影响采样周期；
        // 落后太多时放弃补样，直接对齐当前时刻
        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}

// ———————————————— 馈送模式（debug 会话的监视窗口子进程） ————————————————

/// debug 会话 → 监视窗口子进程的连接（父端，通用连接见 [`FeedChild`]）
pub type FeedWatchChild = FeedChild;

/// 派生 `watch --feed` 子进程并写入头部。值随后经
/// [`feed_write_value`] 逐行馈送。
pub fn spawn_watch_feed_child(labels: &[String], title: &str) -> Result<FeedWatchChild> {
    let mut feed = FeedChild::spawn(&["watch", "--feed"]).context("启动监视窗口子进程失败")?;
    match feed.writer() {
        Some(w) => write_watch_header(w, labels, title).context("写子进程馈送头部失败")?,
        None => bail!("监视窗口子进程 stdin 不可用"),
    }
    Ok(feed)
}

/// 发一行值。写入失败（子进程已退出）静默忽略——关窗检测统一走
/// [`FeedChild::poll_exit`]，内核暂停时不发数据也能可靠检测关窗
pub fn feed_write_value(feed: &mut FeedWatchChild, idx: usize, text: &str) {
    if let Some(w) = feed.writer() {
        let _ = writeln!(w, "V {idx} {}", escape_value(text)).and_then(|_| w.flush());
    }
}

/// 写头部（T/N/C 行）——与 [`read_watch_header`] 对应，见模块文档的协议
fn write_watch_header<W: Write>(w: &mut W, labels: &[String], title: &str) -> std::io::Result<()> {
    writeln!(w, "T {title}")?;
    writeln!(w, "N {}", labels.len())?;
    for l in labels {
        writeln!(w, "C {l}")?;
    }
    w.flush()
}

/// 值文本转义：`\` `\n` `\r` `\t` → 双字符序列（保证一行一值）
fn escape_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// 转义文本还原（[`escape_value`] 的逆操作；坏转义原样保留）
fn unescape_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// 解析一条 `V <行号> <转义值>` 行；坏行返回 None（宽容，不断流）
fn parse_v_line(line: &str) -> Option<(usize, String)> {
    let rest = line.strip_prefix("V ")?;
    let (idx, text) = rest.split_once(' ')?;
    Some((idx.parse().ok()?, unescape_value(text)))
}

/// 馈送头部（T/N/C 行解析结果）
struct WatchFeedHeader {
    title: String,
    labels: Vec<String>,
}

/// 同步读头部。C 行读满 N 行即返回，剩余数据行交给数据读线程继续消费。
fn read_watch_header<R: BufRead>(r: &mut R) -> Result<WatchFeedHeader> {
    let mut title: Option<String> = None;
    let mut count: Option<usize> = None;
    let mut labels: Vec<String> = Vec::new();
    for line in r.lines() {
        let line = line.context("读馈送头部失败")?;
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("T ") {
            title = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("N ") {
            count = Some(rest.trim().parse().context("行数不是数字")?);
        } else if let Some(rest) = line.strip_prefix("C ") {
            labels.push(rest.to_string());
            if labels.len() == count.unwrap_or(usize::MAX) {
                break; // 头部结束；缓冲里的数据行由数据读线程接着读
            }
        } else {
            bail!("未知的头部行：{line:?}（期望 T/N/C 开头的行）");
        }
    }
    let title = title.ok_or_else(|| anyhow!("头部缺少 T 行（窗口标题）"))?;
    if labels.is_empty() {
        bail!("头部没有 C 行（至少一个监视行）");
    }
    if count.is_some_and(|n| n != labels.len()) {
        bail!("头部 N 与 C 行数不符");
    }
    Ok(WatchFeedHeader { title, labels })
}

/// `watch --feed` 隐藏模式：GUI 进程只消费 stdin 馈送，不碰探针。
/// 窗口关闭（✕）后本进程正常退出；stdin EOF（父进程断开，如调试会话
/// 退出/崩溃）也直接退出，不留无主空窗口。
pub fn run_watch_feed() -> Result<()> {
    let stdin = std::io::stdin();
    // 头部在 Stdin 自带的内部缓冲上直接读（同 plot --feed 的做法）：
    // 不套外层 BufReader——外层缓冲跨线程迁移会丢未消费字节
    let header = {
        let mut lock = stdin.lock();
        read_watch_header(&mut lock).context("解析馈送头部失败")?
    };
    let (tx, rx) = mpsc::channel::<(usize, String)>();
    let mut app = WatchApp::new(header.labels);
    app.set_inbox(rx);

    // 数据读线程：stdin 行 → (行号, 值) 通道。EOF（父进程断开）直接
    // 退出本进程，不留下无主的空窗口
    std::thread::Builder::new()
        .name("tscope-watch-feed".into())
        .spawn(move || {
            // 同一 Stdin 再 lock：内部缓冲保留头部之后已读入的剩余数据行
            let lock = stdin.lock();
            for line in lock.lines() {
                let Ok(line) = line else { break };
                if let Some((idx, text)) = parse_v_line(&line) {
                    if tx.send((idx, text)).is_err() {
                        break;
                    }
                }
            }
            std::process::exit(0);
        })
        .context("创建馈送读取线程失败")?;

    run_watch_app(app, &header.title).map_err(|e| anyhow!("GUI 运行失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn value_escape_round_trip() {
        let cases = [
            "1.5 (float)",
            "positionStruct = {\n    position: -3.03 (float)\n    rotations: -1 (int)\n}",
            "反斜杠 \\ 与制表\t符",
            "空行\n\n结束",
        ];
        for text in cases {
            let escaped = escape_value(text);
            assert!(!escaped.contains('\n'), "转义后不得含裸换行：{escaped:?}");
            assert_eq!(unescape_value(&escaped), text);
        }
    }

    #[test]
    fn v_line_parse() {
        let Some((idx, text)) = parse_v_line("V 3 1.5 (float)") else {
            panic!("应解析成功");
        };
        assert_eq!(idx, 3);
        assert_eq!(text, "1.5 (float)");
        // 多行值往返
        let multi = "struct = {\n    a: 1\n}";
        let line = format!("V 0 {}", escape_value(multi));
        let (i, t) = parse_v_line(&line).unwrap();
        assert_eq!(i, 0);
        assert_eq!(t, multi);
        // 坏行忽略
        assert!(parse_v_line("S 0 x").is_none());
        assert!(parse_v_line("V abc x").is_none());
        assert!(parse_v_line("").is_none());
    }

    #[test]
    fn watch_header_round_trip() {
        let labels = vec!["theta".into(), "buf[0]".into(), "ENC_1_POS_SENSOR".into()];
        let mut buf = Vec::new();
        write_watch_header(&mut buf, &labels, "debug watch [w1]").unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text, "T debug watch [w1]\nN 3\nC theta\nC buf[0]\nC ENC_1_POS_SENSOR\n");
        let mut reader = BufReader::new(text.as_bytes());
        let h = read_watch_header(&mut reader).unwrap();
        assert_eq!(h.title, "debug watch [w1]");
        assert_eq!(h.labels, labels);
        // 头部之后的剩余字节留在 reader 缓冲里，数据读线程接着读（不丢行）
        let rest = reader.lines().collect::<std::io::Result<Vec<_>>>().unwrap();
        assert!(rest.is_empty());
    }

    #[test]
    fn watch_header_errors() {
        let bad = [
            "",                             // 全缺
            "T t\n",                        // 缺 N/C
            "T t\nN 2\nC a\n",              // N 与 C 行数不符
            "T t\nC a\nG 1\n",              // 未知行
        ];
        for bad in &bad {
            assert!(
                read_watch_header(&mut BufReader::new(bad.as_bytes())).is_err(),
                "应报错：{bad:?}"
            );
        }
    }
}
