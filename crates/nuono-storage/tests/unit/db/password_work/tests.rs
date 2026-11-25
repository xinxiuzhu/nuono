use super::*;
use std::sync::{
    Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

/// 测试闸门控制器：在 `Drop` 时也会自动唤醒放行所有阻塞线程，防止测试断言失败 panic 时遗留挂起的阻塞线程导致测试运行时卡死无法退出。
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn new() -> Self {
        Self(Arc::new((Mutex::new(false), Condvar::new())))
    }

    fn wait(&self) -> impl FnOnce() + Send + 'static {
        let state = self.0.clone();
        move || {
            let (lock, changed) = &*state;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
        }
    }

    fn release(&self) {
        let (lock, changed) = &*self.0;
        *lock.lock().unwrap() = true;
        changed.notify_all();
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.release();
    }
}

fn one_slot_pool() -> Arc<PasswordWorkPool> {
    Arc::new(PasswordWorkPool {
        permits: Arc::new(Semaphore::new(1)),
    })
}

async fn wait_until_idle(pool: &PasswordWorkPool, capacity: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while pool.permits.available_permits() != capacity {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("blocking work did not release its password permit");
}

struct DropProbe {
    dropped: Arc<AtomicBool>,
    pool: Arc<PasswordWorkPool>,
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        // 任务闭包捕获的资源必须在并发许可（permit）归还复用之前先行销毁（此时可用许可数仍为 0）。
        assert_eq!(self.pool.permits.available_permits(), 0);
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn blocked_password_worker_keeps_async_timer_responsive() {
    let pool = one_slot_pool();
    let gate = Gate::new();
    let wait = gate.wait();
    let (started, ready) = tokio::sync::oneshot::channel();
    let job = tokio::spawn({
        let pool = pool.clone();
        async move {
            pool.run(move || {
                let _ = started.send(());
                wait();
                Ok(())
            })
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), ready)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        tokio::time::sleep(Duration::from_millis(5)),
    )
    .await
    .unwrap();
    assert!(!job.is_finished());
    gate.release();
    job.await.unwrap().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn real_password_hash_yields_to_single_thread_scheduler() {
    let pool = PasswordWorkPool::new();
    let job = tokio::spawn({
        let pool = pool.clone();
        async move { pool.hash("Password-work-123!").await }
    });
    tokio::task::yield_now().await;
    // 首次轮询已将耗时的 bcrypt 任务提交到阻塞线程池，因此在阻塞线程计算期间当前单线程异步调度器仍可正常调度当前协程。
    assert!(!job.is_finished());
    tokio::time::sleep(Duration::from_millis(1)).await;
    let hash = job.await.unwrap().unwrap();
    assert!(pool.verify("Password-work-123!", &hash).await.unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn dummy_login_verification_yields_to_single_thread_scheduler() {
    let pool = PasswordWorkPool::new();
    let job = tokio::spawn(async move { pool.verify_login("Missing-password-123!", None).await });
    tokio::task::yield_now().await;
    assert!(!job.is_finished());
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(!job.await.unwrap().unwrap().valid);
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_login_verification_and_upgrade_share_one_admission() {
    let legacy = tokio::task::spawn_blocking(|| {
        bcrypt::hash("Legacy-password-123!", bcrypt::DEFAULT_COST).unwrap()
    })
    .await
    .unwrap();
    let pool = one_slot_pool();
    let result = pool
        .verify_login("Legacy-password-123!", Some(&legacy))
        .await
        .unwrap();
    assert!(result.valid);
    let upgraded = result.upgraded_hash.unwrap();
    assert!(upgraded.starts_with(PASSWORD_HASH_PREFIX));
    assert!(
        pool.verify("Legacy-password-123!", &upgraded)
            .await
            .unwrap()
    );
    assert_eq!(pool.permits.available_permits(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_truncated_password_is_verified_without_being_upgraded() {
    let password = "你".repeat(30);
    let legacy = tokio::task::spawn_blocking({
        let password = password.clone();
        move || bcrypt::hash(password, bcrypt::DEFAULT_COST).unwrap()
    })
    .await
    .unwrap();
    let pool = one_slot_pool();
    let result = pool.verify_login(&password, Some(&legacy)).await.unwrap();
    assert!(result.valid);
    assert!(result.upgraded_hash.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn password_work_has_four_slots_and_rejects_without_waiting() {
    let pool = PasswordWorkPool::new();
    let gate = Gate::new();
    let mut jobs = Vec::new();
    for _ in 0..PASSWORD_WORK_LIMIT {
        let wait = gate.wait();
        let (started, ready) = tokio::sync::oneshot::channel();
        jobs.push(tokio::spawn({
            let pool = pool.clone();
            async move {
                pool.run(move || {
                    let _ = started.send(());
                    wait();
                    Ok(())
                })
                .await
            }
        }));
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(pool.permits.available_permits(), 0);
    let ran = Arc::new(AtomicBool::new(false));
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        pool.run({
            let ran = ran.clone();
            move || {
                ran.store(true, Ordering::SeqCst);
                Ok(())
            }
        }),
    )
    .await
    .expect("overloaded password work waited for a slot");
    assert!(matches!(result, Err(NuonoError::Overloaded(_))));
    assert!(!ran.load(Ordering::SeqCst));
    assert!(matches!(
        pool.hash("Valid-password-123!").await,
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        pool.verify("password", "hash").await,
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        pool.verify_login("password", None).await,
        Err(NuonoError::Overloaded(_))
    ));
    gate.release();
    for job in jobs {
        job.await.unwrap().unwrap();
    }
    assert_eq!(pool.run(|| Ok(7)).await.unwrap(), 7);
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_running_waiter_does_not_release_its_slot_or_captures() {
    let pool = one_slot_pool();
    let gate = Gate::new();
    let wait = gate.wait();
    let dropped = Arc::new(AtomicBool::new(false));
    let probe = DropProbe {
        dropped: dropped.clone(),
        pool: pool.clone(),
    };
    let (started, ready) = tokio::sync::oneshot::channel();
    let job = tokio::spawn({
        let pool = pool.clone();
        async move {
            pool.run(move || {
                let _probe = probe;
                let _ = started.send(());
                wait();
                Ok(())
            })
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), ready)
        .await
        .unwrap()
        .unwrap();
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    assert!(!dropped.load(Ordering::SeqCst));
    assert!(matches!(
        pool.run(|| Ok(())).await,
        Err(NuonoError::Overloaded(_))
    ));
    gate.release();
    wait_until_idle(&pool, 1).await;
    assert!(dropped.load(Ordering::SeqCst));
    pool.run(|| Ok(())).await.unwrap();
}

#[test]
fn cancelling_queued_blocking_work_keeps_admission_until_actual_cleanup() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let occupied = Gate::new();
    let wait = occupied.wait();
    let (started, ready) = tokio::sync::oneshot::channel();
    let blocker = runtime.spawn_blocking(move || {
        let _ = started.send(());
        wait();
    });
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        let pool = one_slot_pool();
        let dropped = Arc::new(AtomicBool::new(false));
        let probe = DropProbe {
            dropped: dropped.clone(),
            pool: pool.clone(),
        };
        let job = tokio::spawn({
            let pool = pool.clone();
            async move {
                pool.run(move || {
                    let _probe = probe;
                    Ok(())
                })
                .await
            }
        });
        tokio::task::yield_now().await;
        assert_eq!(pool.permits.available_permits(), 0);
        job.abort();
        assert!(job.await.unwrap_err().is_cancelled());
        assert!(!dropped.load(Ordering::SeqCst));
        assert!(matches!(
            pool.run(|| Ok(())).await,
            Err(NuonoError::Overloaded(_))
        ));
        occupied.release();
        blocker.await.unwrap();
        wait_until_idle(&pool, 1).await;
        assert!(dropped.load(Ordering::SeqCst));
        pool.run(|| Ok(())).await.unwrap();
    });
}

#[tokio::test(flavor = "current_thread")]
async fn panic_and_error_release_password_capacity_for_reuse() {
    let pool = one_slot_pool();
    let dropped = Arc::new(AtomicBool::new(false));
    let probe = DropProbe {
        dropped: dropped.clone(),
        pool: pool.clone(),
    };
    let result: Result<()> = pool
        .run(move || {
            let _probe = probe;
            panic!("injected password worker panic")
        })
        .await;
    assert!(matches!(result, Err(NuonoError::Internal(_))));
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(pool.permits.available_permits(), 1);
    let result: Result<()> = pool
        .run(|| Err(NuonoError::Internal("injected work failure".into())))
        .await;
    assert!(result.is_err());
    assert_eq!(pool.run(|| Ok(11)).await.unwrap(), 11);
}

#[tokio::test(flavor = "current_thread")]
async fn database_clones_share_password_admission_and_independent_databases_do_not() {
    use crate::db::{NewUser, TOKEN_RESET_PASSWORD};
    use nuono_app::{
        account_provisioning::{AccountProvisioningStore, NewManagedAccount},
        account_security::{AccountSecurityStore, MfaOperation, TokenOperation},
        auth::Role,
    };
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let db = crate::test_support::test_db(2).await;
    let other = crate::test_support::test_db(2).await;
    let new_user = || NewUser {
        username: "password-owner".into(),
        email: None,
        password: "Valid-password-123!".into(),
        role: "owner".into(),
        status: "active".into(),
        email_verified: true,
    };
    let user = db.create_user(new_user()).await.unwrap();
    let token = db
        .issue_user_token(user.id, TOKEN_RESET_PASSWORD, 30, None)
        .await
        .unwrap();
    let managed_account = || NewManagedAccount {
        username: "new-account".into(),
        email: None,
        password: "Valid-password-123!".into(),
        role: Role::User,
    };
    let cloned = db.clone();
    assert!(Arc::ptr_eq(&db.password_work, &cloned.password_work));
    assert!(!Arc::ptr_eq(&db.password_work, &other.password_work));
    let gate = Gate::new();
    let mut jobs = Vec::new();
    for _ in 0..PASSWORD_WORK_LIMIT {
        let wait = gate.wait();
        let (started, ready) = tokio::sync::oneshot::channel();
        jobs.push(tokio::spawn({
            let db = db.clone();
            async move {
                db.password_work
                    .run(move || {
                        let _ = started.send(());
                        wait();
                        Ok(())
                    })
                    .await
            }
        }));
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
    }
    assert!(matches!(
        cloned
            .verify_user_password("missing-account", "password")
            .await,
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        cloned.set_user_password(1, "Valid-password-123!").await,
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        cloned
            .verify_user_password(&user.username, "Valid-password-123!")
            .await,
        Err(NuonoError::Overloaded(_))
    ));
    let mut candidate_user = new_user();
    candidate_user.username = "would-be-created".into();
    assert!(matches!(
        cloned.create_user(candidate_user).await,
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        cloned.create_first_owner(new_user()).await,
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        cloned
            .change_password_for_verified_user(
                &user,
                &format!("auth_session_{}", "a".repeat(64)),
                "Changed-password-456!",
            )
            .await,
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        cloned
            .recover_admin_credentials(&user.username, "Changed-password-456!", true)
            .await,
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        cloned
            .redeem_account_token(
                &token.token,
                TokenOperation::ResetPassword {
                    password: "Changed-password-456!".into(),
                }
            )
            .await,
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        cloned
            .change_account_mfa(
                user.id,
                MfaOperation::Enroll {
                    password: "Valid-password-123!".into(),
                }
            )
            .await,
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        cloned
            .prepare_email_change(user.id, "Valid-password-123!", "changed@example.com", 30)
            .await,
        Err(NuonoError::Overloaded(_))
    ));
    assert!(matches!(
        cloned
            .create_managed_account(user.id, managed_account())
            .await,
        Err(NuonoError::Overloaded(_))
    ));
    assert_eq!(cloned.user_count().await.unwrap(), 1);
    let unchanged = cloned.find_user_by_id(user.id).await.unwrap().unwrap();
    assert_eq!(unchanged.password_hash, user.password_hash);
    assert!(unchanged.email.is_none() && unchanged.totp_secret.is_none());
    assert!(
        crate::db::account_state::challenges::Entity::find()
            .filter(
                crate::db::account_state::challenges::Column::TokenHash
                    .eq(crate::db::account_state::token_hash(&token.token))
            )
            .one(db.orm())
            .await
            .unwrap()
            .unwrap()
            .consumed_at_ms
            .is_none()
    );
    assert_eq!(other.password_work.run(|| Ok(3)).await.unwrap(), 3);
    gate.release();
    for job in jobs {
        job.await.unwrap().unwrap();
    }
    db.shutdown().await.unwrap();
    other.shutdown().await.unwrap();
}
