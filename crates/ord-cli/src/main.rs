//! `ord` — thin client for the open-recorder daemon.
//!
//! Parses one subcommand, sends it to `ordd` over the control socket, prints
//! the event reply. This is what compositor keybinds call
//! (`bind = ALT, R, exec, ord save --last 30`).

use std::process::ExitCode;

use ord_common::{socket_path, ClipDuration, Command, Config, Event};

mod doctor;
mod export_cmd;

fn usage() -> &'static str {
    "open-recorder CLI\n\
     \n\
     usage:\n  \
       ord save [--last N]        save the last N seconds (default 30)\n  \
       ord mark                   bookmark this moment (chapter in the next save)\n  \
       ord shot                   save a screenshot of the latest frame\n  \
       ord record                 toggle manual recording\n  \
       ord status [--json]        show daemon status (JSON for waybar/scripts)\n  \
       ord buffer on|off          enable/disable the replay buffer\n  \
       ord config show            print the effective daemon configuration\n  \
       ord config set <key> <v>   change one setting (e.g. capture.fps 30)\n  \
       ord subscribe [--reconnect] stream daemon events (for the HUD)\n  \
       ord doctor [--fix]         diagnose/fix the NVIDIA P2 downclock\n  \
       ord export <in> ...        transcode/trim a clip (see `ord export --help`)\n"
}

/// What the top-level argument parse resolved to.
#[derive(Debug, PartialEq)]
enum Parsed {
    Help,
    Cmd { cmd: Command, json: bool },
    Subscribe { reconnect: bool },
    ConfigSet { key: String, value: String },
}

