//! Interpret a [`HubRpcInterfaceSchema`] into an in-memory Rust type/client model
//! and render it as deterministic Rust source.
//!
//! See [`super`] for the public entry points; this module holds the model and
//! the schema→Rust lowering.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::protocol::json_value::JsonValue;
use crate::schema::interface_schema::{HubRpcInterfaceSchema, MethodSchema};

use super::code_writer::CodeWriter;
use super::ident::{strip_raw, to_pascal_case, to_snake_case};
use super::{GenerateRustOptions, GeneratedRust};

/// A Rust type in field / parameter / return position.
#[derive(Debug, Clone, PartialEq)]
enum TypeRef {
    /// A generated named type (struct/enum/alias).
    Named(String),
    Vec(Box<TypeRef>),
    /// `HashMap<String, V>`.
    Map(Box<TypeRef>),
    Tuple(Vec<TypeRef>),
    /// A leaf primitive rendered verbatim (`bool`, `String`, `i64`, ...).
    Prim(&'static str),
    /// `serde_json::Value` — the escape hatch for `any`/unknown and anything
    /// outside the representable subset.
    Json,
}

impl TypeRef {
    /// Collect every [`TypeRef::Named`] reachable from this type into `out`.
    fn collect_named(&self, out: &mut Vec<String>) {
        match self {
            TypeRef::Named(n) => out.push(n.clone()),
            TypeRef::Vec(inner) | TypeRef::Map(inner) => inner.collect_named(out),
            TypeRef::Tuple(items) => items.iter().for_each(|t| t.collect_named(out)),
            TypeRef::Prim(_) | TypeRef::Json => {}
        }
    }
}

#[derive(Debug, Clone)]
struct Field {
    rust_name: String,
    wire_name: String,
    ty: TypeRef,
    optional: bool,
    doc: Option<String>,
    /// A `#[serde(flatten)]` catch-all map for open objects.
    flatten: bool,
}

#[derive(Debug, Clone)]
struct TaggedVariant {
    rust_name: String,
    wire_tag: String,
    fields: Vec<Field>,
    doc: Option<String>,
}

#[derive(Debug, Clone)]
struct UntaggedVariant {
    rust_name: String,
    ty: TypeRef,
}

#[derive(Debug, Clone)]
enum TypeBody {
    Struct(Vec<Field>),
    /// A closed enum of string literals: `(rust_variant, wire_value, doc)`.
    StringEnum(Vec<(String, String, Option<String>)>),
    /// An internally-tagged (`#[serde(tag = "...")]`) union.
    TaggedUnion {
        tag: String,
        variants: Vec<TaggedVariant>,
    },
    /// An untagged (`#[serde(untagged)]`) union.
    UntaggedUnion(Vec<UntaggedVariant>),
    /// A `pub type Name = ...;` alias.
    Alias(TypeRef),
}

#[derive(Debug, Clone)]
struct TypeDef {
    name: String,
    doc: Option<String>,
    body: TypeBody,
}

/// A single method lowered to param/result Rust types.
struct MethodModel {
    wire_name: String,
    rust_name: String,
    doc: Option<String>,
    params: TypeRef,
    result: Option<TypeRef>,
    /// True when tagged `x-hubrpc-codegen.kind = "serverNotification"`: a
    /// server→client event. No client *send* method is generated for these.
    server_notification: bool,
}

// ─────────────────────────────────────────────────────────── collection

struct Collector<'a> {
    components: BTreeMap<String, &'a JsonValue>,
    /// component wire name → assigned Rust type name.
    component_names: HashMap<String, String>,
    /// Rust names already handed out (types), to keep synthetic names unique.
    used_names: HashSet<String>,
    /// Emitted type definitions, in creation order.
    defs: Vec<TypeDef>,
    /// Set of def names already produced, to avoid re-emitting a synthetic type.
    def_names: HashSet<String>,
    unsupported: Vec<String>,
}

impl<'a> Collector<'a> {
    fn new(schema: &'a HubRpcInterfaceSchema) -> Self {
        let mut components: BTreeMap<String, &JsonValue> = BTreeMap::new();
        if let Some(bag) = schema.components.as_ref().and_then(|c| c.schemas.as_ref()) {
            for (k, v) in bag {
                components.insert(k.clone(), v);
            }
        }

        // Assign Rust names up front so `$ref`s can resolve during lowering.
        let mut used_names = HashSet::new();
        let mut component_names = HashMap::new();
        for name in components.keys() {
            let rust = unique_name(&to_pascal_case(name), &mut used_names);
            component_names.insert(name.clone(), rust);
        }

        Collector {
            components,
            component_names,
            used_names,
            defs: Vec::new(),
            def_names: HashSet::new(),
            unsupported: Vec::new(),
        }
    }

    fn note(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !self.unsupported.contains(&msg) {
            self.unsupported.push(msg);
        }
    }

