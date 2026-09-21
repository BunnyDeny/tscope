//! hexdump 子命令：按字节转储目标内存（地址列 + 十六进制列 + ASCII 列）。
//!
//! 设计：
//! - 字节级寻址：任意地址起读，不要求对齐；内部仍按 32 位字**批量**读取
//!   再按字节切片，读取效率与字读一致（不会逐字节发 SWD 事务）；
//! - 宽度（每行字节数 4/8/16/32）与分组（每组字节数 1/2/4/8）可调，
//!   默认 16 字节每行、4 字节一组（正好是 32 位字的视觉）；
//! - ASCII 列：0x20..=0x7E 原样显示，其余为 '.'，不足一行的位置留空；
//! - --collapse：连续相同的行折叠成 '*'（大段 0xFF 擦除区不刷屏）；
//! - --watch：像 watch 一样持续刷新（交替屏），本帧相对上一帧变化的
//!   字节黄色高亮，q / Esc / Ctrl-C 退出，方向键/滚轮滚动。

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use probe_rs::{MemoryInterface, Session};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseEventKind};
use ratatui::crossterm::{execute, ExecutableCommand};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::DefaultTerminal;

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

/// 一次性转储
pub fn run(session: &mut Session, opts: &DumpOptions) -> Result<()> {
    validate(opts)?;
    let bytes = read_region(session, opts.address, opts.length)?;
    print_rows(&bytes, opts);
    Ok(())
}

/// 持续刷新（--watch）：交替屏里像 watch 一样周期性重读并重绘
pub fn run_watch(session: &mut Session, opts: &DumpOptions, interval_ms: u64) -> Result<()> {
    validate(opts)?;
    let interval = Duration::from_millis(interval_ms.max(10));

    let mut terminal = ratatui::try_init()
        .context("初始化终端失败（--watch 需要真实终端，不能运行在管道/重定向下）")?;
    let _ = std::io::stdout().execute(event::EnableMouseCapture);
    // 中途出错也恢复终端
    let res = watch_loop(&mut terminal, session, opts, interval);
    let _ = execute!(std::io::stdout(), event::DisableMouseCapture);
    ratatui::restore();
    res
}

