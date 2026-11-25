//! UTC 时间戳统一封装：在 PostgreSQL 与 SQLite 双后端均以有符号 64 位 Unix 毫秒（`i64`）持久化。
use chrono::{DateTime, Utc};
use sea_orm::{
    ColIdx, QueryResult, TryGetError, TryGetable, Value,
    sea_query::{ArrayType, ColumnType, Nullable, ValueType, ValueTypeErr},
};

/// 数据库存储层时间戳表示（精确到毫秒）；人类可读的时间格式化应在展示层边界完成。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcMillis(DateTime<Utc>);
impl UtcMillis {
    pub fn from_millis(value: i64) -> Option<Self> {
        DateTime::from_timestamp_millis(value).map(Self)
    }
    pub fn signed_duration_since(self, other: impl Into<DateTime<Utc>>) -> chrono::Duration {
        self.0.signed_duration_since(other.into())
    }
    pub fn as_datetime(self) -> DateTime<Utc> {
        self.0
    }
}
impl std::ops::Deref for UtcMillis {
    type Target = DateTime<Utc>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl From<DateTime<Utc>> for UtcMillis {
    fn from(value: DateTime<Utc>) -> Self {
        Self(
            DateTime::from_timestamp_millis(value.timestamp_millis())
                .expect("a chrono instant fits the millisecond range"),
        )
    }
}
impl From<UtcMillis> for DateTime<Utc> {
    fn from(value: UtcMillis) -> Self {
        value.0
    }
}
impl std::fmt::Display for UtcMillis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.timestamp_millis().fmt(f)
    }
}
impl std::ops::Add<chrono::Duration> for UtcMillis {
    type Output = Self;
    fn add(self, rhs: chrono::Duration) -> Self {
        (self.0 + rhs).into()
    }
}
impl std::ops::Sub<chrono::Duration> for UtcMillis {
    type Output = Self;
    fn sub(self, rhs: chrono::Duration) -> Self {
        (self.0 - rhs).into()
    }
}
impl std::ops::Sub<UtcMillis> for UtcMillis {
    type Output = chrono::Duration;
    fn sub(self, rhs: Self) -> Self::Output {
        self.0 - rhs.0
    }
}
impl PartialEq<DateTime<Utc>> for UtcMillis {
    fn eq(&self, other: &DateTime<Utc>) -> bool {
        self.0 == *other
    }
}
impl PartialOrd<DateTime<Utc>> for UtcMillis {
    fn partial_cmp(&self, other: &DateTime<Utc>) -> Option<std::cmp::Ordering> {
        self.0.partial_cmp(other)
    }
}
impl PartialEq<UtcMillis> for DateTime<Utc> {
    fn eq(&self, other: &UtcMillis) -> bool {
        *self == other.0
    }
}
impl PartialOrd<UtcMillis> for DateTime<Utc> {
    fn partial_cmp(&self, other: &UtcMillis) -> Option<std::cmp::Ordering> {
        self.partial_cmp(&other.0)
    }
}
impl serde::Serialize for UtcMillis {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.timestamp_millis().to_string())
    }
}
impl<'de> serde::Deserialize<'de> for UtcMillis {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = nuono_domain::scalar::i64_string::deserialize(d)?;
        Self::from_millis(value)
            .ok_or_else(|| serde::de::Error::custom("UTC timestamp out of range"))
    }
}
impl From<UtcMillis> for Value {
    fn from(value: UtcMillis) -> Self {
        Value::BigInt(Some(value.timestamp_millis()))
    }
}
impl ValueType for UtcMillis {
    fn try_from(value: Value) -> Result<Self, ValueTypeErr> {
        Self::from_millis(<i64 as ValueType>::try_from(value)?).ok_or(ValueTypeErr)
    }
    fn type_name() -> String {
        "UtcMillis".into()
    }
    fn array_type() -> ArrayType {
        ArrayType::BigInt
    }
    fn column_type() -> ColumnType {
        ColumnType::BigInteger
    }
}
impl Nullable for UtcMillis {
    fn null() -> Value {
        Value::BigInt(None)
    }
}
impl TryGetable for UtcMillis {
    fn try_get_by<I: ColIdx>(row: &QueryResult, index: I) -> Result<Self, TryGetError> {
        let value = i64::try_get_by(row, index)?;
        Self::from_millis(value).ok_or_else(|| {
            TryGetError::DbErr(sea_orm::DbErr::Type("UTC milliseconds out of range".into()))
        })
    }
}
impl utoipa::PartialSchema for UtcMillis {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        <String as utoipa::PartialSchema>::schema()
    }
}
impl utoipa::ToSchema for UtcMillis {}
pub fn db_now() -> UtcMillis {
    Utc::now().into()
}
pub fn format_db_ts(value: impl Into<DateTime<Utc>>) -> String {
    nuono_app::clock::format_db_ts(value.into())
}
pub fn parse_db_ts(value: &str) -> Option<UtcMillis> {
    nuono_app::clock::parse_db_ts(value).map(Into::into)
}

