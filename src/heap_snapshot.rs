use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

use serde::Deserialize;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};

const RETAINED_INSTANCES_PER_CONSTRUCTOR: usize = 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeapConstructorGroup {
    pub script_id: i64,
    pub line: u32,
    pub column: u32,
    pub generated_name: String,
    pub instance_count: u64,
    pub shallow_size: u64,
    pub instances: Vec<HeapInstanceRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeapInstanceRecord {
    pub heap_object_id: u64,
    pub shallow_size: u64,
}

pub fn parse_constructor_groups(
    reader: impl Read,
) -> Result<Vec<HeapConstructorGroup>, HeapSnapshotParseError> {
    let mut analyzer = HeapSnapshotAnalyzer::default();
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    HeapSnapshotSeed {
        analyzer: &mut analyzer,
    }
    .deserialize(&mut deserializer)?;
    analyzer.finish()
}

#[derive(Default)]
struct HeapSnapshotAnalyzer {
    meta: Option<HeapSnapshotMeta>,
    objects: Vec<ObjectRecord>,
    groups: BTreeMap<ConstructorLocation, ConstructorAggregate>,
    names: BTreeMap<u32, String>,
}

impl HeapSnapshotAnalyzer {
    fn meta(&self) -> Result<&HeapSnapshotMeta, HeapSnapshotParseError> {
        self.meta
            .as_ref()
            .ok_or(HeapSnapshotParseError::MetadataMustPrecedeData)
    }

    fn finish(self) -> Result<Vec<HeapConstructorGroup>, HeapSnapshotParseError> {
        let mut groups = Vec::with_capacity(self.groups.len());
        for (location, group) in self.groups {
            let generated_name = self
                .names
                .get(&group.name_index)
                .cloned()
                .unwrap_or_else(|| "(unknown)".to_owned());
            groups.push(HeapConstructorGroup {
                script_id: location.script_id,
                line: location.line,
                column: location.column,
                generated_name,
                instance_count: group.instance_count,
                shallow_size: group.shallow_size,
                instances: group.instances,
            });
        }
        Ok(groups)
    }
}

#[derive(Clone)]
struct HeapSnapshotMeta {
    node_field_count: usize,
    node_type_offset: usize,
    node_name_offset: usize,
    node_id_offset: usize,
    node_self_size_offset: usize,
    object_type: u64,
    location_field_count: usize,
    location_object_index_offset: usize,
    location_script_id_offset: usize,
    location_line_offset: usize,
    location_column_offset: usize,
}

#[derive(Deserialize)]
struct SnapshotHeader {
    meta: RawMeta,
}

#[derive(Deserialize)]
struct RawMeta {
    node_fields: Vec<String>,
    node_types: Vec<serde_json::Value>,
    #[serde(default)]
    location_fields: Vec<String>,
}

impl TryFrom<RawMeta> for HeapSnapshotMeta {
    type Error = HeapSnapshotParseError;

    fn try_from(meta: RawMeta) -> Result<Self, Self::Error> {
        let field = |fields: &[String], name: &'static str| {
            fields
                .iter()
                .position(|field| field == name)
                .ok_or(HeapSnapshotParseError::MissingField(name))
        };
        let node_type_offset = field(&meta.node_fields, "type")?;
        let object_type = meta
            .node_types
            .get(node_type_offset)
            .and_then(serde_json::Value::as_array)
            .and_then(|types| {
                types
                    .iter()
                    .position(|value| value.as_str() == Some("object"))
            })
            .ok_or(HeapSnapshotParseError::MissingObjectNodeType)? as u64;
        Ok(Self {
            node_field_count: meta.node_fields.len(),
            node_type_offset,
            node_name_offset: field(&meta.node_fields, "name")?,
            node_id_offset: field(&meta.node_fields, "id")?,
            node_self_size_offset: field(&meta.node_fields, "self_size")?,
            object_type,
            location_field_count: meta.location_fields.len(),
            location_object_index_offset: field(&meta.location_fields, "object_index")?,
            location_script_id_offset: field(&meta.location_fields, "script_id")?,
            location_line_offset: field(&meta.location_fields, "line")?,
            location_column_offset: field(&meta.location_fields, "column")?,
        })
    }
}

