use super::Database;
use crate::{entity::users::orm as users, error::Result};
use nuono_app::personal_profile::PersonalProfile;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};

impl Database {
    pub async fn personal_profile(&self, user_id: i64) -> Result<PersonalProfile> {
        Ok(users::Entity::find_by_id(user_id)
            .one(self.orm())
            .await?
            .map(|row| PersonalProfile {
                display_name: row.display_name,
                birthday: row.birthday,
                gender: row.gender,
                bio: row.bio,
                use_for_ai: row.use_for_ai,
            })
            .unwrap_or_default())
    }

    pub async fn save_personal_profile(
        &self,
        user_id: i64,
        profile: PersonalProfile,
    ) -> Result<PersonalProfile> {
        let profile = profile.validated()?;
        let tx = self.begin_write().await?;
        super::account_state::locked_user(&tx, user_id).await?;
        users::Entity::update_many()
            .col_expr(
                users::Column::DisplayName,
                Expr::value(profile.display_name.clone()),
            )
            .col_expr(
                users::Column::Birthday,
                Expr::value(profile.birthday.clone()),
            )
            .col_expr(users::Column::Gender, Expr::value(profile.gender.clone()))
            .col_expr(users::Column::Bio, Expr::value(profile.bio.clone()))
            .col_expr(users::Column::UseForAi, Expr::value(profile.use_for_ai))
            .col_expr(
                users::Column::UpdatedAtMs,
                Expr::value(crate::clock::db_now().timestamp_millis()),
            )
            .filter(users::Column::Id.eq(user_id))
            .exec(&tx)
            .await?;
        super::account_state::append_audit_on(
            &tx,
            super::account_state::AuditEvent {
                actor_user_id: Some(user_id),
                actor_plugin_id: None,
                action: "account.profile_updated",
                result: "success",
                subject_user_id: Some(user_id),
                group_id: None,
                operation_id: None,
                details: serde_json::json!({}),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn personal_profile_is_account_scoped_and_deletable() {
        let db = crate::test_support::test_db(2).await;
        let mut ids = Vec::new();
        for name in ["profile-alice", "profile-bob"] {
            ids.push(
                db.create_user(crate::db::NewUser {
                    username: name.into(),
                    email: None,
                    password: "profile-test-password".into(),
                    role: if name == "profile-bob" {
                        "owner"
                    } else {
                        "user"
                    }
                    .into(),
                    status: "active".into(),
                    email_verified: true,
                })
                .await
                .unwrap()
                .id,
            );
        }
        let saved = db
            .save_personal_profile(
                ids[0],
                PersonalProfile {
                    display_name: "Alice".into(),
                    birthday: "2000-01-01".into(),
                    use_for_ai: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(db.personal_profile(ids[0]).await.unwrap(), saved);
        assert_eq!(
            db.personal_profile(ids[1]).await.unwrap(),
            PersonalProfile::default()
        );
        db.save_personal_profile(ids[0], PersonalProfile::default())
            .await
            .unwrap();
        assert_eq!(
            db.personal_profile(ids[0]).await.unwrap(),
            PersonalProfile::default()
        );
        let owner = db.find_user_by_id(ids[1]).await.unwrap().unwrap();
        nuono_app::accounts::AccountService::new(&db)
            .delete(&nuono_app::auth::CallerIdentity::from_user(&owner), ids[0])
            .await
            .unwrap();
        assert_eq!(
            db.find_user_by_id(ids[0]).await.unwrap().unwrap().status,
            "deleted"
        );
        assert_eq!(
            db.personal_profile(ids[0]).await.unwrap(),
            PersonalProfile::default()
        );
        db.shutdown().await.unwrap();
    }
}
