// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! The Model Context Protocol over a pair of streams: newline-delimited
//! JSON-RPC 2.0, one message a line, as the stdio transport has it.
//!
//! Both eras of the protocol are served, since clients of both are in
//! use. A *legacy* client (protocol 2025-11-25 and earlier) opens with
//! `initialize`, is answered with the version it asked for when this
//! server speaks it, and sends `notifications/initialized`. A *modern*
//! one (2026-07-28) sends no handshake: every request carries its
//! protocol version in `params._meta`, `server/discover` says what this
//! server is, and a version this server does not speak is refused with
//! `UnsupportedProtocolVersion` naming the ones it does. Nothing here is
//! kept per connection either way - the protocol version of a legacy
//! client is only remembered to answer `initialize` - so the two can mix.
//!
//! Requests run one at a time, in order, on the caller's thread; a reader
//! thread takes lines in meanwhile, so `ping` is answered and
//! `notifications/cancelled` is heard while a long call - `export_wait` -
//! is in progress. A cancelled call sends nothing more, as the protocol
//! requires.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use crate::backend::Backend;
use crate::tools::{Context, Toolbox};

/// The newest protocol, served without a handshake.
pub const MODERN: &str = "2026-07-28";
/// The handshake-era protocols served, newest first.
pub const LEGACY: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// The `_meta` keys a modern request carries.
const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_SERVER: &str = "io.modelcontextprotocol/serverInfo";
/// The longest line read: a tool call's arguments are small.
const MAX_LINE: u64 = 16 * 1024 * 1024;
/// How many cancellations of requests not yet begun are remembered.
const EARLY_CANCELS: usize = 1024;
/// The embedded connection remembers mutation ids for its entire life.
/// Refusing further mutations at the bound is safer than evicting ids
/// and accidentally making an old frame executable again.
const GUARDED_REQUESTS: usize = 4096;

/// JSON-RPC's and MCP's error numbers.
mod code {
    pub const PARSE: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    pub const UNSUPPORTED_VERSION: i64 = -32022;
    pub const NOT_READY: i64 = -32002;
}

/// What the agent is told about this server, once, when it connects.
const INSTRUCTIONS: &str = "Concat is a video editor; these tools edit its projects. A project is a folder: open one (project_open) or make one (project_create), then pass its folder as `path` to every other tool. Read timeline_read for the ids of lanes (tracks), clips and media. Times and durations are seconds on the timeline; positions within a clip (keyframes) are fractions from 0 to 1; sizes and offsets of titles and pictures are fractions of the frame. Every edit is one undoable step (edit_undo); edit_apply with a batch command makes several into one. Edits live in memory until project_save. To render, export_start then export_wait. preview_frame shows the frame at a time. A refused edit says why in the window's words and changes nothing.";

const EMBEDDED_INSTRUCTIONS: &str = "These tools access only the project already open in the native editor. Read project_get or timeline_read to obtain its current sessionId and revision. Retain those values and the exact path when submitting a batch through edit_apply_guarded. Native ownership, recovery consent, session, revision, cancellation and track locks are checked independently of this protocol handshake. A refusal or lost response requires reading and reconciliation before a new user-approved plan; never retry an edit automatically. Legacy edits, private project copies, saving, exporting and external calls are not exposed by this connection. One accepted batch shares the native editor's undo history. Protocol initialization never grants permission to resume an interrupted task.";

/// The server's name and version, as `serverInfo`.
fn server_info() -> Value {
    json!({
        "name": "concat",
        "title": "Concat",
        "version": env!("CARGO_PKG_VERSION"),
    })
}

/// The capabilities this server has: tools, and nothing else.
fn capabilities() -> Value {
    json!({ "tools": { "listChanged": false } })
}

/// Every version this server speaks, newest first.
fn supported() -> Vec<&'static str> {
    std::iter::once(MODERN).chain(LEGACY).collect()
}

/// The one writer of the output stream: a line and a flush, under a lock,
/// so a progress notification from a tool never lands inside a reply.
#[derive(Clone)]
struct Output(Arc<Mutex<Box<dyn Write + Send>>>);

impl Output {
    fn send(&self, message: &Value) {
        if let Ok(mut out) = self.0.lock() {
            // A line, never more: serde_json escapes the newlines a string
            // holds, and the compact form has none of its own.
            let _ = writeln!(out, "{message}");
            let _ = out.flush();
        }
    }
}

