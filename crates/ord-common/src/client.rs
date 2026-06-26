//! Small daemon-socket client shared by every frontend (`ord`, `ord-hud`,
//! future settings UIs), so the connect → frame → decode dance is written once.
//!
//! The framing/encoding itself lives in [`frame`](crate::frame) and
//! [`ipc`](crate::ipc); this is just the request/response and subscribe-stream
//! plumbing over the platform [`transport`](crate::transport) stream (a Unix
//! socket on unix, a loopback TCP connection elsewhere).

use std::io;
use std::path::Path;

use crate::frame::{read_frame, write_frame};
use crate::ipc::{Command, Event, ProtocolError};
use crate::transport::{self, Stream};

/// Errors talking to the daemon.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot reach ordd at {path} ({source}). Is the daemon running?")]
    Connect { path: String, source: io::Error },
    #[error("io error talking to ordd: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// Connect to the daemon socket at `path`.
pub fn connect(path: impl AsRef<Path>) -> Result<Client, ClientError> {
    let path = path.as_ref();
    let stream = transport::connect(path).map_err(|source| ClientError::Connect {
        path: path.display().to_string(),
        source,
    })?;
    Ok(Client { stream })
}

/// One connection to the daemon.
#[derive(Debug)]
pub struct Client {
    stream: Stream,
}

impl Client {
    /// Send one command and read the single reply event.
    pub fn request(&mut self, cmd: &Command) -> Result<Event, ClientError> {
        self.send(cmd)?;
        self.read_event()
    }

    /// Send [`Command::Subscribe`] and turn the connection into the pushed
    /// event stream: the returned iterator yields events until the daemon
    /// closes the connection (undecodable frames are skipped). The first item
    /// is the daemon's initial status snapshot.
    pub fn subscribe(mut self) -> Result<EventStream, ClientError> {
        self.send(&Command::Subscribe)?;
        Ok(EventStream {
            stream: self.stream,
        })
    }

    fn send(&mut self, cmd: &Command) -> Result<(), ClientError> {
        let payload = cmd.encode()?;
        write_frame(&mut self.stream, &payload)?;
        Ok(())
    }

    fn read_event(&mut self) -> Result<Event, ClientError> {
        let bytes = read_frame(&mut self.stream)?;
        Ok(Event::decode(&bytes)?)
    }
}

/// Iterator over events pushed on a subscribed connection.
pub struct EventStream {
    stream: Stream,
}

impl Iterator for EventStream {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        loop {
            let bytes = read_frame(&mut self.stream).ok()?;
            if let Ok(ev) = Event::decode(&bytes) {
                return Some(ev);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_error_is_actionable() {
        let err = connect("/nonexistent/ord.sock").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Is the daemon running?"), "{msg}");
        assert!(msg.contains("/nonexistent/ord.sock"), "{msg}");
    }

    // The round-trip tests hand-roll a Unix-socket server, so they are
    // unix-only; the host CI runs on Linux, so they still execute there. The
    // non-unix (loopback TCP) transport's only non-type-checked logic is
    // `parse_port`, covered in the `transport` module's tests.
    #[cfg(unix)]
    use crate::ipc::Event;
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;
    #[cfg(unix)]
    use std::path::PathBuf;

    #[cfg(unix)]
    fn temp_sock(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ord-client-test-{}-{}.sock",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[cfg(unix)]
    #[test]
    fn request_round_trips_over_socket() {
        let path = temp_sock("req");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let bytes = read_frame(&mut conn).unwrap();
            let cmd = Command::decode(&bytes).unwrap();
            assert!(matches!(cmd, Command::Status));
            let reply = Event::BufferState { enabled: true };
            write_frame(&mut conn, &reply.encode().unwrap()).unwrap();
        });

        let mut client = connect(&path).unwrap();
        let event = client.request(&Command::Status).unwrap();
        assert_eq!(event, Event::BufferState { enabled: true });
        server.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn subscribe_streams_until_close() {
        let path = temp_sock("sub");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let _ = read_frame(&mut conn).unwrap(); // the Subscribe command
            for enabled in [true, false] {
                let ev = Event::BufferState { enabled };
                write_frame(&mut conn, &ev.encode().unwrap()).unwrap();
            }
            // Connection drops here -> the stream ends.
        });

        let events: Vec<Event> = connect(&path).unwrap().subscribe().unwrap().collect();
        assert_eq!(
            events,
            vec![
                Event::BufferState { enabled: true },
                Event::BufferState { enabled: false },
            ]
        );
        server.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
