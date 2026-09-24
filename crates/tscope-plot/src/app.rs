//! UI 层：PlotApp（eframe + egui_plot）。
//!
//! 只依赖 [`DataSource`](crate::source::DataSource) 接口，不关心数据来源。
//! 职责：
//! - 每帧从数据源拉取样本，入每通道环形缓冲（超出上限丢弃最旧数据）；
//! - 滚动窗口绘制（X 轴右缘 = 最新数据，J-Scope/VOFA+ 式）；
//! - 多通道按**图组**绘制：组内通道同图共 Y 轴（图例列出组内全部通道），
//!   多个图组上下叠放、共享 X 轴联动（缩放/平移任一图，全体 X 同步），
//!   各组 Y 独立自动缩放（量纲不同不互相压扁）；
//! - 鼠标手势（缩放/平移/框选/双击复位）与键盘（空格暂停、+/− 窗口、r 恢复滚动、s 导出）；
//! - 暂停期间不接收数据、曲线冻结；暂停时长从时间轴扣除（墙钟跳过 +
//!   样本整体平移），恢复瞬间丢弃暂停期残留样本并补 NaN 断点——
//!   曲线**接着暂停处继续**，不空缺口、不回退、不出现负时间。
//!
//! # 视图策略（关键设计）
//!
//! 跟随/缩放**从不手动 set_plot_bounds**，避免与 egui_plot 自身的交互状态
//! 竞争（否则缩放会被下一帧的强制边界弹回，表现为"缩放无效/重影抖动"）：
//! - 跟随模式：只把「窗口内」的数据喂给图，靠 auto bounds 自动把 X 卡在
//!   窗口上、Y 自动适配窗口数据；
//! - 用户缩放/平移：egui_plot 内部把对应轴切到手动模式（`auto_bounds()`
//!   返回 false），我们据此检测冻结；冻结帧把 Y 也钉住（`set_auto_bounds`
//!   (false,false)），并改喂全量历史——用户可平移回看旧数据，视图稳定；
//! - 双击：egui_plot 自己复位到自动模式，我们同步恢复滚动；
//! - r 键：`set_auto_bounds(true,true)` 强制复位（不用 Plot::reset，
//!   它会顺带清掉 linked-axes 分组，多通道时会闪一帧）。

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui_plot::{Legend, Line, Plot, PlotPoints};

use crate::source::{DataSource, Sample};

/// 曲线历史缓冲上限（采样点），超出从头部丢弃
const DEFAULT_MAX_HISTORY: usize = 20_000;
/// 滚动窗口默认宽度（秒）
const DEFAULT_WINDOW_SECS: f64 = 5.0;
/// 窗口宽度上下限与 +/− 步长（秒）
const MIN_WINDOW_SECS: f64 = 0.5;
const MAX_WINDOW_SECS: f64 = 120.0;
const WINDOW_STEP_SECS: f64 = 0.5;
/// 每个通道子图的高度范围（像素）
const MIN_PLOT_H: f32 = 140.0;
const MAX_PLOT_H: f32 = 340.0;
/// 曲线固定配色（Okabe-Ito 色盲友好 8 色，超出循环）
const PALETTE: [[u8; 3]; 8] = [
    [0x00, 0x72, 0xB2],
    [0xE6, 0x9F, 0x00],
    [0x00, 0x9E, 0x73],
    [0xCC, 0x79, 0xA7],
    [0x56, 0xB4, 0xE9],
    [0xD5, 0x5E, 0x00],
    [0xF0, 0xE4, 0x42],
    [0x00, 0x00, 0x00],
];

fn color_for(i: usize) -> egui::Color32 {
    let [r, g, b] = PALETTE[i % PALETTE.len()];
    egui::Color32::from_rgb(r, g, b)
}

/// 曲线窗口 UI 的可调参数
pub struct PlotOptions {
    /// 滚动窗口宽度（秒）；+/− 键运行时调整
    pub window_secs: f64,
    /// 每通道历史缓冲上限（采样点）
    pub max_history: usize,
    /// 窗口初始尺寸（逻辑像素）
    pub inner_size: [f32; 2],
    /// 图组划分：每个元素 = 一个子图，值为该图包含的通道索引（索引
    /// 对应数据源 `channel_names()` 的顺序）。组内多通道**同图共 Y 轴**
    /// （图例列出组内全部通道）；多个组上下叠放、**共享 X 轴联动**。
    /// 每个通道必须恰好属于一个组。空（默认）= 全部通道一个图。
    ///
    /// 对应 tscope 未来的 YAML 配置：每个组 = yaml 里的一组符号，
    /// tscope 侧把符号名映射成通道索引后填入本字段。
    pub groups: Vec<Vec<usize>>,
}