    fn push_def(&mut self, def: TypeDef) {
        if self.def_names.insert(def.name.clone()) {
            self.defs.push(def);
        }
    }

    /// Resolve a `#/components/schemas/<name>` ref to its Rust type name.
    fn resolve_ref(&mut self, reference: &str) -> Option<String> {
        let prefix = "#/components/schemas/";
        let name = reference.strip_prefix(prefix)?;
        match self.component_names.get(name) {
            Some(rust) => Some(rust.clone()),
            None => {
                self.note(format!("dangling $ref \"{reference}\" → serde_json::Value"));
                None
            }
        }
    }

    /// Interpret every component as a named type, then lower each method.
    fn run(&mut self, schema: &HubRpcInterfaceSchema) -> Vec<MethodModel> {
        let component_list: Vec<(String, &JsonValue)> = self
            .components
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        for (wire, sub) in component_list {
            let rust = self.component_names[&wire].clone();
            let doc = schema_doc(sub);
            self.build_named(rust, sub, doc);
        }

        let mut methods = Vec::new();
        for (wire, method) in &schema.methods {
            methods.push(self.lower_method(wire, method));
        }
        methods
    }

    fn lower_method(&mut self, wire: &str, method: &MethodSchema) -> MethodModel {
        let base = to_pascal_case(wire);
        let params = self.type_ref(&method.params, &format!("{base}Params"));
        let server_notification = codegen_kind(method) == Some("serverNotification");
        // A server event carries no result. If one is present the annotation is
        // contradictory; honor the annotation (drop the result) and report it.
        let result = if server_notification {
            if method.result.is_some() {
                self.note(format!(
                    "method `{wire}` is tagged serverNotification but declares a result; \
                     result ignored (server events have no reply)"
                ));
            }
            None
        } else {
            method
                .result
                .as_ref()
                .map(|r| self.type_ref(r, &format!("{base}Result")))
        };
        MethodModel {
            wire_name: wire.to_string(),
            rust_name: to_snake_case(wire),
            doc: method.description.clone(),
            params,
            result,
            server_notification,
        }
    }

    /// Return a [`TypeRef`] for `schema`, synthesizing a named type (registered
    /// under `name_hint`) for complex inline shapes.
    fn type_ref(&mut self, schema: &JsonValue, name_hint: &str) -> TypeRef {
        match schema {
            JsonValue::Bool(true) => return TypeRef::Json,
            JsonValue::Bool(false) => {
                self.note("`false`/never schema → serde_json::Value");
                return TypeRef::Json;
            }
            JsonValue::Object(_) => {}
            _ => {
                self.note("non-object schema → serde_json::Value");
                return TypeRef::Json;
            }
        }
        let obj = schema.as_object().expect("object checked above");

        if let Some(JsonValue::String(reference)) = obj.get("$ref") {
            return match self.resolve_ref(reference) {
                Some(rust) => TypeRef::Named(rust),
                None => TypeRef::Json,
            };
        }

        // Leaf-ish shapes map directly without a synthetic type.
        if obj.contains_key("const") {
            self.note("standalone `const` schema → serde_json::Value");
            return TypeRef::Json;
        }
        if obj.get("type") == Some(&JsonValue::String("array".into())) {
            if let Some(prefix) = obj.get("prefixItems").and_then(JsonValue::as_array) {
                return self.tuple_ref(prefix, obj.get("items"), name_hint);
            }
            let items = obj.get("items").cloned().unwrap_or(JsonValue::Bool(true));
            let inner = self.type_ref(&items, &format!("{name_hint}Item"));
            return TypeRef::Vec(Box::new(inner));
        }
        if obj.get("type") == Some(&JsonValue::String("object".into())) {
            let has_props = obj
                .get("properties")
                .and_then(JsonValue::as_object)
                .is_some_and(|p| !p.is_empty());
            if !has_props {
                // A pure map / open object (no declared properties).
                if let Some(v) = self.open_object_value(obj, name_hint) {
                    return v;
                }
            }
            // else: fall through to build a synthetic struct.
        } else if let Some(prim) = self.primitive_ref(obj) {
            return prim;
        }

        // Anything remaining (object w/ props, enum, union) becomes a named type.
        let name = unique_name(name_hint, &mut self.used_names);
        self.build_named(name.clone(), schema, schema_doc(schema));
        TypeRef::Named(name)
    }

    /// The element type of a pure map object, or `None` if this is not a map.
    fn open_object_value(
        &mut self,
        obj: &crate::protocol::json_value::JsonMap<String, JsonValue>,
        name_hint: &str,
    ) -> Option<TypeRef> {
        match obj.get("additionalProperties") {
            Some(JsonValue::Bool(false)) | None => None,
            Some(JsonValue::Bool(true)) => Some(TypeRef::Map(Box::new(TypeRef::Json))),
            Some(v) => {
                let inner = self.type_ref(v, &format!("{name_hint}Value"));
                Some(TypeRef::Map(Box::new(inner)))
            }
        }
    }

