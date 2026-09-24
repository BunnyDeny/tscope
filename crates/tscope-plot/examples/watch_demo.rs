//! watch 窗口演示：`cargo run -p tscope-plot --example watch_demo`
//!
//! 展示 Keil Watch 风格的变量监视 GUI：变量名 + 格式化值（结构体多行
//! 展开），值变化时整行黄色高亮、约 0.3 秒渐退；部分变量稳定（不闪）。
//! 数据是模拟的（正弦/计数/结构体/数组/枚举），真实场景下这些文本由
//! debug 主循环采样格式化后馈送，窗口外观完全一致。

use tscope_plot::{run_watch_app, WatchApp};

fn main() -> eframe::Result {
    let app = WatchApp::new_demo();
    run_watch_app(app, "tscope watch [demo]")
}
