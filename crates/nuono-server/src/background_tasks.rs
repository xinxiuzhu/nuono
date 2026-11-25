//! 宿主固定后台维护任务的取消通知与退出等待。
//!
//! 取消边界由各任务决定；这里不会 abort 或丢弃正在执行的备份。

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub(crate) struct HostBackgroundTasks {
    token: CancellationToken,
    tracker: TaskTracker,
    active_names: Arc<Mutex<Vec<&'static str>>>,
    sealed: bool,
}

impl HostBackgroundTasks {
    pub(crate) fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            tracker: TaskTracker::new(),
            active_names: Arc::new(Mutex::new(Vec::new())),
            sealed: false,
        }
    }

    pub(crate) fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    pub(crate) fn spawn(
        &mut self,
        name: &'static str,
        future: impl Future<Output = ()> + Send + 'static,
    ) {
        assert!(
            !self.sealed && !self.token.is_cancelled(),
            "宿主后台任务登记已经关闭"
        );
        self.active_names.lock().unwrap().push(name);
        let registration = TaskRegistration {
            name,
            active_names: self.active_names.clone(),
        };
        let token = self.token.clone();
        self.tracker.spawn(async move {
            // guard 覆盖 future 的执行与析构，也会在 runtime 丢弃任务时移除名称。
            let _registration = registration;
            if let Err(payload) = AssertUnwindSafe(future).catch_unwind().await {
                let message = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("非字符串 panic");
                tracing::error!(task = name, panic = message, "宿主后台任务 panic");
            }
            tracing::debug!(
                task = name,
                cancelled = token.is_cancelled(),
                "宿主后台任务已退出"
            );
        });
    }

    /// 启动时固定任务登记完成，之后不能添加顶层任务。
    pub(crate) fn seal(&mut self) {
        self.sealed = true;
    }

    pub(crate) fn cancel(&self) {
        self.token.cancel();
        self.tracker.close();
    }

    /// 必须先 cancel；持续等待所有任务和它们持有的 future 完成析构。
    /// 长等待仅记录日志，不设置会丢弃备份的超时或强制终止。
    pub(crate) async fn wait(&self) {
        let wait = self.tracker.wait();
        tokio::pin!(wait);
        loop {
            tokio::select! {
                biased;
                _ = &mut wait => return,
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    let names = self.active_names.lock().unwrap().clone();
                    tracing::warn!(
                        remaining = self.tracker.len(),
                        tasks = ?names,
                        "仍在等待宿主后台任务退出"
                    );
                }
            }
        }
    }
}

impl Drop for HostBackgroundTasks {
    fn drop(&mut self) {
        // Drop 无法异步等待，只发取消通知。正常及错误返回路径必须显式 wait。
        self.cancel();
    }
}

struct TaskRegistration {
    name: &'static str,
    active_names: Arc<Mutex<Vec<&'static str>>>,
}

impl Drop for TaskRegistration {
    fn drop(&mut self) {
        let mut names = self.active_names.lock().unwrap();
        if let Some(position) = names.iter().position(|name| *name == self.name) {
            names.swap_remove(position);
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/background_tasks/tests.rs"]
mod tests;