impl Default for PlotOptions {
    fn default() -> Self {
        Self {
            window_secs: DEFAULT_WINDOW_SECS,
            max_history: DEFAULT_MAX_HISTORY,
            inner_size: [1000.0, 600.0],
            groups: Vec::new(),
        }
    }
}

/// 一个通道的两份数据：窗口内数据（跟随滚动用）+ 全量历史（冻结回看用）
type ChannelData = (Vec<(f64, f64)>, Vec<(f64, f64)>);

/// 曲线窗口应用：把任意 [`DataSource`] 画成滚动曲线
pub struct PlotApp {
    source: Box<dyn DataSource>,
    /// 每通道的历史环形缓冲：（时刻秒, 数值）
    histories: Vec<VecDeque<(f64, f64)>>,
    window_secs: f64,
    max_history: usize,
    /// 图组：每组一个子图（组内通道同图共 Y，多组叠放共享 X）
    groups: Vec<Vec<usize>>,
    paused: bool,
    /// 是否自动跟随最新数据滚动（缩放/平移后置 false）
    follow: bool,
    /// r 键置位：本帧强制复位到自动模式并恢复滚动
    force_follow: bool,
    /// 恢复采样时先补 NaN 断点，曲线在暂停段断开
    pending_gap: bool,
    /// 状态消息（导出结果等）与时间
    status: Option<(String, Instant)>,
    last_title: String,
    inner_size: [f32; 2],
    /// UI 墙钟起点：滚动窗口跟墙钟走（60fps 连续滑动），
    /// 不跟数据时间走（否则低采样率下窗口按采样周期跳步，看起来卡）
    started: Instant,
    /// 暂停瞬间的**真实流逝**（started.elapsed()，f64）：暂停期间墙钟
    /// 冻结在 `paused_at − paused_total`。必须存真实流逝而不是墙钟值——
    /// 恢复时 `d = elapsed − paused_at` 才是本次暂停时长；若存墙钟值，
    /// 相减会把之前的累计暂停重复计算（第二次暂停起时间轴越走越负）
    paused_at: Option<f64>,
    /// 累计暂停时长（秒）：墙钟 = 真实流逝 − 累计暂停，
    /// 暂停时长从时间轴上扣除——恢复后曲线**接着暂停处继续**，
    /// 而不是空出暂停时长的缺口
    paused_total: f64,
    /// 样本时间轴平移量（秒，随每次恢复累积）：
    /// 数据源（debug 主循环/采样线程）的 t 在暂停期间照走，
    /// 每次恢复把后续样本整体平移，与扣除暂停的墙钟对齐
    t_offset: f64,
    /// 恢复瞬间丢弃下一批样本：数据源在暂停期间照发样本（其时钟不停），
    /// 这批"暂停期残留"时间在暂停区间内，平移后会落到暂停点之前
    /// （暂停比已运行时间长时甚至为负）——必须丢掉，不能进曲线
    drop_next_batch: bool,
    /// 外部暂停通道（debug 会话内嵌 plot 用）：暂停/恢复由外部消息驱动，
    /// 空格键失效——图像滚动与否只跟随内核运行状态
    external_pause: Option<mpsc::Receiver<bool>>,
}

impl PlotApp {
    /// 当前墙钟时刻（秒）= 真实流逝 − 累计暂停时长；暂停时冻结在暂停瞬间
    fn wall_now(&self) -> f64 {
        match self.paused_at {
            Some(v) => v - self.paused_total,
            None => self.started.elapsed().as_secs_f64() - self.paused_total,
        }
    }

