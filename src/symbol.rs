//! 符号解析：从 ELF 调试信息（DWARF）里查找全局变量，按类型读出并打印其值。
//!
//! 当前支持范围（v2 最小版，与需求对齐）：
//! - 全局 / 静态的 C 基类型标量变量（float、int、uint8_t 等）
//! - 暂不支持：结构体成员、数组元素、指针解引用、函数局部变量
//!
//! 实现方式：直接用 gimli 遍历 DWARF，**只查目标符号**。
//! 曾经用 probe-rs-debug 的 VariableCache 全量扫描（官方 DAP server 的机制），
//! 但那个机制会把固件里所有静态变量逐个经 SWD 读一遍值（含结构体成员树），
//! 在 1000 kHz 下耗时数秒。本实现只发生一次内存读，其余全是纯 CPU 解析。
//!
//! 静态变量的地址直接写在 DWARF 里（DW_AT_location），全程不暂停 CPU。

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use gimli::{
    AttributeValue, DebuggingInformationEntry, Dwarf, EndianRcSlice, Operation, Reader,
    RunTimeEndian, SectionId, Unit, UnitOffset,
};
use object::{Object, ObjectSection};
use probe_rs::{Core, MemoryInterface};

/// 基类型编码（对应 DWARF 的 DW_ATE_*）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Float,
    Signed,
    Unsigned,
}

/// 从 ELF 里解析出的符号信息
struct SymbolInfo {
    address: u64,
    type_name: String,
    byte_size: usize,
    encoding: Encoding,
}

/// 查找名为 `symbol` 的全局/静态变量，读出其值并打印。
/// 找不到符号（或类型暂不支持）时返回错误。
pub fn print_global_value(elf_path: &Path, symbol: &str, core: &mut Core) -> Result<()> {
    let info = find_symbol(elf_path, symbol)?;

    // 唯一一次内存读：按 DWARF 给的字节数读整块
    let mut buf = vec![0u8; info.byte_size];
    core.read_8(info.address, &mut buf).with_context(|| {
        format!(
            "读取符号 {symbol} 失败（地址 0x{:08x}，{} 字节）",
            info.address, info.byte_size
        )
    })?;

    let value_text = format_value(&info, &buf)?;

    println!("{symbol} = {value_text}");
    println!("类型: {}    地址: 0x{:08x}", info.type_name, info.address);
    Ok(())
}

// ===========================================================================
// DWARF 查找
// ===========================================================================

/// 打开 ELF → 加载 DWARF 段 → 逐编译单元找目标变量
fn find_symbol(elf_path: &Path, symbol: &str) -> Result<SymbolInfo> {
    let data =
        std::fs::read(elf_path).with_context(|| format!("读取 ELF 失败：{}", elf_path.display()))?;
    let obj = object::File::parse(&*data).context("解析 ELF 失败（确认文件是 .elf 不是 .bin）")?;

    let endian = if obj.is_little_endian() {
        RunTimeEndian::Little
    } else {
        RunTimeEndian::Big
    };

    // 把 ELF 里的 .debug_* 段交给 gimli（缺失的段按空处理）
    let mut load_section =
        |id: SectionId| -> Result<EndianRcSlice<RunTimeEndian>, gimli::Error> {
            let bytes = obj
                .section_by_name(id.name())
                .map(|s| s.data().map(|d| d.to_vec()).unwrap_or_default())
                .unwrap_or_default();
            Ok(EndianRcSlice::new(
                std::rc::Rc::from(bytes.as_slice()),
                endian,
            ))
        };
    let dwarf_sections =
        gimli::DwarfSections::load(&mut load_section).context("加载 DWARF 调试信息失败")?;
    let dwarf = dwarf_sections.borrow(|section| section.clone());

    // 遍历所有编译单元，找到即返回
    let mut units = dwarf.units();
    while let Some(header) = units.next().context("遍历编译单元失败")? {
        let unit = dwarf.unit(header)?;
        if let Some(info) = search_unit(&dwarf, &unit, symbol)? {
            return Ok(info);
        }
    }

    bail!(
        "在 ELF 里没有找到符号 {symbol}。可能原因：\n\
         ① 名字拼写错误；\n\
         ② 它是局部变量 / 结构体成员 / 数组元素（当前版本不支持）；\n\
         ③ 被编译器彻底优化掉了（用 make OPT=-O0 重新编译再试）；\n\
         ④ ELF 与板上固件不是同一次构建，地址对不上。"
    )
}

