//! watch 子命令：像 Keil Watch 窗口一样实时刷新变量值。
//!
//! 实现：ratatui 交替屏表格。
//! - 进入时切换到交替屏幕（alternate screen），退出时自动恢复终端，
//!   不在用户终端里留任何滚动垃圾；
//! - 复合类型（数组/结构体/联合体）**展开成子项行**（`buf[0]`、
//!   `s.member`、`s.v.x`…），但最多显示 max_elems 项（默认 8），
//!   超出显示「…余N」汇总行——修正 Keil 全量刷屏的缺陷；
//! - 每个基础变量每采样周期只做**一次**内存读（子项行共享同一份
//!   缓冲区），采样期间目标 CPU 不暂停。

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseEventKind};
use ratatui::crossterm::{execute, ExecutableCommand};
use ratatui::layout::Constraint;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Cell, Row, Table, TableState};
use ratatui::DefaultTerminal;

use crate::config::{ToolConfig, WatchGroup};
use crate::session;
use crate::symbol::{prepare_symbol, PreparedSymbol, TypeDesc};

/// 一行监视项
struct WatchRow {
    /// 表格第一列显示的文字（普通行 = 表达式，数组元素行 = `expr[i]`）
    label: String,
    prepared: Result<PreparedSymbol, String>,
    /// 数组元素行：相对字段的偏移 + 元素类型；普通行 None
    elem: Option<(usize, TypeDesc)>,
    /// 「…余N」汇总行：值为 None
    summary: Option<String>,
    last_value: String,
    last_error: Option<String>,
    /// 本采样周期值是否发生了变化（红色高亮，Keil 风格）
    changed: bool,
}

/// 列出配置里定义的所有监视组（`tscope watch` 不带参数时）
pub fn list_groups(config: &ToolConfig) -> Result<()> {
    if config.watch.is_empty() {
        println!("（配置里没有定义 watch 节）");
        return Ok(());
    }
    println!("配置里定义的监视组：");
    for (name, g) in &config.watch {
        println!(
            "  {name:<16} 每 {} ms 采样，{} 个符号，复合类型最多展开 {} 项",
            g.interval_ms,
            g.symbols.len(),
            g.max_elems
        );
    }
    println!("\n用法：tscope watch <组名>");
    Ok(())
}

/// 启动 watch 界面。q / Esc / Ctrl-C 退出。
pub fn run(config: &ToolConfig, group: &str) -> Result<()> {
    let mut session = session::open_session(&config.probe, &config.chip)?;
    run_with_session(config, group, &mut session)
}

/// 复用已有调试会话进入 watch 界面（debug 会话里调用，避免重复占用探针）
pub fn run_with_session(
    config: &ToolConfig,
    group: &str,
    session: &mut probe_rs::Session,
) -> Result<()> {
    let g = config.watch.get(group).ok_or_else(|| {
        let names: Vec<String> = config.watch.keys().cloned().collect();
        if names.is_empty() {
            anyhow!(
                "配置里没有定义 watch 节（在 tscope.yaml 里加 watch 段，格式见模板 tscope.yaml）"
            )
        } else {
            anyhow!(
                "配置里没有名为 {group} 的监视组；可用组：{}",
                names.join(", ")
            )
        }
    })?;
    let elf = config.firmware_image()?;
    run_group(session, elf, &format!("watch [{group}]"), g)
}

/// 临时监视（var --watch 用）：不依赖配置文件里的监视组，就地组装一组
pub fn run_adhoc(
    session: &mut probe_rs::Session,
    elf: &std::path::Path,
    title: &str,
    symbols: Vec<String>,
    interval_ms: u64,
    max_elems: usize,
) -> Result<()> {
    let g = WatchGroup {
        interval_ms,
        max_elems: max_elems.max(1),
        symbols,
    };
    run_group(session, elf, title, &g)
}

