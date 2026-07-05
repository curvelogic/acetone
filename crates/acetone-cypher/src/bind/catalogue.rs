//! The schema snapshot the binder resolves against, decoupled from the
//! repository so binding is a pure function over data. Built from the
//! `schema` map's entries (acetone-model `SchemaEntry`).

use std::collections::BTreeMap;

use acetone_model::schema::{IndexDef, LabelDef, RelTypeDef, SchemaEntry};

/// A read-only snapshot of the schema at the bound ref.
#[derive(Debug, Default, Clone)]
pub struct Catalogue {
    labels: BTreeMap<String, LabelDef>,
    rel_types: BTreeMap<String, RelTypeDef>,
    /// Index name → declaration.
    indexes: BTreeMap<String, IndexDef>,
}

impl Catalogue {
    /// An empty catalogue: no declared labels, types or indexes. Useful
    /// for schema-free binding (TCK) and tests.
    pub fn empty() -> Self {
        Catalogue::default()
    }

    pub fn from_entries(entries: impl IntoIterator<Item = SchemaEntry>) -> Self {
        let mut catalogue = Catalogue::default();
        for entry in entries {
            match entry {
                SchemaEntry::Label { name, def } => {
                    catalogue.labels.insert(name, def);
                }
                SchemaEntry::RelType { name, def } => {
                    catalogue.rel_types.insert(name, def);
                }
                SchemaEntry::Index { name, def } => {
                    catalogue.indexes.insert(name, def);
                }
            }
        }
        catalogue
    }

    pub fn label(&self, name: &str) -> Option<&LabelDef> {
        self.labels.get(name)
    }

    pub fn rel_type(&self, name: &str) -> Option<&RelTypeDef> {
        self.rel_types.get(name)
    }

    /// A declared secondary index over `(label, property)`, if any.
    pub fn index_on(&self, label: &str, property: &str) -> Option<(&str, &IndexDef)> {
        self.indexes
            .iter()
            .find(|(_, def)| def.label() == label && def.property() == property)
            .map(|(name, def)| (name.as_str(), def))
    }

    /// Whether `property` is a (prefix of the) key of `label` — the
    /// primary map itself serves as the key index (spec §3: node keys are
    /// memcomparable prefixes).
    pub fn is_key_prefix(&self, label: &str, property: &str) -> bool {
        self.labels
            .get(label)
            .is_some_and(|def| def.key().first().is_some_and(|k| k == property))
    }
}
