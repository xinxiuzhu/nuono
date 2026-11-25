//! 插件数据表归属与列级访问策略解析（仅在表归属唯一且非核心表时放行插件管理声明）。
use nuono_core::db::{DatabaseTablePolicy, TableInspection, TableOwner};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct PluginPolicies {
    pub owners: BTreeMap<String, BTreeSet<String>>,
    pub tables: BTreeMap<String, DatabaseTablePolicy>,
}

impl PluginPolicies {
    pub async fn load(state: &crate::AppState) -> anyhow::Result<Self> {
        Ok(Self::resolve(
            state
                .manager
                .plugin_host
                .database_management_declarations()
                .await?,
        ))
    }

    fn resolve(declarations: Vec<(String, Vec<String>, Vec<DatabaseTablePolicy>)>) -> Self {
        let mut owners = BTreeMap::<String, BTreeSet<String>>::new();
        for (plugin, names, _) in &declarations {
            for name in names {
                owners
                    .entry(name.clone())
                    .or_default()
                    .insert(plugin.clone());
            }
        }
        let mut candidates = BTreeMap::<String, Vec<DatabaseTablePolicy>>::new();
        for (plugin, _, policies) in declarations {
            for policy in policies {
                let valid_owner = owners
                    .get(&policy.table)
                    .is_some_and(|names| names.len() == 1 && names.contains(&plugin));
                if valid_owner && TableOwner::for_table(&policy.table) != TableOwner::Core {
                    candidates
                        .entry(policy.table.clone())
                        .or_default()
                        .push(policy);
                }
            }
        }
        let tables = candidates
            .into_iter()
            .filter_map(|(name, mut policies)| {
                // 同一张表存在重复或非法的策略声明时，直接拒绝开放该表的管理访问。
                if policies.len() != 1 || policies[0].validate().is_err() {
                    return None;
                }
                Some((name, policies.remove(0)))
            })
            .collect();
        Self { owners, tables }
    }

    pub fn policy(&self, table: &str) -> Option<&DatabaseTablePolicy> {
        self.tables.get(table)
    }

    pub fn assign_owners(&self, tables: &mut [TableInspection]) {
        for table in tables {
            if table.owner == TableOwner::Core {
                continue;
            }
            if let Some(owners) = self.owners.get(&table.name)
                && owners.len() == 1
            {
                table.owner = TableOwner::Plugin {
                    name: owners.first().unwrap().clone(),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_core::db::{DatabaseColumnPolicy, DatabaseFieldType};
    fn policy(table: &str) -> DatabaseTablePolicy {
        DatabaseTablePolicy::read(
            table,
            vec![DatabaseColumnPolicy::edit("name", DatabaseFieldType::Text)],
        )
        .manage()
    }

    #[test]
    fn ownership_conflicts_duplicates_and_core_claims_never_grant_access() {
        let rules = PluginPolicies::resolve(vec![
            (
                "first".into(),
                vec![
                    "owned".into(),
                    "conflict".into(),
                    "duplicate".into(),
                    "nuono_users".into(),
                ],
                vec![
                    policy("owned"),
                    policy("conflict"),
                    policy("duplicate"),
                    policy("duplicate"),
                    policy("nuono_users"),
                    policy("unclaimed"),
                ],
            ),
            (
                "second".into(),
                vec!["conflict".into()],
                vec![policy("conflict")],
            ),
        ]);
        assert!(rules.policy("owned").is_some());
        for name in [
            "conflict",
            "duplicate",
            "nuono_users",
            "unclaimed",
            "owned_old",
        ] {
            assert!(rules.policy(name).is_none(), "{name}");
        }
    }
}
