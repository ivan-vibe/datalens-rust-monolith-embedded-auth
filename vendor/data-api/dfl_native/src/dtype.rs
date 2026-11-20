//! Data types of the formula language, mirroring `dl_formula.core.datatype.DataType`.
//!
//! The names are exactly the ones the registry generator writes into
//! `registry_generated.rs`, so a template key can be parsed back into this enum.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DataType {
    Null,
    Integer,
    ConstInteger,
    Float,
    ConstFloat,
    String,
    ConstString,
    Date,
    ConstDate,
    Datetime,
    ConstDatetime,
    Datetimetz,
    ConstDatetimetz,
    GenericDatetime,
    ConstGenericDatetime,
    Boolean,
    ConstBoolean,
    Geopoint,
    ConstGeopoint,
    Geopolygon,
    ConstGeopolygon,
    Markup,
    ConstMarkup,
    Uuid,
    ConstUuid,
    ArrayFloat,
    ConstArrayFloat,
    ArrayInt,
    ConstArrayInt,
    ArrayStr,
    ConstArrayStr,
    TreeStr,
    ConstTreeStr,
    Unsupported,
}

const NAMES: &[(&str, DataType)] = &[
    ("NULL", DataType::Null),
    ("INTEGER", DataType::Integer),
    ("CONST_INTEGER", DataType::ConstInteger),
    ("FLOAT", DataType::Float),
    ("CONST_FLOAT", DataType::ConstFloat),
    ("STRING", DataType::String),
    ("CONST_STRING", DataType::ConstString),
    ("DATE", DataType::Date),
    ("CONST_DATE", DataType::ConstDate),
    ("DATETIME", DataType::Datetime),
    ("CONST_DATETIME", DataType::ConstDatetime),
    ("DATETIMETZ", DataType::Datetimetz),
    ("CONST_DATETIMETZ", DataType::ConstDatetimetz),
    ("GENERICDATETIME", DataType::GenericDatetime),
    ("CONST_GENERICDATETIME", DataType::ConstGenericDatetime),
    ("BOOLEAN", DataType::Boolean),
    ("CONST_BOOLEAN", DataType::ConstBoolean),
    ("GEOPOINT", DataType::Geopoint),
    ("CONST_GEOPOINT", DataType::ConstGeopoint),
    ("GEOPOLYGON", DataType::Geopolygon),
    ("CONST_GEOPOLYGON", DataType::ConstGeopolygon),
    ("MARKUP", DataType::Markup),
    ("CONST_MARKUP", DataType::ConstMarkup),
    ("UUID", DataType::Uuid),
    ("CONST_UUID", DataType::ConstUuid),
    ("ARRAY_FLOAT", DataType::ArrayFloat),
    ("CONST_ARRAY_FLOAT", DataType::ConstArrayFloat),
    ("ARRAY_INT", DataType::ArrayInt),
    ("CONST_ARRAY_INT", DataType::ConstArrayInt),
    ("ARRAY_STR", DataType::ArrayStr),
    ("CONST_ARRAY_STR", DataType::ConstArrayStr),
    ("TREE_STR", DataType::TreeStr),
    ("CONST_TREE_STR", DataType::ConstTreeStr),
    ("UNSUPPORTED", DataType::Unsupported),
];

