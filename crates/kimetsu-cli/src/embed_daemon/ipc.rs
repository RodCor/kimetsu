//! `interprocess` local-socket transport: name building, single-instance
//! listener, and client connect. Encapsulates the crate's API so the rest of
//! the daemon is transport-agnostic.
//!
//! On Windows this uses named pipes; on Linux the abstract socket namespace;
//! on other Unices `/tmp/<stem>` (courtesy of `GenericNamespaced`).

use interprocess::local_socket::Listener;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, Stream, prelude::*};
use std::io;

/// Sanitize a model name so it is safe to embed in a socket/pipe name.
/// Keeps ASCII alphanumerics, `-`, and `.`; maps everything else to `_`.
fn sanitize(model: &str) -> String {
    model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The stem used as the socket / pipe name, incorporating the protocol major
/// version so a version bump automatically creates a fresh endpoint.
fn name_stem(model: &str) -> String {
    format!(
        "kimetsu-embedd-p{}-{}",
        super::PROTOCOL_MAJOR,
        sanitize(model)
    )
}

/// Build a `Name` from `stem` using the namespaced strategy.
///
/// The returned `Name` borrows from the provided `String`, so the `String`
/// must outlive any use of the `Name`.
fn ns_name(stem: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    stem.to_ns_name::<GenericNamespaced>()
}

/// Bind a single-instance listener for `model`.
///
/// - If a live daemon already holds the name, returns `io::ErrorKind::AddrInUse`.
/// - On Unix, if the socket file is stale (no live listener), the default
///   `reclaim_name` behaviour in `ListenerOptions` removes it automatically.
/// - On Windows (named pipes), a new server instance is always creatable
///   independent of other instances — `reclaim_name` is a no-op — so we
///   first try to connect; if that succeeds we know a live daemon owns it.
pub fn listen(model: &str) -> io::Result<Listener> {
    let stem = name_stem(model);

    // First, check if a live daemon already holds this name.  A successful
    // `connect` is definitive proof of a live listener.
    if try_connect_probe(&stem).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("a live embed-daemon for model '{model}' is already running"),
        ));
    }

    // No live daemon found — attempt to create the listener.
    // `reclaim_name` (enabled by default) handles stale filesystem sockets.
    let name = ns_name(&stem)?;
    ListenerOptions::new().name(name).create_sync()
}

/// Connect to the embed-daemon for `model`.
///
/// Fails immediately (with the OS error) when no listener is present.
pub fn connect(model: &str) -> io::Result<Stream> {
    let stem = name_stem(model);
    let name = ns_name(&stem)?;
    Stream::connect(name)
}

/// Internal probe: try to open a connection to `stem` without holding it.
/// Returns `Ok(())` if a live listener is there, `Err` otherwise.
fn try_connect_probe(stem: &str) -> io::Result<()> {
    let name = ns_name(stem)?;
    Stream::connect(name).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed_daemon::proto::{self, Request, Response};
    use std::io::BufReader;

    #[test]
    fn listen_then_connect_round_trips() {
        let model = format!(
            "test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );
        let listener = listen(&model).expect("listen");

        let handle = std::thread::spawn(move || {
            let conn = listener.accept().expect("accept");
            let mut reader = BufReader::new(&conn);
            let _req: Request = proto::read_line(&mut reader).expect("read");
            let mut w = &conn;
            proto::write_line(&mut w, &Response::Ok).expect("write");
        });

        let conn = connect(&model).expect("connect");
        let mut w = &conn;
        proto::write_line(&mut w, &Request::Ping).expect("write req");
        let mut reader = BufReader::new(&conn);
        let resp: Response = proto::read_line(&mut reader).expect("read resp");
        assert!(matches!(resp, Response::Ok));
        handle.join().unwrap();
    }

    #[test]
    fn connect_with_no_listener_errors() {
        assert!(connect("definitely-not-running-xyz").is_err());
    }
}