    /// 统一的暂停切换：冻结/恢复墙钟 + 恢复时补 NaN 断点。
    /// 暂停时长从时间轴扣除（墙钟跳过 + 样本整体平移），
    /// 曲线恢复后接着暂停处继续显示。
    fn set_paused(&mut self, p: bool) {
        if p == self.paused {
            return;
        }
        self.paused = p;
        if p {
            // 记真实流逝：恢复时用它算本次暂停时长（见字段注释）
            self.paused_at = Some(self.started.elapsed().as_secs_f64());
        } else {
            // 恢复：本次暂停时长 = 真实流逝 − 暂停瞬间的真实流逝
            // （样本时间轴同样照走了这么久，等量平移回来）
            let d = self.started.elapsed().as_secs_f64() - self.paused_at.take().unwrap_or(0.0);
            self.paused_total += d;
            self.t_offset -= d;
            self.pending_gap = true;
            // 恢复瞬间数据源里积压的是暂停期间的旧样本（其时间在暂停
            // 区间内，平移后会落到暂停点之前甚至为负）——下一批整体丢弃
            self.drop_next_batch = true;
        }
    }

    /// 构建应用。数据源通道数必须 ≥1，图组划分必须合法，否则返回错误。
    pub fn new(source: Box<dyn DataSource>, options: PlotOptions) -> Result<Self, String> {
        let n = source.channel_names().len();
        if n == 0 {
            return Err("数据源没有通道（channel_names 为空）".to_string());
        }
        // 图组：空 = 全部通道一个图；否则校验划分
        let groups = if options.groups.is_empty() {
            vec![(0..n).collect()]
        } else {
            options.groups
        };
        let mut seen = vec![false; n];
        for g in &groups {
            if g.is_empty() {
                return Err("存在空的图组（每个图至少要有一个通道）".to_string());
            }
            for &i in g {
                if i >= n {
                    return Err(format!(
                        "图组引用了不存在的通道索引 {i}（数据源共 {n} 个通道）"
                    ));
                }
                if seen[i] {
                    return Err(format!("通道 {i} 被划分到了多个图组"));
                }
                seen[i] = true;
            }
        }
        if let Some(i) = seen.iter().position(|s| !s) {
            return Err(format!(
                "通道 {i}（{}）未被划分到任何图组",
                source.channel_names()[i]
            ));
        }

        Ok(Self {
            source,
            histories: vec![VecDeque::new(); n],
            window_secs: options.window_secs.clamp(MIN_WINDOW_SECS, MAX_WINDOW_SECS),
            max_history: options.max_history.max(100),
            groups,
            paused: false,
            follow: true,
            force_follow: false,
            pending_gap: false,
            status: None,
            last_title: String::new(),
            inner_size: options.inner_size,
            started: Instant::now(),
            paused_at: None,
            paused_total: 0.0,
            t_offset: 0.0,
            drop_next_batch: false,
            external_pause: None,
        })
    }

    /// 启用外部暂停模式：暂停/恢复由传入通道驱动，空格键失效。
    pub fn set_external_pause(&mut self, rx: mpsc::Receiver<bool>) {
        self.external_pause = Some(rx);
    }

    /// 追加一批样本到所有通道的历史（NaN 保持，作为断点）。
    /// 样本 t 先叠加 t_offset：暂停时长从时间轴扣除后，
    /// 样本与墙钟重新对齐（恢复后曲线接着暂停处继续）
    fn append(&mut self, batch: &[Sample]) {
        for s in batch {
            let t = s.t + self.t_offset;
            if self.pending_gap {
                for h in &mut self.histories {
                    h.push_back((t, f64::NAN));
                    if h.len() > self.max_history {
                        h.pop_front();
                    }
                }
                self.pending_gap = false;
            }
            for (i, h) in self.histories.iter_mut().enumerate() {
                h.push_back((t, s.values.get(i).copied().unwrap_or(f64::NAN)));
                if h.len() > self.max_history {
                    h.pop_front();
                }
            }
        }
    }

    /// 每帧拉取数据源并（按暂停/恢复规则）决定是否进入曲线：
    /// - 暂停中：照常排空（防通道积压），不进入曲线；
    /// - 恢复瞬间：丢弃本批（暂停期间的旧样本，见 [`Self::drop_next_batch`]）；
    /// - 其余：追加进历史。
    fn poll_and_append(&mut self) {
        let batch = self.source.poll();
        if !self.paused && !self.drop_next_batch {
            self.append(&batch);
        }
        self.drop_next_batch = false;
    }

