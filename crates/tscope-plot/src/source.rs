//! 数据源：曲线数据的总接口。
//!
//! UI 层只依赖 [`DataSource`] 这一条边界——通道名 + (时刻, 数值) 样本流。
//! 数据从哪来（本地生成 / 硬件采样 / 网络）与 UI 完全无关。
//!
//! 本文件提供：
//! - [`Sample`]：一次采样（时刻 + 每通道一个数值，NaN = 无数据/读取失败）；
//! - [`DataSource`]：trait，UI 每帧 `poll()` 拉走累积样本；
//! - [`WaveSource`]：本地波形（正弦/锯齿，多通道、每通道相位独立；
//!   演示/测试用，验证接口可用）；
//! - [`ChannelSource`]：mpsc 通道包装器——未来 probe-rs 采样线程就接在这里。

use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 一次采样：时刻（秒，自采样开始）+ 每个通道一个数值。
/// NaN 表示该通道本周期无有效数据（读取失败/暂停断点），曲线在该处断开。
#[derive(Debug, Clone)]
pub struct Sample {
    pub t: f64,
    pub values: Vec<f64>,
}

/// 数据源接口：UI 每帧调用 [`DataSource::poll`] 取走全部累积样本。
/// 实现约定：
/// - `channel_names()` 返回的通道名数量必须与每次 `Sample.values` 长度一致；
/// - `poll()` 的样本按时间单调递增；
/// - `set_paused()` 是暂停钩子：UI 暂停时通知源停止产出
///   （硬件源借此停止读芯片；本地生成源可直接返回空）。
pub trait DataSource {
    /// 通道名称（图例与 CSV 列名用）
    fn channel_names(&self) -> &[String];

    /// 取走当前积累的所有样本；没有新数据返回空 Vec
    fn poll(&mut self) -> Vec<Sample>;

    /// 暂停/恢复钩子（默认空实现）
    fn set_paused(&mut self, _paused: bool) {}
}

/// mpsc 通道数据源：把「采样线程 → 通道」包装成 DataSource。
///
/// 未来的 probe-rs 集成路径：采样线程独占 Session（Session 非 Sync），
/// 按周期读标量符号数值后 `send(Sample { .. })`，UI 关窗后 send 失败、
/// 线程自然退出。UI 侧只需要 `ChannelSource::new(rx, 符号名)`。
pub struct ChannelSource {
    rx: mpsc::Receiver<Sample>,
    names: Vec<String>,
}

impl ChannelSource {
    pub fn new(rx: mpsc::Receiver<Sample>, names: Vec<String>) -> Self {
        Self { rx, names }
    }
}

impl DataSource for ChannelSource {
    fn channel_names(&self) -> &[String] {
        &self.names
    }

    fn poll(&mut self) -> Vec<Sample> {
        let mut out = Vec::new();
        while let Ok(s) = self.rx.try_recv() {
            out.push(s);
        }
        out
    }
}

/// 波形种类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waveform {
    /// 正弦：sin(2π·f·t + φ)
    Sine,
    /// 上升锯齿：2·frac((2π·f·t + φ)/2π) − 1，幅度 −1..1
    Sawtooth,
}

/// 本地波形源（演示/测试用）：按固定采样率产出波形样本。
/// 支持多通道，每通道（名称, 波形, 初相位/弧度）独立。
pub struct WaveSource {
    names: Vec<String>,
    /// 每通道（波形, 初相位/弧度）
    channels: Vec<(Waveform, f64)>,
    freq_hz: f64,
    amp: f64,
    period: Duration,
    started: Instant,
    next: Instant,
    paused: bool,
}

impl WaveSource {
    /// `channels` = 每通道 (名称, 波形, 初相位/弧度)；`rate_hz` = 每秒样本数
    pub fn new(channels: &[(String, Waveform, f64)], freq_hz: f64, amp: f64, rate_hz: f64) -> Self {
        let period = Duration::from_secs_f64(1.0 / rate_hz.max(1.0));
        Self {
            names: channels.iter().map(|(n, _, _)| n.clone()).collect(),
            channels: channels.iter().map(|(_, w, p)| (*w, *p)).collect(),
            freq_hz,
            amp,
            period,
            started: Instant::now(),
            next: Instant::now(),
            paused: false,
        }
    }

