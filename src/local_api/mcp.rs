//! MCP (Model Context Protocol) Streamable HTTP JSON-RPC 2.0 handler.
//! Transport-agnostic over [`ApiBackend`] so protocol & permission behavior can
//! be tested independently of GPUI.

use fdl_core::agent::{AgentError, InsertPosition, ReadDocOpts};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

pub const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_SESSIONS: usize = 64;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Debug, Serialize)]
pub struct DocListItem {
    pub doc_id: String,
    pub title: String,
    pub path: String,
    pub format: String,
    pub is_folder: bool,
    pub pinned: bool,
    pub starred: bool,
    pub modified: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SearchHit {
    pub doc_id: String,
    pub title: String,
    pub path: String,
    pub format: String,
    pub snippet: String,
}

#[derive(Clone, Debug)]
pub enum ApiCall {
    GetCurrentContext,
    ListDocs {
        folder_path: Option<String>,
        recursive: bool,
        filter: String,
        limit: usize,
        offset: usize,
    },
    SearchDocs {
        query: String,
        folder_path: Option<String>,
        limit: usize,
    },
    ReadDoc {
        doc_id: String,
        opts: ReadDocOpts,
    },
    ReadBlock {
        doc_id: String,
        block_ref: String,
    },
    CreateDoc {
        title: String,
        markdown: String,
        folder_path: Option<String>,
        format: Option<String>,
    },
    InsertBlocks {
        doc_id: String,
        expected_revision: String,
        position: InsertPosition,
        anchor_block_ref: Option<String>,
        markdown: String,
    },
    ReplaceBlocks {
        doc_id: String,
        expected_revision: String,
        start_block_ref: String,
        end_block_ref: Option<String>,
        markdown: String,
        allow_rich_loss: bool,
    },
    DeleteBlocks {
        doc_id: String,
        expected_revision: String,
        start_block_ref: String,
        end_block_ref: Option<String>,
        allow_rich_loss: bool,
    },
    EditBlockText {
        doc_id: String,
        expected_revision: String,
        block_ref: String,
        old_text: String,
        new_text: String,
        occurrence: Option<usize>,
    },
    UpdateDocTitle {
        doc_id: String,
        expected_revision: String,
        title: String,
    },
}

impl ApiCall {
    pub fn is_write(&self) -> bool {
        matches!(
            self,
            ApiCall::CreateDoc { .. }
                | ApiCall::InsertBlocks { .. }
                | ApiCall::ReplaceBlocks { .. }
                | ApiCall::DeleteBlocks { .. }
                | ApiCall::EditBlockText { .. }
                | ApiCall::UpdateDocTitle { .. }
        )
    }
}

pub trait ApiBackend: Send + Sync + 'static {
    fn execute(&self, call: ApiCall) -> BoxFuture<'_, Result<Value, AgentError>>;
    fn is_read_only(&self) -> bool;
}

pub struct McpServer {
    backend: Arc<dyn ApiBackend>,
    sessions: Mutex<VecDeque<String>>,
}

#[derive(Debug)]
pub struct McpHttpReply {
    pub status: u16,
    pub session_id: Option<String>,
    pub body: Option<Value>,
}

#[derive(Deserialize)]
struct RpcReq {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

impl McpServer {
    pub fn new(backend: Arc<dyn ApiBackend>) -> Self {
        Self { backend, sessions: Mutex::new(VecDeque::new()) }
    }

    pub fn close_session(&self, id: &str) {
        if let Ok(mut guard) = self.sessions.lock() {
            guard.retain(|s| s != id);
        }
    }

    fn has_session(&self, id: &str) -> bool {
        self.sessions.lock().is_ok_and(|g| g.iter().any(|s| s == id))
    }

    fn new_session(&self) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        if let Ok(mut guard) = self.sessions.lock() {
            if guard.len() >= MAX_SESSIONS {
                guard.pop_front();
            }
            guard.push_back(id.clone());
        }
        id
    }

