//! 插件与适配器共享的配置 schema 校验函数重导出。
pub use nuono_app::config_validation::{
    config_defaults, validate_config_field_value, validate_config_fields,
    validate_config_ui_credentials, validate_number, validate_ui_value,
};
