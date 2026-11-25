//! 跨平台通用的强类型按钮定义；各平台特有的协议报文格式由对应适配器负责编码。
pub use nuono_domain::button::{
    ButtonAction, ButtonBuilder, ButtonOne, ButtonPermission, ButtonStyle,
};
