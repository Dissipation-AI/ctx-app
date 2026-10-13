//! The connection to the `ctx` server. One socket: requests and input go out from the main
//! thread; a reader thread decodes what comes back (screen updates, tab list changes, replies)
//! and wakes the main thread. Replies are matched to requests in order.

use ctx_protocol::config::Paths;
use ctx_protocol::*;
use std::collections::HashMap;
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const RESTART_MAGIC: &[u8] = b"ctx: restart the server";

/// Something from the server for the main thread.
#[derive(Debug)]
pub enum Event {
    Stream(StreamOut),
    /// The reply to a request sent with [`Client::send`], by its id.
    Reply(u64, Response),
    Disconnected(String),
}

enum Waiter {
    Wait(Sender<Response>),
    Id(u64),
    Ignore,
}

pub struct Client {
    writer: UnixStream,
    pending: Arc<Mutex<HashMap<u64, Waiter>>>,
    next_id: u64,
}

/// Where the server's socket lives: the same folder the server picks (see ctx's `sys.rs`).
#[cfg(not(target_os = "macos"))]
pub fn user_temp_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => std::env::temp_dir(),
    }
}

#[cfg(target_os = "macos")]
pub fn user_temp_dir() -> PathBuf {
    let mut buf = vec![0u8; 1024];
    // SAFETY: buf is valid for its length; confstr writes at most that many bytes.
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr().cast(), buf.len()) };
    if n > 0 && n <= buf.len() {
        buf.truncate(n - 1);
        return PathBuf::from(String::from_utf8_lossy(&buf).into_owned());
    }
    std::env::temp_dir()
}

pub fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

pub fn paths() -> Paths {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    Paths::resolve(user_temp_dir(), uid, &home())
}

fn hello() -> Hello {
    Hello {
        protocol: PROTOCOL_VERSION,
        client_version: env!("CARGO_PKG_VERSION").into(),
        kind: ClientKind::App,
        folder: home(),
        in_tab: None,
        declared_name: None,
        interactive: true,
    }
}

/// Why connecting failed, in words for the person.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectError {
    /// No server answers on the socket.
    NoServer(String),
    /// The server speaks another protocol version and is busy, so it can't restart by itself.
    Mismatch(String),
}

impl Client {
    /// Connect and start the reader thread. `wake` is called (from the reader thread) whenever an
    /// event is queued.
    pub fn connect(wake: impl Fn() + Send + 'static) -> Result<(Client, Receiver<Event>), ConnectError> {
        let socket = paths().socket();
        let mut restarted = false;
        loop {
            let mut stream = UnixStream::connect(&socket)
                .map_err(|e| ConnectError::NoServer(format!("No ctx server at {}: {e}", socket.display())))?;
            write_message(&mut stream, &hello()).map_err(|e| ConnectError::NoServer(e.to_string()))?;
            match read_message::<Welcome>(&mut stream).map_err(|e| ConnectError::NoServer(e.to_string()))? {
                Some(Welcome::Accepted { .. }) => {
                    let reader = stream.try_clone().map_err(|e| ConnectError::NoServer(e.to_string()))?;
                    let pending: Arc<Mutex<HashMap<u64, Waiter>>> = Arc::default();
                    let (tx, rx) = mpsc::channel();
                    let p = pending.clone();
                    std::thread::Builder::new()
                        .name("ctx-server-reader".into())
                        .spawn(move || read_loop(reader, p, tx, wake))
                        .map_err(|e| ConnectError::NoServer(e.to_string()))?;
                    return Ok((Client { writer: stream, pending, next_id: 1 }, rx));
                }
                Some(Welcome::VersionMismatch { message, busy_tabs, .. }) => {
                    if busy_tabs.is_empty() && !restarted {
                        let mut frame = (RESTART_MAGIC.len() as u32).to_le_bytes().to_vec();
                        frame.extend_from_slice(RESTART_MAGIC);
                        let _ = stream.write_all(&frame);
                        drop(stream);
                        restarted = true;
                        wait_for_server(Duration::from_secs(5));
                        continue;
                    }
                    return Err(ConnectError::Mismatch(message));
                }
                None => return Err(ConnectError::NoServer("The ctx server closed the connection.".into())),
            }
        }
    }

    fn write(&mut self, msg: &StreamIn) -> io::Result<()> {
        write_message(&mut self.writer, msg)
    }

    /// Send a request under a fresh id, registering how its reply is handled first.
    fn send_with(&mut self, sentence: &str, request: Request, waiter: Waiter) -> Option<u64> {
        if std::env::var_os("CTX_APP_TRACE").is_some() {
            eprintln!("-> {request:?}");
        }
        let id = self.next_id;
        self.next_id += 1;
        self.pending.lock().unwrap().insert(id, waiter);
        let msg = StreamIn::Request { id, envelope: Envelope { sentence: sentence.to_string(), request } };
        if self.write(&msg).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return None;
        }
        Some(id)
    }

    /// Send a request; its reply arrives as [`Event::Reply`] with the returned id.
    pub fn send(&mut self, sentence: &str, request: Request) -> u64 {
        let id = self.next_id;
        self.send_with(sentence, request, Waiter::Id(id)).unwrap_or(id)
    }

    /// Send a request whose reply doesn't matter (input, resize, subscriptions).
    pub fn tell(&mut self, request: Request) {
        self.send_with("", request, Waiter::Ignore);
    }

    /// Send a request and wait for its reply (scrollback lines, searches: well under a
    /// millisecond on a local socket).
    pub fn ask(&mut self, sentence: &str, request: Request) -> Option<Response> {
        let (tx, rx) = mpsc::channel();
        self.send_with(sentence, request, Waiter::Wait(tx))?;
        rx.recv_timeout(Duration::from_secs(5)).ok()
    }

    pub fn input(&mut self, tab: u64, input: AppInput) {
        self.tell(Request::Input { tab, input });
    }
}

fn read_loop(mut stream: UnixStream, pending: Arc<Mutex<HashMap<u64, Waiter>>>, tx: Sender<Event>, wake: impl Fn()) {
    loop {
        match read_message::<StreamOut>(&mut stream) {
            Ok(Some(StreamOut::Reply { id, response })) => {
                let w = pending.lock().unwrap().remove(&id);
                if std::env::var_os("CTX_APP_TRACE").is_some() {
                    eprintln!("<- {id} {}", format!("{response:?}").chars().take(120).collect::<String>());
                }
                match w {
                    Some(Waiter::Wait(s)) => {
                        let _ = s.send(response);
                    }
                    Some(Waiter::Id(id)) => {
                        let _ = tx.send(Event::Reply(id, response));
                        wake();
                    }
                    Some(Waiter::Ignore) | None => {}
                }
            }
            Ok(Some(other)) => {
                if tx.send(Event::Stream(other)).is_err() {
                    return;
                }
                wake();
            }
            Ok(None) => {
                let _ = tx.send(Event::Disconnected("The ctx server closed the connection.".into()));
                wake();
                return;
            }
            Err(e) => {
                let _ = tx.send(Event::Disconnected(format!("Lost the connection to the ctx server: {e}")));
                wake();
                return;
            }
        }
    }
}

/// Wait until a server accepts connections (after a restart, or after launchd starts it).
pub fn wait_for_server(limit: Duration) -> bool {
    let socket = paths().socket();
    let start = Instant::now();
    while start.elapsed() < limit {
        if let Ok(mut s) = UnixStream::connect(&socket)
            && write_message(&mut s, &hello()).is_ok()
            && let Ok(Some(Welcome::Accepted { .. })) = read_message::<Welcome>(&mut s)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}
