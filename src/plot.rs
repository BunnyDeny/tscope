//! plot 子命令：独立 GUI 窗口显示变量实时曲线（基于 crates/tscope-plot 库）。
//!
//! 配置来自 tscope.yaml 的 plot 节（结构类似 watch 组）：
//! 每个图组 = 一个子图，组内符号同图共 Y 轴，多组上下叠放、共享 X 轴联动。
//!
//! 四个入口：
//! - [`run`]：独立子命令，自己打开探针；
//! - [`run_adhoc`]：`var --plot` 的单符号临时曲线，自己打开探针；
//! - [`run_feed`]：`plot --feed` 隐藏模式——不碰探针，样本经 stdin 行协议
//!   馈送，供 debug 会话派生本程序当曲线窗口子进程用；
//! - [`spawn_feed_child`]：debug 会话里 `plot` 命令的父端——把本进程再派生
//!   成 `plot --feed` 子进程，探针仍由 debug 主循环独占。
//!
//! 架构：探针唯一属主。debug 主循环自己采样（Session 非 Sync、绝不共享），
//! 样本与内核暂停/恢复状态经子进程 stdin 写入；子进程只做「解析 → 画图」。
//! **GUI 必须放独立子进程**：winit 每进程只允许创建一个 EventLoop（关窗后
//! 同进程再开新窗必报 EventLoop can't be recreated），子进程方案让「关窗后
//! 立刻重开」变得天然可行——每次开窗 = 全新进程。独立路径（run/run_adhoc）
//! 每次 CLI 调用本来就是新进程，不受此限制。
//!
//! 同一符号可出现在多个图组：只采样一次（去重），按通道顺序复用数值。
//!
//! # 馈送行协议（debug 父进程 → `plot --feed` 子进程的 stdin）
//!
//! 每行一条，`\n` 结尾（读端兼容 `\r\n`）：
//! - `T <标题>`：窗口标题（行剩余部分）；
//! - `W <秒>`：滚动窗口宽度（f64）；
//! - `C <通道名>`：每通道一行，按顺序；
//! - `G <n1> [<n2> …]`：图组大小（通道按顺序连续划分）；
//! - `P <0|1>`：1 = 内核暂停（曲线冻结），0 = 内核运行（恢复滚动）；
//! - `S <t> <v0> [<v1> …]`：一个样本（值不足补 NaN、多余忽略）。
//!
//! 子进程窗口关闭（✕）即正常退出；stdin EOF（父进程断开）也直接退出，
//! 不留无主空窗口。父端靠轮询子进程退出状态检测关窗——内核暂停时采样
//! 不发数据，不能依赖写失败检测（会漏）。

use std::io::{BufRead, BufWriter, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use probe_rs::Session;
use tscope_plot::{run_app, ChannelSource, PlotApp, PlotOptions, Sample};

use crate::config::{ChipConfig, PlotConfig, ProbeConfig, ToolConfig};
use crate::session;
use crate::symbol::{prepare_symbol, PreparedSymbol};

/// 一次曲线会话的完整准备（纯 CPU，不碰探针）：
/// 展平符号、图组边界、解析并校验标量、通道映射。
pub struct PreparedPlot {
    pub window_secs: f64,
    pub interval_ms: u64,
    /// 通道顺序（每个符号出现位置一个通道）
    pub ordered: Vec<String>,
    /// 图组划分（通道索引）
    pub groups_idx: Vec<Vec<usize>>,
    /// 去重后的符号解析结果
    pub prepared: Vec<PreparedSymbol>,
    /// 通道顺序 → prepared 下标
    pub chan_of: Vec<usize>,
}

/// 列出配置里定义的所有曲线配置（`tscope plot` 不带参数时）
pub fn list_plots(config: &ToolConfig) -> Result<()> {
    if config.plot.is_empty() {
        println!("（配置里没有定义 plot 节）");
        return Ok(());
    }
    println!("配置里定义的曲线配置：");
    for (name, p) in &config.plot {
        println!(
            "  {name:<16} 每 {} ms 采样，窗口 {:.1} s，{} 个图（共 {} 个符号）",
            p.interval_ms,
            p.window_secs,
            p.groups.len(),
            p.groups.iter().map(|g| g.len()).sum::<usize>()
        );
    }
    println!("\n用法：tscope plot <配置名>");
    Ok(())
}

/// 查找 yaml 里的曲线配置
pub fn resolve_plot<'a>(config: &'a ToolConfig, name: &str) -> Result<&'a PlotConfig> {
    config.plot.get(name).ok_or_else(|| {
        let names: Vec<String> = config.plot.keys().cloned().collect();
        if names.is_empty() {
            anyhow!("配置里没有定义 plot 节（在 tscope.yaml 里加 plot 段，格式见模板 tscope.yaml）")
        } else {
            anyhow!(
                "配置里没有名为 {name} 的曲线配置；可用配置：{}",
                names.join(", ")
            )
        }
    })
}

