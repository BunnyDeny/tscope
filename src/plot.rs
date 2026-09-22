//! plot 子命令：独立 GUI 窗口显示变量实时曲线（基于 crates/tscope-plot 库）。
//!
//! 配置来自 tscope.yaml 的 plot 节（结构类似 watch 组）：
//! 每个图组 = 一个子图，组内符号同图共 Y 轴，多组上下叠放、共享 X 轴联动。
//!
//! 三个入口：
//! - [`run`]：独立子命令，自己打开探针；
//! - [`run_adhoc`]：`var --plot` 的单符号临时曲线，自己打开探针；
//! - [`run_prepared_owned`]：debug 会话里的 `plot` 命令——由 debug 侧先
//!   [`prepare_plot`]（纯 CPU）再调用；它打开**全新**会话跑 GUI，关窗后
//!   把会话**归还**给 debug，全程不存在"关闭后立刻重开"的脆弱交接。
//!
//! 架构：采样线程独占 probe-rs Session（Session 非 Sync），按周期读符号
//! 数值经 mpsc 送出；UI 侧用 tscope_plot::ChannelSource 包装，PlotApp 只认
//! 「通道名 + (时刻, 数值) 流」——UI 层与硬件完全解耦。
//!
//! Windows 实测教训（重要）：J-Link 的 WinUSB 传输对「同一会话上重复挂接 /
//! 关会话后立即重开会话」都很脆弱（bulk read 失步且粘死）。因此挂接次数
//! 压到最低：
//! - debug 里的 plot：**直接借用** debug 会话，不做预检、采样线程只挂接
//!   一次（run_reused），全程零交接；
//! - 独立路径（run / run_adhoc）：全新会话 + 一次预检挂接
//!   （run_prepared_owned），两个平台验证正常。
//!
//! 同一符号可出现在多个图组：只采样一次（去重），按通道顺序复用数值。

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

/// debug 的 plot 命令用：**直接借用** debug 已打开的会话（用户直觉正确：
/// debug 能进来说明探针本来就通，没必要再开关一次）。与独立路径的区别：
/// 不做预检挂接（历史教训——在"用过多次的会话"上额外挂接是 Windows 下
/// 超时高发点），采样线程只挂接一次；GUI 期间 REPL 阻塞，关窗后会话
/// 原地归还，全程零交接。
pub fn run_reused(session: &mut Session, prep: &PreparedPlot, title: &str) -> Result<()> {
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

    // —— 采样线程（借用会话，只挂接一次）+ GUI 主循环（阻塞当前线程） ——
    std::thread::scope(|s| -> Result<()> {
        // 显式重借用：move 闭包只搬走 &mut 引用，Session 本体留在调用方
        let session_ref = &mut *session;
        s.spawn(move || sampler_loop(session_ref, &prep.prepared, &prep.chan_of, interval, tx));
        run_app(app, title).map_err(|e| anyhow!("GUI 运行失败：{e}"))
    })
}

/// 独立路径用：打开**全新**探针会话跑 GUI（run / run_adhoc 内部调用）。
/// 与 run_reused 的区别：这里做一次预检挂接（全新会话上安全），
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
