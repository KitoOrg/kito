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
//!
//! For a server (ADR 0006): `kito-hello server-event` writes a line for
//! the event in a file of its own under the server's directory, fails
//! for one that says `explode`, and for one that says `linger` leaves a
//! child behind that holds its output (`kito-hello sleep <seconds>`);
//! `kito-hello greet [...]`, a command
//! it adds to `kito-server`, says where it runs and what it was told.
//! `kito-hello serve-pages` serves one page, which says what the server
//! told it with the request, until its standard input closes.
//!
//! It signs people in, too (`kito-hello sign-in begin | finish | lookup
//! <login>`), as a provider that does not exist: whoever a file of its
//! own under the server's directory (`plugins/hello/sign-in`) names has
//! signed in, nobody has while the file is not there, and `refuse` in it
//! is a person who said no. In a browser the provider's answer is in the
//! query (`code=<login>`). It knows everybody but `nobody`.
//!
//! For the same plugin in a directory connected to the server (ADR 0007):
//! `kito-hello answer <question>` says what it was asked, by whom and
//! about which repository, and fails for the question `explode`. From
//! such a directory, `kito hello ask plugin_call '{"ask": "greet"}'`
//! asks it.

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
            println!("socket: {}", said(kitod::plugins::ENV_SOCKET));
            ExitCode::SUCCESS
        }
        Some("exit") => ExitCode::from(args.get(1).and_then(|code| code.parse().ok()).unwrap_or(0u8)),
        Some("sleep") => {
            let seconds = args.get(1).and_then(|seconds| seconds.parse().ok()).unwrap_or(1);
            std::thread::sleep(std::time::Duration::from_secs(seconds));
            ExitCode::SUCCESS
        }
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
        Some("server-event") => {
            use std::io::Write;
            let event: serde_json::Value = serde_json::from_reader(std::io::stdin()).unwrap_or_default();
            let text = |key: &str| event[key].as_str().unwrap_or("-").to_owned();
            if text("what").contains("explode") {
                eprintln!("hello: it exploded");
                return ExitCode::from(1);
            }
            // For one that says `linger` it leaves a child behind, which
            // keeps what it prints open for 20 seconds.
            if text("what").contains("linger") {
                let me = std::env::current_exe().unwrap_or_default();
                let _ = std::process::Command::new(me).args(["sleep", "20"]).spawn();
            }
            let dir = PathBuf::from(said("KITO_SERVER_DIR")).join("plugins/hello");
            let _ = std::fs::create_dir_all(&dir);
            let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("events.log"));
            match log.and_then(|mut log| writeln!(log, "{} {}", text("kind"), text("what"))) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("hello: {e}");
                    ExitCode::from(1)
                }
            }
        }
        // One last time, where it is disabled or removed (`[cleanup]`):
        // it takes out what `assistant setup` wrote, and says where it
        // was run in a file whoever tests it names.
        Some("cleanup") => {
            let place = args.get(1).cloned().unwrap_or_default();
            let repo = PathBuf::from(said(kitod::plugins::ENV_REPO));
            if place == "repository" && repo.join("CLEANUP_FAIL").is_file() {
                eprintln!("hello: it would not come out");
                return ExitCode::from(1);
            }
            if place == "repository" && std::fs::remove_file(repo.join("HELLO.md")).is_ok() {
                println!("took HELLO.md out");
            }
            if let Some(told) = std::env::var_os("KITO_HELLO_CLEANED") {
                let server = !said("KITO_SERVER_DIR").is_empty();
                let _ = std::fs::write(told, format!("{place} server={server}\n"));
            }
            ExitCode::SUCCESS
        }
        // What it says when a session starts (`[session]`).
        Some("session") => {
            println!("Mind the step.");
            ExitCode::SUCCESS
        }
        Some("answer") => {
            let asked = args.get(1).cloned().unwrap_or_default();
            if asked == "explode" {
                eprintln!("hello: it exploded");
                return ExitCode::from(1);
            }
            let params: serde_json::Value = serde_json::from_reader(std::io::stdin()).unwrap_or_default();
            println!("thinking about it");
            println!(
                "{}",
                serde_json::json!({
                    "asked": asked,
                    "params": params,
                    "by": said("KITO_ASKED_BY"),
                    "repo": said("KITO_ASKED_REPO"),
                    "role": said("KITO_ASKED_ROLE"),
                    "on_the_server": !said("KITO_SERVER_DIR").is_empty(),
                })
            );
            ExitCode::SUCCESS
        }
        Some("sign-in") => {
            let told: serde_json::Value = serde_json::from_reader(std::io::stdin()).unwrap_or_default();
            let person = |login: &str| {
                serde_json::json!({ "identity": {
                    // Not what the server keeps: it knows which plugin it asked.
                    "provider": "somebody-else",
                    "subject": format!("id-{login}"),
                    "login": login,
                    "email": format!("{login}@hello.example"),
                }})
            };
            let browser = told["kind"] == "browser";
            let answer = match args.get(1).map(String::as_str) {
                Some("begin") if browser => serde_json::json!({
                    "open": format!(
                        "https://hello.example/authorize?state=left-1&redirect_uri={}",
                        told["redirect"].as_str().unwrap_or_default()
                    ),
                    "state": "left-1",
                }),
                Some("begin") => serde_json::json!({
                    "open": "https://hello.example/device",
                    "code": "HELLO-1234",
                    "state": "device-1",
                    "interval": 0,
                }),
                Some("finish") => {
                    let file = PathBuf::from(said("KITO_SERVER_DIR")).join("plugins/hello/sign-in");
                    let said = match browser {
                        true => told["query"]["code"].as_str().map(str::to_owned),
                        false => std::fs::read_to_string(file).ok().map(|text| text.trim().to_owned()),
                    };
                    match said.as_deref() {
                        None | Some("") => serde_json::json!({ "wait": 0 }),
                        Some("refuse") => serde_json::json!({ "refused": "they said no at hello" }),
                        Some(login) => person(login),
                    }
                }
                // Not set up while a file of its own says so, with why.
                Some("ready") => {
                    let file = PathBuf::from(said("KITO_SERVER_DIR")).join("plugins/hello/not-ready");
                    match std::fs::read_to_string(file) {
                        Ok(why) => serde_json::json!({ "ready": false, "why": why.trim() }),
                        Err(_) => serde_json::json!({ "ready": true }),
                    }
                }
                Some("lookup") => match args.get(2).map(String::as_str) {
                    Some("nobody") | None => {
                        serde_json::json!({ "refused": "hello knows nobody by that name" })
                    }
                    Some(login) => person(login),
                },
                _ => return ExitCode::from(2),
            };
            println!("{answer}");
            ExitCode::SUCCESS
        }
        Some("serve-pages") => {
            use std::io::{BufRead, BufReader, Read, Write};
            // The server holds the standard input open: when it closes,
            // the server is gone.
            std::thread::spawn(|| {
                let mut gone = Vec::new();
                let _ = std::io::stdin().read_to_end(&mut gone);
                std::process::exit(0);
            });
            let secret = said("KITO_PAGES_SECRET");
            let listener = match std::net::TcpListener::bind(said("KITO_PAGES_LISTEN")) {
                Ok(listener) => listener,
                Err(e) => {
                    eprintln!("hello: {e}");
                    return ExitCode::from(1);
                }
            };
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(&stream);
                let mut first = String::new();
                let _ = reader.read_line(&mut first);
                let path = first.split(' ').nth(1).unwrap_or("?").to_owned();
                let mut headers = std::collections::BTreeMap::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        headers.insert(name.trim().to_lowercase(), value.trim().to_owned());
                    }
                }
                let told = |name: &str| headers.get(name).cloned().unwrap_or_else(|| "-".to_owned());
                // `/ask/<repository>/<method>`: the server asked as this
                // plugin, with its secret (ADR 0020).
                let asked = path.strip_prefix("/ask/").and_then(|rest| rest.split_once('/')).map(|(repo, method)| {
                    let api = said("KITO_SERVER_API");
                    let address = api.trim_start_matches("http://").to_owned();
                    let call = format!("{{\"method\":\"{method}\",\"params\":{{}}}}");
                    let request = format!(
                        "POST /-/api/{repo} HTTP/1.1\r\nHost: {address}\r\nx-kito-plugin: {secret}\r\n\
                         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{call}",
                        call.len()
                    );
                    let mut answer = String::new();
                    if let Ok(mut server) = std::net::TcpStream::connect(&address) {
                        let _ = server.write_all(request.as_bytes());
                        let _ = server.read_to_string(&mut answer);
                    }
                    let status = answer.lines().next().unwrap_or("").to_owned();
                    let body = answer.split_once("\r\n\r\n").map(|(_, body)| body.to_owned()).unwrap_or_default();
                    format!("asked={status}\n{body}\n")
                });
                let body = asked.unwrap_or_default()
                    + &format!(
                        "hello pages
path={path}
user={}
secret={}
mount={}
cookie={}
authorization={}
",
                        told("x-kito-user"),
                        if told("x-kito-secret") == secret { "ok" } else { "wrong" },
                        told("x-kito-mount"),
                        told("cookie"),
                        told("authorization"),
                    );
                let _ = write!(
                    &stream,
                    "HTTP/1.1 200 OK
Content-Type: text/plain
Content-Length: {}
Set-Cookie: kito_session=theirs
Set-Cookie: hello=1
Connection: close

{body}",
                    body.len()
                );
            }
            ExitCode::SUCCESS
        }
        Some("greet") => {
            let dir = PathBuf::from(said("KITO_SERVER_DIR"));
            let has = dir.join("server.toml").is_file();
            println!(
                "hello from a server{}: {}",
                if has { "" } else { " that is not there" },
                args[1..].join(" ")
            );
            ExitCode::from(if args.get(1).is_some_and(|arg| arg == "fail") { 5 } else { 0 })
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