/// 纯 CPU 准备：解析 yaml 配置 + 全部符号（不碰探针）。
/// debug 的 plot 命令第一步调用它——任何配置/符号错误在此拦下，
/// 调试会话与探针不受影响。
pub fn prepare_plot(config: &ToolConfig, name: &str) -> Result<PreparedPlot> {
    let cfg = resolve_plot(config, name)?;
    let elf = config.firmware_image()?;
    prepare_groups(cfg, elf)
}

/// 纯 CPU 准备（给定 PlotConfig 与固件镜像）
fn prepare_groups(cfg: &PlotConfig, elf: &Path) -> Result<PreparedPlot> {
    // —— 展平符号：每个出现位置 = 一个通道；记录图组边界；去重采样 ——
    let mut ordered: Vec<String> = Vec::new();
    let mut groups_idx: Vec<Vec<usize>> = Vec::new();
    for g in &cfg.groups {
        let start = ordered.len();
        ordered.extend(g.iter().cloned());
        groups_idx.push((start..ordered.len()).collect());
    }
    let mut unique: Vec<String> = Vec::new();
    for s in &ordered {
        if !unique.contains(s) {
            unique.push(s.clone());
        }
    }

    // —— 解析符号（一次性 CPU 工作）：要求全部为标量 ——
    let mut prepared: Vec<PreparedSymbol> = Vec::with_capacity(unique.len());
    for s in &unique {
        let p = prepare_symbol(elf, s)
            .with_context(|| format!("解析符号 {s} 失败（ELF 里没有这个符号？）"))?;
        if !p.plottable() {
            bail!(
                "符号 {s} 是复合类型（{}），plot 只支持标量；请监视具体成员，如 {s}.成员名 或 {s}[0]",
                p.type_name()
            );
        }
        prepared.push(p);
    }
    // 通道顺序 → 唯一符号下标
    let chan_of: Vec<usize> = ordered
        .iter()
        .map(|s| unique.iter().position(|u| u == s).unwrap())
        .collect();

    Ok(PreparedPlot {
        window_secs: cfg.window_secs,
        interval_ms: cfg.interval_ms,
        ordered,
        groups_idx,
        prepared,
        chan_of,
    })
}

/// 独立子命令入口：打开自己的探针，显示 yaml 配置的曲线
pub fn run(config: &ToolConfig, name: &str) -> Result<()> {
    let prep = prepare_plot(config, name)?;
    let _ = run_prepared_owned(
        &config.probe,
        &config.chip,
        &prep,
        &format!("tscope plot [{name}]"),
    )?;
    Ok(())
}

/// `var --plot`：单符号临时曲线（不依赖 yaml plot 节），自己打开探针。
/// 传入复合类型会明确报错（标量检测）。
pub fn run_adhoc(
    probe: &ProbeConfig,
    chip: &ChipConfig,
    elf: &Path,
    symbol: &str,
    interval_ms: u64,
    title: &str,
) -> Result<()> {
    let cfg = PlotConfig {
        interval_ms,
        window_secs: 5.0,
        groups: vec![vec![symbol.to_string()]],
    };
    let prep = prepare_groups(&cfg, elf)?;
    let _ = run_prepared_owned(probe, chip, &prep, title)?;
    Ok(())
}

// ———————————————————— 馈送模式（debug 会话的曲线窗口子进程） ————————————————————

/// 馈送头部（T/W/C/G 行解析结果）
struct FeedHeader {
    title: String,
    window_secs: f64,
    channels: Vec<String>,
    group_sizes: Vec<usize>,
}

