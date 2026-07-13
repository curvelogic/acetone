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

    /// Whether the catalogue declares nothing — a schema-free repository.
    pub fn is_empty(&self) -> bool {
        self.labels.is_empty() && self.rel_types.is_empty() && self.indexes.is_empty()
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

    /// Declared label names, for "did you mean" suggestions on an unknown
    /// label. Read-only; the order is the map's (sorted) key order.
    pub fn label_names(&self) -> impl Iterator<Item = &str> {
        self.labels.keys().map(String::as_str)
    }

    /// Declared relationship-type names, for suggestions on an unknown type.
    pub fn rel_type_names(&self) -> impl Iterator<Item = &str> {
        self.rel_types.keys().map(String::as_str)
    }

    /// The property names a `label` declares — its key tuple plus any typed
    /// (shape) properties — for suggestions on an unknown property. Empty if
    /// the label is undeclared.
    pub fn property_names(&self, label: &str) -> Vec<&str> {
        match self.labels.get(label) {
            Some(def) => def
                .key()
                .iter()
                .map(String::as_str)
                .chain(def.types().keys().map(String::as_str))
                .collect(),
            None => Vec::new(),
        }
    }

    /// A declared **single-property** secondary index over `(label, property)`,
    /// if any. Composite (multi-property) indexes are not yet consulted for
    /// seek planning — a query pinning all their properties falls back to a
    /// scan-and-filter, which is correct but unaccelerated (the seek
    /// acceleration is a tracked follow-up); they are still maintained and
    /// `fsck`-verified.
    pub fn index_on(&self, label: &str, property: &str) -> Option<(&str, &IndexDef)> {
        self.indexes
            .iter()
            .find(|(_, def)| {
                def.label() == label
                    && def.properties().len() == 1
                    && def.properties()[0] == property
            })
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

    /// Whether `property` is anywhere in `label`'s key tuple — the
    /// properties `SET`/`REMOVE` must never touch (spec §5.1, Invariant #3).
    pub fn is_key_property(&self, label: &str, property: &str) -> bool {
        self.labels
            .get(label)
            .is_some_and(|def| def.key().iter().any(|k| k == property))
    }
}