fn parse(args: impl Iterator<Item = String>) -> Result<Parsed, String> {
    let mut args = args;
    let sub = args.next().ok_or_else(|| usage().to_string())?;
    match sub.as_str() {
        "save" => {
            let mut seconds = 30u32;
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
            Ok(Parsed::Cmd {
                cmd: Command::SaveLast { duration },
                json: false,
            })
        }
        "record" => Ok(Parsed::Cmd {
            cmd: Command::ToggleRecord,
            json: false,
        }),
        "status" => {
            let json = match args.next().as_deref() {
                None => false,
                Some("--json") => true,
                Some(other) => return Err(format!("unknown flag: {other}")),
            };
            Ok(Parsed::Cmd {
                cmd: Command::Status,
                json,
            })
        }
        "subscribe" => {
            let reconnect = match args.next().as_deref() {
                None => false,
                Some("--reconnect") => true,
                Some(other) => return Err(format!("unknown flag: {other}")),
            };
            Ok(Parsed::Subscribe { reconnect })
        }
        "mark" => Ok(Parsed::Cmd {
            cmd: Command::Mark,
            json: false,
        }),
        "shot" => Ok(Parsed::Cmd {
            cmd: Command::Screenshot,
            json: false,
        }),
        "config" => match args.next().as_deref() {
            Some("show") => Ok(Parsed::Cmd {
                cmd: Command::GetConfig,
                json: false,
            }),
            Some("set") => {
                let key = args.next().ok_or("config set needs <key> <value>")?;
                let value = args.next().ok_or("config set needs <key> <value>")?;
                if args.next().is_some() {
                    return Err("config set takes exactly <key> <value>".into());
                }
                Ok(Parsed::ConfigSet { key, value })
            }
            _ => Err("config needs a subcommand: show | set <key> <value>".into()),
        },
        "buffer" => {
            let v = args.next().ok_or("buffer needs on|off")?;
            let enabled = match v.as_str() {
                "on" => true,
                "off" => false,
                _ => return Err("buffer needs on|off".into()),
            };
            Ok(Parsed::Cmd {
                cmd: Command::SetBuffer { enabled },
                json: false,
            })
        }
        "-h" | "--help" | "help" => Ok(Parsed::Help),
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
        Event::RecordState { recording, path } => {
            let verb = if recording { "started" } else { "stopped" };
            match path {
                Some(p) => format!("recording {verb} -> {p}"),
                None => format!("recording {verb}"),
            }
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
        Event::ScreenshotSaved { path } => format!("screenshot -> {path}"),
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

/// Machine-readable rendering for scripts/waybar (`ord status --json`).
fn render_json(event: &Event) -> Result<String, String> {
    match event {
        Event::Status {
            buffer_enabled,
            recording,
            buffered_seconds,
            buffered_frames,
            buffered_keyframes,
        } => Ok(serde_json::json!({
            "buffer_enabled": buffer_enabled,
            "recording": recording,
            "buffered_seconds": buffered_seconds,
            "buffered_frames": buffered_frames,
            "buffered_keyframes": buffered_keyframes,
        })
        .to_string()),
        Event::Error { message } => Ok(serde_json::json!({ "error": message }).to_string()),
        other => Err(format!("--json is not supported for this reply: {other:?}")),
    }
}

/// Set a dotted `section.key` in a TOML document, keeping (or inferring) the
/// value's type. Pure so `config set` parsing is unit-testable.
fn set_dotted(doc: &mut toml::Value, key: &str, value: &str) -> Result<(), String> {
    let mut parts: Vec<&str> = key.split('.').collect();
    let last = parts.pop().filter(|s| !s.is_empty()).ok_or("empty key")?;
    let mut cur = doc;
    for p in &parts {
        cur = cur
            .get_mut(p)
            .ok_or_else(|| format!("unknown config section: {p}"))?;
    }
    let table = cur
        .as_table_mut()
        .ok_or_else(|| format!("{key} is not inside a config section"))?;
    let new = match table.get(last) {
        Some(toml::Value::Boolean(_)) => toml::Value::Boolean(
            value
                .parse()
                .map_err(|_| format!("{key} needs true|false"))?,
        ),
        Some(toml::Value::Integer(_)) => toml::Value::Integer(
            value
                .parse()
                .map_err(|_| format!("{key} needs an integer"))?,
        ),
        Some(toml::Value::Float(_)) => {
            toml::Value::Float(value.parse().map_err(|_| format!("{key} needs a number"))?)
        }
        Some(toml::Value::String(_)) => toml::Value::String(value.to_string()),
        Some(_) => return Err(format!("{key} is not a settable scalar")),
        // Absent (an unset Option field): infer, and let the config parse
        // validate the final type — unknown keys are rejected there.
        None => {
            if let Ok(b) = value.parse::<bool>() {
                toml::Value::Boolean(b)
            } else if let Ok(i) = value.parse::<i64>() {
                toml::Value::Integer(i)
            } else if let Ok(f) = value.parse::<f64>() {
                toml::Value::Float(f)
            } else {
                toml::Value::String(value.to_string())
            }
        }
    };
    table.insert(last.to_string(), new);
    Ok(())
}

/// `ord config set key value`: fetch the effective config, patch one dotted
/// key, and send the result back — the daemon persists the sparse diff.
fn config_set(client: &mut ord_common::Client, key: &str, value: &str) -> Result<String, String> {
    let reply = client
        .request(&Command::GetConfig)
        .map_err(|e| e.to_string())?;
    let Event::Config { effective, .. } = reply else {
        return Err(format!("unexpected reply: {}", render(reply)));
    };
    let text = effective.to_toml_string().map_err(|e| e.to_string())?;
    let mut doc: toml::Value = toml::from_str(&text).map_err(|e| e.to_string())?;
    set_dotted(&mut doc, key, value)?;
    let patched = toml::to_string(&doc).map_err(|e| e.to_string())?;
    let config = Config::from_toml_str(&patched).map_err(|e| format!("invalid setting: {e}"))?;
    let reply = client
        .request(&Command::SetConfig {
            config: Box::new(config),
        })
        .map_err(|e| e.to_string())?;
    match reply {
        Event::Config { .. } => Ok(format!("set {key} = {value}")),
        Event::Error { message } => Err(message),
        other => Err(format!("unexpected reply: {}", render(other))),
    }
}

fn connect() -> Result<ord_common::Client, String> {
    ord_common::connect(socket_path()).map_err(|e| e.to_string())
}

/// Stream daemon events, printing each. Without `--reconnect` a closed
/// connection reports and exits nonzero; with it, retry with a 1 s backoff.
fn run_subscribe(reconnect: bool) -> Result<(), String> {
    loop {
        let client = match connect() {
            Ok(c) => c,
            Err(e) if reconnect => {
                eprintln!("ord: {e}; retrying in 1s");
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
            Err(e) => return Err(e),
        };
        match client.subscribe() {
            Ok(events) => {
                for event in events {
                    println!("{}", render(event));
                }
            }
            Err(e) if !reconnect => return Err(e.to_string()),
            Err(e) => eprintln!("ord: subscribe failed: {e}"),
        }
        if !reconnect {
            return Err("daemon connection closed".into());
        }
        eprintln!("ord: daemon connection closed; reconnecting in 1s");
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn run() -> Result<(), String> {
    // `export` and `doctor` are local operations (no daemon socket), so they
    // are dispatched before any connection.
    let mut top = std::env::args().skip(1);
    if let Some(first) = top.next() {
        if first == "-V" || first == "--version" || first == "version" {
            println!("ord {}", ord_common::version::long());
            return Ok(());
        }
        if first == "export" {
            return export_cmd::run_export(top);
        }
        if first == "doctor" {
            return doctor::run(top);
        }
    }

    match parse(std::env::args().skip(1))? {
        Parsed::Help => {
            println!("{}", usage());
            Ok(())
        }
        Parsed::Subscribe { reconnect } => run_subscribe(reconnect),
        Parsed::ConfigSet { key, value } => {
            let mut client = connect()?;
            println!("{}", config_set(&mut client, &key, &value)?);
            Ok(())
        }
        Parsed::Cmd { cmd, json } => {
            let mut client = connect()?;
            let event = client.request(&cmd).map_err(|e| e.to_string())?;
            let is_error = matches!(event, Event::Error { .. });
            let line = if json {
                render_json(&event)?
            } else {
                render(event)
            };
            println!("{line}");
            if is_error {
                return Err(String::new());
            }
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("{msg}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Parsed, String> {
        parse(s.split_whitespace().map(String::from))
    }

    #[test]
    fn save_defaults_and_last_flag() {
        assert_eq!(
            p("save").unwrap(),
            Parsed::Cmd {
                cmd: Command::SaveLast {
                    duration: ClipDuration::new(30).unwrap()
                },
                json: false
            }
        );
        assert_eq!(
            p("save --last 90").unwrap(),
            Parsed::Cmd {
                cmd: Command::SaveLast {
                    duration: ClipDuration::new(90).unwrap()
                },
                json: false
            }
        );
        assert!(p("save --last 0").is_err());
        assert!(p("save --nope").is_err());
    }

    #[test]
    fn status_json_flag() {
        assert_eq!(
            p("status --json").unwrap(),
            Parsed::Cmd {
                cmd: Command::Status,
                json: true
            }
        );
        assert!(p("status --wat").is_err());
    }

    #[test]
    fn buffer_and_record_and_marks() {
        assert_eq!(
            p("buffer on").unwrap(),
            Parsed::Cmd {
                cmd: Command::SetBuffer { enabled: true },
                json: false
            }
        );
        assert!(p("buffer maybe").is_err());
        assert_eq!(
            p("record").unwrap(),
            Parsed::Cmd {
                cmd: Command::ToggleRecord,
                json: false
            }
        );
        assert_eq!(
            p("mark").unwrap(),
            Parsed::Cmd {
                cmd: Command::Mark,
                json: false
            }
        );
        assert_eq!(
            p("shot").unwrap(),
            Parsed::Cmd {
                cmd: Command::Screenshot,
                json: false
            }
        );
    }

    #[test]
    fn config_show_and_set() {
        assert_eq!(
            p("config show").unwrap(),
            Parsed::Cmd {
                cmd: Command::GetConfig,
                json: false
            }
        );
        assert_eq!(
            p("config set capture.fps 30").unwrap(),
            Parsed::ConfigSet {
                key: "capture.fps".into(),
                value: "30".into()
            }
        );
        assert!(p("config set capture.fps").is_err());
        assert!(p("config").is_err());
    }

    #[test]
    fn subscribe_with_reconnect() {
        assert_eq!(
            p("subscribe").unwrap(),
            Parsed::Subscribe { reconnect: false }
        );
        assert_eq!(
            p("subscribe --reconnect").unwrap(),
            Parsed::Subscribe { reconnect: true }
        );
    }

    #[test]
    fn help_is_ok_not_error() {
        assert_eq!(p("--help").unwrap(), Parsed::Help);
        assert_eq!(p("help").unwrap(), Parsed::Help);
        assert!(p("frobnicate").is_err());
    }

    #[test]
    fn set_dotted_keeps_types() {
        let mut doc: toml::Value = toml::from_str(
            "[capture]\nfps = 60\nauto_arm = false\ntarget = \"portal\"\n[storage]\n",
        )
        .unwrap();
        set_dotted(&mut doc, "capture.fps", "30").unwrap();
        set_dotted(&mut doc, "capture.auto_arm", "true").unwrap();
        set_dotted(&mut doc, "capture.target", "DP-1").unwrap();
        assert_eq!(doc["capture"]["fps"].as_integer(), Some(30));
        assert_eq!(doc["capture"]["auto_arm"].as_bool(), Some(true));
        assert_eq!(doc["capture"]["target"].as_str(), Some("DP-1"));

        // Type mismatches are rejected with the key in the message.
        assert!(set_dotted(&mut doc, "capture.fps", "fast").is_err());
        // Unknown sections error; absent keys in a real section are inferred.
        assert!(set_dotted(&mut doc, "nope.key", "1").is_err());
        set_dotted(&mut doc, "storage.max_gib", "50").unwrap();
        assert_eq!(doc["storage"]["max_gib"].as_integer(), Some(50));
    }

    #[test]
    fn render_json_status() {
        let s = render_json(&Event::Status {
            buffer_enabled: true,
            recording: false,
            buffered_seconds: 12,
            buffered_frames: 720,
            buffered_keyframes: 12,
        })
        .unwrap();
        assert!(s.contains("\"buffer_enabled\":true"), "{s}");
        assert!(s.contains("\"buffered_seconds\":12"), "{s}");
        assert!(render_json(&Event::CaptureRestarted).is_err());
    }
}
