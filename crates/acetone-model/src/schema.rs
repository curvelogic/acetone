//! The `schema` map layout: label, relationship-type and index
//! declarations (spec §2, §3.3, ADR-0008).
//!
//! Schema entries are rows in the `schema` prolly map. The map key is the
//! memcomparable tuple `[String(kind), String(name)]` with `kind` one of
//! [`KIND_LABEL`], [`KIND_RTYPE`], [`KIND_INDEX`] — so enumerating all
//! labels (or types, or indexes) is a prefix scan. Values are canonical
//! CBOR **text-keyed maps** (cold path — clarity over compactness,
//! ADR-0008), with map keys in canonical order and a fixed field set per
//! kind.
//!
//! Constructors validate the spec §2 declaration rules (non-empty key
//! tuples, surrogate shape, UNIQUE only on non-key properties) and
//! normalise set-valued fields, so a definition in existence encodes
//! deterministically. Decoders are strict: exactly the canonical bytes.
//! Any change here is a `format_version` bump (spec §10).

use crate::Value;
use crate::cbor::{
    MAJOR_ARRAY, MAJOR_MAP, Reader, SIMPLE_FALSE, SIMPLE_NULL, SIMPLE_TRUE, canonical_str_cmp,
    write_head, write_text,
};
use crate::keys::{self, KeyDecodeError};
use crate::values::ValueDecodeError;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use thiserror::Error;

/// Schema-map kind for label definitions.
pub const KIND_LABEL: &str = "label";
/// Schema-map kind for relationship-type definitions.
pub const KIND_RTYPE: &str = "rtype";
/// Schema-map kind for index declarations.
pub const KIND_INDEX: &str = "index";

/// The property name minted for `KEY SURROGATE` labels (spec §2).
pub const SURROGATE_KEY_PROPERTY: &str = "_id";

