//! 插件商店清单的 ed25519 签名验证。
//!
//! 签的对象是「仓库 URL + 固定 commit」这对键值，不是编译产物：源码插件在启动时
//! 本地编译，二进制内容在安装前根本不存在。签名证明的是「这个 repo 的这个 commit
//! 确实出自运营者发布」，配合 B1 的 commit pin 构成两层供应链约束。
//!
//! 签名的规范消息由 [`canonical_commit_message`] 统一定义：`sign` 子命令与验证端
//! 必须共用同一份拼法，任何一边改动都会破坏另一边 —— 所以两边都只调用这一个函数。

use crate::error::{NuonoError, Result};

/// 签名的规范消息。
pub fn canonical_commit_message(repo_url: &str, commit: &str) -> Vec<u8> {
    format!("{repo_url}\n{commit}").into_bytes()
}

/// 校验 hex 公钥 + hex 签名。返回 bool 而不是 Result：
/// 调用方只需要「能不能装」这一个结论，坏格式、错密钥、被篡改同样都该拒装。
pub fn verify_commit_signature(
    repo_url: &str,
    commit: &str,
    signature_hex: &str,
    public_key_hex: &str,
) -> bool {
    let Ok(sig_bytes) = hex::decode(signature_hex.trim()) else {
        return false;
    };
    let Ok(pk_bytes) = hex::decode(public_key_hex.trim()) else {
        return false;
    };
    let pk_arr: [u8; 32] = match pk_bytes.try_into() {
        Ok(arr) => arr,
        Err(_) => return false,
    };
    let Ok(public_key) = ed25519_dalek::VerifyingKey::from_bytes(&pk_arr) else {
        return false;
    };
    let Ok(signature) = ed25519_dalek::Signature::from_slice(&sig_bytes) else {
        return false;
    };

    use ed25519_dalek::Verifier;
    public_key
        .verify(&canonical_commit_message(repo_url, commit), &signature)
        .is_ok()
}

/// 用 hex 私钥对规范消息签名，返回 hex 签名。`sign manifest` 子命令用。
pub fn sign_commit(repo_url: &str, commit: &str, private_key_hex: &str) -> Result<String> {
    let bytes = hex::decode(private_key_hex.trim())
        .map_err(|e| NuonoError::InvalidInput(format!("私钥 hex 无效: {e}")))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| NuonoError::InvalidInput("私钥必须是 32 字节 hex".into()))?;
    let key = ed25519_dalek::SigningKey::from_bytes(&arr);

    use ed25519_dalek::Signer;
    Ok(hex::encode(
        key.sign(&canonical_commit_message(repo_url, commit))
            .to_bytes(),
    ))
}

/// 生成新密钥对，返回 `(私钥 hex, 公钥 hex)`。`sign keygen` 子命令用。
///
/// 用项目的 `rand` 直接取 32 字节种子再 `from_bytes`，不调 dalek 的 `generate`：
/// 那需要 rand_core 的 RNG trait 版本对齐，跨版本一漂移就连编译都过不去，
/// 而种子字节与 dalek 的生成方式在密码学上等价（都是 32 字节 OS 熵）。
pub fn generate_keypair() -> Result<(String, String)> {
    use rand::TryRng;
    let mut seed = [0u8; 32];
    rand::rngs::SysRng
        .try_fill_bytes(&mut seed)
        .map_err(|e| NuonoError::Internal(format!("操作系统熵源不可用: {e}")))?;
    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    Ok((
        hex::encode(key.to_bytes()),
        hex::encode(key.verifying_key().to_bytes()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_round_trip() {
        let (secret, public) = generate_keypair().unwrap();
        let signature = sign_commit("https://github.com/x/plugin", "abc123", &secret).unwrap();
        assert!(
            verify_commit_signature("https://github.com/x/plugin", "abc123", &signature, &public),
            "同一密钥签出的签名必须验过"
        );
    }

    #[test]
    fn tampered_repo_or_commit_fails_verification() {
        let (secret, public) = generate_keypair().unwrap();
        let signature = sign_commit("https://github.com/x/plugin", "abc123", &secret).unwrap();
        assert!(!verify_commit_signature(
            "https://github.com/evil/plugin",
            "abc123",
            &signature,
            &public
        ));
        assert!(!verify_commit_signature(
            "https://github.com/x/plugin",
            "deadbeef",
            &signature,
            &public
        ));
    }

    #[test]
    fn wrong_key_or_bad_hex_fails_verification() {
        let (secret, _) = generate_keypair().unwrap();
        let (_, other_public) = generate_keypair().unwrap();
        let signature = sign_commit("https://github.com/x/plugin", "abc123", &secret).unwrap();

        assert!(!verify_commit_signature(
            "https://github.com/x/plugin",
            "abc123",
            &signature,
            &other_public
        ));
        assert!(!verify_commit_signature(
            "https://github.com/x/plugin",
            "abc123",
            "not-hex",
            &other_public
        ));
        assert!(!verify_commit_signature(
            "https://github.com/x/plugin",
            "abc123",
            "00",
            &other_public
        ));
    }

    #[test]
    fn bad_private_key_is_rejected_by_signer() {
        assert!(sign_commit("r", "c", "zz").is_err());
        assert!(sign_commit("r", "c", "00").is_err());
    }
}
