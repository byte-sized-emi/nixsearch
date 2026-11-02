//! Read-only MCP frontend served at `/-/mcp`.
//!
//! Two tools only: `search` (bounded hit list, no descriptions beyond one line) and `get_entry`
//! (bounded detail for a single entry). Everything an agent needs is discoverable from a search
//! hit, so there is no separate source/ref listing tool.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler, schemars, tool, tool_handler, tool_router};

use nixsearch_config::app::AppConfig;
use nixsearch_core::document::{DocText, SearchDocument};
use nixsearch_core::source_link::SourceLinkResolver;
use nixsearch_index::search::EntryLookupResult;
use nixsearch_service::{EntryRequest, SearchRequest, SearchService};

use crate::templates::results::source_link_config_for_document;

/// Hits per `search` call. Not caller-controlled.
const MAX_HITS: usize = 10;
/// Deepest offset a caller may page to.
const MAX_OFFSET: usize = 90;
const MAX_QUERY_CHARS: usize = 256;
const MAX_IDENT_CHARS: usize = 128;
const MAX_SUMMARY_CHARS: usize = 160;
const MAX_TEXT_CHARS: usize = 1_500;
const MAX_VALUE_CHARS: usize = 600;
const MAX_DECLARATIONS: usize = 5;
const MAX_CANDIDATES: usize = 10;
/// Hard ceiling for a single tool response.
const MAX_RESPONSE_CHARS: usize = 8_000;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_INSTRUCTIONS_SOURCES: usize = 20;
const MAX_INSTRUCTIONS_REFS: usize = 6;

pub(crate) fn service(
    config: Arc<AppConfig>,
    search: SearchService,
) -> StreamableHttpService<SearchServer, NeverSessionManager> {
    let server = SearchServer {
        instructions: instructions_for(&config),
        config: Arc::clone(&config),
        search,
    };

    // Stateless: no per-client sessions or open SSE streams to exhaust.
    let mut transport = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_max_request_body_bytes(MAX_REQUEST_BYTES);
    // rmcp only accepts loopback Host headers by default; a public deployment needs its own.
    if let Some(host) = config
        .server
        .public_url
        .as_deref()
        .and_then(|url| url::Url::parse(url).ok())
        .and_then(|url| url.host_str().map(ToOwned::to_owned))
    {
        transport = transport.with_allowed_hosts(["localhost", "127.0.0.1", "::1", &host]);
    }

    StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        transport,
    )
}

#[derive(Debug, Clone)]
pub(crate) struct SearchServer {
    config: Arc<AppConfig>,
    search: SearchService,
    instructions: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct SearchParams {
    /// Search query, same syntax as the web UI (`git enable`, `"exact phrase"`).
    query: String,
    /// Restrict results to one of the sources listed in the server instructions.
    source: Option<String>,
    /// Restrict results to one ref, as shown in the results.
    ref_id: Option<String>,
    /// Skip this many results; at most 90.
    offset: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct EntryParams {
    /// Source name, e.g. `nixpkgs` (the part before the `@` in search results).
    source: String,
    /// Exact attribute name, e.g. `programs.git.enable` or `pkgs.git`.
    name: String,
    /// Ref id, e.g. `nixos-unstable` (the part after the `@`); defaults to the source's default ref.
    ref_id: Option<String>,
}

#[tool_router]
impl SearchServer {
    #[tool(
        name = "search",
        description = "Search Nix packages and options. Returns up to 10 one-line hits formatted \
                       as `source@ref kind name — summary`; pass a hit's source and name to \
                       `get_entry` for details."
    )]
    async fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_result(self.search_text(params))
    }

    #[tool(
        name = "get_entry",
        description = "Look up one package or option by exact name. Returns a truncated \
                       description, metadata, and where the entry is declared."
    )]
    async fn get_entry(
        &self,
        Parameters(params): Parameters<EntryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_result(self.entry_text(params))
    }
}

#[tool_handler]
impl ServerHandler for SearchServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("nixsearch", env!("CARGO_PKG_VERSION")))
            .with_instructions(self.instructions.clone())
    }
}

impl SearchServer {
    fn search_text(&self, params: SearchParams) -> Result<String, String> {
        let query = params.query.trim().to_owned();
        if query.is_empty() {
            return Err("query must not be empty".to_owned());
        }
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err(format!(
                "query must be at most {MAX_QUERY_CHARS} characters"
            ));
        }
        let offset = params.offset.unwrap_or(0);
        if offset > MAX_OFFSET {
            return Err(format!(
                "offset must be at most {MAX_OFFSET}; narrow the query instead of paging deeper"
            ));
        }
        if let Some(source) = &params.source {
            check_ident("source", source)?;
        }
        if let Some(ref_id) = &params.ref_id {
            check_ident("ref_id", ref_id)?;
        }

        let snapshot = self.search.snapshot();
        let result = self
            .search
            .search_with_snapshot(
                &snapshot,
                SearchRequest {
                    query: query.clone(),
                    source: params.source,
                    ref_id: params.ref_id,
                    ref_set: None,
                    offset,
                    limit: MAX_HITS,
                },
            )
            .map_err(|error| error.to_string())?;