/// Errors from constructing, encoding or decoding schema entries.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SchemaError {
    /// A name (label, type, index, property) was empty.
    #[error("empty name: {0}")]
    EmptyName(&'static str),
    /// A key declaration violated spec §2.
    #[error("invalid key declaration: {0}")]
    InvalidKey(&'static str),
    /// A constraint declaration violated spec §2.
    #[error("invalid constraint declaration: {0}")]
    InvalidConstraint(&'static str),
    /// A low-level CBOR failure (truncation, non-canonical form, ...).
    #[error(transparent)]
    Cbor(#[from] ValueDecodeError),
    /// The schema-map key bytes did not decode.
    #[error(transparent)]
    KeyDecode(#[from] KeyDecodeError),
    /// Well-formed bytes whose shape is not a schema entry.
    #[error("unexpected schema entry shape: {0}")]
    Shape(&'static str),
    /// Field order or set order not canonical.
    #[error("schema entry not canonical: {0}")]
    NotCanonical(&'static str),
    /// A kind string that is not `label`, `rtype` or `index`.
    #[error("unknown schema entry kind {0:?}")]
    UnknownKind(String),
    /// A property type name that is not in the v0.1 vocabulary.
    #[error("unknown property type {0:?}")]
    UnknownType(String),
}

/// A declarable property type (spec §2's value vocabulary).
///
/// v0.1 shape declarations are coarse: `List` does not constrain the
/// element type (list homogeneity is a graph-layer check).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyType {
    /// Boolean.
    Bool,
    /// 64-bit signed integer.
    Int,
    /// 64-bit IEEE-754 float.
    Float,
    /// UTF-8 string.
    String,
    /// Opaque bytes.
    Bytes,
    /// Calendar date.
    Date,
    /// Time of day.
    Time,
    /// Instant with UTC offset.
    DateTime,
    /// Months/days/nanoseconds duration.
    Duration,
    /// Homogeneous list (element type unconstrained in v0.1).
    List,
}

impl PropertyType {
    /// The stored name of this type.
    pub fn as_str(self) -> &'static str {
        match self {
            PropertyType::Bool => "bool",
            PropertyType::Int => "int",
            PropertyType::Float => "float",
            PropertyType::String => "string",
            PropertyType::Bytes => "bytes",
            PropertyType::Date => "date",
            PropertyType::Time => "time",
            PropertyType::DateTime => "datetime",
            PropertyType::Duration => "duration",
            PropertyType::List => "list",
        }
    }

    /// Parse a stored type name.
    pub fn parse(name: &str) -> Result<Self, SchemaError> {
        Ok(match name {
            "bool" => PropertyType::Bool,
            "int" => PropertyType::Int,
            "float" => PropertyType::Float,
            "string" => PropertyType::String,
            "bytes" => PropertyType::Bytes,
            "date" => PropertyType::Date,
            "time" => PropertyType::Time,
            "datetime" => PropertyType::DateTime,
            "duration" => PropertyType::Duration,
            "list" => PropertyType::List,
            other => return Err(SchemaError::UnknownType(other.to_owned())),
        })
    }
}

fn normalise_names(
    names: impl IntoIterator<Item = String>,
    context: &'static str,
) -> Result<Vec<String>, SchemaError> {
    let mut v: Vec<String> = names.into_iter().collect();
    if v.iter().any(String::is_empty) {
        return Err(SchemaError::EmptyName(context));
    }
    v.sort_by(|a, b| canonical_str_cmp(a, b));
    v.dedup();
    Ok(v)
}

fn check_types(types: &BTreeMap<String, PropertyType>) -> Result<(), SchemaError> {
    if types.keys().any(|k| k.is_empty()) {
        return Err(SchemaError::EmptyName("typed property name"));
    }
    Ok(())
}

/// A label definition: key declaration, optional shape, constraints
/// (spec §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelDef {
    key: Vec<String>,
    surrogate: bool,
    types: BTreeMap<String, PropertyType>,
    exists: Vec<String>,
    unique: Vec<String>,
}

impl LabelDef {
    /// Declare a label with a natural key: an ordered, non-empty tuple of
    /// property names. `exists` and `unique` are constraint property
    /// sets; `unique` must not name key properties (spec §2: UNIQUE
    /// applies to non-key properties — key uniqueness is identity).
    pub fn new(
        key: Vec<String>,
        types: BTreeMap<String, PropertyType>,
        exists: impl IntoIterator<Item = String>,
        unique: impl IntoIterator<Item = String>,
    ) -> Result<Self, SchemaError> {
        Self::build(key, false, types, exists, unique)
    }

    /// Declare a `KEY SURROGATE` label: acetone mints a ULID `_id` key
    /// property at creation (spec §2).
    pub fn surrogate(
        types: BTreeMap<String, PropertyType>,
        exists: impl IntoIterator<Item = String>,
        unique: impl IntoIterator<Item = String>,
    ) -> Result<Self, SchemaError> {
        Self::build(
            vec![SURROGATE_KEY_PROPERTY.to_owned()],
            true,
            types,
            exists,
            unique,
        )
    }

    fn build(
        key: Vec<String>,
        surrogate: bool,
        types: BTreeMap<String, PropertyType>,
        exists: impl IntoIterator<Item = String>,
        unique: impl IntoIterator<Item = String>,
    ) -> Result<Self, SchemaError> {
        if key.is_empty() {
            return Err(SchemaError::InvalidKey("key tuple must be non-empty"));
        }
        if key.iter().any(String::is_empty) {
            return Err(SchemaError::EmptyName("key property"));
        }
        {
            let mut seen = key.clone();
            seen.sort();
            seen.dedup();
            if seen.len() != key.len() {
                return Err(SchemaError::InvalidKey("key properties must be distinct"));
            }
        }
        if surrogate && key.as_slice() != [SURROGATE_KEY_PROPERTY.to_owned()] {
            return Err(SchemaError::InvalidKey(
                "surrogate labels key on exactly [\"_id\"]",
            ));
        }
        check_types(&types)?;
        let exists = normalise_names(exists, "existence-constraint property")?;
        let unique = normalise_names(unique, "unique-constraint property")?;
        if unique.iter().any(|u| key.contains(u)) {
            return Err(SchemaError::InvalidConstraint(
                "UNIQUE applies to non-key properties only",
            ));
        }
        Ok(LabelDef {
            key,
            surrogate,
            types,
            exists,
            unique,
        })
    }

    /// The declared key tuple, in declaration order.
    pub fn key(&self) -> &[String] {
        &self.key
    }

    /// Whether the key is surrogate (`KEY SURROGATE`).
    pub fn is_surrogate(&self) -> bool {
        self.surrogate
    }

    /// Declared property types.
    pub fn types(&self) -> &BTreeMap<String, PropertyType> {
        &self.types
    }

    /// Existence-constrained properties (sorted).
    pub fn exists(&self) -> &[String] {
        &self.exists
    }

    /// Unique-constrained properties (sorted).
    pub fn unique(&self) -> &[String] {
        &self.unique
    }
}

/// A relationship-type definition (spec §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelTypeDef {
    discriminator: Option<String>,
    types: BTreeMap<String, PropertyType>,
    exists: Vec<String>,
}

impl RelTypeDef {
    /// Declare a relationship type. A discriminator property permits
    /// parallel relationships of this type between the same endpoints
    /// (spec §2); `None` means the discriminator is always the default.
    pub fn new(
        discriminator: Option<String>,
        types: BTreeMap<String, PropertyType>,
        exists: impl IntoIterator<Item = String>,
    ) -> Result<Self, SchemaError> {
        if let Some(d) = &discriminator
            && d.is_empty()
        {
            return Err(SchemaError::EmptyName("discriminator property"));
        }
        check_types(&types)?;
        let exists = normalise_names(exists, "existence-constraint property")?;
        Ok(RelTypeDef {
            discriminator,
            types,
            exists,
        })
    }

    /// The discriminator property, if declared.
    pub fn discriminator(&self) -> Option<&str> {
        self.discriminator.as_deref()
    }

    /// Declared property types.
    pub fn types(&self) -> &BTreeMap<String, PropertyType> {
        &self.types
    }

    /// Existence-constrained properties (sorted).
    pub fn exists(&self) -> &[String] {
        &self.exists
    }
}

/// A declared property index (spec §3.3): the map `idx/<name>` indexes one or
/// more `properties` over nodes with `label`. A **composite** index has more
/// than one property; its key is the ordered tuple of their values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef {
    label: String,
    properties: Vec<String>,
}

impl IndexDef {
    /// Declare an index over `(label, properties)`. `properties` must be
    /// non-empty and contain no empty names.
    pub fn new(label: impl Into<String>, properties: Vec<String>) -> Result<Self, SchemaError> {
        let label = label.into();
        if label.is_empty() {
            return Err(SchemaError::EmptyName("index label"));
        }
        if properties.is_empty() || properties.iter().any(|p| p.is_empty()) {
            return Err(SchemaError::EmptyName("index property"));
        }
        Ok(IndexDef { label, properties })
    }

    /// The indexed label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The indexed properties, in declaration order.
    pub fn properties(&self) -> &[String] {
        &self.properties
    }
}

/// One row of the `schema` map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaEntry {
    /// A label definition, keyed `["label", name]`.
    Label {
        /// The label name.
        name: String,
        /// The definition.
        def: LabelDef,
    },
    /// A relationship-type definition, keyed `["rtype", name]`.
    RelType {
        /// The type name.
        name: String,
        /// The definition.
        def: RelTypeDef,
    },
    /// An index declaration, keyed `["index", name]`; `name` is the
    /// `<name>` of the `idx/<name>` map.
    Index {
        /// The index name.
        name: String,
        /// The declaration.
        def: IndexDef,
    },
}

