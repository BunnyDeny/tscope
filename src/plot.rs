//! plot 子命令：独立 GUI 窗口显示变量实时曲线（基于 crates/tscope-plot 库）。
//!
//! 配置来自 tscope.yaml 的 plot 节（结构类似 watch 组）：
//! 每个图组 = 一个子图，组内符号同图共 Y 轴，多组上下叠放、共享 X 轴联动。
//!
//! 三个入口：
//! - [`run`]：独立子命令，自己打开探针；
//! - [`run_adhoc`]：`var --plot` 的单符号临时曲线，自己打开探针；
//! - [`run_with_session`]：给定会话启动曲线（run / run_adhoc 内部使用；
//!   Windows 实测教训——debug 会话不能复用它，重复挂接会让 J-Link 的
//!   WinUSB 传输失步，debug 里的 plot 改走 run() 独立路径）。
//!
//! 架构：采样线程独占 probe-rs Session（Session 非 Sync），按周期读符号
//! 数值经 mpsc 送出；UI 侧用 tscope_plot::ChannelSource 包装，PlotApp 只认
//! 「通道名 + (时刻, 数值) 流」——UI 层与硬件完全解耦。
//! 复用会话时采样线程经 std::thread::scope 借用 &mut Session。
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

/// 查找 yaml 里的曲线配置（run / debug 的 plot 命令共用）
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

/// 独立子命令入口：打开自己的探针，显示 yaml 配置的曲线
pub fn run(config: &ToolConfig, name: &str) -> Result<()> {
    let cfg = resolve_plot(config, name)?;
    let elf = config.firmware_image()?;
    let mut session = session::open_session(&config.probe, &config.chip)?;
    run_with_session(&mut session, cfg, elf, &format!("tscope plot [{name}]"))
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
    let mut session = session::open_session(probe, chip)?;
    run_with_session(&mut session, &cfg, elf, title)
}

/// 在给定会话上启动曲线 GUI（run / run_adhoc 内部使用：先开自己的
/// 全新会话再传入）。GUI 期间调用方阻塞；窗口关闭后 scope 结束、
/// 采样线程退出，会话完好。**不要**在 debug 会话里复用（Windows 上
/// J-Link 会 bulk read 失步），debug 的 plot 走 run() 独立路径。
pub fn run_with_session(
    session: &mut Session,
    cfg: &PlotConfig,
    elf: &Path,
    title: &str,
) -> Result<()> {
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

    // —— 预检：attach 失败立即报错退出，不弹空窗口（采样线程内部会重新 attach） ——
    session.core(0).context("attach 内核失败")?;
    let interval = Duration::from_millis(cfg.interval_ms.max(10));
    let (tx, rx) = mpsc::channel::<Sample>();

    // —— UI ——
    let source = ChannelSource::new(rx, ordered);
    let app = PlotApp::new(
        Box::new(source),
        PlotOptions {
            window_secs: cfg.window_secs,
            groups: groups_idx,
            ..Default::default()
        },
    )
    .map_err(|e| anyhow!("曲线配置错误：{e}"))?;

    // —— 采样线程（借用会话）+ GUI 主循环（阻塞当前线程） ——
    std::thread::scope(|s| -> Result<()> {
        s.spawn(move || sampler_loop(session, &prepared, &chan_of, interval, tx));
        run_app(app, title).map_err(|e| anyhow!("GUI 运行失败：{e}"))
    })
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
    let mut core = match session.core(0) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("采样线程 attach 内核失败：{e:#}");
            return;
        }
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