    fn tuple_ref(
        &mut self,
        prefix: &[JsonValue],
        rest: Option<&JsonValue>,
        name_hint: &str,
    ) -> TypeRef {
        if matches!(rest, Some(v) if *v != JsonValue::Bool(false)) {
            self.note(format!(
                "tuple `{name_hint}` with a rest schema is unsupported → serde_json::Value elements"
            ));
        }
        let items = prefix
            .iter()
            .enumerate()
            .map(|(i, s)| self.type_ref(s, &format!("{name_hint}{i}")))
            .collect();
        TypeRef::Tuple(items)
    }

    fn primitive_ref(
        &mut self,
        obj: &crate::protocol::json_value::JsonMap<String, JsonValue>,
    ) -> Option<TypeRef> {
        // A string `enum` is handled as a named type, not here.
        if obj.contains_key("enum") {
            return None;
        }
        let ty = obj.get("type")?.as_str()?;
        let format = obj.get("format").and_then(JsonValue::as_str);
        Some(match ty {
            "boolean" => TypeRef::Prim("bool"),
            "string" => TypeRef::Prim("String"),
            "integer" => TypeRef::Prim(match format {
                Some("int32") => "i32",
                Some("uint32") => "u32",
                Some("uint64") => "u64",
                _ => "i64",
            }),
            "number" => TypeRef::Prim(match format {
                Some("float32") => "f32",
                _ => "f64",
            }),
            "null" => {
                self.note("`null` type → serde_json::Value");
                TypeRef::Json
            }
            _ => return None,
        })
    }

    /// Build (and register) the named type `name` describing `schema` directly.
    fn build_named(&mut self, name: String, schema: &JsonValue, doc: Option<String>) {
        if self.def_names.contains(&name) {
            return;
        }
        let Some(obj) = schema.as_object() else {
            self.push_def(TypeDef {
                name,
                doc,
                body: TypeBody::Alias(TypeRef::Json),
            });
            return;
        };

        // Alias to another component.
        if let Some(JsonValue::String(reference)) = obj.get("$ref") {
            let target = match self.resolve_ref(reference) {
                Some(rust) => TypeRef::Named(rust),
                None => TypeRef::Json,
            };
            self.push_def(TypeDef {
                name,
                doc,
                body: TypeBody::Alias(target),
            });
            return;
        }

        // String enum.
        if let Some(values) = obj.get("enum").and_then(JsonValue::as_array) {
            self.build_enum(name, doc, values);
            return;
        }

        // Unions.
        if let Some(branches) = obj.get("oneOf").and_then(JsonValue::as_array) {
            let disc = obj
                .get("discriminator")
                .and_then(|d| d.get("propertyName"))
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            self.build_union(name, doc, branches, disc);
            return;
        }
        if let Some(branches) = obj.get("anyOf").and_then(JsonValue::as_array) {
            self.build_union(name, doc, branches, None);
            return;
        }

        // Object with properties.
        if obj.get("type") == Some(&JsonValue::String("object".into())) {
            self.build_struct(name, doc, obj);
            return;
        }

        // Array / tuple component → alias.
        if obj.get("type") == Some(&JsonValue::String("array".into())) {
            let ty = self.type_ref(schema, &format!("{name}Inner"));
            self.push_def(TypeDef {
                name,
                doc,
                body: TypeBody::Alias(ty),
            });
            return;
        }

        // Primitive component → alias.
        if let Some(prim) = self.primitive_ref(obj) {
            self.push_def(TypeDef {
                name,
                doc,
                body: TypeBody::Alias(prim),
            });
            return;
        }

        // `true`/empty or unrepresentable → alias to Value.
        self.note(format!(
            "component `{name}` is not representable → serde_json::Value alias"
        ));
        self.push_def(TypeDef {
            name,
            doc,
            body: TypeBody::Alias(TypeRef::Json),
        });
    }

    fn build_enum(&mut self, name: String, doc: Option<String>, values: &[JsonValue]) {
        if !values.iter().all(|v| v.is_string()) {
            self.note(format!(
                "heterogeneous `enum` in `{name}` → serde_json::Value alias"
            ));
            self.push_def(TypeDef {
                name,
                doc,
                body: TypeBody::Alias(TypeRef::Json),
            });
            return;
        }
        let mut variants = Vec::new();
        let mut seen = HashSet::new();
        for v in values {
            let wire = v.as_str().unwrap().to_string();
            let mut rust = to_pascal_case(&wire);
            let mut n = 1;
            while !seen.insert(rust.clone()) {
                n += 1;
                rust = format!("{}{n}", to_pascal_case(&wire));
            }
            variants.push((rust, wire, None));
        }
        self.push_def(TypeDef {
            name,
            doc,
            body: TypeBody::StringEnum(variants),
        });
    }