fn entry_key(kind: &str, name: &str) -> Vec<u8> {
    keys::encode_key(&[
        Value::String(kind.to_owned()),
        Value::String(name.to_owned()),
    ])
    .expect("string tuples always encode")
}

/// Byte prefix of every schema-map key of the given kind (one of the
/// `KIND_*` constants): "all labels" etc. as a prefix scan.
pub fn schema_kind_prefix(kind: &str) -> Vec<u8> {
    keys::encode_key(std::slice::from_ref(&Value::String(kind.to_owned())))
        .expect("string tuples always encode")
}

impl SchemaEntry {
    /// The entry's name.
    pub fn name(&self) -> &str {
        match self {
            SchemaEntry::Label { name, .. }
            | SchemaEntry::RelType { name, .. }
            | SchemaEntry::Index { name, .. } => name,
        }
    }

    /// The `schema`-map key for this entry.
    pub fn map_key(&self) -> Vec<u8> {
        match self {
            SchemaEntry::Label { name, .. } => entry_key(KIND_LABEL, name),
            SchemaEntry::RelType { name, .. } => entry_key(KIND_RTYPE, name),
            SchemaEntry::Index { name, .. } => entry_key(KIND_INDEX, name),
        }
    }

    /// The `schema`-map value: a canonical CBOR text-keyed map.
    /// Infallible: definitions contain only names and booleans.
    pub fn encode_value(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            SchemaEntry::Label { def, .. } => {
                // Canonical field order: key, types, exists, unique,
                // surrogate (verified against canonical_str_cmp in tests).
                write_head(&mut out, MAJOR_MAP, 5);
                write_text(&mut out, "key");
                write_name_array(&mut out, &def.key);
                write_text(&mut out, "types");
                write_types(&mut out, &def.types);
                write_text(&mut out, "exists");
                write_name_array(&mut out, &def.exists);
                write_text(&mut out, "unique");
                write_name_array(&mut out, &def.unique);
                write_text(&mut out, "surrogate");
                out.push(if def.surrogate {
                    SIMPLE_TRUE
                } else {
                    SIMPLE_FALSE
                });
            }
            SchemaEntry::RelType { def, .. } => {
                // Canonical field order: disc, types, exists.
                write_head(&mut out, MAJOR_MAP, 3);
                write_text(&mut out, "disc");
                match &def.discriminator {
                    Some(d) => write_text(&mut out, d),
                    None => out.push(SIMPLE_NULL),
                }
                write_text(&mut out, "types");
                write_types(&mut out, &def.types);
                write_text(&mut out, "exists");
                write_name_array(&mut out, &def.exists);
            }
            SchemaEntry::Index { def, .. } => {
                // Canonical field order: label < properties. The property list
                // is in *declaration* order (a composite index's key is the
                // ordered tuple), not sorted.
                write_head(&mut out, MAJOR_MAP, 2);
                write_text(&mut out, "label");
                write_text(&mut out, &def.label);
                write_text(&mut out, "properties");
                write_head(&mut out, MAJOR_ARRAY, def.properties.len() as u64);
                for property in &def.properties {
                    write_text(&mut out, property);
                }
            }
        }
        out
    }

    /// Decode a `schema`-map row from its key and value bytes.
    pub fn decode(key: &[u8], value: &[u8]) -> Result<Self, SchemaError> {
        let tuple = keys::decode_key(key)?;
        let [Value::String(kind), Value::String(name)] = tuple.as_slice() else {
            return Err(SchemaError::Shape(
                "schema key must be [string kind, string name]",
            ));
        };
        if name.is_empty() {
            return Err(SchemaError::EmptyName("schema entry name"));
        }
        let mut reader = Reader::new(value);
        let entry = match kind.as_str() {
            KIND_LABEL => {
                expect_map(&mut reader, 5, "label definition")?;
                expect_field(&mut reader, "key")?;
                let key_tuple = read_name_array(&mut reader, false)?;
                expect_field(&mut reader, "types")?;
                let types = read_types(&mut reader)?;
                expect_field(&mut reader, "exists")?;
                let exists = read_name_array(&mut reader, true)?;
                expect_field(&mut reader, "unique")?;
                let unique = read_name_array(&mut reader, true)?;
                expect_field(&mut reader, "surrogate")?;
                let surrogate = read_bool(&mut reader)?;
                let def = LabelDef::build(key_tuple, surrogate, types, exists, unique)?;
                SchemaEntry::Label {
                    name: name.clone(),
                    def,
                }
            }
            KIND_RTYPE => {
                expect_map(&mut reader, 3, "relationship-type definition")?;
                expect_field(&mut reader, "disc")?;
                let discriminator = read_optional_text(&mut reader)?;
                expect_field(&mut reader, "types")?;
                let types = read_types(&mut reader)?;
                expect_field(&mut reader, "exists")?;
                let exists = read_name_array(&mut reader, true)?;
                let def = RelTypeDef::new(discriminator, types, exists)?;
                SchemaEntry::RelType {
                    name: name.clone(),
                    def,
                }
            }
            KIND_INDEX => {
                expect_map(&mut reader, 2, "index declaration")?;
                expect_field(&mut reader, "label")?;
                let label = reader.read_text()?;
                expect_field(&mut reader, "properties")?;
                let count = reader.read_head(MAJOR_ARRAY)?;
                let count =
                    usize::try_from(count).map_err(|_| SchemaError::EmptyName("index property"))?;
                let mut properties = Vec::with_capacity(count.min(reader.remaining()));
                for _ in 0..count {
                    properties.push(reader.read_text()?);
                }
                let def = IndexDef::new(label, properties)?;
                SchemaEntry::Index {
                    name: name.clone(),
                    def,
                }
            }
            other => return Err(SchemaError::UnknownKind(other.to_owned())),
        };
        if reader.remaining() != 0 {
            return Err(SchemaError::Cbor(ValueDecodeError::TrailingBytes));
        }
        Ok(entry)
    }
}

