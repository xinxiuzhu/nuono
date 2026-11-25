//! 工具并发调度器：为模型自动调用与审批恢复执行提供统一的并发槽位与读写资源锁协调。
use super::contracts::{ToolResource, ToolResourceMode, ToolScheduling};
use nuono_error::{NuonoError, Result};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{
    OwnedRwLockReadGuard, OwnedRwLockWriteGuard, OwnedSemaphorePermit, RwLock, Semaphore,
};

impl ToolScheduling {
    pub(super) fn normalize(self) -> Result<Self> {
        let Self::Concurrent { resources } = self else {
            return Ok(self);
        };
        if resources.len() > 32 {
            return Err(NuonoError::Validation("工具最多声明 32 个资源".into()));
        }
        let mut merged = BTreeMap::new();
        for resource in resources {
            if resource.key.trim().is_empty()
                || resource.key.len() > 512
                || resource.key.chars().any(char::is_control)
            {
                return Err(NuonoError::Validation("工具资源键无效".into()));
            }
            merged
                .entry(resource.key)
                .and_modify(|mode| {
                    if resource.mode == ToolResourceMode::Write {
                        *mode = ToolResourceMode::Write;
                    }
                })
                .or_insert(resource.mode);
        }
        Ok(Self::concurrent(
            merged
                .into_iter()
                .map(|(key, mode)| ToolResource { key, mode }),
        ))
    }

    pub(super) fn conflicts_with(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Concurrent { resources: a }, Self::Concurrent { resources: b }) => {
                a.iter().any(|a| {
                    b.iter().any(|b| {
                        a.key == b.key
                            && (a.mode == ToolResourceMode::Write
                                || b.mode == ToolResourceMode::Write)
                    })
                })
            }
            _ => true,
        }
    }
}

pub(super) struct ToolScheduler {
    limit: usize,
    gate: Arc<RwLock<()>>,
    slots: Arc<Semaphore>,
    resources: Mutex<HashMap<String, Weak<RwLock<()>>>>,
}

impl ToolScheduler {
    pub(super) fn new(limit: usize) -> Result<Self> {
        if !(1..=64).contains(&limit) {
            return Err(NuonoError::Validation("工具并发上限必须为 1..=64".into()));
        }
        Ok(Self {
            limit,
            gate: Arc::new(RwLock::new(())),
            slots: Arc::new(Semaphore::new(limit)),
            resources: Mutex::new(HashMap::new()),
        })
    }

    pub(super) fn limit(&self) -> usize {
        self.limit
    }

    pub(super) async fn acquire(&self, scheduling: &ToolScheduling) -> ToolPermit {
        let gate = match scheduling {
            ToolScheduling::Exclusive => ResourceGuard::Write {
                _guard: self.gate.clone().write_owned().await,
            },
            ToolScheduling::Concurrent { .. } => ResourceGuard::Read {
                _guard: self.gate.clone().read_owned().await,
            },
        };
        let slot = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .expect("tool scheduler remains open");
        let locks = {
            let mut known = self.resources.lock().unwrap_or_else(|e| e.into_inner());
            // 清理已无人持有的弱引用资源锁，防止动态会话或文件键无限累积导致内存泄漏。
            known.retain(|_, lock| lock.strong_count() > 0);
            match scheduling {
                ToolScheduling::Exclusive => Vec::new(),
                ToolScheduling::Concurrent { resources } => resources
                    .iter()
                    .map(|resource| {
                        let lock = known
                            .get(&resource.key)
                            .and_then(Weak::upgrade)
                            .unwrap_or_else(|| {
                                let lock = Arc::new(RwLock::new(()));
                                known.insert(resource.key.clone(), Arc::downgrade(&lock));
                                lock
                            });
                        (lock, resource.mode)
                    })
                    .collect(),
            }
        };
        let mut guards = Vec::with_capacity(locks.len());
        // 资源键已按字典序规范化排序，确保所有并发调用（包括排队写锁）始终按相同顺序加锁，避免死锁。
        for (lock, mode) in locks {
            guards.push(match mode {
                ToolResourceMode::Read => ResourceGuard::Read {
                    _guard: lock.read_owned().await,
                },
                ToolResourceMode::Write => ResourceGuard::Write {
                    _guard: lock.write_owned().await,
                },
            });
        }
        ToolPermit {
            _resources: guards,
            _slot: slot,
            _gate: gate,
        }
    }
}

enum ResourceGuard {
    Read { _guard: OwnedRwLockReadGuard<()> },
    Write { _guard: OwnedRwLockWriteGuard<()> },
}

/// 工具执行租约守卫：一旦被丢弃（包括等待中途被取消），会自动按逆序释放所有已获取的资源锁与并发槽位。
pub(super) struct ToolPermit {
    _resources: Vec<ResourceGuard>,
    _slot: OwnedSemaphorePermit,
    _gate: ResourceGuard,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{future::Future, pin::Pin, task::Poll, time::Duration};

    async fn queued<F: Future>(future: Pin<&mut F>) {
        let mut future = future;
        std::future::poll_fn(|cx| {
            assert!(
                future.as_mut().poll(cx).is_pending(),
                "conflicting call must wait"
            );
            Poll::Ready(())
        })
        .await;
    }

