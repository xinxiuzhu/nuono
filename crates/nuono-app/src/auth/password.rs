use unicode_segmentation::UnicodeSegmentation;

pub const MIN_PASSWORD_LEN: usize = 10;
pub const MAX_PASSWORD_LEN: usize = 72;

pub fn password_length_message() -> String {
    format!("密码长度应为 {MIN_PASSWORD_LEN}–{MAX_PASSWORD_LEN} 个字符")
}

pub fn validate_password(password: &str) -> std::result::Result<(), String> {
    let len = UnicodeSegmentation::graphemes(password, true).count();
    if !(MIN_PASSWORD_LEN..=MAX_PASSWORD_LEN).contains(&len) {
        return Err(password_length_message());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_user_perceived_characters() {
        assert!(validate_password(&"a".repeat(MIN_PASSWORD_LEN)).is_ok());
        assert!(validate_password(&"你".repeat(MAX_PASSWORD_LEN)).is_ok());
        assert!(validate_password(&"a".repeat(MIN_PASSWORD_LEN - 1)).is_err());
        assert!(validate_password(&"你".repeat(MAX_PASSWORD_LEN + 1)).is_err());

        let family = "👨‍👩‍👧‍👦";
        assert!(validate_password(&family.repeat(MIN_PASSWORD_LEN)).is_ok());
        assert!(validate_password(&family.repeat(MAX_PASSWORD_LEN)).is_ok());
    }

    #[test]
    fn error_uses_the_shared_character_range() {
        assert_eq!(
            validate_password("short").unwrap_err(),
            password_length_message()
        );
    }
}
