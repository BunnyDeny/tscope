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
//! - 所有操作只动 CPU 调试逻辑，不碰 flash（bootloader 安全）。

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use probe_rs::{Core, Session};

use crate::backtrace;
use crate::config::ToolConfig;
use crate::session;
use crate::symbol;
use crate::watch;

/// 暂停内核时常用的超时
const HALT_TIMEOUT: Duration = Duration::from_millis(1000);

/// 等待断点命中的超时
const BP_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// 进入交互式调试会话
pub fn run(config: &ToolConfig) -> Result<()> {
    let elf = config.firmware.elf.clone();
    let mut session = session::open_session(&config.probe, &config.chip)?;

    println!("tscope 调试会话已建立。输入 help 查看命令，q 退出。");
    println!("提示：halt/step/regs/pc 会自动暂停运行中的内核（暂停后保持，run 恢复运行）；bp 命中后内核保持暂停。");
    println!("行编辑：左右光标移动，↑/↓ 翻阅历史命令（跨会话保存）。");

    // readline 风格行编辑（rustyline）：光标移动、历史记录、Ctrl-A/E 等
    let mut rl = rustyline::DefaultEditor::new().context("初始化行编辑器失败")?;
    if let Some(path) = history_path() {
        // 首次运行没有历史文件属正常，忽略加载失败
        let _ = rl.load_history(&path);
    }

    // 会话内的断点集合：跨 run/halt/reset 保持，退出会话即消失
    let mut breakpoints: Vec<Breakpoint> = Vec::new();

    loop {
        let line = match rl.readline("> ") {
            Ok(line) => line,
            // 某些终端环境（rustyline 原始模式生效时）Ctrl-C 会走到这里
            Err(rustyline::error::ReadlineError::Interrupted) => {
                println!("（Ctrl-C：输入 q 退出会话）");
                continue;
            }
            Err(rustyline::error::ReadlineError::Eof) => {
                println!();
                break;
            }
            Err(e) => return Err(anyhow!("读取输入失败：{e}")),
        };

        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(line);

        let mut parts = line.split_whitespace();
        let cmd = parts.next().unwrap();
        let arg = parts.next();

        let result: Result<()> = match cmd {
            "help" | "h" | "?" => {
                print_help();
                Ok(())
            }
            "q" | "quit" | "exit" => break,
            "halt" => debug_halt(&mut session),
            "run" | "continue" | "c" => debug_run(&mut session, elf.as_deref(), &breakpoints),
            "step" | "s" | "next" | "n" => debug_step(&mut session, elf.as_deref()),
            "stepi" | "si" => debug_stepi(&mut session, elf.as_deref()),
            "finish" | "fin" | "f" => debug_finish(&mut session, elf.as_deref()),
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
                    let elf = elf
                        .as_deref()
                        .ok_or_else(|| anyhow!("reset 需要 firmware.elf 来解析函数地址（复位后暂停在函数开头）"))?;
                    debug_reset(&mut session, elf, target, &breakpoints)
                })()
            }
            "bp" => match arg {
                Some(target) => debug_bp_add(&mut session, elf.as_deref(), target, &mut breakpoints),
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
                    let elf = elf
                        .as_deref()
                        .ok_or_else(|| anyhow!("配置里没有 firmware.elf，无法解析符号"))?;
                    let mut core = session.core(0)?;
                    let opts = symbol::FmtOptions {
                        max_elems: Some(16),
                        max_depth: 3,
                    };
                    symbol::print_global_value(elf, expr, &mut core, &opts)
                })()
            }
            "watch" | "w" => match arg {
                Some(group) => watch::run_with_session(config, group, &mut session),
                None => watch::list_groups(config),
            },
            other => {
                println!("未知命令 {other}（help 查看命令列表）");
                Ok(())
            }
        };

        if let Err(e) = result {
            println!("错误：{e:#}");
        }
    }

    // 退出前保存历史（best-effort，失败不影响退出）
    if let Some(path) = history_path() {
        let _ = rl.save_history(&path);
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
        core.halt(HALT_TIMEOUT).context("暂停失败（超时？接线/供电？）")?;
        if verbose {
            // 提示放在暂停成功之后：描述「已发生的动作」，而不是指引用户去暂停
            println!("（内核在运行，已自动暂停）");
        }
    }
    Ok(())
}