impl FeedHeader {
    /// 图组大小 → 通道索引划分（通道按顺序连续）
    fn groups_idx(&self) -> Vec<Vec<usize>> {
        let mut start = 0;
        self.group_sizes
            .iter()
            .map(|&size| {
                let g = (start..start + size).collect();
                start += size;
                g
            })
            .collect()
    }
}

/// 一条数据行的解析结果（P/S 行；其余行忽略）
enum DataEvent {
    Pause(bool),
    Sample(Sample),
    Ignore,
}

/// 解析一条数据行（宽容：坏行一律忽略，不断流）
fn parse_data_line(line: &str, n: usize) -> DataEvent {
    let line = line.trim_end();
    if let Some(rest) = line.strip_prefix("P ") {
        return match rest.trim() {
            "1" => DataEvent::Pause(true),
            "0" => DataEvent::Pause(false),
            _ => DataEvent::Ignore,
        };
    }
    if let Some(rest) = line.strip_prefix("S ") {
        let mut it = rest.split_whitespace();
        let Some(tok) = it.next() else {
            return DataEvent::Ignore;
        };
        let Ok(t) = tok.parse::<f64>() else {
            return DataEvent::Ignore;
        };
        let mut values: Vec<f64> = it
            .take(n)
            .map(|v| v.parse::<f64>().unwrap_or(f64::NAN))
            .collect();
        values.resize(n, f64::NAN); // 值不足补 NaN（多余忽略）
        return DataEvent::Sample(Sample { t, values });
    }
    DataEvent::Ignore
}