impl DataType {
    pub fn from_name(name: &str) -> Option<Self> {
        NAMES
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, dtype)| *dtype)
    }

    pub fn name(self) -> &'static str {
        NAMES
            .iter()
            .find(|(_, dtype)| *dtype == self)
            .map(|(name, _)| *name)
            .unwrap_or("UNSUPPORTED")
    }

    /// CONst variants map to their plain counterpart: the native engine never folds
    /// constants, it only needs the base type to find a translation.
    pub fn base(self) -> Self {
        match self {
            DataType::ConstInteger => DataType::Integer,
            DataType::ConstFloat => DataType::Float,
            DataType::ConstString => DataType::String,
            DataType::ConstDate => DataType::Date,
            DataType::ConstDatetime => DataType::Datetime,
            DataType::ConstDatetimetz => DataType::Datetimetz,
            DataType::ConstGenericDatetime => DataType::GenericDatetime,
            DataType::ConstBoolean => DataType::Boolean,
            DataType::ConstGeopoint => DataType::Geopoint,
            DataType::ConstGeopolygon => DataType::Geopolygon,
            DataType::ConstMarkup => DataType::Markup,
            DataType::ConstUuid => DataType::Uuid,
            DataType::ConstArrayFloat => DataType::ArrayFloat,
            DataType::ConstArrayInt => DataType::ArrayInt,
            DataType::ConstArrayStr => DataType::ArrayStr,
            DataType::ConstTreeStr => DataType::TreeStr,
            other => other,
        }
    }

    pub fn is_const(self) -> bool {
        self != self.base()
    }

    /// Every type in `NAMES` order: the order in which the implicit-cast widening of
    /// `registry::lookup` tries the targets of a `casts_to` edge.
    pub fn all() -> impl Iterator<Item = DataType> {
        NAMES.iter().map(|(_, dtype)| *dtype)
    }

    /// `dl_formula.core.datatype.DataType.casts_to`: can `self` be cast to `other` without an
    /// explicit cast? Mirrors `_AUTOCAST_FROM_TYPES`, which decides which argument types a
    /// translation accepts (`ArgTypeSequence.match_arg_types`).
    pub fn casts_to(self, other: Self) -> bool {
        use DataType::*;
        match other {
            Null => self == Null,
            ConstInteger => self == ConstInteger,
            ConstDate => self == ConstDate,
            ConstString => self == ConstString,
            ConstBoolean => self == ConstBoolean,
            ConstDatetime => self == ConstDatetime,
            ConstDatetimetz => self == ConstDatetimetz,
            ConstGenericDatetime => self == ConstGenericDatetime,
            ConstFloat => matches!(self, ConstFloat | ConstInteger),
            ConstGeopoint => self == ConstGeopoint,
            ConstGeopolygon => self == ConstGeopolygon,
            ConstMarkup => self == ConstMarkup,
            ConstUuid => self == ConstUuid,
            ConstArrayFloat => self == ConstArrayFloat,
            ConstArrayInt => self == ConstArrayInt,
            ConstArrayStr => self == ConstArrayStr,
            ConstTreeStr => self == ConstTreeStr,
            Integer => matches!(self, Integer | ConstInteger | Null),
            Date => matches!(self, Date | ConstDate | Null),
            String => matches!(self, String | ConstString | Null),
            Geopoint => matches!(self, Geopoint | ConstGeopoint | Null),
            Geopolygon => matches!(self, Geopolygon | ConstGeopolygon | Null),
            Markup => matches!(self, Markup | ConstMarkup | Null),
            Boolean => matches!(self, Boolean | ConstBoolean | Null),
            Datetime => matches!(self, Datetime | ConstDatetime | Null),
            Datetimetz => matches!(self, Datetimetz | ConstDatetimetz | Null),
            GenericDatetime => matches!(self, GenericDatetime | ConstGenericDatetime | Null),
            Float => {
                matches!(self, Float | Integer | ConstFloat | ConstInteger | Null)
            }
            Uuid => matches!(self, Uuid | ConstUuid | Null),
            TreeStr => matches!(self, TreeStr | ConstTreeStr | Null),
            ArrayFloat => matches!(self, ArrayFloat | ConstArrayFloat | Null),
            ArrayInt => matches!(self, ArrayInt | ConstArrayInt | Null),
            // A tree of strings is a list of strings as far as the argument matcher is concerned.
            ArrayStr => matches!(
                self,
                ArrayStr | ConstArrayStr | TreeStr | ConstTreeStr | Null
            ),
            Unsupported => self == Unsupported,
        }
    }
}