/// Which requests were cancelled: the one running, by its flag, and ones
/// not yet begun, by id.
#[derive(Default)]
struct Cancels {
    running: Option<(String, Arc<AtomicBool>)>,
    early: HashSet<String>,
    // Kept until the connection ends. The embedded profile refuses all
    // further guarded writes after overflow instead of forgetting which
    // of those frames were stopped. Read-only reconciliation remains.
    overflowed: bool,
}

/// A request handed from the reader to the worker.
struct Incoming {
    id: Value,
    method: String,
    params: Value,
}

/// Serves MCP on `input` and `output` until `input` ends, with `backend`
/// behind every tool. Returns once the last request has been answered and
/// the backend has finished.
pub fn serve(
    input: impl BufRead + Send + 'static,
    output: impl Write + Send + 'static,
    mut backend: Box<dyn Backend>,
) {
    let output = Output(Arc::new(Mutex::new(Box::new(output))));
    let cancels: Arc<Mutex<Cancels>> = Arc::default();
    let (requests, queue) = mpsc::channel::<Incoming>();
    let reader = {
        let output = output.clone();
        let cancels = Arc::clone(&cancels);
        std::thread::Builder::new()
            .name("concat mcp read".to_owned())
            .spawn(move || read(input, &output, &cancels, &requests))
    };
    if reader.is_err() {
        eprintln!("concat mcp: could not start the reader");
        return;
    }
    let toolbox = Toolbox::new();
    let mut server = Server {
        toolbox,
        output: output.clone(),
        cancels,
        instructions: INSTRUCTIONS,
    };
    for incoming in queue {
        server.handle(incoming, backend.as_mut());
    }
    backend.finish();
}

