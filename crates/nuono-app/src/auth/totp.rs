//! RFC 6238 TOTP（HMAC-SHA1 / 6 位 / 30 秒）的最小实现。
//!
//! 没有引 totp crate：核心就是 HMAC + 动态截断，几十行代码，依赖只加 `sha1`（与现有
//! `sha2`/`hmac` 同一 digest 版本族）。用 RFC 6238 附录 B 的官方测试向量做回归。
//!
//! # secret 的存储（见 `db/users.rs`）
//!
//! TOTP secret 必须可逆才能验码，所以以 base32 明文落库。威胁模型要讲清楚：数据库
//! 被读时攻击者拿到的是 secret 而非可用凭据 —— 登录仍要先过 bcrypt 密码这道门，
//! secret 单独不能登录；用户换设备/重装验证器由恢复码兜底，恢复码只存 SHA-256。

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

use super::constant_time_eq;

type HmacSha1 = Hmac<Sha1>;

pub const TOTP_DIGITS: u32 = 6;
pub const TOTP_STEP_SECS: u64 = 30;
/// 每次开启/重置发放的恢复码数量。
pub const RECOVERY_CODE_COUNT: usize = 10;

const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// 生成 160 bit 随机 secret，编码为 base32（32 字符，无填充）。
pub fn generate_secret() -> String {
    use rand::TryRng;
    let mut bytes = [0u8; 20];
    // 熵源失败意味着系统熵不可用，此时不能退回弱随机来生成两步验证凭据。
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .expect("操作系统熵源不可用，无法生成 TOTP secret");
    base32_encode(&bytes)
}

pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32_ALPHABET[((buffer >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32_ALPHABET[((buffer << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

pub fn base32_decode(input: &str) -> Option<Vec<u8>> {
    let mut buffer = 0u32;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(input.len() * 5 / 8);
    // 容忍空白与填充；RFC 4648 字母表是 A-Z 与 2-7，其余字符一律拒绝。
    for ch in input.chars().filter(|c| !c.is_whitespace() && *c != '=') {
        let c = ch.to_ascii_uppercase();
        let value: u32 = match c {
            'A'..='Z' => u32::from(c as u8 - b'A'),
            '2'..='7' => u32::from(c as u8 - b'2') + 26,
            _ => return None,
        };
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

fn counter(unix_secs: u64) -> u64 {
    unix_secs / TOTP_STEP_SECS
}

fn code_for_counter(key: &[u8], counter: u64) -> String {
    let mut mac = HmacSha1::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[19] & 0x0f) as usize;
    let bin = u32::from_be_bytes([
        digest[offset],
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]) & 0x7fff_ffff;
    let code = bin % 10u32.pow(TOTP_DIGITS);
    format!("{code:0width$}", width = TOTP_DIGITS as usize)
}

/// 校验一个 TOTP 码（±1 步窗口），返回命中的时间步。
///
/// 所有候选都做常量时间比较，不短路。返回 `Some(counter)` 供调用方做重放保护：
/// 按 RFC 6238 §5.2，命中的 counter 必须严格大于该账号已记录的最近成功值
///（见 `db::users::consume_totp_step`），否则同一个码在窗口内可被无限重放。
pub fn validate_code_counter(secret: &str, code: &str, now_unix: u64) -> Option<u64> {
    let key = base32_decode(secret)?;
    let trimmed: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    if trimmed.len() != TOTP_DIGITS as usize {
        return None;
    }
    let c = counter(now_unix);
    let mut candidates = vec![(c, code_for_counter(&key, c))];
    if c > 0 {
        candidates.push((c - 1, code_for_counter(&key, c - 1)));
    }
    candidates.push((c + 1, code_for_counter(&key, c + 1)));
    candidates
        .into_iter()
        .fold(None, |hit, (counter, candidate)| {
            if constant_time_eq(candidate.as_bytes(), trimmed.as_bytes()) {
                Some(counter)
            } else {
                hit
            }
        })
}

/// 校验一个 TOTP 码（±1 步窗口）。所有候选都做常量时间比较，不短路。
pub fn validate_code(secret: &str, code: &str, now_unix: u64) -> bool {
    validate_code_counter(secret, code, now_unix).is_some()
}

/// 生成 otpauth:// 标准 URI，供验证器扫码/导入。
pub fn otpauth_uri(issuer: &str, account: &str, secret: &str) -> String {
    format!(
        "otpauth://totp/{issuer}:{account}?secret={secret}&issuer={issuer}&algorithm=SHA1&digits={TOTP_DIGITS}&period={TOTP_STEP_SECS}"
    )
}

/// 生成 `count` 个恢复码（格式 `XXXXX-XXXXX`，base32 字符集）。
pub fn generate_recovery_codes() -> Vec<String> {
    use rand::TryRng;
    let mut codes = Vec::with_capacity(RECOVERY_CODE_COUNT);
    for _ in 0..RECOVERY_CODE_COUNT {
        let mut bytes = [0u8; 6];
        rand::rngs::SysRng
            .try_fill_bytes(&mut bytes)
            .expect("操作系统熵源不可用，无法生成恢复码");
        let encoded = base32_encode(&bytes); // 10 字符
        let mut code = String::with_capacity(11);
        for (i, ch) in encoded.chars().enumerate() {
            if i == 5 {
                code.push('-');
            }
            code.push(ch);
        }
        codes.push(code);
    }
    codes
}

/// 恢复码的存储哈希：归一化（去空白与破折号、统一大写）后 SHA-256（十六进制）。
///
/// 用户手输时大小写不一致不该导致误判；生成端本来就是大写 base32，折叠大小写
/// 不降低熵。
pub fn recovery_code_hash(code: &str) -> String {
    use sha2::{Digest, Sha256};
    let normalized: String = code
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    hex::encode(Sha256::digest(normalized.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_roundtrip() {
        for bytes in [b"hello".as_slice(), &[0u8; 20], &[0xff, 0x00, 0xaa]] {
            let encoded = base32_encode(bytes);
            assert_eq!(
                base32_decode(&encoded).unwrap(),
                bytes,
                "encoded: {encoded}"
            );
        }
        // 小写与填充容忍
        assert_eq!(
            base32_decode("jbswy3dp").unwrap(),
            b"Hello".to_vec(),
            "RFC 4648 测试向量"
        );
    }

    #[test]
    fn base32_decode_rejects_illegal_characters() {
        assert!(base32_decode("ABC1").is_none());
        assert!(base32_decode("ABC!").is_none());
    }

    /// RFC 6238 附录 B 的 SHA1 测试向量（8 位截断为 6 位）。
    ///
    /// 注意 T=1111111111 这一行是 `14050471`（6 位 050471）——网上流传的
    /// 05047187 是误抄，RFC 原文即 14050471。
    #[test]
    fn rfc6238_sha1_vectors() {
        let key = b"12345678901234567890"; // ASCII 20 字节
        let cases: [(u64, &str); 6] = [
            (59, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ];
        for (time, expected) in cases {
            assert_eq!(
                code_for_counter(key, counter(time)),
                expected,
                "time = {time}"
            );
        }
    }

    #[test]
    fn validate_code_accepts_current_and_adjacent_steps() {
        let secret = "JBSWY3DPEHPK3PXP"; // "Hello!\xde\xad\xbe\xef"
        let now = 1_234_567_890;
        let current = code_for_counter(&base32_decode(secret).unwrap(), counter(now));
        assert!(validate_code(secret, &current, now));
        assert!(
            validate_code(secret, &current, now + 29),
            "同一时间窗内应有效"
        );
        assert!(validate_code(secret, &current, now + 30), "±1 步窗口应放行");
        assert!(!validate_code(secret, &current, now + 90), "窗口外应拒绝");
        assert!(!validate_code(secret, "000000", now + 120));
        assert!(!validate_code(secret, "abc", now));
    }

    /// 命中时返回的时间步必须能区分上一窗/当前/下一窗 —— 重放保护靠它单调推进。
    #[test]
    fn validate_code_counter_reports_the_matched_step() {
        let secret = "JBSWY3DPEHPK3PXP";
        let now = 1_234_567_890u64;
        let c = counter(now);

        // 当前步的码 → Some(c)。
        let current = code_for_counter(&base32_decode(secret).unwrap(), c);
        assert_eq!(validate_code_counter(secret, &current, now), Some(c));

        // 同一个码在下一窗被解释为「上一窗命中」→ Some(c)，但调用方应因
        // counter 不大于已记录值而拒绝（这正是重放保护要挡的场景）。
        assert_eq!(
            validate_code_counter(secret, &current, now + TOTP_STEP_SECS),
            Some(c)
        );

        // 下一窗的码提前使用 → 命中 c+1，允许（单调记录会推进到 c+1）。
        let next = code_for_counter(&base32_decode(secret).unwrap(), c + 1);
        assert_eq!(validate_code_counter(secret, &next, now), Some(c + 1));

        assert_eq!(validate_code_counter(secret, "000000", now), None);
    }

    #[test]
    fn recovery_codes_are_unique_and_hashable() {
        let codes = generate_recovery_codes();
        assert_eq!(codes.len(), RECOVERY_CODE_COUNT);
        let mut seen = std::collections::HashSet::new();
        for code in &codes {
            assert!(seen.insert(code.clone()), "恢复码重复: {code}");
            assert_eq!(code.len(), 11, "格式 XXXXX-XXXXX: {code}");
            assert_eq!(recovery_code_hash(code).len(), 64);
        }
        // 归一化：破折号与空白不影响哈希
        let code = "ABCD-EFGH-12";
        assert_eq!(recovery_code_hash(code), recovery_code_hash("abcdefgh12"));
    }
}
