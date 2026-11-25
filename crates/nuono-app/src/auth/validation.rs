pub const MIN_USERNAME_LEN: usize = 3;
pub const MAX_USERNAME_LEN: usize = 64;
pub fn validate_username(username: &str) -> std::result::Result<(), String> {
    let len = username.chars().count();
    if len < MIN_USERNAME_LEN {
        return Err(format!("用户名长度至少为 {MIN_USERNAME_LEN} 个字符"));
    }
    if len > MAX_USERNAME_LEN {
        return Err(format!("用户名长度不能超过 {MAX_USERNAME_LEN} 个字符"));
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err("用户名仅允许包含字母、数字、_、- 和 .".to_string());
    }
    Ok(())
}

pub fn validate_password_strength(password: &str) -> std::result::Result<(), String> {
    crate::auth::validate_password(password)
}

/// 结构性邮箱校验。真正的有效性由后续的邮件验证流程确认，这里只挡明显错误的输入。
pub fn validate_email(email: &str) -> std::result::Result<(), String> {
    if email.chars().count() > 320 {
        return Err("邮箱地址过长，请检查后重试".to_string());
    }
    let mut parts = email.split('@');
    let local = parts.next().unwrap_or_default();
    let domain = parts.next().unwrap_or_default();
    if parts.next().is_some() || local.is_empty() || domain.is_empty() {
        return Err("邮箱地址格式无效，示例：nuono@example.com".to_string());
    }
    if !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.') {
        return Err("邮箱域名格式无效，示例：nuono@example.com".to_string());
    }
    if email.chars().any(char::is_control) {
        return Err("邮箱地址不能包含控制字符".to_string());
    }
    if email.chars().any(char::is_whitespace) {
        return Err("邮箱地址不能包含空格".to_string());
    }
    Ok(())
}
