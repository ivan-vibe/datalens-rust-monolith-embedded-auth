//! Data types: the 34 `dl_formula` types plus the 17 user-facing `UserDataType`s.
//!
//! The formula engine is the single source of truth for the type list: `DataType` here is
//! a newtype over `dfl_native::DataType`, so a type can never exist in the backend but not
//! in the translator (or the other way round).

use std::fmt;

use dfl_native::DataType as NativeDataType;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::CoreError;

/// A `dl_formula` data type (`INTEGER`, `DATETIMETZ`, `ARRAY_STR`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DataType(NativeDataType);

impl DataType {
    pub const fn from_native(native: NativeDataType) -> Self {
        Self(native)
    }

    pub const fn native(self) -> NativeDataType {
        self.0
    }

    /// Parse a type name as `dl_formula` spells it (case-insensitive).
    pub fn parse(name: &str) -> Result<Self, CoreError> {
        let upper = name.trim().to_ascii_uppercase();
        NativeDataType::from_name(&upper)
            .map(Self)
            .ok_or(CoreError::UnknownDataType(name.to_string()))
    }

    pub fn name(self) -> &'static str {
        self.0.name()
    }

    pub fn is_numeric(self) -> bool {
        matches!(
            self.0.base(),
            NativeDataType::Integer | NativeDataType::Float
        )
    }

    pub fn is_datetime(self) -> bool {
        // `genericdatetime` is the user type of a timestamp without a declared zone; a filter
        // value on such a field is still a timestamp and has to be rendered as one (for
        // ClickHouse that means `toDateTime(...)`, not a bare string literal).
        matches!(
            self.0.base(),
            NativeDataType::Date
                | NativeDataType::Datetime
                | NativeDataType::Datetimetz
                | NativeDataType::GenericDatetime
        )
    }

    pub fn is_string_like(self) -> bool {
        matches!(
            self.0.base(),
            NativeDataType::String | NativeDataType::Markup
        )
    }

    /// A list-valued type: an `array` or a `tree`.
    ///
    /// `_generate_tree_filter_items` locks a tree level with a `STARTSWITH` filter, which
    /// `dl_formula` resolves to `FuncStartswithArrayConst` for a list and to the `LIKE` form for
    /// a string. The compiler needs the same distinction.
    pub fn is_array(self) -> bool {
        matches!(
            self.0.base(),
            NativeDataType::ArrayStr
                | NativeDataType::ArrayInt
                | NativeDataType::ArrayFloat
                | NativeDataType::TreeStr
        )
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl Serialize for DataType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for DataType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        DataType::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// `dl_constants.enums.UserDataType`: what the UI and the dataset JSON speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserDataType {
    String,
    Integer,
    Float,
    Date,
    Datetime,
    Boolean,
    Geopoint,
    Geopolygon,
    Uuid,
    Markup,
    Datetimetz,
    Unsupported,
    ArrayStr,
    ArrayInt,
    ArrayFloat,
    TreeStr,
    Genericdatetime,
}

impl UserDataType {
    pub fn parse(value: &str) -> Result<Self, CoreError> {
        let lower = value.trim().to_ascii_lowercase();
        match lower.as_str() {
            "string" => Ok(Self::String),
            "integer" => Ok(Self::Integer),
            "float" => Ok(Self::Float),
            "date" => Ok(Self::Date),
            "datetime" => Ok(Self::Datetime),
            "boolean" => Ok(Self::Boolean),
            "geopoint" => Ok(Self::Geopoint),
            "geopolygon" => Ok(Self::Geopolygon),
            "uuid" => Ok(Self::Uuid),
            "markup" => Ok(Self::Markup),
            "datetimetz" => Ok(Self::Datetimetz),
            "unsupported" => Ok(Self::Unsupported),
            "array_str" => Ok(Self::ArrayStr),
            "array_int" => Ok(Self::ArrayInt),
            "array_float" => Ok(Self::ArrayFloat),
            "tree_str" => Ok(Self::TreeStr),
            "genericdatetime" => Ok(Self::Genericdatetime),
            other => Err(CoreError::UnknownUserDataType(other.to_string())),
        }
    }

    /// The `dl_formula` type the user type maps onto (`UserDataType.string -> STRING`).
    pub fn to_data_type(self) -> DataType {
        let native = match self {
            Self::String => NativeDataType::String,
            Self::Integer => NativeDataType::Integer,
            Self::Float => NativeDataType::Float,
            Self::Date => NativeDataType::Date,
            Self::Datetime => NativeDataType::Datetime,
            Self::Boolean => NativeDataType::Boolean,
            Self::Geopoint => NativeDataType::Geopoint,
            Self::Geopolygon => NativeDataType::Geopolygon,
            Self::Uuid => NativeDataType::Uuid,
            Self::Markup => NativeDataType::Markup,
            Self::Datetimetz => NativeDataType::Datetimetz,
            Self::Unsupported => NativeDataType::Unsupported,
            Self::ArrayStr => NativeDataType::ArrayStr,
            Self::ArrayInt => NativeDataType::ArrayInt,
            Self::ArrayFloat => NativeDataType::ArrayFloat,
            Self::TreeStr => NativeDataType::TreeStr,
            Self::Genericdatetime => NativeDataType::GenericDatetime,
        };
        DataType(native)
    }

