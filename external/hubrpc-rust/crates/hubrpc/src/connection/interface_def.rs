//! `InterfaceDefinition`: the runtime description of one interface — its id/docs and an
//! ordered set of typed members (request/notification). Lowers to a wire `HubRpcInterfaceSchema`
//! and exposes the content hash. Mirrors TS `connection/interfaceDefinition.ts`.
//!
//! Member param/result/stream schemas are carried as the already-normalized hubrpc
//! `JsonValue` subset (typically produced by [`crate::schema::schemars_to_subset`] in the macro,
//! or hand-authored). This keeps the definition language-agnostic and makes the hash match TS.

use std::sync::OnceLock;

use crate::protocol::json_value::JsonValue;
use crate::schema::hash::compute_interface_hash;
use crate::schema::interface_schema::{
    HubRpcInterfaceSchema, MemberAnnotations, MethodMap, MethodSchema,
};

/// Identity + human docs for an interface.
#[derive(Debug, Clone)]
pub struct InterfaceInfo {
    pub id: String,
    /// Normative description (part of the hash).
    pub description: Option<String>,
    /// Non-normative notes (stripped from the hash).
    pub comment: Option<String>,
}

impl InterfaceInfo {
    pub fn new(id: impl Into<String>) -> Self {
        InterfaceInfo {
            id: id.into(),
            description: None,
            comment: None,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Optional per-method documentation (mirrors TS `MemberDocs`).
#[derive(Debug, Clone, Default)]
pub struct MemberDocs {
    pub description: Option<String>,
    pub comment: Option<String>,
    pub annotations: Option<MemberAnnotations>,
}

/// A request method: typed params + result, optional bidirectional stream payload schemas.
#[derive(Debug, Clone)]
pub struct RequestMember {
    pub params_schema: JsonValue,
    pub result_schema: JsonValue,
    pub client_stream_schema: Option<JsonValue>,
    pub server_stream_schema: Option<JsonValue>,
    pub docs: MemberDocs,
}

/// A notification method: typed params, no result.
#[derive(Debug, Clone)]
pub struct NotificationMember {
    pub params_schema: JsonValue,
    pub docs: MemberDocs,
}

/// One member of an interface.
#[derive(Debug, Clone)]
pub enum Member {
    Request(Box<RequestMember>),
    Notification(NotificationMember),
}

impl Member {
    fn docs(&self) -> &MemberDocs {
        match self {
            Member::Request(m) => &m.docs,
            Member::Notification(m) => &m.docs,
        }
    }

    fn params_schema(&self) -> &JsonValue {
        match self {
            Member::Request(m) => &m.params_schema,
            Member::Notification(m) => &m.params_schema,
        }
    }
}

/// A runtime interface definition: id/docs + an ordered member list.
///
/// Member order is preserved for stable serialization but is **not** part of
/// identity: the lowered `methods` map is keyed by member name and the hash
/// sorts keys (spec 04 §4).
pub struct InterfaceDefinition {
    info: InterfaceInfo,
    members: Vec<(String, Member)>,
    hash: OnceLock<String>,
}

impl InterfaceDefinition {
    pub fn new(info: InterfaceInfo, members: Vec<(String, Member)>) -> Self {
        InterfaceDefinition {
            info,
            members,
            hash: OnceLock::new(),
        }
    }

    pub fn id(&self) -> &str {
        &self.info.id
    }

    pub fn info(&self) -> &InterfaceInfo {
        &self.info
    }

    pub fn members(&self) -> &[(String, Member)] {
        &self.members
    }

    /// Look up a member by name.
    pub fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|(n, _)| n == name).map(|(_, m)| m)
    }

    /// Content hash of this interface (`id@hash` pairs with [`Self::id`]).
    pub fn schema_hash(&self) -> &str {
        self.hash
            .get_or_init(|| compute_interface_hash(&self.build_schema(String::new())))
    }

    /// `id@hash` addressing form.
    pub fn id_at_hash(&self) -> String {
        format!("{}@{}", self.info.id, self.schema_hash())
    }

    /// Lower to a wire `HubRpcInterfaceSchema`, with `hash` filled in.
    pub fn to_schema(&self) -> HubRpcInterfaceSchema {
        let hash = self.schema_hash().to_string();
        self.build_schema(hash)
    }

    fn build_schema(&self, hash: String) -> HubRpcInterfaceSchema {
        let mut methods: MethodMap = MethodMap::new();
        for (name, member) in &self.members {
            methods.insert(name.clone(), to_method_schema(member));
        }
        HubRpcInterfaceSchema {
            id: self.info.id.clone(),
            hash,
            description: self.info.description.clone(),
            comment: self.info.comment.clone(),
            methods,
            components: None,
            extensions: Default::default(),
        }
    }
}

fn to_method_schema(member: &Member) -> MethodSchema {
    let docs = member.docs();
    let mut m = MethodSchema {
        params: member.params_schema().clone(),
        result: None,
        client_stream: None,
        server_stream: None,
        errors: None,
        summary: None,
        description: docs.description.clone(),
        comment: docs.comment.clone(),
        deprecated: None,
        annotations: docs.annotations.clone(),
        extensions: Default::default(),
    };

    if let Member::Request(req) = member {
        m.result = Some(req.result_schema.clone());
        m.client_stream = req.client_stream_schema.clone();
        m.server_stream = req.server_stream_schema.clone();
    }

    m
}
