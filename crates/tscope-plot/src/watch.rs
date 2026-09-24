//! 变量监视窗口（Keil Watch 风格）：两列表格（变量名 + 格式化值），
//! 值变化时整行黄色高亮、约 0.3 秒渐退。
//!
//! 数据驱动：外部每节拍调用 [`WatchApp::set_values`] 送入各变量的
//! 格式化文本（真实场景由 debug 主循环采样、格式化后经子进程 stdin
//! 馈送；演示模式 [`WatchApp::new_demo`] 自产模拟数据）。
//! 与曲线窗口不同，watch 不需要"暂停"概念——采样停了，显示就停在
//! 最后一次值上，天然就是"冻结"。

use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, FontId, Label, Margin, RichText, ScrollArea};

/// 变化高亮持续时长（秒）
const HIGHLIGHT_SECS: f32 = 0.3;

/// 一行监视项：名称 + 格式化值（值可多行，如结构体展开）
pub struct WatchEntry {
    pub name: String,
    pub value: String,
}

/// 变量监视窗口应用
pub struct WatchApp {
    entries: Vec<WatchEntry>,
    /// 上次送入的值文本（变化检测；空串 = 尚未填充过，首填不高亮）
    prev: Vec<String>,
    /// 每行高亮剩余秒数（>0 时整行黄色渐退）
    highlight_left: Vec<f32>,
    /// 名称列宽度：首次绘制时按最长名称测量一次
    name_width: Option<f32>,
    /// 外部馈送通道（debug 会话经子进程 stdin 送入 (行号, 值文本)）
    inbox: Option<mpsc::Receiver<(usize, String)>>,
    /// 演示模式：自产模拟数据
    demo: bool,
    demo_next: Instant,
    started: Instant,
}

impl WatchApp {
    /// 构建窗口：`names` 为要监视的变量名（真实场景来自 watch 组配置）
    pub fn new(names: Vec<String>) -> Self {
        let n = names.len();
        Self {
            entries: names
                .into_iter()
                .map(|name| WatchEntry {
                    name,
                    value: String::new(),
                })
                .collect(),
            prev: vec![String::new(); n],
            highlight_left: vec![0.0; n],
            name_width: None,
            inbox: None,
            demo: false,
            demo_next: Instant::now(),
            started: Instant::now(),
        }
    }

    /// 演示模式：模拟一组典型监视变量（float/uint/结构体/数组/枚举），
    /// 每 0.5 秒刷新一次，部分变化、部分稳定——展示高亮效果
    pub fn new_demo() -> Self {
        let (names, values): (Vec<String>, Vec<String>) =
            demo_rows(0.0).into_iter().unzip();
        let mut app = Self::new(names);
        app.set_values(values); // 首填不高亮
        app.demo = true;
        app.demo_next = Instant::now() + Duration::from_millis(500);
        app
    }

    /// 启用外部馈送：值由外部每节拍经 `(行号, 值文本)` 送入
    /// （debug 会话内嵌 watch 用，每帧在 logic 里排空）
    pub fn set_inbox(&mut self, rx: mpsc::Receiver<(usize, String)>) {
        self.inbox = Some(rx);
    }

    /// 更新单行值。与上次文本不同即整行高亮（首填不高亮）
    pub fn set_value(&mut self, idx: usize, value: String) {
        if idx >= self.entries.len() {
            return;
        }
        if !self.prev[idx].is_empty() && self.prev[idx] != value {
            self.highlight_left[idx] = HIGHLIGHT_SECS;
        }
        self.prev[idx] = value.clone();
        self.entries[idx].value = value;
    }

    /// 送入一轮值（顺序对应构造时的 names）
    pub fn set_values(&mut self, values: Vec<String>) {
        for (i, v) in values.into_iter().enumerate() {
            self.set_value(i, v);
        }
    }

    /// 演示数据：t = 自启动秒数
    fn demo_tick(&mut self, t: f64) {
        let values: Vec<String> = demo_rows(t).into_iter().map(|(_, v)| v).collect();
        self.set_values(values);
    }
}

/// 演示用的（名称, 值文本）对
fn demo_rows(t: f64) -> Vec<(String, String)> {
    let pos = t.sin() * 3.0;
    let elec = (t * 0.9 + 0.4).sin() * 2.0;
    let rotations = (t / 4.0) as i32;
    vec![
        (
            "ENC_1_POS_SENSOR".into(),
            format!(
                "positionStruct = {{\n    position: {pos:.4} (float)\n    ElecPosition: {elec:.4} (float)\n    rotations: {rotations} (int)\n}}"
            ),
        ),
        (
            "ENC_1_POS_SENSOR.position".into(),
            format!("{pos:.4} (float)"),
        ),
        (
            "ENC_1_POS_SENSOR.rotations".into(),
            format!("{rotations} (int)"),
        ),
        ("theta_ref".into(), format!("{:.3} (float)", (t * 0.7).sin())),
        ("loop_cnt".into(), format!("{} (uint32_t)", (t * 20.0) as u32)),
        (
            "state_machine".into(),
            format!("{} (enum)", ["INIT", "RUN", "FAULT"][(t / 2.0) as usize % 3]),
        ),
        (
            "adc_buf[0..4]".into(),
            format!(
                "[{}, {}, {}, {}] (uint16_t[4])",
                1284 + (t.sin() * 30.0) as i32,
                1291 + (t.cos() * 20.0) as i32,
                1287,
                1303
            ),
        ),
        ("fault_flags".into(), "0x00000000 (uint32_t)".into()),
    ]
}