/// 在 ORM 写入边界将应用层时间转换为跨数据库可移植的存储层时间戳表示。
pub trait IntoStorageTime {
    type Output;
    fn into_storage_time(self) -> Self::Output;
}
impl IntoStorageTime for DateTime<Utc> {
    type Output = UtcMillis;
    fn into_storage_time(self) -> UtcMillis {
        self.into()
    }
}
impl IntoStorageTime for UtcMillis {
    type Output = Self;
    fn into_storage_time(self) -> Self {
        self
    }
}
impl IntoStorageTime for Option<DateTime<Utc>> {
    type Output = Option<UtcMillis>;
    fn into_storage_time(self) -> Self::Output {
        self.map(Into::into)
    }
}
impl IntoStorageTime for Option<UtcMillis> {
    type Output = Self;
    fn into_storage_time(self) -> Self {
        self
    }
}
pub fn store_time<T: IntoStorageTime>(value: T) -> T::Output {
    value.into_storage_time()
}

pub trait IntoApplicationTime {
    type Output;
    fn into_application_time(self) -> Self::Output;
}
impl IntoApplicationTime for UtcMillis {
    type Output = DateTime<Utc>;
    fn into_application_time(self) -> Self::Output {
        self.into()
    }
}
impl IntoApplicationTime for Option<UtcMillis> {
    type Output = Option<DateTime<Utc>>;
    fn into_application_time(self) -> Self::Output {
        self.map(Into::into)
    }
}
impl IntoApplicationTime for DateTime<Utc> {
    type Output = Self;
    fn into_application_time(self) -> Self {
        self
    }
}
impl IntoApplicationTime for Option<DateTime<Utc>> {
    type Output = Self;
    fn into_application_time(self) -> Self {
        self
    }
}
pub fn load_time<T: IntoApplicationTime>(value: T) -> T::Output {
    value.into_application_time()
}

/// 将时间戳格式化为无精度损失的十进制 Unix 毫秒字符串，供 API 传输使用。
pub fn format_wire_ts(value: impl Into<DateTime<Utc>>) -> String {
    nuono_app::clock::format_wire_ts(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_instants_truncate_once_and_use_decimal_wire_values() {
        let instant = DateTime::parse_from_rfc3339("2026-11-15T10:00:00.123987654Z")
            .unwrap()
            .with_timezone(&Utc);
        let stored = UtcMillis::from(instant);
        assert_eq!(stored.timestamp_subsec_nanos(), 123_000_000);
        assert_eq!(
            Value::from(stored),
            Value::BigInt(Some(instant.timestamp_millis()))
        );
        let wire = serde_json::to_value(stored).unwrap();
        assert_eq!(
            wire,
            serde_json::json!(instant.timestamp_millis().to_string())
        );
        assert_eq!(serde_json::from_value::<UtcMillis>(wire).unwrap(), stored);
        assert_eq!(store_time(Some(instant)), Some(stored));
        assert_eq!(load_time(Some(stored)), Some(stored.as_datetime()));
    }
}
