//! GUI 窗口子进程连接的通用父端（plot 与 watch 共用）。
//!
//! 为什么 GUI 必须放独立子进程：winit 每进程只允许创建一个 EventLoop
//! （Linux 上创建后永不复位），debug 会话同进程关窗后无法再开新窗。
//! 派生为 `tscope plot --feed` / `tscope watch --feed` 子进程后，
//! 每次开窗 = 全新进程 = 全新 EventLoop，开/关/重开天然干净。
//!
//! 父端只做三件事：经 stdin 写行协议数据、轮询子进程退出状态检测关窗
//! （内核暂停时采样不发数据，不能依赖写失败检测）、会话结束时收尾。

use std::io::BufWriter;
use std::process::{Child, ChildStdin, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};

/// 与 GUI 子进程的连接
pub struct FeedChild {
    pub child: Child,
    stdin: Option<BufWriter<ChildStdin>>,
}

impl FeedChild {
    /// 派生本进程自身（`current_exe`）为带参数的新子进程，stdin 走管道。
    /// `args` 形如 `["plot", "--feed"]`。子进程继承 stdout/stderr，
    /// 其报错直接出现在调试终端里。
    pub fn spawn(args: &[&str]) -> Result<Self> {
        let exe = std::env::current_exe().context("取不到当前可执行文件路径（无法派生窗口子进程）")?;
        let mut child = std::process::Command::new(exe)
            .args(args)
            .stdin(Stdio::piped())
            .spawn()
            .context("启动窗口子进程失败")?;
        let stdin = BufWriter::new(child.stdin.take().context("拿不到子进程 stdin 管道")?);
        Ok(Self {
            child,
            stdin: Some(stdin),
        })
    }

    /// 取写端（写头部/数据行用；子进程已退出或管道已关时为 None）
    pub fn writer(&mut self) -> Option<&mut BufWriter<ChildStdin>> {
        self.stdin.as_mut()
    }

    /// 子进程是否已退出（用户关窗/窗口异常）。可重复调用。
    pub fn poll_exit(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// 会话结束：先关写端让子进程经 stdin EOF 自行干净退出，
    /// 短暂等待后仍未退出再强杀兜底（已退出的忽略错误）
    pub fn kill(&mut self) {
        self.stdin = None; // drop 写端 → 子进程读线程 EOF → exit(0)
        for _ in 0..30 {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
