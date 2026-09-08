use crate::service_api::TargetSnapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetSelectorMatch {
    Friendly,
    Canonical,
    Qualified,
}

pub fn qualified_target_selector(connection_id: &str, target_id: &str, generation: u64) -> String {
    format!("{connection_id}/{target_id}@{generation}")
}

pub fn resolved_target_selector(
    connection_id: &str,
    target_id: &str,
    generation: u64,
    requested: Option<&str>,
) -> String {
    let qualified = qualified_target_selector(connection_id, target_id, generation);
    if requested == Some(qualified.as_str()) {
        qualified
    } else {
        target_id.to_owned()
    }
}

pub fn match_target_selector(
    target: &TargetSnapshot,
    connection_id: &str,
    generation: u64,
    selector: &str,
) -> Option<TargetSelectorMatch> {
    if selector == qualified_target_selector(connection_id, &target.target_id, generation)
        || selector == format!("{connection_id}/{}", target.target_id)
    {
        Some(TargetSelectorMatch::Qualified)
    } else if selector == target.target_id {
        Some(TargetSelectorMatch::Canonical)
    } else if !selector.is_empty()
        && (target.target_type.eq_ignore_ascii_case(selector)
            || target
                .title
                .to_lowercase()
                .contains(&selector.to_lowercase())
            || target.url.to_lowercase().contains(&selector.to_lowercase()))
    {
        Some(TargetSelectorMatch::Friendly)
    } else {
        None
    }
}
