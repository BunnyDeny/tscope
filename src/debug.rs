//! debug 子命令：交互式调试会话（提示符 `> `）。
//!
//! 设计（与项目约定一致）：
//! - 会话开始时 attach 一次，整个会话复用同一个 Session；
//! - 每个命令独立执行、独立报错——单条命令出错**不会**退出会话；
//! - `watch` 命令复用 watch 模块：进入交替屏持续显示，按 q 恢复终端
//!   并回到提示符（会话与探针连接保持不变）；
//! - `bp` 的断点地址可写十六进制或函数名（从 firmware.elf 符号表解析，
//!   自动清 Thumb 位）；命中断点后打印 PC 与所在函数，并自动清除断点；
//! - halt/step/regs/pc 在运行时会自动先暂停内核并提示，暂停后保持（run 恢复）；
//! - 所有操作只动 CPU 调试逻辑，不碰 flash（bootloader 安全）；
//! - `plot` 曲线窗口异步运行：探针唯一属主是 REPL 主循环（rustyline 输入
//!   在独立线程），GUI 线程纯消费数据；曲线滚动/暂停只跟随内核运行状态。

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use probe_rs::{Core, Session};
use tscope_plot::{run_app, ChannelSource, PlotApp, PlotOptions, Sample};

use crate::backtrace;
use crate::config::ToolConfig;
use crate::plot;
use crate::session;
use crate::symbol::{self, PreparedSymbol};
use crate::watch;

/// 暂停内核时常用的超时
const HALT_TIMEOUT: Duration = Duration::from_millis(1000);

/// 等待断点命中的超时
const BP_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// debug 会话内 plot 窗口的连接：主循环是**探针唯一属主**，
/// 采样数据经 tx 发给 GUI 线程（GUI 纯消费）；内核状态变化经 ctrl
/// 通知暂停/恢复——图像滚动与否只跟随内核运行状态。
struct PlotLink {
    tx: mpsc::Sender<Sample>,
    ctrl: mpsc::Sender<bool>,
    /// 去重后的符号解析结果（只读字段字节）
    prepared: Vec<PreparedSymbol>,
    /// 通道顺序 → prepared 下标
    chan_of: Vec<usize>,
    interval: Duration,
    next_tick: Instant,
    started: Instant,
    /// 上次采样时的内核运行状态（用于暂停/恢复标记去重）
    was_running: Option<bool>,
    /// GUI 线程关窗后的"已关闭"通知（内核暂停时不发数据，
    /// 靠 tx.send 失败检测不到关窗——必须显式通知）
    closed_rx: mpsc::Receiver<()>,
    window_open: bool,
}

/// 采样一个节拍（`running` 由调用方判定）：状态变化发暂停标记；
/// 内核在跑则读符号发样本。返回窗口是否还开着。
fn plot_sample(core: &mut Core, link: &mut PlotLink, running: bool) -> bool {
    if link.was_running != Some(running) {
        link.was_running = Some(running);
        let _ = link.ctrl.send(!running); // true = 暂停
    }
    if !running {
        return link.window_open;
    }
    let t = link.started.elapsed().as_secs_f64();
    let vals: Vec<f64> = link
        .prepared
        .iter()
        .map(|p| {
            p.read_field(core)
                .ok()
                .and_then(|b| p.value_from_field(&b))
                .unwrap_or(f64::NAN)
        })
        .collect();
    let values = link.chan_of.iter().map(|&i| vals[i]).collect();
    if link.tx.send(Sample { t, values }).is_err() {
        link.window_open = false; // GUI 已关闭
    }
    link.window_open
}

/// 空闲节拍：自取内核状态后采样（主循环调用）
fn plot_tick(session: &mut Session, link: &mut PlotLink) {
    if Instant::now() < link.next_tick {
        return;
    }
    link.next_tick += link.interval;
    if let Ok(mut core) = session.core(0) {
        let running = core.core_halted().map(|h| !h).unwrap_or(false);
        plot_sample(&mut core, link, running);
    }
}

