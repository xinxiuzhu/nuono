//! 验证记忆工具在 PostgreSQL 与 SQLite 后端下均严格校验调用方作用域，并拒绝非法修改。
use nuono_app::agent::{
    AgentContext,
    store::AgentMemoryRecord,
    tool::{AgentToolContext, BuiltinToolRegistry, ToolRegistry},
};
use nuono_core::{NuonoError, db::Database, test_support};
use serde_json::{Value, json};

fn context(user: i64, group: Option<i64>) -> AgentContext {
    AgentContext::from_runtime_event(
        42,
        &json!({
            "post_type":"message", "message_type":if group.is_some() {"group"} else {"private"},
            "user_id":user, "group_id":group, "message":"vaultmarker", "raw_message":"vaultmarker",
        }),
    )
    .unwrap()
}

async fn call(
    tools: &ToolRegistry,
    db: &Database,
    caller: &AgentContext,
    name: &str,
    input: Value,
) -> nuono_core::Result<Value> {
    tools
        .get(name)
        .unwrap()
        .call(AgentToolContext::with_db(caller.clone(), db.clone()), input)
        .await
}

fn memory(scope_type: &str, scope_key: String) -> AgentMemoryRecord {
    AgentMemoryRecord {
        scope_type: scope_type.into(),
        scope_key,
        memory_key: "vaultmarker".into(),
        content: "vaultmarker scoped content".into(),
        tags_json: None,
        enabled: true,
        expires_at: Some(chrono::Utc::now() + chrono::Duration::days(1)),
        actor: None,
    }
}