#[derive(Clone)]
struct ObjectRecord {
    node_offset: u64,
    name_index: u32,
    heap_object_id: u64,
    shallow_size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ConstructorLocation {
    script_id: i64,
    line: u32,
    column: u32,
}

struct ConstructorAggregate {
    name_index: u32,
    instance_count: u64,
    shallow_size: u64,
    instances: Vec<HeapInstanceRecord>,
}

struct HeapSnapshotSeed<'a> {
    analyzer: &'a mut HeapSnapshotAnalyzer,
}

impl<'de> DeserializeSeed<'de> for HeapSnapshotSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(HeapSnapshotVisitor {
            analyzer: self.analyzer,
        })
    }
}

struct HeapSnapshotVisitor<'a> {
    analyzer: &'a mut HeapSnapshotAnalyzer,
}

impl<'de> Visitor<'de> for HeapSnapshotVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a V8 heap snapshot object")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "snapshot" => {
                    let header = map.next_value::<SnapshotHeader>()?;
                    self.analyzer.meta =
                        Some(header.meta.try_into().map_err(serde::de::Error::custom)?);
                }
                "nodes" => {
                    let meta = self
                        .analyzer
                        .meta()
                        .map_err(serde::de::Error::custom)?
                        .clone();
                    map.next_value_seed(NodesSeed {
                        meta,
                        objects: &mut self.analyzer.objects,
                    })?;
                }
                "locations" => {
                    let meta = self
                        .analyzer
                        .meta()
                        .map_err(serde::de::Error::custom)?
                        .clone();
                    map.next_value_seed(LocationsSeed {
                        meta,
                        objects: &self.analyzer.objects,
                        groups: &mut self.analyzer.groups,
                    })?;
                    self.analyzer.objects.clear();
                    self.analyzer.objects.shrink_to_fit();
                }
                "strings" => {
                    let needed = self
                        .analyzer
                        .groups
                        .values()
                        .map(|group| group.name_index)
                        .collect::<BTreeSet<_>>();
                    map.next_value_seed(StringsSeed {
                        needed: &needed,
                        names: &mut self.analyzer.names,
                    })?;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(())
    }
}

struct NodesSeed<'a> {
    meta: HeapSnapshotMeta,
    objects: &'a mut Vec<ObjectRecord>,
}

impl<'de> DeserializeSeed<'de> for NodesSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(NodesVisitor {
            meta: self.meta,
            objects: self.objects,
        })
    }
}

struct NodesVisitor<'a> {
    meta: HeapSnapshotMeta,
    objects: &'a mut Vec<ObjectRecord>,
}

impl<'de> Visitor<'de> for NodesVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the flattened heap node array")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        if self.meta.node_field_count == 0 {
            return Err(serde::de::Error::custom("node_fields cannot be empty"));
        }
        let mut record = vec![0_u64; self.meta.node_field_count];
        let mut node_offset = 0_u64;
        loop {
            for (index, value) in record.iter_mut().enumerate() {
                let Some(next) = sequence.next_element::<u64>()? else {
                    if index == 0 {
                        return Ok(());
                    }
                    return Err(serde::de::Error::custom(
                        "nodes array ended inside a record",
                    ));
                };
                *value = next;
            }
            if record[self.meta.node_type_offset] == self.meta.object_type {
                self.objects.push(ObjectRecord {
                    node_offset,
                    name_index: u32::try_from(record[self.meta.node_name_offset])
                        .map_err(serde::de::Error::custom)?,
                    heap_object_id: record[self.meta.node_id_offset],
                    shallow_size: record[self.meta.node_self_size_offset],
                });
            }
            node_offset = node_offset.saturating_add(self.meta.node_field_count as u64);
        }
    }
}

struct LocationsSeed<'a> {
    meta: HeapSnapshotMeta,
    objects: &'a [ObjectRecord],
    groups: &'a mut BTreeMap<ConstructorLocation, ConstructorAggregate>,
}

impl<'de> DeserializeSeed<'de> for LocationsSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(LocationsVisitor {
            meta: self.meta,
            objects: self.objects,
            groups: self.groups,
        })
    }
}

struct LocationsVisitor<'a> {
    meta: HeapSnapshotMeta,
    objects: &'a [ObjectRecord],
    groups: &'a mut BTreeMap<ConstructorLocation, ConstructorAggregate>,
}