/// 进入交互式调试会话。
///
/// 架构（plot 异步化的关键）：**探针唯一属主**——Session 只被主循环
/// 一个线程持有。rustyline 输入跑在独立线程，命令经 mpsc 送达主循环；
/// plot 窗口跑在 GUI 线程、纯消费数据。图像滚动与否只跟随内核运行状态
/// （主循环每个采样节拍读 DHCSR 判定），与命令阻塞与否无关。
pub fn run(config: &ToolConfig) -> Result<()> {
    let elf = config.firmware_image().ok().map(|p| p.to_path_buf());
    let mut session = session::open_session(&config.probe, &config.chip)?;

    println!("tscope 调试会话已建立。输入 help 查看命令，q 退出。");
    println!("提示：halt/step/regs/pc 会自动暂停运行中的内核（暂停后保持，run 恢复运行）；bp 命中后内核保持暂停。");
    println!("行编辑：左右光标移动，↑/↓ 翻阅历史命令（跨会话保存）。");
    println!("plot 曲线窗口异步运行：REPL 不阻塞，曲线滚动跟随内核运行状态；与 watch 互斥。");

    // —— 输入线程：rustyline 阻塞读，命令经 mpsc 送主循环 ——
    // 提示符握手：主循环处理完命令、输出完毕后才发令牌放行下一次读入，
    // 保证「输出在提示符之前」的严格顺序（双线程 REPL 的经典坑）
    enum InputMsg {
        Line(String),
        Eof,
    }
    let (cmd_tx, cmd_rx) = mpsc::channel::<InputMsg>();
    let (prompt_tx, prompt_rx) = mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("tscope-input".into())
        .spawn(move || {
            let mut rl = match rustyline::DefaultEditor::new() {
                Ok(rl) => rl,
                Err(e) => {
                    eprintln!("初始化行编辑器失败：{e}");
                    return;
                }
            };
            if let Some(path) = history_path() {
                let _ = rl.load_history(&path);
            }
            'outer: loop {
                if prompt_rx.recv().is_err() {
                    break; // 主循环已退出
                }
                'read: loop {
                    match rl.readline("> ") {
                        Ok(line) => {
                            // 每次读到就存历史：退出路径不经过本线程时也不丢历史
                            let _ = rl.add_history_entry(&line);
                            if let Some(path) = history_path() {
                                let _ = rl.save_history(&path);
                            }
                            if cmd_tx.send(InputMsg::Line(line)).is_err() {
                                break 'outer;
                            }
                            break 'read; // 等下一个令牌再画提示符
                        }
                        Err(rustyline::error::ReadlineError::Interrupted) => {
                            println!("（Ctrl-C：输入 q 退出会话）");
                            continue 'read; // Ctrl-C 后继续读，不消耗令牌
                        }
                        Err(_) => {
                            let _ = cmd_tx.send(InputMsg::Eof);
                            break 'outer;
                        }
                    }
                }
            }
        })
        .context("创建输入线程失败")?;
    // 放行第一次输入
    let _ = prompt_tx.send(());

    // 会话内的断点集合：跨 run/halt/reset 保持，退出会话即消失
    let mut breakpoints: Vec<Breakpoint> = Vec::new();
    // 当前打开的曲线窗口连接（一次一个）
    let mut plot_ctx: Option<PlotLink> = None;

    loop {
        // —— 取一条命令：无曲线窗口时阻塞等待；有则 10ms 超时轮询以保持采样 ——
        let msg = if plot_ctx.as_ref().is_some_and(|l| l.window_open) {
            match cmd_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match cmd_rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break, // 输入线程退出（Ctrl-D）
            }
        };
        let line = match msg {
            Some(InputMsg::Line(l)) => Some(l),
            Some(InputMsg::Eof) => break,
            None => None,
        };

        if let Some(line) = line {
            let line = line.trim();
            let mut quit = false;
            if !line.is_empty() {
                let mut parts = line.split_whitespace();
                let cmd = parts.next().unwrap();
                let arg = parts.next();

                let result: Result<()> = match cmd {
                    "help" | "h" | "?" => {
                        print_help();
                        Ok(())
                    }
                    "q" | "quit" | "exit" => {
                        quit = true;
                        Ok(())
                    }
                    "halt" => debug_halt(&mut session),
                    "run" | "continue" | "c" => {
                        debug_run(&mut session, elf.as_deref(), &breakpoints, plot_ctx.as_mut())
                    }
                    "step" | "s" | "next" | "n" => debug_step(&mut session, elf.as_deref()),
                    "stepi" | "si" => debug_stepi(&mut session, elf.as_deref()),
                    "finish" | "fin" | "f" => {
                        debug_finish(&mut session, elf.as_deref(), plot_ctx.as_mut())
                    }
                    "regs" => debug_regs(&mut session),
                    "pc" => debug_pc(&mut session, elf.as_deref()),
                    "bt" | "backtrace" => {
                        let max = arg.and_then(|a| a.parse::<usize>().ok()).unwrap_or(20);
                        debug_bt(&mut session, elf.as_deref(), max)
                    }
                    "list" | "l" => match arg {
                        Some(loc) => debug_list_at(elf.as_deref(), loc),
                        None => debug_list_current(&mut session, elf.as_deref()),
                    },
                    "reset" | "rst" => {
                        let target = arg.unwrap_or("main");
                        // 注意：命令分支里的 ? 必须包在闭包里，否则会直接返回 run()
                        // 退出整个会话（主循环的错误捕获就失效了）
                        (|| -> Result<()> {
                            let elf = elf.as_deref().ok_or_else(|| {
                                anyhow!("reset 需要固件镜像（firmware.elf / firmware.axf）来解析函数地址（复位后暂停在函数开头）")
                            })?;
                            debug_reset(&mut session, elf, target, &breakpoints, plot_ctx.as_mut())
                        })()
                    }
                    "bp" => match arg {
                        Some(target) => {
                            debug_bp_add(&mut session, elf.as_deref(), target, &mut breakpoints)
                        }
                        None => Err(anyhow!(
                            "用法：bp <地址|函数名|文件:行号>，如 bp main / bp 0x08004200 / bp foc.c:123"
                        )),
                    },
                    "bl" => debug_bp_list(&mut session, &breakpoints),
                    "bc" => match arg {
                        Some(which) => debug_bp_clear(&mut session, which, &mut breakpoints),
                        None => Err(anyhow!("用法：bc <断点编号|all>，如 bc 1 / bc all")),
                    },
                    "var" => {
                        (|| -> Result<()> {
                            let expr = arg.ok_or_else(|| anyhow!("用法：var <表达式>，如 var theta_ref 或 var ENC_1_POS_SENSOR.readAngleCmd"))?;
                            let elf = elf.as_deref().ok_or_else(|| {
                                anyhow!(
                                    "配置里没有可用的固件镜像（firmware.elf / firmware.axf），无法解析符号"
                                )
                            })?;
                            let mut core = session.core(0)?;
                            let opts = symbol::FmtOptions {
                                max_elems: Some(16),
                                max_depth: 3,
                            };
                            symbol::print_global_value(elf, expr, &mut core, &opts)
                        })()
                    }
                    "watch" | "w" => {
                        if plot_ctx.as_ref().is_some_and(|l| l.window_open) {
                            Err(anyhow!("曲线窗口打开期间不支持 watch（两者互斥）；请先关闭曲线窗口"))
                        } else {
                            match arg {
                                Some(group) => watch::run_with_session(config, group, &mut session),
                                None => watch::list_groups(config),
                            }
                        }
                    }
                    "plot" => {
                        // 曲线窗口异步打开：GUI 线程纯消费数据，主循环继续响应命令；
                        // 采样由主循环按内核运行状态驱动（见 plot_tick / plot_sample）
                        (|| -> Result<()> {
                            let Some(name) = arg else {
                                plot::list_plots(config)?;
                                return Ok(());
                            };
                            if plot_ctx.as_ref().is_some_and(|l| l.window_open) {
                                bail!("已有曲线窗口打开（一次只支持一个）；关闭后再试");
                            }
                            let prep = plot::prepare_plot(config, name)?;
                            let (tx, data_rx) = mpsc::channel::<Sample>();
                            let (ctrl_tx, ctrl_rx) = mpsc::channel::<bool>();
                            let (closed_tx, closed_rx) = mpsc::channel::<()>();
                            let ordered = prep.ordered.clone();
                            let groups = prep.groups_idx.clone();
                            let window_secs = prep.window_secs;
                            let title = format!("debug plot [{name}]");
                            std::thread::Builder::new()
                                .name("tscope-plot-gui".into())
                                .spawn(move || {
                                    // 关窗后（或启动失败时）无论哪条路径退出都发关闭通知，
                                    // 主循环据此清理 plot_ctx——内核暂停时采样不发数据，
                                    // 不能依赖 tx.send 失败来检测关窗
                                    let closed = closed_tx;
                                    let source = ChannelSource::new(data_rx, ordered);
                                    let mut app = match PlotApp::new(
                                        Box::new(source),
                                        PlotOptions {
                                            window_secs,
                                            groups,
                                            ..Default::default()
                                        },
                                    ) {
                                        Ok(a) => a,
                                        Err(e) => {
                                            eprintln!("曲线配置错误：{e}");
                                            let _ = closed.send(());
                                            return;
                                        }
                                    };
                                    // 外部暂停模式：空格键失效，跟随内核状态
                                    app.set_external_pause(ctrl_rx);
                                    if let Err(e) = run_app(app, &title) {
                                        eprintln!("曲线窗口线程运行失败：{e}");
                                    }
                                    let _ = closed.send(());
                                })
                                .context("创建曲线窗口线程失败")?;
                            plot_ctx = Some(PlotLink {
                                tx,
                                ctrl: ctrl_tx,
                                prepared: prep.prepared,
                                chan_of: prep.chan_of,
                                interval: Duration::from_millis(prep.interval_ms.max(10)),
                                next_tick: Instant::now(),
                                started: Instant::now(),
                                was_running: None,
                                closed_rx,
                                window_open: true,
                            });
                            println!("曲线窗口已打开（异步）：REPL 可继续输入命令；曲线滚动跟随内核运行状态；关窗自动停止");
                            Ok(())
                        })()
                    }
                    other => {
                        println!("未知命令 {other}（help 查看命令列表）");
                        Ok(())
                    }
                };

                if let Err(e) = result {
                    println!("错误：{e:#}");
                }
            }
            if quit {
                break;
            }
            // 输出完毕（空行也一样）放行输入线程绘制下一次提示符——
            // 空行若不发令牌，输入线程会永远等下去，提示符消失
            let _ = prompt_tx.send(());
        }

        // —— 采样节拍：内核状态驱动（空闲与阻塞命令内都会走到） ——
        if let Some(link) = plot_ctx.as_mut() {
            // GUI 线程的关窗通知优先于数据通道检测（内核暂停时后者失效）
            if link.closed_rx.try_recv().is_ok() {
                link.window_open = false;
            }
            if link.window_open {
                plot_tick(&mut session, link);
            } else {
                println!("曲线窗口已关闭，停止采样");
                plot_ctx = None; // 清理连接
            }
        }
    }

    println!("已退出调试会话");
    Ok(())
}

