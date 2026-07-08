//! Linux raw-keyboard reader for the pressed-key HUD.
//!
//! This is intentionally owned by `ord-hud`: enabling pressed keys is a visual
//! demo aid, not daemon state, and raw input must never cross the recorder IPC.

use std::fs::OpenOptions;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};

use input::event::keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait};
use input::event::Event;
use input::{Libinput, LibinputInterface};

use crate::PressedKeyEvent;

/// Message emitted by the raw input reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PressedKeyMessage {
    Event(PressedKeyEvent),
    Error(String),
}

/// Background libinput reader. Dropping it asks the thread to stop and joins it.
pub struct PressedKeyReader {
    rx: mpsc::Receiver<PressedKeyMessage>,
    stop: mpsc::Sender<()>,
    join: Option<JoinHandle<()>>,
}

impl PressedKeyReader {
    /// Start reading keyboard events on the current seat.
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let (stop, stop_rx) = mpsc::channel();
        let join = thread::spawn(move || run(tx, stop_rx));
        Self {
            rx,
            stop,
            join: Some(join),
        }
    }

    /// Poll one pending key-reader message without blocking.
    pub fn try_recv(&mut self) -> Result<PressedKeyMessage, mpsc::TryRecvError> {
        self.rx.try_recv()
    }
}

impl Drop for PressedKeyReader {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct Interface {
    tx: mpsc::Sender<PressedKeyMessage>,
    reported_open_error: Arc<AtomicBool>,
}

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let read = (flags & libc::O_ACCMODE == libc::O_RDONLY)
            || (flags & libc::O_ACCMODE == libc::O_RDWR);
        let write = (flags & libc::O_ACCMODE == libc::O_WRONLY)
            || (flags & libc::O_ACCMODE == libc::O_RDWR);
        match OpenOptions::new()
            .custom_flags(flags)
            .read(read)
            .write(write)
            .open(path)
        {
            Ok(file) => Ok(file.into()),
            Err(err) => {
                let errno = err.raw_os_error().unwrap_or(libc::EIO);
                if !self.reported_open_error.swap(true, Ordering::Relaxed) {
                    let msg = if errno == libc::EACCES || errno == libc::EPERM {
                        format!(
                            "pressed keys need read access to {} (grant /dev/input permission, then restart ord-hud)",
                            path.display()
                        )
                    } else {
                        format!("could not open input device {}: {err}", path.display())
                    };
                    let _ = self.tx.send(PressedKeyMessage::Error(msg));
                }
                Err(errno)
            }
        }
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(fd);
    }
}

fn run(tx: mpsc::Sender<PressedKeyMessage>, stop_rx: mpsc::Receiver<()>) {
    let mut input = Libinput::new_with_udev(Interface {
        tx: tx.clone(),
        reported_open_error: Arc::new(AtomicBool::new(false)),
    });
    if input.udev_assign_seat("seat0").is_err() {
        let _ = tx.send(PressedKeyMessage::Error(
            "could not start pressed-key input reader on seat0".to_string(),
        ));
        return;
    }

    loop {
        if stop_rx.try_recv().is_ok() {
            return;
        }
        if !wait_for_input(input.as_raw_fd()) {
            continue;
        }
        if let Err(e) = input.dispatch() {
            let _ = tx.send(PressedKeyMessage::Error(format!(
                "pressed-key input reader stopped: {e}"
            )));
            return;
        }
        for event in &mut input {
            let Event::Keyboard(KeyboardEvent::Key(key)) = event else {
                continue;
            };
            let pressed = match key.key_state() {
                KeyState::Pressed => true,
                KeyState::Released => false,
            };
            if tx
                .send(PressedKeyMessage::Event(PressedKeyEvent {
                    code: key.key(),
                    pressed,
                }))
                .is_err()
            {
                return;
            }
        }
    }
}

fn wait_for_input(fd: i32) -> bool {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: `pollfd` points to one initialized pollfd value for the whole
        // call, and libinput owns the fd lifetime while this thread runs.
        let ready = unsafe { libc::poll(&mut pollfd, 1, 250) };
        if ready > 0 {
            return pollfd.revents & libc::POLLIN != 0;
        }
        if ready == 0 {
            return false;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return false;
        }
    }
}
