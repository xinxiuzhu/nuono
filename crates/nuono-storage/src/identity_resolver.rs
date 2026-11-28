//! 并发安全的平台身份解析与占位用户懒创建（`IdentityResolver`）。
//!
//! 执行路径：
//! 1. 过滤机器人自身、系统广播与匿名非真人身份；
//! 2. 查进程内有界 LRU 缓存，命中直接返回；
//! 3. 查 `nuono_platform_identities` 并沿 `merged_into_user_id` 解析规范根 `user_id`；
//! 4. 事务内插入 `shadow` 占位账号与平台身份，若命中四元组唯一约束冲突则立即回滚并重查胜出记录，确保零孤儿占位账号。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use nuono_domain::{IdentityKind, IdentityRef};
use sea_orm::{ConnectionTrait, DatabaseTransaction, EntityTrait, Set};

use crate::{
    Database,
    db::{
        platform_identities::{identities, identity_parts, identity_query},
        users::new_shadow_user_active_model,
    },
    entity::users,
    error::{NuonoError, Result},
};

/// 进程内身份解析缓存容量上限。
const IDENTITY_CACHE_CAPACITY: usize = 16_384;
/// 合并重定向链最大跳数（防止异常数据成环导致死循环）。
pub const MAX_CANONICAL_USER_HOPS: usize = 5;

/// 解析后的规范身份锚点。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedIdentity {
    pub identity_id: i64,
    pub user_id: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct IdentityCacheKey {
    pub platform: String,
    pub namespace: String,
    pub identity_kind: String,
    pub native_user_id: String,
}

#[derive(Debug, Default)]
struct LruInner {
    tick: u64,
    by_key: HashMap<IdentityCacheKey, (u64, ResolvedIdentity)>,
    by_tick: BTreeMap<u64, IdentityCacheKey>,
}

/// 线程安全的进程内有界 LRU 身份缓存，支持按 `user_id` 精准剔除（合并、转正、解绑后调用）。
#[derive(Debug)]
pub(crate) struct IdentityCache {
    capacity: usize,
    inner: Mutex<LruInner>,
}

impl IdentityCache {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            capacity: IDENTITY_CACHE_CAPACITY,
            inner: Mutex::new(LruInner::default()),
        })
    }

    pub(crate) fn get(&self, key: &IdentityCacheKey) -> Option<ResolvedIdentity> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (old_tick, value) = *guard.by_key.get(key)?;
        guard.tick = guard.tick.wrapping_add(1);
        let new_tick = guard.tick;
        guard.by_tick.remove(&old_tick);
        guard.by_tick.insert(new_tick, key.clone());
        if let Some(entry) = guard.by_key.get_mut(key) {
            entry.0 = new_tick;
        }
        Some(value)
    }

    pub(crate) fn put(&self, key: IdentityCacheKey, value: ResolvedIdentity) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((old_tick, _)) = guard.by_key.remove(&key) {
            guard.by_tick.remove(&old_tick);
        }
        while guard.by_key.len() >= self.capacity {
            if let Some((_, oldest_key)) = guard.by_tick.pop_first() {
                guard.by_key.remove(&oldest_key);
            } else {
                break;
            }
        }
        guard.tick = guard.tick.wrapping_add(1);
        let tick = guard.tick;
        guard.by_key.insert(key.clone(), (tick, value));
        guard.by_tick.insert(tick, key);
    }

    /// 清除所有映射到指定 `user_id` 的缓存条目（账号合并或解绑提交后立即调用）。
    pub(crate) fn invalidate_user(&self, user_id: i64) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let victims: Vec<(IdentityCacheKey, u64)> = guard
            .by_key
            .iter()
            .filter(|(_, (_, val))| val.user_id == user_id)
            .map(|(k, (tick, _))| (k.clone(), *tick))
            .collect();
        for (key, tick) in victims {
            guard.by_key.remove(&key);
            guard.by_tick.remove(&tick);
        }
    }

    /// 按平台身份四元组清除单条缓存。
    pub(crate) fn invalidate_key(&self, key: &IdentityCacheKey) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((tick, _)) = guard.by_key.remove(key) {
            guard.by_tick.remove(&tick);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .by_key
            .len()
    }
}