    /// 键盘处理：空格暂停、+/− 窗口、r 恢复滚动、s 导出 CSV
    fn handle_keys(&mut self, ctx: &egui::Context) {
        ctx.input(|i| {
            if self.external_pause.is_none() && i.key_pressed(egui::Key::Space) {
                // 通知数据源暂停/恢复产出（硬件源可停止读芯片）
                self.source.set_paused(!self.paused);
                self.set_paused(!self.paused);
            }
            if i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals) {
                self.window_secs = (self.window_secs + WINDOW_STEP_SECS).min(MAX_WINDOW_SECS);
            }
            if i.key_pressed(egui::Key::Minus) {
                self.window_secs = (self.window_secs - WINDOW_STEP_SECS).max(MIN_WINDOW_SECS);
            }
            if i.key_pressed(egui::Key::R) {
                self.follow = true;
                self.force_follow = true;
            }
            if i.key_pressed(egui::Key::S) {
                let names = self.source.channel_names().to_vec();
                let msg =
                    match export_csv(Path::new("."), &names, &self.histories, self.window_secs) {
                        Ok(m) => m,
                        Err(e) => format!("导出失败：{e:#}"),
                    };
                self.status = Some((msg, Instant::now()));
            }
        });
    }

    /// 某通道的（窗口内数据, 全量历史）两份数据。
    /// 窗口按**墙钟**切：右缘跟墙钟连续滑动，与采样率无关（低采样率也丝滑）。
    fn channel_pairs(&self, i: usize, now: f64) -> ChannelData {
        let hist = &self.histories[i];
        let window = hist
            .iter()
            .copied()
            .filter(|p| p.0 >= now - self.window_secs)
            .collect();
        let full = hist.iter().copied().collect();
        (window, full)
    }

    /// 视图状态决策（模块文档里的核心策略，叠放/共享两种布局共用）：
    /// 返回（是否喂全量历史, 是否本帧冻结）。按需同步 egui_plot 的
    /// 自动/手动模式，全程不手动设边界（避免与它的交互状态竞争）。
    fn apply_view(
        &self,
        plot_ui: &mut egui_plot::PlotUi,
        following: bool,
        force_follow: bool,
    ) -> (bool, bool) {
        let auto = plot_ui.auto_bounds();
        if force_follow {
            // r 键：复位到自动模式并恢复滚动（本帧喂窗口数据，无视图跳变）
            plot_ui.set_auto_bounds(egui::Vec2b::new(true, true));
            (false, false)
        } else if following && !auto.x {
            // 上一帧用户缩放过（auto 被 egui_plot 切到手动）：
            // 钉住 Y（保持上次自动适配的值），视图交给用户，喂全量历史
            plot_ui.set_auto_bounds(egui::Vec2b::new(false, false));
            (true, true)
        } else if following {
            (false, false)
        } else {
            (true, false)
        }
    }

    /// 画一个图组（子图）：组内通道同图共 Y 轴、图例列出组内通道。
    /// 多组之间通过 `link_axis` 共享 X（缩放/平移/双击复位联动）。
    fn draw_group(
        &mut self,
        ui: &mut egui::Ui,
        gi: usize,
        height: f32,
        following: bool,
        force_follow: bool,
    ) {
        let bottom = gi == self.groups.len() - 1;
        let group = self.groups[gi].clone();
        let now = self.wall_now();
        // 组内各通道的两份数据（窗口按墙钟切）
        let datasets: Vec<ChannelData> =
            group.iter().map(|&i| self.channel_pairs(i, now)).collect();

        let mut plot = Plot::new(("tscope-group", gi))
            .legend(Legend::default())
            .height(height)
            .allow_drag(true)
            .allow_zoom(true)
            .allow_boxed_zoom(true)
            .allow_double_click_reset(true)
            .allow_scroll(true)
            // X 几乎无边距：最右侧贴住最新数据；Y 留 5% 呼吸空间
            .set_margin_fraction(egui::Vec2::new(0.005, 0.05))
            .link_axis(egui::Id::new("tscope-x"), egui::Vec2b::new(true, false))
            .link_cursor(egui::Id::new("tscope-x"), egui::Vec2b::new(true, false));
        if bottom {
            plot = plot.show_x(true).x_axis_label("t (s)");
        } else {
            plot = plot.show_x(false);
        }
        // 跟随模式：X 右缘 = 墙钟当前时刻（连续滑动；数据滞后时右缘仍平滑
        // 前移，采样点落在自己的时间位置上）；冻结后手动模式不受影响
        if following && datasets.iter().any(|(w, _)| !w.is_empty()) {
            plot = plot.include_x(now);
        }

        let mut froze = false;
        let resp = plot.show(ui, |plot_ui| {
            let (use_full, froze_now) = self.apply_view(plot_ui, following, force_follow);
            if froze_now {
                froze = true;
            }
            for (k, &ch) in group.iter().enumerate() {
                let (window_pairs, full_pairs) = &datasets[k];
                let pairs = if use_full { full_pairs } else { window_pairs };
                let name = self.source.channel_names()[ch].clone();
                for seg in line_segments(pairs, &name, color_for(ch)) {
                    plot_ui.line(seg);
                }
            }
        });

        if froze {
            self.follow = false;
        }
        if resp.response.double_clicked() {
            // 双击：egui_plot 自己复位到自动模式，这里同步恢复滚动
            self.follow = true;
        }
    }

    /// 标题栏：模式（滚动/冻结/暂停）+ 窗口宽度 + 状态消息
    fn update_title(&mut self, ctx: &egui::Context) {
        let external = self.external_pause.is_some();
        let mode = if self.paused {
            if external {
                "⏸ 内核已暂停（跟随调试器）"
            } else {
                "⏸ 已暂停（空格继续）"
            }
        } else if self.follow {
            if external {
                "▶ 内核运行中"
            } else {
                "▶ 滚动中"
            }
        } else {
            "⏸ 视图已冻结（双击/r 恢复滚动）"
        };
        let mut title = format!("tscope-plot — {mode} — 窗口 {:.1} s", self.window_secs);
        if let Some((msg, at)) = &self.status {
            if at.elapsed() < Duration::from_secs(4) {
                title.push_str(&format!(" — {msg}"));
            }
        }
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }
    }
}