/// 同步读头部（T/W/C/G 行，见模块文档的协议）。G 行是头部终点：
/// 读到 G 行即返回，剩下的数据行交给数据读线程继续消费。
fn read_header<R: BufRead>(r: &mut R) -> Result<FeedHeader> {
    let mut title: Option<String> = None;
    let mut window_secs: Option<f64> = None;
    let mut channels: Vec<String> = Vec::new();
    let mut group_sizes: Vec<usize> = Vec::new();
    for line in r.lines() {
        let line = line.context("读馈送头部失败")?;
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("T ") {
            title = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("W ") {
            window_secs = Some(rest.trim().parse().context("窗口宽度不是数字")?);
        } else if let Some(rest) = line.strip_prefix("C ") {
            channels.push(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("G ") {
            for tok in rest.split_whitespace() {
                group_sizes.push(tok.parse().context("图组大小不是数字")?);
            }
            break; // 头部结束；缓冲里的数据行由数据读线程接着读
        } else {
            bail!("未知的头部行：{line:?}（期望 T/W/C/G 开头的行）");
        }
    }
    let title = title.ok_or_else(|| anyhow!("头部缺少 T 行（窗口标题）"))?;
    let window_secs = window_secs.ok_or_else(|| anyhow!("头部缺少 W 行（窗口宽度）"))?;
    if channels.is_empty() {
        bail!("头部没有 C 行（至少一个通道）");
    }
    if group_sizes.is_empty() {
        bail!("头部缺少 G 行（图组大小）");
    }
    if group_sizes.contains(&0) {
        bail!("图组大小不能为 0");
    }
    let total: usize = group_sizes.iter().sum();
    if total != channels.len() {
        bail!("图组大小之和 {total} 与通道数 {} 不符", channels.len());
    }
    Ok(FeedHeader {
        title,
        window_secs,
        channels,
        group_sizes,
    })
}

/// `plot --feed` 隐藏模式：GUI 进程只消费 stdin 馈送，不碰探针。
/// 窗口关闭（✕）后本进程正常退出；stdin EOF（父进程断开，如调试会话
/// 退出/崩溃）也直接退出，不留无主空窗口。
pub fn run_feed() -> Result<()> {
    let stdin = std::io::stdin();
    // 头部在 Stdin 自带的内部缓冲上直接读：不套外层 BufReader——
    // 外层缓冲跨线程迁移会丢未消费字节；Stdin 的内部缓冲随对象保留，
    // 数据线程重新 lock 后接着读（见下）
    let header = {
        let mut lock = stdin.lock();
        read_header(&mut lock).context("解析馈送头部失败")?
    };
    let n = header.channels.len();
    let groups = header.groups_idx();
    let (sample_tx, sample_rx) = mpsc::channel::<Sample>();
    let (pause_tx, pause_rx) = mpsc::channel::<bool>();

    let mut app = PlotApp::new(
        Box::new(ChannelSource::new(sample_rx, header.channels)),
        PlotOptions {
            window_secs: header.window_secs,
            groups,
            ..Default::default()
        },
    )
    .map_err(|e| anyhow!("曲线配置错误：{e}"))?;
    // 外部暂停模式：暂停/恢复由父进程（debug 主循环按内核状态）驱动，
    // 空格键失效——图像滚动与否只跟随内核运行状态
    app.set_external_pause(pause_rx);

    // 数据读线程：stdin 行 → 样本/暂停两个通道。EOF（父进程断开）直接
    // 退出本进程，不留下无主的空窗口
    std::thread::Builder::new()
        .name("tscope-plot-feed".into())
        .spawn(move || {
            // 同一 Stdin 再 lock：内部缓冲保留头部之后已读入的剩余数据行
            let lock = stdin.lock();
            for line in lock.lines() {
                let Ok(line) = line else { break };
                match parse_data_line(&line, n) {
                    DataEvent::Pause(p) => {
                        if pause_tx.send(p).is_err() {
                            break;
                        }
                    }
                    DataEvent::Sample(s) => {
                        if sample_tx.send(s).is_err() {
                            break;
                        }
                    }
                    DataEvent::Ignore => {}
                }
            }
            std::process::exit(0);
        })
        .context("创建馈送读取线程失败")?;

    run_app(app, &header.title).map_err(|e| anyhow!("GUI 运行失败：{e}"))
}

/// debug 会话 → 曲线窗口子进程的连接（父端）。
/// GUI 必须独立子进程：winit 每进程只允许一个 EventLoop，关窗后同进程
/// 再开新窗必报 "EventLoop can't be recreated"——每次 plot 派生全新
/// `plot --feed` 子进程，开/关/重开天然干净。
pub struct FeedChild {
    pub child: std::process::Child,
    stdin: Option<BufWriter<std::process::ChildStdin>>,
}

/// 派生 `plot --feed` 子进程并写入头部。样本/暂停随后经
/// [`FeedChild::write_sample`] / [`FeedChild::write_pause`] 逐行馈送。
pub fn spawn_feed_child(prep: &PreparedPlot, title: &str) -> Result<FeedChild> {
    let exe =
        std::env::current_exe().context("取不到当前可执行文件路径（无法派生曲线窗口子进程）")?;
    let mut child = std::process::Command::new(exe)
        .args(["plot", "--feed"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("启动曲线窗口子进程失败")?;
    let stdin = BufWriter::new(child.stdin.take().context("拿不到子进程 stdin 管道")?);
    let mut feed = FeedChild {
        child,
        stdin: Some(stdin),
    };
    write_header(
        feed.stdin.as_mut().expect("stdin 刚置为 Some"),
        prep,
        title,
    )
    .context("写子进程馈送头部失败")?;
    Ok(feed)
}

impl FeedChild {
    /// 发暂停/恢复标记（true = 内核暂停 → 曲线冻结）
    pub fn write_pause(&mut self, paused: bool) {
        if let Some(w) = &mut self.stdin {
            let _ = write_pause_line(w, paused);
        }
    }

    /// 发一个样本。写入失败（子进程已退出）静默忽略——关窗检测统一走
    /// [`FeedChild::poll_exit`]，内核暂停时不发数据也能可靠检测关窗
    pub fn write_sample(&mut self, t: f64, values: &[f64]) {
        if let Some(w) = &mut self.stdin {
            let _ = write_sample_line(w, t, values);
        }
    }

    /// 子进程是否已退出（用户关窗/窗口异常）。可重复调用。
    pub fn poll_exit(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// 会话结束：先关写端让子进程经 stdin EOF 自行干净退出，
    /// 短暂等待后仍未退出再强杀兜底（已退出的忽略错误）
    pub fn kill(&mut self) {
        self.stdin = None; // drop 写端 → 子进程读线程 EOF → exit(0)
        for _ in 0..30 {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 写头部（T/W/C/G 行）——与 [`read_header`] 对应，见模块文档的协议
fn write_header<W: Write>(w: &mut W, prep: &PreparedPlot, title: &str) -> std::io::Result<()> {
    writeln!(w, "T {title}")?;
    writeln!(w, "W {}", prep.window_secs)?;
    for name in &prep.ordered {
        writeln!(w, "C {name}")?;
    }
    let sizes: Vec<String> = prep.groups_idx.iter().map(|g| g.len().to_string()).collect();
    writeln!(w, "G {}", sizes.join(" "))?;
    w.flush()
}

/// 写暂停/恢复行（每条都 flush：样本要尽快到 GUI，不能等缓冲满）
fn write_pause_line<W: Write>(w: &mut W, paused: bool) -> std::io::Result<()> {
    writeln!(w, "P {}", u8::from(paused))?;
    w.flush()
}

/// 写样本行（每条都 flush：样本要尽快到 GUI，不能等缓冲满）
fn write_sample_line<W: Write>(w: &mut W, t: f64, values: &[f64]) -> std::io::Result<()> {
    write!(w, "S {t}")?;
    for v in values {
        write!(w, " {v}")?;
    }
    writeln!(w)?;
    w.flush()
}

/// 独立路径用：打开**全新**探针会话跑 GUI（run / run_adhoc 内部调用）。
/// 这里做一次预检挂接（attach 失败立即报错退出，不弹空窗口），
/// 采样线程再挂接一次。
pub fn run_prepared_owned(
    probe: &ProbeConfig,
    chip: &ChipConfig,
    prep: &PreparedPlot,
    title: &str,
) -> Result<Session> {
    let mut session = session::open_session(probe, chip)?;
    // 预检：attach 失败立即报错退出，不弹空窗口（采样线程内部会重新 attach）
    session.core(0).context("attach 内核失败")?;
    let interval = Duration::from_millis(prep.interval_ms.max(10));
    let (tx, rx) = mpsc::channel::<Sample>();

    // —— UI ——
    let source = ChannelSource::new(rx, prep.ordered.clone());
    let app = PlotApp::new(
        Box::new(source),
        PlotOptions {
            window_secs: prep.window_secs,
            groups: prep.groups_idx.clone(),
            ..Default::default()
        },
    )
    .map_err(|e| anyhow!("曲线配置错误：{e}"))?;

    // —— 采样线程（借用会话）+ GUI 主循环（阻塞当前线程） ——
    std::thread::scope(|s| -> Result<()> {
        // 显式重借用：move 闭包只搬走 &mut 引用，Session 本体留在这里
        let session_ref = &mut session;
        s.spawn(move || sampler_loop(session_ref, &prep.prepared, &prep.chan_of, interval, tx));
        run_app(app, title).map_err(|e| anyhow!("GUI 运行失败：{e}"))
    })?;
    Ok(session)
}

/// 采样循环：独占（借用）Session，按固定节拍读符号 → mpsc。
/// UI 关闭（接收端 drop）后 send 失败，循环退出、线程结束。
fn sampler_loop(
    session: &mut Session,
    prepared: &[PreparedSymbol],
    chan_of: &[usize],
    interval: Duration,
    tx: mpsc::Sender<Sample>,
) {
    // 挂接带退避重试：Windows 上 J-Link 的 USB 传输偶发超时
    // （环境问题，与工具逻辑无关），重试可显著提高成功率
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
                eprintln!("采样线程挂接失败（第 5/5 次）：{e:#}，放弃采样（曲线窗口将无数据）");
                break 'attach None;
            }
        }
    };
    let Some(mut core) = core else {
        return;
    };
    let start = Instant::now();
    let mut next = Instant::now();
    loop {
        let t = start.elapsed().as_secs_f64();
        // 读每个唯一符号的字段字节；失败记 NaN（曲线断开）
        let vals: Vec<f64> = prepared
            .iter()
            .map(|p| {
                p.read_field(&mut core)
                    .ok()
                    .and_then(|b| p.value_from_field(&b))
                    .unwrap_or(f64::NAN)
            })
            .collect();
        // 按通道顺序组装（同一符号多图共用）
        let values = chan_of.iter().map(|&i| vals[i]).collect();
        if tx.send(Sample { t, values }).is_err() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    /// 造一个不依赖 ELF 的 PreparedPlot（只测头部/样本协议，不碰符号解析）
    fn prep_fixture() -> PreparedPlot {
        PreparedPlot {
            window_secs: 7.5,
            interval_ms: 20,
            ordered: vec!["theta".into(), "cnt".into(), "gain".into()],
            groups_idx: vec![vec![0, 1], vec![2]],
            prepared: Vec::new(),
            chan_of: vec![0, 1, 2],
        }
    }

    #[test]
    fn header_round_trip() {
        let mut buf = Vec::new();
        write_header(&mut buf, &prep_fixture(), "debug plot [plot1]").unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(
            text,
            "T debug plot [plot1]\nW 7.5\nC theta\nC cnt\nC gain\nG 2 1\n"
        );
        let mut reader = BufReader::new(text.as_bytes());
        let h = read_header(&mut reader).unwrap();
        assert_eq!(h.title, "debug plot [plot1]");
        assert_eq!(h.window_secs, 7.5);
        assert_eq!(h.channels, ["theta", "cnt", "gain"]);
        assert_eq!(h.group_sizes, [2, 1]);
        assert_eq!(h.groups_idx(), vec![vec![0usize, 1], vec![2]]);
        // 头部之后的剩余字节留在 reader 缓冲里，数据读线程接着读（不丢行）
        let rest = reader.lines().collect::<std::io::Result<Vec<_>>>().unwrap();
        assert!(rest.is_empty());
    }

    #[test]
    fn header_errors_are_explicit() {
        let bad = [
            "",                                 // 全缺
            "T t\n",                            // 缺 W/C/G
            "T t\nW 5\n",                       // 缺 C/G
            "T t\nW 5\nC a\nG 2\n",             // 大小之和不符
            "T t\nW x\nC a\nG 1\n",             // W 不是数字
            "T t\nW 5\nC a\nG 0\n",             // 空图组
            "T t\nW 5\nC a\nG 1 1\n",           // 大小之和超出
            "X nope\n",                         // 未知行
        ];
        for bad in &bad {
            assert!(
                read_header(&mut BufReader::new(bad.as_bytes())).is_err(),
                "应报错：{bad:?}"
            );
        }
    }

    #[test]
    fn sample_line_round_trip_and_padding() {
        let mut buf = Vec::new();
        write_sample_line(&mut buf, 1.25, &[2.5, f64::NAN]).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "S 1.25 2.5 NaN\n");
        // 写端 → 读端往返：NaN 保真
        let DataEvent::Sample(s) = parse_data_line("S 1.25 2.5 NaN", 2) else {
            panic!("应解析为样本");
        };
        assert_eq!(s.t, 1.25);
        assert_eq!(s.values.len(), 2);
        assert_eq!(s.values[0], 2.5);
        assert!(s.values[1].is_nan());
        // 值不足补 NaN
        let DataEvent::Sample(s) = parse_data_line("S 1.25 2.5", 3) else {
            panic!()
        };
        assert_eq!(s.values.len(), 3);
        assert_eq!(s.values[0], 2.5);
        assert!(s.values[1].is_nan());
        assert!(s.values[2].is_nan());
        // 多余的值忽略
        let DataEvent::Sample(s) = parse_data_line("S 1.25 2.5 3 4", 2) else {
            panic!()
        };
        assert_eq!(s.values, [2.5, 3.0]);
    }

    #[test]
    fn pause_and_garbage_lines() {
        assert!(matches!(parse_data_line("P 1", 2), DataEvent::Pause(true)));
        assert!(matches!(parse_data_line("P 0", 2), DataEvent::Pause(false)));
        assert!(matches!(parse_data_line("P 2", 2), DataEvent::Ignore));
        assert!(matches!(parse_data_line("", 2), DataEvent::Ignore));
        assert!(matches!(parse_data_line("S abc 1", 2), DataEvent::Ignore));
        assert!(matches!(parse_data_line("随便一行", 2), DataEvent::Ignore));
    }

    #[test]
    fn pause_line_format() {
        let mut buf = Vec::new();
        write_pause_line(&mut buf, true).unwrap();
        write_pause_line(&mut buf, false).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "P 1\nP 0\n");
    }
}