    /// 单个通道在时刻 t 的值
    fn wave_value(&self, wf: Waveform, phi: f64, t: f64) -> f64 {
        let th = 2.0 * std::f64::consts::PI * self.freq_hz * t + phi;
        let v = match wf {
            Waveform::Sine => th.sin(),
            Waveform::Sawtooth => 2.0 * ((th / (2.0 * std::f64::consts::PI)).fract() - 0.5),
        };
        self.amp * v
    }
}

impl DataSource for WaveSource {
    fn channel_names(&self) -> &[String] {
        &self.names
    }

    fn poll(&mut self) -> Vec<Sample> {
        if self.paused {
            return Vec::new();
        }
        let mut out = Vec::new();
        while Instant::now() >= self.next {
            // 按实际当前时刻生成，长时间无积压也不会漂移
            let t = self.started.elapsed().as_secs_f64();
            let values = self
                .channels
                .iter()
                .map(|(wf, phi)| self.wave_value(*wf, *phi, t))
                .collect();
            out.push(Sample { t, values });
            self.next += self.period;
            // 积压保护：落后太多时直接跳到当前时刻，不疯狂补样
            if self.next < Instant::now() - self.period {
                self.next = Instant::now();
            }
        }
        out
    }

    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wave_source_produces_monotonic_samples() {
        let mut s = WaveSource::new(
            &[
                ("sine".to_string(), Waveform::Sine, 0.0),
                (
                    "cosine".to_string(),
                    Waveform::Sine,
                    std::f64::consts::FRAC_PI_2,
                ),
                ("saw".to_string(), Waveform::Sawtooth, 0.0),
                (
                    "saw90".to_string(),
                    Waveform::Sawtooth,
                    std::f64::consts::FRAC_PI_2,
                ),
            ],
            1.0,
            1.0,
            100.0,
        );
        // 等两个周期，poll 至少能拿到样本
        std::thread::sleep(Duration::from_millis(30));
        let batch = s.poll();
        assert!(!batch.is_empty());
        for w in batch.windows(2) {
            assert!(w[0].t < w[1].t, "样本时间必须单调递增");
        }
        // 每样本四通道；同一时刻验证各波形解析式
        let s0 = &batch[0];
        assert_eq!(s0.values.len(), 4);
        let phi = 2.0 * std::f64::consts::PI * s0.t;
        assert!((s0.values[0] - phi.sin()).abs() < 1e-6);
        assert!((s0.values[1] - phi.cos()).abs() < 1e-6);
        // 锯齿：2·frac(θ/2π) − 1，90° 相位 = 多 1/4 周期
        let frac = (s0.t).fract();
        assert!((s0.values[2] - (2.0 * frac - 1.0)).abs() < 1e-6);
        let frac90 = (s0.t + 0.25).fract();
        assert!((s0.values[3] - (2.0 * frac90 - 1.0)).abs() < 1e-6);
        // 暂停后不再产出
        s.set_paused(true);
        std::thread::sleep(Duration::from_millis(20));
        assert!(s.poll().is_empty());
        // 恢复后继续产出
        s.set_paused(false);
        std::thread::sleep(Duration::from_millis(20));
        assert!(!s.poll().is_empty());
    }

    #[test]
    fn channel_source_drains_all_pending() {
        let (tx, rx) = mpsc::channel();
        let mut s = ChannelSource::new(rx, vec!["theta".to_string()]);
        for i in 0..5 {
            tx.send(Sample {
                t: i as f64,
                values: vec![i as f64],
            })
            .unwrap();
        }
        assert_eq!(s.poll().len(), 5);
        assert!(s.poll().is_empty());
        assert_eq!(s.channel_names(), &["theta".to_string()]);
    }
}