    fn build_union(
        &mut self,
        name: String,
        doc: Option<String>,
        branches: &[JsonValue],
        discriminator: Option<String>,
    ) {
        if let Some(tag) = &discriminator {
            if let Some(variants) = self.tagged_variants(&name, branches, tag) {
                self.push_def(TypeDef {
                    name,
                    doc,
                    body: TypeBody::TaggedUnion {
                        tag: tag.clone(),
                        variants,
                    },
                });
                return;
            }
            self.note(format!(
                "discriminated union `{name}` has non-inline branches → untagged fallback"
            ));
        }
        // Untagged union.
        let mut variants = Vec::new();
        let mut seen = HashSet::new();
        for (i, branch) in branches.iter().enumerate() {
            let hint = union_variant_hint(&name, branch, i);
            let ty = self.type_ref(branch, &hint);
            let mut rust = match &ty {
                TypeRef::Named(n) => strip_raw(n).to_string(),
                _ => format!("Variant{i}"),
            };
            let mut n = 1;
            while !seen.insert(rust.clone()) {
                n += 1;
                rust = format!("Variant{i}_{n}");
            }
            variants.push(UntaggedVariant {
                rust_name: rust,
                ty,
            });
        }
        self.push_def(TypeDef {
            name,
            doc,
            body: TypeBody::UntaggedUnion(variants),
        });
    }

    /// Try to lower `branches` into internally-tagged variants; `None` if any
    /// branch is not an inline object carrying a `const` tag property.
    fn tagged_variants(
        &mut self,
        name: &str,
        branches: &[JsonValue],
        tag: &str,
    ) -> Option<Vec<TaggedVariant>> {
        let mut variants = Vec::new();
        let mut seen = HashSet::new();
        for branch in branches {
            let obj = branch.as_object()?;
            if obj.get("type") != Some(&JsonValue::String("object".into())) {
                return None;
            }
            let props = obj.get("properties").and_then(JsonValue::as_object)?;
            let tag_const = props.get(tag)?.get("const")?.as_str()?.to_string();

            let required: HashSet<&str> = obj
                .get("required")
                .and_then(JsonValue::as_array)
                .map(|a| a.iter().filter_map(JsonValue::as_str).collect())
                .unwrap_or_default();

            let mut rust_variant = to_pascal_case(&tag_const);
            let mut n = 1;
            while !seen.insert(rust_variant.clone()) {
                n += 1;
                rust_variant = format!("{}{n}", to_pascal_case(&tag_const));
            }

            let variant_hint = format!("{name}{rust_variant}");
            let mut fields = Vec::new();
            for (key, prop) in props {
                if key == tag {
                    continue;
                }
                fields.push(self.field(&variant_hint, key, prop, required.contains(key.as_str())));
            }
            variants.push(TaggedVariant {
                rust_name: rust_variant,
                wire_tag: tag_const,
                fields,
                doc: schema_doc(branch),
            });
        }
        Some(variants)
    }

    fn build_struct(
        &mut self,
        name: String,
        doc: Option<String>,
        obj: &crate::protocol::json_value::JsonMap<String, JsonValue>,
    ) {
        let required: HashSet<&str> = obj
            .get("required")
            .and_then(JsonValue::as_array)
            .map(|a| a.iter().filter_map(JsonValue::as_str).collect())
            .unwrap_or_default();

        let mut fields = Vec::new();
        if let Some(props) = obj.get("properties").and_then(JsonValue::as_object) {
            for (key, prop) in props {
                fields.push(self.field(&name, key, prop, required.contains(key.as_str())));
            }
        }

        // Open object: an `additionalProperties` schema becomes a flattened map.
        match obj.get("additionalProperties") {
            Some(JsonValue::Bool(false)) | None => {}
            Some(JsonValue::Bool(true)) => {
                fields.push(open_extra_field(TypeRef::Map(Box::new(TypeRef::Json))));
            }
            Some(v) => {
                let inner = self.type_ref(v, &format!("{name}Extra"));
                fields.push(open_extra_field(TypeRef::Map(Box::new(inner))));
            }
        }

        self.push_def(TypeDef {
            name,
            doc,
            body: TypeBody::Struct(fields),
        });
    }

    fn field(&mut self, owner_hint: &str, key: &str, prop: &JsonValue, required: bool) -> Field {
        let rust_name = to_snake_case(key);
        let hint = format!("{owner_hint}{}", to_pascal_case(key));
        let ty = self.type_ref(prop, &hint);
        Field {
            rust_name,
            wire_name: key.to_string(),
            ty,
            optional: !required,
            doc: schema_doc(prop),
            flatten: false,
        }
    }
}

fn open_extra_field(ty: TypeRef) -> Field {
    Field {
        rust_name: "extra".to_string(),
        wire_name: String::new(),
        ty,
        optional: false,
        doc: Some("Additional properties not captured by named fields.".to_string()),
        flatten: true,
    }
}

