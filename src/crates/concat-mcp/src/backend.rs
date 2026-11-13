// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

//! Where a tool's request goes: the window's server, or an API of this
//! process's own.
//!
//! Both take a [`Request`] and give back its result as JSON, so a tool
//! cannot tell them apart and the meaning of every request stays the
//! API's. The window's own open project needs no backend of its own: the
//! window's server runs a request naming it in the window, when the
//! Automation page lets scripts edit it ("live mode", #337).
//!
//! - [`Window`]: a JSON-RPC connection to the server the Automation page
//!   runs. Found through the listing the window writes
//!   (`concat_server::discovery`) or named by `CONCAT_API_ADDRESS` and
//!   `CONCAT_API_TOKEN`. Goes through the window's register of open
//!   projects, so the project on screen cannot be opened here and saved
//!   over, and through its one export slot.
//! - [`Headless`]: an [`Api`] in this process, writing under the user's
//!   home (or `CONCAT_MCP_ROOTS`). Nothing to start first, and nothing
//!   shared with a window: a project open in the window can be opened here
//!   too, which is why the window's server is preferred when it is there.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use concat_api::{Api, ApiError, AppDirs, Request, Response, Stopper};
use concat_server::client::Client;
use concat_server::discovery;
use serde_json::Value;

/// How long a connection to the window's server may take to open.
const PATIENCE: Duration = Duration::from_secs(3);

/// What a tool's request is handed to.
pub trait Backend: Send {
    /// Runs one request; its result as the JSON the API replies with.
    fn call(&mut self, request: Request) -> Result<Value, ApiError>;

    /// What this backend is, for the agent and the log: "window" or
    /// "headless", and where.
    fn describe(&self) -> Value;

    /// The session is over: stop what is running, and wait for it.
    fn finish(&mut self) {}
}

/// Which backend `concat --mcp` uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    /// The window's server when it can be reached, this process otherwise.
    #[default]
    Auto,
    /// The window's server, or an error saying how to turn it on.
    Window,
    /// This process, whatever else is running.
    Headless,
}

impl Mode {
    /// The mode a word names: "auto", "window" or "headless".
    pub fn parse(word: &str) -> Option<Mode> {
        match word.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Some(Mode::Auto),
            "window" => Some(Mode::Window),
            "headless" => Some(Mode::Headless),
            _ => None,
        }
    }
}

/// The backend `mode` asks for, with the reason for whatever was not
/// chosen written to standard error, where an MCP client shows a server's
/// log.
pub fn open(mode: Mode) -> Result<Box<dyn Backend>, String> {
    match mode {
        Mode::Headless => Ok(Box::new(Headless::start()?)),
        Mode::Window => Ok(Box::new(Window::find()?)),
        Mode::Auto => match Window::find() {
            Ok(window) => Ok(Box::new(window)),
            Err(why) => {
                eprintln!("concat mcp: working headless: {why}");
                Ok(Box::new(Headless::start()?))
            }
        },
    }
}

/// Where the window's server is, and its token: the environment's if it
/// names them, the window's listing otherwise.
fn locate() -> Result<(SocketAddr, String), String> {
    let address = std::env::var("CONCAT_API_ADDRESS")
        .ok()
        .filter(|text| !text.is_empty());
    let token = std::env::var("CONCAT_API_TOKEN")
        .ok()
        .filter(|text| !text.is_empty());
    if let (Some(address), Some(token)) = (&address, &token) {
        let address = address.parse().map_err(|_| {
            format!("CONCAT_API_ADDRESS {address:?} is not an address like 127.0.0.1:7420")
        })?;
        return Ok((address, token.clone()));
    }
    if address.is_some() != token.is_some() {
        return Err("CONCAT_API_ADDRESS and CONCAT_API_TOKEN go together".to_owned());
    }
    let dirs = AppDirs::locate()?;
    let path = discovery::path_in(&dirs.config);
    match discovery::read(&path)? {
        Some(listing) => Ok((listing.address, listing.token)),
        None => Err(
            "the Concat window is not serving the API: open Concat, then Settings > Automation, and turn on Scripts and apps"
                .to_owned(),
        ),
    }
}

/// The window's server, over a connection that is reopened once if it
/// drops - the window restarted its server, say. A guarded mutation is
/// never replayed: a lost reply leaves its outcome unknown until the
/// caller reads and reconciles the current native project.
pub struct Window {
    client: Client,
}

impl Window {
    /// Connects to the window's server.
    pub fn find() -> Result<Window, String> {
        let (address, token) = locate()?;
        let client = Client::connect(address, &token, PATIENCE)
            .map_err(|error| format!("{error} (if the window has closed, its listing is stale)"))?;
        Ok(Window { client })
    }