/// 历史文件路径：~/.tscope_history（Windows 用 USERPROFILE）
fn history_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(std::path::PathBuf::from(home).join(".tscope_history"))
}

/// 会话内断点：地址 + 用户输入的目标描述
struct Breakpoint {
    addr: u64,
    desc: String,
}

fn print_help() {
    println!(
        r#"可用命令：
  bp <目标>          添加断点：地址 / 函数名 / 文件:行号（bp foc.c:123）
  bl                 列出所有断点（含硬件断点占用情况）
  bc <编号|all>      删除断点，如 bc 1 / bc all
  run                全速运行（continue / c 同义）；有断点时命中停下并报现场
  halt               暂停内核
  step               单步一行源码（s/next/n 同义）；函数末尾自动走出，
                     中断函数末尾自动越过异常返回（回到被打断的代码）
  stepi              单步一条机器指令（si 同义；-O2 下行号会跳）
  finish             运行到当前函数返回（f / fin 同义；中断函数回到被打断处）
  reset [函数名]     复位并暂停在函数开头，默认 main（rst 同义）
  regs               导出全部内核寄存器
  pc                 打印 PC / SP / LR 及所在位置（文件:行号 + 函数）
  bt [帧数]          打印函数调用栈（backtrace 同义；默认最多 20 帧）
  list [文件:行号]   显示当前 PC 附近源码；带参数显示指定位置（l 同义）
  var <表达式>       一次性读取全局变量（与 var 子命令相同）
  watch [组名]       持续显示监视组（w 同义；不带参数列出所有组）；q 返回提示符
  plot [配置名]      GUI 窗口显示曲线（异步：不阻塞命令输入，曲线滚动跟随
                      内核运行状态；与 watch 互斥；不带参数列出所有配置）
  help               显示本帮助
  q                  退出调试会话（quit / exit 同义）

断点目标三种写法：0x08004200（十六进制地址）/ main（函数名）/
foc.c:123（文件:行号）。文件路径支持完整路径、后缀、文件名匹配。
函数名与行号都从 firmware.elf 解析；静态函数与局部变量不在符号表里。
断点数量上限 = 芯片硬件断点数（本芯片 8 个）；芯片复位会清空断点，
tscope 会自动恢复，无需重新 bp。"#
    );
}

