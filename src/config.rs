//! 工具配置文件（tscope.yaml）的加载与校验。
//!
//! 设计约定（与项目讨论一致）：
//! - 这是「人写的工具配置」，与 target-gen 生成的「芯片描述 YAML」严格分开，
//!   配置只通过 `chip.description` **引用**后者，绝不合并；
//! - 芯片是否被 probe-rs 官方内置支持，**不写进配置** —— 由运行时查询内置
//!   Registry 判定（见 session.rs），配置只描述用户意图（型号名）；
//! - 配置里的相对路径一律相对「配置文件所在目录」解析，与运行目录无关；
//! - 危险操作默认关闭（本工具 v1 只读内存，天然安全）。

use std::collections::BTreeMap;
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
    /// 固件相关配置；ELF 供 var / watch 子命令解析符号用
    #[serde(default)]
    pub firmware: FirmwareConfig,
    /// 实时监视组：键是组名（`tscope watch <组名>` 的参数），可定义多组
    #[serde(default)]
    pub watch: BTreeMap<String, WatchGroup>,
    /// 曲线显示配置：键是配置名（`tscope plot <配置名>` 的参数），可定义多个
    #[serde(default)]
    pub plot: BTreeMap<String, PlotConfig>,
}

/// 一个监视组：一组符号 + 采样周期
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchGroup {
    /// 采样周期（毫秒）
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
    /// 数组类型的符号在单元格里最多显示的元素个数
    #[serde(default = "default_watch_max_elems")]
    pub max_elems: usize,
    /// 要监视的符号表达式列表（语法与 var 子命令相同，支持成员路径）
    pub symbols: Vec<String>,
}

/// 一个曲线配置：采样参数 + 图组划分（GUI 窗口，见 crates/tscope-plot）
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlotConfig {
    /// 采样周期（毫秒）。曲线用 20 ms 默认（≈50 Hz，滚动观感接近平滑；
    /// 比 watch 的 100 ms 快，因为 plot 只读字段字节、SWD 流量小）
    #[serde(default = "default_plot_interval_ms")]
    pub interval_ms: u64,
    /// 滚动窗口宽度（秒）：窗口内实时显示最近这么长时间的数据
    #[serde(default = "default_plot_window_secs")]
    pub window_secs: f64,
    /// 图组：每个元素 = 一个子图；组内符号**同图共 Y 轴**（图例列出组内符号），
    /// 多个组上下叠放、**共享 X 轴联动**。同一符号可出现在多个图（只采样一次）
    pub groups: Vec<Vec<String>>,
}

fn default_plot_window_secs() -> f64 {
    5.0
}

fn default_plot_interval_ms() -> u64 {
    20
}

fn default_interval_ms() -> u64 {
    100
}