    /// Connects to the server at `address` with `token`.
    pub fn connect(address: SocketAddr, token: &str) -> Result<Window, String> {
        Ok(Window {
            client: Client::connect(address, token, PATIENCE)?,
        })
    }

    fn call_with_reconnect(
        &mut self,
        request: Request,
        reconnect: impl FnOnce() -> Result<Window, String>,
    ) -> Result<Value, ApiError> {
        if matches!(&request, Request::EditApplyGuarded { .. }) {
            return self.client.call(&request);
        }
        match self.client.call(&request) {
            Err(error)
                if error
                    .message
                    .starts_with("the connection to the Concat server") =>
            {
                let again = reconnect().map_err(|why| {
                    ApiError::failed(format!(
                        "{} - and reconnecting failed: {why}",
                        error.message
                    ))
                })?;
                *self = again;
                self.client.call(&request)
            }
            outcome => outcome,
        }
    }
}

impl Backend for Window {
    fn call(&mut self, request: Request) -> Result<Value, ApiError> {
        self.call_with_reconnect(request, Window::find)
    }

    fn describe(&self) -> Value {
        serde_json::json!({ "mode": "window", "address": self.client.address().to_string() })
    }
}

/// An API in this process.
pub struct Headless {
    api: Api,
    stopper: Stopper,
}

impl Headless {
    /// An API over this machine's app directories, writing under the
    /// roots `CONCAT_MCP_ROOTS` names (the platform's path-list syntax),
    /// or the user's home.
    pub fn start() -> Result<Headless, String> {
        Ok(Headless::with_api(Api::new(Arc::new(|_| {}))?))
    }

    /// An API made by the caller - a test's, over a scratch folder.
    pub fn with_api(mut api: Api) -> Headless {
        let roots: Vec<PathBuf> = std::env::var_os("CONCAT_MCP_ROOTS")
            .map(|roots| std::env::split_paths(&roots).collect())
            .filter(|roots: &Vec<PathBuf>| !roots.is_empty())
            .unwrap_or_else(concat_server::home_root);
        api.add_capability("mcp");
        if api.roots().is_empty() {
            api.restrict_writes_to(roots);
        }
        let stopper = api.stopper();
        Headless { api, stopper }
    }
}

impl Backend for Headless {
    fn call(&mut self, request: Request) -> Result<Value, ApiError> {
        match self.api.dispatch(request) {
            Response::Result(reply) => serde_json::to_value(&reply)
                .map_err(|error| ApiError::failed(format!("the reply did not encode: {error}"))),
            Response::Error(error) => Err(error),
        }
    }

    fn describe(&self) -> Value {
        let roots: Vec<String> = self
            .api
            .roots()
            .iter()
            .map(|root| root.display().to_string())
            .collect();
        serde_json::json!({ "mode": "headless", "writesUnder": roots })
    }

    fn finish(&mut self) {
        self.stopper.cancel_all();
        self.api.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{Shutdown, TcpListener};

    /// The socket peer receives an authenticated guarded edit and drops
    /// the connection before replying. Its outcome is unknown; the
    /// adapter must return that first error without even trying discovery
    /// or reconnecting. No environment variables or real server are used.
    #[test]
    fn a_lost_guarded_reply_never_discovers_reconnects_or_replays() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener");
        let address = listener.local_addr().expect("address");
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("test connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("timeout");
            let mut reader = BufReader::new(stream.try_clone().expect("reader"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("authentication frame");
            let auth: Value = serde_json::from_str(&line).expect("auth JSON");
            assert_eq!(auth["method"], "auth");
            writeln!(
                stream,
                "{}",
                json!({ "jsonrpc": "2.0", "id": auth["id"], "result": {} })
            )
            .expect("auth reply");
            stream.flush().expect("flush");
            line.clear();
            reader.read_line(&mut line).expect("edit frame");
            let request: Value = serde_json::from_str(&line).expect("edit JSON");
            stream.shutdown(Shutdown::Both).expect("lost reply");
            request
        });
        let mut window = Window::connect(address, "isolated-test-token").expect("connect");
        let request: Request = serde_json::from_value(json!({
            "method": "edit.applyGuarded", "path": "/current",
            "expectedSession": 4, "expectedRevision": 8,
            "command": { "op": "batch", "commands": [] },
        }))
        .expect("typed request");
        let failure = window
            .call_with_reconnect(request, || {
                panic!("a guarded request must not discover or reconnect")
            })
            .expect_err("the original lost reply");
        assert!(
            failure
                .message
                .starts_with("the connection to the Concat server")
        );
        assert!(!failure.message.contains("reconnecting"));
        let received = peer.join().expect("peer completed");
        assert_eq!(received["method"], "edit.applyGuarded");
        assert_eq!(received["params"]["expectedSession"], 4);
        assert_eq!(received["params"]["expectedRevision"], 8);
    }
}