impl eframe::App for PlotApp {
    /// 每帧先于绘制调用（窗口隐藏时也会调用）：拉数据、处理键盘、更新标题。
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 外部暂停模式：跟随内核运行状态（恢复时补 NaN 断点）
        let msgs: Vec<bool> = match &self.external_pause {
            Some(rx) => rx.try_iter().collect(),
            None => Vec::new(),
        };
        for p in msgs {
            self.set_paused(p);
        }
        // 拉取数据：暂停时排空、恢复瞬间丢弃残留、其余进曲线
        self.poll_and_append();
        self.handle_keys(ctx);
        self.update_title(ctx);
        // 曲线 60fps 连续重绘
        ctx.request_repaint_after(Duration::from_millis(16));
    }

    /// 绘制：每个图组一个子图，上下叠放、共享 X 轴联动。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 本帧统一使用同一个跟随状态（避免画到一半状态翻转导致各图组视图不一致）
        let following = self.follow;
        let force_follow = self.force_follow;
        let ng = self.groups.len();
        let height = (ui.available_height() / ng as f32).clamp(MIN_PLOT_H, MAX_PLOT_H);
        for gi in 0..ng {
            ui.push_id(gi, |ui| {
                self.draw_group(ui, gi, height, following, force_follow);
            });
        }
        // r 键的强制复位只作用一帧
        self.force_follow = false;
    }
}

/// 启动 eframe 主循环（阻塞，直到窗口关闭）
pub fn run_app(app: PlotApp, initial_title: &str) -> eframe::Result {
    let options = eframe::NativeOptions {
        // Linux：允许在非主线程创建事件循环——debug 会话里 plot 窗口
        // 异步运行（GUI 线程独立于 REPL 主循环）
        event_loop_builder: Some(Box::new(|builder| {
            #[cfg(target_os = "linux")]
            {
                // X11 与 Wayland 后端各自设置 any_thread（UFCS 避免同名方法歧义）
                winit::platform::x11::EventLoopBuilderExtX11::with_any_thread(builder, true);
                winit::platform::wayland::EventLoopBuilderExtWayland::with_any_thread(
                    builder, true,
                );
            }
        })),
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(app.inner_size)
            .with_title(initial_title),
        ..Default::default()
    };
    eframe::run_native("tscope-plot", options, Box::new(|_cc| Ok(Box::new(app))))
}

