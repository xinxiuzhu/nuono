//! 界面与交互语言偏好属于宿主级配置：保存在 `nuono_global_configs` 表中（键前缀 `locale.pref.`），不修改账号表或插件业务数据。
use crate::Database;
use nuono_app::localization::{Locale, LocalePreferenceStore, LocaleScope};
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};

const LOCALE_PREFIX: &str = "locale.pref.";

#[derive(Serialize, Deserialize)]
struct StoredLocalePreference {
    scope_key: String,
    scope: LocaleScope,
    locale: String,
}

fn config_key(scope_key: &str) -> String {
    format!("{LOCALE_PREFIX}{scope_key}")
}

fn read_locale(value: serde_json::Value, expected: &LocaleScope) -> Result<Locale> {
    let record: StoredLocalePreference = serde_json::from_value(value)
        .map_err(|_| NuonoError::ConfigLoad("语言偏好记录格式无效".into()))?;
    if &record.scope != expected || record.scope.canonical_key()? != record.scope_key {
        return Err(NuonoError::ConfigLoad("语言偏好记录的身份不一致".into()));
    }
    Locale::parse_explicit(&record.locale)
}

#[async_trait::async_trait]
impl LocalePreferenceStore for Database {
    async fn get_locale_preference(&self, scope: &LocaleScope) -> Result<Option<Locale>> {
        let key = config_key(&scope.canonical_key()?);
        self.get_global_config(&key)
            .await?
            .map(|value| read_locale(value, scope))
            .transpose()
    }

    async fn set_locale_preference(
        &self,
        scope: &LocaleScope,
        locale: Option<Locale>,
    ) -> Result<()> {
        let scope_key = scope.canonical_key()?;
        let key = config_key(&scope_key);
        match locale {
            None => {
                self.delete_global_config(&key).await?;
            }
            Some(locale) => {
                let payload = serde_json::to_value(StoredLocalePreference {
                    scope_key,
                    scope: scope.clone(),
                    locale: locale.as_str().to_string(),
                })?;
                self.set_global_config(&key, payload).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../tests/unit/localization_tests.rs"]
mod tests;