#[tokio::test]
async fn tools_cannot_read_or_mutate_another_user_group_or_session() {
    let db = test_support::test_db(2).await;
    let tools = BuiltinToolRegistry::read_only();
    let caller = context(1001, Some(3001));
    let other = context(2002, Some(3002));
    for (scope_type, own_key, foreign_key) in [
        ("user", caller.user_scope_key(), other.user_scope_key()),
        (
            "group",
            caller.group_scope_key().unwrap(),
            other.group_scope_key().unwrap(),
        ),
        ("session", caller.session_id(), other.session_id()),
    ] {
        let (foreign, _) = db
            .upsert_agent_memory(memory(scope_type, foreign_key.clone()))
            .await
            .unwrap();
        let request = json!({"scope_type":scope_type,"scope_key":foreign_key});
        assert!(
            matches!(
                call(&tools, &db, &caller, "memory.read", request.clone()).await,
                Err(NuonoError::PermissionDenied(_))
            ),
            "foreign {scope_type} must not be readable"
        );
        let mut write = request;
        write["memory_key"] = json!("vaultmarker");
        write["content"] = json!("must not overwrite");
        assert!(
            matches!(
                call(&tools, &db, &caller, "memory.write", write).await,
                Err(NuonoError::PermissionDenied(_))
            ),
            "foreign {scope_type} must not be writable"
        );
        for (name, input) in [
            (
                "memory.update",
                json!({"id":foreign.id,"content":"must not change"}),
            ),
            ("memory.delete", json!({"id":foreign.id})),
        ] {
            assert!(
                call(&tools, &db, &caller, name, input).await.is_err(),
                "{name} foreign {scope_type}"
            );
            assert_eq!(
                db.get_agent_memory(foreign.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .content,
                foreign.content
            );
        }
        let created = call(&tools, &db, &caller, "memory.write", json!({
            "scope_type":scope_type,"scope_key":own_key,"memory_key":"vaultmarker","content":"vaultmarker own content"
        })).await.unwrap();
        let id = created["memory"]["id"]
            .as_str()
            .unwrap()
            .parse::<i64>()
            .unwrap();
        let read = call(
            &tools,
            &db,
            &caller,
            "memory.read",
            json!({"scope_type":scope_type,"scope_key":own_key}),
        )
        .await
        .unwrap();
        assert_eq!(read["memories"][0]["id"], id.to_string());
        call(
            &tools,
            &db,
            &caller,
            "memory.update",
            json!({"id":id,"content":"allowed update"}),
        )
        .await
        .unwrap();
        let deleted = call(&tools, &db, &caller, "memory.delete", json!({"id":id}))
            .await
            .unwrap();
        assert_eq!(deleted["deleted"], true);
    }
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_ids_never_select_a_different_record() {
    let db = test_support::test_db(2).await;
    let tools = BuiltinToolRegistry::read_only();
    let caller = context(1001, None);
    let (saved, _) = db
        .upsert_agent_memory(memory("user", caller.user_scope_key()))
        .await
        .unwrap();
    assert_eq!(saved.id, 1, "the former clamp selected this record");
    for id in [
        json!(0),
        json!(-1),
        json!(i64::MAX),
        json!(u64::MAX),
        Value::Null,
        json!("01"),
        json!("9223372036854775808"),
        json!(1.5),
    ] {
        for name in ["memory.update", "memory.delete"] {
            let result = call(
                &tools,
                &db,
                &caller,
                name,
                json!({"id":id,"content":"must not change"}),
            )
            .await;
            assert!(
                matches!(result, Err(NuonoError::Validation(_))),
                "{name}, id={id}: {result:?}"
            );
            assert_eq!(
                db.get_agent_memory(saved.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .content,
                saved.content
            );
        }
    }
    // 超过 i32 范围的 ID 同样合法，必须精确匹配对应记录，不能因截断或回退而误命中记录 1。
    for id in [json!(2_147_483_648_i64), json!(i64::MAX.to_string())] {
        for name in ["memory.update", "memory.delete"] {
            let result = call(
                &tools,
                &db,
                &caller,
                name,
                json!({"id":id,"content":"must not change"}),
            )
            .await;
            if name == "memory.update" {
                assert!(
                    matches!(result, Err(NuonoError::NotFound(_))),
                    "{name}, id={id}: {result:?}"
                );
            } else {
                assert_eq!(result.unwrap()["deleted"], false);
            }
        }
    }
    assert_eq!(
        db.get_agent_memory(saved.id)
            .await
            .unwrap()
            .unwrap()
            .content,
        saved.content
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_expiry_is_rejected_and_explicit_clear_remains_available() {
    let db = test_support::test_db(2).await;
    let tools = BuiltinToolRegistry::read_only();
    let caller = context(1001, None);
    let (saved, _) = db
        .upsert_agent_memory(memory("user", caller.user_scope_key()))
        .await
        .unwrap();
    for expiry in [
        json!("not-a-date"),
        json!("2026-02-30 12:00:00"),
        json!(true),
        json!(123),
        json!({}),
    ] {
        for (name, input) in [
            ("memory.update", json!({"id":saved.id,"expires_at":expiry})),
            (
                "memory.write",
                json!({"memory_key":"invalid-new","content":"must not save","expires_at":expiry}),
            ),
        ] {
            let result = call(&tools, &db, &caller, name, input).await;
            assert!(
                matches!(result, Err(NuonoError::Validation(_))),
                "{name}, expiry={expiry}: {result:?}"
            );
            assert_eq!(
                db.get_agent_memory(saved.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .expires_at,
                saved.expires_at
            );
            assert_eq!(
                db.list_agent_memories(None, None, None, false, 10)
                    .await
                    .unwrap()
                    .len(),
                1
            );
        }
    }
    call(
        &tools,
        &db,
        &caller,
        "memory.update",
        json!({"id":saved.id,"content":"valid patch"}),
    )
    .await
    .unwrap();
    assert_eq!(
        db.get_agent_memory(saved.id)
            .await
            .unwrap()
            .unwrap()
            .expires_at,
        saved.expires_at
    );
    for clear in [Value::Null, json!(""), json!("   ")] {
        call(
            &tools,
            &db,
            &caller,
            "memory.update",
            json!({"id":saved.id,"expires_at":"2030-01-02 03:04:05"}),
        )
        .await
        .unwrap();
        assert!(
            db.get_agent_memory(saved.id)
                .await
                .unwrap()
                .unwrap()
                .expires_at
                .is_some()
        );
        call(
            &tools,
            &db,
            &caller,
            "memory.update",
            json!({"id":saved.id,"expires_at":clear}),
        )
        .await
        .unwrap();
        assert!(
            db.get_agent_memory(saved.id)
                .await
                .unwrap()
                .unwrap()
                .expires_at
                .is_none()
        );
    }
    db.shutdown().await.unwrap();
}