        if result.hits.is_empty() {
            return Ok(match result.total {
                0 => format!("no results for `{query}`"),
                total => format!("no results for `{query}` at offset {offset} (total {total})"),
            });
        }

        let mut out = format!(
            "{} result{} for `{query}`, showing {} from offset {offset}\n",
            result.total,
            if result.total == 1 { "" } else { "s" },
            result.hits.len()
        );
        for hit in &result.hits {
            let common = hit.document.common();
            out.push_str(&format!(
                "{}@{} {} {} — {}\n",
                common.source,
                common.ref_id,
                common.kind.as_str(),
                common.name,
                summary_for_document(&hit.document).unwrap_or_default()
            ));
        }
        if offset + result.hits.len() < result.total {
            match (offset + MAX_HITS).min(MAX_OFFSET) {
                next if next > offset => out.push_str(&format!("more results: offset={next}\n")),
                _ => out.push_str("more results available; narrow the query to see them\n"),
            }
        }

        Ok(out)
    }

    fn entry_text(&self, params: EntryParams) -> Result<String, String> {
        check_ident("source", &params.source)?;
        check_ident("name", &params.name)?;
        if let Some(ref_id) = &params.ref_id {
            check_ident("ref_id", ref_id)?;
        }

        let snapshot = self.search.snapshot();
        let found = self
            .search
            .find_entry_with_snapshot(
                &snapshot,
                EntryRequest {
                    source: params.source.clone(),
                    ref_id: params.ref_id,
                    name: params.name.clone(),
                },
            )
            .map_err(|error| error.to_string())?;

        match found {
            EntryLookupResult::Found(document) => Ok(render_entry(&self.config, &document)),
            EntryLookupResult::NotFound => Err(format!(
                "no entry `{}` in source `{}`",
                params.name, params.source
            )),
            EntryLookupResult::Ambiguous(documents) => {
                let mut message = format!(
                    "`{}` is ambiguous in source `{}` ({} candidates)",
                    params.name,
                    params.source,
                    documents.len()
                );
                for document in documents.iter().take(MAX_CANDIDATES) {
                    message.push_str(&format!("\n{}", document.common().kind.as_str()));
                }
                Err(message)
            }
        }
    }
}

fn render_entry(config: &AppConfig, document: &SearchDocument) -> String {
    let common = document.common();
    let mut out = format!(
        "{}@{} {} {}\n",
        common.source,
        common.ref_id,
        common.kind.as_str(),
        common.name
    );
    if let Some(revision) = &common.revision {
        push_section(&mut out, "revision", &revision[..revision.len().min(8)]);
    }

    let resolver = source_link_config_for_document(config, common)
        .map(|config| SourceLinkResolver::new(config, common.revision.as_deref()));

    match document {
        SearchDocument::Option(option) => {
            if let Some(description) = &option.description {
                push_section(&mut out, "description", &doc_text(description));
            }
            if let Some(option_type) = &option.option_type {
                push_section(&mut out, "type", option_type);
            }
            if let Some(default) = &option.default {
                push_section(&mut out, "default", &default.plain_text());
            }
            if let Some(example) = &option.example {
                push_section(&mut out, "example", &example.plain_text());
            }
            push_declarations(
                &mut out,
                option.declarations.iter().map(|declaration| {
                    let url = resolver
                        .as_ref()
                        .and_then(|resolver| resolver.resolve_declaration(declaration));
                    (declaration.name.as_str(), url)
                }),
            );
        }
        SearchDocument::Package(package) => {
            if let Some(description) = &package.description {
                push_section(&mut out, "description", description);
            }
            if let Some(pname) = &package.pname {
                push_section(&mut out, "pname", pname);
            }
            if let Some(version) = &package.version {
                push_section(&mut out, "version", version);
            }
            if let Some(homepage) = package.homepages.first() {
                push_section(&mut out, "homepage", homepage);
            }
            if let Some(position) = &package.position {
                let url = resolver
                    .as_ref()
                    .and_then(|resolver| resolver.resolve_package_position(position));
                push_declarations(&mut out, std::iter::once((position.as_str(), url)));
            }
        }
    }

    out
}

