//! 本机运维操作（插件商店、数据迁移等）的错误分类包装：
//! 未细分的字符串/I/O 错误默认归为内部错误（500），参数校验失败、状态冲突、资源不存在及上游请求错误则在源头显式分类。
use nuono_core::NuonoError;
#[derive(Debug)]
pub(crate) struct OperationError(pub NuonoError);
pub(crate) type Result<T> = std::result::Result<T, OperationError>;
impl OperationError {
    pub(crate) fn invalid(message: impl ToString) -> Self {
        Self(NuonoError::Validation(message.to_string()))
    }
    pub(crate) fn conflict(message: impl ToString) -> Self {
        Self(NuonoError::Conflict(message.to_string()))
    }
    pub(crate) fn missing(message: impl ToString) -> Self {
        Self(NuonoError::NotFound(message.to_string()))
    }
    pub(crate) fn upstream(message: impl ToString) -> Self {
        Self(NuonoError::Http(message.to_string()))
    }
}
impl std::fmt::Display for OperationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for OperationError {}
impl From<String> for OperationError {
    fn from(error: String) -> Self {
        Self(NuonoError::Internal(error))
    }
}
impl From<&str> for OperationError {
    fn from(error: &str) -> Self {
        error.to_string().into()
    }
}
impl From<NuonoError> for OperationError {
    fn from(error: NuonoError) -> Self {
        Self(error)
    }
}
impl From<std::io::Error> for OperationError {
    fn from(error: std::io::Error) -> Self {
        error.to_string().into()
    }
}
impl From<OperationError> for String {
    fn from(error: OperationError) -> Self {
        error.to_string()
    }
}
