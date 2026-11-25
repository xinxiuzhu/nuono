//! 插件内嵌 WebUI 页面的平台无关声明定义。

use super::PluginWebAccessPolicy;
use anyhow::{Result, ensure};
use std::collections::HashSet;

/// 随插件编译打包的前端静态资源文件（字节数据归插件二进制所有）。
#[derive(Clone)]
pub struct PluginWebAsset {
    /// 相对于页面资源根目录的路径（区分大小写，不带前导斜杠 `/`）。
    pub path: &'static str,
    /// 浏览器 MIME 类型；WebAssembly 模块必须设为 `application/wasm`。
    pub content_type: &'static str,
    /// 不可变的静态文件内容，通常在编译期通过 `include_bytes!` 嵌入。
    pub data: &'static [u8],
}

impl std::fmt::Debug for PluginWebAsset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginWebAsset")
            .field("path", &self.path)
            .field("content_type", &self.content_type)
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl PluginWebAsset {
    /// 声明一个静态资源文件（通常在编译期通过 `include_bytes!` 嵌入字节内容）。
    pub const fn new(path: &'static str, content_type: &'static str, data: &'static [u8]) -> Self {
        Self {
            path,
            content_type,
            data,
        }
    }
}

/// 插件 WebUI 页面在宿主控制台中的导航入口位置。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginWebPagePlacement {
    /// 仅在已启用插件的详情页中显示入口按钮。
    #[default]
    PluginDetail,
    /// 除了插件详情页外，同时在主导航栏（Dock）中显示快捷入口。
    Dock,
}

/// 插件独立构建的前端页面声明；默认仅限运行时管理员访问。
#[derive(Debug, Clone)]
pub struct PluginWebPage {
    /// 插件内唯一的稳定 ASCII 页面标识。
    pub id: String,
    /// 面向运行时管理员展示的导航标题。
    pub title: String,
    /// 面向普通已登录用户展示的可选导航标题。
    pub user_title: Option<String>,
    /// 语义化图标名称；若宿主不支持该图标则回退为默认插件图标。
    pub icon: String,
    /// 页面访问权限策略；激活校验阶段拒绝 `Public`（不允许完全公开未登录访问）。
    pub access: PluginWebAccessPolicy,
    /// 页面在控制台中的导航入口展示位置。
    pub placement: PluginWebPagePlacement,
    /// 相对于 `assets` 列表的入口 HTML 文件路径；默认为 `index.html`。
    pub entry: String,
    /// 页面完整静态资源包（必须包含入口 HTML 文件及其引用的所有静态依赖）。
    pub assets: Vec<PluginWebAsset>,
}

impl PluginWebPage {
    /// 声明一个默认以 `index.html` 为入口的管理员页面。
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        assets: Vec<PluginWebAsset>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            user_title: None,
            icon: "puzzle".into(),
            access: PluginWebAccessPolicy::ManageRuntime,
            placement: PluginWebPagePlacement::PluginDetail,
            entry: "index.html".into(),
            assets,
        }
    }

    /// 设置允许访问该页面的用户权限范围（所有已登录用户或仅限运行时管理员）。
    pub fn with_access(mut self, access: PluginWebAccessPolicy) -> Self {
        self.access = access;
        self
    }

    /// 设置页面在控制台中的导航入口位置（例如需要高频直达时可设为 `Dock`）。
    pub fn with_placement(mut self, placement: PluginWebPagePlacement) -> Self {
        self.placement = placement;
        self
    }

    /// 为非管理员的普通登录用户指定专门的导航标题。
    pub fn with_user_title(mut self, title: impl Into<String>) -> Self {
        self.user_title = Some(title.into());
        self
    }

    /// 设置语义化图标名称（如 `receipt`、`chart` 或 `settings`）。
    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = icon.into();
        self
    }

    /// 在执行插件生命周期回调前，校验全部页面声明的合法性。
    ///
    /// # Errors
    /// 若存在非法或重复路径、公开未登录页面（`Public`）、缺失入口 HTML 或资源总大小超限，则返回错误。
    pub fn validate_all(pages: &[Self]) -> Result<()> {
        ensure!(
            pages.len() <= 16,
            "a plugin may declare at most 16 WebUI pages"
        );
        let mut ids = HashSet::new();
        let mut total = 0usize;
        for page in pages {
            ensure!(
                valid_id(&page.id) && ids.insert(&page.id),
                "invalid or duplicate WebUI page id"
            );
            ensure!(
                !page.title.trim().is_empty() && page.title.len() <= 128,
                "invalid WebUI page title"
            );
            ensure!(
                page.user_title
                    .as_ref()
                    .is_none_or(|title| !title.trim().is_empty() && title.len() <= 128),
                "invalid WebUI customer title"
            );
            ensure!(valid_id(&page.icon), "invalid WebUI icon identifier");
            ensure!(
                page.access != PluginWebAccessPolicy::Public,
                "WebUI pages require an authenticated session"
            );
            ensure!(
                !page.assets.is_empty() && page.assets.len() <= 512,
                "a WebUI page requires 1 to 512 assets"
            );
            ensure!(valid_asset_path(&page.entry), "invalid WebUI entry path");
            let mut paths = HashSet::new();
            for asset in &page.assets {
                ensure!(
                    valid_asset_path(asset.path) && paths.insert(asset.path),
                    "invalid or duplicate WebUI asset path"
                );
                ensure!(
                    !asset.content_type.is_empty()
                        && asset.content_type.len() <= 128
                        && !asset.content_type.bytes().any(|b| b.is_ascii_control()),
                    "invalid WebUI asset content type"
                );
                total = total
                    .checked_add(asset.data.len())
                    .ok_or_else(|| anyhow::anyhow!("WebUI asset size overflow"))?;
                ensure!(
                    total <= 64 * 1024 * 1024,
                    "plugin WebUI assets exceed 64 MiB"
                );
            }
            ensure!(
                page.assets
                    .iter()
                    .any(|asset| asset.path == page.entry
                        && asset.content_type.starts_with("text/html")),
                "WebUI entry must be a declared HTML asset"
            );
        }
        Ok(())
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn valid_asset_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path
            .bytes()
            .any(|b| b.is_ascii_control() || matches!(b, b'\\' | b'%' | b'?' | b'#' | b':'))
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

#[cfg(test)]
#[path = "../../tests/unit/plugin/web_page.rs"]
mod tests;