    #[tokio::test]
    async fn readers_share_resources_but_writers_and_exclusive_tools_wait() {
        let scheduler = ToolScheduler::new(4).unwrap();
        let read = ToolScheduling::concurrent([ToolResource::read("notes")]);
        let write = ToolScheduling::concurrent([ToolResource::write("notes")]);
        let a = scheduler.acquire(&read).await;
        let b = scheduler.acquire(&read).await;
        let mut writer = Box::pin(scheduler.acquire(&write));
        queued(writer.as_mut()).await;
        drop(a);
        queued(writer.as_mut()).await;
        drop(b);
        let writer = writer.await;
        let mut exclusive = Box::pin(scheduler.acquire(&ToolScheduling::Exclusive));
        queued(exclusive.as_mut()).await;
        drop(writer);
        let exclusive = exclusive.await;
        let mut reader = Box::pin(scheduler.acquire(&read));
        queued(reader.as_mut()).await;
        drop(exclusive);
        tokio::time::timeout(Duration::from_secs(1), reader)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn concurrency_cap_is_global_even_for_independent_resources() {
        let scheduler = ToolScheduler::new(2).unwrap();
        let a = ToolScheduling::concurrent([ToolResource::write("a")]);
        let b = ToolScheduling::concurrent([ToolResource::write("b")]);
        let c = ToolScheduling::concurrent([]);
        let first = scheduler.acquire(&a).await;
        let second = scheduler.acquire(&b).await;
        let mut third = Box::pin(scheduler.acquire(&c));
        queued(third.as_mut()).await;
        drop(first);
        let third = third.await;
        drop((second, third));
        assert_eq!(scheduler.slots.available_permits(), 2);
    }

    #[tokio::test]
    async fn cancelling_resource_and_slot_waiters_releases_partial_acquisitions() {
        let scheduler = ToolScheduler::new(2).unwrap();
        let a = ToolScheduling::concurrent([ToolResource::write("a")]);
        let both = ToolScheduling::concurrent([ToolResource::write("b"), ToolResource::write("a")])
            .normalize()
            .unwrap();
        let independent = ToolScheduling::concurrent([ToolResource::write("b")]);
        let holder = scheduler.acquire(&a).await;
        let mut resources = Box::pin(scheduler.acquire(&both));
        queued(resources.as_mut()).await;
        let mut slots = Box::pin(scheduler.acquire(&independent));
        queued(slots.as_mut()).await;
        drop((resources, slots));
        drop(holder);
        tokio::time::timeout(
            Duration::from_secs(1),
            scheduler.acquire(&ToolScheduling::Exclusive),
        )
        .await
        .unwrap();
        assert_eq!(scheduler.slots.available_permits(), 2);
    }

    #[tokio::test]
    async fn opposite_multi_resource_declarations_do_not_deadlock() {
        let scheduler = ToolScheduler::new(2).unwrap();
        let forward =
            ToolScheduling::concurrent([ToolResource::read("a"), ToolResource::write("b")])
                .normalize()
                .unwrap();
        let reverse =
            ToolScheduling::concurrent([ToolResource::write("b"), ToolResource::write("a")])
                .normalize()
                .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            let (_, _) = tokio::join!(
                async {
                    let _permit = scheduler.acquire(&forward).await;
                    tokio::task::yield_now().await;
                },
                async {
                    let _permit = scheduler.acquire(&reverse).await;
                    tokio::task::yield_now().await;
                },
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn completed_dynamic_keys_do_not_accumulate_locks() {
        let scheduler = ToolScheduler::new(2).unwrap();
        for session in 0..100 {
            let declaration =
                ToolScheduling::concurrent([ToolResource::write(format!("session:{session}"))]);
            drop(scheduler.acquire(&declaration).await);
        }
        assert_eq!(scheduler.resources.lock().unwrap().len(), 1);
    }

    #[test]
    fn declarations_validate_keys_and_merge_duplicate_accesses() {
        let normalized = ToolScheduling::concurrent([
            ToolResource::read("b"),
            ToolResource::write("b"),
            ToolResource::read("a"),
        ])
        .normalize()
        .unwrap();
        assert_eq!(
            normalized,
            ToolScheduling::concurrent([ToolResource::read("a"), ToolResource::write("b")])
        );
        assert!(normalized.conflicts_with(&ToolScheduling::concurrent([ToolResource::read("b")])));
        assert!(!normalized.conflicts_with(&ToolScheduling::concurrent([ToolResource::read("a")])));
        for key in [String::new(), "  ".into(), "a\nb".into(), "x".repeat(513)] {
            assert!(
                ToolScheduling::concurrent([ToolResource::read(key)])
                    .normalize()
                    .is_err()
            );
        }
        assert!(
            ToolScheduling::concurrent((0..33).map(|i| ToolResource::read(i.to_string())))
                .normalize()
                .is_err()
        );
        assert!(ToolScheduler::new(0).is_err());
        assert!(ToolScheduler::new(65).is_err());
    }
}