/// 在单个编译单元里深度优先遍历 DIE 树，找名为 target 的 DW_TAG_variable
fn search_unit<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    target: &str,
) -> Result<Option<SymbolInfo>> {
    let mut entries = unit.entries();
    while let Some(entry) = entries.next_dfs().context("遍历 DIE 失败")? {
        if entry.tag() != gimli::DW_TAG_variable {
            continue;
        }
        if let Some(info) = parse_variable(dwarf, unit, entry, target)? {
            return Ok(Some(info));
        }
    }
    Ok(None)
}

/// 解析一个变量 DIE：名字匹配时取出地址与类型
fn parse_variable<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    entry: &DebuggingInformationEntry<R, usize>,
    target: &str,
) -> Result<Option<SymbolInfo>> {
    let mut matched = false;
    let mut address: Option<u64> = None;
    let mut type_offset: Option<UnitOffset<usize>> = None;

    for attr in entry.attrs().iter() {
        match attr.name() {
            // C 没有名字修饰，DW_AT_name 即可；兼容处理 linkage_name
            gimli::DW_AT_name
            | gimli::DW_AT_linkage_name
            | gimli::DW_AT_MIPS_linkage_name => {
                let raw = dwarf.attr_string(unit, attr.value())?;
                if raw.to_string_lossy()? == target {
                    matched = true;
                }
            }
            gimli::DW_AT_location => {
                address = static_address(unit, attr.value());
            }
            gimli::DW_AT_type => {
                type_offset = match attr.value() {
                    AttributeValue::UnitRef(offset) => Some(offset),
                    _ => None,
                };
            }
            _ => {}
        }
    }

    if !matched {
        return Ok(None);
    }

    // 找到了符号，但拿不到静态地址 → 几乎都是被优化掉了。
    // 注意：GCC 对优化掉的变量会发出 DW_OP_addr 0（地址 0 是"无处安放"的标记），
    // 而本芯片没有任何变量会落在地址 0，一并按"不可用"处理。
    let address = address.filter(|a| *a != 0).ok_or_else(|| {
        anyhow!(
            "符号 {target} 在 ELF 里存在，但读不出值：被编译器优化掉了\n\
             （用 make OPT=-O0 重新编译再试）。"
        )
    })?;
    let type_offset = type_offset.ok_or_else(|| anyhow!("符号 {target} 没有类型信息"))?;

    let (type_name, byte_size, encoding) =
        resolve_base_type(dwarf, unit, type_offset)
            .with_context(|| format!("解析符号 {target} 的类型失败（暂只支持标量）"))?;

    Ok(Some(SymbolInfo {
        address,
        type_name,
        byte_size,
        encoding,
    }))
}

/// 提取静态变量的地址：DW_AT_location 通常是常量地址（DW_OP_addr）
fn static_address<R: Reader<Offset = usize>>(
    unit: &Unit<R>,
    value: AttributeValue<R>,
) -> Option<u64> {
    match value {
        AttributeValue::Addr(addr) => Some(addr),
        AttributeValue::Exprloc(expr) => {
            let mut ops = expr.operations(unit.encoding());
            let first = match ops.next() {
                Ok(Some(op)) => op,
                _ => return None,
            };
            if matches!(ops.next(), Ok(Some(_))) {
                // 多操作数的复杂 location 表达式，暂不支持
                return None;
            }
            match first {
                Operation::Address { address } => Some(address),
                _ => None,
            }
        }
        _ => None,
    }
}

