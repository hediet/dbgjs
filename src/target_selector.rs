use crate::service_api::TargetSnapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetSelectorMatch {
    Friendly,
    Canonical,
    Qualified,
}

pub struct TargetSelectorCandidate<'a> {
    pub target: &'a TargetSnapshot,
    pub connection_id: &'a str,
    pub generation: u64,
}

pub fn select_target_matches<'a, T>(
    targets: &'a [T],
    connections: impl IntoIterator<Item = (&'a str, u64)>,
    selector: Option<&str>,
    candidate: impl Fn(&'a T) -> TargetSelectorCandidate<'a>,
) -> Result<Vec<&'a T>, String> {
    let Some(selector) = selector else {
        return Ok(targets.iter().collect());
    };
    let ranked = targets
        .iter()
        .filter_map(|item| {
            let target = candidate(item);
            match_target_selector(
                target.target,
                target.connection_id,
                target.generation,
                selector,
            )
            .map(|rank| (rank, item))
        })
        .collect::<Vec<_>>();
    let best_rank = ranked.iter().map(|(rank, _)| *rank).max();
    if best_rank.is_some_and(|rank| rank >= TargetSelectorMatch::Canonical) {
        return Ok(ranked
            .into_iter()
            .filter(|(rank, _)| Some(*rank) == best_rank)
            .map(|(_, item)| item)
            .collect());
    }

    // A qualified identity must not become a title/URL search when its generation disappears.
    if let Some((connection_id, current_generation, rest)) = connections
        .into_iter()
        .filter_map(|(connection_id, generation)| {
            selector
                .strip_prefix(connection_id)
                .and_then(|rest| rest.strip_prefix('/'))
                .map(|rest| (connection_id, generation, rest))
        })
        .max_by_key(|(connection_id, _, _)| connection_id.len())
    {
        if let Some((_, generation)) = rest.rsplit_once('@')
            && let Ok(generation) = generation.parse::<u64>()
            && generation != current_generation
        {
            return Err(format!(
                "target selector '{selector}' has stale connection generation {generation}; connection '{connection_id}' is at generation {current_generation}",
            ));
        }
        return Err(target_selector_not_found(selector));
    }
    if ranked.is_empty()
        || selector.rsplit_once('@').is_some_and(|(path, generation)| {
            path.contains('/') && generation.parse::<u64>().is_ok()
        })
    {
        return Err(target_selector_not_found(selector));
    }
    Ok(ranked.into_iter().map(|(_, item)| item).collect())
}

fn target_selector_not_found(selector: &str) -> String {
    format!(
        "target selector '{selector}' did not match a discovered target; target discovery may be incomplete"
    )
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
    let unqualified = format!("{connection_id}/{target_id}");
    if requested == Some(qualified.as_str()) {
        qualified
    } else if requested == Some(unqualified.as_str()) {
        unqualified
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
