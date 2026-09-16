//! 工具配置文件（tscope.yaml）的加载与校验。
//!
//! 设计约定（与项目讨论一致）：
//! - 这是「人写的工具配置」，与 target-gen 生成的「芯片描述 YAML」严格分开，
//!   配置只通过 `chip.description` **引用**后者，绝不合并；
//! - 芯片是否被 probe-rs 官方内置支持，**不写进配置** —— 由运行时查询内置
//!   Registry 判定（见 session.rs），配置只描述用户意图（型号名）；
//! - 配置里的相对路径一律相对「配置文件所在目录」解析，与运行目录无关；
//! - 危险操作默认关闭（本工具 v1 只读内存，天然安全）。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// 当前支持的配置格式版本
const CURRENT_VERSION: u32 = 1;

/// 顶层配置
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolConfig {
    pub version: u32,
    #[serde(default)]
    pub probe: ProbeConfig,
    pub chip: ChipConfig,
    /// 固件相关配置；v1 尚未使用 ELF，字段先行保留
    #[serde(default)]
    pub firmware: FirmwareConfig,
}

/// 调试协议（serde 会把 yaml 里的 "swd"/"jtag" 转成枚举，写错直接报错）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Swd,
    Jtag,
}

/// 调试器相关配置
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeConfig {
    #[serde(default)]
    pub protocol: Protocol,
    /// SWD 时钟，单位 kHz。本板实测 >1000 不稳定，默认即红线值
    #[serde(default = "default_speed_khz")]
    pub speed_khz: u32,
    /// 多个探针同时存在时用来选定目标；单探针时留空
    #[serde(default)]
    pub selector: Option<ProbeSelector>,
}

// 注意：不能 derive(Default) —— 那样省略整个 probe: 节时 speed_khz 会变成 0；
// 省略任何配置都应落到安全默认值 1000 kHz。
impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            protocol: Protocol::Swd,
            speed_khz: default_speed_khz(),
            selector: None,
        }
    }
}

fn default_speed_khz() -> u32 {
    1000
}

/// 按 VID / PID / 序列号选择探针（三项均为可选，匹配同时满足）
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeSelector {
    #[serde(default)]
    pub vid: Option<u16>,
    #[serde(default)]
    pub pid: Option<u16>,
    #[serde(default)]
    pub serial: Option<String>,
}

/// 芯片相关配置
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChipConfig {
    /// 芯片型号，必须与芯片描述文件里的变体名精确一致
    pub name: String,
    /// 芯片描述 YAML（target-gen 产物）。芯片在 probe-rs 内置列表时可省略
    #[serde(default)]
    pub description: Option<PathBuf>,
    /// CMSIS-Pack（.pack 文件或 Keil 已解压目录）。
    /// 缺描述文件时程序用它拼出 target-gen 命令指引；自动生成待 v2
    #[serde(default)]
    pub pack: Option<PathBuf>,
}

/// 固件相关配置
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareConfig {
    /// 固件 ELF。v1 未使用；将来用于符号→地址解析与烧录
    #[serde(default)]
    pub elf: Option<PathBuf>,
}

impl ToolConfig {
    /// 从文件加载并校验
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置文件失败：{}", path.display()))?;

        let config: ToolConfig = yaml_serde::from_str(&text)
            .with_context(|| format!("解析配置文件失败：{}", path.display()))?;

        // 语义校验（语法正确但不合理的内容在这里拦下）
        if config.version != CURRENT_VERSION {
            bail!(
                "配置文件版本 {} 不受支持（当前支持版本 {}），请检查 version 字段",
                config.version,
                CURRENT_VERSION
            );
        }
        if config.chip.name.trim().is_empty() {
            bail!("chip.name 不能为空");
        }
        if config.probe.speed_khz == 0 {
            bail!("probe.speed_khz 不能为 0");
        }
        Ok(config)
    }

    /// 把配置里的相对路径按「配置文件所在目录」解析为绝对路径
    pub fn absolutize(&mut self, config_dir: &Path) {
        if let Some(p) = &mut self.chip.description {
            absolutize_one(p, config_dir);
        }
        if let Some(p) = &mut self.chip.pack {
            absolutize_one(p, config_dir);
        }
        if let Some(p) = &mut self.firmware.elf {
            absolutize_one(p, config_dir);
        }
    }
}

fn absolutize_one(p: &mut PathBuf, config_dir: &Path) {
    if p.is_relative() {
        *p = config_dir.join(&*p);
    }
}
