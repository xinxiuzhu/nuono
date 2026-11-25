//! 业务启动许可；等待中的任务只有收到许可后才能产生业务副作用。

use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Waiting,
    Running,
    Closed,
}

/// 进程内共享的单向启动许可。关闭后无法再次打开。
///
/// 宿主与每次插件激活分别拥有自己的许可；停机或激活失败会永久关闭对应许可。
#[derive(Clone, Debug)]
pub struct BusinessGate {
    phase: watch::Sender<Phase>,
}

impl BusinessGate {
    /// 创建等待启动的许可。
    pub fn paused() -> Self {
        Self {
            phase: watch::channel(Phase::Waiting).0,
        }
    }

    /// 创建已允许运行的许可，供独立嵌入运行时使用。
    pub fn running() -> Self {
        Self {
            phase: watch::channel(Phase::Running).0,
        }
    }

    /// 当前是否允许运行业务。
    pub fn is_running(&self) -> bool {
        *self.phase.borrow() == Phase::Running
    }

    /// 当前是否已永久关闭。
    pub fn is_closed(&self) -> bool {
        *self.phase.borrow() == Phase::Closed
    }

    /// 发布启动许可；已关闭时返回 false，重复打开是幂等的。
    pub fn open(&self) -> bool {
        self.phase.send_if_modified(|phase| {
            if *phase == Phase::Waiting {
                *phase = Phase::Running;
                true
            } else {
                false
            }
        });
        self.is_running()
    }

    /// 关闭许可并唤醒所有等待者；不能通过 open 再次启动。
    pub fn close(&self) {
        self.phase.send_replace(Phase::Closed);
    }

    /// 等待开始运行；等待期间被关闭则返回 false。
    pub async fn wait_until_running(&self) -> bool {
        let mut phase = self.phase.subscribe();
        loop {
            match *phase.borrow_and_update() {
                Phase::Running => return true,
                Phase::Closed => return false,
                Phase::Waiting => {}
            }
            if phase.changed().await.is_err() {
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn waiting_observes_start_and_terminal_close() {
        let gate = BusinessGate::paused();
        let waiter = gate.clone();
        let task = tokio::spawn(async move { waiter.wait_until_running().await });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        assert!(gate.open());
        assert!(task.await.unwrap());
        assert!(gate.open());
        gate.close();
        assert!(!gate.open());
        assert!(!gate.wait_until_running().await);
    }

    #[tokio::test]
    async fn closing_before_start_wakes_waiters_without_starting() {
        let gate = BusinessGate::paused();
        let waiter = gate.clone();
        let task = tokio::spawn(async move { waiter.wait_until_running().await });
        gate.close();
        assert!(!task.await.unwrap());
        assert!(!gate.open());
    }
}