    pub async fn handle_post(&self, session_hdr: Option<&str>, raw_body: &[u8]) -> McpHttpReply {
        let req: RpcReq = match serde_json::from_slice(raw_body) {
            Ok(r) => r,
            Err(e) => {
                return McpHttpReply {
                    status: 400,
                    session_id: None,
                    body: Some(rpc_err(Value::Null, -32700, format!("Parse error: {e}"), None)),
                };
            }
        };

        // Per MCP Streamable HTTP: unknown/expired Mcp-Session-Id must return HTTP 404
        // on non-initialize requests so clients know to reinitialize.
        if req.method != "initialize"
            && let Some(sid) = session_hdr
            && !self.has_session(sid)
        {
            return McpHttpReply {
                status: 404,
                session_id: None,
                body: Some(rpc_err(req.id.unwrap_or(Value::Null), -32001, "Session not found or expired; re-initialize".into(), None)),
            };
        }

        // Notifications have no id and expect 202 Accepted with no body.
        if req.id.is_none() {
            return McpHttpReply { status: 202, session_id: None, body: None };
        }
        let id = req.id.unwrap_or(Value::Null);

        match req.method.as_str() {
            "initialize" => {
                let requested = req.params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-03-26");
                let version = if SUPPORTED_VERSIONS.contains(&requested) { requested } else { SUPPORTED_VERSIONS[0] };
                let sid = self.new_session();
                McpHttpReply {
                    status: 200,
                    session_id: Some(sid),
                    body: Some(rpc_ok(
                        id,
                        json!({
                            "protocolVersion": version,
                            "capabilities": {
                                "tools": {},
                                "resources": {}
                            },
                            "serverInfo": {
                                "name": "quiet-md",
                                "version": env!("CARGO_PKG_VERSION")
                            },
                            "instructions": "Quiet.md local notes MCP server. Call read_doc before editing to get revision and [path@hash] block_ref handles. Prefer edit_block_text for in-place text edits to preserve inline formatting and comments; use insert_blocks/replace_blocks/delete_blocks for structural changes."
                        }),
                    )),
                }
            }
            "ping" => McpHttpReply {
                status: 200,
                session_id: session_hdr.map(str::to_string),
                body: Some(rpc_ok(id, json!({}))),
            },
            "tools/list" => McpHttpReply {
                status: 200,
                session_id: session_hdr.map(str::to_string),
                body: Some(rpc_ok(id, json!({ "tools": tool_schemas() }))),
            },
            "resources/list" => McpHttpReply {
                status: 200,
                session_id: session_hdr.map(str::to_string),
                body: Some(rpc_ok(
                    id,
                    json!({
                        "resources": [
                            {
                                "uri": "quiet://current",
                                "name": "Current Document",
                                "description": "The document currently open in the active Quiet.md window (with selection context).",
                                "mimeType": "text/markdown"
                            },
                            {
                                "uri": "quiet://library/tree",
                                "name": "Library Tree",
                                "description": "Folders and documents in the active Quiet.md library.",
                                "mimeType": "application/json"
                            }
                        ]
                    }),
                )),
            },
            "resources/templates/list" => McpHttpReply {
                status: 200,
                session_id: session_hdr.map(str::to_string),
                body: Some(rpc_ok(
                    id,
                    json!({
                        "resourceTemplates": [
                            {
                                "uriTemplate": "quiet://doc/{doc_id}",
                                "name": "Document by ID",
                                "description": "Read a Quiet.md document projected as anchored Markdown.",
                                "mimeType": "text/markdown"
                            }
                        ]
                    }),
                )),
            },
            "resources/read" => {
                let uri = req.params.get("uri").and_then(Value::as_str).unwrap_or("");
                let resp = self.handle_resource_read(uri).await;
                let body = match resp {
                    Ok(v) => rpc_ok(id, v),
                    Err(e) => rpc_err(id, -32002, e.message, Some(json!({ "code": e.code, "details": e.details }))),
                };
                McpHttpReply { status: 200, session_id: session_hdr.map(str::to_string), body: Some(body) }
            }
            "tools/call" => {
                let name = req.params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = req.params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let body = match self.handle_tool_call(name, args).await {
                    Ok(v) => rpc_ok(
                        id,
                        json!({
                            "content": [{
                                "type": "text",
                                "text": serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
                            }],
                            "structuredContent": v,
                            "isError": false
                        }),
                    ),
                    Err(e) => rpc_ok(
                        id,
                        json!({
                            "content": [{
                                "type": "text",
                                "text": serde_json::to_string_pretty(&e).unwrap_or_else(|_| e.to_string())
                            }],
                            "isError": true
                        }),
                    ),
                };
                McpHttpReply { status: 200, session_id: session_hdr.map(str::to_string), body: Some(body) }
            }
            other => McpHttpReply {
                status: 200,
                session_id: session_hdr.map(str::to_string),
                body: Some(rpc_err(id, -32601, format!("Method not found: {other}"), None)),
            },
        }
    }