/// Reads lines until the input ends: answers what needs no backend at
/// once, and queues the rest.
fn read(
    input: impl BufRead,
    output: &Output,
    cancels: &Mutex<Cancels>,
    requests: &mpsc::Sender<Incoming>,
) {
    let mut input = input;
    loop {
        let mut bytes = Vec::new();
        match std::io::Read::take(&mut input, MAX_LINE).read_until(b'\n', &mut bytes) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        // JSON is UTF-8. A line that is not is that line's mistake, not the
        // end of the session: `read_line` would fail the read itself and
        // stop the server here without a word.
        let Ok(line) = String::from_utf8(bytes) else {
            output.send(&failure(
                Value::Null,
                code::PARSE,
                "not JSON: the line is not UTF-8",
                None,
            ));
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let message = match serde_json::from_str::<Value>(&line) {
            Ok(message) => message,
            Err(error) => {
                output.send(&failure(
                    Value::Null,
                    code::PARSE,
                    &format!("not JSON: {error}"),
                    None,
                ));
                continue;
            }
        };
        let Value::Object(mut message) = message else {
            // A batch, or a bare value: neither is a message since
            // 2025-06-18.
            output.send(&failure(
                Value::Null,
                code::INVALID_REQUEST,
                "a message is one JSON-RPC object; batches are not supported",
                None,
            ));
            continue;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = message.remove("id");
        let params = message.remove("params").unwrap_or(Value::Null);
        match (method, id) {
            // A response from the client: this server sends no requests.
            (None, _) => {}
            (Some(method), None) => notification(&method, &params, cancels),
            (Some(_), Some(id)) if !(id.is_string() || id.is_i64() || id.is_u64()) => {
                output.send(&failure(
                    Value::Null,
                    code::INVALID_REQUEST,
                    "a request's id is a string or an integer",
                    None,
                ));
            }
            (Some(method), Some(id)) if method == "ping" => {
                output.send(&success(&id, json!({}), &params));
            }
            (Some(method), Some(id)) => {
                if requests.send(Incoming { id, method, params }).is_err() {
                    return;
                }
            }
        }
    }
}

/// A notification: a cancellation is acted on; the rest need nothing.
fn notification(method: &str, params: &Value, cancels: &Mutex<Cancels>) {
    if method != "notifications/cancelled" {
        return;
    }
    let Some(id) = params.get("requestId") else {
        return;
    };
    let key = id.to_string();
    let Ok(mut cancels) = cancels.lock() else {
        return;
    };
    match &cancels.running {
        Some((running, flag)) if *running == key => flag.store(true, Ordering::Relaxed),
        _ => {
            if cancels.early.len() < EARLY_CANCELS {
                cancels.early.insert(key);
            } else if !cancels.early.contains(&key) {
                cancels.overflowed = true;
            }
        }
    }
}

/// A result, with the modern era's additions when the request was modern.
fn success(id: &Value, mut result: Value, params: &Value) -> Value {
    if modern_version(params).is_some()
        && let Some(object) = result.as_object_mut()
    {
        object.insert("resultType".to_owned(), json!("complete"));
        let meta = object
            .entry("_meta")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(meta) = meta.as_object_mut() {
            meta.insert(META_SERVER.to_owned(), server_info());
        }
    }
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// The caching hints 2026-07-28 requires on these results.
fn cacheable(mut reply: Value) -> Value {
    if let Some(result) = reply.get_mut("result").and_then(Value::as_object_mut)
        && result.get("resultType") == Some(&json!("complete"))
    {
        result.insert("ttlMs".to_owned(), json!(0));
        result.insert("cacheScope".to_owned(), json!("public"));
    }
    reply
}

fn failure(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({ "code": code, "message": message });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

/// The protocol version a request names in its `_meta`, if it names one.
fn modern_version(params: &Value) -> Option<&str> {
    params
        .get("_meta")
        .and_then(|meta| meta.get(META_VERSION))
        .and_then(Value::as_str)
}

struct Server {
    toolbox: Toolbox,
    output: Output,
    cancels: Arc<Mutex<Cancels>>,
    instructions: &'static str,
}

impl Server {
    fn handle(&mut self, incoming: Incoming, backend: &mut dyn Backend) {
        if let Some(reply) = self.reply(incoming, backend) {
            self.output.send(&reply);
        }
    }

    fn reply(&mut self, incoming: Incoming, backend: &mut dyn Backend) -> Option<Value> {
        let Incoming { id, method, params } = incoming;
        let key = id.to_string();
        let flag = Arc::new(AtomicBool::new(false));
        {
            let Ok(mut cancels) = self.cancels.lock() else {
                return Some(failure(
                    id,
                    code::INTERNAL,
                    "cancellation state is unavailable",
                    None,
                ));
            };
            if cancels.early.remove(&key) {
                return None;
            }
            cancels.running = Some((key, Arc::clone(&flag)));
        }
        let reply = self.answer(&id, &method, &params, backend, &flag);
        if let Ok(mut cancels) = self.cancels.lock() {
            cancels.running = None;
        }
        (!flag.load(Ordering::Relaxed)).then_some(reply)
    }

    fn answer(
        &mut self,
        id: &Value,
        method: &str,
        params: &Value,
        backend: &mut dyn Backend,
        cancelled: &AtomicBool,
    ) -> Value {
        // A modern request is checked before anything else: its version,
        // then the metadata every modern request must carry.
        if let Some(version) = modern_version(params) {
            if !supported().contains(&version) {
                return failure(
                    id.clone(),
                    code::UNSUPPORTED_VERSION,
                    "Unsupported protocol version",
                    Some(json!({ "supported": supported(), "requested": version })),
                );
            }
            if version == MODERN
                && params
                    .pointer(&format!("/_meta/{}", META_CAPABILITIES.replace('/', "~1")))
                    .is_none()
            {
                return failure(
                    id.clone(),
                    code::INVALID_PARAMS,
                    &format!("a {MODERN} request carries {META_CAPABILITIES} in _meta"),
                    None,
                );
            }
        }
        match method {
            "ping" => success(id, json!({}), params),
            "initialize" => {
                let asked = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                // The version asked for when it is one of the handshake
                // era's this server speaks; the newest of them otherwise,
                // which the client may then decline.
                let version = LEGACY
                    .iter()
                    .find(|known| **known == asked)
                    .copied()
                    .unwrap_or(LEGACY[0]);
                success(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": capabilities(),
                        "serverInfo": server_info(),
                        "instructions": self.instructions,
                    }),
                    params,
                )
            }
            "server/discover" => {
                let mut result = json!({
                    "supportedVersions": supported(),
                    "capabilities": capabilities(),
                    "instructions": self.instructions,
                });
                let mut meta = Map::new();
                meta.insert(META_SERVER.to_owned(), server_info());
                result["_meta"] = Value::Object(meta);
                let mut reply = success(id, result, params);
                reply["result"]["resultType"] = json!("complete");
                cacheable(reply)
            }
            "tools/list" => cacheable(success(id, json!({ "tools": self.toolbox.list() }), params)),
            "tools/call" => self.call(id, params, backend, cancelled),
            _ => failure(
                id.clone(),
                code::METHOD_NOT_FOUND,
                &format!("Method not found: {method}"),
                None,
            ),
        }
    }

    fn call(
        &mut self,
        id: &Value,
        params: &Value,
        backend: &mut dyn Backend,
        cancelled: &AtomicBool,
    ) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return failure(
                id.clone(),
                code::INVALID_PARAMS,
                "tools/call names a tool",
                None,
            );
        };
        let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
        if !(arguments.is_object() || arguments.is_null()) {
            return failure(
                id.clone(),
                code::INVALID_PARAMS,
                "a tool's arguments are an object",
                None,
            );
        }
        let token = params
            .pointer("/_meta/progressToken")
            .filter(|token| token.is_string() || token.is_i64() || token.is_u64())
            .cloned();
        let output = self.output.clone();
        let report = move |progress: f64, total: f64, message: String| {
            if let Some(token) = &token {
                output.send(&json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": token,
                        "progress": progress,
                        "total": total,
                        "message": message,
                    },
                }));
            }
        };
        let wants_progress = params.pointer("/_meta/progressToken").is_some();
        let context = Context {
            backend,
            cancelled,
            progress: wants_progress.then_some(&report as &dyn Fn(f64, f64, String)),
        };
        // A panic in a tool is that call's failure, not the server's end.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.toolbox.call(name, arguments, context)
        }));
        match outcome {
            Ok(Some(outcome)) => {
                let mut result = json!({ "content": outcome.content, "isError": outcome.is_error });
                if let Some(structured) = outcome.structured {
                    result["structuredContent"] = structured;
                }
                success(id, result, params)
            }
            Ok(None) => failure(
                id.clone(),
                code::INVALID_PARAMS,
                &format!("Unknown tool: {name}"),
                None,
            ),
            Err(_) => failure(
                id.clone(),
                code::INTERNAL,
                &format!("the {name} tool failed inside the server"),
                None,
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    New,
    AwaitingInitialized,
    Ready,
    Closed,
}

/// A real MCP connection for an in-process, controlled script transport.
///
/// The pinned Pi client uses the handshake-era protocol: `initialize`,
/// `notifications/initialized`, then tools. This session preserves that
/// lifecycle and its negotiated version instead of treating tool JSON as
/// a private RPC format. It uses the same definitions and dispatch as the
/// stdio server, but only the reviewed embedded toolbox is callable.
///
/// The supplied backend must independently enforce native ownership and
/// recovery consent. Construction and initialization do not confirm
/// recovery, discover a socket, open a headless project, or start Pi.
///
/// Call `handle` on a serialized worker, never on the native UI thread:
/// its backend can synchronously wait for the UI to apply a guarded edit.
/// Notifications cancel requests not yet started. A synchronous running
/// call cannot receive another frame on this same object; the host must
/// route its stop/revoke signal directly to the native owner and pending
/// UI callbacks. This type does not claim that runtime wiring is present.
pub struct Session {
    server: Server,
    backend: Box<dyn Backend>,
    phase: Phase,
    version: Option<&'static str>,
    guarded_ids: HashSet<String>,
}

impl Session {
    /// Creates the restricted native-current-project connection. No
    /// legacy editing tool is callable even if its name is guessed.
    pub fn embedded(backend: Box<dyn Backend>) -> Self {
        Self {
            server: Server {
                toolbox: Toolbox::embedded(),
                // The four embedded tools emit no progress notifications.
                // Additional asynchronous tools need an explicit outgoing
                // transport before they can join this reviewed profile.
                output: Output(Arc::new(Mutex::new(Box::new(std::io::sink())))),
                cancels: Arc::default(),
                instructions: EMBEDDED_INSTRUCTIONS,
            },
            backend,
            phase: Phase::New,
            version: None,
            guarded_ids: HashSet::new(),
        }
    }

    /// The negotiated handshake protocol, after a successful initialize.
    pub fn protocol_version(&self) -> Option<&'static str> {
        self.version
    }

    /// Processes one JSON-RPC object; notifications and cancelled
    /// requests have no response. The caller owns framing and transport.
    pub fn handle(&mut self, message: Value) -> Option<Value> {
        let Some(object) = message.as_object() else {
            return Some(failure(
                Value::Null,
                code::INVALID_REQUEST,
                "one JSON-RPC object is required",
                None,
            ));
        };
        if object.get("jsonrpc") != Some(&json!("2.0")) {
            return Some(failure(
                Value::Null,
                code::INVALID_REQUEST,
                "jsonrpc must be 2.0",
                None,
            ));
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            // This server sends no requests. Responses have no work to do.
            if object.contains_key("result") || object.contains_key("error") {
                return None;
            }
            return Some(failure(
                Value::Null,
                code::INVALID_REQUEST,
                "a request names a method",
                None,
            ));
        };
        let id = object.get("id");
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        if !(params.is_null() || params.is_object()) {
            return id.map(|id| {
                failure(
                    id.clone(),
                    code::INVALID_PARAMS,
                    "MCP parameters are an object",
                    None,
                )
            });
        }
        let Some(id) = id else {
            if self.phase == Phase::AwaitingInitialized && method == "notifications/initialized" {
                self.phase = Phase::Ready;
            } else if self.phase != Phase::Closed {
                notification(method, &params, &self.server.cancels);
            }
            return None;
        };
        if !(id.is_string() || id.is_i64() || id.is_u64())
            || id.as_str().is_some_and(|id| id.len() > 256)
        {
            return Some(failure(
                Value::Null,
                code::INVALID_REQUEST,
                "a request id is an integer or a string of at most 256 bytes",
                None,
            ));
        }
        if self.phase == Phase::Closed {
            return Some(failure(
                id.clone(),
                code::NOT_READY,
                "the embedded session is closed",
                None,
            ));
        }
        if method == "ping" {
            return Some(success(id, json!({}), &Value::Null));
        }
        if method == "initialize" {
            if self.phase != Phase::New {
                return Some(failure(
                    id.clone(),
                    code::INVALID_REQUEST,
                    "this connection has already initialized",
                    None,
                ));
            }
            let Some(asked) = params.get("protocolVersion").and_then(Value::as_str) else {
                return Some(failure(
                    id.clone(),
                    code::INVALID_PARAMS,
                    "initialize requires protocolVersion",
                    None,
                ));
            };
            if !params.get("capabilities").is_some_and(Value::is_object)
                || !params
                    .pointer("/clientInfo/name")
                    .is_some_and(Value::is_string)
                || !params
                    .pointer("/clientInfo/version")
                    .is_some_and(Value::is_string)
                || modern_version(&params).is_some()
            {
                return Some(failure(
                    id.clone(),
                    code::INVALID_PARAMS,
                    "initialize requires capabilities and clientInfo, without per-request modern protocol metadata",
                    None,
                ));
            }
            let version = LEGACY
                .iter()
                .find(|version| **version == asked)
                .copied()
                .unwrap_or(LEGACY[0]);
            let reply = self.server.reply(
                Incoming {
                    id: id.clone(),
                    method: method.to_owned(),
                    params,
                },
                self.backend.as_mut(),
            );
            if reply
                .as_ref()
                .is_some_and(|reply| reply.get("result").is_some())
            {
                self.version = Some(version);
                self.phase = Phase::AwaitingInitialized;
            }
            return reply;
        }
        if self.phase != Phase::Ready {
            return Some(failure(
                id.clone(),
                code::NOT_READY,
                "complete initialize and notifications/initialized before using tools",
                None,
            ));
        }
        if let Some(requested) = modern_version(&params)
            && Some(requested) != self.version
        {
            return Some(failure(
                id.clone(),
                code::UNSUPPORTED_VERSION,
                "the request must use this connection's negotiated version",
                Some(json!({ "requested": requested, "supported": [self.version] })),
            ));
        }
        if !matches!(method, "tools/list" | "tools/call") {
            return Some(failure(
                id.clone(),
                code::METHOD_NOT_FOUND,
                &format!("Method not found: {method}"),
                None,
            ));
        }
        if method == "tools/call"
            && params.get("name").and_then(Value::as_str) == Some("edit_apply_guarded")
        {
            if self
                .server
                .cancels
                .lock()
                .map(|cancels| cancels.overflowed)
                .unwrap_or(true)
            {
                return Some(failure(
                    id.clone(),
                    code::NOT_READY,
                    "cancellation tracking is unavailable or full; only read-only reconciliation is permitted on this connection",
                    None,
                ));
            }
            let key = id.to_string();
            if self.guarded_ids.contains(&key) {
                return Some(failure(
                    id.clone(),
                    code::INVALID_REQUEST,
                    "this guarded request id has already been received; reconcile the project before a new plan",
                    None,
                ));
            }
            if self.guarded_ids.len() >= GUARDED_REQUESTS {
                return Some(failure(
                    id.clone(),
                    code::NOT_READY,
                    "this connection's guarded-request limit is reached; reconcile and establish a new host session",
                    None,
                ));
            }
            // Reserve before dispatch, including refused and cancelled
            // calls. No exception or lost response makes a frame reusable.
            self.guarded_ids.insert(key);
        }
        self.server.reply(
            Incoming {
                id: id.clone(),
                method: method.to_owned(),
                params,
            },
            self.backend.as_mut(),
        )
    }

    /// Closes once and lets the supplied native backend drain its work.
    /// Like handle, this belongs on the session's worker thread.
    pub fn close(&mut self) {
        if self.phase != Phase::Closed {
            self.phase = Phase::Closed;
            self.backend.finish();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concat_api::{ApiError, Request};
    use std::io::Cursor;

    /// A backend that answers every request with an empty object.
    struct Idle;

    impl Backend for Idle {
        fn call(&mut self, _request: Request) -> Result<Value, ApiError> {
            Ok(json!({}))
        }

        fn describe(&self) -> Value {
            json!({ "mode": "test" })
        }
    }

    /// A writer whose bytes the test can read after `serve` has returned.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Serves `input` to its end and returns every line written.
    fn serve_bytes(input: &[u8]) -> Vec<Value> {
        let written = Shared::default();
        serve(Cursor::new(input.to_vec()), written.clone(), Box::new(Idle));
        let bytes = written.0.lock().expect("lock").clone();
        String::from_utf8(bytes)
            .expect("the server writes UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is JSON"))
            .collect()
    }

    #[test]
    fn a_line_that_is_not_utf8_is_refused_and_the_session_goes_on() {
        let mut input = b"\xff\xfe not text\n".to_vec();
        input.extend_from_slice(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        input.push(b'\n');
        let replies = serve_bytes(&input);
        assert_eq!(replies.len(), 2, "{replies:?}");
        assert_eq!(replies[0]["error"]["code"], code::PARSE);
        assert_eq!(replies[0]["id"], Value::Null);
        assert_eq!(replies[1]["id"], 1);
        assert!(replies[1]["result"].is_object());
    }

    #[derive(Default)]
    struct Recorded {
        requests: Vec<Value>,
        finishes: usize,
    }

    struct Recording(Arc<Mutex<Recorded>>);

    impl Backend for Recording {
        fn call(&mut self, request: Request) -> Result<Value, ApiError> {
            self.0
                .lock()
                .expect("record")
                .requests
                .push(serde_json::to_value(request).expect("request encodes"));
            Ok(
                json!({ "sessionId": 7, "revision": 42, "canUndo": true, "canRedo": false,
                "project": { "activeTimelineId": "t", "timelines": [], "media": [] },
                "settings": { "name": "Current", "width": 1920, "height": 1080, "rateNum": 30, "rateDen": 1 } }),
            )
        }

        fn describe(&self) -> Value {
            json!({ "mode": "recording-protocol-test" })
        }

        fn finish(&mut self) {
            self.0.lock().expect("record").finishes += 1;
        }
    }

    fn embedded() -> (Session, Arc<Mutex<Recorded>>) {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let session = Session::embedded(Box::new(Recording(Arc::clone(&recorded))));
        (session, recorded)
    }

    /// Mirrors packages/mcp/src/client.ts at Pi d36266e0: its 1.5.2
    /// package requests 2025-11-25, checks the offered version, then sends
    /// notifications/initialized. This is a protocol contract test, not
    /// execution of the Pi runtime or a model request.
    fn initialize(session: &mut Session, version: &str) -> Value {
        session
            .handle(json!({
                "jsonrpc": "2.0", "id": "init", "method": "initialize",
                "params": { "protocolVersion": version, "capabilities": {},
                    "clientInfo": { "name": "pi-contract-test", "version": "1.5.2" } },
            }))
            .expect("initialize replies")
    }

    fn ready(session: &mut Session) {
        assert_eq!(
            initialize(session, "2025-11-25")["result"]["protocolVersion"],
            "2025-11-25"
        );
        assert!(
            session
                .handle(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .is_none()
        );
    }

    fn guarded(id: i64) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
            "name": "edit_apply_guarded", "arguments": {
                "path": "/current", "expectedSession": 7, "expectedRevision": 41,
                "command": { "op": "batch", "commands": [
                    { "op": "addTextClip", "trackId": "track-1", "start": 0.0, "duration": 1.0, "style": { "content": "One" } },
                    { "op": "addTextClip", "trackId": "track-1", "start": 1.0, "duration": 1.0, "style": { "content": "Two" } },
                ] },
            },
        } })
    }

    #[test]
    fn pinned_pi_handshake_retains_its_version_and_requires_initialized() {
        let (mut session, recorded) = embedded();
        let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
        assert_eq!(
            session.handle(list.clone()).expect("reply")["error"]["code"],
            code::NOT_READY
        );
        let reply = initialize(&mut session, "2025-11-25");
        assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(
            reply["result"]["capabilities"],
            json!({ "tools": { "listChanged": false } })
        );
        assert!(
            reply["result"].get("resultType").is_none(),
            "legacy result shape"
        );
        assert_eq!(session.protocol_version(), Some("2025-11-25"));
        assert_eq!(
            session.handle(list.clone()).expect("reply")["error"]["code"],
            code::NOT_READY
        );
        session.handle(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        let listed = session.handle(list).expect("reply");
        let mut names: Vec<&str> = listed["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "concat_version",
                "edit_apply_guarded",
                "project_get",
                "timeline_read"
            ]
        );
        assert!(
            recorded.lock().expect("record").requests.is_empty(),
            "handshake never opens or resumes a project"
        );
    }

    #[test]
    fn every_supported_legacy_handshake_is_negotiated_without_relabeling() {
        for version in LEGACY {
            let (mut session, _) = embedded();
            assert_eq!(
                initialize(&mut session, version)["result"]["protocolVersion"],
                version
            );
            assert_eq!(session.protocol_version(), Some(version));
        }
        let (mut session, _) = embedded();
        assert_eq!(
            initialize(&mut session, "2099-01-01")["result"]["protocolVersion"],
            LEGACY[0]
        );
        // The requesting client must accept the offered version before it
        // sends its initialized notification. No tool was run meanwhile.
        assert_eq!(session.phase, Phase::AwaitingInitialized);
    }

    #[test]
    fn embedded_tool_dispatch_preserves_guard_fields_and_refuses_duplicate_ids() {
        let (mut session, recorded) = embedded();
        ready(&mut session);
        let request = guarded(10);
        let reply = session.handle(request.clone()).expect("reply");
        assert_eq!(reply["result"]["isError"], false);
        assert_eq!(reply["result"]["structuredContent"]["sessionId"], 7);
        assert_eq!(reply["result"]["structuredContent"]["revision"], 42);
        let duplicate = session.handle(request).expect("duplicate refused");
        assert_eq!(duplicate["error"]["code"], code::INVALID_REQUEST);
        let recorded = recorded.lock().expect("record");
        assert_eq!(recorded.requests.len(), 1, "no replay or legacy fallback");
        let request = &recorded.requests[0];
        assert_eq!(request["method"], "edit.applyGuarded");
        assert_eq!(request["path"], "/current");
        assert_eq!(request["expectedSession"], 7);
        assert_eq!(request["expectedRevision"], 41);
        assert_eq!(
            request["command"]["commands"].as_array().map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn hidden_legacy_tools_and_unknown_methods_never_reach_the_backend() {
        let (mut session, recorded) = embedded();
        ready(&mut session);
        for (index, name) in [
            "edit_apply",
            "title_add",
            "captions_add",
            "edit_undo",
            "project_open",
            "project_create",
            "project_save",
            "export_start",
        ]
        .iter()
        .enumerate()
        {
            let reply = session.handle(json!({ "jsonrpc": "2.0", "id": index, "method": "tools/call", "params": { "name": name, "arguments": {} } })).expect("refused");
            assert_eq!(
                reply["error"]["code"],
                code::INVALID_PARAMS,
                "{name}: {reply}"
            );
        }
        let direct = session
            .handle(json!({ "jsonrpc": "2.0", "id": 99, "method": "edit.apply", "params": {} }))
            .expect("refused");
        assert_eq!(direct["error"]["code"], code::METHOD_NOT_FOUND);
        assert!(recorded.lock().expect("record").requests.is_empty());
    }

    #[test]
    fn guarded_schema_and_typed_dispatch_refuse_missing_or_wrong_versions() {
        let (mut session, recorded) = embedded();
        ready(&mut session);
        let toolbox = Toolbox::embedded();
        let (_, schema) = toolbox.find("edit_apply_guarded").expect("guarded tool");
        let valid = guarded(1)["params"]["arguments"].clone();
        crate::schema::check(schema, &valid, schema, "guarded").expect("valid shape");
        for (index, wrong) in [
            {
                let mut wrong = valid.clone();
                wrong
                    .as_object_mut()
                    .expect("object")
                    .remove("expectedSession");
                wrong
            },
            {
                let mut wrong = valid.clone();
                wrong["expectedRevision"] = json!(-1);
                wrong
            },
            {
                let mut wrong = valid.clone();
                wrong["expectedSession"] = json!("7");
                wrong
            },
            {
                let mut wrong = valid.clone();
                wrong["expectedRevision"] = json!(0.5);
                wrong
            },
        ]
        .into_iter()
        .enumerate()
        {
            assert!(crate::schema::check(schema, &wrong, schema, "guarded").is_err());
            let reply = session.handle(json!({ "jsonrpc": "2.0", "id": index, "method": "tools/call", "params": { "name": "edit_apply_guarded", "arguments": wrong } })).expect("refused");
            assert_eq!(reply["result"]["isError"], true);
            assert_eq!(
                reply["result"]["structuredContent"]["error"]["code"],
                "invalid"
            );
        }
        assert!(recorded.lock().expect("record").requests.is_empty());
    }

    #[test]
    fn a_cancelled_queued_guarded_frame_cannot_be_replayed() {
        let (mut session, recorded) = embedded();
        ready(&mut session);
        assert!(session.handle(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 10, "reason": "stopped" } })).is_none());
        assert!(session.handle(guarded(10)).is_none());
        assert_eq!(
            session.handle(guarded(10)).expect("no replay")["error"]["code"],
            code::INVALID_REQUEST
        );
        assert!(recorded.lock().expect("record").requests.is_empty());
    }

    #[test]
    fn cancellation_overflow_refuses_every_later_guarded_write() {
        let (mut session, recorded) = embedded();
        ready(&mut session);
        for id in 0..=EARLY_CANCELS {
            assert!(session.handle(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": id } })).is_none());
        }
        // The last cancellation could not fit in the set. It must not
        // disappear and turn that stopped edit back into runnable work.
        for id in [EARLY_CANCELS as i64, 10_000] {
            assert_eq!(
                session.handle(guarded(id)).expect("refused")["error"]["code"],
                code::NOT_READY
            );
        }
        assert!(recorded.lock().expect("record").requests.is_empty());
        let read = session.handle(json!({ "jsonrpc": "2.0", "id": "reconcile", "method": "tools/call", "params": { "name": "project_get", "arguments": { "path": "/current" } } })).expect("read remains possible");
        assert_eq!(read["result"]["isError"], false);
        let recorded = recorded.lock().expect("record");
        assert_eq!(recorded.requests.len(), 1);
        assert_eq!(recorded.requests[0]["method"], "project.get");
    }

    #[test]
    fn a_connection_cannot_switch_to_a_different_protocol_after_initialize() {
        let (mut session, recorded) = embedded();
        ready(&mut session);
        let mut request = guarded(1);
        request["params"]["_meta"] = json!({ "io.modelcontextprotocol/protocolVersion": MODERN, "io.modelcontextprotocol/clientCapabilities": {} });
        assert_eq!(
            session.handle(request).expect("refused")["error"]["code"],
            code::UNSUPPORTED_VERSION
        );
        assert_eq!(
            initialize(&mut session, "2025-06-18")["error"]["code"],
            code::INVALID_REQUEST
        );
        assert!(recorded.lock().expect("record").requests.is_empty());
    }

    #[test]
    fn close_finishes_the_backend_once_and_never_reopens_the_session() {
        let (mut session, recorded) = embedded();
        ready(&mut session);
        session.close();
        session.close();
        assert_eq!(
            session.handle(guarded(1)).expect("closed")["error"]["code"],
            code::NOT_READY
        );
        drop(session);
        let recorded = recorded.lock().expect("record");
        assert_eq!(recorded.finishes, 1);
        assert!(recorded.requests.is_empty());
    }

    #[test]
    fn stdio_still_accepts_the_fixed_pi_handshake_version() {
        let input = [
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": { "name": "pi-contract-test", "version": "1.5.2" },
            } }),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        ]
        .map(|message| format!("{message}\n"))
        .join("");
        let replies = serve_bytes(input.as_bytes());
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-11-25");
        assert!(replies[0]["result"].get("resultType").is_none());
        assert!(
            replies[1]["result"]["tools"]
                .as_array()
                .expect("tools")
                .iter()
                .any(|tool| tool["name"] == "edit_apply_guarded")
        );
    }
}