/// 判断该平台身份是否属于机器人、系统、匿名或沙盒虚拟负数非真人实体。
pub fn is_non_human_identity(identity: &IdentityRef) -> bool {
    if *identity.kind() != IdentityKind::User {
        return true;
    }
    let raw = identity.value().as_str().trim();
    if raw.is_empty()
        || raw == "0"
        || raw
            .strip_prefix('-')
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    {
        return true;
    }
    let lower = raw.to_ascii_lowercase();
    lower == "anonymous"
        || lower == "system"
        || lower.starts_with("system:")
        || lower.starts_with("bot:")
}

/// 沿 `nuono_users.merged_into_user_id` 解析当前有效的规范根用户 ID（带最大跳数限制防环）。
pub async fn resolve_canonical_user_id_on<C: ConnectionTrait>(
    conn: &C,
    user_id: i64,
) -> Result<i64> {
    if user_id <= 0 {
        return Err(NuonoError::Validation("用户 ID 须为正整数".into()));
    }
    let mut current = user_id;
    let mut visited = [0_i64; MAX_CANONICAL_USER_HOPS + 1];
    for hop in 0..=MAX_CANONICAL_USER_HOPS {
        if visited[..hop].contains(&current) {
            return Err(NuonoError::Conflict("用户合并链存在循环引用".into()));
        }
        visited[hop] = current;
        let row = users::orm::Entity::find_by_id(current)
            .one(conn)
            .await?
            .ok_or_else(|| NuonoError::NotFound(format!("账号不存在: {current}")))?;
        if row.status != "merged" {
            return Ok(row.id);
        }
        let next = row
            .merged_into_user_id
            .ok_or_else(|| NuonoError::Internal("已合并账号缺少目标指向".into()))?;
        if hop == MAX_CANONICAL_USER_HOPS {
            return Err(NuonoError::Conflict(format!(
                "用户合并重定向超过最大跳数上限({MAX_CANONICAL_USER_HOPS})"
            )));
        }
        current = next;
    }
    unreachable!()
}

fn is_unique_violation(error: &sea_orm::DbErr) -> bool {
    if matches!(
        error.sql_err(),
        Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
    ) {
        return true;
    }
    let msg = error.to_string();
    msg.contains("UNIQUE constraint failed")
        || msg.contains("duplicate key value violates unique constraint")
}

/// 在外部已有写事务内解析或懒创建平台身份与占位账号。
///
/// 若当前事务已持有写锁（如 SQLite `BEGIN IMMEDIATE`）则直接幂等查/建；
/// 若外部需要并发防竞态且可独立回滚，请调用 [`Database::resolve_or_provision_identity`]。
pub(crate) async fn resolve_or_provision_on_tx(
    tx: &DatabaseTransaction,
    identity: &IdentityRef,
    nickname_hint: Option<&str>,
) -> Result<ResolvedIdentity> {
    if is_non_human_identity(identity) {
        return Err(NuonoError::Validation(
            "非真人平台身份不允许创建占位用户".into(),
        ));
    }
    let (platform, namespace, kind, native) = identity_parts(identity)?;
    if let Some(existing) = identity_query(identity)?.one(tx).await? {
        let canonical = resolve_canonical_user_id_on(tx, existing.user_id).await?;
        return Ok(ResolvedIdentity {
            identity_id: existing.id,
            user_id: canonical,
        });
    }
    let now = crate::clock::db_now().timestamp_millis();
    let user_id = users::orm::Entity::insert(new_shadow_user_active_model(
        nickname_hint.unwrap_or(""),
        now,
    ))
    .exec(tx)
    .await?
    .last_insert_id;
    let identity_id = identities::Entity::insert(identities::ActiveModel {
        platform: Set(platform),
        namespace: Set(namespace),
        identity_kind: Set(kind),
        native_user_id: Set(native),
        user_id: Set(user_id),
        verified_at_ms: Set(None),
        revision: Set(1),
        created_at_ms: Set(now),
        updated_at_ms: Set(now),
        ..Default::default()
    })
    .exec(tx)
    .await?
    .last_insert_id;
    Ok(ResolvedIdentity {
        identity_id,
        user_id,
    })
}

