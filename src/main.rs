//! tscope —— 基于 probe-rs 库的嵌入式调试 / 监视工具
//!
//! v1 功能：hexdump 转储目标内存（地址列 + 十六进制列 + ASCII 列）。
//! v2 功能：按符号名解析 ELF 调试信息，读取全局变量值（`var` 子命令）。
//! 架构按项目约定：
//! - YAML 配置文件作为每次执行的"环境变量"（探针 / 芯片 / 固件信息）；
//! - 芯片描述 YAML（target-gen 产物）与工具配置严格分开，配置只引用其路径；
//! - 芯片是否官方支持由运行时查询内置 Registry 判定，四级 fallback 见 session.rs；
//! - 库直接内嵌：本程序就是唯一进程，直接经 SWD 访问芯片，无中间服务。

mod backtrace;
mod config;
mod debug;
mod flash;
mod hexdump;
mod plot;
mod session;
mod symbol;
mod watch;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

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

    /// 按字节转储目标内存：左列地址、中间十六进制值、右列 ASCII（read 为隐藏别名）
    #[command(name = "hexdump", alias = "read")]
    Hexdump {
        /// 起始地址，十六进制，默认 0x20000000（本芯片 RAM 起点）
        #[arg(long, default_value = "0x20000000")]
        address: String,

        /// 总字节数（十进制或 0x 十六进制，默认 256；count 为旧写法别名）
        #[arg(long, default_value = "256", visible_alias = "count")]
        length: String,

        /// 每行字节数：4 / 8 / 16 / 32（默认 16）
        #[arg(long, default_value_t = 16)]
        width: usize,

        /// 每组字节数：1 / 2 / 4 / 8（默认 4；1 即 hexdump -C 同款）
        #[arg(long, default_value_t = 4)]
        group: usize,

        /// 隐藏最右 ASCII 列
        #[arg(long)]
        no_ascii: bool,

        /// 连续相同行折叠成 *（大段 0xFF 擦除区不刷屏）
        #[arg(long)]
        collapse: bool,

        /// 持续刷新显示（watch 风格交替屏，变化字节黄色高亮；q/Esc 退出）
        #[arg(long)]
        watch: bool,

        /// 采样周期（毫秒，仅 --watch 生效，默认 100）
        #[arg(long, default_value_t = 100)]
        interval: u64,
    },

    /// 按符号名读取全局变量值（从 ELF 调试信息解析地址与类型）
    Var {
        /// 符号名或成员路径：theta_ref / ENC_1_POS_SENSOR.readAngleCmd / items[0].v.x
        symbol: String,

        /// 数组最多显示的元素个数（默认 16；--watch 时即表格展开行数上限，等同 watch 组的 max_elems）
        #[arg(long, default_value_t = 16)]
        count: usize,

        /// 显示数组全部元素（覆盖 --count；--watch 时展开全部行）
        #[arg(long)]
        all: bool,

        /// 持续刷新显示（watch 风格表格；q/Esc 退出；展开上限由 --count / --all 控制）
        #[arg(long)]
        watch: bool,

        /// 采样周期（毫秒，仅 --watch 生效，默认 100）
        #[arg(long, default_value_t = 100)]
        interval: u64,
    },

    /// 实时刷新监视组（Keil Watch 风格；组定义在 tscope.yaml 的 watch 节）
    Watch {
        /// 监视组名（对应 tscope.yaml 里 watch 节的键）；省略则列出所有组
        group: Option<String>,
    },

    /// 独立 GUI 窗口显示变量实时曲线（配置在 tscope.yaml 的 plot 节；
    /// 滚轮缩放/拖拽平移/右键框选，双击/r 恢复滚动，空格暂停，+/− 窗口，s 导出 CSV）
    Plot {
        /// 曲线配置名（对应 tscope.yaml 里 plot 节的键）；省略则列出所有配置
        name: Option<String>,
    },

    /// 烧录 firmware.elf 到芯片（默认扇区擦除 + 校验，不复位）
    Flash {
        /// 整片擦除（⚠️ 会永久擦除 bootloader，需交互确认）
        #[arg(long = "erase_all")]
        erase_all: bool,

        /// 跳过 --erase_all 的交互确认（危险，供自动化脚本使用）
        #[arg(long)]
        yes: bool,
    },

    /// 进入交互式调试会话（提示符 "> "，输入 help 查看命令）
    Debug,
}

fn parse_hex(s: &str) -> Result<u64> {
    let t = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if t.is_empty() {
        bail!("无效的十六进制数：{s}");
    }
    u64::from_str_radix(t, 16).map_err(|e| anyhow::anyhow!("解析十六进制失败 {s}：{e}"))
}

/// 解析长度：纯十进制，或以 0x/0X 开头的十六进制
fn parse_len(s: &str) -> Result<u64> {
    let t = s.trim();
    let n = if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        if hex.is_empty() {
            bail!("无效的长度：{s}");
        }
        u64::from_str_radix(hex, 16)
    } else {
        t.parse::<u64>()
    };
    n.map_err(|e| anyhow::anyhow!("解析长度失败 {s}：{e}"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Cmd::List => {
            session::list_probes();
            Ok(())
        }
        Cmd::Hexdump {
            address,
            length,
            width,
            group,
            no_ascii,
            collapse,
            watch,
            interval,
        } => {
            let config = load_config(&cli.config)?;
            let mut session = session::open_session(&config.probe, &config.chip)?;
            let opts = hexdump::DumpOptions {
                address: parse_hex(&address)?,
                length: parse_len(&length)?,
                width,
                group,
                show_ascii: !no_ascii,
                collapse,
            };
            if watch {
                hexdump::run_watch(&mut session, &opts, interval)
            } else {
                hexdump::run(&mut session, &opts)
            }
        }
        Cmd::Var {
            symbol,
            count,
            all,
            watch,
            interval,
        } => {
            let config = load_config(&cli.config)?;
            let elf = config
                .firmware
                .elf
                .ok_or_else(|| anyhow::anyhow!("配置里没有 firmware.elf 路径，无法解析符号"))?;
            let mut session = session::open_session(&config.probe, &config.chip)?;
            if watch {
                // 临时监视组：不必编辑 tscope.yaml，max_elems 复用 --count/--all
                watch::run_adhoc(
                    &mut session,
                    &elf,
                    &format!("var --watch [{symbol}]"),
                    vec![symbol.clone()],
                    interval,
                    if all { usize::MAX } else { count },
                )
            } else {
                let mut core = session.core(0)?;
                let opts = symbol::FmtOptions {
                    max_elems: if all { None } else { Some(count) },
                    max_depth: 3,
                };
                symbol::print_global_value(&elf, &symbol, &mut core, &opts)
            }
        }
        Cmd::Watch { group } => {
            let config = load_config(&cli.config)?;
            match group {
                Some(g) => watch::run(&config, &g),
                None => watch::list_groups(&config),
            }
        }
        Cmd::Plot { name } => {
            let config = load_config(&cli.config)?;
            match name {
                Some(n) => plot::run(&config, &n),
                None => plot::list_plots(&config),
            }
        }
        Cmd::Flash { erase_all, yes } => {
            let config = load_config(&cli.config)?;
            flash::run(&config, erase_all, yes)
        }
        Cmd::Debug => {
            let config = load_config(&cli.config)?;
            debug::run(&config)
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