/// 一条曲线的线段集合：NaN（读取失败/暂停断点）处断开；
/// 只给首段命名（图例只显示一行，其余段共用颜色）
fn line_segments(pairs: &[(f64, f64)], name: &str, color: egui::Color32) -> Vec<Line<'static>> {
    let mut segs: Vec<Line<'static>> = Vec::new();
    let mut run: Vec<[f64; 2]> = Vec::new();
    for &(t, v) in pairs {
        if v.is_nan() {
            let first = segs.is_empty();
            flush_run(&mut segs, &mut run, name, color, first);
        } else {
            run.push([t, v]);
        }
    }
    let first = segs.is_empty();
    flush_run(&mut segs, &mut run, name, color, first);
    segs
}

fn flush_run(
    segs: &mut Vec<Line<'static>>,
    run: &mut Vec<[f64; 2]>,
    name: &str,
    color: egui::Color32,
    first: bool,
) {
    if run.is_empty() {
        return;
    }
    let points = std::mem::take(run);
    let line = if first {
        Line::new(name.to_string(), PlotPoints::from(points))
    } else {
        Line::new(String::new(), PlotPoints::from(points))
    }
    .color(color);
    segs.push(line);
}

/// 导出当前滚动窗口的曲线数据到 dir：CSV（一列时间 + 每通道一列）。
fn export_csv(
    dir: &Path,
    names: &[String],
    histories: &[VecDeque<(f64, f64)>],
    window_secs: f64,
) -> Result<String, String> {
    let len = histories.iter().map(|h| h.len()).max().unwrap_or(0);
    if len == 0 {
        return Err("还没有采样数据".to_string());
    }
    // 窗口起点：最近 window_secs 秒
    let t_last = histories[0].back().map(|p| p.0).unwrap_or(0.0);
    let start = histories[0]
        .iter()
        .position(|p| p.0 >= t_last - window_secs)
        .unwrap_or(0);
    let ts = timestamp_now();
    let path = dir.join(format!("tscope-plot-export-{ts}.csv"));
    let mut w = BufWriter::new(
        File::create(&path).map_err(|e| format!("创建 {} 失败：{e}", path.display()))?,
    );
    write!(w, "t").map_err(|e| e.to_string())?;
    for n in names {
        write!(w, ",{}", csv_field(n)).map_err(|e| e.to_string())?;
    }
    writeln!(w).map_err(|e| e.to_string())?;
    for i in start..len {
        let t = histories[0].get(i).map(|p| p.0).unwrap_or(0.0);
        write!(w, "{t:.6}").map_err(|e| e.to_string())?;
        for h in histories {
            match h.get(i) {
                Some((_, v)) if !v.is_nan() => write!(w, ",{v}").map_err(|e| e.to_string())?,
                _ => write!(w, ",").map_err(|e| e.to_string())?,
            }
        }
        writeln!(w).map_err(|e| e.to_string())?;
    }
    w.flush().map_err(|e| e.to_string())?;
    Ok(format!(
        "已导出 {}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
    ))
}

