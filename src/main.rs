//! tscope —— 基于 probe-rs 库的嵌入式调试 / 监视工具
//!
//! v1 功能：读取目标内存（默认 0x20000000）。
//! v2 功能：按符号名解析 ELF 调试信息，读取全局变量值（`var` 子命令）。
//! 架构按项目约定：
//! - YAML 配置文件作为每次执行的"环境变量"（探针 / 芯片 / 固件信息）；
//! - 芯片描述 YAML（target-gen 产物）与工具配置严格分开，配置只引用其路径；
//! - 芯片是否官方支持由运行时查询内置 Registry 判定，四级 fallback 见 session.rs；
//! - 库直接内嵌：本程序就是唯一进程，直接经 SWD 访问芯片，无中间服务。

mod config;
mod session;
mod symbol;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use probe_rs::MemoryInterface;

use crate::config::ToolConfig;

/// tscope：YAML 配置驱动的嵌入式调试/监视工具
#[derive(Parser)]
#[command(name = "tscope", version, about)]
struct Cli {
    /// 工具配置文件（默认 ./tscope.yaml）
    #[arg(short, long, default_value = "tscope.yaml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 列出所有调试探针（不需要配置文件）
    List,

    /// 读取目标内存（32 位字）
    Read {
        /// 起始地址，十六进制，默认 0x20000000（本芯片 RAM 起点）
        #[arg(long, default_value = "0x20000000")]
        address: String,

        /// 读取的 32 位字数
        #[arg(long, default_value_t = 1)]
        count: usize,
    },

    /// 按符号名读取全局变量值（从 ELF 调试信息解析地址与类型）
    Var {
        /// 符号名或成员路径：theta_ref / ENC_1_POS_SENSOR.readAngleCmd / items[0].v.x
        symbol: String,

        /// 数组最多显示的元素个数（默认 16）
        #[arg(long, default_value_t = 16)]
        count: usize,

        /// 显示数组全部元素（覆盖 --count）
        #[arg(long)]
        all: bool,
    },
}

fn parse_hex(s: &str) -> Result<u64> {
    let t = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if t.is_empty() {
        bail!("无效的十六进制数：{s}");
    }
    u64::from_str_radix(t, 16).map_err(|e| anyhow::anyhow!("解析十六进制失败 {s}：{e}"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Cmd::List => {
            session::list_probes();
            Ok(())
        }
        Cmd::Read { address, count } => {
            let config = load_config(&cli.config)?;
            let mut session = session::open_session(&config.probe, &config.chip)?;
            read_words(&mut session, parse_hex(&address)?, count)
        }
        Cmd::Var {
            symbol,
            count,
            all,
        } => {
            let config = load_config(&cli.config)?;
            let elf = config
                .firmware
                .elf
                .ok_or_else(|| anyhow::anyhow!("配置里没有 firmware.elf 路径，无法解析符号"))?;
            let mut session = session::open_session(&config.probe, &config.chip)?;
            let mut core = session.core(0)?;
            let opts = symbol::FmtOptions {
                max_elems: if all { None } else { Some(count) },
                max_depth: 3,
            };
            symbol::print_global_value(&elf, &symbol, &mut core, &opts)
        }
    }
}

/// 加载配置，并把配置里的相对路径按「配置文件所在目录」解析
fn load_config(path: &Path) -> Result<ToolConfig> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("获取当前目录失败")?
            .join(path)
    };
    let mut config = ToolConfig::load(&abs)?;
    config.absolutize(abs.parent().unwrap_or_else(|| Path::new(".")));
    Ok(config)
}

/// 读 count 个 32 位字并打印
fn read_words(session: &mut probe_rs::Session, address: u64, count: usize) -> Result<()> {
    if address % 4 != 0 {
        bail!("地址 0x{address:08x} 不是 4 字节对齐（32 位读要求）");
    }

    let mut core = session.core(0)?;
    let mut buf = vec![0u32; count];
    core.read_32(address, &mut buf)
        .with_context(|| format!("读取 0x{address:08x} 失败（芯片未上电？地址无效？）"))?;

    for (i, v) in buf.iter().enumerate() {
        println!("0x{:08x}: 0x{v:08x}", address + i as u64 * 4);
    }
    Ok(())
}