/// --watch 主循环：采样 → 渲染（变化字节高亮）→ 处理滚动/退出事件
fn watch_loop(
    terminal: &mut DefaultTerminal,
    session: &mut Session,
    opts: &DumpOptions,
    interval: Duration,
) -> Result<()> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut prev: Option<Vec<u8>> = None;
    let mut last_error: Option<String> = None;
    let mut scroll: usize = 0;
    let mut visible_rows: usize = 10;
    let mut next_sample = Instant::now();
    let started = Instant::now();

    loop {
        // —— 到点采样 ——
        if Instant::now() >= next_sample {
            match read_region(session, opts.address, opts.length) {
                Ok(b) => {
                    bytes = b;
                    last_error = None;
                }
                Err(e) => {
                    bytes.clear();
                    last_error = Some(format!("{e:#}"));
                }
            }
            next_sample = Instant::now() + interval;
        }

        // —— 渲染：每行 = 地址 + 字节 Span（变化高亮）+ ASCII 列 ——
        terminal
            .draw(|f| {
                visible_rows = f.area().height.saturating_sub(2) as usize;
                let mut lines: Vec<Line> = Vec::new();
                if let Some(err) = &last_error {
                    lines.push(Line::styled(err.clone(), Style::default().fg(Color::Red)));
                } else {
                    let width = opts.width;
                    let group = opts.group;
                    let mut idx = 0usize;
                    while idx < bytes.len() {
                        let row_len = (bytes.len() - idx).min(width);
                        let mut spans: Vec<Span> = vec![Span::raw(format!(
                            "0x{:08x}  ",
                            opts.address + idx as u64
                        ))];

                        // 十六进制列
                        for i in 0..width {
                            if i > 0 {
                                spans.push(Span::raw(" "));
                                if i % group == 0 {
                                    spans.push(Span::raw(" "));
                                }
                            }
                            if i < row_len {
                                let b = bytes[idx + i];
                                let changed = prev
                                    .as_ref()
                                    .is_some_and(|p| p.get(idx + i) != Some(&b));
                                let style = if changed {
                                    Style::default()
                                        .fg(Color::Yellow)
                                        .add_modifier(Modifier::BOLD)
                                } else {
                                    Style::default()
                                };
                                spans.push(Span::styled(format!("{b:02x}"), style));
                            } else {
                                spans.push(Span::raw("  "));
                            }
                        }

                        // ASCII 列
                        if opts.show_ascii {
                            spans.push(Span::raw("  |"));
                            for i in 0..width {
                                if i < row_len {
                                    let b = bytes[idx + i];
                                    let ch = if (0x20..=0x7e).contains(&b) {
                                        b as char
                                    } else {
                                        '.'
                                    };
                                    let changed = prev
                                        .as_ref()
                                        .is_some_and(|p| p.get(idx + i) != Some(&b));
                                    let style = if changed {
                                        Style::default()
                                            .fg(Color::Yellow)
                                            .add_modifier(Modifier::BOLD)
                                    } else {
                                        Style::default()
                                    };
                                    spans.push(Span::styled(ch.to_string(), style));
                                } else {
                                    spans.push(Span::raw(" "));
                                }
                            }
                            spans.push(Span::raw("|"));
                        }

                        lines.push(Line::from(spans));
                        idx += row_len;
                    }
                }

                // 滚动钳制（行数超出可视区）
                let max_scroll = lines.len().saturating_sub(visible_rows);
                scroll = scroll.min(max_scroll);

                let para = Paragraph::new(lines)
                    .block(
                        Block::default().borders(Borders::ALL).title(format!(
                            " tscope hexdump --watch [0x{:08x} +{} 字节] — 每 {} ms 采样，已运行 {} s — \
                             变化字节黄色高亮，↑↓/PgUp/PgDn/滚轮 滚动，q/Esc 退出 ",
                            opts.address,
                            opts.length,
                            interval.as_millis(),
                            started.elapsed().as_secs()
                        )),
                    )
                    .scroll((scroll as u16, 0));
                f.render_widget(para, f.area());
            })
            .context("渲染失败")?;

        // 上一帧数据就位后更新基线（高亮 = 与上一帧比较）
        prev = Some(bytes.clone());

        // —— 事件：q / Esc / Ctrl-C 退出；方向键/滚轮滚动 ——
        if event::poll(Duration::from_millis(20)).context("读取终端事件失败")? {
            loop {
                let ev = event::read().context("读取事件失败")?;
                match ev {
                    Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('c') => return Ok(()),
                        KeyCode::Up | KeyCode::Char('k') => {
                            scroll = scroll.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            scroll = scroll.saturating_add(1);
                        }
                        KeyCode::PageUp => {
                            scroll = scroll.saturating_sub(visible_rows.max(1));
                        }
                        KeyCode::PageDown => {
                            scroll = scroll.saturating_add(visible_rows.max(1));
                        }
                        KeyCode::Home => {
                            scroll = 0;
                        }
                        KeyCode::End => {
                            scroll = usize::MAX / 2; // 渲染时钳制到最大值
                        }
                        _ => {}
                    },
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollDown => {
                            scroll = scroll.saturating_add(3);
                        }
                        MouseEventKind::ScrollUp => {
                            scroll = scroll.saturating_sub(3);
                        }
                        _ => {}
                    },
                    _ => {}
                }
                // 没有更多待处理事件就回到采样/渲染循环
                if !event::poll(Duration::ZERO).context("读取终端事件失败")? {
                    break;
                }
            }
        }
    }
}

// ===========================================================================
// 内部实现
// ===========================================================================

fn validate(opts: &DumpOptions) -> Result<()> {
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
    Ok(())
}

/// 按字对齐批量读取，再按字节切片，返回恰好 length 字节
fn read_region(session: &mut Session, address: u64, length: u64) -> Result<Vec<u8>> {
    let end = address.checked_add(length).context("地址 + 长度溢出")?;
    let word_start = address & !3;
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
    let all: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let skip = (address - word_start) as usize;
    Ok(all[skip..skip + length as usize].to_vec())
}

/// 一次性打印（collapse 折叠、ASCII 列、部分行占位）
fn print_rows(bytes: &[u8], opts: &DumpOptions) {
    let mut prev_row: Option<&[u8]> = None;
    let mut collapsed = false;
    let mut row_addr = opts.address;
    let mut printed: u64 = 0;

    while printed < bytes.len() as u64 {
        let row_len = (bytes.len() as u64 - printed).min(opts.width as u64) as usize;
        let start = printed as usize;
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
}
