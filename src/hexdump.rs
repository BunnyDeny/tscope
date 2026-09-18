//! hexdump 子命令：按字节转储目标内存（地址列 + 十六进制列 + ASCII 列）。
//!
//! 设计：
//! - 字节级寻址：任意地址起读，不要求对齐；内部仍按 32 位字**批量**读取
//!   再按字节切片，读取效率与字读一致（不会逐字节发 SWD 事务）；
//! - 宽度（每行字节数 4/8/16/32）与分组（每组字节数 1/2/4/8）可调，
//!   默认 16 字节每行、4 字节一组（正好是 32 位字的视觉）；
//! - ASCII 列：0x20..=0x7E 原样显示，其余为 '.'，不足一行的位置留空；
//! - --collapse：连续相同的行折叠成 '*'（大段 0xFF 擦除区不刷屏）。

use anyhow::{bail, Context, Result};
use probe_rs::{MemoryInterface, Session};

/// 单次转储上限（防手滑把整片 flash 打出来）
const MAX_DUMP_BYTES: u64 = 1024 * 1024;

/// 转储参数（address / length 已由 main.rs 完成字符串解析与校验）
pub struct DumpOptions {
    pub address: u64,
    pub length: u64,
    pub width: usize,
    pub group: usize,
    pub show_ascii: bool,
    pub collapse: bool,
}

pub fn run(session: &mut Session, opts: &DumpOptions) -> Result<()> {
    // ---- 参数校验 ----
    if opts.length == 0 {
        bail!("length 必须大于 0");
    }
    if opts.length > MAX_DUMP_BYTES {
        bail!("单次转储上限 {MAX_DUMP_BYTES} 字节（请求 {}），防手滑请分段读", opts.length);
    }
    if !matches!(opts.width, 4 | 8 | 16 | 32) {
        bail!("width 只支持 4 / 8 / 16 / 32 字节每行（当前 {}）", opts.width);
    }
    if !matches!(opts.group, 1 | 2 | 4 | 8) {
        bail!("group 只支持 1 / 2 / 4 / 8 字节每组（当前 {}）", opts.group);
    }
    if opts.width % opts.group != 0 {
        bail!("group（{}）必须整除 width（{}）", opts.group, opts.width);
    }

    // ---- 覆盖范围按字对齐，一次批量读取 ----
    let end = opts
        .address
        .checked_add(opts.length)
        .context("地址 + 长度溢出")?;
    let word_start = opts.address & !3;
    let word_end = (end + 3) & !3;
    let word_count = (word_end - word_start) / 4;

    let mut core = session.core(0)?;
    let mut words = vec![0u32; word_count as usize];
    core.read_32(word_start, &mut words)
        .with_context(|| {
            format!(
                "读取 0x{word_start:08x} 起 {} 字节失败（芯片未上电？地址无效？）",
                word_count * 4
            )
        })?;
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();

    // ---- 逐行输出 ----
    let mut prev_row: Option<&[u8]> = None;
    let mut collapsed = false;
    let mut row_addr = opts.address;
    let mut printed: u64 = 0;

    while printed < opts.length {
        let row_len = (opts.length - printed).min(opts.width as u64) as usize;
        let start = (row_addr - word_start) as usize;
        let row = &bytes[start..start + row_len];

        // 折叠：连续相同行只打印一个 '*'
        if opts.collapse {
            if let Some(prev) = prev_row {
                if prev == row {
                    if !collapsed {
                        println!("*");
                        collapsed = true;
                    }
                    printed += row_len as u64;
                    row_addr += row_len as u64;
                    prev_row = Some(row);
                    continue;
                }
            }
            collapsed = false;
            prev_row = Some(row);
        }

        // 十六进制列：组内字节一个空格，组间两个空格；不足一行占位对齐
        let mut hex = String::new();
        for i in 0..opts.width {
            if i > 0 {
                hex.push(' ');
                if i % opts.group == 0 {
                    hex.push(' ');
                }
            }
            if i < row_len {
                hex.push_str(&format!("{:02x}", row[i]));
            } else {
                hex.push_str("  ");
            }
        }

        print!("0x{row_addr:08x}  {hex}");
        if opts.show_ascii {
            // 行内：可打印字符原样显示，其余为 '.'；行外占位保持空白对齐
            let ascii: String = (0..opts.width)
                .map(|i| {
                    if i < row_len {
                        let b = row[i];
                        if (0x20..=0x7e).contains(&b) {
                            b as char
                        } else {
                            '.'
                        }
                    } else {
                        ' '
                    }
                })
                .collect();
            println!("  |{ascii}|");
        } else {
            println!();
        }

        printed += row_len as u64;
        row_addr += row_len as u64;
    }
    Ok(())
}
