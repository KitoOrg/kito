//! A plugin, for the tests and as the smallest example of one (ADR 0005).
//!
//! `kito hello env` says what Kito gave it; `kito hello ask <method>
//! [<params as JSON>]` asks the daemon, with the plugin's own key, and
//! prints the answer; `kito hello exit <code>` leaves with that code.
//!
//! As a gate (`kito-hello gate <name>`) it passes, unless the copy it
//! looks at has a file `GATE_FAIL` or `GATE_WAIT`, whose words it then
//! answers with; the gate `mute` answers nothing and `slow` never does.
//! For an event (`kito-hello event`) it writes a line in a file of its
//! own, and fails for an event that says `explode`.
//!
//! As a tool for agents (`kito-hello tool <name>`): `shout` answers the
//! text it is given in capitals, with how many lanes are open (asked of
//! the daemon, with its own key); `plain` prints text that is not JSON;
//! `broken` fails. As an assistant (`kito-hello assistant setup |
//! remove`) it writes and removes `HELLO.md`, the rules of an assistant
//! that does not exist.

use kitod::client::Client;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let said = |name: &str| std::env::var(name).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("env") => {
            println!("repo: {}", said(kitod::plugins::ENV_REPO));
            println!("api: {}", said(kitod::plugins::ENV_API));
            println!("key: {} characters", said(kitod::plugins::ENV_KEY).len());
            ExitCode::SUCCESS
        }
        Some("exit") => ExitCode::from(args.get(1).and_then(|code| code.parse().ok()).unwrap_or(0u8)),
        Some("ask") => {
            let method = args.get(1).map(String::as_str).unwrap_or("ping");
            let params =
                args.get(2).and_then(|text| serde_json::from_str(text).ok()).unwrap_or(serde_json::json!({}));
            let root = PathBuf::from(said(kitod::plugins::ENV_REPO));
            // The client says the key Kito put in the environment.
            let answer = Client::connect(&root).and_then(|mut client| client.call_value(method, params));
            match answer {
                Ok(answer) => {
                    println!("{answer}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("hello: {e}");
                    ExitCode::from(3)
                }
            }
        }
        Some("gate") => {
            let told: serde_json::Value = serde_json::from_reader(std::io::stdin()).unwrap_or_default();
            let lanes: Vec<String> = told["lanes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|lane| {
                    let files = lane["diff"]["files"].as_array().map_or(0, Vec::len);
                    format!(
                        "{} by {} ({files} files)",
                        lane["lane"].as_str().unwrap_or("?"),
                        lane["agent"].as_str().unwrap_or("?")
                    )
                })
                .collect();
            let about = format!("{} onto {}", lanes.join(", "), told["target"].as_str().unwrap_or("?"));
            let words = |file: &str| std::fs::read_to_string(file).ok().map(|words| words.trim().to_owned());
            match args.get(1).map(String::as_str) {
                Some("mute") => return ExitCode::SUCCESS,
                Some("slow") => std::thread::sleep(std::time::Duration::from_secs(600)),
                _ => {}
            }
            let answer = match (words("GATE_FAIL"), words("GATE_WAIT")) {
                (Some(why), _) => {
                    serde_json::json!({ "result": "fail", "message": format!("{why}: {about}") })
                }
                (None, Some(why)) => serde_json::json!({ "result": "wait", "message": why }),
                (None, None) => serde_json::json!({ "result": "pass" }),
            };
            println!("looked at {about}");
            println!("{answer}");
            ExitCode::SUCCESS
        }
        Some("tool") => {
            let asked: serde_json::Value = serde_json::from_reader(std::io::stdin()).unwrap_or_default();
            match args.get(1).map(String::as_str) {
                Some("shout") => {
                    let root = PathBuf::from(said(kitod::plugins::ENV_REPO));
                    let lanes = Client::connect(&root)
                        .and_then(|mut client| client.call_value("lanes", serde_json::json!({})))
                        .ok()
                        .and_then(|lanes| lanes.as_array().map(Vec::len));
                    let text = asked["text"].as_str().unwrap_or_default().to_uppercase();
                    println!("{}", serde_json::json!({ "said": text, "open_lanes": lanes }));
                    ExitCode::SUCCESS
                }
                Some("plain") => {
                    println!("just words");
                    ExitCode::SUCCESS
                }
                _ => {
                    eprintln!("hello: this tool is broken");
                    ExitCode::from(4)
                }
            }
        }
        Some("assistant") => {
            let rules = PathBuf::from(said(kitod::plugins::ENV_REPO)).join("HELLO.md");
            match args.get(1).map(String::as_str) {
                Some("setup") => {
                    let text = "# Rules for hello\n\nWork in a Kito lane: open one before you edit.\n";
                    if let Err(e) = std::fs::write(&rules, text) {
                        eprintln!("hello: {e}");
                        return ExitCode::from(1);
                    }
                    println!("hello is set up for Kito: its rules are in HELLO.md.");
                    println!(
                        "It has no hook before an edit: an edit outside a lane is caught when it is saved."
                    );
                    ExitCode::SUCCESS
                }
                Some("remove") => {
                    let _ = std::fs::remove_file(&rules);
                    println!("hello is no longer set up for Kito.");
                    ExitCode::SUCCESS
                }
                _ => ExitCode::from(2),
            }
        }
        Some("event") => {
            use std::io::Write;
            let event: serde_json::Value = serde_json::from_reader(std::io::stdin()).unwrap_or_default();
            let text = |key: &str| event[key].as_str().unwrap_or("-").to_owned();
            if text("what").contains("explode") {
                eprintln!("hello: it exploded");
                return ExitCode::from(1);
            }
            let dir = PathBuf::from(said(kitod::plugins::ENV_REPO)).join(".kito/plugins/hello");
            let _ = std::fs::create_dir_all(&dir);
            let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("events.log"));
            match log.and_then(|mut log| writeln!(log, "{} {} {}", text("kind"), text("lane"), text("what")))
            {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("hello: {e}");
                    ExitCode::from(1)
                }
            }
        }
        _ => {
            eprintln!("hello: env | ask <method> [<params>] | exit <code> | gate <name> | event");
            ExitCode::from(2)
        }
    }
}
