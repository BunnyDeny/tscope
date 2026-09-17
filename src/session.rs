//! 会话建立：实现项目商定的四级芯片解析 fallback。
//!
//! ```
//! 配置里的 chip.name（如 "GD32F503RE"）
//!     │
//!     ├─ ① 内置 Registry 命中 → 直接使用（零额外文件）
//!     │
//!     └─ 未命中（ChipNotFound）
//!         ├─ ② 配置里有 chip.description 且文件存在
//!         │      → 加载进 Registry → 再按名字取变体
//!         │      → 若变体仍不存在，报「描述文件里没有这个型号」
//!         │
//!         ├─ ③ 描述文件缺失 / 未配置 → 打印 target-gen 指引
//!         │      （chip.pack 存在时给出可直接复制的命令）
//!         │
//!         └─ ④ 以上皆无 → 报错退出，信息里带方案 A / 方案 B
//! ```
//!
//! 三种错误信息刻意分开：内置无此型号 / 描述文件不存在 / 描述文件里无此变体。

use anyhow::{anyhow, bail, Context, Result};
use probe_rs::config::{Registry, RegistryError, Target, TargetSelector};
use probe_rs::probe::list::Lister;
use probe_rs::probe::{DebugProbeInfo, Probe, WireProtocol};
use probe_rs::{Permissions, Session};

use crate::config::{ChipConfig, ProbeConfig, ProbeSelector, Protocol};

/// 建立调试会话：芯片解析（四级 fallback）+ 探针打开
pub fn open_session(probe_cfg: &ProbeConfig, chip_cfg: &ChipConfig) -> Result<Session> {
    // Registry 必须贯穿始终使用同一个：resolve 用它取 Target，attach 也用它
    let mut registry = Registry::from_builtin_families();
    let target = resolve_target(&mut registry, chip_cfg)?;

    let probe = open_probe(probe_cfg)?;

    probe
        .attach_with_registry(
            TargetSelector::Specified(target),
            Permissions::default(),
            &registry,
        )
        .context("attach 失败：检查探针连接、芯片上电、SWD 接线与时钟速度")
}

/// 列出所有调试探针（不需要配置文件，用于排查连接问题）
pub fn list_probes() {
    let lister = Lister::new();
    let probes = lister.list_all();
    if probes.is_empty() {
        println!("（未发现调试探针：检查 USB 连接 / udev 规则）");
        return;
    }
    for (i, p) in probes.iter().enumerate() {
        println!(
            "[{i}] {:<24} vid=0x{:04x} pid=0x{:04x} sn={}",
            p.identifier,
            p.vendor_id,
            p.product_id,
            p.serial_number.as_deref().unwrap_or("(无)")
        );
    }
}

/// 四级 fallback：把 chip.name 解析成可用的 Target
fn resolve_target(registry: &mut Registry, chip: &ChipConfig) -> Result<Target> {
    let name = chip.name.trim();

    // ① 内置列表
    if let Ok(target) = registry.get_target_by_name(name) {
        return Ok(target);
    }

    // ② 配置里指定的芯片描述文件
    if let Some(desc_path) = &chip.description {
        let yaml = std::fs::read_to_string(desc_path).with_context(|| {
            format!(
                "芯片 {name} 不在 probe-rs 内置支持列表，且配置指定的芯片描述文件不存在：{}",
                desc_path.display()
            )
        })?;

        registry.add_target_family_from_yaml(&yaml).with_context(|| {
            format!("芯片描述文件解析失败：{}", desc_path.display())
        })?;

        return registry.get_target_by_name(name).map_err(|e| match e {
            // 文件加载成功但里面没有这个变体名 —— 和「文件不存在」是两种病，分开报
            RegistryError::ChipNotFound(_) => anyhow!(
                "芯片描述文件 {} 里没有名为 {name} 的变体：\n\
                 请检查 chip.name 拼写（必须与描述文件里的变体名精确一致），\n\
                 或换用正确的 CMSIS-Pack 重新生成描述文件。",
                desc_path.display()
            ),
            // chip.name 写得太短，前缀匹配命中多个型号，无法唯一确定
            RegistryError::ChipNotUnique(matched, candidates) => anyhow!(
                "chip.name \"{matched}\" 匹配到多个型号（{candidates}），无法唯一确定：\n\
                 请把 chip.name 写成完整型号名（可用 grep \"^- name:\" 查看描述文件里的全部变体）。"
            ),
            RegistryError::ChipAutodetectFailed => {
                anyhow!("无法自动识别连接的芯片：请在配置里显式指定 chip.name。")
            }
            // 描述文件里的内核类型当前 probe-rs 版本不认识
            RegistryError::UnknownCoreType(core_type) => anyhow!(
                "描述文件里 {name} 的内核类型 \"{core_type}\" 当前 probe-rs 版本不支持：\n\
                 请升级 probe-rs，或检查所用的 CMSIS-Pack 是否与该芯片匹配。"
            ),
            RegistryError::Io(e) => anyhow!("读取芯片描述文件时发生 IO 错误：{e}"),
            RegistryError::Yaml(e) => anyhow!("芯片描述文件 YAML 解析失败：{e}"),
            // 数据级校验失败（校验发生在 get_target_by_name，而非加载时）
            RegistryError::InvalidChipFamilyDefinition(family, reason) => anyhow!(
                "芯片描述文件的数据校验未通过（家族 {}）：{reason}",
                family.name
            ),
        });
    }

    // ③④ 没有描述文件 → 打印带指引的错误
    Err(unsupported_chip_help(name, chip))
}