/// 启动一组监视的 TUI（行构建 + 交替屏循环）
fn run_group(
    session: &mut probe_rs::Session,
    elf: &std::path::Path,
    title: &str,
    g: &WatchGroup,
) -> Result<()> {
    // 解析阶段（一次性）：单个符号解析失败只影响那一行，不中断整体。
    // 数组符号在此展开成元素行（封顶 max_elems）。
    let mut rows: Vec<WatchRow> = Vec::new();
    for expr in &g.symbols {
        match prepare_symbol(elf, expr) {
            Err(e) => rows.push(WatchRow {
                label: expr.clone(),
                prepared: Err(format!("{e:#}")),
                elem: None,
                summary: None,
                last_value: String::new(),
                last_error: None,
                changed: false,
            }),
            Ok(p) => match p.child_rows(g.max_elems) {
                Some((rows_elems, remaining)) => {
                    for (lab, off, ty) in rows_elems {
                        rows.push(WatchRow {
                            label: format!("{expr}{lab}"),
                            prepared: Ok(p.clone()),
                            elem: Some((off, ty)),
                            summary: None,
                            last_value: String::new(),
                            last_error: None,
                            changed: false,
                        });
                    }
                    if remaining > 0 {
                        rows.push(WatchRow {
                            label: expr.clone(),
                            prepared: Ok(p),
                            elem: None,
                            summary: Some(format!(
                                "… 其余 {remaining} 项未显示（调大 max_elems 或直接监视具体成员/下标）"
                            )),
                            last_value: String::new(),
                            last_error: None,
                            changed: false,
                        });
                    }
                }
                None => rows.push(WatchRow {
                    label: expr.clone(),
                    prepared: Ok(p),
                    elem: None,
                    summary: None,
                    last_value: String::new(),
                    last_error: None,
                    changed: false,
                }),
            },
        }
    }

    // 会话复用：调用方传入的 session（采样期间持续使用）

    // TUI 生命周期：进交替屏，restore 恢复终端（即使中途出错也恢复）。
    // 用 try_init 而不是 init：无 TTY（管道/重定向）时返回错误而非 panic，
    // 保证 debug 会话里单条命令失败不炸掉整个会话。
    let mut terminal = ratatui::try_init()
        .context("初始化终端失败（监视界面需要真实终端，不能运行在管道/重定向下）")?;
    // 开启鼠标捕获（滚轮滚动表格）
    let _ = std::io::stdout().execute(event::EnableMouseCapture);
    let mut state = TableState::default();
    if !rows.is_empty() {
        state.select(Some(0));
    }
    let res = run_loop(
        &mut terminal,
        session,
        &mut rows,
        title,
        g.interval_ms,
        &mut state,
    );
    let _ = execute!(std::io::stdout(), event::DisableMouseCapture);
    ratatui::restore();
    res
}

/// 保持选中行在可视窗口内（offset 是表格渲染的起始行）
fn keep_visible(state: &mut TableState, visible: usize, total: usize) {
    let Some(i) = state.selected() else { return };
    if total == 0 {
        return;
    }
    let i = i.min(total.saturating_sub(1));
    let off = *state.offset_mut();
    if i < off {
        *state.offset_mut() = i;
    } else if visible > 0 && i >= off.saturating_add(visible) {
        *state.offset_mut() = i - visible + 1;
    }
}

