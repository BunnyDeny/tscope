//! 符号解析：从 ELF 调试信息（DWARF）里查找全局变量，按类型读出并打印其值。
//!
//! 支持范围：
//! - C 基类型标量（float / int / uint8_t 等）
//! - 数组（一维/多维）、结构体、联合体，任意嵌套（如结构体数组）
//! - 枚举：尽量解析枚举器名字，解析不了回退打印整数
//! - 指针成员：只显示地址值（NULL 显示 NULL），不追踪
//! - 位域：明确报"暂不支持"（不中断整个结构体的打印）
//!
//! 实现方式：直接用 gimli 遍历 DWARF，**只查目标符号**，整个变量
//! 一次内存读回（struct/数组都只读一次），其余全是纯 CPU 解析。
//! 静态变量的地址直接写在 DWARF 里，全程不暂停 CPU。

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use gimli::{
    AttributeValue, DebuggingInformationEntry, Dwarf, EndianRcSlice, Operation, Reader,
    RunTimeEndian, SectionId, Unit, UnitOffset,
};
use object::{Object, ObjectSection, ObjectSymbol};
use probe_rs::{Core, MemoryInterface};

// ===========================================================================
// 类型模型
// ===========================================================================

/// 基类型编码（对应 DWARF 的 DW_ATE_*）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    Float,
    Signed,
    Unsigned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompoundKind {
    Struct,
    Union,
}

#[derive(Debug, Clone)]
pub(crate) struct Member {
    name: String,
    offset: usize,
    ty: TypeDesc,
}

/// 递归类型描述
#[derive(Debug, Clone)]
pub(crate) enum TypeDesc {
    Base {
        name: String,
        byte_size: usize,
        encoding: Encoding,
    },
    Pointer {
        name: String,
        byte_size: usize,
    },
    Enum {
        name: String,
        byte_size: usize,
        variants: Vec<(String, i64)>,
    },
    Array {
        name: String,
        elem: Box<TypeDesc>,
        count: usize,
    },
    Struct {
        name: String,
        kind: CompoundKind,
        byte_size: usize,
        members: Vec<Member>,
    },
    /// 无法解析/暂不支持的叶子（如位域）：打印占位说明而不中断整体
    Unsupported {
        name: String,
        reason: String,
    },
}

impl TypeDesc {
    pub(crate) fn byte_size(&self) -> Option<usize> {
        match self {
            TypeDesc::Base { byte_size, .. }
            | TypeDesc::Pointer { byte_size, .. }
            | TypeDesc::Enum { byte_size, .. }
            | TypeDesc::Struct { byte_size, .. } => Some(*byte_size),
            TypeDesc::Array { elem, count, .. } => Some(elem.byte_size()? * count),
            TypeDesc::Unsupported { .. } => None,
        }
    }

    pub(crate) fn name(&self) -> &str {
        match self {
            TypeDesc::Base { name, .. }
            | TypeDesc::Pointer { name, .. }
            | TypeDesc::Enum { name, .. }
            | TypeDesc::Array { name, .. }
            | TypeDesc::Struct { name, .. }
            | TypeDesc::Unsupported { name, .. } => name,
        }
    }
}

/// 打印选项
pub struct FmtOptions {
    /// 数组最多显示的元素个数（None = 全部）
    pub max_elems: Option<usize>,
    /// 嵌套展开深度上限（防自引用等极端情况）
    pub max_depth: usize,
}

/// 解析出的顶层符号
struct SymbolInfo {
    address: u64,
    ty: TypeDesc,
}

// ===========================================================================
// 入口
// ===========================================================================

/// 访问路径段：结构体成员名或数组下标
#[derive(Debug, Clone)]
enum PathSeg {
    Field(String),
    Index(usize),
}

/// 把表达式拆成「基础符号名 + 访问路径」。
/// 例：
///   theta_ref                          → ("theta_ref", [])
///   ENC_1_POS_SENSOR.readAngleCmd      → ("ENC_1_POS_SENSOR", [Field])
///   items[0].v.x                       → ("items", [Index(0), Field(v), Field(x)])
///   matrix[1][2]                       → ("matrix", [Index(1), Index(2)])
fn parse_expr(expr: &str) -> Result<(String, Vec<PathSeg>)> {
    let mut segs: Vec<PathSeg> = Vec::new();

    for part in expr.split('.') {
        if part.is_empty() {
            bail!("表达式 {expr} 里有空的段（连续的点？）");
        }
        let mut rest = part;
        if let Some(b) = rest.find('[') {
            let field = &rest[..b];
            if !field.is_empty() {
                segs.push(PathSeg::Field(field.to_string()));
            }
            while let Some(b) = rest.find('[') {
                let e = rest
                    .find(']')
                    .ok_or_else(|| anyhow!("表达式 {expr} 里缺少 ']'"))?;
                let num: usize = rest[b + 1..e]
                    .trim()
                    .parse()
                    .with_context(|| format!("表达式 {expr} 里下标必须是数字"))?;
                segs.push(PathSeg::Index(num));
                rest = &rest[e + 1..];
            }
        } else {
            segs.push(PathSeg::Field(part.to_string()));
        }
    }

    let base = match segs.first() {
        Some(PathSeg::Field(f)) => f.clone(),
        _ => bail!("表达式必须以符号名开头（不能以 [下标] 开头）"),
    };
    segs.remove(0);
    Ok((base, segs))
}

/// 沿访问路径从根类型走到目标字段，返回（相对根的字节偏移, 字段类型）。
/// 只做纯 CPU 的偏移计算，不读内存。
fn walk_path<'a>(root: &'a TypeDesc, path: &[PathSeg]) -> Result<(usize, &'a TypeDesc)> {
    let mut cur = root;
    let mut offset = 0usize;

    for seg in path {
        match seg {
            PathSeg::Field(name) => match cur {
                TypeDesc::Struct { members, .. } => {
                    let m = members
                        .iter()
                        .find(|m| &m.name == name)
                        .ok_or_else(|| {
                            anyhow!("类型 {} 里没有成员 {name}", cur.name())
                        })?;
                    offset += m.offset;
                    cur = &m.ty;
                }
                other => bail!("{} 不是结构体/联合体，无法访问成员 {name}", other.name()),
            },
            PathSeg::Index(i) => match cur {
                TypeDesc::Array { elem, count, .. } => {
                    if *i >= *count {
                        bail!("下标 {i} 越界（数组长度 {count}）");
                    }
                    offset += i * elem.byte_size().unwrap_or(0);
                    cur = elem;
                }
                other => bail!("{} 不是数组，无法用下标访问", other.name()),
            },
        }
    }

    Ok((offset, cur))
}

