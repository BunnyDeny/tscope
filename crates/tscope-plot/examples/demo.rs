//! 演示入口：用 WaveSource（数据源接口的一个实现）驱动 PlotApp。
//!
//! 两个图组：
//! - 图 1：两路正弦（相位差 90°）
//! - 图 2：两路锯齿（相位差 90°）
//!
//! 组内同图共 Y 轴（图例列出组内通道），两组上下叠放、共享 X 轴联动——
//! 这正是 tscope 未来 YAML 配置（"开几个图、每图哪些符号"）的形态。
//!
//! 键位：滚轮缩放 / 左拖平移 / 右键框选 / 双击复位并恢复滚动 /
//!       空格全局暂停 / +/− 窗口 / r 恢复滚动 / s 导出 CSV

use tscope_plot::{run_app, PlotApp, PlotOptions, WaveSource, Waveform};

fn main() -> eframe::Result {
    let channels = vec![
        ("sine 0°".to_string(), Waveform::Sine, 0.0),
        (
            "sine 90°".to_string(),
            Waveform::Sine,
            std::f64::consts::FRAC_PI_2,
        ),
        ("sawtooth 0°".to_string(), Waveform::Sawtooth, 0.0),
        (
            "sawtooth 90°".to_string(),
            Waveform::Sawtooth,
            std::f64::consts::FRAC_PI_2,
        ),
    ];
    let source = WaveSource::new(&channels, 1.0, 1.0, 200.0);
    let app = PlotApp::new(
        Box::new(source),
        PlotOptions {
            window_secs: 5.0,
            // 图 1 = 通道 0/1（两路正弦），图 2 = 通道 2/3（两路锯齿）
            groups: vec![vec![0, 1], vec![2, 3]],
            inner_size: [1000.0, 800.0],
            ..Default::default()
        },
    )
    .expect("数据源配置错误");

    run_app(
        app,
        "tscope-plot 演示（两图：正弦组 + 锯齿组，各相位差 90°）— 滚轮缩放/拖拽平移/右键框选，双击/r 恢复滚动，空格暂停，+/− 窗口，s 导出",
    )
}