// ===========================================================================
// 各命令实现
// ===========================================================================

/// 运行中则自动先暂停（暂停成功后才提示），返回暂停后的内核
fn ensure_halted(core: &mut Core, verbose: bool) -> Result<()> {
    if !core.core_halted()? {
        core.halt(HALT_TIMEOUT)
            .context("暂停失败（超时？接线/供电？）")?;
        if verbose {
            // 提示放在暂停成功之后：描述「已发生的动作」，而不是指引用户去暂停
            println!("（内核在运行，已自动暂停）");
        }
    }
    Ok(())
}

fn debug_halt(session: &mut Session) -> Result<()> {
    let mut core = session.core(0)?;
    let info = core
        .halt(HALT_TIMEOUT)
        .context("暂停失败（超时？接线/供电？）")?;
    println!("已暂停，PC = 0x{:08x}", info.pc);
    Ok(())
}

fn debug_run(
    session: &mut Session,
    elf: Option<&std::path::Path>,
    breakpoints: &[Breakpoint],
    plot: Option<&mut PlotLink>,
) -> Result<()> {
    let mut core = session.core(0)?;
    if !core.core_halted()? {
        println!("内核正在运行（未暂停）。若需要从头启动固件，请用 rst（内部先复位并运行到 main）");
        return Ok(());
    }

    core.run().context("继续运行失败")?;

    if breakpoints.is_empty() {
        println!("已继续运行");
        return Ok(());
    }

    println!(
        "已继续运行，等待断点命中（{} 秒超时）…",
        BP_WAIT_TIMEOUT.as_secs()
    );
    match wait_halted_with_plot(&mut core, plot) {
        Ok(true) => {
            let pc: u32 = core
                .read_core_reg(core.registers().pc().context("找不到 PC 寄存器定义")?.id())
                .context("读 PC 失败")?;
            let pc = (pc & !1) as u64;
            match breakpoints.iter().position(|b| b.addr == pc) {
                Some(idx) => {
                    print!("断点 {}（{}）命中：", idx + 1, breakpoints[idx].desc);
                    print_pc(pc, elf);
                }
                None => {
                    // 停在别处（如主动 halt、异常），如实报告
                    print!("内核已暂停：");
                    print_pc(pc, elf);
                }
            }
        }
        Ok(false) => {
            println!(
                "{} 秒内未命中断点（内核仍在运行；halt 可暂停查看）",
                BP_WAIT_TIMEOUT.as_secs()
            );
        }
        Err(e) => return Err(e.context("等待断点失败")),
    }
    Ok(())
}

