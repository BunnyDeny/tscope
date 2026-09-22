//! bt 子命令：基于 .debug_frame 的函数调用栈回溯。
//!
//! 算法（与 gdb/probe-rs 同类）：
//! 1. 读当前 PC/SP/LR 及通用寄存器；
//! 2. 按 PC 在 .debug_frame 里找当前函数的 FDE（展开描述），取当前地址的
//!    展开行（UnwindTableRow）；
//! 3. CFA 规则给出"调用者的 SP"，各寄存器的规则（Offset/SameValue/Register）
//!    用于重建调用者上下文；返回地址 = LR 寄存器的规则求值；
//! 4. 逐帧向上，直到返回地址为 0、无展开信息（bootloader/汇编）、
//!    到达初始栈顶（读 SCB->VTOR，Cortex-M 通用）、或帧数上限。
//!
//! 中断（ISR）语义：回溯到中断服务函数为止。ISR 的返回地址规则是
//! Undefined，或其保存的 LR 是 EXC_RETURN（0xFFFFFFxx）——两种情况下
//! 都停止，不追溯"被打断的任务"（那是异常帧的领地，按需简化）。
//! 本模块不依赖 FPU 或具体芯片型号，对 M0/M3/M4/M33 通用。

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use gimli::read::{CfaRule, RegisterRule, UnwindContext, UnwindSection};
use probe_rs::{Core, MemoryInterface};

use crate::symbol;

/// 一帧调用栈
pub struct Frame {
    /// 帧内指令地址（返回地址，已清 Thumb 位）
    pub pc: u64,
}

/// Cortex-M 异常返回码特征：高 24 位全 1（0xFFFFFFxx）
pub fn is_exc_return(lr: u64) -> bool {
    lr & 0xFFFF_FF00 == 0xFFFF_FF00
}

/// 当前帧的返回地址（LR 规则求值）：源码级单步/finish"走出函数"用。
/// 中断函数最外层返回的是 EXC_RETURN，由调用方特殊处理。
pub fn frame_return_address(elf_path: &Path, core: &mut Core) -> Option<u64> {
    let (_, debug_frame) = symbol::load_debug_data(elf_path).ok()?;
    let bases = gimli::BaseAddresses::default();
    let mut ctx = UnwindContext::new();

    let regs = read_core_registers(core).ok()?;
    let pc = *regs.get(&15)? & !1;
    let lr = regs.get(&14).copied().unwrap_or(0);

    let fde = debug_frame
        .fde_for_address(&bases, pc, gimli::read::DebugFrame::cie_from_offset)
        .ok()?;
    let row = fde
        .unwind_info_for_address(&debug_frame, &bases, &mut ctx, pc)
        .ok()?;

    let CfaRule::RegisterAndOffset { register, offset } = row.cfa() else {
        return None;
    };
    let base = *regs.get(&register.0).unwrap_or(&0);
    let cfa = base.wrapping_add_signed(*offset);

    match row.register(gimli::Register(14)) {
        Some(RegisterRule::Undefined) => None,
        Some(RegisterRule::SameValue) | None => Some(lr),
        Some(RegisterRule::Offset(o)) => read_u32(core, cfa.wrapping_add_signed(o)).map(u64::from),
        Some(RegisterRule::Register(r)) => regs.get(&r.0).copied(),
        Some(_) => None,
    }
}

/// 当前活跃异常的中断向量入口函数地址（全部 Cortex-M 通用）：
/// SCB->ICSR 的 VECTACTIVE 字段（低 9 位）= 异常号；
/// 向量表第 [异常号] 项 = 该异常的入口函数地址。
fn active_vector_handler(core: &mut Core) -> Option<u64> {
    let icsr = read_u32(core, 0xE000_ED04)?;
    let vect_active = icsr & 0x1FF;
    if vect_active < 2 {
        // 0 = 无异常（普通上下文）；1 = 复位。都无需补帧
        return None;
    }
    let vtor = read_u32(core, 0xE000_ED08)?;
    let entry = read_u32(core, u64::from(vtor) + u64::from(vect_active) * 4)?;
    if entry == 0 {
        return None;
    }
    Some(u64::from(entry) & !1)
}

/// 从内核读出全部通用寄存器 → DWARF 编号映射
/// ARM DWARF 寄存器号：R0-R12 → 0-12，R13/SP → 13，R14/LR → 14，R15/PC → 15
fn read_core_registers(core: &mut Core) -> Result<HashMap<u16, u64>> {
    let mut map = HashMap::new();
    let table = core.registers();
    for reg in table.core_registers() {
        let dwarf_id = match reg.name() {
            "R0" | "R1" | "R2" | "R3" | "R4" | "R5" | "R6" | "R7" | "R8" | "R9" | "R10" | "R11"
            | "R12" => reg.name()[1..].parse::<u16>().ok(),
            "R13" | "SP" => Some(13),
            "R14" | "LR" | "RA" => Some(14),
            "R15" | "PC" => Some(15),
            _ => None,
        };
        let Some(dwarf_id) = dwarf_id else { continue };
        if let Ok(v) = core.read_core_reg::<u32>(reg.id()) {
            map.insert(dwarf_id, u64::from(v));
        }
    }
    Ok(map)
}

/// 读目标内存一个 32 位字；失败返回 None（断链）
fn read_u32(core: &mut Core, addr: u64) -> Option<u32> {
    core.read_word_32(addr).ok()
}