    /// `dl_api_lib.api_common.data_types.bi_to_yql`: the legacy name the `v1` responses use.
    ///
    /// `make_data_response_v1` describes every column with a YQL schema
    /// (`["OptionalType", ["DataType", "Double"]]`), while the `v2` envelope spells the same
    /// type as `float`. `_BI_TO_YQL` has no entry for `unsupported` (Python raises there); the
    /// port keeps the name it already has, and no response reaches that state.
    pub fn yql_name(self) -> &'static str {
        match self {
            Self::String => "String",
            Self::Integer => "Int64",
            Self::Float => "Double",
            Self::Date => "Date",
            Self::Datetime => "Datetime",
            Self::Datetimetz => "DatetimeTZ",
            Self::Genericdatetime => "GenericDatetime",
            Self::Boolean => "Bool",
            Self::Geopoint => "GeoPoint",
            Self::Geopolygon => "GeoPolygon",
            Self::Uuid => "UUID",
            Self::Markup => "Markup",
            Self::ArrayStr => "ArrayStr",
            Self::ArrayInt => "ArrayInt",
            Self::ArrayFloat => "ArrayFloat",
            Self::TreeStr => "TreeStr",
            Self::Unsupported => "Unsupported",
        }
    }

    /// Inverse of [`UserDataType::to_data_type`], for the legend the API returns.
    pub fn from_data_type(dtype: DataType) -> Self {
        match dtype.native().base() {
            NativeDataType::String => Self::String,
            NativeDataType::Integer => Self::Integer,
            NativeDataType::Float => Self::Float,
            NativeDataType::Date => Self::Date,
            NativeDataType::Datetime => Self::Datetime,
            NativeDataType::Boolean => Self::Boolean,
            NativeDataType::Geopoint => Self::Geopoint,
            NativeDataType::Geopolygon => Self::Geopolygon,
            NativeDataType::Uuid => Self::Uuid,
            NativeDataType::Markup => Self::Markup,
            NativeDataType::Datetimetz => Self::Datetimetz,
            NativeDataType::ArrayStr => Self::ArrayStr,
            NativeDataType::ArrayInt => Self::ArrayInt,
            NativeDataType::ArrayFloat => Self::ArrayFloat,
            NativeDataType::TreeStr => Self::TreeStr,
            NativeDataType::GenericDatetime => Self::Genericdatetime,
            _ => Self::Unsupported,
        }
    }

    /// The name the JSON uses (`UserDataType.date -> "date"`), for messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Float => "float",
            Self::Date => "date",
            Self::Datetime => "datetime",
            Self::Boolean => "boolean",
            Self::Geopoint => "geopoint",
            Self::Geopolygon => "geopolygon",
            Self::Uuid => "uuid",
            Self::Markup => "markup",
            Self::Datetimetz => "datetimetz",
            Self::Unsupported => "unsupported",
            Self::ArrayStr => "array_str",
            Self::ArrayInt => "array_int",
            Self::ArrayFloat => "array_float",
            Self::TreeStr => "tree_str",
            Self::Genericdatetime => "genericdatetime",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_types_map_onto_formula_types_and_round_trip_through_json() {
        for (raw, expected) in [
            ("string", "STRING"),
            ("integer", "INTEGER"),
            ("float", "FLOAT"),
            ("date", "DATE"),
            ("datetime", "DATETIME"),
            ("boolean", "BOOLEAN"),
            ("datetimetz", "DATETIMETZ"),
            ("array_int", "ARRAY_INT"),
            ("tree_str", "TREE_STR"),
            ("genericdatetime", "GENERICDATETIME"),
        ] {
            let user = UserDataType::parse(raw).expect(raw);
            assert_eq!(user.to_data_type().name(), expected);
            let json = serde_json::to_string(&user).unwrap();
            assert_eq!(json, format!("\"{raw}\""));
            let decoded: UserDataType = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, user);
        }
    }

    #[test]
    fn data_type_parsing_is_case_insensitive_and_strict() {
        assert_eq!(DataType::parse("integer").unwrap().name(), "INTEGER");
        assert_eq!(DataType::parse(" DateTime ").unwrap().name(), "DATETIME");
        assert!(DataType::parse("varchar").is_err());
        assert!(UserDataType::parse("varchar").is_err());
    }

    #[test]
    fn type_predicates_use_the_base_type_of_constants() {
        assert!(DataType::parse("CONST_INTEGER").unwrap().is_numeric());
        assert!(DataType::parse("CONST_DATE").unwrap().is_datetime());
        assert!(!DataType::parse("STRING").unwrap().is_numeric());
    }
}