/// 添加持久断点（会话内有效，跨 run/halt/reset 保持）
fn debug_bp_add(
    session: &mut Session,
    elf: Option<&std::path::Path>,
    target: &str,
    breakpoints: &mut Vec<Breakpoint>,
) -> Result<()> {
    let addr = resolve_target(elf, target)?;

    if breakpoints.iter().any(|b| b.addr == addr) {
        println!(
            "0x{addr:08x} 已有断点（{}）",
            breakpoints.iter().find(|b| b.addr == addr).unwrap().desc
        );
        return Ok(());
    }

    let mut core = session.core(0)?;
    let units = core.available_breakpoint_units()? as usize;
    if breakpoints.len() >= units {
        bail!("硬件断点已满（共 {units} 个）：先用 bc 删除再添加（本工具只支持硬件断点）");
    }

    ensure_halted(&mut core, true)?;
    core.set_hw_breakpoint(addr)
        .with_context(|| format!("设置断点 @ 0x{addr:08x} 失败"))?;

    breakpoints.push(Breakpoint {
        addr,
        desc: target.to_string(),
    });
    println!("断点 {}：{target} @ 0x{addr:08x}", breakpoints.len());
    Ok(())
}

/// 列出所有断点
fn debug_bp_list(session: &mut Session, breakpoints: &[Breakpoint]) -> Result<()> {
    if breakpoints.is_empty() {
        println!("（没有断点）");
        return Ok(());
    }
    let units = session.core(0)?.available_breakpoint_units()?;
    println!("断点列表（{} / {units} 个硬件断点）：", breakpoints.len());
    for (i, b) in breakpoints.iter().enumerate() {
        println!("  {}：{} @ 0x{:08x}", i + 1, b.desc, b.addr);
    }
    Ok(())
}

/// 删除断点：编号（1 起）或 all
fn debug_bp_clear(
    session: &mut Session,
    which: &str,
    breakpoints: &mut Vec<Breakpoint>,
) -> Result<()> {
    let mut core = session.core(0)?;
    ensure_halted(&mut core, true)?;

    if which == "all" {
        for b in breakpoints.iter() {
            core.clear_hw_breakpoint(b.addr)
                .with_context(|| format!("清除断点 @ 0x{:08x} 失败", b.addr))?;
        }
        let n = breakpoints.len();
        breakpoints.clear();
        println!("已删除全部 {n} 个断点");
        return Ok(());
    }

    let idx: usize = which
        .parse()
        .ok()
        .filter(|i| *i >= 1 && *i <= breakpoints.len())
        .ok_or_else(|| {
            anyhow!(
                "无效的断点编号 {which}（当前共 {} 个，用 bl 查看）",
                breakpoints.len()
            )
        })?;

    let b = &breakpoints[idx - 1];
    let addr = b.addr;
    let desc = b.desc.clone();
    core.clear_hw_breakpoint(addr)
        .with_context(|| format!("清除断点 @ 0x{addr:08x} 失败"))?;
    breakpoints.remove(idx - 1);
    println!("已删除断点 {idx}（{desc}）");
    Ok(())
}

