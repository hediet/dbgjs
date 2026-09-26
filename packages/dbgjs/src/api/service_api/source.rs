use super::*;

#[link_rpc_interface(id = "dev.dbgjs.source")]
pub trait SourceApi {
    async fn set_source_formatting(
        context_id: String,
        mode: SourceFormattingMode,
    ) -> Result<SourceFormattingSettings, JsonRpcError>;

    async fn add_source_formatting_rule(
        context_id: String,
        mode: SourceFormattingMode,
        target_pattern: Option<String>,
        url_pattern: Option<String>,
    ) -> Result<SourceFormattingSettings, JsonRpcError>;

    async fn delete_source_formatting_rule(
        context_id: String,
        rule_id: String,
    ) -> Result<SourceFormattingSettings, JsonRpcError>;

    async fn list_sources(
        context_id: String,
        path: Option<String>,
    ) -> Result<Vec<SourceSnapshotInfo>, JsonRpcError>;

    async fn show_source_graph(
        context_id: String,
    ) -> Result<CompactedSourceGraphSnapshot, JsonRpcError>;

    async fn show_uncompacted_source_graph(
        context_id: String,
    ) -> Result<UncompactedSourceGraphSnapshot, JsonRpcError>;

    async fn show_source_tree(
        context_id: String,
        kind: SourceTreeKind,
    ) -> Result<SourceTreeSnapshot, JsonRpcError>;

    async fn resolve_sources(
        context_id: String,
        source: String,
    ) -> Result<UncompactedSourceGraphSnapshot, JsonRpcError>;

    async fn show_source(
        context_id: String,
        path: String,
        options: SourceDisplayOptions,
    ) -> Result<SourceContentSnapshot, JsonRpcError>;

    async fn grep_sources(
        context_id: String,
        options: SourceSearchOptions,
    ) -> Result<SourceSearchSnapshot, JsonRpcError>;

    async fn explain_source(
        context_id: String,
        path: String,
    ) -> Result<Vec<SourceGraphViewSnapshot>, JsonRpcError>;

    async fn map_source(
        context_id: String,
        path: String,
        line: u32,
        column: u32,
    ) -> Result<Vec<SourceMappingSnapshot>, JsonRpcError>;

    async fn evict_source_caches(context_id: String) -> Result<u32, JsonRpcError>;

    async fn export_sources(
        context_id: String,
        destination: String,
    ) -> Result<Vec<String>, JsonRpcError>;
}
