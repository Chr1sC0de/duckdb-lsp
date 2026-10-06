//! LSP transport and versioned background work. Interactive requests read only
//! immutable in-memory snapshots; Python and database I/O run separately.
use crate::{
    analysis::{Analysis, Catalog},
    config::{self, Effective, Settings},
    features, text,
    worker::Worker,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock};
use tower_lsp::{
    jsonrpc::{Error, Result},
    lsp_types::*,
    Client, LanguageServer,
};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionParams {
    pub uri: Url,
    pub profile: Option<Value>,
}
#[derive(Deserialize)]
pub struct UriParams {
    pub uri: Url,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Mapping {
    kind: String,
    source_start: usize,
    source_end: usize,
    target_start: usize,
    target_end: usize,
}
#[derive(Clone)]
struct Rendered {
    text: Arc<str>,
    analysis: Arc<Analysis>,
    mappings: Vec<Mapping>,
}
impl Rendered {
    fn target(&self, byte: usize) -> Option<usize> {
        let mut matches = self.mappings.iter().filter(|m| {
            m.kind == "literal"
                && m.source_start <= byte
                && byte <= m.source_end
                && m.source_end - m.source_start == m.target_end - m.target_start
        });
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(first.target_start + byte - first.source_start)
    }
}
#[derive(Clone)]
struct Document {
    uri: Url,
    path: PathBuf,
    root: PathBuf,
    language: String,
    version: i32,
    generation: u64,
    text: Arc<str>,
    analysis: Arc<Analysis>,
    config: Effective,
    rendered: Option<Rendered>,
    connection: String,
    error: Option<String>,
}
#[derive(Clone)]
struct Entry {
    profile: Value,
    catalog: Arc<Catalog>,
    root: PathBuf,
    refreshed: Option<Instant>,
    error: Option<String>,
    busy: bool,
}
struct State {
    client: Client,
    settings: RwLock<Settings>,
    roots: RwLock<Vec<PathBuf>>,
    docs: RwLock<HashMap<Url, Arc<Document>>>,
    catalogs: RwLock<HashMap<String, Entry>>,
    analyzer: Worker,
    database: Worker,
    formatter: Worker,
    analysis_gate: Mutex<()>,
    stopped: AtomicBool,
    snippets: AtomicBool,
}
pub struct Backend {
    state: Arc<State>,
}
fn connection_id(profile: &Value, root: &std::path::Path) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    profile.to_string().hash(&mut h);
    root.hash(&mut h);
    format!("{:016x}", h.finish())
}
fn config_diagnostics(c: &Effective) -> Vec<Diagnostic> {
    c.issues
        .iter()
        .map(|s| Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("duckdb-lsp/config".into()),
            message: s.clone(),
            ..Default::default()
        })
        .collect()
}
fn worker_params(d: &Document, s: &Settings, cat: &Catalog) -> Value {
    json!({"text":&*d.text,"path":d.path,"root":d.root,"render":d.config.render,"config":d.config,"catalog":cat,"native":s.native_diagnostics,"lint":s.lint})
}

