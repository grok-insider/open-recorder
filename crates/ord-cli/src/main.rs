//! `ord` — thin client for the open-recorder daemon.
//!
//! Parses one subcommand, sends it to `ordd` over the Unix socket, prints the
//! event reply. This is what compositor keybinds call
//! (`bind = ALT, R, exec, ord save --last 30`).

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

use ord_common::{read_frame, write_frame, ClipDuration, Command, Event};

fn socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(dir).join("open-recorder.sock")
}

fn usage() -> &'static str {
    "open-recorder CLI\n\
     \n\
     usage:\n  \
       ord save [--last N]   save the last N seconds (default 30)\n  \
       ord record            toggle manual recording\n  \
       ord status            show daemon status\n  \
       ord buffer on|off     enable/disable the replay buffer\n"
}

fn parse() -> Result<Command, String> {
    let mut args = std::env::args().skip(1);
    let sub = args.next().ok_or_else(|| usage().to_string())?;
    match sub.as_str() {
        "save" => {
            let mut seconds = 30u32;
            // Optional: --last N
            let mut rest = args;
            while let Some(flag) = rest.next() {
                match flag.as_str() {
                    "--last" => {
                        let v = rest.next().ok_or("--last needs a value")?;
                        seconds = v.parse().map_err(|_| "--last must be a number")?;
                    }
                    other => return Err(format!("unknown flag: {other}")),
                }
            }
            let duration = ClipDuration::new(seconds).ok_or("--last must be >= 1")?;
            Ok(Command::SaveLast { duration })
        }
        "record" => Ok(Command::ToggleRecord),
        "status" => Ok(Command::Status),
        "buffer" => {
            let v = args.next().ok_or("buffer needs on|off")?;
            let enabled = match v.as_str() {
                "on" => true,
                "off" => false,
                _ => return Err("buffer needs on|off".into()),
            };
            Ok(Command::SetBuffer { enabled })
        }
        "-h" | "--help" | "help" => Err(usage().to_string()),
        other => Err(format!("unknown command: {other}\n\n{}", usage())),
    }
}

fn render(event: Event) -> String {
    match event {
        Event::ClipSaved { path, duration } => {
            format!("saved {}s clip -> {path}", duration.get())
        }
        Event::BufferState { enabled } => {
            format!(
                "replay buffer {}",
                if enabled { "enabled" } else { "disabled" }
            )
        }
        Event::RecordState { recording } => {
            format!(
                "recording {}",
                if recording { "started" } else { "stopped" }
            )
        }
        Event::Status {
            buffer_enabled,
            recording,
            buffered_seconds,
        } => format!(
            "buffer: {} | recording: {} | buffered: {}s",
            if buffer_enabled { "on" } else { "off" },
            if recording { "yes" } else { "no" },
            buffered_seconds
        ),
        Event::Error { message } => format!("error: {message}"),
    }
}

fn run() -> Result<(), String> {
    let cmd = parse()?;
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        format!(
            "cannot reach ordd at {} ({e}). Is the daemon running?",
            path.display()
        )
    })?;
    write_frame(&mut stream, &cmd.encode().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let bytes = read_frame(&mut stream).map_err(|e| e.to_string())?;
    let event = Event::decode(&bytes).map_err(|e| e.to_string())?;
    println!("{}", render(event));
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}