fn union_variant_hint(name: &str, branch: &JsonValue, i: usize) -> String {
    if let Some(JsonValue::String(reference)) = branch.get("$ref") {
        if let Some(last) = reference.rsplit('/').next() {
            return format!("{name}{}", to_pascal_case(last));
        }
    }
    format!("{name}Variant{i}")
}

fn unique_name(base: &str, used: &mut HashSet<String>) -> String {
    let mut candidate = base.to_string();
    let mut n = 1;
    while !used.insert(candidate.clone()) {
        n += 1;
        candidate = format!("{base}{n}");
    }
    candidate
}

fn schema_doc(schema: &JsonValue) -> Option<String> {
    let obj = schema.as_object()?;
    if let Some(JsonValue::String(d)) = obj.get("description") {
        return Some(d.clone());
    }
    if let Some(JsonValue::String(t)) = obj.get("title") {
        return Some(t.clone());
    }
    None
}

// ─────────────────────────────────────────── strongly-connected components

/// Assign each type name an SCC id. Types in the same SCC are mutually
/// recursive; a direct (non-collection) field reference between them needs
/// [`Box`] to have a finite size. Tarjan's algorithm over a deterministic node
/// ordering keeps the result stable.
fn compute_sccs(defs: &[TypeDef]) -> HashMap<String, usize> {
    let mut edges: HashMap<&str, Vec<String>> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for def in defs {
        order.push(&def.name);
        let mut named = Vec::new();
        match &def.body {
            TypeBody::Struct(fields) => {
                fields.iter().for_each(|f| f.ty.collect_named(&mut named));
            }
            TypeBody::TaggedUnion { variants, .. } => {
                for v in variants {
                    v.fields.iter().for_each(|f| f.ty.collect_named(&mut named));
                }
            }
            TypeBody::UntaggedUnion(variants) => {
                variants.iter().for_each(|v| v.ty.collect_named(&mut named));
            }
            TypeBody::Alias(ty) => ty.collect_named(&mut named),
            TypeBody::StringEnum(_) => {}
        }
        edges.insert(&def.name, named);
    }

    let mut state = TarjanState {
        edges: &edges,
        index: HashMap::new(),
        low: HashMap::new(),
        on_stack: HashSet::new(),
        stack: Vec::new(),
        next_index: 0,
        next_scc: 0,
        result: HashMap::new(),
    };
    for name in &order {
        if !state.index.contains_key(*name) {
            state.strong_connect(name);
        }
    }
    state.result
}

struct TarjanState<'a> {
    edges: &'a HashMap<&'a str, Vec<String>>,
    index: HashMap<String, usize>,
    low: HashMap<String, usize>,
    on_stack: HashSet<String>,
    stack: Vec<String>,
    next_index: usize,
    next_scc: usize,
    result: HashMap<String, usize>,
}