/// 解析完成、可重复采样的符号。
/// 解析只做一次（ELF/DWARF 全部是 CPU 工作），采样每次只做一次内存读。
/// 解析完成、可重复采样的符号。
/// 解析只做一次（ELF/DWARF 全部是 CPU 工作），采样每次只做一次内存读。
#[derive(Clone)]
pub struct PreparedSymbol {
    /// 基础变量的地址（成员路径相对它的偏移）
    address: u64,
    /// 基础变量的字节数
    base_size: usize,
    /// 目标字段在基础变量内的偏移
    field_offset: usize,
    /// 目标字段的类型描述
    field_ty: TypeDesc,
}

impl PreparedSymbol {
    /// 目标字段的绝对地址
    pub fn field_address(&self) -> u64 {
        self.address + self.field_offset as u64
    }

    pub fn type_name(&self) -> &str {
        self.field_ty.name()
    }

    pub fn byte_size(&self) -> usize {
        self.field_ty.byte_size().unwrap_or(0)
    }

    /// 读整个基础变量（**一次**内存读）。多个数组元素行共享这份缓冲区。
    pub fn read_base(&self, core: &mut Core) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.base_size];
        core.read_8(self.address, &mut buf).with_context(|| {
            format!(
                "读取 0x{:08x}（{} 字节）失败",
                self.address, self.base_size
            )
        })?;
        Ok(buf)
    }

    /// 若目标字段是复合类型（数组/结构体/联合体），展开为子项行列表
    /// （**最多 max_elems 个**，修正 Keil 全量刷屏的缺陷）：
    /// 返回（子项行, 未显示的剩余项数）。
    /// 子项行 = （显示标签后缀，相对字段的字节偏移, 项类型）。
    /// 标签以 `.` 或 `[` 开头，便于调用方拼在表达式后面：
    ///   结构体成员 → `.member`（嵌套为 `.v.x`）
    ///   数组元素   → `[i]`（多维为 `[i][j]`）
    /// 数组与结构体可任意交错（结构体数组 → `.member[i]` 等）。
    /// 非复合类型返回 None。
    pub fn child_rows(&self, max_elems: usize) -> Option<(Vec<(String, usize, TypeDesc)>, usize)> {
        let total = leaf_count(&self.field_ty);
        match &self.field_ty {
            TypeDesc::Array { .. } | TypeDesc::Struct { .. } => {
                let mut out = Vec::new();
                collect_child_rows(&self.field_ty, 0, "", max_elems, &mut out);
                let remaining = total.saturating_sub(out.len());
                Some((out, remaining))
            }
            _ => None,
        }
    }

    /// 从已读回的基础缓冲区渲染指定偏移处的元素为**单行**文本
    pub fn render_at(&self, buf: &[u8], offset: usize, ty: &TypeDesc) -> String {
        let size = ty.byte_size().unwrap_or(0);
        let start = self.field_offset + offset;
        let slice = buf.get(start..start + size).unwrap_or(&[]);
        render_compact(slice, ty)
    }

    /// 从已读回的基础缓冲区渲染目标字段本身（单行）
    pub fn render_self(&self, buf: &[u8]) -> String {
        self.render_at(buf, 0, &self.field_ty)
    }

    /// 采样缓冲区分组缓存用：标识同一个基础变量
    pub(crate) fn base_key(&self) -> (u64, usize) {
        (self.address, self.base_size)
    }
}

/// 展平后的标量叶子总数（用于统计未显示的剩余量）
fn leaf_count(ty: &TypeDesc) -> usize {
    match ty {
        TypeDesc::Array { elem, count, .. } => count * leaf_count(elem),
        TypeDesc::Struct { members, .. } => members.iter().map(|m| leaf_count(&m.ty)).sum(),
        _ => 1,
    }
}

/// 递归展平复合类型：结构体成员用 `.name`、数组元素用 `[i]` 进入标签；
/// 到达 budget 即停止
fn collect_child_rows(
    ty: &TypeDesc,
    base_off: usize,
    label: &str,
    budget: usize,
    out: &mut Vec<(String, usize, TypeDesc)>,
) {
    if out.len() >= budget {
        return;
    }
    match ty {
        TypeDesc::Array { elem, count, .. } => {
            let Some(elem_size) = elem.byte_size() else {
                out.push((format!("{label}[…]"), base_off, ty.clone()));
                return;
            };
            for i in 0..*count {
                if out.len() >= budget {
                    return;
                }
                collect_child_rows(
                    elem,
                    base_off + i * elem_size,
                    &format!("{label}[{i}]"),
                    budget,
                    out,
                );
            }
        }
        TypeDesc::Struct { members, .. } => {
            for m in members {
                if out.len() >= budget {
                    return;
                }
                collect_child_rows(&m.ty, base_off + m.offset, &format!("{label}.{}", m.name), budget, out);
            }
        }
        // 标量 / 指针 / 枚举 / 位域（不支持占位）：叶子行
        other => out.push((label.to_string(), base_off, other.clone())),
    }
}

/// watch 单元格用的紧凑渲染：全部结果保证单行。
/// - 标量 / 指针 / 枚举：正常值
/// - 数组：`[v0, v1, …]`（防御分支：watch 顶层数组已展开成多行，正常不会走到；
///   仅嵌套在结构体等场景可能触达，最多内联 8 个元素）
/// - 结构体 / 联合体 / 不支持类型：折叠为「类型名 (N 字节)」或占位说明
const MAX_INLINE_ELEMS: usize = 8;

fn render_compact(buf: &[u8], ty: &TypeDesc) -> String {
    match ty {
        TypeDesc::Array { elem, count, .. } => {
            let Some(elem_size) = elem.byte_size() else {
                return format!("{} (元素暂不支持)", ty.name());
            };
            let shown = (*count).min(MAX_INLINE_ELEMS);
            let mut parts: Vec<String> = Vec::new();
            for i in 0..shown {
                match buf.get(i * elem_size..(i + 1) * elem_size) {
                    Some(slice) => parts.push(render_compact(slice, elem)),
                    None => parts.push("?".to_string()),
                }
            }
            if shown < *count {
                parts.push(format!("…余{}", count - shown));
            }
            format!("[{}]", parts.join(", "))
        }
        TypeDesc::Struct { .. } => {
            format!("{} ({} 字节)", ty.name(), ty.byte_size().unwrap_or(0))
        }
        TypeDesc::Unsupported { reason, .. } => format!("〈{reason}〉"),
        // 标量 / 指针 / 枚举：复用单行渲染
        _ => render(
            buf,
            ty,
            &FmtOptions {
                max_elems: Some(MAX_INLINE_ELEMS),
                max_depth: 1,
            },
            0,
        ),
    }
}