fn run_loop(
    terminal: &mut DefaultTerminal,
    session: &mut probe_rs::Session,
    rows: &mut [WatchRow],
    title: &str,
    interval_ms: u64,
    state: &mut TableState,
) -> Result<()> {
    let interval = Duration::from_millis(interval_ms.max(10));
    let mut next_sample = Instant::now();
    let started = Instant::now();
    // 上一帧表格能显示的行数（表头+边框占 3 行），用于翻页与保持可见
    let mut visible_rows: usize = 10;

    loop {
        // —— 到点采样 ——
        // 同一个基础变量（如整块数组）只读一次，其元素行共享缓冲区
        if Instant::now() >= next_sample {
            let mut cache_key: Option<(u64, usize)> = None;
            let mut cache_buf: Option<Vec<u8>> = None;

            for row in rows.iter_mut() {
                if row.prepared.is_err() || row.summary.is_some() {
                    continue;
                }
                let p = row.prepared.as_ref().unwrap();

                let key = p.base_key();
                if cache_key != Some(key) {
                    let mut core = session.core(0)?;
                    match p.read_base(&mut core) {
                        Ok(buf) => {
                            cache_key = Some(key);
                            cache_buf = Some(buf);
                        }
                        Err(e) => {
                            cache_key = Some(key);
                            cache_buf = None;
                            row.last_error = Some(format!("{e:#}"));
                            continue;
                        }
                    }
                }
                let Some(buf) = cache_buf.as_ref() else {
                    row.last_error = Some("读取失败".to_string());
                    continue;
                };

                let value = match &row.elem {
                    Some((off, ty)) => p.render_at(buf, *off, ty),
                    None => p.render_self(buf),
                };
                row.changed = !row.last_value.is_empty() && value != row.last_value;
                row.last_value = value;
                row.last_error = None;
            }
            next_sample = Instant::now() + interval;
        }

        // —— 渲染 ——
        terminal.draw(|f| {
            let header = Row::new(
                ["表达式", "值", "类型", "地址"]
                    .iter()
                    .map(|h| Cell::from(Line::styled(*h, Style::default().add_modifier(Modifier::BOLD)))),
            );

            let body: Vec<Row> = rows
                .iter()
                .map(|row| {
                    let (type_text, addr_text, value_text) = match &row.prepared {
                        Ok(p) => {
                            let addr = match &row.elem {
                                Some((off, _)) => p.field_address() + *off as u64,
                                None => p.field_address(),
                            };
                            (
                                match &row.elem {
                                    Some((_, ty)) => ty.name().to_string(),
                                    None => p.type_name().to_string(),
                                },
                                format!("0x{addr:08x}"),
                                if let Some(s) = &row.summary {
                                    s.clone()
                                } else if row.last_error.is_some() {
                                    row.last_error.clone().unwrap()
                                } else {
                                    row.last_value.clone()
                                },
                            )
                        }
                        Err(e) => ("—".to_string(), "—".to_string(), e.clone()),
                    };

                    // Keil 风格：出错红色、值变化黄色高亮、正常默认色
                    let value_style = if row.last_error.is_some() || row.prepared.is_err() {
                        Style::default().fg(Color::Red)
                    } else if row.changed {
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                    } else if row.summary.is_some() {
                        Style::default().fg(Color::DarkGray)
                    } else {
                        Style::default()
                    };

                    Row::new(vec![
                        Cell::from(Line::raw(row.label.clone())),
                        Cell::from(Line::styled(value_text, value_style)),
                        Cell::from(Line::raw(type_text)),
                        Cell::from(Line::raw(addr_text)),
                    ])
                })
                .collect();

            let table = Table::new(
                body,
                [
                    Constraint::Percentage(32),
                    Constraint::Percentage(40),
                    Constraint::Percentage(16),
                    Constraint::Percentage(12),
                ],
            )
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(
                        " tscope {title} — 每 {} ms 采样，已运行 {} s — ↑↓/PgUp/PgDn/滚轮 滚动，q/Esc 退出 ",
                        interval_ms,
                        started.elapsed().as_secs()
                    )),
            )
            .column_spacing(2)
            .row_highlight_style(Style::default().bg(Color::DarkGray));

            // 记录本帧表格可显示的行数（表头 1 行 + 上下边框 2 行）
            visible_rows = f.area().height.saturating_sub(3) as usize;
            f.render_stateful_widget(table, f.area(), state);
        })
        .context("渲染失败")?;

        // —— 事件：q / Esc / Ctrl-C 退出；方向键/滚轮滚动 ——
        // 每轮排空所有待处理事件：快速连续按键/滚轮也不会丢步
        if event::poll(Duration::from_millis(20)).context("读取终端事件失败")? {
            loop {
                let quit = match event::read().context("读取事件失败")? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('c') => true,
                        KeyCode::Up | KeyCode::Char('k') => {
                            state.select_previous();
                            keep_visible(state, visible_rows, rows.len());
                            false
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            state.select_next();
                            keep_visible(state, visible_rows, rows.len());
                            false
                        }
                        KeyCode::PageUp => {
                            state.scroll_up_by(visible_rows.max(1) as u16);
                            keep_visible(state, visible_rows, rows.len());
                            false
                        }
                        KeyCode::PageDown => {
                            state.scroll_down_by(visible_rows.max(1) as u16);
                            keep_visible(state, visible_rows, rows.len());
                            false
                        }
                        KeyCode::Home => {
                            state.select(Some(0));
                            keep_visible(state, visible_rows, rows.len());
                            false
                        }
                        KeyCode::End => {
                            state.select(Some(rows.len().saturating_sub(1)));
                            keep_visible(state, visible_rows, rows.len());
                            false
                        }
                        _ => false,
                    },
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollDown => {
                            state.scroll_down_by(3);
                            keep_visible(state, visible_rows, rows.len());
                            false
                        }
                        MouseEventKind::ScrollUp => {
                            state.scroll_up_by(3);
                            keep_visible(state, visible_rows, rows.len());
                            false
                        }
                        _ => false,
                    },
                    _ => false,
                };
                if quit {
                    return Ok(());
                }
                // 选中行越界保护
                if let Some(i) = state.selected() {
                    if rows.is_empty() {
                        state.select(None);
                    } else if i >= rows.len() {
                        state.select(Some(rows.len() - 1));
                    }
                }
                // 没有更多待处理事件就回到采样/渲染循环
                if !event::poll(Duration::ZERO).context("读取终端事件失败")? {
                    break;
                }
            }
        }
    }
}