/// 构造「芯片不受支持」的指引信息
fn unsupported_chip_help(name: &str, chip: &ChipConfig) -> anyhow::Error {
    let mut msg = format!(
        "芯片 {name} 不在 probe-rs 内置支持列表，且配置里没有 chip.description。\n\
         \n\
         方案 A —— 已有描述文件（target-gen 产物），在 tscope.yaml 里加：\n\
         \x20   chip:\n\
         \x20     name: {name}\n\
         \x20     description: targets/xxx_Series.yaml\n\
         \n\
         方案 B —— 有 CMSIS-Pack，先用 target-gen 生成描述文件：\n"
    );
    if let Some(pack) = &chip.pack {
        msg.push_str(&format!(
            "\x20   target-gen pack {} ./targets/\n",
            pack.display()
        ));
    } else {
        msg.push_str(
            "\x20   target-gen pack <厂商DFP.pack 或 Keil 已解压目录> ./targets/\n",
        );
    }
    msg.push_str("\n   然后把生成文件的路径写进 chip.description。");
    anyhow!(msg)
}

/// 按配置打开调试探针
fn open_probe(cfg: &ProbeConfig) -> Result<Probe> {
    let lister = Lister::new();
    let probes = lister.list_all();
    if probes.is_empty() {
        bail!(
            "没有发现调试探针：检查 USB 连接与 udev 规则；\n\
             （一个探针同一时刻只能被一个进程占用，请先退出 VSCode 调试 / probe-rs dap-server）"
        );
    }

    let info = select_probe(&probes, cfg.selector.as_ref())?;
    let mut probe = info
        .open()
        .with_context(|| format!("打开探针 {} 失败", info.identifier))?;

    probe
        .select_protocol(match cfg.protocol {
            Protocol::Swd => WireProtocol::Swd,
            Protocol::Jtag => WireProtocol::Jtag,
        })
        .context("切换调试协议失败")?;

    probe.set_speed(cfg.speed_khz).context("设置时钟速度失败")?;
    Ok(probe)
}

/// 从枚举结果里挑出要用的探针
fn select_probe<'a>(
    probes: &'a [DebugProbeInfo],
    selector: Option<&ProbeSelector>,
) -> Result<&'a DebugProbeInfo> {
    match selector {
        None => {
            if probes.len() == 1 {
                Ok(&probes[0])
            } else {
                let mut list = String::new();
                for (i, p) in probes.iter().enumerate() {
                    list.push_str(&format!(
                        "\n  [{i}] {}  vid=0x{:04x} pid=0x{:04x} sn={}",
                        p.identifier,
                        p.vendor_id,
                        p.product_id,
                        p.serial_number.as_deref().unwrap_or("(无)")
                    ));
                }
                bail!(
                    "发现 {} 个调试探针，请在配置的 probe.selector 里用 vid/pid/serial 指定其中一个：{list}",
                    probes.len()
                )
            }
        }
        Some(sel) => probes
            .iter()
            .find(|p| {
                sel.vid.map_or(true, |v| p.vendor_id == v)
                    && sel.pid.map_or(true, |v| p.product_id == v)
                    && sel
                        .serial
                        .as_ref()
                        .map_or(true, |s| p.serial_number.as_deref() == Some(s))
            })
            .ok_or_else(|| anyhow!("没有找到匹配 probe.selector 的探针（检查 vid/pid/serial）")),
    }
}