fn instructions_for(config: &AppConfig) -> String {
    let mut out = String::from(
        "Search the Nix ecosystem index. Call `search`, then `get_entry` with a result's source \
         and name when you need more detail. Pass `source` and `ref_id` to `search` to narrow it. \
         Available sources (kind in parentheses, `*` marks the default ref):",
    );

    for (id, source) in config.sources.iter().take(MAX_INSTRUCTIONS_SOURCES) {
        out.push_str(&format!(
            "\n- {} ({})",
            truncate(id, MAX_IDENT_CHARS),
            source.kind.as_str()
        ));

        let refs = source
            .refs
            .iter()
            .take(MAX_INSTRUCTIONS_REFS)
            .map(|candidate| {
                if source.default_ref.as_deref() == Some(candidate.id.as_str()) {
                    format!("{}*", candidate.id)
                } else {
                    candidate.id.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");

        if !refs.is_empty() {
            out.push_str(": ");
            out.push_str(&refs);
        }
    }

    if config.sources.len() > MAX_INSTRUCTIONS_SOURCES {
        out.push_str("\n- …");
    }

    out
}

fn tool_result(text: Result<String, String>) -> Result<CallToolResult, ErrorData> {
    Ok(match text {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(capped(text))]),
        Err(message) => tool_error(message),
    })
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

fn check_ident(label: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if value.chars().count() > MAX_IDENT_CHARS {
        return Err(format!(
            "{label} must be at most {MAX_IDENT_CHARS} characters"
        ));
    }

    Ok(())
}

fn push_section(out: &mut String, label: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    out.push_str(label);
    out.push_str(": ");
    out.push_str(&truncate(value, MAX_TEXT_CHARS));
    out.push('\n');
}

fn push_declarations<'a>(
    out: &mut String,
    declarations: impl Iterator<Item = (&'a str, Option<String>)>,
) {
    for (index, (name, url)) in declarations.take(MAX_DECLARATIONS).enumerate() {
        if index == 0 {
            out.push_str("declared:\n");
        }
        out.push_str("  ");
        out.push_str(&truncate(name, MAX_IDENT_CHARS));
        if let Some(url) = url {
            out.push(' ');
            out.push_str(&truncate(&url, MAX_VALUE_CHARS));
        }
        out.push('\n');
    }
}

fn summary_for_document(document: &SearchDocument) -> Option<String> {
    let description = match document {
        SearchDocument::Option(option) => option
            .description
            .as_ref()
            .map(|description| description.plain_text().into_owned()),
        SearchDocument::Package(package) => package.description.clone(),
    }?;
    let line = description
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;

    Some(truncate(line, MAX_SUMMARY_CHARS))
}

fn doc_text(value: &DocText) -> String {
    match value {
        DocText::Markdown(value) => value.clone(),
        value => value.plain_text().into_owned(),
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }

    let mut truncated: String = value.chars().take(max_chars).collect();
    truncated.push('…');

    truncated
}

fn capped(text: String) -> String {
    if text.chars().count() <= MAX_RESPONSE_CHARS {
        return text;
    }

    let mut capped: String = text.chars().take(MAX_RESPONSE_CHARS).collect();
    capped.push_str("\n(truncated)");

    capped
}

#[cfg(test)]
mod tests {
    use super::*;
    use nixsearch_core::document::{OptionDoc, PackageDoc};
    use nixsearch_core::ingest::IngestContext;

    fn context() -> IngestContext {
        IngestContext {
            source: "fixtures".into(),
            ref_id: "small".into(),
            revision: Some("0123456789abcdef".into()),
            repo: None,
        }
    }

    #[test]
    fn truncate_keeps_short_values_and_marks_long_ones() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "abc…");
        assert_eq!(truncate("äöüß", 3), "äöü…");
    }

    #[test]
    fn summary_uses_first_non_empty_description_line() {
        let mut option = OptionDoc::new(&context(), "programs.git.enable");
        option.description = Some(DocText::Markdown(
            "\n\n  Enable git.\n\nSecond line.".to_owned(),
        ));

        let summary = summary_for_document(&SearchDocument::Option(option)).unwrap();
        assert_eq!(summary, "Enable git.");

        let mut package = PackageDoc::new(&context(), "pkgs.git");
        package.description = Some("A distributed version control system".to_owned());
        assert_eq!(
            summary_for_document(&SearchDocument::Package(package)).as_deref(),
            Some("A distributed version control system")
        );
    }

    #[test]
    fn instructions_list_configured_sources_with_default_ref_marked() {
        let config = nixsearch_test_support::multi_ref_app_config("./data/indexes");
        let instructions = instructions_for(&config);

        assert!(
            instructions.contains("- fixtures (options): small*, stable"),
            "{instructions}"
        );
    }

    #[test]
    fn oversized_inputs_are_rejected() {
        assert!(check_ident("source", &"x".repeat(MAX_IDENT_CHARS + 1)).is_err());
        assert!(check_ident("source", "   ").is_err());
        assert!(check_ident("source", "nixpkgs").is_ok());
    }

    #[test]
    fn entry_render_caps_description_and_declarations() {
        let config = nixsearch_test_support::app_config("./data/indexes");
        let mut option = OptionDoc::new(&context(), "programs.git.enable");
        option.description = Some(DocText::Markdown("word ".repeat(10_000)));
        for index in 0..50 {
            option
                .declarations
                .push(nixsearch_core::source_link::Declaration {
                    name: format!("module{index}.nix:1"),
                    url: Some(format!("https://example.com/{}", "p".repeat(200))),
                });
        }

        let rendered = capped(render_entry(&config, &SearchDocument::Option(option)));

        assert!(rendered.chars().count() <= MAX_RESPONSE_CHARS + "\n(truncated)".len());
        assert!(rendered.contains("declared:"));
        assert!(!rendered.contains("module10.nix"));
    }
}
