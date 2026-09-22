//! flash 子命令：把配置里的 firmware.elf 烧录到芯片。
//!
//! 安全设计（与项目约定一致）：
//! - 默认**扇区擦除**：只擦 ELF 数据覆盖到的扇区，bootloader
//!   （0x08000000 起）不受影响——APP 在 0x08004000，恰好从第 9 页开始；
//! - `--erase_all` **整片擦除**：会永久擦除 bootloader（本仓库没有备份），
//!   执行前必须交互确认，`--yes` 跳过确认（供自动化脚本，危险）；
//! - 烧录完成后**复位并暂停**在复位向量（零指令执行）：probe-rs 烧完会
//!   「恢复现场并继续运行」，而恢复的 PC 指向已被新固件覆盖的旧代码，
//!   等于在跑乱码（DHCSR 却显示在运行，很误导）；复位+暂停后 PC 回到
//!   复位向量，tscope 退出、探针断开时内核从复位向量继续运行——新固件
//!   自动从头启动（bootloader 跳转 APP），无需手动 rst。
//!
//! 进度显示：终端里用 indicatif 多阶段进度条（擦除/写入/校验各一条，
//! 按字节计），重定向/管道输出时自动退回分行打印，不刷屏。

use std::cell::RefCell;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use probe_rs::flashing::{self, FlashProgress, ProgressEvent, ProgressOperation};

use crate::config::ToolConfig;
use crate::session;

/// 烧录 firmware.elf。
pub fn run(config: &ToolConfig, erase_all: bool, assume_yes: bool) -> Result<()> {
    let elf = config
        .firmware_image()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !elf.exists() {
        anyhow::bail!("固件文件不存在：{}", elf.display());
    }

    // 整片擦除确认（bootloader 红线）
    if erase_all && !assume_yes {
        println!("⚠️  整片擦除将永久删除芯片全部 flash 内容，包括 bootloader！");
        println!("   本仓库没有 bootloader 备份，擦除后无法恢复。");
        print!("   确认执行请输入 yes：");
        std::io::stdout().flush().context("刷新输出失败")?;
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("读取输入失败")?;
        if line.trim() != "yes" {
            println!("已取消，未执行擦除。");
            return Ok(());
        }
    }

    let mut session = session::open_session(&config.probe, &config.chip)?;

    // 进度条显示条件：stdout 是终端，且 TERM 不是 dumb/空
    // （indicatif 在 TERM=dumb 时静默隐藏进度条；此时退回分行打印，
    //  保证任何环境下烧录过程都有可见输出）。
    let term_ok = std::env::var("TERM")
        .map(|t| !t.is_empty() && t != "dumb")
        .unwrap_or(false);
    let progress = if std::io::stdout().is_terminal() && term_ok {
        progress_with_bars()
    } else {
        FlashProgress::new(|event| match event {
            ProgressEvent::Started(op) => println!("{}…", operation_name(op)),
            ProgressEvent::Finished(op) => println!("{} 完成", operation_name(op)),
            ProgressEvent::Failed(op) => println!("{} 失败", operation_name(op)),
            ProgressEvent::DiagnosticMessage { message } => {
                println!("烧写算法消息：{message}")
            }
            _ => {}
        })
    };

    let mut options = flashing::DownloadOptions::new();
    options.progress = progress;
    options.do_chip_erase = erase_all;
    options.preverify = true;
    options.verify = true;

    println!(
        "开始烧录 {}（{}）…",
        elf.display(),
        if erase_all {
            "整片擦除"
        } else {
            "扇区擦除"
        }
    );
    let start = Instant::now();
    flashing::download_file_with_options(
        &mut session,
        elf,
        flashing::ElfLoader(flashing::ElfOptions::default()),
        options,
    )
    .context("烧录失败")?;
    let elapsed = start.elapsed().as_secs_f32();

    // 烧完把内核停在干净状态：复位向量处、未执行任何指令（见模块注释）。
    // tscope 退出、探针断开后，J-Link 会释放内核，从复位向量继续运行——
    // 即新固件自动从头启动（bootloader 跳转 APP），不再需要手动 rst。
    let mut core = session.core(0).context("获取内核失败")?;
    core.reset_and_halt(Duration::from_millis(500))
        .context("复位并暂停内核失败")?;

    println!("烧录完成，耗时 {elapsed:.1} 秒；芯片已复位并停在复位向量（未执行任何指令）");
    println!("提示：调试器断开后内核从复位向量继续运行，新固件自动启动（未启动则重新上电或 debug 里 rst）");
    Ok(())
}

// ===========================================================================
// 进度条（仅 TTY）
// ===========================================================================

fn operation_name(op: ProgressOperation) -> &'static str {
    match op {
        ProgressOperation::Fill => "恢复保留区域",
        ProgressOperation::Erase => "擦除扇区",
        ProgressOperation::Program => "写入数据",
        ProgressOperation::Verify => "校验",
    }
}

fn op_index(op: ProgressOperation) -> usize {
    match op {
        ProgressOperation::Fill => 0,
        ProgressOperation::Erase => 1,
        ProgressOperation::Program => 2,
        ProgressOperation::Verify => 3,
    }
}

fn sized_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} {msg:>12} [{bar:30.cyan/blue}] \
         {bytes:>9}/{total_bytes:<9} {percent:>3}% {eta}",
    )
    .unwrap()
    .progress_chars("█▉▊▋▌▍▎▏ ")
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.green} {msg:>12} …").unwrap()
}

/// 多阶段进度条：AddProgressBar 建条（带总字节数则按字节计），
/// Progress 累计，Finished 收尾清屏，Failed 标失败。
fn progress_with_bars() -> FlashProgress<'static> {
    let multi = MultiProgress::new();
    let bars: RefCell<[Option<ProgressBar>; 4]> = RefCell::new(Default::default());

    FlashProgress::new(move |event| match event {
        ProgressEvent::AddProgressBar { operation, total } => {
            let bar = match total {
                Some(t) => {
                    let pb = multi.add(ProgressBar::new(t));
                    pb.set_style(sized_style());
                    pb
                }
                None => {
                    let pb = multi.add(ProgressBar::new_spinner());
                    pb.set_style(spinner_style());
                    pb
                }
            };
            bar.set_message(operation_name(operation).to_string());
            bars.borrow_mut()[op_index(operation)] = Some(bar);
        }
        ProgressEvent::Started(op) => {
            // 个别路径（如整片擦除）不发 AddProgressBar，兜底建不确定长度的
            let mut slots = bars.borrow_mut();
            let i = op_index(op);
            if slots[i].is_none() {
                let pb = multi.add(ProgressBar::new_spinner());
                pb.set_style(spinner_style());
                pb.set_message(operation_name(op).to_string());
                slots[i] = Some(pb);
            }
        }
        ProgressEvent::Progress {
            operation, size, ..
        } => {
            if let Some(bar) = &bars.borrow()[op_index(operation)] {
                bar.inc(size);
            }
        }
        ProgressEvent::Finished(op) => {
            let i = op_index(op);
            if let Some(bar) = bars.borrow_mut()[i].take() {
                bar.finish_and_clear();
            }
        }
        ProgressEvent::Failed(op) => {
            let i = op_index(op);
            if let Some(bar) = bars.borrow_mut()[i].take() {
                bar.finish_with_message(format!("{} 失败", operation_name(op)));
            }
        }
        ProgressEvent::DiagnosticMessage { message } => {
            let _ = multi.println(format!("烧写算法消息：{message}"));
        }
        ProgressEvent::FlashLayoutReady { .. } => {}
    })
}
