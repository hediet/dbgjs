//! Acquisition-independent, bounded descriptions shared by live and captured values.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ValueDescription {
    pub kind: String,
    pub summary: Option<String>,
    pub value: Option<Value>,
    pub properties: Vec<DescribedProperty>,
    pub reference: Option<String>,
    pub identity: Option<String>,
    pub state: Option<String>,
    pub origin: Option<String>,
    pub result: Option<Box<ValueDescription>>,
    pub truncated: bool,
    pub incomplete: bool,
    pub next_start: Option<u32>,
    pub source: crate::debugger::object_inspection::ObjectSourceSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DescribedProperty {
    pub name: String,
    pub value: ValueDescription,
}

impl ValueDescription {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            summary: None,
            value: None,
            properties: Vec::new(),
            reference: None,
            identity: None,
            state: None,
            origin: None,
            result: None,
            truncated: false,
            incomplete: false,
            next_start: None,
            source: Default::default(),
        }
    }

    /// No target code, getters, or serializers are invoked by this renderer.
    pub fn json(&self, exact: bool) -> Result<Value, String> {
        if !exact {
            if let Ok(value) = self.render_json(true) {
                return Ok(value);
            }
        }
        self.render_json(exact)
    }

    fn render_json(&self, exact: bool) -> Result<Value, String> {
        if !self.truncated && !self.incomplete {
            if self.kind == "null" {
                return Ok(Value::Null);
            }
            if let Some(value) = &self.value {
                return Ok(value.clone());
            }
            if self.kind == "object"
                && !self.properties.iter().any(|p| {
                    p.name == "$dbgjs"
                        && p.value.kind == "object"
                        && p.value.properties.iter().any(|p| p.name == "kind")
                })
            {
                let mut value = serde_json::Map::new();
                for property in &self.properties {
                    value.insert(property.name.clone(), property.value.render_json(exact)?);
                }
                return Ok(Value::Object(value));
            }
            if self.kind == "object" && exact {
                let mut value = serde_json::Map::new();
                for property in &self.properties {
                    value.insert(property.name.clone(), property.value.render_json(true)?);
                }
                return Ok(Value::Object(value));
            }
            if self.kind == "array" {
                return self
                    .properties
                    .iter()
                    .map(|p| p.value.render_json(exact))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Value::Array);
            }
        }
        if exact {
            return Err(format!(
                "value cannot be exported exactly as JSON: {}{}",
                self.kind,
                if self.truncated || self.incomplete {
                    " (inspection is incomplete)"
                } else {
                    ""
                }
            ));
        }
        let mut metadata = serde_json::Map::new();
        metadata.insert("kind".into(), json!(self.kind));
        if let Some(v) = &self.summary {
            metadata.insert("summary".into(), json!(v));
        }
        if let Some(v) = &self.reference {
            metadata.insert("reference".into(), json!(v));
        }
        if let Some(v) = &self.identity {
            metadata.insert("identity".into(), json!(v));
        }
        if let Some(v) = &self.state {
            metadata.insert("state".into(), json!(v));
        }
        if let Some(v) = &self.result {
            metadata.insert("result".into(), v.render_json(false)?);
        }
        if self.truncated {
            metadata.insert("truncated".into(), json!(true));
        }
        if self.incomplete {
            metadata.insert("incomplete".into(), json!(true));
        }
        if let Some(start) = self.next_start {
            metadata.insert("nextStart".into(), json!(start));
        }
        if !self.properties.is_empty() {
            metadata.insert(
                "properties".into(),
                Value::Array(
                    self.properties
                        .iter()
                        .map(|p| Ok(json!({"name":p.name,"value":p.value.render_json(false)?})))
                        .collect::<Result<Vec<_>, String>>()?,
                ),
            );
        }
        Ok(json!({"$dbgjs": metadata}))
    }

    pub fn display(&self) -> String {
        let mut text = self.summary.clone().unwrap_or_else(|| self.kind.clone());
        if let Some(state) = &self.state {
            text.push_str(&format!(" ({state})"));
        }
        if let Some(reference) = &self.reference {
            text.push_str(&format!(" [{reference}]"));
        }
        if let Some(result) = &self.result {
            text.push_str(&format!(": {}", result.display()));
        }
        for property in &self.properties {
            text.push_str(&format!(
                "\n  {}: {}",
                property.name,
                property.value.display().replace('\n', "\n  ")
            ));
        }
        if self.truncated {
            text.push_str(" …");
        }
        if let Some(start) = self.next_start {
            text.push_str(&format!("\nMore children: --start {start}"));
        }
        text
    }

    pub fn inline(&self, max_chars: usize) -> String {
        if !matches!(self.kind.as_str(), "object" | "array" | "instance")
            || self.properties.is_empty()
        {
            return self.summary.clone().unwrap_or_else(|| self.kind.clone());
        }
        let array = self.kind == "array";
        let name = self
            .summary
            .as_deref()
            .unwrap_or(&self.kind)
            .trim_end_matches(" {...}")
            .trim_end_matches(" [...]");
        let mut entries = Vec::new();
        let mut used = name.chars().count() + 3;
        let mut omitted = self.truncated;
        for property in &self.properties {
            let label = if array && property.name.parse::<u64>().is_ok() {
                property.name.clone()
            } else {
                format!("\"{}\"", property.name.escape_default())
            };
            let entry = format!(
                "{label}: {}",
                property
                    .value
                    .summary
                    .as_deref()
                    .unwrap_or(&property.value.kind)
            );
            let cost = entry.chars().count() + if entries.is_empty() { 0 } else { 2 };
            if used + cost + 5 > max_chars {
                omitted = true;
                break;
            }
            used += cost;
            entries.push(entry);
        }
        if omitted {
            entries.push("...".into());
        }
        format!(
            "{name} {}{}{}",
            if array { '[' } else { '{' },
            entries.join(", "),
            if array { ']' } else { '}' }
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DescribeOptions {
    #[serde(default)]
    pub start: u32,
    pub max_depth: u32,
    pub max_properties: u32,
    pub max_nodes: u32,
    pub max_string_length: u32,
}

impl Default for DescribeOptions {
    fn default() -> Self {
        Self {
            start: 0,
            max_depth: 8,
            max_properties: 100,
            max_nodes: 1000,
            max_string_length: 10_000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ValueOperation {
    Evaluate {
        expression: String,
        retain: bool,
        #[serde(rename = "awaitResult")]
        await_result: bool,
    },
    Show {
        reference: String,
    },
    Children {
        reference: String,
    },
    Await {
        reference: String,
    },
    Release {
        reference: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_values_and_collision_safe_metadata() {
        let mut scalar = ValueDescription::new("number");
        scalar.value = Some(json!(4.25));
        let mut object = ValueDescription::new("object");
        object.properties.push(DescribedProperty {
            name: "$dbgjs".into(),
            value: scalar,
        });
        assert_eq!(object.json(true).unwrap(), json!({"$dbgjs":4.25}));
        assert_eq!(object.json(false).unwrap(), json!({"$dbgjs":4.25}));
        let mut collision = ValueDescription::new("object");
        let mut kind = ValueDescription::new("string");
        kind.value = Some(json!("promise"));
        collision.properties.push(DescribedProperty {
            name: "kind".into(),
            value: kind,
        });
        object.properties[0].value = collision;
        object.properties.push(DescribedProperty {
            name: "unsupported".into(),
            value: ValueDescription::new("undefined"),
        });
        assert_eq!(object.json(false).unwrap()["$dbgjs"]["kind"], "object");
        object.truncated = true;
        assert!(object.json(true).is_err());
    }
    #[test]
    fn unsupported_values_are_never_silently_dropped() {
        for kind in [
            "undefined",
            "function",
            "symbol",
            "bigint",
            "reference",
            "accessor",
            "promise",
            "number",
        ] {
            let value = ValueDescription::new(kind);
            assert!(value.json(true).is_err());
            assert_eq!(value.json(false).unwrap()["$dbgjs"]["kind"], kind);
        }
    }
}