/// 等待内核暂停（断点命中/主动 halt）。期间若 plot 窗口打开且内核在跑，
/// 采样照常进行——图像滚动只跟随内核运行状态，与命令是否阻塞无关。
/// 返回是否在超时内暂停。
fn wait_halted_with_plot(core: &mut Core, mut plot: Option<&mut PlotLink>) -> Result<bool> {
    let deadline = Instant::now() + BP_WAIT_TIMEOUT;
    loop {
        if let Some(link) = plot.as_deref_mut() {
            if Instant::now() >= link.next_tick {
                link.next_tick += link.interval;
                plot_sample(core, link, true); // 等待期间内核在跑
            }
        }
        if core.core_halted()? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_pc(core: &mut probe_rs::Core) -> Result<u64> {
    let pc: u32 = core
        .read_core_reg(core.registers().pc().context("找不到 PC 寄存器定义")?.id())
        .context("读 PC 失败")?;
    Ok(pc as u64)
}

/// 设断点全速运行到 addr（内核需已暂停）。命中返回 true；超时则停下返回 false。
/// 运行期间 plot 采样照常（等待循环里集成采样节拍）。
fn run_to_address(
    core: &mut probe_rs::Core,
    addr: u64,
    plot: Option<&mut PlotLink>,
) -> Result<bool> {
    core.set_hw_breakpoint(addr)
        .with_context(|| format!("设置临时断点 @ 0x{addr:08x} 失败"))?;
    core.run().context("继续运行失败")?;
    let hit = wait_halted_with_plot(core, plot)?;
    if !hit {
        core.halt(HALT_TIMEOUT).context("暂停失败")?;
    }
    core.clear_hw_breakpoint(addr)
        .with_context(|| format!("清除临时断点 @ 0x{addr:08x} 失败"))?;
    Ok(hit)
}

/// 走出当前函数：普通函数断点停到返回地址；中断函数单步越过异常返回
/// （走过 pop+bx lr 后自然落在被打断的代码，不读异常帧、不依赖 FPU）
fn run_out_of_function(
    session: &mut Session,
    elf: &std::path::Path,
    plot: Option<&mut PlotLink>,
) -> Result<()> {
    let mut core = session.core(0)?;
    ensure_halted(&mut core, true)?;
    let start_pc = read_pc(&mut core)?;
    let start_func = symbol::function_name_at(elf, start_pc);

    match backtrace::frame_return_address(elf, &mut core) {
        Some(ra) if backtrace::is_exc_return(ra) => {
            // 中断函数最外层：硬件单步越过尾声（pop + bx lr），
            // 下一步自然停在被打断的代码（如 main 的 while 循环）
            let mut escaped = false;
            for _ in 0..64 {
                core.step().context("单步失败")?;
                let p = read_pc(&mut core)?;
                if symbol::function_name_at(elf, p) != start_func {
                    print_pc(p, Some(elf));
                    escaped = true;
                    break;
                }
            }
            if !escaped {
                println!("（单步 64 次仍未离开当前函数，可能仍在中断上下文中）");
                print_pc(read_pc(&mut core)?, Some(elf));
            }
        }
        Some(ra) => {
            let addr = ra & !1;
            if run_to_address(&mut core, addr, plot)? {
                print_pc(read_pc(&mut core)?, Some(elf));
            } else {
                println!("（未运行到返回地址，已暂停在当前处）");
                print_pc(read_pc(&mut core)?, Some(elf));
            }
        }
        None => {
            // 无展开信息：退化为指令单步
            let info = core.step().context("单步失败")?;
            print_pc(info.pc, Some(elf));
        }
    }
    Ok(())
}

fn debug_step(session: &mut Session, elf: Option<&std::path::Path>) -> Result<()> {
    let mut core = session.core(0)?;
    ensure_halted(&mut core, true)?;
    let start_pc = read_pc(&mut core)?;

    // 无行号信息（汇编/库代码）：直接指令单步
    let Some((start_file, start_line)) = elf.and_then(|e| symbol::address_to_line(e, start_pc))
    else {
        let info = core.step().context("单步失败")?;
        print_pc(info.pc, elf);
        return Ok(());
    };

    // 源码级单步（gdb 同款做法）：逐指令单步，直到
    //   ① 源码行号变化（走到下一行）；
    //   ② 离开当前函数（走出函数/中断返回）；
    // 这样在互斥分支的状态机里也永远停在"真实执行过的行"，不会像
    // 断点法那样等一个不执行的分支而超时。上限 200 条指令防长循环。
    let start_func = elf.and_then(|e| symbol::function_name_at(e, start_pc));
    for _ in 0..200 {
        core.step().context("单步失败")?;
        let p = read_pc(&mut core)?;

        if elf.and_then(|e| symbol::function_name_at(e, p)) != start_func {
            // 走出函数（含中断函数经异常返回落到被打断的代码）
            print_pc(p, elf);
            return Ok(());
        }
        if let Some((f, l)) = elf.and_then(|e| symbol::address_to_line(e, p)) {
            if f != start_file || l != start_line {
                print_pc(p, elf);
                return Ok(());
            }
        }
    }
    println!("（单步 200 条指令仍未走到下一行，可能在长循环中）");
    print_pc(read_pc(&mut core)?, elf);
    Ok(())
}

/// finish：一步运行到当前函数返回（中断函数则运行到被打断的代码）
fn debug_finish(
    session: &mut Session,
    elf: Option<&std::path::Path>,
    plot: Option<&mut PlotLink>,
) -> Result<()> {
    let e = elf.ok_or_else(|| anyhow!("finish 需要固件镜像（展开信息）"))?;
    run_out_of_function(session, e, plot)
}

/// 指令级单步：一次执行一条机器指令（不管源码行）
fn debug_stepi(session: &mut Session, elf: Option<&std::path::Path>) -> Result<()> {
    let mut core = session.core(0)?;
    ensure_halted(&mut core, true)?;
    let info = core.step().context("单步失败")?;
    print_pc(info.pc, elf);
    Ok(())
}

fn debug_regs(session: &mut Session) -> Result<()> {
    let mut core = session.core(0)?;
    ensure_halted(&mut core, true)?;
    let table = core.registers();
    for reg in table.core_registers() {
        let name = reg.name();
        match core.read_core_reg::<u32>(reg.id()) {
            Ok(v) => println!("{name:>12} = 0x{v:08x}"),
            Err(_) => println!("{name:>12} = （不可读）"),
        }
    }
    Ok(())
}

fn debug_pc(session: &mut Session, elf: Option<&std::path::Path>) -> Result<()> {
    let mut core = session.core(0)?;
    ensure_halted(&mut core, true)?;
    let pc: u32 = core
        .read_core_reg(core.registers().pc().context("找不到 PC 寄存器定义")?.id())
        .context("读 PC 失败")?;
    print_pc(pc as u64, elf);

    // SP / LR：Cortex-M 的寄存器表里架构名是 R13 / R14（没有 "SP"/"LR" 别名），
    // 按名字链查找；找不到或读不了都明确提示，而不是静默跳过。
    for (label, names) in [("SP", &["R13", "SP"][..]), ("LR", &["R14", "LR", "RA"][..])] {
        let found = names
            .iter()
            .find_map(|n| core.registers().core_registers().find(|r| r.name() == *n));
        match found {
            Some(reg) => match core.read_core_reg::<u32>(reg.id()) {
                Ok(v) => println!("{label} = 0x{v:08x}"),
                Err(e) => println!("{label} = 读取失败：{e}"),
            },
            None => println!("{label} = （寄存器表里没有）"),
        }
    }
    Ok(())
}

/// 复位并暂停在指定函数（默认 main）开头。
/// 实现：复位并暂停 → 设硬件断点 → 全速运行 → 命中后清除断点。
/// 无论芯片有没有 bootloader 都通用（bootloader 会把控制权交给 APP）。
/// 复位会清空硬件断点（芯片特性），完成后自动重新应用会话内断点。
fn debug_reset(
    session: &mut Session,
    elf: &std::path::Path,
    target: &str,
    breakpoints: &[Breakpoint],
    plot: Option<&mut PlotLink>,
) -> Result<()> {
    let addr = symbol::code_symbol_address(elf, target)?;
    println!("复位并运行到 {target} 开头（0x{addr:08x}）…");

    let mut core = session.core(0)?;
    core.reset_and_halt(HALT_TIMEOUT)
        .context("复位并暂停失败（接线/供电？）")?;

    core.set_hw_breakpoint(addr)
        .with_context(|| format!("设置断点 @ 0x{addr:08x} 失败"))?;
    core.run().context("继续运行失败")?;

    match wait_halted_with_plot(&mut core, plot) {
        Ok(true) => {
            let pc: u32 = core
                .read_core_reg(core.registers().pc().context("找不到 PC 寄存器定义")?.id())
                .context("读 PC 失败")?;
            print!("已暂停在 {target} 开头：");
            print_pc(pc as u64, Some(elf));
        }
        Ok(false) => {
            println!(
                "{} 秒内未运行到 {target}（bootloader 未跳转？函数名不对？）",
                BP_WAIT_TIMEOUT.as_secs()
            );
            core.halt(HALT_TIMEOUT).context("暂停失败")?;
        }
        Err(e) => return Err(e.context("等待运行到目标函数失败")),
    }

    core.clear_hw_breakpoint(addr)
        .with_context(|| format!("清除断点 @ 0x{addr:08x} 失败"))?;

    // 复位会清空 FPB 硬件断点，重新应用会话内断点（此刻内核已暂停）
    if !breakpoints.is_empty() {
        for b in breakpoints {
            core.set_hw_breakpoint(b.addr)
                .with_context(|| format!("重新应用断点 @ 0x{:08x} 失败", b.addr))?;
        }
        println!(
            "已自动恢复 {} 个断点（芯片复位会清空硬件断点，tscope 已重新写入，无需重新 bp）",
            breakpoints.len()
        );
    }
    Ok(())
}

/// bt：打印当前函数调用栈（#0 是最内层）
fn debug_bt(session: &mut Session, elf: Option<&std::path::Path>, max_frames: usize) -> Result<()> {
    let mut core = session.core(0)?;
    ensure_halted(&mut core, true)?;
    let elf = elf.ok_or_else(|| anyhow!("bt 需要固件镜像（.debug_frame 栈展开表）"))?;

    let frames = crate::backtrace::backtrace(elf, &mut core, max_frames)?;

    println!("调用栈（共 {} 帧，#0 为最内层）：", frames.len());
    for (i, f) in frames.iter().enumerate() {
        let func = symbol::function_name_at(elf, f.pc);
        let loc = symbol::address_to_line(elf, f.pc);
        match (func, loc) {
            (Some(fn_name), Some((file, line))) => {
                println!("#{i:<3} {fn_name:<28} {file}:{line}  0x{:08x}", f.pc)
            }
            (Some(fn_name), None) => {
                println!("#{i:<3} {fn_name:<28} （无行号信息）  0x{:08x}", f.pc)
            }
            (None, _) => {
                println!("#{i:<3} <未知函数>                   0x{:08x}", f.pc)
            }
        }
    }
    Ok(())
}

/// 打印 PC，并尝试反查所在位置（文件:行号 + 函数）
fn print_pc(pc: u64, elf: Option<&std::path::Path>) {
    let loc = elf.and_then(|e| symbol::address_to_line(e, pc));
    let func = elf.and_then(|e| symbol::function_name_at(e, pc));
    match (loc, func) {
        (Some((file, line)), Some(f)) => println!("PC = 0x{pc:08x}（{file}:{line}，{f}）"),
        (Some((file, line)), None) => println!("PC = 0x{pc:08x}（{file}:{line}）"),
        (None, Some(f)) => println!("PC = 0x{pc:08x}（{f}）"),
        (None, None) => println!("PC = 0x{pc:08x}"),
    }
}

/// list（不带参数）：显示当前 PC 附近源码。需要 firmware.elf；内核在运行时会自动暂停。
fn debug_list_current(session: &mut Session, elf: Option<&std::path::Path>) -> Result<()> {
    let mut core = session.core(0)?;
    ensure_halted(&mut core, true)?;
    let pc: u32 = core
        .read_core_reg(core.registers().pc().context("找不到 PC 寄存器定义")?.id())
        .context("读 PC 失败")?;

    let elf = elf.ok_or_else(|| anyhow!("list 需要固件镜像来反查当前源码位置"))?;
    let (file, line) = symbol::address_to_line(elf, pc as u64)
        .ok_or_else(|| anyhow!("当前 PC 0x{pc:08x} 没有对应的源码行（汇编/库代码？）"))?;
    print_source_context(&file, line)
}

/// list <文件:行号>：显示指定位置的源码（无需暂停内核）。
/// 文件路径写法与 bp 相同：完整路径/后缀/基名，由 ELF 行号表解析。
fn debug_list_at(elf: Option<&std::path::Path>, loc: &str) -> Result<()> {
    let (file, line_str) = loc
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("用法：l <文件:行号>，如 l port.c:244"))?;
    let line: u64 = line_str.parse().context("行号必须是数字")?;

    let full_path = match elf {
        Some(e) => symbol::resolve_source_path(e, file)?,
        None => file.to_string(),
    };
    print_source_context(&full_path, line)
}

/// 打印文件 center 行附近 ±5 行，当前行用 "=> " 标记（gdb 风格）
fn print_source_context(path: &str, center: u64) -> Result<()> {
    if center == 0 {
        bail!("行号不能为 0");
    }
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "读取源码失败：{path}\n\
             （路径来自 ELF 编译时记录；tscope 只依赖 ELF，源码文件不在本机属正常，\n\
             想用 list 请保证该路径下存在对应源码）"
        )
    })?;
    // 源码可能是 GBK 等非 UTF-8 编码（中文注释），有损转换保证能显示
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();

    let start = center.saturating_sub(5).max(1);
    let end = (center + 5).min(lines.len() as u64);
    println!("── {path}:{center} ──");
    for n in start..=end {
        let marker = if n == center { "=> " } else { "   " };
        println!("{marker}{n:>6}\t{}", lines[n as usize - 1]);
    }
    Ok(())
}

