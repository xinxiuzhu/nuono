//! 宿主内置联网搜索与网页抓取工具的配置结构及校验规则。
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebProvider {
    Tavily,
    Exa,
    Firecrawl,
    Jina,
}
impl WebProvider {
    pub const ALL: [Self; 4] = [Self::Tavily, Self::Exa, Self::Firecrawl, Self::Jina];
    pub fn id(self) -> &'static str {
        match self {
            Self::Tavily => "tavily",
            Self::Exa => "exa",
            Self::Firecrawl => "firecrawl",
            Self::Jina => "jina",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Tavily => "Tavily",
            Self::Exa => "Exa",
            Self::Firecrawl => "Firecrawl",
            Self::Jina => "Jina",
        }
    }
    pub fn secret_key(self) -> &'static str {
        match self {
            Self::Tavily => "ai.web.tavily_api_key",
            Self::Exa => "ai.web.exa_api_key",
            Self::Firecrawl => "ai.web.firecrawl_api_key",
            Self::Jina => "ai.web.jina_api_key",
        }
    }
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebStrategy {
    #[default]
    PrimaryOnly,
    Fallback,
    RoundRobin,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchDepth {
    #[default]
    Basic,
    Advanced,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebCredentials {
    pub tavily_api_key: Option<String>,
    pub exa_api_key: Option<String>,
    pub firecrawl_api_key: Option<String>,
    pub jina_api_key: Option<String>,
}
impl std::fmt::Debug for WebCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(WebProvider::ALL.map(|p| (p.id(), self.key(p).is_some())))
            .finish()
    }
}
impl WebCredentials {
    pub fn field_mut(&mut self, provider: WebProvider) -> &mut Option<String> {
        match provider {
            WebProvider::Tavily => &mut self.tavily_api_key,
            WebProvider::Exa => &mut self.exa_api_key,
            WebProvider::Firecrawl => &mut self.firecrawl_api_key,
            WebProvider::Jina => &mut self.jina_api_key,
        }
    }
    pub fn key(&self, provider: WebProvider) -> Option<&str> {
        let value = match provider {
            WebProvider::Tavily => &self.tavily_api_key,
            WebProvider::Exa => &self.exa_api_key,
            WebProvider::Firecrawl => &self.firecrawl_api_key,
            WebProvider::Jina => &self.jina_api_key,
        };
        value.as_deref().filter(|s| !s.trim().is_empty())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchSettings {
    pub enabled: bool,
    pub primary: WebProvider,
    pub strategy: WebStrategy,
    pub fallbacks: Vec<WebProvider>,
    pub timeout_secs: u64,
    pub max_results: usize,
    pub search_depth: SearchDepth,
}
impl Default for WebSearchSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            primary: WebProvider::Tavily,
            strategy: WebStrategy::PrimaryOnly,
            fallbacks: vec![],
            timeout_secs: 15,
            max_results: 5,
            search_depth: SearchDepth::Basic,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebFetchSettings {
    pub enabled: bool,
    pub primary: WebProvider,
    pub strategy: WebStrategy,
    pub fallbacks: Vec<WebProvider>,
    pub timeout_secs: u64,
    pub max_content_chars: usize,
}
impl Default for WebFetchSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            primary: WebProvider::Jina,
            strategy: WebStrategy::PrimaryOnly,
            fallbacks: vec![],
            timeout_secs: 15,
            max_content_chars: 12_000,
        }
    }
}
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSettings {
    pub search: WebSearchSettings,
    pub fetch: WebFetchSettings,
    pub credentials: WebCredentials,
}
impl WebSettings {
    pub fn validate(&self) -> Result<()> {
        for (label, enabled, primary, strategy, fallbacks, timeout) in [
            (
                "联网搜索",
                self.search.enabled,
                self.search.primary,
                self.search.strategy,
                &self.search.fallbacks,
                self.search.timeout_secs,
            ),
            (
                "网页读取",
                self.fetch.enabled,
                self.fetch.primary,
                self.fetch.strategy,
                &self.fetch.fallbacks,
                self.fetch.timeout_secs,
            ),
        ] {
            if !(1..=60).contains(&timeout) {
                return invalid(format!("{label}超时须为 1–60 秒"));
            }
            if fallbacks.len() > 3 {
                return invalid(format!("{label}最多配置 3 个备用源"));
            }
            let mut seen = vec![primary];
            for fallback in fallbacks {
                if seen.contains(fallback) {
                    return invalid(format!("{label}的主用和备用源不能重复"));
                }
                seen.push(*fallback);
            }
            if enabled {
                for source in selected_sources(primary, strategy, fallbacks) {
                    if self.credentials.key(source).is_none() {
                        return invalid(format!("{label}启用前请配置 {} API 密钥", source.name()));
                    }
                }
            }
        }
        if !(1..=20).contains(&self.search.max_results) {
            return invalid("搜索结果数须为 1–20".into());
        }
        if !(1000..=50_000).contains(&self.fetch.max_content_chars) {
            return invalid("网页正文上限须为 1000–50000 字符".into());
        }
        for provider in WebProvider::ALL {
            if let Some(key) = self.credentials.key(provider)
                && (key.len() > 4096 || key.chars().any(char::is_control))
            {
                return invalid(format!("{} API 密钥格式无效", provider.name()));
            }
        }
        Ok(())
    }
    pub fn allows_tool(&self, name: &str) -> bool {
        match name {
            "web.search" => self.search.enabled,
            "web.fetch" => self.fetch.enabled,
            _ => true,
        }
    }
}
pub fn selected_sources(
    primary: WebProvider,
    strategy: WebStrategy,
    fallbacks: &[WebProvider],
) -> Vec<WebProvider> {
    let mut sources = vec![primary];
    if strategy != WebStrategy::PrimaryOnly {
        sources.extend_from_slice(fallbacks);
    }
    sources
}
fn invalid<T>(message: String) -> Result<T> {
    Err(NuonoError::ConfigValidation(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_disabled_and_require_credentials_only_for_active_sources() {
        let mut cfg = WebSettings::default();
        cfg.validate().unwrap();
        assert!(!cfg.allows_tool("web.search"));
        assert!(!cfg.allows_tool("web.fetch"));
        assert!(cfg.allows_tool("memory.read"));
        cfg.search.enabled = true;
        assert!(cfg.validate().is_err());
        cfg.credentials.tavily_api_key = Some("fixture".into());
        cfg.search.fallbacks.push(WebProvider::Exa);
        cfg.validate().unwrap();
        cfg.search.strategy = WebStrategy::Fallback;
        assert!(cfg.validate().is_err());
        cfg.credentials.exa_api_key = Some("fixture".into());
        cfg.validate().unwrap();
        cfg.search.fallbacks.push(WebProvider::Tavily);
        assert!(cfg.validate().is_err());
    }
    #[test]
    fn legacy_defaults_unknown_fields_and_secret_debug_are_safe() {
        let mut cfg: WebSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, WebSettings::default());
        assert!(serde_json::from_str::<WebSettings>(r#"{"unknown":true}"#).is_err());
        cfg.credentials.jina_api_key = Some("never-show-this-key".into());
        assert!(!format!("{cfg:?}").contains("never-show-this-key"));
    }
}