impl State {
    async fn current(&self, d: &Document) -> bool {
        self.docs.read().await.get(&d.uri).is_some_and(|now| {
            now.generation == d.generation
                && now.version == d.version
                && Arc::ptr_eq(&now.text, &d.text)
        })
    }
    async fn catalog(&self, d: &Document) -> Arc<Catalog> {
        self.catalogs
            .read()
            .await
            .get(&d.connection)
            .map(|e| e.catalog.clone())
            .unwrap_or_default()
    }
    async fn register_profile(&self, profile: Value, root: PathBuf) -> String {
        let id = connection_id(&profile, &root);
        self.catalogs
            .write()
            .await
            .entry(id.clone())
            .or_insert(Entry {
                profile,
                catalog: Arc::default(),
                root,
                refreshed: None,
                error: None,
                busy: false,
            });
        id
    }
    fn schedule(self: &Arc<Self>, d: Arc<Document>) {
        let state = self.clone();
        tokio::spawn(async move {
            let settings = state.settings.read().await.clone();
            tokio::time::sleep(Duration::from_millis(
                settings.diagnostics_delay_ms.min(5000),
            ))
            .await;
            let _gate = state.analysis_gate.lock().await;
            if state.stopped.load(Ordering::Relaxed) || !state.current(&d).await {
                return;
            }
            let mut refreshed = (*d).clone();
            refreshed.config = config::resolve(&d.path, &d.root, &d.language, &d.text, &settings);
            let d = Arc::new(refreshed);
            let cat = state.catalog(&d).await;
            let mut diagnostics = config_diagnostics(&d.config);
            let result = state
                .analyzer
                .request(
                    &settings.python,
                    settings.worker_timeout_ms,
                    "analyze",
                    worker_params(&d, &settings, &cat),
                )
                .await;
            if !state.current(&d).await {
                return;
            }
            let mut updated = (*d).clone();
            match result {
                Ok(value) => {
                    diagnostics.extend(
                        serde_json::from_value::<Vec<Diagnostic>>(value["diagnostics"].clone())
                            .unwrap_or_default(),
                    );
                    if let Some(sql) = value["rendered"].as_str() {
                        updated.rendered = Some(Rendered {
                            text: Arc::from(sql),
                            analysis: Arc::new(Analysis::new(sql)),
                            mappings: serde_json::from_value(value["mappings"].clone())
                                .unwrap_or_default(),
                        });
                    }
                    if !d.config.render && !value["native"].as_bool().unwrap_or(false) {
                        diagnostics.extend(features::syntax_diagnostics(&d.text));
                    }
                    updated.error = None;
                }
                Err(error) => {
                    updated.error = Some(error.clone());
                    diagnostics.push(Diagnostic {
                        range: Range::default(),
                        severity: Some(DiagnosticSeverity::INFORMATION),
                        source: Some("duckdb-lsp/worker".into()),
                        message: error,
                        ..Default::default()
                    });
                    if !d.config.render {
                        diagnostics.extend(features::syntax_diagnostics(&d.text));
                    }
                }
            }
            // Hold the version lock through publication so an older task cannot
            // replace a newer buffer snapshot between its check and publish.
            let mut docs = state.docs.write().await;
            if docs.get(&d.uri).is_some_and(|now| {
                now.generation == d.generation && Arc::ptr_eq(&now.text, &d.text)
            }) {
                docs.insert(d.uri.clone(), Arc::new(updated));
                state
                    .client
                    .publish_diagnostics(d.uri.clone(), diagnostics, Some(d.version))
                    .await;
            }
        });
    }
    fn refresh(self: &Arc<Self>, id: String, force: bool) {
        let state = self.clone();
        tokio::spawn(async move {
            let settings = state.settings.read().await.clone();
            let entry = {
                let mut entries = state.catalogs.write().await;
                let Some(e) = entries.get_mut(&id) else {
                    return;
                };
                if e.busy
                    || (!force
                        && e.refreshed.is_some_and(|t| {
                            t.elapsed() < Duration::from_secs(settings.catalog_ttl_seconds.max(1))
                        }))
                {
                    return;
                }
                e.busy = true;
                e.clone()
            };
            let result = state
                .database
                .request(
                    &settings.python,
                    settings.worker_timeout_ms,
                    "catalog",
                    json!({"profile":entry.profile,"root":entry.root}),
                )
                .await;
            let mut entries = state.catalogs.write().await;
            let Some(e) = entries.get_mut(&id) else {
                return;
            };
            e.busy = false;
            e.refreshed = Some(Instant::now());
            match result.and_then(|v| {
                serde_json::from_value::<Catalog>(v).map_err(|_| "Invalid catalog response".into())
            }) {
                Ok(c) => {
                    e.catalog = Arc::new(c);
                    e.error = None;
                }
                Err(err) => e.error = Some(err),
            }
            drop(entries);
            let docs: Vec<_> = state
                .docs
                .read()
                .await
                .values()
                .filter(|d| d.connection == id)
                .cloned()
                .collect();
            for d in docs {
                state.schedule(d);
            }
        });
    }
    async fn reconfigure(self: &Arc<Self>) {
        let settings = self.settings.read().await.clone();
        let mut docs = self.docs.write().await;
        let mut changed = vec![];
        for doc in docs.values_mut() {
            let mut d = (**doc).clone();
            d.generation += 1;
            d.rendered = None;
            d.config = config::resolve(&d.path, &d.root, &d.language, &d.text, &settings);
            let d = Arc::new(d);
            *doc = d.clone();
            changed.push(d);
        }
        drop(docs);
        for d in changed {
            self.schedule(d);
        }
    }
}
impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            state: Arc::new(State {
                client,
                settings: RwLock::new(Settings::default()),
                roots: RwLock::new(vec![]),
                docs: RwLock::new(HashMap::new()),
                catalogs: RwLock::new(HashMap::new()),
                analyzer: Worker::default(),
                database: Worker::default(),
                formatter: Worker::default(),
                analysis_gate: Mutex::new(()),
                stopped: AtomicBool::new(false),
                snippets: AtomicBool::new(false),
            }),
        }
    }
    async fn document(&self, uri: &Url) -> Result<Arc<Document>> {
        self.state
            .docs
            .read()
            .await
            .get(uri)
            .cloned()
            .ok_or_else(|| Error::invalid_params("Document is not open"))
    }
    pub async fn status(&self, params: Option<UriParams>) -> Result<Value> {
        let docs = self.state.docs.read().await;
        let entries = self.state.catalogs.read().await;
        let result:Vec<_>=docs.values().filter(|d|params.as_ref().is_none_or(|p|p.uri==d.uri)).map(|d|{
            let e=entries.get(&d.connection);json!({"uri":d.uri,"version":d.version,"languageId":d.language,"provider":d.config.provider,"configFiles":d.config.files,"configIssues":d.config.issues,"rendering":d.config.render,"rendered":d.rendered.is_some(),"workerError":d.error,"connectionId":d.connection,"catalogTables":e.map(|e|e.catalog.tables.len()).unwrap_or(0),"catalogError":e.and_then(|e|e.error.clone()),"catalogRefreshing":e.is_some_and(|e|e.busy),"engineVersion":e.map(|e|e.catalog.engine_version.clone())})}).collect();
        Ok(json!({"documents":result}))
    }
    pub async fn set_connection(&self, params: ConnectionParams) -> Result<Value> {
        let doc = self.document(&params.uri).await?;
        let profile = params.profile.unwrap_or_else(|| json!({}));
        if !profile.is_object() {
            return Err(Error::invalid_params("profile must be an object or null"));
        }
        let id = self.state.register_profile(profile, doc.root.clone()).await;
        let mut docs = self.state.docs.write().await;
        let current = docs
            .get(&params.uri)
            .ok_or_else(|| Error::invalid_params("Document closed"))?;
        let mut d = (**current).clone();
        d.connection = id.clone();
        d.generation += 1;
        d.rendered = None;
        let d = Arc::new(d);
        docs.insert(params.uri, d.clone());
        drop(docs);
        self.state.refresh(id.clone(), true);
        self.state.schedule(d);
        Ok(json!({"connectionId":id}))
    }
    pub async fn catalog_document(&self, params: UriParams) -> Result<Value> {
        let id = params
            .uri
            .host_str()
            .ok_or_else(|| Error::invalid_params("Invalid catalog URI"))?;
        let parts: Vec<String> = params
            .uri
            .path_segments()
            .unwrap_or_else(|| "".split('/'))
            .map(|s| percent_decode(s))
            .collect();
        let entries = self.state.catalogs.read().await;
        let table = entries
            .get(id)
            .and_then(|e| e.catalog.resolve(&parts))
            .ok_or_else(|| Error::invalid_params("Catalog object is no longer cached"))?;
        Ok(json!({"text":features::table_document(table),"languageId":"duckdbsql"}))
    }
    fn catalog_location(d: &Document, t: &crate::analysis::Table) -> Option<Location> {
        let mut uri = Url::parse(&format!("duckdb-lsp://{}/", d.connection)).ok()?;
        uri.path_segments_mut()
            .ok()?
            .clear()
            .extend([&t.catalog, &t.schema, &t.name]);
        Some(Location {
            uri,
            range: Range::new(Position::new(1, 0), Position::new(1, 0)),
        })
    }
}
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into()
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, p: InitializeParams) -> Result<InitializeResult> {
        self.state.snippets.store(
            serde_json::to_value(&p.capabilities)
                .ok()
                .and_then(|v| {
                    v.pointer("/textDocument/completion/completionItem/snippetSupport")
                        .and_then(Value::as_bool)
                })
                .unwrap_or(false),
            Ordering::Relaxed,
        );
        let opts = p.initialization_options.unwrap_or_else(|| json!({}));
        let s: Settings = serde_json::from_value(opts.get("duckdbLsp").unwrap_or(&opts).clone())
            .map_err(|_| Error::invalid_params("Invalid duckdbLsp settings"))?;
        let format = s.format;
        *self.state.settings.write().await = s;
        let mut roots: Vec<_> = p
            .workspace_folders
            .unwrap_or_default()
            .into_iter()
            .filter_map(|f| f.uri.to_file_path().ok())
            .collect();
        #[allow(deprecated)]
        if let Some(r) = p.root_uri.and_then(|u| u.to_file_path().ok()) {
            if !roots.contains(&r) {
                roots.push(r);
            }
        }
        *self.state.roots.write().await = roots;
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "duckdb-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                position_encoding: Some(PositionEncodingKind::UTF16),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".into(), "{".into(), " ".into()]),
                    ..Default::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".into(), ",".into()]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                document_formatting_provider: Some(OneOf::Left(format)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        "duckdb.refreshCatalog".into(),
                        "duckdb.explainConfiguration".into(),
                    ],
                    ..Default::default()
                }),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        })
    }
    async fn initialized(&self, _: InitializedParams) {
        let weak = Arc::downgrade(&self.state);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(state) = weak.upgrade() else {
                    break;
                };
                if state.stopped.load(Ordering::Relaxed) {
                    break;
                }
                let settings = state.settings.read().await.clone();
                if settings.catalog_ttl_seconds == 0 {
                    continue;
                }
                let mut ids: Vec<_> = state
                    .docs
                    .read()
                    .await
                    .values()
                    .map(|d| d.connection.clone())
                    .collect();
                ids.sort();
                ids.dedup();
                for id in ids {
                    state.refresh(id, false);
                }
            }
        });
        let _=self.state.client.register_capability(vec![Registration{id:"duckdb-config".into(),method:"workspace/didChangeWatchedFiles".into(),register_options:Some(json!({"watchers":[{"globPattern":"**/{.sqruff,.sqruff.ini,.sqlfluff,sqruff.toml,pyproject.toml,setup.cfg,tox.ini,pep8.ini,*.jinja,*.j2}"}]}))}]).await;
    }
    async fn shutdown(&self) -> Result<()> {
        self.state.stopped.store(true, Ordering::Relaxed);
        self.state.analyzer.close().await;
        self.state.database.close().await;
        self.state.formatter.close().await;
        Ok(())
    }
    async fn did_open(&self, p: DidOpenTextDocumentParams) {
        let t = p.text_document;
        if t.text.len() > 2 * 1024 * 1024 {
            self.state
                .client
                .log_message(
                    MessageType::WARNING,
                    "Document exceeds the 2 MiB analysis limit",
                )
                .await;
            return;
        }
        let path = t
            .uri
            .to_file_path()
            .unwrap_or_else(|_| PathBuf::from("untitled.sql"));
        let roots = self.state.roots.read().await;
        let root = roots
            .iter()
            .filter(|r| path.starts_with(r))
            .max_by_key(|r| r.components().count())
            .cloned()
            .unwrap_or_else(|| {
                path.parent()
                    .unwrap_or(std::path::Path::new("."))
                    .to_path_buf()
            });
        drop(roots);
        let s = self.state.settings.read().await.clone();
        let cfg = config::resolve(&path, &root, &t.language_id, &t.text, &s);
        let connection = self
            .state
            .register_profile(
                s.default_connection.clone().unwrap_or_else(|| json!({})),
                root.clone(),
            )
            .await;
        let d = Arc::new(Document {
            uri: t.uri.clone(),
            path,
            root,
            language: t.language_id,
            version: t.version,
            generation: 0,
            analysis: Arc::new(Analysis::new(&t.text)),
            text: Arc::from(t.text),
            config: cfg,
            rendered: None,
            connection: connection.clone(),
            error: None,
        });
        self.state.docs.write().await.insert(t.uri, d.clone());
        self.state.schedule(d);
        self.state.refresh(connection, false);
    }
    async fn did_change(&self, p: DidChangeTextDocumentParams) {
        let mut docs = self.state.docs.write().await;
        let Some(old) = docs.get(&p.text_document.uri) else {
            return;
        };
        if p.text_document.version <= old.version {
            return;
        }
        let mut d = (**old).clone();
        let mut sql = d.text.to_string();
        for change in p.content_changes {
            if let Some(range) = change.range {
                let start = text::offset(&sql, range.start);
                let end = text::offset(&sql, range.end);
                if start > end {
                    return;
                }
                sql.replace_range(start..end, &change.text);
            } else {
                sql = change.text;
            }
        }
        if sql.len() > 2 * 1024 * 1024 {
            return;
        }
        d.version = p.text_document.version;
        d.generation += 1;
        d.analysis = Arc::new(Analysis::new(&sql));
        d.text = Arc::from(sql);
        d.rendered = None;
        let d = Arc::new(d);
        docs.insert(d.uri.clone(), d.clone());
        drop(docs);
        self.state.schedule(d);
    }
    async fn did_close(&self, p: DidCloseTextDocumentParams) {
        self.state.docs.write().await.remove(&p.text_document.uri);
        self.state
            .client
            .publish_diagnostics(p.text_document.uri, vec![], None)
            .await;
    }
    async fn did_save(&self, _: DidSaveTextDocumentParams) {
        self.state.reconfigure().await;
    }
    async fn did_change_configuration(&self, p: DidChangeConfigurationParams) {
        let v = p.settings.get("duckdbLsp").unwrap_or(&p.settings).clone();
        if let Ok(s) = serde_json::from_value(v) {
            *self.state.settings.write().await = s;
            self.state.reconfigure().await;
        }
    }
    async fn did_change_watched_files(&self, _: DidChangeWatchedFilesParams) {
        self.state.reconfigure().await;
    }
    async fn did_change_workspace_folders(&self, p: DidChangeWorkspaceFoldersParams) {
        let mut roots = self.state.roots.write().await;
        for f in p.event.removed {
            if let Ok(path) = f.uri.to_file_path() {
                roots.retain(|r| r != &path);
            }
        }
        for f in p.event.added {
            if let Ok(path) = f.uri.to_file_path() {
                roots.push(path);
            }
        }
    }
    async fn completion(&self, p: CompletionParams) -> Result<Option<CompletionResponse>> {
        let d = self
            .document(&p.text_document_position.text_document.uri)
            .await?;
        let cat = self.state.catalog(&d).await;
        let byte = text::offset(&d.text, p.text_document_position.position);
        let limit = self
            .state
            .settings
            .read()
            .await
            .max_completions
            .clamp(1, 2000);
        let mut list = if let Some((r, at)) = d
            .rendered
            .as_ref()
            .and_then(|r| r.target(byte).map(|at| (r, at)))
        {
            features::complete(&r.text, at, &r.analysis, &cat, &d.config, limit)
        } else {
            features::complete(&d.text, byte, &d.analysis, &cat, &d.config, limit)
        };
        // Completion replaces only the identifier under the original cursor.
        let (start, end, _) = features::prefix(&d.text, byte);
        for item in &mut list.items {
            if let Some(CompletionTextEdit::Edit(edit)) = &mut item.text_edit {
                edit.range = text::range(&d.text, start, end);
            }
        }
        if self.state.snippets.load(Ordering::Relaxed) {
            list.items
                .extend(features::snippets(&d.text, byte, &d.config));
        }
        if list.items.len() > limit {
            list.is_incomplete = true;
            list.items.truncate(limit);
        }
        Ok(Some(CompletionResponse::List(list)))
    }
    async fn hover(&self, p: HoverParams) -> Result<Option<Hover>> {
        let pos = p.text_document_position_params;
        let d = self.document(&pos.text_document.uri).await?;
        let cat = self.state.catalog(&d).await;
        Ok(features::hover(
            &d.text,
            text::offset(&d.text, pos.position),
            &d.analysis,
            &cat,
        ))
    }
    async fn signature_help(&self, p: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let pos = p.text_document_position_params;
        let d = self.document(&pos.text_document.uri).await?;
        let cat = self.state.catalog(&d).await;
        Ok(features::signature(
            &d.text,
            text::offset(&d.text, pos.position),
            &d.analysis,
            &cat,
        ))
    }
    async fn goto_definition(
        &self,
        p: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pos = p.text_document_position_params;
        let d = self.document(&pos.text_document.uri).await?;
        let byte = text::offset(&d.text, pos.position);
        if let Some(id) = d.analysis.symbol_at(byte) {
            let s = &d.analysis.symbols[id];
            return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri: d.uri.clone(),
                range: text::range(&d.text, s.start, s.end),
            })));
        }
        let cat = self.state.catalog(&d).await;
        for scope in &d.analysis.scopes {
            for s in &scope.sources {
                if s.start <= byte && byte < s.end {
                    if let Some(decl) = d
                        .analysis
                        .declarations
                        .iter()
                        .rev()
                        .find(|decl| decl.end < s.start && decl.parts == s.parts)
                    {
                        return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                            uri: d.uri.clone(),
                            range: text::range(&d.text, decl.start, decl.start),
                        })));
                    }
                    if let Some(t) = cat.resolve(&s.parts) {
                        return Ok(
                            Self::catalog_location(&d, t).map(GotoDefinitionResponse::Scalar)
                        );
                    }
                }
            }
        }
        Ok(None)
    }
    async fn references(&self, p: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let d = self
            .document(&p.text_document_position.text_document.uri)
            .await?;
        let byte = text::offset(&d.text, p.text_document_position.position);
        let Some(id) = d.analysis.symbol_at(byte) else {
            return Ok(None);
        };
        let sym = &d.analysis.symbols[id];
        Ok(Some(
            d.analysis
                .tokens
                .iter()
                .filter(|t| {
                    d.analysis.bindings.get(&t.start) == Some(&id)
                        && (p.context.include_declaration || t.start != sym.start)
                })
                .map(|t| Location {
                    uri: d.uri.clone(),
                    range: text::range(&d.text, t.start, t.end),
                })
                .collect(),
        ))
    }
    async fn prepare_rename(
        &self,
        p: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let d = self.document(&p.text_document.uri).await?;
        let byte = text::offset(&d.text, p.position);
        if d.analysis
            .tokens
            .iter()
            .any(|t| t.kind == text::Kind::Template)
        {
            return Ok(None);
        }
        Ok(d.analysis
            .symbol_at(byte)
            .and_then(|_| d.analysis.token_at(byte))
            .map(|t| PrepareRenameResponse::RangeWithPlaceholder {
                range: text::range(&d.text, t.start, t.end),
                placeholder: t.name(),
            }))
    }
    async fn rename(&self, p: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let d = self
            .document(&p.text_document_position.text_document.uri)
            .await?;
        if d.analysis
            .tokens
            .iter()
            .any(|t| t.kind == text::Kind::Template)
        {
            return Err(Error::invalid_params("Rename is available in raw SQL scopes; template expansion can make references ambiguous"));
        }
        if p.new_name.is_empty() || p.new_name.contains(['\n', '\r', '\0']) {
            return Err(Error::invalid_params("Invalid SQL identifier"));
        }
        let byte = text::offset(&d.text, p.text_document_position.position);
        let Some(id) = d.analysis.symbol_at(byte) else {
            return Ok(None);
        };
        if d.analysis
            .symbols
            .iter()
            .enumerate()
            .any(|(i, x)| i != id && crate::analysis::eq(&x.name, &p.new_name))
        {
            return Err(Error::invalid_params("Name already exists in this scope"));
        }
        let edits = d
            .analysis
            .tokens
            .iter()
            .filter(|t| d.analysis.bindings.get(&t.start) == Some(&id))
            .map(|t| {
                OneOf::Left(TextEdit {
                    range: text::range(&d.text, t.start, t.end),
                    new_text: text::quote(&p.new_name),
                })
            })
            .collect();
        Ok(Some(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: d.uri.clone(),
                    version: Some(d.version),
                },
                edits,
            }])),
            ..Default::default()
        }))
    }
    async fn document_highlight(
        &self,
        p: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let d = self
            .document(&p.text_document_position_params.text_document.uri)
            .await?;
        let byte = text::offset(&d.text, p.text_document_position_params.position);
        let Some(id) = d.analysis.symbol_at(byte) else {
            return Ok(None);
        };
        Ok(Some(
            d.analysis
                .tokens
                .iter()
                .filter(|t| d.analysis.bindings.get(&t.start) == Some(&id))
                .map(|t| DocumentHighlight {
                    range: text::range(&d.text, t.start, t.end),
                    kind: Some(if t.start == d.analysis.symbols[id].start {
                        DocumentHighlightKind::WRITE
                    } else {
                        DocumentHighlightKind::READ
                    }),
                })
                .collect(),
        ))
    }
    async fn document_symbol(
        &self,
        p: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let d = self.document(&p.text_document.uri).await?;
        #[allow(deprecated)]
        let symbols = d
            .analysis
            .symbols
            .iter()
            .map(|s| DocumentSymbol {
                name: s.name.clone(),
                detail: Some(s.kind.into()),
                kind: if s.kind == "cte" {
                    SymbolKind::STRUCT
                } else {
                    SymbolKind::VARIABLE
                },
                tags: None,
                deprecated: None,
                range: text::range(&d.text, s.start, s.end),
                selection_range: text::range(&d.text, s.start, s.end),
                children: None,
            })
            .collect();
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }
    async fn symbol(&self, p: WorkspaceSymbolParams) -> Result<Option<Vec<SymbolInformation>>> {
        let docs = self.state.docs.read().await;
        let mut symbols = vec![];
        for d in docs.values() {
            for s in &d.analysis.symbols {
                if s.name.to_lowercase().contains(&p.query.to_lowercase()) {
                    #[allow(deprecated)]
                    symbols.push(SymbolInformation {
                        name: s.name.clone(),
                        kind: SymbolKind::VARIABLE,
                        tags: None,
                        deprecated: None,
                        location: Location {
                            uri: d.uri.clone(),
                            range: text::range(&d.text, s.start, s.end),
                        },
                        container_name: None,
                    });
                }
            }
        }
        symbols.truncate(500);
        Ok(Some(symbols))
    }
    async fn folding_range(&self, p: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let d = self.document(&p.text_document.uri).await?;
        let mut ranges = vec![];
        let mut stack = vec![];
        for t in text::lex(&d.text) {
            if t.is("(") {
                stack.push(t.start);
            }
            if t.is(")") {
                if let Some(start) = stack.pop() {
                    ranges.push((start, t.end));
                }
            }
            if t.kind == text::Kind::Comment {
                ranges.push((t.start, t.end));
            }
        }
        Ok(Some(
            ranges
                .into_iter()
                .filter_map(|(s, e)| {
                    let s = text::position(&d.text, s);
                    let e = text::position(&d.text, e);
                    (s.line < e.line).then_some(FoldingRange {
                        start_line: s.line,
                        start_character: Some(s.character),
                        end_line: e.line,
                        end_character: Some(e.character),
                        kind: None,
                        collapsed_text: None,
                    })
                })
                .collect(),
        ))
    }
    async fn formatting(&self, p: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let d = self.document(&p.text_document.uri).await?;
        let settings = self.state.settings.read().await.clone();
        if !settings.format {
            return Ok(None);
        }
        let result = self
            .state
            .formatter
            .request(
                &settings.python,
                settings.worker_timeout_ms,
                "format",
                worker_params(&d, &settings, &Catalog::default()),
            )
            .await
            .map_err(|s| Error::invalid_params(s))?;
        if !self.state.current(&d).await {
            return Err(Error::content_modified());
        }
        let Some(new) = result["text"].as_str() else {
            return Ok(None);
        };
        Ok(Some(if new == &*d.text {
            vec![]
        } else {
            vec![TextEdit {
                range: text::range(&d.text, 0, d.text.len()),
                new_text: new.into(),
            }]
        }))
    }
    async fn code_action(&self, p: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        Ok(Some(vec![CodeActionOrCommand::Command(Command {
            title: "Refresh DuckDB catalog".into(),
            command: "duckdb.refreshCatalog".into(),
            arguments: Some(vec![json!({"uri":p.text_document.uri})]),
        })]))
    }
    async fn execute_command(&self, p: ExecuteCommandParams) -> Result<Option<Value>> {
        let uri = p
            .arguments
            .first()
            .and_then(|v| v.get("uri"))
            .and_then(Value::as_str)
            .and_then(|u| Url::parse(u).ok())
            .ok_or_else(|| Error::invalid_params("Expected {uri} command argument"))?;
        let d = self.document(&uri).await?;
        match p.command.as_str() {
            "duckdb.refreshCatalog" => {
                self.state.refresh(d.connection.clone(), true);
                Ok(Some(json!({"scheduled":true})))
            }
            "duckdb.explainConfiguration" => Ok(Some(serde_json::to_value(&d.config).unwrap())),
            _ => Err(Error::method_not_found()),
        }
    }
}