fn write_name_array(out: &mut Vec<u8>, names: &[String]) {
    write_head(out, MAJOR_ARRAY, names.len() as u64);
    for name in names {
        write_text(out, name);
    }
}

fn write_types(out: &mut Vec<u8>, types: &BTreeMap<String, PropertyType>) {
    let mut entries: Vec<(&String, PropertyType)> = types.iter().map(|(k, v)| (k, *v)).collect();
    entries.sort_by(|a, b| canonical_str_cmp(a.0, b.0));
    write_head(out, MAJOR_MAP, entries.len() as u64);
    for (name, ty) in entries {
        write_text(out, name);
        write_text(out, ty.as_str());
    }
}

fn expect_map(reader: &mut Reader, fields: u64, what: &'static str) -> Result<(), SchemaError> {
    let count = reader.read_head(MAJOR_MAP)?;
    if count != fields {
        return Err(SchemaError::Shape(what));
    }
    Ok(())
}

fn expect_field(reader: &mut Reader, name: &'static str) -> Result<(), SchemaError> {
    let got = reader.read_text()?;
    if got != name {
        return Err(SchemaError::NotCanonical("unexpected field name or order"));
    }
    Ok(())
}

fn read_bool(reader: &mut Reader) -> Result<bool, SchemaError> {
    match reader.read_u8()? {
        SIMPLE_TRUE => Ok(true),
        SIMPLE_FALSE => Ok(false),
        _ => Err(SchemaError::Shape("expected boolean")),
    }
}

