use crate::api::service_api::{CpuProfileFunctionSnapshot, CpuProfileNodeSnapshot, SourceLocation};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CpuProfileFunctionKey {
    name: String,
    source_url: String,
    line: u32,
    column: u32,
}

pub(crate) fn cpu_profile_function_key(node: &CpuProfileNodeSnapshot) -> CpuProfileFunctionKey {
    let location = node
        .authored_location
        .as_ref()
        .cloned()
        .unwrap_or_else(|| cpu_profile_generated_location(node));
    CpuProfileFunctionKey {
        name: node.call_frame.function_name.clone(),
        source_url: location.source_url,
        line: location.line,
        column: location.column,
    }
}

pub(crate) fn cpu_profile_function(node: &CpuProfileNodeSnapshot) -> CpuProfileFunctionSnapshot {
    CpuProfileFunctionSnapshot {
        name: node.call_frame.function_name.clone(),
        breadcrumb: node.breadcrumb.clone(),
        generated_location: cpu_profile_generated_location(node),
        authored_location: node.authored_location.clone(),
        self_time_micros: 0,
        total_time_micros: 0,
        sample_count: 0,
    }
}

pub(crate) fn cpu_profile_generated_location(node: &CpuProfileNodeSnapshot) -> SourceLocation {
    SourceLocation {
        source_url: node.call_frame.url.clone(),
        line: u32::try_from(node.call_frame.line_number)
            .map(|line| line.saturating_add(1))
            .unwrap_or(0),
        column: u32::try_from(node.call_frame.column_number)
            .map(|column| column.saturating_add(1))
            .unwrap_or(0),
    }
}