fn debug_halt(session: &mut Session) -> Result<()> {
    let mut core = session.core(0)?;
    let info = core.halt(HALT_TIMEOUT).context("暂停失败（超时？接线/供电？）")?;
    println!("已暂停，PC = 0x{:08x}", info.pc);
    Ok(())
}

fn debug_run(
    session: &mut Session,
    elf: Option<&std::path::Path>,
    breakpoints: &[Breakpoint],
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

    println!("已继续运行，等待断点命中（{} 秒超时）…", BP_WAIT_TIMEOUT.as_secs());
    match core.wait_for_core_halted(BP_WAIT_TIMEOUT) {
        Ok(()) => {
            let pc: u32 = core
                .read_core_reg(
                    core.registers()
                        .pc()
                        .context("找不到 PC 寄存器定义")?
                        .id(),
                )
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
        Err(_) => {
            println!(
                "{} 秒内未命中断点（内核仍在运行；halt 可暂停查看）",
                BP_WAIT_TIMEOUT.as_secs()
            );
        }
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
        println!("0x{addr:08x} 已有断点（{}）", breakpoints.iter().find(|b| b.addr == addr).unwrap().desc);
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
        .ok_or_else(|| anyhow!("无效的断点编号 {which}（当前共 {} 个，用 bl 查看）", breakpoints.len()))?;

    let b = &breakpoints[idx - 1];
    let addr = b.addr;
    let desc = b.desc.clone();
    core.clear_hw_breakpoint(addr)
        .with_context(|| format!("清除断点 @ 0x{addr:08x} 失败"))?;
    breakpoints.remove(idx - 1);
    println!("已删除断点 {idx}（{desc}）");
    Ok(())
}

fn read_pc(core: &mut probe_rs::Core) -> Result<u64> {
    let pc: u32 = core
        .read_core_reg(
            core.registers()
                .pc()
                .context("找不到 PC 寄存器定义")?
                .id(),
        )
        .context("读 PC 失败")?;
    Ok(pc as u64)
}

/// 设断点全速运行到 addr（内核需已暂停）。命中返回 true；超时则停下返回 false。
fn run_to_address(core: &mut probe_rs::Core, addr: u64) -> Result<bool> {
    core.set_hw_breakpoint(addr)
        .with_context(|| format!("设置临时断点 @ 0x{addr:08x} 失败"))?;
    core.run().context("继续运行失败")?;
    let hit = core.wait_for_core_halted(BP_WAIT_TIMEOUT).is_ok();
    if !hit {
        core.halt(HALT_TIMEOUT).context("暂停失败")?;
    }
    core.clear_hw_breakpoint(addr)
        .with_context(|| format!("清除临时断点 @ 0x{addr:08x} 失败"))?;
    Ok(hit)
}

/// 走出当前函数：普通函数断点停到返回地址；中断函数单步越过异常返回
/// （走过 pop+bx lr 后自然落在被打断的代码，不读异常帧、不依赖 FPU）
fn run_out_of_function(session: &mut Session, elf: &std::path::Path) -> Result<()> {
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
            if run_to_address(&mut core, addr)? {
                print_pc(read_pc(&mut core)?, Some(elf));
            } else {
                println!("（未运行到返回地址，已暂停在当前处）");
                print_pc(read_pc(&mut core)?, Some(elf));
            }
        }
        None => {
            // 无展开信息：退化为指令单步
            let info = core.step().context("单步失败")?;
            print_pc(info.pc as u64, Some(elf));
        }
    }
    Ok(())
}

