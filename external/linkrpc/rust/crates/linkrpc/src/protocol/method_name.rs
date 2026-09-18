//! The linkrpc method-name grammar. Method strings use `::` to separate up to three
//! segments (port of TS `protocol/methodName.ts`):
//!
//!   - `"member"`                         — bare (preset-bound dispatch)
//!   - `"interfaceId::member"`            — interface form (root service)
//!   - `"serviceId::interfaceId::member"` — fully-qualified form
//!
//! A method is malformed if it has zero, more than three segments, or any empty segment.

/// A parsed JSON-RPC method string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedMethodName {
    /// `member` — no interface context.
    Bare { member: String },
    /// `interfaceId::member` — addressed to a root service.
    Interface {
        interface_id: String,
        member: String,
    },
    /// `serviceId::interfaceId::member` — fully qualified.
    Full {
        service_id: String,
        interface_id: String,
        member: String,
    },
}

impl ParsedMethodName {
    /// The trailing member name in all three forms.
    pub fn member(&self) -> &str {
        match self {
            ParsedMethodName::Bare { member }
            | ParsedMethodName::Interface { member, .. }
            | ParsedMethodName::Full { member, .. } => member,
        }
    }
}

/// Parse a method string. Returns `None` if any segment is empty or the segment count is
/// out of range (matches TS `parseMethodName` exactly).
pub fn parse_method_name(method: &str) -> Option<ParsedMethodName> {
    let parts: Vec<&str> = method.split("::").collect();
    if parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    match parts.as_slice() {
        [member] => Some(ParsedMethodName::Bare {
            member: (*member).to_string(),
        }),
        [interface_id, member] => Some(ParsedMethodName::Interface {
            interface_id: (*interface_id).to_string(),
            member: (*member).to_string(),
        }),
        [service_id, interface_id, member] => Some(ParsedMethodName::Full {
            service_id: (*service_id).to_string(),
            interface_id: (*interface_id).to_string(),
            member: (*member).to_string(),
        }),
        _ => None,
    }
}

/// Format a parsed method back to its `::`-joined string form. Round-trips with
/// [`parse_method_name`] for any well-formed value.
pub fn format_method_name(parsed: &ParsedMethodName) -> String {
    match parsed {
        ParsedMethodName::Bare { member } => member.clone(),
        ParsedMethodName::Interface {
            interface_id,
            member,
        } => format!("{interface_id}::{member}"),
        ParsedMethodName::Full {
            service_id,
            interface_id,
            member,
        } => format!("{service_id}::{interface_id}::{member}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_three_forms() {
        assert_eq!(
            parse_method_name("list"),
            Some(ParsedMethodName::Bare {
                member: "list".into()
            })
        );
        assert_eq!(
            parse_method_name("hubrpc.directory::list"),
            Some(ParsedMethodName::Interface {
                interface_id: "hubrpc.directory".into(),
                member: "list".into()
            })
        );
        assert_eq!(
            parse_method_name("acme::com.acme.pizza@ab12::order"),
            Some(ParsedMethodName::Full {
                service_id: "acme".into(),
                interface_id: "com.acme.pizza@ab12".into(),
                member: "order".into()
            })
        );
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(parse_method_name(""), None);
        assert_eq!(parse_method_name("a::"), None);
        assert_eq!(parse_method_name("::a"), None);
        assert_eq!(parse_method_name("a::b::c::d"), None);
    }

    #[test]
    fn round_trips() {
        for m in ["list", "i::m", "s::i::m"] {
            let p = parse_method_name(m).unwrap();
            assert_eq!(format_method_name(&p), m);
        }
    }
}