fn read_optional_text(reader: &mut Reader) -> Result<Option<String>, SchemaError> {
    if reader.remaining() > 0 && reader.input[reader.pos] == SIMPLE_NULL {
        reader.read_u8()?;
        return Ok(None);
    }
    Ok(Some(reader.read_text()?))
}

fn read_name_array(reader: &mut Reader, sorted: bool) -> Result<Vec<String>, SchemaError> {
    let count = reader.read_head(MAJOR_ARRAY)?;
    if count > reader.remaining() as u64 {
        return Err(SchemaError::Cbor(ValueDecodeError::LengthOverrun {
            declared: count,
            remaining: reader.remaining(),
        }));
    }
    let mut names: Vec<String> = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let name = reader.read_text()?;
        if sorted
            && let Some(prev) = names.last()
            && canonical_str_cmp(prev, &name) != Ordering::Less
        {
            return Err(SchemaError::NotCanonical(
                "name set must be strictly ascending",
            ));
        }
        names.push(name);
    }
    Ok(names)
}

fn read_types(reader: &mut Reader) -> Result<BTreeMap<String, PropertyType>, SchemaError> {
    let count = reader.read_head(MAJOR_MAP)?;
    if count > reader.remaining() as u64 {
        return Err(SchemaError::Cbor(ValueDecodeError::LengthOverrun {
            declared: count,
            remaining: reader.remaining(),
        }));
    }
    let mut types = BTreeMap::new();
    let mut previous: Option<String> = None;
    for _ in 0..count {
        let name = reader.read_text()?;
        if let Some(prev) = &previous
            && canonical_str_cmp(prev, &name) != Ordering::Less
        {
            return Err(SchemaError::NotCanonical(
                "typed property names must be strictly ascending",
            ));
        }
        let ty = PropertyType::parse(&reader.read_text()?)?;
        previous = Some(name.clone());
        types.insert(name, ty);
    }
    Ok(types)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label_entry() -> SchemaEntry {
        SchemaEntry::Label {
            name: "Host".into(),
            def: LabelDef::new(
                vec!["dc".into(), "name".into()],
                [("cores".to_owned(), PropertyType::Int)].into(),
                ["os".to_owned()],
                ["serial".to_owned()],
            )
            .expect("valid"),
        }
    }

    #[test]
    fn label_field_order_is_canonical() {
        let fields = ["key", "types", "exists", "unique", "surrogate"];
        let mut sorted = fields;
        sorted.sort_by(|a, b| canonical_str_cmp(a, b));
        assert_eq!(fields, sorted, "encoder writes fields in canonical order");
        let rel_fields = ["disc", "types", "exists"];
        let mut rel_sorted = rel_fields;
        rel_sorted.sort_by(|a, b| canonical_str_cmp(a, b));
        assert_eq!(rel_fields, rel_sorted);
        let idx_fields = ["label", "property"];
        let mut idx_sorted = idx_fields;
        idx_sorted.sort_by(|a, b| canonical_str_cmp(a, b));
        assert_eq!(idx_fields, idx_sorted);
    }

    #[test]
    fn entries_round_trip() {
        let entries = [
            label_entry(),
            SchemaEntry::Label {
                name: "Event".into(),
                def: LabelDef::surrogate(BTreeMap::new(), [], []).expect("valid"),
            },
            SchemaEntry::RelType {
                name: "DEPENDS_ON".into(),
                def: RelTypeDef::new(
                    Some("port".into()),
                    [("weight".to_owned(), PropertyType::Float)].into(),
                    ["since".to_owned()],
                )
                .expect("valid"),
            },
            SchemaEntry::RelType {
                name: "HOSTS".into(),
                def: RelTypeDef::new(None, BTreeMap::new(), []).expect("valid"),
            },
            SchemaEntry::Index {
                name: "host_os".into(),
                def: IndexDef::new("Host", vec!["os".into()]).expect("valid"),
            },
        ];
        for entry in entries {
            let key = entry.map_key();
            let value = entry.encode_value();
            let back = SchemaEntry::decode(&key, &value).expect("decode");
            assert_eq!(back, entry);
            assert_eq!(back.encode_value(), value, "re-encode is byte-identical");
        }
    }

    #[test]
    fn kind_prefixes_partition_the_map() {
        let label = label_entry().map_key();
        assert!(label.starts_with(&schema_kind_prefix(KIND_LABEL)));
        assert!(!label.starts_with(&schema_kind_prefix(KIND_RTYPE)));
        assert!(!label.starts_with(&schema_kind_prefix(KIND_INDEX)));
    }

    #[test]
    fn declaration_rules_are_enforced() {
        assert!(matches!(
            LabelDef::new(vec![], BTreeMap::new(), [], []),
            Err(SchemaError::InvalidKey(_))
        ));
        assert!(matches!(
            LabelDef::new(vec!["a".into(), "a".into()], BTreeMap::new(), [], []),
            Err(SchemaError::InvalidKey(_))
        ));
        assert!(matches!(
            LabelDef::new(vec!["a".into()], BTreeMap::new(), [], ["a".to_owned()]),
            Err(SchemaError::InvalidConstraint(_))
        ));
        assert!(matches!(
            RelTypeDef::new(Some(String::new()), BTreeMap::new(), []),
            Err(SchemaError::EmptyName(_))
        ));
        assert!(matches!(
            IndexDef::new("", vec!["os".into()]),
            Err(SchemaError::EmptyName(_))
        ));
    }

    #[test]
    fn decode_rejects_unknown_kind_and_wrong_shape() {
        let key = entry_key("view", "V");
        assert!(matches!(
            SchemaEntry::decode(&key, &[0xa0]),
            Err(SchemaError::UnknownKind(_))
        ));
        let label_key = entry_key(KIND_LABEL, "Host");
        // Empty map instead of the five-field definition.
        assert!(matches!(
            SchemaEntry::decode(&label_key, &[0xa0]),
            Err(SchemaError::Shape(_))
        ));
        // Trailing byte after a valid entry.
        let entry = label_entry();
        let mut value = entry.encode_value();
        value.push(0x00);
        assert!(matches!(
            SchemaEntry::decode(&entry.map_key(), &value),
            Err(SchemaError::Cbor(ValueDecodeError::TrailingBytes))
        ));
    }
}