/// 执行栈回溯。max_frames 防栈损坏时死循环。
pub fn backtrace(elf_path: &Path, core: &mut Core, max_frames: usize) -> Result<Vec<Frame>> {
    let (dwarf, debug_frame) = symbol::load_debug_data(elf_path)?;
    let _ = &dwarf; // dwarf 暂未直接使用（帧信息用符号表/行号表查），保留给将来

    let bases = gimli::BaseAddresses::default();
    let mut ctx = UnwindContext::new();

    let mut regs = read_core_registers(core)?;
    let mut pc = *regs.get(&15).context("读 PC 失败（内核是否已暂停？）")? & !1;
    let mut lr = regs.get(&14).copied().unwrap_or(0);

    // 初始栈顶：读 SCB->VTOR（0xE000ED08）得到向量表地址，其第 0 个字即
    // 复位时的初始 SP。CFA 超过它就说明回溯已越出栈顶（如 main 之上），
    // 此时剩余数据不可信，停止回溯。读不到则不设上限（依赖无 FDE 断链）。
    let stack_top = read_u32(core, 0xE000_ED08)
        .and_then(|vtor| read_u32(core, u64::from(vtor)))
        .map(u64::from);

    let mut frames: Vec<Frame> = Vec::new();
    // 回溯停在 EXC_RETURN 时置位：说明链条顶端是"被异常机制调用的函数"，
    // 之后补上向量表里真正的中断入口（如 ADC0_1_IRQHandler）
    let mut stopped_at_exception = false;

    for _ in 0..max_frames {
        if pc == 0 {
            break;
        }
        frames.push(Frame { pc });

        // ---- 找当前 PC 的展开行 ----
        let fde =
            match debug_frame.fde_for_address(&bases, pc, gimli::read::DebugFrame::cie_from_offset)
            {
                Ok(fde) => fde,
                Err(_) => break, // 无展开信息（bootloader / 启动汇编 / 库代码）：断链
            };
        let row = match fde.unwind_info_for_address(&debug_frame, &bases, &mut ctx, pc) {
            Ok(row) => row,
            Err(_) => break,
        };

        // ---- CFA：调用者的 SP ----
        let CfaRule::RegisterAndOffset { register, offset } = row.cfa() else {
            break; // CFA 是表达式等复杂规则：暂不支持，断链
        };
        let offset: i64 = *offset;
        let base = *regs.get(&register.0).unwrap_or(&0);
        let cfa = base.wrapping_add_signed(offset);

        // 到达/越出初始栈顶 → 当前帧已是最外层（main），再往上只有启动代码/陈旧数据
        if stack_top.is_some_and(|top| cfa >= top) {
            break;
        }

        // ---- 返回地址：LR 寄存器（DWARF 14）的规则 ----
        // 注意：无显式规则时 DWARF 默认是 SameValue（沿用当前值）
        let ra = match row.register(gimli::Register(14)) {
            // Undefined = 不按正常路径返回（最外层 main / 中断服务函数入口）
            Some(RegisterRule::Undefined) => break,
            Some(RegisterRule::SameValue) | None => Some(lr),
            Some(RegisterRule::Offset(o)) => {
                read_u32(core, cfa.wrapping_add_signed(o)).map(u64::from)
            }
            Some(RegisterRule::Register(r)) => regs.get(&r.0).copied(),
            Some(_) => None, // Expression 等：暂不支持，断链
        };
        let Some(ra) = ra else { break };
        // 返回地址是 EXC_RETURN（0xFFFFFFxx）= 到达中断服务函数入口，
        // 其"调用者"是硬件异常机制，按需求停在这里（不追溯被打断的任务）
        if is_exc_return(ra) {
            stopped_at_exception = true;
            break;
        }
        if ra == 0 || ra & !1 == pc {
            break; // 防循环
        }

        // ---- 重建调用者寄存器（规则：Offset→读内存；Register→取被调用者寄存器；默认 SameValue→沿用）----
        let callee_regs = regs.clone();
        let mut next_regs: HashMap<u16, u64> = HashMap::new();
        for (num, val) in &callee_regs {
            let v = match row.register(gimli::Register(*num)) {
                Some(RegisterRule::Undefined) => None,
                Some(RegisterRule::SameValue) | None => Some(*val),
                Some(RegisterRule::Offset(o)) => {
                    read_u32(core, cfa.wrapping_add_signed(o)).map(u64::from)
                }
                Some(RegisterRule::Register(r)) => callee_regs.get(&r.0).copied(),
                Some(_) => None,
            };
            if let Some(v) = v {
                next_regs.insert(*num, v);
            }
        }

        pc = ra & !1;
        lr = ra;
        next_regs.insert(13, cfa);
        next_regs.insert(14, lr);
        next_regs.insert(15, pc | 1);
        regs = next_regs;
    }

    if frames.len() >= max_frames {
        // 达到上限：提示可能栈损坏或过深
    }
    // 链条停在中断边界时，补上向量表里的中断入口函数（ISR 的实际入口），
    // 例如 call_isr_d 之上是 ADC0_1_IRQHandler。若最后一帧本身就是该入口
    // （直接在入口函数里暂停），则不重复。
    if stopped_at_exception {
        if let Some(handler_addr) = active_vector_handler(core) {
            if frames.last().is_none_or(|f| f.pc != handler_addr) {
                frames.push(Frame { pc: handler_addr });
            }
        }
    }

    if frames.is_empty() {
        bail!("回溯失败：没有取到任何帧（PC 无展开信息？内核未暂停？）");
    }
    Ok(frames)
}
