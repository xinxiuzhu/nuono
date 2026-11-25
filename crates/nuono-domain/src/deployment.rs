//! 宿主运行时与源码构建工具共用的轻量部署常量与命名规则（不含任何文件 I/O）。
pub const DEFAULT_BUILD_INCREMENTAL: bool = true;

pub fn is_reserved_plugin_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("hub")
}