/// 解析表达式为可采样符号（不读内存）
pub fn prepare_symbol(elf_path: &Path, expr: &str) -> Result<PreparedSymbol> {
    let (base, path) = parse_expr(expr)?;
    let info = find_symbol(elf_path, &base)?;
    let (field_offset, field_ty) = walk_path(&info.ty, &path)?;
    let base_size = info
        .ty
        .byte_size()
        .ok_or_else(|| anyhow!("符号 {base} 的类型暂不支持（{}）", info.ty.name()))?;

    Ok(PreparedSymbol {
        address: info.address,
        base_size,
        field_offset,
        field_ty: field_ty.clone(),
    })
}

/// 查找名为 `symbol` 的全局/静态变量，读出其值并打印。
pub fn print_global_value(
    elf_path: &Path,
    expr: &str,
    core: &mut Core,
    opts: &FmtOptions,
) -> Result<()> {
    let prepared = prepare_symbol(elf_path, expr)?;

    // 唯一一次内存读：读整个基础变量（目标字段也在其中）
    let mut buf = vec![0u8; prepared.base_size];
    core.read_8(prepared.address, &mut buf).with_context(|| {
        format!(
            "读取符号 {expr} 失败（地址 0x{:08x}，{} 字节）",
            prepared.address, prepared.base_size
        )
    })?;

    let size = prepared.byte_size();
    let slice = buf
        .get(prepared.field_offset..prepared.field_offset + size)
        .ok_or_else(|| {
            anyhow!(
                "字段数据越界：偏移 {} 大小 {size}，但基础变量只有 {} 字节",
                prepared.field_offset,
                prepared.base_size
            )
        })?;

    let rendered = render(slice, &prepared.field_ty, opts, 0);

    if rendered.contains('\n') {
        println!(
            "{expr} ({}) = {}",
            prepared.field_ty.name(),
            rendered.trim_start()
        );
    } else {
        println!("{expr} = {rendered}");
    }
    println!(
        "类型: {}    地址: 0x{:08x}",
        prepared.field_ty.name(),
        prepared.field_address()
    );
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

    // 第一轮：扫 DWARF。一个符号可能出现在多个编译单元里
    // （头文件的声明 + 源文件的定义），定义才有 DW_AT_location。
    // 所以"匹配但无地址"不立刻报错，先继续找带地址的定义。
    let mut any_match = false;
    let mut best_address: Option<u64> = None;
    // 类型偏移必须回到它所属的编译单元里解析，所以连同单元头一起记
    type UnitHeaderT = gimli::UnitHeader<EndianRcSlice<RunTimeEndian>>;
    let mut best_type_loc: Option<(UnitOffset<usize>, UnitHeaderT)> = None;
    let mut addr_type_loc: Option<(UnitOffset<usize>, UnitHeaderT)> = None;

    let mut units = dwarf.units();
    while let Some(header) = units.next().context("遍历编译单元失败")? {
        let unit = dwarf.unit(header.clone())?;
        let mut entries = unit.entries();
        while let Some(entry) = entries.next_dfs().context("遍历 DIE 失败")? {
            if entry.tag() != gimli::DW_TAG_variable {
                continue;
            }
            if !entry_name_matches(&dwarf, &unit, entry, symbol)? {
                continue;
            }
            any_match = true;

            let ty = match entry.attr_value(gimli::DW_AT_type) {
                Some(AttributeValue::UnitRef(o)) => Some((o, header.clone())),
                _ => None,
            };
            if ty.is_some() && best_type_loc.is_none() {
                best_type_loc = ty;
            }

            // 地址取第一个带有效 location 的定义（过滤 GCC 的 DW_OP_addr 0 标记）
            let address = entry
                .attr_value(gimli::DW_AT_location)
                .and_then(|v| static_address(&unit, v))
                .filter(|a| *a != 0);
            if address.is_some() && best_address.is_none() {
                best_address = address;
                addr_type_loc = match entry.attr_value(gimli::DW_AT_type) {
                    Some(AttributeValue::UnitRef(o)) => Some((o, header.clone())),
                    _ => None,
                };
            }
        }
    }

    if !any_match {
        bail!(
            "在 ELF 里没有找到符号 {symbol}。可能原因：\n\
             ① 名字拼写错误；\n\
             ② 它是局部变量（当前版本只支持全局/静态变量）；\n\
             ③ 被编译器彻底优化掉了（用 make OPT=-O0 重新编译再试）；\n\
             ④ ELF 与板上固件不是同一次构建，地址对不上。"
        );
    }

    // 第二轮：DWARF 拿不到地址时回退到 ELF 符号表。
    // 有些全局变量 DWARF 里只有声明（extern 声明 + 定义在别的翻译单元却
    // 没留下 location），但 .symtab 里有链接后的真实地址，类型仍从 DWARF 取。
    let address = match best_address {
        Some(addr) => addr,
        None => {
            let sym_addr = symbol_table_address(&obj, symbol)?;
            if sym_addr == 0 {
                bail!(
                    "符号 {symbol} 在 ELF 里存在，但读不出值：被编译器优化掉了\n\
                     （用 make OPT=-O0 重新编译再试）。"
                );
            }
            sym_addr
        }
    };

    // 优先用"带地址那个 DIE"的类型；没有就用任意匹配 DIE 的类型（声明也带类型）
    let (type_offset, unit_header) = addr_type_loc
        .or(best_type_loc)
        .ok_or_else(|| anyhow!("符号 {symbol} 没有类型信息"))?;

    let unit = dwarf.unit(unit_header).context("重新打开编译单元失败")?;
    let ty = resolve_type(&dwarf, &unit, type_offset, None, 0)
        .with_context(|| format!("解析符号 {symbol} 的类型失败"))?;

    Ok(SymbolInfo { address, ty })
}