/// 解析 bp 目标：十六进制地址 / 文件:行号 / 函数名
fn resolve_target(elf: Option<&std::path::Path>, s: &str) -> Result<u64> {
    let t = s.trim();

    // 0x 前缀：明确是十六进制
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        if hex.is_empty() {
            bail!("地址不能为空");
        }
        return u64::from_str_radix(hex, 16).with_context(|| format!("无效的十六进制地址：{t}"));
    }

    // 纯十六进制数字且足够长（≥8 位，避免把符号名误判成地址）
    if t.len() >= 8 && t.chars().all(|c| c.is_ascii_hexdigit()) {
        return u64::from_str_radix(t, 16).with_context(|| format!("无效的地址：{t}"));
    }

    // 文件:行号（用最后一个冒号分割，兼容 Windows 盘符）
    if let Some((file, line_str)) = t.rsplit_once(':') {
        if let Ok(line) = line_str.parse::<u64>() {
            if file.contains('/') || file.contains('\\') || file.contains('.') {
                let elf =
                    elf.ok_or_else(|| anyhow!("目标是源文件位置 {t}，但配置里没有固件镜像"))?;
                return symbol::line_to_address(elf, file, line);
            }
        }
    }

    let elf = elf.ok_or_else(|| anyhow!("目标是符号 {t}，但配置里没有 firmware.elf"))?;
    symbol::code_symbol_address(elf, t)
}