/// 面向应用层与事件中间件的身份解析服务。
#[derive(Clone, Debug)]
pub struct IdentityResolver {
    db: Database,
}

impl IdentityResolver {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// 交互懒创建入口：LRU 快路径 -> 只读库查 -> 事务内创建并在唯一冲突时回滚重试。
    pub async fn resolve_or_provision(
        &self,
        identity: &IdentityRef,
        nickname_hint: Option<&str>,
    ) -> Result<ResolvedIdentity> {
        self.db
            .resolve_or_provision_identity(identity, nickname_hint)
            .await
    }

    /// 解析已合并用户的规范根 `user_id`（最多 5 跳防环）。
    pub async fn resolve_canonical_user_id(&self, user_id: i64) -> Result<i64> {
        self.db.resolve_canonical_user_id(user_id).await
    }

    /// 清除指定用户关联的所有身份解析缓存。
    pub fn invalidate_user(&self, user_id: i64) {
        self.db.invalidate_identity_cache_for_user(user_id);
    }
}

impl Database {
    /// 获取共享同一连接池与 LRU 缓存的身份解析器句柄。
    pub fn identity_resolver(&self) -> IdentityResolver {
        IdentityResolver::new(self.clone())
    }

    /// 清除内存中指向指定用户的身份解析缓存（合并、转正或解绑提交后调用）。
    pub fn invalidate_identity_cache_for_user(&self, user_id: i64) {
        self.identity_cache.invalidate_user(user_id);
    }

    /// 清除单条平台身份缓存。
    pub(crate) fn invalidate_identity_cache_for_ref(&self, identity: &IdentityRef) {
        if let Ok((platform, namespace, identity_kind, native_user_id)) = identity_parts(identity) {
            self.identity_cache.invalidate_key(&IdentityCacheKey {
                platform,
                namespace,
                identity_kind,
                native_user_id,
            });
        }
    }

    /// 沿 `merged_into_user_id` 解析当前规范根用户 ID（带最大 5 跳防环保护）。
    pub async fn resolve_canonical_user_id(&self, user_id: i64) -> Result<i64> {
        resolve_canonical_user_id_on(self.orm(), user_id).await
    }

