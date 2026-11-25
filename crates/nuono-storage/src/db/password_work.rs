//! 账号密码哈希与校验的有界 CPU 工作池（同一 `Database` 的所有克隆共享同一并发配额）。
//! 并发准入涵盖已派发到 Tokio 阻塞线程池（`spawn_blocking`）的任务；
//! 若异步调用方中途取消等待，在底层阻塞闭包真正执行完毕前不会中断 bcrypt 计算，也不会提前归还并发许可。

use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::error::{NuonoError, Result};

pub(super) const PASSWORD_WORK_LIMIT: usize = 4;
const PASSWORD_HASH_PREFIX: &str = "sha256-bcrypt$";

#[derive(Debug)]
pub(super) struct PasswordWorkPool {
    permits: Arc<Semaphore>,
}

pub(super) struct PasswordVerification {
    pub valid: bool,
    pub upgraded_hash: Option<String>,
}

impl PasswordWorkPool {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(PASSWORD_WORK_LIMIT)),
        })
    }

    async fn run<T, F>(&self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        // 并发满载时不排队等待，直接快速失败返回 Overloaded。
        let permit = self.permits.clone().try_acquire_owned().map_err(|_| {
            metrics::counter!("nuono_password_work_rejected_total", "reason" => "full")
                .increment(1);
            NuonoError::Overloaded("密码处理繁忙，请稍后重试".into())
        })?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // 调用 FnOnce 闭包会消费并在当前阻塞线程内销毁其捕获的密码明文（即使发生 panic 栈展开，也会在信号量许可归还前完成释放）。
            work()
        })
        .await
        .map_err(|error| {
            tracing::error!(%error, "密码处理任务异常退出");
            NuonoError::Internal("密码处理任务异常退出".into())
        })?
    }

    pub async fn hash(&self, password: &str) -> Result<String> {
        // 密码格式校验开销极小，在进入阻塞线程池前先完成，避免格式非法的密码占用工作线程或触发繁忙限流。
        nuono_app::auth::validate_password(password).map_err(NuonoError::ConfigValidation)?;
        let password = password.to_owned();
        self.run(move || hash_password_sync(&password)).await
    }

    pub async fn verify(&self, password: &str, stored_hash: &str) -> Result<bool> {
        let password = password.to_owned();
        let stored_hash = stored_hash.to_owned();
        self.run(move || Ok(verify_password_sync(&password, &stored_hash)))
            .await
    }

    /// 登录密码校验与旧版哈希平滑升级共用同一次并发准入许可。
    /// 当账号不存在或已停用时，仍会执行一次假 bcrypt 校验（含首次懒加载初始化）以对齐耗时，防止时序侧信道探测。
    pub async fn verify_login(
        &self,
        password: &str,
        stored_hash: Option<&str>,
    ) -> Result<PasswordVerification> {
        let password = password.to_owned();
        let stored_hash = stored_hash.map(str::to_owned);
        self.run(move || {
            let Some(stored_hash) = stored_hash else {
                let _ = verify_password_sync(&password, &DUMMY_PASSWORD_HASH);
                return Ok(PasswordVerification {
                    valid: false,
                    upgraded_hash: None,
                });
            };
            let valid = verify_password_sync(&password, &stored_hash);
            let upgraded_hash = if valid
                && !stored_hash.starts_with(PASSWORD_HASH_PREFIX)
                && password.len() <= 72
            {
                // 旧格式密码升级采用尽力而为策略：只要旧密码校验通过，即使生成新版哈希失败也不能拒绝本次正常登录。
                match hash_password_sync(&password) {
                    Ok(hash) => Some(hash),
                    Err(error) => {
                        tracing::warn!(%error, "生成新版密码哈希失败");
                        None
                    }
                }
            } else {
                None
            };
            Ok(PasswordVerification {
                valid,
                upgraded_hash,
            })
        })
        .await
    }
}

// 同步哈希/校验原语在生产构建中仅限本模块内部调用；
// `cfg(test)` 下仅由 `db::users` 重新导出，供现有测试数据构造与哈希格式断言使用。
fn hash_password_sync(password: &str) -> Result<String> {
    nuono_app::auth::validate_password(password).map_err(NuonoError::ConfigValidation)?;
    let digest = hex::encode(Sha256::digest(password.as_bytes()));
    let hash = bcrypt::non_truncating_hash(digest, bcrypt::DEFAULT_COST)
        .map_err(|e| NuonoError::Internal(format!("密码哈希失败: {e}")))?;
    Ok(format!("{PASSWORD_HASH_PREFIX}{hash}"))
}

fn verify_password_sync(password: &str, stored_hash: &str) -> bool {
    if let Some(hash) = stored_hash.strip_prefix(PASSWORD_HASH_PREFIX) {
        let digest = hex::encode(Sha256::digest(password.as_bytes()));
        return bcrypt::non_truncating_verify(digest, hash).unwrap_or(false);
    }
    bcrypt::verify(password, stored_hash).unwrap_or(false)
}

static DUMMY_PASSWORD_HASH: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    use rand::TryRng;
    let mut bytes = [0u8; 24];
    let password = match rand::rngs::SysRng.try_fill_bytes(&mut bytes) {
        Ok(()) => hex::encode(bytes),
        Err(_) => "dummy-password-for-timing".to_string(),
    };
    hash_password_sync(&password).unwrap_or_default()
});

#[cfg(test)]
pub(super) fn hash_password_fixture(password: &str) -> Result<String> {
    hash_password_sync(password)
}

#[cfg(test)]
pub(super) fn verify_password_fixture(password: &str, stored_hash: &str) -> bool {
    verify_password_sync(password, stored_hash)
}

#[cfg(test)]
#[path = "../../tests/unit/db/password_work/tests.rs"]
mod tests;
