//! `ord` — thin client for the open-recorder daemon.
//!
//! Parses one subcommand, sends it to `ordd` over the Unix socket, prints the
//! event reply. This is what compositor keybinds call
//! (`bind = ALT, R, exec, ord save --last 30`).

use std::process::ExitCode;

use ord_common::{socket_path, ClipDuration, Command, Event};

mod export_cmd;

fn usage() -> &'static str {
    "open-recorder CLI\n\
     \n\
     usage:\n  \
       ord save [--last N]   save the last N seconds (default 30)\n  \
       ord mark              bookmark this moment (chapter in the next save)\n  \
       ord record            toggle manual recording\n  \
       ord status            show daemon status\n  \
       ord buffer on|off     enable/disable the replay buffer\n  \
       ord config show       print the effective daemon configuration\n  \
       ord subscribe         stream daemon events (for the HUD)\n  \
       ord export <in> ...   transcode/trim a clip (see `ord export --help`)\n"
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
        "subscribe" => Ok(Command::Subscribe),
        "mark" => Ok(Command::Mark),
        "config" => match args.next().as_deref() {
            Some("show") => Ok(Command::GetConfig),
            _ => Err("config needs a subcommand: show".into()),
        },
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
            buffered_frames,
            buffered_keyframes,
        } => format!(
            "buffer: {} | recording: {} | buffered: {}s ({} frames, {} keyframes)",
            if buffer_enabled { "on" } else { "off" },
            if recording { "yes" } else { "no" },
            buffered_seconds,
            buffered_frames,
            buffered_keyframes
        ),
        Event::Error { message } => format!("error: {message}"),
        Event::Marked { auto_saving } => {
            if auto_saving {
                "marked (auto-saving a clip)".to_string()
            } else {
                "marked".to_string()
            }
        }
        Event::CaptureRestarted => "capture restarted".to_string(),
        Event::Config { effective, base } => {
            let overridden = effective.overridden_fields(&base);
            let toml = effective
                .to_toml_string()
                .unwrap_or_else(|e| format!("error: {e}"));
            if overridden.is_empty() {
                toml
            } else {
                format!("{toml}\n# runtime overrides: {}", overridden.join(", "))
            }
        }
    }
}

fn run() -> Result<(), String> {
    // `export` is a local operation (transcode a file), not a daemon command, so
    // handle it before any socket connection.
    let mut top = std::env::args().skip(1);
    if let Some(first) = top.next() {
        if first == "export" {
            return export_cmd::run_export(top);
        }
    }

    let cmd = parse()?;
    let mut client = ord_common::connect(socket_path()).map_err(|e| e.to_string())?;

    // Subscribe streams events until the daemon closes; print each as it arrives.
    if matches!(cmd, Command::Subscribe) {
        let events = client.subscribe().map_err(|e| e.to_string())?;
        for event in events {
            println!("{}", render(event));
        }
        return Ok(());
    }

    let event = client.request(&cmd).map_err(|e| e.to_string())?;
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
