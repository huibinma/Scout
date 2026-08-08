//! Tool Loop Controller：控制多步工具调用循环。

use std::error::Error;
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

/// 单步执行后的循环指令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopStep<T> {
    /// 继续执行下一步。
    Continue,
    /// 循环完成，并返回最终值。
    Done(T),
}

/// Tool Loop 错误。
#[derive(Debug)]
pub enum LoopError {
    /// 达到最大步数仍未完成。
    MaxStepsExceeded,
    /// 总耗时超过整体超时。
    OverallTimeout,
    /// 单步耗时超过单步超时。
    StepTimeout,
    /// 外部取消信号已触发。
    Cancelled,
    /// 单步闭包返回的内部错误。
    Inner(Box<dyn Error + Send + Sync>),
}

impl fmt::Display for LoopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MaxStepsExceeded => f.write_str("tool loop max steps exceeded"),
            Self::OverallTimeout => f.write_str("tool loop overall timeout"),
            Self::StepTimeout => f.write_str("tool loop step timeout"),
            Self::Cancelled => f.write_str("tool loop cancelled"),
            Self::Inner(error) => write!(f, "tool loop inner error: {error}"),
        }
    }
}

impl Error for LoopError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inner(error) => Some(error.as_ref()),
            Self::MaxStepsExceeded | Self::OverallTimeout | Self::StepTimeout | Self::Cancelled => {
                None
            }
        }
    }
}

impl PartialEq for LoopError {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::MaxStepsExceeded, Self::MaxStepsExceeded)
                | (Self::OverallTimeout, Self::OverallTimeout)
                | (Self::StepTimeout, Self::StepTimeout)
                | (Self::Cancelled, Self::Cancelled)
                | (Self::Inner(_), Self::Inner(_))
        )
    }
}

impl Eq for LoopError {}

/// Tool Loop 的最终输出。
#[derive(Debug, PartialEq, Eq)]
pub enum LoopOutcome<T> {
    /// 循环正常完成。
    Done(T),
    /// 循环被控制器中止。
    Aborted(LoopError),
}

/// Tool Loop Controller 配置与执行入口。
#[derive(Debug, Clone)]
pub struct ToolLoopController {
    /// 最大执行步数。
    pub max_steps: usize,
    /// 整体超时时间。
    pub overall_timeout: Duration,
    /// 单步超时时间。
    pub per_step_timeout: Duration,
    cancel_signal: Arc<AtomicBool>,
}