impl TarjanState<'_> {
    fn strong_connect(&mut self, v: &str) {
        self.index.insert(v.to_string(), self.next_index);
        self.low.insert(v.to_string(), self.next_index);
        self.next_index += 1;
        self.stack.push(v.to_string());
        self.on_stack.insert(v.to_string());

        if let Some(neighbors) = self.edges.get(v) {
            let neighbors = neighbors.clone();
            for w in neighbors {
                if !self.index.contains_key(&w) {
                    self.strong_connect(&w);
                    let low_w = self.low[&w];
                    let low_v = self.low[v];
                    self.low.insert(v.to_string(), low_v.min(low_w));
                } else if self.on_stack.contains(&w) {
                    let idx_w = self.index[&w];
                    let low_v = self.low[v];
                    self.low.insert(v.to_string(), low_v.min(idx_w));
                }
            }
        }

        if self.low[v] == self.index[v] {
            let scc = self.next_scc;
            self.next_scc += 1;
            loop {
                let w = self.stack.pop().unwrap();
                self.on_stack.remove(&w);
                self.result.insert(w.clone(), scc);
                if w == v {
                    break;
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────── rendering

struct Renderer<'a> {
    scc: &'a HashMap<String, usize>,
}

impl Renderer<'_> {
    /// Render a [`TypeRef`]. `boxable` is true when the value is stored inline
    /// (a struct field, tuple element, or newtype payload) so a same-SCC named
    /// reference must be boxed; collection element positions pass `false`.
    fn ty(&self, ty: &TypeRef, owner_scc: usize, boxable: bool) -> String {
        match ty {
            TypeRef::Named(name) => {
                if boxable && self.scc.get(name) == Some(&owner_scc) {
                    format!("Box<{name}>")
                } else {
                    name.clone()
                }
            }
            TypeRef::Vec(inner) => format!("Vec<{}>", self.ty(inner, owner_scc, false)),
            TypeRef::Map(inner) => {
                format!(
                    "std::collections::HashMap<String, {}>",
                    self.ty(inner, owner_scc, false)
                )
            }
            TypeRef::Tuple(items) => {
                let inner: Vec<String> =
                    items.iter().map(|t| self.ty(t, owner_scc, true)).collect();
                format!("({},)", inner.join(", "))
            }
            TypeRef::Prim(p) => p.to_string(),
            TypeRef::Json => "serde_json::Value".to_string(),
        }
    }

    fn field_type(&self, field: &Field, owner_scc: usize) -> String {
        let inner = self.ty(&field.ty, owner_scc, true);
        if field.optional {
            format!("Option<{inner}>")
        } else {
            inner
        }
    }

    fn write_def(&self, w: &mut CodeWriter, def: &TypeDef) {
        let owner_scc = self.scc[&def.name];
        w.doc(def.doc.as_deref());
        match &def.body {
            TypeBody::Alias(ty) => {
                w.line(&format!(
                    "pub type {} = {};",
                    def.name,
                    self.ty(ty, owner_scc, false)
                ));
            }
            TypeBody::Struct(fields) => self.write_struct(w, def, owner_scc, fields),
            TypeBody::StringEnum(variants) => self.write_string_enum(w, def, variants),
            TypeBody::TaggedUnion { tag, variants } => {
                self.write_tagged_union(w, def, owner_scc, tag, variants)
            }
            TypeBody::UntaggedUnion(variants) => {
                self.write_untagged_union(w, def, owner_scc, variants)
            }
        }
    }

    fn write_struct(&self, w: &mut CodeWriter, def: &TypeDef, owner_scc: usize, fields: &[Field]) {
        w.line("#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]");
        if fields.is_empty() {
            w.line(&format!("pub struct {} {{}}", def.name));
        } else {
            w.line(&format!("pub struct {} {{", def.name));
            w.indent();
            for field in fields {
                self.write_field_in(w, field, owner_scc, true);
            }
            w.dedent();
            w.line("}");
        }
        self.write_struct_ctor(w, def, owner_scc, fields);
    }

    /// Every field's Rust identifier is unique, meaning a `new` constructor
    /// can bind each field exactly once (as a parameter or a struct-literal
    /// shorthand) without a name clash. Two distinct wire property names can
    /// collide onto the same sanitized Rust identifier (e.g. `"someId"` and
    /// `"some_id"`); the struct definition itself is already unrepresentable
    /// in that case, so the safest thing the constructor can do is not exist.
    fn struct_ctor_is_safe(fields: &[Field]) -> bool {
        let mut seen = HashSet::new();
        fields.iter().all(|f| seen.insert(f.rust_name.as_str()))
    }

    /// Emit `impl StructName { pub fn new(...) -> Self { ... } }` for a
    /// generated struct, one inherent impl per struct so there is never a
    /// duplicate/conflicting `new` across impls.
    ///
    /// Parameters are exactly the struct's required (non-optional,
    /// non-flatten) fields, in the same order they were emitted on the
    /// struct. Optional fields are initialized to `None`; the `#[serde(flatten)]`
    /// open-object extra map is initialized with `Default::default()`. A
    /// struct with no required fields still gets a niladic `new()`. Skipped
    /// entirely when [`Self::struct_ctor_is_safe`] reports a name clash.
    fn write_struct_ctor(
        &self,
        w: &mut CodeWriter,
        def: &TypeDef,
        owner_scc: usize,
        fields: &[Field],
    ) {
        if !Self::struct_ctor_is_safe(fields) {
            return;
        }
        let required: Vec<&Field> = fields
            .iter()
            .filter(|f| !f.optional && !f.flatten)
            .collect();

        w.blank();
        w.line(&format!("impl {} {{", def.name));
        w.indent();
        w.doc(Some(
            "Construct with all required fields; optional fields default to `None`\n\
             and any open-object extra map defaults to `Default::default()`.",
        ));
        let params = required
            .iter()
            .map(|f| format!("{}: {}", f.rust_name, self.field_type(f, owner_scc)))
            .collect::<Vec<_>>()
            .join(", ");
        w.line(&format!("pub fn new({params}) -> Self {{"));
        w.indent();
        if fields.is_empty() {
            w.line("Self {}");
        } else {
            w.line("Self {");
            w.indent();
            for field in fields {
                if field.flatten {
                    w.line(&format!("{}: Default::default(),", field.rust_name));
                } else if field.optional {
                    w.line(&format!("{}: None,", field.rust_name));
                } else {
                    w.line(&format!("{},", field.rust_name));
                }
            }
            w.dedent();
            w.line("}");
        }
        w.dedent();
        w.line("}");
        w.dedent();
        w.line("}");
    }

    fn write_field_in(&self, w: &mut CodeWriter, field: &Field, owner_scc: usize, with_vis: bool) {
        w.doc(field.doc.as_deref());
        if field.flatten {
            w.line("#[serde(flatten)]");
        } else {
            let wire = &field.wire_name;
            if strip_raw(&field.rust_name) != wire {
                w.line(&format!("#[serde(rename = {})]", quote_str(wire)));
            }
            if field.optional {
                w.line("#[serde(default, skip_serializing_if = \"Option::is_none\")]");
            }
        }
        w.line(&format!(
            "{}{}: {},",
            if with_vis { "pub " } else { "" },
            field.rust_name,
            self.field_type(field, owner_scc)
        ));
    }

    fn write_string_enum(
        &self,
        w: &mut CodeWriter,
        def: &TypeDef,
        variants: &[(String, String, Option<String>)],
    ) {
        w.line("#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]");
        w.line(&format!("pub enum {} {{", def.name));
        w.indent();
        for (rust, wire, doc) in variants {
            w.doc(doc.as_deref());
            if rust != wire {
                w.line(&format!("#[serde(rename = {})]", quote_str(wire)));
            }
            w.line(&format!("{rust},"));
        }
        w.dedent();
        w.line("}");
    }

    fn write_tagged_union(
        &self,
        w: &mut CodeWriter,
        def: &TypeDef,
        owner_scc: usize,
        tag: &str,
        variants: &[TaggedVariant],
    ) {
        w.line("#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]");
        w.line(&format!("#[serde(tag = {})]", quote_str(tag)));
        w.line(&format!("pub enum {} {{", def.name));
        w.indent();
        for variant in variants {
            w.doc(variant.doc.as_deref());
            if variant.rust_name != variant.wire_tag {
                w.line(&format!(
                    "#[serde(rename = {})]",
                    quote_str(&variant.wire_tag)
                ));
            }
            if variant.fields.is_empty() {
                w.line(&format!("{},", variant.rust_name));
            } else {
                w.line(&format!("{} {{", variant.rust_name));
                w.indent();
                for field in &variant.fields {
                    self.write_field_in(w, field, owner_scc, false);
                }
                w.dedent();
                w.line("},");
            }
        }
        w.dedent();
        w.line("}");
    }

    fn write_untagged_union(
        &self,
        w: &mut CodeWriter,
        def: &TypeDef,
        owner_scc: usize,
        variants: &[UntaggedVariant],
    ) {
        w.line("#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]");
        w.line("#[serde(untagged)]");
        w.line(&format!("pub enum {} {{", def.name));
        w.indent();
        for variant in variants {
            w.line(&format!(
                "{}({}),",
                variant.rust_name,
                self.ty(&variant.ty, owner_scc, true)
            ));
        }
        w.dedent();
        w.line("}");
    }
}

/// Serde internally-tagged variants serialize under their Rust identifier by
/// default; the `rename` is emitted when the wire tag differs.
fn quote_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ─────────────────────────────────────────────────────────────── entry point

pub(super) fn generate(
    schema: &HubRpcInterfaceSchema,
    options: &GenerateRustOptions,
) -> GeneratedRust {
    let mut collector = Collector::new(schema);
    let methods = collector.run(schema);

    let scc = compute_sccs(&collector.defs);
    let renderer = Renderer { scc: &scc };

    let mut w = CodeWriter::new();
    write_header(&mut w, schema, &collector.unsupported, options);

    for def in &collector.defs {
        w.blank();
        renderer.write_def(&mut w, def);
    }

    w.blank();
    write_client(&mut w, schema, &methods, &renderer, options);

    GeneratedRust {
        code: w.finish(),
        unsupported: collector.unsupported.clone(),
    }
}

fn write_header(
    w: &mut CodeWriter,
    schema: &HubRpcInterfaceSchema,
    unsupported: &[String],
    options: &GenerateRustOptions,
) {
    w.line(&format!(
        "// @generated by {}::schema::codegen from interface `{}`.",
        options.hubrpc_path, schema.id
    ));
    w.line("// Do not edit by hand; regenerate from the interface schema instead.");
    if !unsupported.is_empty() {
        w.line("//");
        w.line("// Unsupported constructs (lowered to serde_json::Value / fallbacks):");
        for note in unsupported {
            w.line(&format!("//   - {note}"));
        }
    }
    w.line("#![allow(dead_code, clippy::all, non_camel_case_types)]");
    // Keep the byte-exact, deterministic output stable under a downstream
    // `cargo fmt`; regenerate from the schema instead of reformatting.
    w.line("#![cfg_attr(rustfmt, rustfmt::skip)]");
}

fn write_client(
    w: &mut CodeWriter,
    schema: &HubRpcInterfaceSchema,
    methods: &[MethodModel],
    renderer: &Renderer,
    options: &GenerateRustOptions,
) {
    let hub = &options.hubrpc_path;
    let client = options
        .client_name
        .clone()
        .unwrap_or_else(|| format!("{}Client", to_pascal_case(&schema.id)));

    w.doc(schema.description.as_deref());
    w.line("#[derive(Clone)]");
    w.line(&format!("pub struct {client}<C> {{"));
    w.indent();
    w.line("caller: C,");
    w.line("prefix: String,");
    w.dedent();
    w.line("}");
    w.blank();

    w.line(&format!("impl<C: {hub}::prelude::RpcCall> {client}<C> {{"));
    w.indent();
    w.line("/// The interface id (the `id` half of `id@hash`).");
    w.line(&format!(
        "pub const INTERFACE_ID: &'static str = {};",
        quote_str(&schema.id)
    ));
    w.blank();
    w.line("/// Address members with the interface-qualified name `id::member`.");
    w.line("pub fn new(caller: C) -> Self {");
    w.indent();
    w.line("Self { caller, prefix: format!(\"{}::\", Self::INTERFACE_ID) }");
    w.dedent();
    w.line("}");
    w.blank();
    w.line("/// Address members by bare method name (root-addressed, e.g. a CDP channel).");
    w.line("pub fn root(caller: C) -> Self {");
    w.indent();
    w.line("Self { caller, prefix: String::new() }");
    w.dedent();
    w.line("}");
    w.blank();
    w.line("/// Address members under a specific service id: `service::id::member`.");
    w.line("pub fn with_service(caller: C, service_id: &str) -> Self {");
    w.indent();
    w.line("Self { caller, prefix: format!(\"{}::{}::\", service_id, Self::INTERFACE_ID) }");
    w.dedent();
    w.line("}");
    w.blank();
    w.line("/// Address members with an explicit wire-name prefix (e.g. `\"Page.\"`).");
    w.line("pub fn with_prefix(caller: C, prefix: impl Into<String>) -> Self {");
    w.indent();
    w.line("Self { caller, prefix: prefix.into() }");
    w.dedent();
    w.line("}");
    w.blank();
    w.line("fn method_name(&self, member: &str) -> String {");
    w.indent();
    w.line("format!(\"{}{}\", self.prefix, member)");
    w.dedent();
    w.line("}");

    for method in methods {
        w.blank();
        write_client_method(w, method, renderer, hub);
    }

    w.dedent();
    w.line("}");
}

/// Read `x-hubrpc-codegen.kind` from a method's preserved extensions, if present.
///
/// This is the generic mechanism for codegen directives: the schema model keeps
/// unknown `x-…` keys via `#[serde(flatten)]`, and the generator interprets the
/// `x-hubrpc-codegen` object. `kind = "serverNotification"` marks a result-less
/// method as a server→client event (no client send method is generated).
fn codegen_kind(method: &MethodSchema) -> Option<&str> {
    method.extension("x-hubrpc-codegen")?.get("kind")?.as_str()
}

fn write_client_method(w: &mut CodeWriter, method: &MethodModel, renderer: &Renderer, hub: &str) {
    let params_ty = renderer.ty(&method.params, usize::MAX, true);
    let err = format!("{hub}::prelude::JsonRpcError");
    w.doc(method.doc.as_deref());

    // Server→client events: emit the addressed wire name (so consumers can match
    // inbound notifications) and the payload type, but NOT a client send method.
    if method.server_notification {
        w.line(
            "/// Server notification (server→client, `x-hubrpc-codegen.kind = \"serverNotification\"`).",
        );
        w.line(&format!(
            "/// No client send method is generated; decode inbound payloads as `{params_ty}`."
        ));
        w.line(&format!(
            "pub fn {}_event_name(&self) -> String {{",
            method.rust_name
        ));
        w.indent();
        w.line(&format!(
            "self.method_name({})",
            quote_str(&method.wire_name)
        ));
        w.dedent();
        w.line("}");
        return;
    }

    match &method.result {
        Some(result) => {
            let result_ty = renderer.ty(result, usize::MAX, true);
            w.line(&format!(
                "pub async fn {}(&self, params: {params_ty}) -> Result<{result_ty}, {err}> {{",
                method.rust_name
            ));
            w.indent();
            write_encode_params(w, hub);
            w.line(&format!(
                "let __v = self.caller.call(&self.method_name({}), __p).await?;",
                quote_str(&method.wire_name)
            ));
            w.line("serde_json::from_value(__v).map_err(|e| {");
            w.indent();
            w.line(&format!(
                "{err}::new({hub}::prelude::error_codes::INTERNAL_ERROR, e.to_string())"
            ));
            w.dedent();
            w.line("})");
            w.dedent();
            w.line("}");
        }
        None => {
            w.line(&format!(
                "pub async fn {}(&self, params: {params_ty}) -> Result<(), {err}> {{",
                method.rust_name
            ));
            w.indent();
            write_encode_params(w, hub);
            w.line(&format!(
                "self.caller.notify(&self.method_name({}), __p).await",
                quote_str(&method.wire_name)
            ));
            w.dedent();
            w.line("}");
        }
    }
}

fn write_encode_params(w: &mut CodeWriter, hub: &str) {
    w.line("let __p = serde_json::to_value(&params).map_err(|e| {");
    w.indent();
    w.line(&format!(
        "{hub}::prelude::JsonRpcError::new({hub}::prelude::error_codes::INTERNAL_ERROR, e.to_string())"
    ));
    w.dedent();
    w.line("})?;");
}
