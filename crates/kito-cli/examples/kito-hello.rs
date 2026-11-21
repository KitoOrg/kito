//! A plugin, for the tests and as the smallest example of one (ADR 0005).
//!
//! `kito hello env` says what Kito gave it; `kito hello ask <method>
//! [<params as JSON>]` asks the daemon, with the plugin's own key, and
//! prints the answer; `kito hello exit <code>` leaves with that code.

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
        _ => {
            eprintln!("hello: env | ask <method> [<params>] | exit <code>");
            ExitCode::from(2)
        }
    }
}