fn debug_step(session: &mut Session, elf: Option<&std::path::Path>) -> Result<()> {
    let mut core = session.core(0)?;
    ensure_halted(&mut core, true)?;
    let start_pc = read_pc(&mut core)?;

    // 无行号信息（汇编/库代码）：直接指令单步
    let Some((start_file, start_line)) =
        elf.and_then(|e| symbol::address_to_line(e, start_pc))
    else {
        let info = core.step().context("单步失败")?;
        print_pc(info.pc as u64, elf);
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
fn debug_finish(session: &mut Session, elf: Option<&std::path::Path>) -> Result<()> {
    let e = elf.ok_or_else(|| anyhow!("finish 需要 firmware.elf（展开信息）"))?;
    run_out_of_function(session, e)
}

/// 指令级单步：一次执行一条机器指令（不管源码行）
fn debug_stepi(session: &mut Session, elf: Option<&std::path::Path>) -> Result<()> {
    let mut core = session.core(0)?;
    ensure_halted(&mut core, true)?;
    let info = core.step().context("单步失败")?;
    print_pc(info.pc as u64, elf);
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
        .read_core_reg(
            core.registers()
                .pc()
                .context("找不到 PC 寄存器定义")?
                .id(),
        )
        .context("读 PC 失败")?;
    print_pc(pc as u64, elf);

    // SP / LR：Cortex-M 的寄存器表里架构名是 R13 / R14（没有 "SP"/"LR" 别名），
    // 按名字链查找；找不到或读不了都明确提示，而不是静默跳过。
    for (label, names) in [("SP", &["R13", "SP"][..]), ("LR", &["R14", "LR", "RA"][..])] {
        let found = names.iter().find_map(|n| {
            core.registers().core_registers().find(|r| r.name() == *n)
        });
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
) -> Result<()> {
    let addr = symbol::code_symbol_address(elf, target)?;
    println!("复位并运行到 {target} 开头（0x{addr:08x}）…");

    let mut core = session.core(0)?;
    core.reset_and_halt(HALT_TIMEOUT)
        .context("复位并暂停失败（接线/供电？）")?;

    core.set_hw_breakpoint(addr)
        .with_context(|| format!("设置断点 @ 0x{addr:08x} 失败"))?;
    core.run().context("继续运行失败")?;

    match core.wait_for_core_halted(BP_WAIT_TIMEOUT) {
        Ok(()) => {
            let pc: u32 = core
                .read_core_reg(
                    core.registers()
                        .pc()
                        .context("找不到 PC 寄存器定义")?
                        .id(),
                )
                .context("读 PC 失败")?;
            print!("已暂停在 {target} 开头：");
            print_pc(pc as u64, Some(elf));
        }
        Err(_) => {
            println!(
                "{} 秒内未运行到 {target}（bootloader 未跳转？函数名不对？）",
                BP_WAIT_TIMEOUT.as_secs()
            );
            core.halt(HALT_TIMEOUT).context("暂停失败")?;
        }
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
fn debug_bt(
    session: &mut Session,
    elf: Option<&std::path::Path>,
    max_frames: usize,
) -> Result<()> {
    let mut core = session.core(0)?;
    ensure_halted(&mut core, true)?;
    let elf = elf.ok_or_else(|| anyhow!("bt 需要 firmware.elf（.debug_frame 栈展开表）"))?;

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
        .read_core_reg(
            core.registers()
                .pc()
                .context("找不到 PC 寄存器定义")?
                .id(),
        )
        .context("读 PC 失败")?;

    let elf = elf.ok_or_else(|| anyhow!("list 需要 firmware.elf 来反查当前源码位置"))?;
    let (file, line) = symbol::address_to_line(elf, pc as u64).ok_or_else(|| {
        anyhow!("当前 PC 0x{pc:08x} 没有对应的源码行（汇编/库代码？）")
    })?;
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
        return u64::from_str_radix(hex, 16)
            .with_context(|| format!("无效的十六进制地址：{t}"));
    }

    // 纯十六进制数字且足够长（≥8 位，避免把符号名误判成地址）
    if t.len() >= 8 && t.chars().all(|c| c.is_ascii_hexdigit()) {
        return u64::from_str_radix(t, 16).with_context(|| format!("无效的地址：{t}"));
    }

    // 文件:行号（用最后一个冒号分割，兼容 Windows 盘符）
    if let Some((file, line_str)) = t.rsplit_once(':') {
        if let Ok(line) = line_str.parse::<u64>() {
            if file.contains('/') || file.contains('\\') || file.contains('.') {
                let elf = elf
                    .ok_or_else(|| anyhow!("目标是源文件位置 {t}，但配置里没有 firmware.elf"))?;
                return symbol::line_to_address(elf, file, line);
            }
        }
    }

    let elf = elf.ok_or_else(|| anyhow!("目标是符号 {t}，但配置里没有 firmware.elf"))?;
    symbol::code_symbol_address(elf, t)
}