fn default_watch_max_elems() -> usize {
    8
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
    /// 固件镜像（GCC 产物 ELF；Linux 常用）：var / watch / plot 从这里解析符号
    #[serde(default)]
    pub elf: Option<PathBuf>,
    /// Keil 产物 .axf（armclang/armcc 输出，本质也是 ELF + DWARF；
    /// Windows/Keil 用户填这条）。与 elf 可同时配置：程序优先用**存在的** elf，
    /// 回退到 axf——同一份 yaml 可跨 Linux / Windows 使用
    #[serde(default)]
    pub axf: Option<PathBuf>,
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
        for (group, w) in &config.watch {
            if group.trim().is_empty() {
                bail!("watch 组的名字不能为空");
            }
            if w.interval_ms == 0 {
                bail!("watch 组 {group} 的 interval_ms 不能为 0");
            }
            if w.max_elems == 0 {
                bail!("watch 组 {group} 的 max_elems 不能为 0");
            }
            if w.symbols.is_empty() {
                bail!("watch 组 {group} 的 symbols 不能为空");
            }
            for sym in &w.symbols {
                if sym.trim().is_empty() {
                    bail!("watch 组 {group} 里有空的符号表达式");
                }
            }
        }
        for (name, p) in &config.plot {
            if name.trim().is_empty() {
                bail!("plot 配置名不能为空");
            }
            if p.interval_ms == 0 {
                bail!("plot 配置 {name} 的 interval_ms 不能为 0");
            }
            if p.window_secs <= 0.0 || !p.window_secs.is_finite() {
                bail!("plot 配置 {name} 的 window_secs 必须为正数");
            }
            if p.groups.is_empty() {
                bail!("plot 配置 {name} 的 groups 不能为空（至少一个图）");
            }
            for (i, g) in p.groups.iter().enumerate() {
                if g.is_empty() {
                    bail!(
                        "plot 配置 {name} 的第 {} 个图为空（每个图至少要有一个符号）",
                        i + 1
                    );
                }
                for sym in g {
                    if sym.trim().is_empty() {
                        bail!("plot 配置 {name} 里有空的符号表达式");
                    }
                }
            }
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
        if let Some(p) = &mut self.firmware.axf {
            absolutize_one(p, config_dir);
        }
    }

    /// 选择固件镜像文件：优先 `firmware.elf`（存在时），否则回退 `firmware.axf`。
    /// 两者都是 ELF 格式（Keil 的 .axf 本质即 ELF+DWARF），解析不看扩展名。
    /// 目的：同一份 tscope.yaml 在 Linux（GCC 产物）与 Windows（Keil 产物）
    /// 两个平台通用——哪边产物存在就用哪边。
    pub fn firmware_image(&self) -> Result<&Path> {
        if let Some(p) = self.firmware.elf.as_deref() {
            if p.is_file() {
                return Ok(p);
            }
        }
        if let Some(p) = self.firmware.axf.as_deref() {
            if p.is_file() {
                return Ok(p);
            }
        }
        let show = |p: &Option<PathBuf>| match p {
            Some(p) => format!(
                "{}（{}）",
                p.display(),
                if p.exists() {
                    "存在但不是文件"
                } else {
                    "不存在"
                }
            ),
            None => "未配置".to_string(),
        };
        bail!(
            "找不到固件镜像：firmware.elf 为 {}，firmware.axf 为 {}；请确认路径（Keil 的 .axf 也可直接填）",
            show(&self.firmware.elf),
            show(&self.firmware.axf)
        )
    }
}

fn absolutize_one(p: &mut PathBuf, config_dir: &Path) {
    if p.is_relative() {
        *p = config_dir.join(&*p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_image_prefers_elf_falls_back_to_axf() {
        let dir = std::env::temp_dir().join(format!(
            "tscope-cfg-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let elf = dir.join("f.elf");
        let axf = dir.join("f.axf");
        std::fs::write(&elf, b"x").unwrap();
        std::fs::write(&axf, b"x").unwrap();

        let base = || ToolConfig {
            version: 1,
            probe: Default::default(),
            chip: ChipConfig {
                name: "x".to_string(),
                description: None,
                pack: None,
            },
            firmware: FirmwareConfig {
                elf: Some(elf.clone()),
                axf: Some(axf.clone()),
            },
            watch: Default::default(),
            plot: Default::default(),
        };

        // 两者都存在 → 优先 elf（保持既有行为）
        let cfg = base();
        assert_eq!(cfg.firmware_image().unwrap(), elf);

        // elf 不存在、axf 存在 → 回退 axf（Windows/Keil 场景）
        let mut cfg = base();
        cfg.firmware.elf = Some(dir.join("missing.elf"));
        assert_eq!(cfg.firmware_image().unwrap(), axf);

        // 都不存在 → 报错
        let mut cfg = base();
        cfg.firmware.elf = Some(dir.join("missing.elf"));
        cfg.firmware.axf = Some(dir.join("missing.axf"));
        assert!(cfg.firmware_image().is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