impl eframe::App for WatchApp {
    /// 每帧先于绘制调用：演示数据刷新 + 高亮渐退 + 持续重绘请求。
    /// 本库的 eframe 版本用 logic/ui 双阶段（update 已移除），
    /// logic 里不允许画任何东西。
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 演示模式：周期刷新模拟数据
        if self.demo && Instant::now() >= self.demo_next {
            self.demo_next += Duration::from_millis(500);
            self.demo_tick(self.started.elapsed().as_secs_f64());
        }
        // 外部馈送：排空所有待处理的行更新
        let msgs: Vec<(usize, String)> = match &self.inbox {
            Some(rx) => rx.try_iter().collect(),
            None => Vec::new(),
        };
        for (idx, text) in msgs {
            self.set_value(idx, text);
        }
        // 高亮渐退
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        for left in &mut self.highlight_left {
            *left = (*left - dt).max(0.0);
        }
        // 高亮渐退 / 演示刷新都需要持续重绘
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 顶部图例：变量数 + 高亮含义
        ui.horizontal(|ui| {
            ui.label(format!("{} 个变量", self.entries.len()));
            ui.separator();
            ui.colored_label(Color32::from_rgb(255, 200, 60), "■");
            ui.label("黄色高亮 = 值刚变化");
        });
        ui.separator();

        // 名称列宽度：首次绘制时按最长名称测量一次（之后固定）
        let name_width = *self.name_width.get_or_insert_with(|| {
            let font = FontId::monospace(14.0);
            let mut max = 0.0_f32;
            for e in &self.entries {
                let galley =
                    ui.painter()
                        .layout_no_wrap(e.name.clone(), font.clone(), Color32::WHITE);
                max = max.max(galley.size().x);
            }
            max + 16.0
        });

        // 表格主体：每行 名称 | 值（值可多行）
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for i in 0..self.entries.len() {
                    let alpha = self.highlight_left[i] / HIGHLIGHT_SECS;
                    let fill = if alpha > 0.0 {
                        Color32::from_rgba_unmultiplied(255, 225, 70, (alpha * 200.0) as u8)
                    } else {
                        Color32::TRANSPARENT
                    };
                    egui::Frame::new()
                        .inner_margin(Margin::symmetric(6, 3))
                        .fill(fill)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.add_sized(
                                    [name_width, 16.0],
                                    Label::new(
                                        RichText::new(&self.entries[i].name)
                                            .monospace()
                                            .strong(),
                                    ),
                                );
                                ui.separator();
                                ui.label(RichText::new(&self.entries[i].value).monospace());
                            });
                        });
                }
            });
    }
}

/// 启动 watch 窗口（阻塞，直到窗口关闭）
pub fn run_watch_app(app: WatchApp, initial_title: &str) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 480.0])
            .with_title(initial_title),
        ..Default::default()
    };
    eframe::run_native("tscope-watch", options, Box::new(|_cc| Ok(Box::new(app))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_only_on_change_and_not_on_first_fill() {
        let mut app = WatchApp::new(vec!["a".into(), "b".into()]);
        // 首填：都不高亮
        app.set_values(vec!["1".into(), "2".into()]);
        assert_eq!(app.highlight_left, [0.0, 0.0]);
        // 只有 b 变化 → 只有 b 高亮
        app.set_values(vec!["1".into(), "3".into()]);
        assert_eq!(app.highlight_left[0], 0.0);
        assert!(app.highlight_left[1] > 0.0);
        // 值没变 → 高亮渐退（此处只验证不重新点亮）
        app.highlight_left[1] = 0.0;
        app.set_values(vec!["1".into(), "3".into()]);
        assert_eq!(app.highlight_left, [0.0, 0.0]);
        // 多余的值忽略
        app.set_values(vec!["9".into(), "9".into(), "9".into()]);
        assert_eq!(app.entries.len(), 2);
    }

    #[test]
    fn demo_has_rows_and_updates() {
        let mut app = WatchApp::new_demo();
        assert!(app.entries.len() >= 4);
        let before = app.entries[3].value.clone();
        app.demo_tick(0.5);
        assert_ne!(app.entries[3].value, before, "演示数据应随时间变化");
    }
}