/// 名字匹配（DW_AT_name / linkage_name）
fn entry_name_matches<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    entry: &DebuggingInformationEntry<R, usize>,
    target: &str,
) -> Result<bool> {
    for attr in entry.attrs().iter() {
        match attr.name() {
            gimli::DW_AT_name
            | gimli::DW_AT_linkage_name
            | gimli::DW_AT_MIPS_linkage_name => {
                let raw = dwarf.attr_string(unit, attr.value())?;
                if raw.to_string_lossy()? == target {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

/// 在 ELF 符号表（.symtab）里找全局符号的链接后地址；找不到返回 0
fn symbol_table_address<'data>(
    obj: &object::File<'data, &'data [u8]>,
    name: &str,
) -> Result<u64> {
    for sym in obj.symbols() {
        let Ok(sym_name) = sym.name() else { continue };
        if sym_name != name {
            continue;
        }
        if sym.address() != 0 && (sym.is_definition() || !sym.is_common()) {
            return Ok(sym.address());
        }
    }
    Ok(0)
}

/// 查函数符号的地址（debug 的 bp 命令用）。
/// Cortex-M 是 Thumb 架构，符号地址最低位是 1（Thumb 标记），
/// 断点地址必须是偶数指令地址，这里统一清掉最低位。
pub fn code_symbol_address(elf_path: &Path, name: &str) -> Result<u64> {
    let data =
        std::fs::read(elf_path).with_context(|| format!("读取 ELF 失败：{}", elf_path.display()))?;
    let obj = object::File::parse(&*data).context("解析 ELF 失败")?;
    let addr = symbol_table_address(&obj, name)?;
    if addr == 0 {
        bail!(
            "在 ELF 符号表里没有找到符号 {name}：\n\
             检查拼写；局部变量与静态函数（static）不会出现在符号表里。"
        );
    }
    Ok(addr & !1)
}

/// 反查：包含 addr 的函数符号名（断点命中提示用）。找不到返回 None。
/// 取「起始地址最大且覆盖 addr」的 Text 符号，即最内层函数。
pub fn function_name_at(elf_path: &Path, addr: u64) -> Option<String> {
    let data = std::fs::read(elf_path).ok()?;
    let obj = object::File::parse(&*data).ok()?;
    let addr = addr & !1; // 清 Thumb 位
    let mut best: Option<(u64, String)> = None;
    for sym in obj.symbols() {
        if sym.kind() != object::SymbolKind::Text {
            continue;
        }
        let start = sym.address() & !1;
        let size = sym.size();
        if start <= addr && addr < start + size {
            let better = best.as_ref().is_none_or(|(bs, _)| start > *bs);
            if better {
                if let Ok(name) = sym.name() {
                    best = Some((start, name.to_string()));
                }
            }
        }
    }
    best.map(|(_, name)| name)
}

// ===========================================================================
// 行号表：file:line ↔ 地址
// ===========================================================================

/// 加载 ELF 的 DWARF 与 .debug_frame（栈回溯用），返回 (dwarf, debug_frame)
pub(crate) fn load_debug_data(
    elf_path: &Path,
) -> Result<(
    Dwarf<EndianRcSlice<RunTimeEndian>>,
    gimli::read::DebugFrame<EndianRcSlice<RunTimeEndian>>,
)> {
    let data =
        std::fs::read(elf_path).with_context(|| format!("读取 ELF 失败：{}", elf_path.display()))?;
    let obj = object::File::parse(&*data).context("解析 ELF 失败（确认文件是 .elf）")?;

    let endian = if obj.is_little_endian() {
        RunTimeEndian::Little
    } else {
        RunTimeEndian::Big
    };

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

    // .debug_frame：函数栈展开表（bt 命令用）
    let frame_bytes = obj
        .section_by_name(".debug_frame")
        .and_then(|s| s.data().ok())
        .map(|d| d.to_vec())
        .unwrap_or_default();
    let mut debug_frame = gimli::read::DebugFrame::from(EndianRcSlice::new(
        std::rc::Rc::from(frame_bytes.as_slice()),
        endian,
    ));
    // 关键：FDE 里的地址字段宽度必须与目标一致（32 位 ARM = 4 字节），
    // 默认是宿主机字长 8，不设置会解析出乱码地址。
    // 从 DWARF 第一个编译单元取地址宽度，取不到就按 4 处理。
    let address_size = dwarf
        .units()
        .next()
        .ok()
        .flatten()
        .map(|h| h.address_size())
        .unwrap_or(4);
    debug_frame.set_address_size(address_size);

    Ok((dwarf, debug_frame))
}

/// 加载 ELF 的 DWARF 并执行闭包（行号表查询等一次性用途）
fn with_dwarf<T>(
    elf_path: &Path,
    f: impl FnOnce(&Dwarf<EndianRcSlice<RunTimeEndian>>) -> Result<T>,
) -> Result<T> {
    let data =
        std::fs::read(elf_path).with_context(|| format!("读取 ELF 失败：{}", elf_path.display()))?;
    let obj = object::File::parse(&*data).context("解析 ELF 失败（确认文件是 .elf）")?;

    let endian = if obj.is_little_endian() {
        RunTimeEndian::Little
    } else {
        RunTimeEndian::Big
    };

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
    f(&dwarf)
}

/// 属性值 → 字符串（DW_AT_string 等）
fn attr_value_string<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    value: AttributeValue<R>,
) -> Option<String> {
    dwarf
        .attr_string(unit, value)
        .ok()?
        .to_string_lossy()
        .ok()
        .map(|s| s.into_owned())
}

/// 路径匹配强度：3 = 精确，2 = 后缀（组件边界），1 = 基名；不匹配返回 None
fn path_match_strength(candidate: &str, wanted: &str) -> Option<u32> {
    let norm = |s: &str| s.replace('\\', "/");
    let c = norm(candidate);
    let w = norm(wanted);
    if c == w {
        return Some(3);
    }
    if c.ends_with(&format!("/{w}")) {
        return Some(2);
    }
    if std::path::Path::new(&c).file_name() == std::path::Path::new(&w).file_name() {
        return Some(1);
    }
    None
}

/// 构建行号程序的文件表：索引 → 完整路径字符串
fn build_file_paths<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    header: &gimli::LineProgramHeader<R, usize>,
) -> Vec<String> {
    let comp_dir = unit
        .comp_dir
        .as_ref()
        .and_then(|s| s.to_string_lossy().ok())
        .map(|s| s.into_owned());

    let mut paths = Vec::new();
    // DWARF5 起索引 0 是当前编译文件；旧版本索引 0 保留给当前文件但不在
    // file_names 里（本项目固件是 DWARF5；取不到就留空串，行引用它时不匹配）
    for idx in 0..=header.file_names().len() {
        paths.push(resolve_file_path(
            dwarf,
            unit,
            header,
            idx as u64,
            comp_dir.as_deref(),
        ));
    }
    paths
}

/// 解析单个文件条目为完整路径（comp_dir + 目录表 + 文件名）
fn resolve_file_path<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    header: &gimli::LineProgramHeader<R, usize>,
    file_index: u64,
    comp_dir: Option<&str>,
) -> String {
    let Some(entry) = header.file(file_index) else {
        return String::new();
    };
    let name = attr_value_string(dwarf, unit, entry.path_name()).unwrap_or_default();
    if is_absolute_path(&name) {
        return name;
    }

    // 目录表条目：可能是相对路径（拼在 comp_dir 后面），也可能是绝对路径
    // （GCC 常把源文件的绝对目录放进 include_directories 表）
    let dir = entry
        .directory(header)
        .and_then(|v| attr_value_string(dwarf, unit, v));

    match dir.as_deref() {
        Some(d) if !d.is_empty() && d != "." => {
            if is_absolute_path(d) {
                format!("{d}/{name}")
            } else if let Some(c) = comp_dir.filter(|c| !c.is_empty()) {
                format!("{c}/{d}/{name}")
            } else {
                format!("{d}/{name}")
            }
        }
        _ => match comp_dir.filter(|c| !c.is_empty()) {
            Some(c) => format!("{c}/{name}"),
            None => name,
        },
    }
}

fn is_absolute_path(s: &str) -> bool {
    s.starts_with('/') || s.get(1..3) == Some(":\\")
}

/// 收集行号表里与 wanted 匹配的所有文件路径（去重，保留各自最高匹配强度）
fn collect_matching_files(
    dwarf: &Dwarf<EndianRcSlice<RunTimeEndian>>,
    wanted: &str,
) -> Vec<(u32, String)> {
    let mut found: Vec<(u32, String)> = Vec::new();
    let mut units = dwarf.units();
    while let Ok(Some(header)) = units.next() {
        let Ok(unit) = dwarf.unit(header) else { continue };
        let Some(program) = unit.line_program.clone() else {
            continue;
        };
        for path in build_file_paths(dwarf, &unit, program.header()) {
            if path.is_empty() {
                continue;
            }
            let Some(strength) = path_match_strength(&path, wanted) else {
                continue;
            };
            match found.iter_mut().find(|(_, p)| *p == path) {
                Some((s, _)) => *s = (*s).max(strength),
                None => found.push((strength, path)),
            }
        }
    }
    found
}

/// 把用户写的路径（完整/后缀/基名）解析成 ELF 行号表里的完整路径；
/// 同一级别匹配到多个文件时报歧义。list 命令用。
pub fn resolve_source_path(elf_path: &Path, wanted: &str) -> Result<String> {
    with_dwarf(elf_path, |dwarf| {
        let found = collect_matching_files(dwarf, wanted);
        let Some(max_strength) = found.iter().map(|f| f.0).max() else {
            bail!("行号表里没有找到源文件 {wanted}（支持完整路径/后缀/文件名匹配）");
        };
        let tops: Vec<&str> = found
            .iter()
            .filter(|f| f.0 == max_strength)
            .map(|f| f.1.as_str())
            .collect();
        if tops.len() > 1 {
            let list = tops
                .iter()
                .map(|p| format!("  {p}"))
                .collect::<Vec<_>>()
                .join("\n");
            bail!("{wanted} 匹配到多个文件：\n{list}\n请把路径写得更具体");
        }
        Ok(tops[0].to_string())
    })
}

/// 行号表正查：file:line → 该行第一条指令地址（bp 用）。
/// 路径三级匹配：精确 → 后缀 → 基名；同一级别匹配到多个文件时报歧义。
pub fn line_to_address(elf_path: &Path, wanted: &str, line: u64) -> Result<u64> {
    // (匹配强度, 地址, 实际文件路径)
    let mut candidates: Vec<(u32, u64, String)> = Vec::new();

    with_dwarf(elf_path, |dwarf| {
        let mut units = dwarf.units();
        while let Some(header) = units.next().context("遍历编译单元失败")? {
            let unit = dwarf.unit(header)?;
            let Some(program) = unit.line_program.clone() else {
                continue;
            };
            let paths = build_file_paths(dwarf, &unit, program.header());
            let mut rows = program.rows();
            while let Some((_, row)) = rows.next_row().context("遍历行号表失败")? {
                if row.line().map(std::num::NonZeroU64::get) != Some(line) {
                    continue;
                }
                let Some(path) = paths.get(row.file_index() as usize) else {
                    continue;
                };
                if path.is_empty() {
                    continue;
                }
                if let Some(strength) = path_match_strength(path, wanted) {
                    candidates.push((strength, row.address() & !1, path.clone()));
                }
            }
        }
        Ok(())
    })?;

    let Some(max_strength) = candidates.iter().map(|c| c.0).max() else {
        bail!(
            "在行号表里没有找到 {wanted}:{line}：\n\
             该行可能没有可执行代码，或路径写法不匹配（支持完整路径/后缀/文件名匹配）"
        );
    };

    // 同级最强匹配里按"实际文件"去重；多于一个文件 → 报歧义
    let mut files: Vec<(String, u64)> = Vec::new();
    for (strength, addr, path) in candidates {
        if strength != max_strength {
            continue;
        }
        if !files.iter().any(|(p, _)| *p == path) {
            files.push((path, addr));
        }
    }
    if files.len() > 1 {
        let list = files
            .iter()
            .map(|(p, _)| format!("  {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        bail!("{wanted} 匹配到多个文件：\n{list}\n请把路径写得更具体");
    }

    Ok(files[0].1)
}

/// 行号表反查：地址 → (文件, 行号)。断点命中/暂停时展示用。
pub fn address_to_line(elf_path: &Path, addr: u64) -> Option<(String, u64)> {
    let addr = addr & !1;
    // (地址, 文件, 行号)，取地址 ≤ addr 的最近一行
    let mut best: Option<(u64, String, u64)> = None;

    let _ = with_dwarf(elf_path, |dwarf| {
        let mut units = dwarf.units();
        while let Some(header) = units.next()? {
            let unit = dwarf.unit(header)?;
            let Some(program) = unit.line_program.clone() else {
                continue;
            };
            let paths = build_file_paths(dwarf, &unit, program.header());
            let mut rows = program.rows();
            while let Some((_, row)) = rows.next_row()? {
                let row_addr = row.address();
                if row_addr > addr {
                    break; // 行号表按地址递增，可提前退出
                }
                let Some(line) = row.line() else { continue };
                let path = paths
                    .get(row.file_index() as usize)
                    .cloned()
                    .unwrap_or_default();
                if path.is_empty() {
                    continue;
                }
                if best.as_ref().is_none_or(|(ba, _, _)| row_addr >= *ba) {
                    best = Some((row_addr, path, line.get()));
                }
            }
        }
        Ok(())
    });

    best.filter(|(_, p, _)| !p.is_empty())
        .map(|(_, p, l)| (p, l))
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

// ===========================================================================
// 类型解析（递归）
// ===========================================================================

const MAX_TYPE_DEPTH: usize = 16;

/// 取一个 DIE 的 DW_AT_name（若无则 None）
fn entry_name<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    entry: &DebuggingInformationEntry<R, usize>,
) -> Option<String> {
    for attr in entry.attrs().iter() {
        if attr.name() == gimli::DW_AT_name {
            let raw = dwarf.attr_string(unit, attr.value()).ok()?;
            return Some(raw.to_string_lossy().ok()?.into_owned());
        }
    }
    None
}

/// 沿修饰链快速取类型显示名（只追 typedef/const/volatile 与指针）
fn quick_name<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    offset: UnitOffset<usize>,
    depth: usize,
) -> String {
    if depth > 8 {
        return "…".to_string();
    }
    let Ok(entry) = unit.entry(offset) else {
        return "…".to_string();
    };
    if let Some(name) = entry_name(dwarf, unit, &entry) {
        return name;
    }
    match entry.tag() {
        gimli::DW_TAG_pointer_type | gimli::DW_TAG_reference_type => {
            match entry.attr_value(gimli::DW_AT_type) {
                Some(AttributeValue::UnitRef(next)) => {
                    format!("{}*", quick_name(dwarf, unit, next, depth + 1))
                }
                _ => "*".to_string(),
            }
        }
        gimli::DW_TAG_typedef
        | gimli::DW_TAG_const_type
        | gimli::DW_TAG_volatile_type
        | gimli::DW_TAG_atomic_type
        | gimli::DW_TAG_restrict_type => match entry.attr_value(gimli::DW_AT_type) {
            Some(AttributeValue::UnitRef(next)) => quick_name(dwarf, unit, next, depth + 1),
            _ => "…".to_string(),
        },
        _ => "<未命名类型>".to_string(),
    }
}

/// 把任意 DW_AT_type 偏移解析成 TypeDesc
fn resolve_type<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    offset: UnitOffset<usize>,
    inherited_name: Option<String>,
    depth: usize,
) -> Result<TypeDesc> {
    if depth > MAX_TYPE_DEPTH {
        bail!("类型嵌套过深（>{} 层）", MAX_TYPE_DEPTH);
    }
    let entry = unit.entry(offset).context("读取类型条目失败")?;

    match entry.tag() {
        gimli::DW_TAG_base_type => parse_base_type(dwarf, unit, &entry, inherited_name),

        // 修饰类型：记下最外层的 typedef 名作显示名，继续往下追
        gimli::DW_TAG_typedef
        | gimli::DW_TAG_const_type
        | gimli::DW_TAG_volatile_type
        | gimli::DW_TAG_atomic_type
        | gimli::DW_TAG_restrict_type => {
            let name = inherited_name.or_else(|| entry_name(dwarf, unit, &entry));
            match entry.attr_value(gimli::DW_AT_type) {
                Some(AttributeValue::UnitRef(next)) => {
                    resolve_type(dwarf, unit, next, name, depth + 1)
                }
                _ => bail!("类型修饰链断裂（缺少 DW_AT_type）"),
            }
        }

        gimli::DW_TAG_structure_type => {
            parse_compound(dwarf, unit, &entry, CompoundKind::Struct, inherited_name, depth)
        }
        gimli::DW_TAG_union_type => {
            parse_compound(dwarf, unit, &entry, CompoundKind::Union, inherited_name, depth)
        }
        gimli::DW_TAG_array_type => parse_array(dwarf, unit, &entry, inherited_name, depth),
        gimli::DW_TAG_enumeration_type => parse_enum(dwarf, unit, &entry, inherited_name),

        gimli::DW_TAG_pointer_type | gimli::DW_TAG_reference_type => {
            let pointee_name = match entry.attr_value(gimli::DW_AT_type) {
                Some(AttributeValue::UnitRef(next)) => quick_name(dwarf, unit, next, 0),
                _ => "?".to_string(),
            };
            let name = inherited_name.unwrap_or_else(|| format!("{pointee_name}*"));
            let byte_size = entry
                .attr_value(gimli::DW_AT_byte_size)
                .and_then(|v| v.udata_value())
                .unwrap_or(u64::from(unit.encoding().address_size)) as usize;
            Ok(TypeDesc::Pointer { name, byte_size })
        }

        // 函数指针等：解析目标类型无意义，标为不支持
        gimli::DW_TAG_subroutine_type => Ok(TypeDesc::Unsupported {
            name: inherited_name.unwrap_or_else(|| "<函数指针>".to_string()),
            reason: "函数指针暂不支持".to_string(),
        }),

        other => Ok(TypeDesc::Unsupported {
            name: inherited_name.unwrap_or_else(|| format!("{:?}", other)),
            reason: "不支持的类型".to_string(),
        }),
    }
}

/// 解析 DW_TAG_base_type
fn parse_base_type<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    entry: &DebuggingInformationEntry<R, usize>,
    inherited_name: Option<String>,
) -> Result<TypeDesc> {
    let mut base_name = None;
    let mut size = None;
    let mut encoding = None;

    for attr in entry.attrs().iter() {
        match attr.name() {
            gimli::DW_AT_name => {
                let raw = dwarf.attr_string(unit, attr.value())?;
                base_name = Some(raw.to_string_lossy()?.into_owned());
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

    let base_name = base_name.unwrap_or_else(|| "?".to_string());
    let name = inherited_name.unwrap_or_else(|| base_name.clone());
    let size = size
        .ok_or_else(|| anyhow!("基类型 {base_name} 没有字节数"))?
        as usize;
    let encoding = match encoding {
        Some(e) if e == gimli::DW_ATE_float.0 as u64 => Encoding::Float,
        Some(e) if e == gimli::DW_ATE_signed.0 as u64 || e == gimli::DW_ATE_signed_char.0 as u64 => {
            Encoding::Signed
        }
        Some(e)
            if e == gimli::DW_ATE_unsigned.0 as u64
                || e == gimli::DW_ATE_unsigned_char.0 as u64
                || e == gimli::DW_ATE_boolean.0 as u64 =>
        {
            Encoding::Unsigned
        }
        other => bail!("基类型 {base_name} 的编码 {other:?} 暂不支持"),
    };

    Ok(TypeDesc::Base {
        name,
        byte_size: size,
        encoding,
    })
}

/// 解析结构体 / 联合体：遍历子 DIE 收集成员
fn parse_compound<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    entry: &DebuggingInformationEntry<R, usize>,
    kind: CompoundKind,
    inherited_name: Option<String>,
    depth: usize,
) -> Result<TypeDesc> {
    let own_name = entry_name(dwarf, unit, entry);
    let name = own_name
        .or(inherited_name)
        .unwrap_or_else(|| match kind {
            CompoundKind::Struct => "<结构体>".to_string(),
            CompoundKind::Union => "<联合体>".to_string(),
        });

    let declared_size = entry
        .attr_value(gimli::DW_AT_byte_size)
        .and_then(|v| v.udata_value())
        .map(|v| v as usize);

    let mut members = Vec::new();

    let mut tree = unit.entries_tree(Some(entry.offset())).context("遍历成员失败")?;
    let root = tree.root()?;
    let mut children = root.children();
    while let Some(child) = children.next().context("遍历成员失败")? {
        let child_entry = child.entry();
        match child_entry.tag() {
            gimli::DW_TAG_member => {
                // 位域：不解析内部结构，占位说明
                if child_entry.attr(gimli::DW_AT_bit_size).is_some() {
                    let mname = entry_name(dwarf, unit, child_entry)
                        .unwrap_or_else(|| "<位域>".to_string());
                    members.push(Member {
                        name: mname.clone(),
                        offset: 0,
                        ty: TypeDesc::Unsupported {
                            name: mname,
                            reason: "位域暂不支持".to_string(),
                        },
                    });
                    continue;
                }

                let mname = entry_name(dwarf, unit, child_entry)
                    .unwrap_or_else(|| "<匿名成员>".to_string());

                // 联合体成员没有 data_member_location（全部偏移 0），按 0 处理；
                // 结构体成员缺偏移才是异常。
                let offset = member_offset(unit, child_entry)
                    .or(match kind {
                        CompoundKind::Union => Some(0),
                        CompoundKind::Struct => None,
                    })
                    .ok_or_else(|| {
                        anyhow!("成员 {mname} 没有可解析的偏移（location 表达式太复杂？）")
                    })?;

                let ty = match child_entry.attr_value(gimli::DW_AT_type) {
                    Some(AttributeValue::UnitRef(o)) => {
                        // 注意：这里不传成员名作 inherited_name —— typedef 名由
                        // 类型链自己提供；成员名只是成员名，不能冒充类型名
                        match resolve_type(dwarf, unit, o, None, depth + 1) {
                            Ok(ty) => ty,
                            Err(e) => TypeDesc::Unsupported {
                                name: mname.clone(),
                                reason: format!("成员类型解析失败: {e:#}"),
                            },
                        }
                    }
                    _ => TypeDesc::Unsupported {
                        name: mname.clone(),
                        reason: "成员缺少类型信息".to_string(),
                    },
                };

                members.push(Member {
                    name: mname,
                    offset,
                    ty,
                });
            }
            _ => {}
        }
    }

    // DW_AT_byte_size 缺失时自行推算（union 取最大成员大小）
    let byte_size = match declared_size {
        Some(s) => s,
        None => match kind {
            CompoundKind::Struct => members
                .iter()
                .filter_map(|m| Some(m.offset + m.ty.byte_size()?))
                .max()
                .unwrap_or(0),
            CompoundKind::Union => members
                .iter()
                .filter_map(|m| m.ty.byte_size())
                .max()
                .unwrap_or(0),
        },
    };

    Ok(TypeDesc::Struct {
        name,
        kind,
        byte_size,
        members,
    })
}

/// 结构体成员的字节偏移：常量或单操作数 location 表达式
fn member_offset<R: Reader<Offset = usize>>(
    unit: &Unit<R>,
    entry: &DebuggingInformationEntry<R, usize>,
) -> Option<usize> {
    let value = entry.attr_value(gimli::DW_AT_data_member_location)?;
    if let Some(v) = value.udata_value() {
        return Some(v as usize);
    }
    match value {
        AttributeValue::Exprloc(expr) => {
            let mut ops = expr.operations(unit.encoding());
            match ops.next() {
                Ok(Some(Operation::PlusConstant { value }))
                | Ok(Some(Operation::UnsignedConstant { value })) => Some(value as usize),
                _ => None,
            }
        }
        _ => None,
    }
}

/// 解析数组：元素类型 + subrange 上界/计数
fn parse_array<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    entry: &DebuggingInformationEntry<R, usize>,
    inherited_name: Option<String>,
    depth: usize,
) -> Result<TypeDesc> {
    let elem_offset = match entry.attr_value(gimli::DW_AT_type) {
        Some(AttributeValue::UnitRef(o)) => o,
        _ => bail!("数组缺少元素类型（DW_AT_type）"),
    };

    let mut dims: Vec<usize> = Vec::new();
    let mut tree = unit.entries_tree(Some(entry.offset())).context("遍历数组定义失败")?;
    let root = tree.root()?;
    let mut children = root.children();
    while let Some(child) = children.next().context("遍历数组定义失败")? {
        let child_entry = child.entry();
        if child_entry.tag() != gimli::DW_TAG_subrange_type {
            continue;
        }
        let mut this_dim = None;
        for attr in child_entry.attrs().iter() {
            match attr.name() {
                gimli::DW_AT_count => this_dim = attr.value().udata_value().map(|v| v as usize),
                gimli::DW_AT_upper_bound => {
                    this_dim = attr.value().udata_value().map(|v| v as usize + 1)
                }
                _ => {}
            }
        }
        if let Some(d) = this_dim {
            dims.push(d);
        }
    }

    if dims.is_empty() {
        bail!("数组没有元素个数（不完整类型？）");
    }

    let elem = resolve_type(dwarf, unit, elem_offset, None, depth + 1)?;
    if elem.byte_size().is_none() {
        bail!("数组元素类型暂不支持（{}）", elem.name());
    }

    // GCC 把多维数组拍平：一个 array_type 带多个 subrange（按声明顺序）。
    // 重建嵌套结构：从最内维往外包。
    let elem_name = elem.name().to_string();
    let full_name = format!(
        "{}{}",
        elem_name,
        dims.iter().map(|d| format!("[{d}]")).collect::<String>()
    );

    let mut ty = elem;
    let mut inner_name = elem_name;
    for d in dims.iter().rev() {
        inner_name = format!("{inner_name}[{d}]");
        ty = TypeDesc::Array {
            name: inner_name.clone(),
            elem: Box::new(ty),
            count: *d,
        };
    }
    // 最外层显示名：优先 typedef 名，其次完整 C 语法名（float[2][3]）
    let outer_name = inherited_name.unwrap_or(full_name);
    if let TypeDesc::Array { name, .. } = &mut ty {
        *name = outer_name;
    }

    Ok(ty)
}

/// 解析枚举：枚举器名 + 常量值
fn parse_enum<R: Reader<Offset = usize>>(
    dwarf: &Dwarf<R>,
    unit: &Unit<R>,
    entry: &DebuggingInformationEntry<R, usize>,
    inherited_name: Option<String>,
) -> Result<TypeDesc> {
    let name = entry_name(dwarf, unit, entry)
        .or(inherited_name)
        .unwrap_or_else(|| "<枚举>".to_string());
    let byte_size = entry
        .attr_value(gimli::DW_AT_byte_size)
        .and_then(|v| v.udata_value())
        .unwrap_or(4) as usize;

    let mut variants = Vec::new();
    let mut tree = unit.entries_tree(Some(entry.offset())).context("遍历枚举器失败")?;
    let root = tree.root()?;
    let mut children = root.children();
    while let Some(child) = children.next().context("遍历枚举器失败")? {
        let child_entry = child.entry();
        if child_entry.tag() != gimli::DW_TAG_enumerator {
            continue;
        }
        let vname = entry_name(dwarf, unit, child_entry).unwrap_or_else(|| "?".to_string());
        let value = child_entry
            .attr_value(gimli::DW_AT_const_value)
            .and_then(|v| v.sdata_value().or_else(|| v.udata_value().map(|u| u as i64)))
            .unwrap_or(0);
        variants.push((vname, value));
    }

    Ok(TypeDesc::Enum {
        name,
        byte_size,
        variants,
    })
}

// ===========================================================================
// 渲染
// ===========================================================================

fn indent(depth: usize) -> String {
    "    ".repeat(depth)
}

fn le_u64(buf: &[u8], n: usize) -> u64 {
    let mut v = 0u64;
    for (i, b) in buf[..n.min(buf.len())].iter().enumerate() {
        v |= u64::from(*b) << (8 * i);
    }
    v
}

/// 按编码 + 字节数把内存字节解释成可读文本（小端）
fn format_base(buf: &[u8], encoding: Encoding, byte_size: usize) -> String {
    match (encoding, byte_size) {
        (Encoding::Float, 4) => {
            format!("{:?}", f32::from_le_bytes(buf[..4].try_into().unwrap()))
        }
        (Encoding::Float, 8) => {
            format!("{:?}", f64::from_le_bytes(buf[..8].try_into().unwrap()))
        }
        (Encoding::Signed, 1) => format!("{}", buf[0] as i8),
        (Encoding::Signed, 2) => format!("{}", le_u64(buf, 2) as i16),
        (Encoding::Signed, 4) => format!("{}", le_u64(buf, 4) as i32),
        (Encoding::Signed, 8) => format!("{}", le_u64(buf, 8) as i64),
        (Encoding::Unsigned, 1) => format!("{}", buf[0]),
        (Encoding::Unsigned, 2) => format!("{}", le_u64(buf, 2) as u16),
        (Encoding::Unsigned, 4) => format!("{}", le_u64(buf, 4) as u32),
        (Encoding::Unsigned, 8) => format!("{}", le_u64(buf, 8)),
        _ => format!("〈不支持的编码/大小: {:?}×{} 字节〉", encoding, byte_size),
    }
}

/// 递归渲染。标量返回单行字符串；复合类型返回含换行的多行块。
fn render(buf: &[u8], ty: &TypeDesc, opts: &FmtOptions, depth: usize) -> String {
    match ty {
        TypeDesc::Base {
            encoding,
            byte_size,
            ..
        } => format_base(buf, *encoding, *byte_size),

        TypeDesc::Pointer { byte_size, .. } => {
            let v = le_u64(buf, *byte_size);
            if v == 0 {
                "NULL".to_string()
            } else {
                format!("0x{v:08x}")
            }
        }

        TypeDesc::Enum {
            byte_size,
            variants,
            ..
        } => {
            let raw = le_u64(buf, *byte_size) as i64;
            match variants.iter().find(|(_, v)| *v == raw) {
                Some((n, _)) => format!("{n}({raw})"),
                None => format!("{raw}"),
            }
        }

        TypeDesc::Array { elem, count, .. } => {
            if depth >= opts.max_depth {
                return "…".to_string();
            }
            let Some(elem_size) = elem.byte_size() else {
                return "〈元素类型暂不支持〉".to_string();
            };
            let shown = match opts.max_elems {
                Some(n) => (*count).min(n),
                None => *count,
            };

            let mut lines: Vec<String> = Vec::new();
            for i in 0..shown {
                let start = i * elem_size;
                let slice = buf.get(start..start + elem_size);
                let v = match slice {
                    Some(s) => render(s, elem, opts, depth + 1),
                    None => "〈数据越界〉".to_string(),
                };
                // 复合元素值的第一行自带缩进，顶到 "= " 后面
                lines.push(format!("{}[{i}] = {}", indent(depth + 1), v.trim_start()));
            }
            if shown < *count {
                lines.push(format!(
                    "{}… 其余 {} 个未显示（--all 查看全部）",
                    indent(depth + 1),
                    count - shown
                ));
            }
            lines.join("\n")
        }

        TypeDesc::Struct {
            members, kind, ..
        } => {
            if depth >= opts.max_depth {
                return "…".to_string();
            }
            let mut lines: Vec<String> = Vec::new();
            for m in members {
                let Some(size) = m.ty.byte_size() else {
                    lines.push(format!(
                        "{}{} ({}) = 〈{}〉",
                        indent(depth + 1),
                        m.name,
                        m.ty.name(),
                        match &m.ty {
                            TypeDesc::Unsupported { reason, .. } => reason.as_str(),
                            _ => "暂不支持",
                        }
                    ));
                    continue;
                };
                let slice = buf.get(m.offset..m.offset + size);
                let v = match slice {
                    Some(s) => render(s, &m.ty, opts, depth + 1),
                    None => "〈数据越界〉".to_string(),
                };
                if v.contains('\n') {
                    lines.push(format!(
                        "{}{} ({}) = {v}",
                        indent(depth + 1),
                        m.name,
                        m.ty.name()
                    ));
                } else {
                    lines.push(format!(
                        "{}{}: {v} ({})",
                        indent(depth + 1),
                        m.name,
                        m.ty.name()
                    ));
                }
            }
            match kind {
                CompoundKind::Struct => {
                    format!("{{\n{}\n{}}}", lines.join("\n"), indent(depth))
                }
                CompoundKind::Union => {
                    format!("union {{\n{}\n{}}}", lines.join("\n"), indent(depth))
                }
            }
        }

        TypeDesc::Unsupported { reason, .. } => format!("〈{reason}〉"),
    }
}
