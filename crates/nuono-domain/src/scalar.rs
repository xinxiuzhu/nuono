//! 无损传输的 64 位整数序列化工具与严格跨平台 JSON 解析器。

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::Value;
use std::fmt;

/// 将 `i64` 序列化为十进制字符串；反序列化时若输入为 JSON 数字，则必须处于 JavaScript 安全整数范围内。
pub mod i64_string {
    use super::*;
    pub fn serialize<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        struct Integer;
        impl de::Visitor<'_> for Integer {
            type Value = i64;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a decimal i64 string")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<i64, E> {
                let parsed = value.parse::<i64>().map_err(E::custom)?;
                if parsed.to_string() != value {
                    return Err(E::custom("noncanonical integer"));
                }
                Ok(parsed)
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<i64, E> {
                if !(-9_007_199_254_740_991..=9_007_199_254_740_991).contains(&value) {
                    return Err(E::custom("large integers must be decimal strings"));
                }
                Ok(value)
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<i64, E> {
                self.visit_i64(i64::try_from(value).map_err(E::custom)?)
            }
        }
        deserializer.deserialize_any(Integer)
    }
}

/// 可选的十进制字符串整数序列化模块，严格区分 JSON `null` 与数值 `0`。
pub mod optional_i64_string {
    use super::*;
    pub fn serialize<S: Serializer>(value: &Option<i64>, serializer: S) -> Result<S::Ok, S::Error> {
        value.map(|v| v.to_string()).serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<i64>, D::Error> {
        #[derive(Deserialize)]
        struct Wire(#[serde(with = "super::i64_string")] i64);
        Ok(Option::<Wire>::deserialize(deserializer)?.map(|v| v.0))
    }
}

/// 严格解析 JSON：拒绝重复键、包含 NUL 字符的字符串以及超出有符号 64 位范围的整数。
pub fn parse_json(text: &str) -> Result<Value, serde_json::Error> {
    // 在交给 serde_json 解析前先扫描整数字面量，防止超大整数被静默降级转换为 f64。
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut quoted = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            index += 1;
        } else if byte == b'"' {
            quoted = true;
            index += 1;
        } else if byte == b'-' || byte.is_ascii_digit() {
            let start = index;
            while index < bytes.len()
                && matches!(bytes[index], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
            {
                index += 1;
            }
            let number = &text[start..index];
            if !number.contains(['.', 'e', 'E']) && number.parse::<i64>().is_err() {
                return Err(<serde_json::Error as de::Error>::custom(
                    "JSON integer exceeds signed 64 bits",
                ));
            }
        } else {
            index += 1;
        }
    }
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = StrictValue::deserialize(&mut deserializer)?.0;
    deserializer.end()?;
    Ok(value)
}

struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Strict;
        impl<'de> de::Visitor<'de> for Strict {
            type Value = StrictValue;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("portable JSON")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                self.visit_unit()
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                self.visit_i64(i64::try_from(value).map_err(E::custom)?)
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|v| StrictValue(Value::Number(v)))
                    .ok_or_else(|| E::custom("nonfinite number"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value.contains('\0') {
                    return Err(E::custom("NUL is not portable text"));
                }
                Ok(StrictValue(value.into()))
            }
            fn visit_seq<A: de::SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<StrictValue>()? {
                    values.push(value.0);
                }
                Ok(StrictValue(Value::Array(values)))
            }
            fn visit_map<A: de::MapAccess<'de>>(
                self,
                mut object: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = object.next_key::<String>()? {
                    if key.contains('\0') || values.contains_key(&key) {
                        return Err(de::Error::custom("duplicate or invalid JSON key"));
                    }
                    values.insert(key, object.next_value::<StrictValue>()?.0);
                }
                Ok(StrictValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(Strict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_json_preserves_large_signed_integers_and_rejects_ambiguous_values() {
        let value = parse_json(r#"{"n":9007199254740993,"x":false,"a":[],"nil":null}"#).unwrap();
        assert_eq!(value["n"].as_i64(), Some(9_007_199_254_740_993));
        for invalid in [
            r#"{"a":0,"a":1}"#,
            r#"{"a":{"x":0,"x":1}}"#,
            "9223372036854775808",
            "18446744073709551616",
            r#""\u0000""#,
        ] {
            assert!(parse_json(invalid).is_err(), "{invalid}");
        }
    }
}

pub mod i64_strings {
    use super::*;
    pub fn serialize<S: Serializer>(values: &[i64], serializer: S) -> Result<S::Ok, S::Error> {
        values
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<i64>, D::Error> {
        #[derive(Deserialize)]
        struct Item(#[serde(with = "super::i64_string")] i64);
        Ok(Vec::<Item>::deserialize(deserializer)?
            .into_iter()
            .map(|item| item.0)
            .collect())
    }
}

pub mod utc_millis {
    use super::*;
    pub fn serialize<S: Serializer>(
        value: &chrono::DateTime<chrono::Utc>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        i64_string::serialize(&value.timestamp_millis(), serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<chrono::DateTime<chrono::Utc>, D::Error> {
        chrono::DateTime::from_timestamp_millis(i64_string::deserialize(deserializer)?)
            .ok_or_else(|| de::Error::custom("UTC milliseconds out of range"))
    }
}
pub mod optional_utc_millis {
    use super::*;
    pub fn serialize<S: Serializer>(
        value: &Option<chrono::DateTime<chrono::Utc>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        optional_i64_string::serialize(&value.map(|value| value.timestamp_millis()), serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, D::Error> {
        optional_i64_string::deserialize(deserializer)?
            .map(|value| {
                chrono::DateTime::from_timestamp_millis(value)
                    .ok_or_else(|| de::Error::custom("UTC milliseconds out of range"))
            })
            .transpose()
    }
}
/// 支持三态更新的毫秒时间戳字段：字段缺失表示不修改，JSON `null` 表示清空，有值表示更新。
pub mod nullable_utc_millis {
    use super::*;
    pub fn serialize<S: Serializer>(
        value: &Option<Option<chrono::DateTime<chrono::Utc>>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        super::optional_utc_millis::serialize(&value.flatten(), serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Option<chrono::DateTime<chrono::Utc>>>, D::Error> {
        super::optional_utc_millis::deserialize(deserializer).map(Some)
    }
}
/// 从动态 JSON 值中安全读取 `i64` ID，拒绝超出 JavaScript 安全整数范围的裸数字以防精度丢失。
pub fn wire_i64(value: &Value) -> Option<i64> {
    #[derive(Deserialize)]
    struct Number(#[serde(with = "i64_string")] i64);
    serde_json::from_value::<Number>(value.clone())
        .ok()
        .map(|value| value.0)
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Record {
        #[serde(with = "i64_string")]
        id: i64,
        #[serde(with = "utc_millis")]
        updated_at: chrono::DateTime<chrono::Utc>,
    }
    #[test]
    fn large_ids_round_trip_as_strings_without_float_conversion() {
        let original = Record {
            id: 9_007_199_254_740_993,
            updated_at: chrono::DateTime::from_timestamp_millis(1_794_737_600_123).unwrap(),
        };
        let wire = serde_json::to_value(&original).unwrap();
        assert_eq!(wire["id"], "9007199254740993");
        assert_eq!(wire["updated_at"], "1794737600123");
        assert_eq!(serde_json::from_value::<Record>(wire).unwrap(), original);
        assert_eq!(
            wire_i64(&serde_json::json!(9_007_199_254_740_993_i64)),
            None
        );
        assert_eq!(
            wire_i64(&serde_json::json!("9007199254740993")),
            Some(9_007_199_254_740_993)
        );
    }
}