impl Default for ToolLoopController {
    fn default() -> Self {
        Self {
            max_steps: 10,
            overall_timeout: Duration::from_secs(30),
            per_step_timeout: Duration::from_secs(10),
            cancel_signal: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl ToolLoopController {
    /// 创建使用默认配置的控制器。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置最大步数。
    #[must_use]
    pub const fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps;
        self
    }

    /// 设置整体超时时间。
    #[must_use]
    pub const fn with_overall_timeout(mut self, overall_timeout: Duration) -> Self {
        self.overall_timeout = overall_timeout;
        self
    }

    /// 设置单步超时时间。
    #[must_use]
    pub const fn with_per_step_timeout(mut self, per_step_timeout: Duration) -> Self {
        self.per_step_timeout = per_step_timeout;
        self
    }

    /// 设置外部取消信号。
    #[must_use]
    pub fn with_cancel_signal(mut self, cancel_signal: Arc<AtomicBool>) -> Self {
        self.cancel_signal = cancel_signal;
        self
    }

    /// 返回当前取消信号，便于调用方在其他线程触发取消。
    #[must_use]
    pub fn cancel_signal(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel_signal)
    }

    /// 运行多步工具循环。
    ///
    /// 闭包参数是从 0 开始的 step index。闭包返回 [`LoopStep::Done`] 时循环立即结束；
    /// 返回 [`LoopStep::Continue`] 时进入下一步。当前实现为同步阻塞控制器，单步超时在
    /// 闭包返回后按耗时判定；MVP-07A async/streaming 后可升级为可抢占式超时。
    pub fn run<T, E, F>(&self, mut step: F) -> LoopOutcome<T>
    where
        E: Error + Send + Sync + 'static,
        F: FnMut(usize) -> Result<LoopStep<T>, E>,
    {
        let started_at = Instant::now();

        for step_index in 0..self.max_steps {
            if self.cancel_signal.load(Ordering::SeqCst) {
                return LoopOutcome::Aborted(LoopError::Cancelled);
            }
            if started_at.elapsed() >= self.overall_timeout {
                return LoopOutcome::Aborted(LoopError::OverallTimeout);
            }

            let step_started_at = Instant::now();
            let step_result = step(step_index);
            let step_elapsed = step_started_at.elapsed();

            if step_elapsed >= self.per_step_timeout {
                return LoopOutcome::Aborted(LoopError::StepTimeout);
            }
            if self.cancel_signal.load(Ordering::SeqCst) {
                return LoopOutcome::Aborted(LoopError::Cancelled);
            }
            if started_at.elapsed() >= self.overall_timeout {
                return LoopOutcome::Aborted(LoopError::OverallTimeout);
            }

            match step_result {
                Ok(LoopStep::Done(value)) => return LoopOutcome::Done(value),
                Ok(LoopStep::Continue) => {}
                Err(error) => return LoopOutcome::Aborted(LoopError::Inner(Box::new(error))),
            }
        }

        LoopOutcome::Aborted(LoopError::MaxStepsExceeded)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::io;
    use std::thread;

    #[test]
    fn stops_immediately_when_step_returns_done() {
        let controller = ToolLoopController::new();
        let mut calls = 0usize;
        let outcome = controller.run(|_| -> Result<LoopStep<&'static str>, io::Error> {
            calls += 1;
            Ok(LoopStep::Done("ok"))
        });

        assert_eq!(outcome, LoopOutcome::Done("ok"));
        assert_eq!(calls, 1);
    }

    #[test]
    fn max_steps_exceeded_when_never_done() {
        let controller = ToolLoopController::new().with_max_steps(2);
        let outcome =
            controller.run(|_| -> Result<LoopStep<()>, io::Error> { Ok(LoopStep::Continue) });

        assert_eq!(outcome, LoopOutcome::Aborted(LoopError::MaxStepsExceeded));
    }

    #[test]
    fn overall_timeout_stops_loop() {
        let controller = ToolLoopController::new()
            .with_max_steps(10)
            .with_overall_timeout(Duration::from_millis(20))
            .with_per_step_timeout(Duration::from_secs(1));
        let outcome = controller.run(|_| -> Result<LoopStep<()>, io::Error> {
            thread::sleep(Duration::from_millis(25));
            Ok(LoopStep::Continue)
        });

        assert_eq!(outcome, LoopOutcome::Aborted(LoopError::OverallTimeout));
    }

    #[test]
    fn step_timeout_stops_loop() {
        let controller = ToolLoopController::new()
            .with_overall_timeout(Duration::from_secs(1))
            .with_per_step_timeout(Duration::from_millis(10));
        let outcome = controller.run(|_| -> Result<LoopStep<()>, io::Error> {
            thread::sleep(Duration::from_millis(15));
            Ok(LoopStep::Continue)
        });

        assert_eq!(outcome, LoopOutcome::Aborted(LoopError::StepTimeout));
    }

    #[test]
    fn cancellation_signal_stops_loop() {
        let cancel_signal = Arc::new(AtomicBool::new(false));
        let controller = ToolLoopController::new()
            .with_max_steps(10)
            .with_cancel_signal(Arc::clone(&cancel_signal));
        let outcome = controller.run(|_| -> Result<LoopStep<()>, io::Error> {
            cancel_signal.store(true, Ordering::SeqCst);
            Ok(LoopStep::Continue)
        });

        assert_eq!(outcome, LoopOutcome::Aborted(LoopError::Cancelled));
    }

    #[test]
    fn inner_error_is_forwarded() {
        let controller = ToolLoopController::new();
        let outcome = controller
            .run(|_| -> Result<LoopStep<()>, io::Error> { Err(io::Error::other("boom")) });

        let LoopOutcome::Aborted(LoopError::Inner(error)) = outcome else {
            panic!("expected inner error");
        };
        assert_eq!(error.to_string(), "boom");
    }
}