    async fn handle_resource_read(&self, uri: &str) -> Result<Value, AgentError> {
        if uri == "quiet://current" {
            let ctx = self.backend.execute(ApiCall::GetCurrentContext).await?;
            let Some(doc_id) = ctx.get("doc_id").and_then(Value::as_str) else {
                return Err(AgentError::new("NO_ACTIVE_DOC", "no document is currently open in Quiet.md"));
            };
            let doc = self.backend.execute(ApiCall::ReadDoc {
                doc_id: doc_id.to_string(),
                opts: ReadDocOpts::default(),
            }).await?;
            let md = format_doc_resource(&doc, ctx.get("selection"));
            return Ok(json!({
                "contents": [{ "uri": uri, "mimeType": "text/markdown", "text": md }]
            }));
        }
        if uri == "quiet://library/tree" {
            let list = self.backend.execute(ApiCall::ListDocs {
                folder_path: None,
                recursive: true,
                filter: "all".into(),
                limit: 500,
                offset: 0,
            }).await?;
            return Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": serde_json::to_string_pretty(&list).unwrap_or_default()
                }]
            }));
        }
        if let Some(doc_id) = uri.strip_prefix("quiet://doc/") {
            if doc_id.is_empty() {
                return Err(AgentError::new("INVALID_URI", "missing doc_id in quiet://doc/{doc_id}"));
            }
            let doc = self.backend.execute(ApiCall::ReadDoc {
                doc_id: doc_id.to_string(),
                opts: ReadDocOpts::default(),
            }).await?;
            let md = format_doc_resource(&doc, None);
            return Ok(json!({
                "contents": [{ "uri": uri, "mimeType": "text/markdown", "text": md }]
            }));
        }
        Err(AgentError::new("UNKNOWN_RESOURCE", format!("unknown resource URI: {uri}")))
    }

    async fn handle_tool_call(&self, name: &str, args: Value) -> Result<Value, AgentError> {
        let call = parse_tool_call(name, &args)?;
        if call.is_write() && self.backend.is_read_only() {
            return Err(AgentError::new(
                "PERMISSION_DENIED",
                "Quiet.md local MCP server is currently in Read-Only mode; switch to Read & Write in Settings -> AI to enable editing",
            ));
        }
        self.backend.execute(call).await
    }
}

fn format_doc_resource(doc: &Value, selection: Option<&Value>) -> String {
    let title = doc.get("title").and_then(Value::as_str).unwrap_or("");
    let doc_id = doc.get("doc_id").and_then(Value::as_str).unwrap_or("");
    let revision = doc.get("revision").and_then(Value::as_str).unwrap_or("");
    let format = doc.get("format").and_then(Value::as_str).unwrap_or("docxml");
    let content = doc.get("content").and_then(Value::as_str).unwrap_or("");
    let sel_line = selection
        .filter(|v| !v.is_null())
        .map(|s| format!("selection: {}\n", s))
        .unwrap_or_default();
    format!("---\ndoc_id: {doc_id:?}\ntitle: {title:?}\nformat: {format:?}\nrevision: {revision:?}\n{sel_line}---\n\n{content}\n")
}

