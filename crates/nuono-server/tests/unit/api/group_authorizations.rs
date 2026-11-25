use super::*;
#[tokio::test]
async fn independent_group_sources_allow_two_users_without_platform_authority() {
    let f = Fixture::new(true).await;
    let owner = f.session("grant-owner", "owner").await;
    let red = f.session("grant-red", "user").await;
    let blue = f.session("grant-blue", "user").await;
    let gid = f
        .state
        .db
        .ensure_group(
            &nuono_core::db::groups::GroupIdentity::onebot("123456"),
            "shared",
        )
        .await
        .unwrap()
        .id;
    let path = format!("/api/groups/{gid}/authorizations");
    let mut ids = Vec::new();
    for (name, token) in [("grant-red", &red), ("grant-blue", &blue)] {
        let id = f
            .state
            .db
            .find_user_by_username(name)
            .await
            .unwrap()
            .unwrap()
            .id;
        let body = json!({"user_id":id.to_string(),"plugin_id":null,"source_key":name,"valid_from_ms":"0","expires_at_ms":null});
        assert_eq!(
            f.authenticated_json("POST", &path, token, body.clone())
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let (status, result) = f
            .authenticated_json("POST", &path, &owner, body.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        ids.push(result["data"]["id"].as_str().unwrap().to_owned());
        let (_, retry) = f.authenticated_json("POST", &path, &owner, body).await;
        assert_eq!(retry["data"]["id"], *ids.last().unwrap());
        let (_, list) = f.authenticated("/api/groups", token).await;
        assert_eq!(list["data"][0]["group_id"], gid.to_string());
        assert_eq!(
            f.authenticated_json(
                "POST",
                &format!("/api/groups/{gid}/kick/1"),
                token,
                json!({"user_id":"999"})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        f.authenticated_json("DELETE", &format!("{path}/{}", ids[0]), &owner, Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    let (_, red_groups) = f.authenticated("/api/groups", &red).await;
    assert!(red_groups["data"].as_array().unwrap().is_empty());
    let (_, blue_groups) = f.authenticated("/api/groups", &blue).await;
    assert_eq!(blue_groups["data"].as_array().unwrap().len(), 1);
    f.close().await;
}

#[tokio::test]
async fn groups_list_reports_present_bots_and_surfaces_bound_bot_groups_for_users() {
    use sea_orm::{ConnectionTrait, Statement};

    let f = Fixture::new(true).await;
    let owner = f.session("bot-presence-owner", "owner").await;
    let alice = f.session("bot-presence-alice", "user").await;
    let bob = f.session("bot-presence-bob", "user").await;

    let alice_user = f
        .state
        .db
        .find_user_by_username("bot-presence-alice")
        .await
        .unwrap()
        .unwrap();

    let g1 = f
        .state
        .db
        .ensure_group(
            &nuono_core::db::groups::GroupIdentity::onebot("100001"),
            "核心群",
        )
        .await
        .unwrap()
        .id;
    let g2 = f
        .state
        .db
        .ensure_group(
            &nuono_core::db::groups::GroupIdentity::onebot("100002"),
            "测试群",
        )
        .await
        .unwrap()
        .id;

    // 写入机器人昵称并登记两个机器人的在群分布。
    f.state
        .db
        .update_bot_status("200001", "offline")
        .await
        .unwrap();
    f.state
        .db
        .update_bot_nickname("200001", "测试机器人")
        .await
        .unwrap();
    f.state
        .db
        .replace_bot_groups("onebot", "200001", &[g1])
        .await
        .unwrap();
    f.state
        .db
        .replace_bot_groups("onebot", "200002", &[g1, g2])
        .await
        .unwrap();

    // 为普通用户 alice 绑定已验证的机器人 QQ 身份。
    let now = chrono::Utc::now().timestamp_millis();
    f.state
        .db
        .orm()
        .execute_raw(Statement::from_sql_and_values(
            f.state.db.backend(),
            "INSERT INTO nuono_platform_identities \
             (platform, namespace, identity_kind, native_user_id, user_id, verified_at_ms, revision, created_at_ms, updated_at_ms) \
             VALUES ('onebot', '', '\"numeric\"', '200001', $1, $2, 1, $2, $2)",
            [alice_user.id.into(), now.into()],
        ))
        .await
        .unwrap();

    // 管理员视角：每个群都应带出完整的群内机器人清单和解析后的昵称。
    let (status, owner_list) = f.authenticated("/api/groups", &owner).await;
    assert_eq!(status, StatusCode::OK, "{owner_list}");
    let owner_rows = owner_list["data"].as_array().unwrap();
    assert_eq!(owner_rows.len(), 2);
    let target_g1 = g1.to_string();
    let row1 = owner_rows
        .iter()
        .find(|row| row["group_id"] == target_g1.as_str())
        .unwrap();
    let row1_bots = row1["bots"].as_array().unwrap();
    assert_eq!(row1_bots.len(), 2);
    let bot_entry = row1_bots
        .iter()
        .find(|bot| bot["bot_id"] == "200001")
        .unwrap();
    assert_eq!(bot_entry["label"], "测试机器人");
    assert_eq!(bot_entry["platform"], "onebot");

    // 普通用户 alice 视角：即使没有配置授权，也应看到其绑定机器人所在的群，且保持只读边界。
    let (status, alice_list) = f.authenticated("/api/groups", &alice).await;
    assert_eq!(status, StatusCode::OK, "{alice_list}");
    let alice_rows = alice_list["data"].as_array().unwrap();
    assert_eq!(alice_rows.len(), 1);
    assert_eq!(alice_rows[0]["group_id"], g1.to_string());
    assert!(alice_rows[0]["enabled"].is_null());
    assert_eq!(alice_rows[0]["bots"].as_array().unwrap().len(), 2);
    assert_eq!(
        f.authenticated_json(
            "POST",
            &format!("/api/groups/{g1}/toggle"),
            &alice,
            json!({"enabled": false}),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    // 未绑定任何机器人且无授权的普通用户 bob 仍看不到群。
    let (status, bob_list) = f.authenticated("/api/groups", &bob).await;
    assert_eq!(status, StatusCode::OK, "{bob_list}");
    assert!(bob_list["data"].as_array().unwrap().is_empty());

    // 机器人同步变更在群集合后，管理员和普通用户视图应实时反映退群与新入群。
    f.state
        .db
        .replace_bot_groups("onebot", "200001", &[g2])
        .await
        .unwrap();
    let (_, alice_after) = f.authenticated("/api/groups", &alice).await;
    let alice_after_rows = alice_after["data"].as_array().unwrap();
    assert_eq!(alice_after_rows.len(), 1);
    assert_eq!(alice_after_rows[0]["group_id"], g2.to_string());

    f.close().await;
}