/// CSV 字段转义（含逗号/引号/换行时加引号）
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// 当前 UTC 时间戳（文件名用）：YYYYMMDD-HHMMSS
fn timestamp_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// 自 1970-01-01 起的天数 → (年, 月, 日)。Howard Hinnant 的 civil_from_days 算法。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist(pairs: &[(f64, f64)]) -> VecDeque<(f64, f64)> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn csv_field_escaping() {
        assert_eq!(csv_field("theta_ref"), "theta_ref");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
    }

    #[test]
    fn line_segments_break_at_nan() {
        let h = [(0.0, 1.0), (1.0, 2.0), (2.0, f64::NAN), (3.0, 4.0)];
        assert_eq!(line_segments(&h, "sym", egui::Color32::WHITE).len(), 2);
        let h2 = [(0.0, f64::NAN), (1.0, f64::NAN)];
        assert!(line_segments(&h2, "sym", egui::Color32::WHITE).is_empty());
    }

    #[test]
    fn export_csv_writes_window_aligned_columns() {
        let dir = std::env::temp_dir().join(format!(
            "tscope-plot-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let names = vec!["theta".to_string(), "cnt".to_string()];
        let histories = vec![
            hist(&[(0.0, 1.0), (1.0, 2.5), (2.0, 3.0), (3.0, f64::NAN)]),
            hist(&[(0.0, 10.0), (1.0, 20.0), (2.0, 30.0), (3.0, 40.0)]),
        ];
        // 窗口 2 秒：只导出 t >= 1 的两行
        let msg = export_csv(&dir, &names, &histories, 2.0).unwrap();
        assert!(msg.contains("已导出"), "msg: {msg}");

        let mut csv = String::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().and_then(|e| e.to_str()) == Some("csv") {
                csv = std::fs::read_to_string(&p).unwrap();
            }
        }
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 4, "csv: {csv}");
        assert_eq!(lines[0], "t,theta,cnt");
        assert_eq!(lines[1], "1.000000,2.5,20");
        assert_eq!(lines[3], "3.000000,,40");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod groups_tests {
    use super::*;
    use crate::source::ChannelSource;
    use std::sync::mpsc;

    fn app_with_groups(groups: Vec<Vec<usize>>) -> Result<PlotApp, String> {
        let (_tx, rx) = mpsc::channel::<crate::source::Sample>();
        let source = ChannelSource::new(rx, vec!["a".into(), "b".into(), "c".into(), "d".into()]);
        PlotApp::new(
            Box::new(source),
            PlotOptions {
                groups,
                ..Default::default()
            },
        )
    }

    #[test]
    fn groups_validation() {
        // 空 = 全部通道一个图
        assert!(app_with_groups(vec![]).is_ok());
        // 合法划分
        assert!(app_with_groups(vec![vec![0, 1], vec![2, 3]]).is_ok());
        // 索引越界
        assert!(app_with_groups(vec![vec![0], vec![4]]).is_err());
        // 通道重复划分
        assert!(app_with_groups(vec![vec![0, 1], vec![1, 2, 3]]).is_err());
        // 有通道未划分
        assert!(app_with_groups(vec![vec![0], vec![1, 2]]).is_err());
        // 空图组
        assert!(app_with_groups(vec![vec![], vec![0, 1, 2, 3]]).is_err());
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use crate::source::{ChannelSource, Sample};
    use std::sync::mpsc;

    /// 构造一个 1 通道的 PlotApp，手动灌历史数据
    fn app_with_history(pairs: &[(f64, f64)], window_secs: f64) -> PlotApp {
        let (_tx, rx) = mpsc::channel::<Sample>();
        let source = ChannelSource::new(rx, vec!["x".to_string()]);
        let mut app = PlotApp::new(
            Box::new(source),
            PlotOptions {
                window_secs,
                ..Default::default()
            },
        )
        .unwrap();
        app.append(&[Sample {
            t: 0.0,
            values: vec![f64::NAN],
        }]);
        app.histories[0].clear();
        for &(t, v) in pairs {
            app.histories[0].push_back((t, v));
        }
        app
    }

    #[test]
    fn channel_pairs_window_cutoff_uses_wall_clock() {
        let mut app = app_with_history(
            &[(0.0, 1.0), (1.0, 2.0), (2.0, 3.0), (3.0, 4.0), (4.0, 5.0)],
            2.0,
        );
        // 墙钟 now=4.5：窗口应只含 t >= 2.5 的点（与最后数据点无关）
        let (window, full) = app.channel_pairs(0, 4.5);
        assert_eq!(window.len(), 2, "window: {window:?}");
        assert_eq!(window[0], (3.0, 4.0));
        assert_eq!(full.len(), 5);
        // 暂停冻结墙钟：paused_at 存的是暂停瞬间的墙钟值，之后不再前进
        app.paused_at = Some(2.0);
        std::thread::sleep(Duration::from_millis(50));
        let frozen = app.wall_now();
        assert!(
            (frozen - 2.0).abs() < 1e-9,
            "暂停后墙钟仍在走：frozen={frozen}"
        );
    }

    /// 暂停期间数据源的时间照走：恢复时必须把暂停时长从时间轴扣除，
    /// 曲线才会"接着暂停处继续"而不是空出暂停时长的缺口
    #[test]
    fn pause_resume_excludes_pause_from_time_axis() {
        let (_tx, rx) = mpsc::channel::<Sample>();
        let source = ChannelSource::new(rx, vec!["x".to_string()]);
        let mut app = PlotApp::new(
            Box::new(source),
            PlotOptions {
                window_secs: 5.0,
                ..Default::default()
            },
        )
        .unwrap();
        // 运行期样本：生产者时钟 = 真实流逝（与墙钟同源）
        let t1 = app.started.elapsed().as_secs_f64();
        app.append(&[Sample {
            t: t1,
            values: vec![1.0],
        }]);
        // 暂停 50ms（期间生产者时钟照走，但不发样本）
        let w1 = app.wall_now();
        app.set_paused(true);
        std::thread::sleep(Duration::from_millis(50));
        app.set_paused(false);
        // 墙钟连续：恢复后接着暂停处，不跳 50ms
        let w2 = app.wall_now();
        assert!(
            (w2 - w1).abs() < 0.01,
            "恢复后墙钟应接着暂停处：w1={w1} w2={w2}"
        );
        // 恢复后的样本：t 含 50ms 暂停（生产者时钟照走），应被平移回来
        let t2 = app.started.elapsed().as_secs_f64();
        app.append(&[Sample {
            t: t2,
            values: vec![2.0],
        }]);
        let h = &app.histories[0];
        assert_eq!(h.len(), 3, "样本1 + NaN 断点 + 样本2：{h:?}");
        assert_eq!(h[0].1, 1.0);
        assert!(h[1].1.is_nan(), "恢复处应有 NaN 断点");
        assert_eq!(h[2].1, 2.0);
        let dt = h[2].0 - h[0].0;
        assert!(
            dt.abs() < 0.02,
            "暂停时长应从时间轴扣除（50ms 暂停不应出现在 X 轴）：dt={dt}"
        );
        // 样本时间与墙钟对齐：样本2 应落在当前墙钟附近
        assert!(
            (h[2].0 - app.wall_now()).abs() < 0.02,
            "样本时间应与扣除暂停的墙钟对齐"
        );
    }

    /// 用户场景回归：连按多次暂停（独立模式采样线程在暂停期间照发样本）。
    /// 恢复瞬间必须丢弃暂停期残留——否则残留样本被平移回暂停点之前，
    /// 暂停比已运行时间长时出现负时间、曲线回退重画
    #[test]
    fn rapid_pause_resume_never_goes_negative() {
        let (tx, rx) = mpsc::channel::<Sample>();
        let source = ChannelSource::new(rx, vec!["x".to_string()]);
        let mut app = PlotApp::new(
            Box::new(source),
            PlotOptions {
                window_secs: 5.0,
                ..Default::default()
            },
        )
        .unwrap();
        // 运行期样本（真实时钟 = started.elapsed，含全部暂停时长）
        app.append(&[Sample {
            t: app.started.elapsed().as_secs_f64(),
            values: vec![1.0],
        }]);
        for i in 0..5 {
            // 暂停 40ms：期间采样线程照发样本（残留，数值用大数标记）
            app.set_paused(true);
            std::thread::sleep(Duration::from_millis(40));
            tx.send(Sample {
                t: app.started.elapsed().as_secs_f64(),
                values: vec![100.0 + i as f64],
            })
            .unwrap();
            app.set_paused(false);
            // 恢复瞬间那一帧：残留样本必须被丢弃（真实逻辑路径）
            app.poll_and_append();
            // 恢复后的正常样本
            app.append(&[Sample {
                t: app.started.elapsed().as_secs_f64(),
                values: vec![2.0 + i as f64],
            }]);
        }
        let h = &app.histories[0];
        assert!(h.len() >= 2, "至少应有样本+断点：{h:?}");
        let mut last_t = -1.0f64;
        for &(t, v) in h.iter() {
            assert!(t >= 0.0, "出现负时间：t={t}（{h:?}）");
            assert!(t >= last_t, "时间回退：t={t}，前一个={last_t}（{h:?}）");
            if !v.is_nan() {
                assert!(
                    v < 50.0,
                    "暂停期残留样本混入曲线（值 {v} 是暂停期间发的）：{h:?}"
                );
            }
            last_t = t;
        }
    }
}
