//! tscope-plot：tscope 的 GUI 库——曲线窗口（plot）与变量监视窗口（watch）。
//!
//! 两个窗口共用同一套 eframe 依赖与"数据驱动"设计：
//! - 曲线窗口：`PlotApp` 消费「通道名 + (时刻, 数值) 流」（见下）；
//! - 监视窗口：`WatchApp` 消费「变量名 + 格式化值文本」逐拍快照，
//!   值变化整行黄色高亮（Keil Watch 风格，见 [`watch`]）。
//!
//! # 架构
//!
//! ```text
//! 数据源（DataSource）── poll() 拉取 Sample 流 ──▶ PlotApp（eframe UI）
//!                                                    │
//!     本地生成：WaveSource（正弦/锯齿，多通道）      ├─ 60fps 滚动绘制（J-Scope/VOFA+ 式，
//!     硬件采样：采样线程 + mpsc + ChannelSource      │    X 轴右缘 = 最新数据）
//!                                                    ├─ 图组：N 个子图上下叠放、共享 X 联动，
//!                                                    │   组内多通道同图共 Y（图例列组内通道）
//!                                                    ├─ 全局暂停（空格）、窗口宽度（+/−）、
//!                                                    │   导出 CSV（s）
//!                                                    └─ XY 缩放（滚轮/拖拽/框选/双击，内置）
//! ```
//!
//! # 与 tscope 的集成路径
//!
//! 关键接口是 [`DataSource`]：UI 只认「通道名 + (时刻, 数值) 流」。
//! 将来接真实符号数据只需实现一个 `SymbolSource`（内部起采样线程独占
//! probe-rs Session，读标量符号数值后经 mpsc 送出），再包进
//! [`ChannelSource`]（本库已提供）即可，UI 层零改动。
//!
//! 对应 tscope 未来的 YAML 配置（类似 watch 组："开几个图、每图哪些符号"）：
//! 每个 yaml 组映射为一个**图组**——把组内符号名按数据源通道顺序映射成
//! 索引，填入 [`PlotOptions::groups`]；组内符号同图共 Y 轴，多组上下叠放
//! 共享 X 轴联动。
//!
//! # 交互一览（PlotApp 内建）
//!
//! - 鼠标：滚轮 = 缩放（X/Y 同时、光标锚定）、左键拖拽 = 平移、右键框选 =
//!   X/Y 同时缩放、触控板横向滚动 = 平移、双击 = 复位并恢复滚动；
//!   悬停 = 十字准星读数；多图之间 X 轴联动（任一图操作全体同步），
//!   Y 轴各自独立
//! - 键盘：空格 = 全局暂停/继续（暂停后不接收数据、所有曲线冻结，
//!   恢复时曲线断开补 NaN）、+/− = 调窗口宽度（秒）、r = 恢复滚动、
//!   s = 导出 CSV（当前目录）
//! - 缩放/平移后自动停止滚动（视图冻结），标题栏实时显示当前模式

pub mod app;
pub mod source;
pub mod watch;

pub use app::{run_app, PlotApp, PlotOptions};
pub use source::{ChannelSource, DataSource, Sample, WaveSource, Waveform};
pub use watch::{run_watch_app, WatchApp, WatchEntry};