impl<'de> Visitor<'de> for LocationsVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the flattened heap locations array")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        if self.meta.location_field_count == 0 {
            return Err(serde::de::Error::custom("location_fields cannot be empty"));
        }
        let mut record = vec![0_i64; self.meta.location_field_count];
        loop {
            for (index, value) in record.iter_mut().enumerate() {
                let Some(next) = sequence.next_element::<i64>()? else {
                    if index == 0 {
                        return Ok(());
                    }
                    return Err(serde::de::Error::custom(
                        "locations array ended inside a record",
                    ));
                };
                *value = next;
            }
            let Ok(node_offset) = u64::try_from(record[self.meta.location_object_index_offset])
            else {
                continue;
            };
            let Ok(object_index) = self
                .objects
                .binary_search_by_key(&node_offset, |object| object.node_offset)
            else {
                continue;
            };
            let object = &self.objects[object_index];
            let Ok(line) = u32::try_from(record[self.meta.location_line_offset]) else {
                continue;
            };
            let Ok(column) = u32::try_from(record[self.meta.location_column_offset]) else {
                continue;
            };
            let group = self
                .groups
                .entry(ConstructorLocation {
                    script_id: record[self.meta.location_script_id_offset],
                    line,
                    column,
                })
                .or_insert_with(|| ConstructorAggregate {
                    name_index: object.name_index,
                    instance_count: 0,
                    shallow_size: 0,
                    instances: Vec::new(),
                });
            group.instance_count = group.instance_count.saturating_add(1);
            group.shallow_size = group.shallow_size.saturating_add(object.shallow_size);
            if group.instances.len() < RETAINED_INSTANCES_PER_CONSTRUCTOR {
                group.instances.push(HeapInstanceRecord {
                    heap_object_id: object.heap_object_id,
                    shallow_size: object.shallow_size,
                });
            }
        }
    }
}

struct StringsSeed<'a> {
    needed: &'a BTreeSet<u32>,
    names: &'a mut BTreeMap<u32, String>,
}

impl<'de> DeserializeSeed<'de> for StringsSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(StringsVisitor {
            needed: self.needed,
            names: self.names,
        })
    }
}

struct StringsVisitor<'a> {
    needed: &'a BTreeSet<u32>,
    names: &'a mut BTreeMap<u32, String>,
}

impl<'de> Visitor<'de> for StringsVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the heap snapshot string table")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut index = 0_u32;
        while let Some(value) = sequence.next_element::<String>()? {
            if self.needed.contains(&index) {
                self.names.insert(index, value);
            }
            index = index.saturating_add(1);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HeapSnapshotParseError {
    #[error("invalid heap snapshot JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("heap snapshot metadata must precede nodes and locations")]
    MetadataMustPrecedeData,
    #[error("heap snapshot metadata is missing field '{0}'")]
    MissingField(&'static str),
    #[error("heap snapshot metadata does not define the object node type")]
    MissingObjectNodeType,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_constructor_groups_without_retaining_edges_or_unused_strings() {
        let snapshot = r#"{
          "snapshot": {
            "meta": {
              "node_fields": ["type", "name", "id", "self_size", "edge_count"],
              "node_types": [["hidden", "object"], "string", "number", "number", "number"],
              "edge_fields": ["type", "name_or_index", "to_node"],
              "edge_types": [["property"], "string_or_number", "node"],
              "location_fields": ["object_index", "script_id", "line", "column"]
            },
            "node_count": 3,
            "edge_count": 1
          },
          "nodes": [
            0, 0, 1, 0, 0,
            1, 2, 101, 16, 1,
            1, 2, 103, 24, 0
          ],
          "edges": [0, 1, 10],
          "locations": [5, 7, 10, 2, 10, 7, 10, 2],
          "strings": ["", "unused", "GeneratedClass"]
        }"#;

        let groups = parse_constructor_groups(snapshot.as_bytes()).unwrap();
        assert_eq!(
            groups,
            vec![HeapConstructorGroup {
                script_id: 7,
                line: 10,
                column: 2,
                generated_name: "GeneratedClass".to_owned(),
                instance_count: 2,
                shallow_size: 40,
                instances: vec![
                    HeapInstanceRecord {
                        heap_object_id: 101,
                        shallow_size: 16,
                    },
                    HeapInstanceRecord {
                        heap_object_id: 103,
                        shallow_size: 24,
                    },
                ],
            }]
        );
    }
}