    /// 并发安全的平台身份解析与占位用户懒创建：
    /// 1. 过滤非真人身份；
    /// 2. 命中进程内 LRU 缓存直接返回；
    /// 3. 只读查询 `nuono_platform_identities` 并解析规范根 `user_id`；
    /// 4. 开启写事务创建 `shadow` 占位账号与平台身份，若命中四元组唯一冲突则立即回滚当前事务（零孤儿用户）并重查返回胜出记录。
    pub async fn resolve_or_provision_identity(
        &self,
        identity: &IdentityRef,
        nickname_hint: Option<&str>,
    ) -> Result<ResolvedIdentity> {
        if is_non_human_identity(identity) {
            return Err(NuonoError::Validation(
                "非真人平台身份不允许创建占位用户".into(),
            ));
        }
        let (platform, namespace, identity_kind, native_user_id) = identity_parts(identity)?;
        let cache_key = IdentityCacheKey {
            platform: platform.clone(),
            namespace: namespace.clone(),
            identity_kind: identity_kind.clone(),
            native_user_id: native_user_id.clone(),
        };

        // 1. 内存 LRU 快路径
        if let Some(hit) = self.identity_cache.get(&cache_key) {
            return Ok(hit);
        }

        // 2. 数据库只读查询
        if let Some(existing) = identity_query(identity)?.one(self.orm()).await? {
            let canonical = resolve_canonical_user_id_on(self.orm(), existing.user_id).await?;
            let resolved = ResolvedIdentity {
                identity_id: existing.id,
                user_id: canonical,
            };
            self.identity_cache.put(cache_key, resolved);
            return Ok(resolved);
        }

        // 3. 事务内创建 + 唯一冲突回滚重试（防止并发产生孤儿占位用户）
        let tx = self.begin_write().await?;
        if let Some(existing) = identity_query(identity)?.one(&tx).await? {
            let canonical = resolve_canonical_user_id_on(&tx, existing.user_id).await?;
            tx.rollback().await?;
            let resolved = ResolvedIdentity {
                identity_id: existing.id,
                user_id: canonical,
            };
            self.identity_cache.put(cache_key, resolved);
            return Ok(resolved);
        }

        let now = crate::clock::db_now().timestamp_millis();
        let new_user_id = users::orm::Entity::insert(new_shadow_user_active_model(
            nickname_hint.unwrap_or(""),
            now,
        ))
        .exec(&tx)
        .await?
        .last_insert_id;

        let insert_identity = identities::Entity::insert(identities::ActiveModel {
            platform: Set(platform),
            namespace: Set(namespace),
            identity_kind: Set(identity_kind),
            native_user_id: Set(native_user_id),
            user_id: Set(new_user_id),
            verified_at_ms: Set(None),
            revision: Set(1),
            created_at_ms: Set(now),
            updated_at_ms: Set(now),
            ..Default::default()
        })
        .exec(&tx)
        .await;

        match insert_identity {
            Ok(inserted) => {
                tx.commit().await?;
                let resolved = ResolvedIdentity {
                    identity_id: inserted.last_insert_id,
                    user_id: new_user_id,
                };
                self.identity_cache.put(cache_key, resolved);
                Ok(resolved)
            }
            Err(error) if is_unique_violation(&error) => {
                // 并发竞态败出方立即回滚，将刚刚插入的多余 shadow 用户一并回滚，确保零孤儿记录。
                tx.rollback().await?;
                let winner = identity_query(identity)?
                    .one(self.orm())
                    .await?
                    .ok_or_else(|| {
                        NuonoError::DatabaseQuery("平台身份唯一冲突后重查未命中胜出记录".into())
                    })?;
                let canonical = resolve_canonical_user_id_on(self.orm(), winner.user_id).await?;
                let resolved = ResolvedIdentity {
                    identity_id: winner.id,
                    user_id: canonical,
                };
                self.identity_cache.put(cache_key, resolved);
                Ok(resolved)
            }
            Err(error) => {
                let _ = tx.rollback().await;
                Err(error.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_domain::{IdentityScheme, Platform};
    use sea_orm::{ColumnTrait, PaginatorTrait, QueryFilter, Statement};

    fn qq_user(qq: &str) -> IdentityRef {
        IdentityRef::new(
            Platform::OneBot,
            IdentityKind::User,
            IdentityScheme::Native,
            qq,
            None::<String>,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn multiple_shadow_users_with_null_username_key_coexist_and_can_be_disabled() {
        let db = crate::test_support::test_db(2).await;
        let first = db
            .resolve_or_provision_identity(&qq_user("100001"), Some("甲"))
            .await
            .unwrap();
        let second = db
            .resolve_or_provision_identity(&qq_user("100002"), Some("乙"))
            .await
            .unwrap();
        assert_ne!(first.user_id, second.user_id);

        // 确认占位用户可以被独立封禁（account_kind='shadow' 与 status='disabled' 正交）
        db.orm()
            .execute_raw(Statement::from_sql_and_values(
                db.backend(),
                "UPDATE nuono_users SET status = 'disabled' WHERE id = $1",
                [first.user_id.into()],
            ))
            .await
            .unwrap();
        let row = users::orm::Entity::find_by_id(first.user_id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.account_kind, users::ACCOUNT_KIND_SHADOW);
        assert_eq!(row.status, "disabled");
        assert!(row.username_key.is_none());
        // 占位账号不计入正式注册账号数，不会阻塞首启设置向导
        assert_eq!(db.user_count().await.unwrap(), 0);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_20_tasks_provision_exactly_one_shadow_user_and_identity() {
        let db = crate::test_support::test_db(8).await;
        let resolver = db.identity_resolver();
        let identity = qq_user("202699999");

        let mut handles = Vec::with_capacity(20);
        for index in 0..20 {
            let resolver = resolver.clone();
            let identity = identity.clone();
            handles.push(tokio::spawn(async move {
                resolver
                    .resolve_or_provision(&identity, Some(&format!("玩家{index}")))
                    .await
            }));
        }

        let mut results = Vec::with_capacity(20);
        for handle in handles {
            results.push(handle.await.unwrap().unwrap());
        }

        let expected = results[0];
        for item in &results {
            assert_eq!(*item, expected);
        }

        let shadow_count = users::orm::Entity::find()
            .filter(users::orm::Column::AccountKind.eq(users::ACCOUNT_KIND_SHADOW))
            .count(db.orm())
            .await
            .unwrap();
        let identity_count = identities::Entity::find().count(db.orm()).await.unwrap();
        assert_eq!(shadow_count, 1, "并发懒创建绝不能产生孤儿占位用户");
        assert_eq!(identity_count, 1, "并发懒创建绝不能产生重复平台身份");
        assert_eq!(db.identity_cache.len(), 1);

        db.invalidate_identity_cache_for_user(expected.user_id);
        assert_eq!(db.identity_cache.len(), 0);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn non_human_identities_are_rejected_and_canonical_redirect_guards_hops() {
        let db = crate::test_support::test_db(2).await;
        let bot_id = IdentityRef::new(
            Platform::OneBot,
            IdentityKind::Bot,
            IdentityScheme::Native,
            "10000",
            None::<String>,
        )
        .unwrap();
        assert!(
            db.resolve_or_provision_identity(&bot_id, None)
                .await
                .is_err()
        );
        assert!(
            db.resolve_or_provision_identity(&qq_user("0"), None)
                .await
                .is_err()
        );
        assert!(
            db.resolve_or_provision_identity(&qq_user("-4000000000001"), None)
                .await
                .is_err()
        );
        assert_eq!(users::orm::Entity::find().count(db.orm()).await.unwrap(), 0);

        // 构造 2 跳合并链，验证规范根 ID 解析
        let a = db
            .resolve_or_provision_identity(&qq_user("30001"), None)
            .await
            .unwrap();
        let b = db
            .resolve_or_provision_identity(&qq_user("30002"), None)
            .await
            .unwrap();
        let c = db
            .resolve_or_provision_identity(&qq_user("30003"), None)
            .await
            .unwrap();
        db.orm()
            .execute_raw(Statement::from_sql_and_values(
                db.backend(),
                "UPDATE nuono_users SET status='merged', merged_into_user_id=$2, display_name='' WHERE id=$1",
                [a.user_id.into(), b.user_id.into()],
            ))
            .await
            .unwrap();
        db.orm()
            .execute_raw(Statement::from_sql_and_values(
                db.backend(),
                "UPDATE nuono_users SET status='merged', merged_into_user_id=$2, display_name='' WHERE id=$1",
                [b.user_id.into(), c.user_id.into()],
            ))
            .await
            .unwrap();
        assert_eq!(
            db.resolve_canonical_user_id(a.user_id).await.unwrap(),
            c.user_id
        );
        db.shutdown().await.unwrap();
    }
}