fn req_str(args: &Value, key: &str) -> Result<String, AgentError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| AgentError::new("INVALID_PARAMS", format!("missing required string parameter '{key}'")))
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn parse_tool_call(name: &str, args: &Value) -> Result<ApiCall, AgentError> {
    match name {
        "get_current_context" => Ok(ApiCall::GetCurrentContext),
        "list_docs" => Ok(ApiCall::ListDocs {
            folder_path: opt_str(args, "folder_path"),
            recursive: args.get("recursive").and_then(Value::as_bool).unwrap_or(true),
            filter: opt_str(args, "filter").unwrap_or_else(|| "all".into()),
            limit: args.get("limit").and_then(Value::as_u64).unwrap_or(100).clamp(1, 500) as usize,
            offset: args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize,
        }),
        "search_docs" => Ok(ApiCall::SearchDocs {
            query: req_str(args, "query")?,
            folder_path: opt_str(args, "folder_path"),
            limit: args.get("limit").and_then(Value::as_u64).unwrap_or(20).clamp(1, 100) as usize,
        }),
        "read_doc" => Ok(ApiCall::ReadDoc {
            doc_id: req_str(args, "doc_id")?,
            opts: ReadDocOpts {
                start_block: args.get("start_block").and_then(Value::as_u64).unwrap_or(0) as usize,
                limit: args.get("limit").and_then(Value::as_u64).unwrap_or(150).clamp(1, 500) as usize,
                include_comments: args.get("include_comments").and_then(Value::as_bool).unwrap_or(true),
            },
        }),
        "read_block" => Ok(ApiCall::ReadBlock {
            doc_id: req_str(args, "doc_id")?,
            block_ref: req_str(args, "block_ref")?,
        }),
        "create_doc" => Ok(ApiCall::CreateDoc {
            title: req_str(args, "title")?,
            markdown: opt_str(args, "markdown").unwrap_or_default(),
            folder_path: opt_str(args, "folder_path"),
            format: opt_str(args, "format"),
        }),
        "insert_blocks" => {
            let pos_str = opt_str(args, "position").unwrap_or_else(|| "append".into());
            let position = match pos_str.as_str() {
                "before" => InsertPosition::Before,
                "after" => InsertPosition::After,
                "append" => InsertPosition::Append,
                "prepend" => InsertPosition::Prepend,
                _ => return Err(AgentError::new("INVALID_PARAMS", format!("invalid position {pos_str:?}; expected before|after|append|prepend"))),
            };
            Ok(ApiCall::InsertBlocks {
                doc_id: req_str(args, "doc_id")?,
                expected_revision: req_str(args, "expected_revision")?,
                position,
                anchor_block_ref: opt_str(args, "anchor_block_ref"),
                markdown: req_str(args, "markdown")?,
            })
        }
        "replace_blocks" => Ok(ApiCall::ReplaceBlocks {
            doc_id: req_str(args, "doc_id")?,
            expected_revision: req_str(args, "expected_revision")?,
            start_block_ref: req_str(args, "start_block_ref")?,
            end_block_ref: opt_str(args, "end_block_ref"),
            markdown: req_str(args, "markdown")?,
            allow_rich_loss: args.get("allow_rich_loss").and_then(Value::as_bool).unwrap_or(false),
        }),
        "delete_blocks" => Ok(ApiCall::DeleteBlocks {
            doc_id: req_str(args, "doc_id")?,
            expected_revision: req_str(args, "expected_revision")?,
            start_block_ref: req_str(args, "start_block_ref")?,
            end_block_ref: opt_str(args, "end_block_ref"),
            allow_rich_loss: args.get("allow_rich_loss").and_then(Value::as_bool).unwrap_or(false),
        }),
        "edit_block_text" => Ok(ApiCall::EditBlockText {
            doc_id: req_str(args, "doc_id")?,
            expected_revision: req_str(args, "expected_revision")?,
            block_ref: req_str(args, "block_ref")?,
            old_text: req_str(args, "old_text")?,
            new_text: req_str(args, "new_text")?,
            occurrence: args.get("occurrence").and_then(Value::as_u64).map(|n| n as usize),
        }),
        "update_doc_title" => Ok(ApiCall::UpdateDocTitle {
            doc_id: req_str(args, "doc_id")?,
            expected_revision: req_str(args, "expected_revision")?,
            title: req_str(args, "title")?,
        }),
        other => Err(AgentError::new("UNKNOWN_TOOL", format!("unknown tool: {other}"))),
    }
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_err(id: Value, code: i32, message: String, data: Option<Value>) -> Value {
    let mut err = serde_json::Map::new();
    err.insert("code".into(), json!(code));
    err.insert("message".into(), json!(message));
    if let Some(d) = data {
        err.insert("data".into(), d);
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": Value::Object(err) })
}

fn tool_schemas() -> Value {
    json!([
        {
            "name": "get_current_context",
            "description": "Get the document currently open in the active Quiet.md window, including doc_id, title, path, format, revision, and current text selection.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "list_docs",
            "description": "List documents and folders in the Quiet.md library, optionally filtered by folder path or starred/pinned status.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "folder_path": { "type": "string", "description": "Relative folder path (e.g. 'Work' or 'Work/Specs'). Omit for root." },
                    "recursive": { "type": "boolean", "description": "Include nested folders/documents (default true)." },
                    "filter": { "type": "string", "enum": ["all", "starred", "pinned"], "description": "Optional status filter." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Max entries to return (default 100)." },
                    "offset": { "type": "integer", "minimum": 0, "description": "Pagination offset (default 0)." }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "search_docs",
            "description": "Full-text search across document titles and content in the Quiet.md library.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query string." },
                    "folder_path": { "type": "string", "description": "Optional folder path prefix to restrict search." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Max hits to return (default 20)." }
                },
                "required": ["query"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "read_doc",
            "description": "Read a Quiet.md document as anchored Markdown with [path@hash] block_ref handles and a revision token for safe editing. Supports pagination for long documents.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "doc_id": { "type": "string", "description": "Document ID from get_current_context, list_docs, or search_docs." },
                    "start_block": { "type": "integer", "minimum": 0, "description": "0-based top-level block offset (default 0)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Max top-level blocks to return (default 150)." },
                    "include_comments": { "type": "boolean", "description": "Include comment thread summaries (default true)." }
                },
                "required": ["doc_id"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "read_block",
            "description": "Inspect detailed structured content of a single block (such as full table cells, code block source, image metadata, or whiteboard text).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "doc_id": { "type": "string" },
                    "block_ref": { "type": "string", "description": "Block handle in '<path>@<hash6>' format from read_doc." }
                },
                "required": ["doc_id", "block_ref"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "create_doc",
            "description": "Create a new document in the Quiet.md library with optional initial Markdown content.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Document title." },
                    "markdown": { "type": "string", "description": "Optional initial body in Markdown." },
                    "folder_path": { "type": "string", "description": "Optional relative folder path (e.g. 'Work')." },
                    "format": { "type": "string", "enum": ["docxml", "markdown"], "description": "Storage format (default follows user preference or docxml)." }
                },
                "required": ["title"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false }
        },
        {
            "name": "insert_blocks",
            "description": "Insert Markdown content as structured blocks before/after an anchor block, or append/prepend to the document or a container block.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "doc_id": { "type": "string" },
                    "expected_revision": { "type": "string", "description": "Revision token from the latest read_doc or write response." },
                    "position": { "type": "string", "enum": ["before", "after", "append", "prepend"], "description": "Where to insert (default 'append')." },
                    "anchor_block_ref": { "type": "string", "description": "Required for 'before'/'after'. Optional container/nesting block_ref for 'append'/'prepend'." },
                    "markdown": { "type": "string", "description": "Markdown content to convert and insert." }
                },
                "required": ["doc_id", "expected_revision", "markdown"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false }
        },
        {
            "name": "replace_blocks",
            "description": "Replace a block or contiguous range of sibling blocks with Markdown content. Refuses to overwrite rich blocks (tables, whiteboards, images, grids) or open comments unless allow_rich_loss=true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "doc_id": { "type": "string" },
                    "expected_revision": { "type": "string" },
                    "start_block_ref": { "type": "string" },
                    "end_block_ref": { "type": "string", "description": "Optional inclusive end block_ref (same parent as start_block_ref)." },
                    "markdown": { "type": "string" },
                    "allow_rich_loss": { "type": "boolean", "description": "Set true to confirm replacing protected rich blocks or open comments." }
                },
                "required": ["doc_id", "expected_revision", "start_block_ref", "markdown"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false }
        },
        {
            "name": "delete_blocks",
            "description": "Delete a block or contiguous range of sibling blocks. Refuses to delete rich blocks or open comments unless allow_rich_loss=true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "doc_id": { "type": "string" },
                    "expected_revision": { "type": "string" },
                    "start_block_ref": { "type": "string" },
                    "end_block_ref": { "type": "string" },
                    "allow_rich_loss": { "type": "boolean" }
                },
                "required": ["doc_id", "expected_revision", "start_block_ref"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false }
        },
        {
            "name": "edit_block_text",
            "description": "Replace an exact substring inside a single text block while preserving surrounding inline marks (bold, links, colors, equations) and comment anchors.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "doc_id": { "type": "string" },
                    "expected_revision": { "type": "string" },
                    "block_ref": { "type": "string" },
                    "old_text": { "type": "string", "description": "Exact existing plain text substring to replace." },
                    "new_text": { "type": "string", "description": "Replacement plain text." },
                    "occurrence": { "type": "integer", "minimum": 1, "description": "1-based match index when old_text appears more than once." }
                },
                "required": ["doc_id", "expected_revision", "block_ref", "old_text", "new_text"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false }
        },
        {
            "name": "update_doc_title",
            "description": "Rename a document's title (and update its file name on disk accordingly).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "doc_id": { "type": "string" },
                    "expected_revision": { "type": "string" },
                    "title": { "type": "string" }
                },
                "required": ["doc_id", "expected_revision", "title"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdl_core::agent::{
        apply_delete_blocks, apply_edit_block_text, apply_insert_blocks, apply_replace_blocks,
        check_revision, document_revision, inspect_block, parse_agent_markdown, project_doc,
    };
    use fdl_core::editor::core::Core;
    use fdl_core::model::{Block, Document, RichText};
    use fdl_core::store::Store;

    struct StoreBackend {
        store: Mutex<Store>,
    }

    impl ApiBackend for StoreBackend {
        fn is_read_only(&self) -> bool { false }
        fn execute(&self, call: ApiCall) -> BoxFuture<'_, Result<Value, AgentError>> {
            Box::pin(async move {
                let mut s = self.store.lock().unwrap();
                match call {
                    ApiCall::CreateDoc { title, markdown, .. } => {
                        let blocks = if markdown.trim().is_empty() { vec![Block::text("")] } else { parse_agent_markdown(&markdown)? };
                        let doc = Document { title: RichText::plain(title), icon: None, cover: None, blocks, comments: Vec::new() };
                        let id = s.create_from(&doc).map_err(|e| AgentError::new("CREATE_FAILED", e.to_string()))?;
                        let saved = s.load(&id).unwrap();
                        let rev = document_revision(&saved, false, "");
                        Ok(json!({ "doc_id": id, "revision": rev }))
                    }
                    ApiCall::ReadDoc { doc_id, opts } => {
                        let doc = s.load(&doc_id).map_err(|e| AgentError::new("LOAD_FAILED", e.to_string()))?;
                        let proj = project_doc(&doc, false, "", opts);
                        Ok(json!({ "doc_id": doc_id, "title": proj.title, "revision": proj.revision, "content": proj.content }))
                    }
                    ApiCall::ReadBlock { doc_id, block_ref } => {
                        let doc = s.load(&doc_id).map_err(|e| AgentError::new("LOAD_FAILED", e.to_string()))?;
                        inspect_block(&doc, &block_ref)
                    }
                    ApiCall::EditBlockText { doc_id, expected_revision, block_ref, old_text, new_text, occurrence } => {
                        let doc = s.load(&doc_id).map_err(|e| AgentError::new("LOAD_FAILED", e.to_string()))?;
                        check_revision(&doc, false, "", &expected_revision)?;
                        let mut core = Core::new(doc);
                        let updated = apply_edit_block_text(&mut core, &block_ref, &old_text, &new_text, occurrence)?;
                        let _ = s.save_version_now(&doc_id);
                        s.save(&doc_id, &core.doc).map_err(|e| AgentError::new("SAVE_FAILED", e.to_string()))?;
                        Ok(json!({ "doc_id": doc_id, "revision": document_revision(&core.doc, false, ""), "updated_block_ref": updated }))
                    }
                    ApiCall::InsertBlocks { doc_id, expected_revision, position, anchor_block_ref, markdown } => {
                        let doc = s.load(&doc_id).map_err(|e| AgentError::new("LOAD_FAILED", e.to_string()))?;
                        check_revision(&doc, false, "", &expected_revision)?;
                        let mut core = Core::new(doc);
                        let refs = apply_insert_blocks(&mut core, position, anchor_block_ref.as_deref(), &markdown)?;
                        let _ = s.save_version_now(&doc_id);
                        s.save(&doc_id, &core.doc).map_err(|e| AgentError::new("SAVE_FAILED", e.to_string()))?;
                        Ok(json!({ "doc_id": doc_id, "revision": document_revision(&core.doc, false, ""), "inserted_refs": refs }))
                    }
                    ApiCall::ReplaceBlocks { doc_id, expected_revision, start_block_ref, end_block_ref, markdown, allow_rich_loss } => {
                        let doc = s.load(&doc_id).map_err(|e| AgentError::new("LOAD_FAILED", e.to_string()))?;
                        check_revision(&doc, false, "", &expected_revision)?;
                        let mut core = Core::new(doc);
                        let refs = apply_replace_blocks(&mut core, &start_block_ref, end_block_ref.as_deref(), &markdown, allow_rich_loss)?;
                        let _ = s.save_version_now(&doc_id);
                        s.save(&doc_id, &core.doc).map_err(|e| AgentError::new("SAVE_FAILED", e.to_string()))?;
                        Ok(json!({ "doc_id": doc_id, "revision": document_revision(&core.doc, false, ""), "replaced_refs": refs }))
                    }
                    ApiCall::DeleteBlocks { doc_id, expected_revision, start_block_ref, end_block_ref, allow_rich_loss } => {
                        let doc = s.load(&doc_id).map_err(|e| AgentError::new("LOAD_FAILED", e.to_string()))?;
                        check_revision(&doc, false, "", &expected_revision)?;
                        let mut core = Core::new(doc);
                        apply_delete_blocks(&mut core, &start_block_ref, end_block_ref.as_deref(), allow_rich_loss)?;
                        let _ = s.save_version_now(&doc_id);
                        s.save(&doc_id, &core.doc).map_err(|e| AgentError::new("SAVE_FAILED", e.to_string()))?;
                        Ok(json!({ "doc_id": doc_id, "revision": document_revision(&core.doc, false, "") }))
                    }
                    _ => Ok(json!({})),
                }
            })
        }
    }

    #[test]
    fn mcp_end_to_end_lifecycle_and_version_snapshots() {
        let dir = std::env::temp_dir().join(format!("quiet-mcp-e2e-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open_at(dir.clone()).unwrap();
        let backend = Arc::new(StoreBackend { store: Mutex::new(store) });
        let server = McpServer::new(backend.clone());

        let rt = quiet_sync_client::tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            // tools/list returns all 11 tools.
            let list = server.handle_post(None, br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).await;
            let tools = list.body.unwrap()["result"]["tools"].as_array().unwrap().len();
            assert_eq!(tools, 11);

            // create_doc
            let create_req = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"create_doc","arguments":{"title":"架构笔记","markdown":"# 概述\n\n原始正文内容"}}}).to_string();
            let create = server.handle_post(None, create_req.as_bytes()).await;
            let sc = create.body.unwrap()["result"]["structuredContent"].clone();
            let doc_id = sc["doc_id"].as_str().unwrap().to_string();
            let rev1 = sc["revision"].as_str().unwrap().to_string();

            // read_doc via MCP resource template
            let res_req = json!({"jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri": format!("quiet://doc/{doc_id}")}}).to_string();
            let res_reply = server.handle_post(None, res_req.as_bytes()).await;
            let res_text = res_reply.body.unwrap()["result"]["contents"][0]["text"].as_str().unwrap().to_string();
            assert!(res_text.contains("原始正文内容"));

            // read_doc via tool to extract block_ref of block 1
            let read_req = json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_doc","arguments":{"doc_id": doc_id}}}).to_string();
            let read_reply = server.handle_post(None, read_req.as_bytes()).await;
            let content = read_reply.body.unwrap()["result"]["structuredContent"]["content"].as_str().unwrap().to_string();
            let b1_line = content.lines().nth(1).unwrap();
            let b1_ref = b1_line.strip_prefix('[').unwrap().split(']').next().unwrap().to_string();

            // edit_block_text with stale revision fails with REVISION_CONFLICT
            let stale_req = json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"edit_block_text","arguments":{"doc_id": doc_id, "expected_revision": "0000000000000000", "block_ref": b1_ref, "old_text": "原始", "new_text": "更新"}}}).to_string();
            let stale_reply = server.handle_post(None, stale_req.as_bytes()).await;
            assert_eq!(stale_reply.body.as_ref().unwrap()["result"]["isError"], json!(true));
            assert!(stale_reply.body.unwrap()["result"]["content"][0]["text"].as_str().unwrap().contains("REVISION_CONFLICT"));

            // edit_block_text with valid revision succeeds & creates a version snapshot
            let edit_req = json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"edit_block_text","arguments":{"doc_id": doc_id, "expected_revision": rev1, "block_ref": b1_ref, "old_text": "原始", "new_text": "更新后"}}}).to_string();
            let edit_reply = server.handle_post(None, edit_req.as_bytes()).await;
            assert_eq!(edit_reply.body.as_ref().unwrap()["result"]["isError"], json!(false));
            let rev2 = edit_reply.body.unwrap()["result"]["structuredContent"]["revision"].as_str().unwrap().to_string();
            assert_ne!(rev1, rev2);

            // Version history captured the pre-edit snapshot
            let versions = backend.store.lock().unwrap().versions(&doc_id);
            assert!(!versions.is_empty(), "pre-edit snapshot must appear in version history");

            // insert_blocks appends new section
            let ins_req = json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"insert_blocks","arguments":{"doc_id": doc_id, "expected_revision": rev2, "position": "append", "markdown": "## 后续计划\n\n- 完成测试"}}}).to_string();
            let ins_reply = server.handle_post(None, ins_req.as_bytes()).await;
            assert_eq!(ins_reply.body.as_ref().unwrap()["result"]["isError"], json!(false));
            let saved = backend.store.lock().unwrap().load(&doc_id).unwrap();
            assert!(saved.plain_text().contains("更新后正文内容"));
            assert!(saved.plain_text().contains("后续计划"));
        });

        let _ = std::fs::remove_dir_all(&dir);
    }
}