/// 沿 typedef / const / volatile 链一路追到 DW_TAG_base_type，返回
/// （类型名, 字节数, 编码）。结构体、数组、指针等在此明确报"暂不支持"。
fn resolve_base_type<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    mut offset: UnitOffset<usize>,
) -> Result<(String, usize, Encoding)> {
    loop {
        let entry = unit.entry(offset).context("读取类型条目失败")?;
        match entry.tag() {
            gimli::DW_TAG_base_type => {
                let mut name = None;
                let mut size = None;
                let mut encoding = None;

                for attr in entry.attrs().iter() {
                    match attr.name() {
                        gimli::DW_AT_name => {
                            let raw = dwarf.attr_string(unit, attr.value())?;
                            name = Some(raw.to_string_lossy()?.into_owned());
                        }
                        gimli::DW_AT_byte_size => size = attr.value().udata_value(),
                        // 注意：gimli 会把 DW_AT_encoding 规范化为
                        // AttributeValue::Encoding(DwAte)，而不是普通整数
                        gimli::DW_AT_encoding => {
                            encoding = match attr.value() {
                                AttributeValue::Encoding(ate) => Some(u64::from(ate.0)),
                                other => other.udata_value(),
                            };
                        }
                        _ => {}
                    }
                }

                let name = name.unwrap_or_else(|| "?".to_string());
                let size = size.ok_or_else(|| anyhow!("基类型 {name} 没有字节数"))? as usize;
                let encoding = match encoding {
                    Some(e) if e == gimli::DW_ATE_float.0 as u64 => Encoding::Float,
                    Some(e)
                        if e == gimli::DW_ATE_signed.0 as u64
                            || e == gimli::DW_ATE_signed_char.0 as u64 =>
                    {
                        Encoding::Signed
                    }
                    Some(e)
                        if e == gimli::DW_ATE_unsigned.0 as u64
                            || e == gimli::DW_ATE_unsigned_char.0 as u64
                            || e == gimli::DW_ATE_boolean.0 as u64 =>
                    {
                        Encoding::Unsigned
                    }
                    other => bail!("基类型 {name} 的编码 {other:?} 暂不支持"),
                };
                return Ok((name, size, encoding));
            }
            // 修饰类型：继续往下追
            gimli::DW_TAG_typedef
            | gimli::DW_TAG_const_type
            | gimli::DW_TAG_volatile_type
            | gimli::DW_TAG_atomic_type
            | gimli::DW_TAG_restrict_type => match entry.attr_value(gimli::DW_AT_type) {
                Some(AttributeValue::UnitRef(next)) => offset = next,
                _ => bail!("类型修饰链断裂（缺少 DW_AT_type）"),
            },
            gimli::DW_TAG_enumeration_type => bail!("枚举类型暂不支持"),
            gimli::DW_TAG_structure_type | gimli::DW_TAG_union_type => {
                bail!("结构体/联合体暂不支持")
            }
            gimli::DW_TAG_array_type => bail!("数组暂不支持"),
            gimli::DW_TAG_pointer_type | gimli::DW_TAG_reference_type => {
                bail!("指针/引用暂不支持")
            }
            other => bail!("未知类型条目 {other:?}"),
        }
    }
}

// ===========================================================================
// 值格式化
// ===========================================================================

/// 按编码 + 字节数把内存字节解释成可读文本（小端，本芯片为 Cortex-M 小端）
fn format_value(info: &SymbolInfo, buf: &[u8]) -> Result<String> {
    let le_u64 = |n: usize| -> u64 {
        let mut v = 0u64;
        for (i, b) in buf[..n].iter().enumerate() {
            v |= u64::from(*b) << (8 * i);
        }
        v
    };

    match (info.encoding, info.byte_size) {
        (Encoding::Float, 4) => {
            Ok(format!("{:?}", f32::from_le_bytes(buf[..4].try_into().unwrap())))
        }
        (Encoding::Float, 8) => {
            Ok(format!("{:?}", f64::from_le_bytes(buf[..8].try_into().unwrap())))
        }
        (Encoding::Signed, 1) => Ok(format!("{}", buf[0] as i8)),
        (Encoding::Signed, 2) => Ok(format!("{}", le_u64(2) as i16)),
        (Encoding::Signed, 4) => Ok(format!("{}", le_u64(4) as i32)),
        (Encoding::Signed, 8) => Ok(format!("{}", le_u64(8) as i64)),
        (Encoding::Unsigned, 1) => Ok(format!("{}", buf[0])),
        (Encoding::Unsigned, 2) => Ok(format!("{}", le_u64(2) as u16)),
        (Encoding::Unsigned, 4) => Ok(format!("{}", le_u64(4) as u32)),
        (Encoding::Unsigned, 8) => Ok(format!("{}", le_u64(8))),
        _ => bail!(
            "不支持的编码/大小组合：{:?} × {} 字节",
            info.encoding,
            info.byte_size
        ),
    }
}
