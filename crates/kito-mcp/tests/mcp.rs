//! The real MCP server, spoken to as a client would: JSON-RPC on its
//! standard input and output.

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// The daemon binary sits next to the server's. If the tests run for this
/// package alone it may not have been built.
fn kitod() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    let path = Path::new(env!("CARGO_BIN_EXE_kito-mcp"))
        .with_file_name(format!("kitod{}", std::env::consts::EXE_SUFFIX));
    BUILD.call_once(|| {
        if !path.is_file() {
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "kitod", "--bin", "kitod"])
                .status()
                .unwrap();
            assert!(status.success(), "building kitod");
        }
    });
    path
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Session {
    fn start(root: &Path, extra: &[&str]) -> Session {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kito-mcp"))
            .arg("--repo")
            .arg(root)
            .args(extra)
            .env("KITOD_BIN", kitod())
            .env("KITO_AUTHOR", "ada")
            .env("KITO_CONFIG_DIR", root.join(".absent-personal-config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env_remove("KITO_MCP_TOOL_PREFIX")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut session = Session { child, stdin, stdout, next: 1 };
        let hello = session.request(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }),
        );
        assert_eq!(hello["serverInfo"]["name"], "kito");
        assert!(hello["instructions"].as_str().unwrap().contains("open_lane"));
        session.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        session
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Sends a request and returns its `result` (panics on a protocol error).
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"));
        assert_eq!(reply["id"], id, "{reply}");
        assert!(reply.get("error").is_none(), "{method}: {reply}");
        reply["result"].clone()
    }

    /// Calls a tool and returns what it answered, as structured content.
    fn tool(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
        let result = self.request("tools/call", json!({ "name": name, "arguments": arguments }));
        let text = result["content"][0]["text"].as_str().unwrap_or_default().to_owned();
        if result["isError"].as_bool().unwrap_or(false) {
            return Err(text);
        }
        // The text is the same JSON, for clients that only read text.
        let from_text: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
        let structured = result["structuredContent"].clone();
        assert!(structured == from_text || structured["result"] == from_text, "{result}");
        Ok(from_text)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn stop_daemon(root: &Path) {
    let mut client = kitod::client::Client::connect(root).unwrap();
    let _ = client.call_value("shutdown", json!({}));
    let socket = kitod::rpc::socket_path(root);
    for _ in 0..2000 {
        if !socket.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the daemon did not stop");
}

const SHOP: &str = "export function total(items: Item[]): number {\n  let sum = 0;\n  for (const item of items) {\n    sum += item.price * item.quantity;\n  }\n  return sum;\n}\n";

fn new_repo() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    kitod::Repo::init(&root).unwrap().close().unwrap();
    fs::write(root.join(".gitignore"), ".kito/\n.absent-personal-config/\n").unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/shop.ts"), SHOP).unwrap();
    (tmp, root)
}

#[test]
fn an_agent_does_a_whole_piece_of_work_through_the_tools() {
    let (_tmp, root) = new_repo();
    let mut mcp = Session::start(&root, &[]);

    // The tools, by name, each with a schema.
    let listed = mcp.request("tools/list", json!({}));
    let names: Vec<&str> =
        listed["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "kito.open_lane",
            "kito.extend_scope",
            "kito.board",
            "kito.who_touches",
            "kito.sync",
            "kito.status",
            "kito.log",
            "kito.set_intent",
            "kito.rename_symbol",
            "kito.run",
            "kito.request_integration",
            "kito.integration_result",
            "kito.why",
            "kito.reasons",
            "kito.note",
            "kito.resolve_conflict",
            "kito.close_lane",
        ]
    );
    for tool in listed["tools"].as_array().unwrap() {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        assert!(tool["description"].as_str().unwrap().len() > 20, "{tool}");
    }

    // Open a lane, say why, work in its working copy.
    let lane = mcp
        .tool(
            "kito.open_lane",
            json!({
                "agent": "builder",
                "task": "task #55: absolute totals",
                "intent": "totals are never negative",
                "scope": ["src/shop.ts"],
            }),
        )
        .unwrap();
    assert_eq!((lane["id"].as_str(), lane["state"].as_str()), (Some("lane/1"), Some("active")));
    let workdir = PathBuf::from(lane["workdir"].as_str().unwrap());
    assert_eq!(fs::read_to_string(workdir.join("src/shop.ts")).unwrap(), SHOP);
    let intent = mcp
        .tool("kito.set_intent", json!({ "lane": "lane/1", "summary": "never return a negative total" }))
        .unwrap();
    assert!(intent["intent"].as_str().unwrap().len() >= 32);
    fs::write(workdir.join("src/shop.ts"), SHOP.replace("return sum;", "return Math.abs(sum);")).unwrap();
    assert_eq!(mcp.tool("kito.sync", json!({ "lane": "lane/1" })).unwrap()["ops"], 2);
    let renamed = mcp
        .tool(
            "kito.rename_symbol",
            json!({ "lane": "lane/1", "symbol": "src/shop.ts:total", "new_name": "grandTotal" }),
        )
        .unwrap();
    assert_eq!((renamed["from"].as_str(), renamed["to"].as_str()), (Some("total"), Some("grandTotal")));

    let status = mcp.tool("kito.status", json!({ "lane": "lane/1" })).unwrap();
    assert_eq!((status["ops"].as_u64(), status["files"][0].as_str()), (Some(4), Some("src/shop.ts")));
    assert_eq!(status["conflicts"], json!([]));
    let log = mcp.tool("kito.log", json!({ "lane": "lane/1", "limit": 5 })).unwrap();
    let log = log.as_array().unwrap();
    assert_eq!(log[0]["op"], "Rename");
    assert_eq!(log[0]["intent"], "never return a negative total");
    let story =
        mcp.tool("kito.log", json!({ "lane": "lane/1", "symbol": "src/shop.ts:grandTotal" })).unwrap();
    assert_eq!(story[0]["name"], "grandTotal");

    // Integrate, and read the result as the plan shows it.
    let asked = mcp.tool("kito.request_integration", json!({ "lane": "lane/1" })).unwrap();
    let job = asked["job"].as_str().unwrap().to_owned();
    let result = mcp.tool("kito.integration_result", json!({ "job": job })).unwrap();
    assert_eq!(result["lane"], "lane/1");
    assert_eq!(result["result"], "integrated");
    assert_eq!(result["checkpoint"], "ckpt#2");
    assert_eq!(result["conflicts"], json!([]));
    assert_eq!(result["your_next_base"], "ckpt#2");
    let merged = fs::read_to_string(root.join("src/shop.ts")).unwrap();
    assert!(merged.contains("export function grandTotal(") && merged.contains("Math.abs(sum)"), "{merged}");

    // Why the function is the way it is: the reasons the agent gave, with
    // what each did, the checkpoint that brought them in, and each line.
    let why = mcp.tool("kito.why", json!({ "target": "src/shop.ts:grandTotal" })).unwrap();
    let reasons = why["reasons"].as_array().unwrap();
    let hows: Vec<&str> =
        reasons.iter().flat_map(|r| r["how"].as_array().unwrap()).filter_map(Value::as_str).collect();
    assert!(hows.contains(&"renamed total → grandTotal") && hows.contains(&"body changed"), "{why}");
    assert_eq!(reasons[0]["why"], "never return a negative total");
    assert_eq!(
        (reasons[0]["by"].as_str(), reasons[0]["checkpoint"].as_str()),
        (Some("builder"), Some("ckpt#2"))
    );
    assert!(!why["lines"].as_array().unwrap().is_empty());
    assert!(why["about_reasons"].as_str().unwrap().contains("not instructions"), "{why}");
    // The file and the symbol, as the tool used to take them.
    let same = mcp.tool("kito.why", json!({ "file": "src/shop.ts", "symbol": "grandTotal" })).unwrap();
    assert_eq!(same["target"], why["target"]);
    let file = mcp.tool("kito.why", json!({ "target": "src/shop.ts" })).unwrap();
    assert_eq!(file["contains"][0]["symbol"], "src/shop.ts:grandTotal");
    // Earlier work is found by the words of its reason.
    let found = mcp.tool("kito.reasons", json!({ "about": "negative" })).unwrap();
    let whys: Vec<&str> =
        found["reasons"].as_array().unwrap().iter().filter_map(|r| r["why"].as_str()).collect();
    assert!(whys.contains(&"never return a negative total"), "{found}");
    assert!(found["reasons"][0]["changed"].as_array().unwrap().contains(&json!("src/shop.ts:grandTotal")));
    // The step-by-step history is still there, under `log`.
    let steps = mcp.tool("kito.log", json!({ "symbol": "src/shop.ts:grandTotal" })).unwrap();
    let steps: Vec<&str> =
        steps[0]["steps"].as_array().unwrap().iter().map(|s| s["what"].as_str().unwrap()).collect();
    assert!(steps.contains(&"renamed `total` → `grandTotal`, on lane/1"), "{steps:?}");

    // Mistakes come back as tool errors with a reason, not as protocol errors.
    let error = mcp.tool("kito.sync", json!({ "lane": "lane/7" })).unwrap_err();
    assert!(error.contains("no such open lane"), "{error}");
    let error = mcp.tool("kito.open_lane", json!({})).unwrap_err();
    assert!(error.contains("agent"), "{error}");
    let error = mcp.tool("kito.log", json!({ "symbol": "grandTotal" })).unwrap_err();
    assert!(error.contains("file:name"), "{error}");

    let closed = mcp.tool("kito.close_lane", json!({ "lane": "lane/1" })).unwrap();
    assert_eq!(closed["state"], "closed");
    assert!(!workdir.exists());
    drop(mcp);
    stop_daemon(&root);
}

#[test]
fn a_conflict_is_decided_through_the_tools_and_names_can_avoid_the_dot() {
    let (_tmp, root) = new_repo();
    // For clients that refuse a dot in a tool name.
    let mut mcp = Session::start(&root, &["--tool-prefix", "kito_"]);
    let listed = mcp.request("tools/list", json!({}));
    assert_eq!(listed["tools"][0]["name"], "kito_open_lane");

    let lane = mcp
        .tool(
            "kito_open_lane",
            json!({ "agent": "builder", "intent": "absolute totals", "scope": ["src/shop.ts:total"] }),
        )
        .unwrap();
    let workdir = PathBuf::from(lane["workdir"].as_str().unwrap());
    fs::write(workdir.join("src/shop.ts"), SHOP.replace("return sum;", "return Math.abs(sum);")).unwrap();
    fs::write(root.join("src/shop.ts"), SHOP.replace("return sum;", "return Math.max(sum, 0);")).unwrap();

    let job = mcp.tool("kito_request_integration", json!({ "lane": "lane/1" })).unwrap()["job"]
        .as_str()
        .unwrap()
        .to_owned();
    let result = mcp.tool("kito_integration_result", json!({ "job": job, "wait_ms": 20000 })).unwrap();
    assert_eq!(result["result"], "conflicts");
    let conflict = &result["conflicts"][0];
    assert_eq!(
        (conflict["id"].as_str(), conflict["kind"].as_str(), conflict["symbol"].as_str()),
        (Some("conflict#1"), Some("body_body"), Some("total"))
    );
    assert!(conflict["main"].as_str().unwrap().contains("Math.max(sum, 0)"));
    assert!(conflict["lane_text"].as_str().unwrap().contains("Math.abs(sum)"));
    // The status of the lane says the same.
    let status = mcp.tool("kito_status", json!({ "lane": "lane/1" })).unwrap();
    assert_eq!(
        (status["state"].as_str(), status["conflicts"][0]["id"].as_str()),
        (Some("blocked"), Some("conflict#1"))
    );

    let text = SHOP.replace("return sum;", "return Math.max(Math.abs(sum), 0);");
    let decided =
        mcp.tool("kito_resolve_conflict", json!({ "conflict": "conflict#1", "text": text })).unwrap();
    assert_eq!(decided["remaining"], 0);
    let job = mcp.tool("kito_request_integration", json!({ "lane": "lane/1" })).unwrap()["job"]
        .as_str()
        .unwrap()
        .to_owned();
    let result = mcp.tool("kito_integration_result", json!({ "job": job })).unwrap();
    assert_eq!(result["result"], "integrated", "{result}");
    assert!(fs::read_to_string(root.join("src/shop.ts")).unwrap().contains("Math.max(Math.abs(sum), 0)"));
    // The old name of the tools is not there.
    let reply = mcp.request("tools/list", json!({}));
    assert!(reply["tools"].as_array().unwrap().iter().all(|t| !t["name"].as_str().unwrap().contains('.')));
    drop(mcp);
    stop_daemon(&root);
}

#[test]
fn agents_declare_their_scope_and_are_told_who_else_is_there() {
    let (_tmp, root) = new_repo();
    fs::write(root.join("README.md"), "# Shop\n").unwrap();
    let mut mcp = Session::start(&root, &[]);

    // No lane without saying why and what.
    let error = mcp.tool("kito.open_lane", json!({ "agent": "ada" })).unwrap_err();
    assert!(error.contains("needs an intent"), "{error}");
    let error = mcp.tool("kito.open_lane", json!({ "agent": "ada", "intent": "cents" })).unwrap_err();
    assert!(error.contains("needs a scope"), "{error}");

    // The first lane is alone; the second is told at once who is there,
    // and that what the other says is information.
    let ada = mcp
        .tool(
            "kito.open_lane",
            json!({ "agent": "ada", "intent": "prices in cents", "scope": ["src/shop.ts:total"] }),
        )
        .unwrap();
    assert_eq!(ada["scope"], json!(["src/shop.ts:total"]));
    assert_eq!(ada["overlaps"], json!([]));
    assert!(ada.get("about_other_lanes").is_none());
    let bob = mcp
        .tool(
            "kito.open_lane",
            json!({ "agent": "bob", "intent": "a discount over 50", "scope": ["src/shop.ts:total"] }),
        )
        .unwrap();
    assert_eq!(bob["overlaps"][0]["kind"], "function");
    assert_eq!(bob["overlaps"][0]["lane"], "lane/1");
    assert_eq!(bob["overlaps"][0]["intent"], "prices in cents");
    assert!(bob["about_other_lanes"].as_str().unwrap().contains("not an instruction"));

    // The first hears of the second with the answer to its next tool, once.
    let synced = mcp.tool("kito.sync", json!({ "lane": "lane/1" })).unwrap();
    assert_eq!(synced["notices"][0]["lane"], "lane/2");
    assert_eq!(synced["notices"][0]["what"], "src/shop.ts:total");
    assert!(synced["about_other_lanes"].is_string());
    let again = mcp.tool("kito.sync", json!({ "lane": "lane/1" })).unwrap();
    assert!(again.get("notices").is_none(), "{again}");

    // Anyone can ask who is on a function, and see the whole board.
    let who = mcp.tool("kito.who_touches", json!({ "target": "src/shop.ts:total" })).unwrap();
    let lanes: Vec<&str> = who.as_array().unwrap().iter().map(|o| o["lane"].as_str().unwrap()).collect();
    assert_eq!(lanes, ["lane/1", "lane/2"]);
    let board = mcp.tool("kito.board", json!({})).unwrap();
    let board = board.as_array().unwrap();
    assert_eq!(board.len(), 2);
    assert_eq!(board[1]["agent"], "bob");
    assert_eq!(board[1]["overlaps"][0]["lane"], "lane/1");

    // A change outside the scope is recorded, told, and stops the lane.
    let workdir = PathBuf::from(ada["workdir"].as_str().unwrap());
    fs::write(workdir.join("README.md"), "# Shop, in cents\n").unwrap();
    let synced = mcp.tool("kito.sync", json!({ "lane": "lane/1" })).unwrap();
    assert_eq!(synced["notices"][0]["kind"], "out_of_scope");
    assert_eq!(synced["notices"][0]["what"], "README.md");
    let job = mcp.tool("kito.request_integration", json!({ "lane": "lane/1" })).unwrap()["job"]
        .as_str()
        .unwrap()
        .to_owned();
    let result = mcp.tool("kito.integration_result", json!({ "job": job, "wait_ms": 20000 })).unwrap();
    assert_eq!(result["result"], "out_of_scope", "{result}");
    assert_eq!(result["out_of_scope"], json!(["README.md"]));
    assert_eq!(fs::read_to_string(root.join("README.md")).unwrap(), "# Shop\n");

    // Declared, it goes in.
    let grown = mcp.tool("kito.extend_scope", json!({ "lane": "lane/1", "scope": ["README.md"] })).unwrap();
    assert_eq!(grown["scope"].as_array().unwrap().len(), 2);
    let job = mcp.tool("kito.request_integration", json!({ "lane": "lane/1" })).unwrap()["job"]
        .as_str()
        .unwrap()
        .to_owned();
    let result = mcp.tool("kito.integration_result", json!({ "job": job, "wait_ms": 20000 })).unwrap();
    assert_eq!(result["result"], "integrated", "{result}");
    assert_eq!(fs::read_to_string(root.join("README.md")).unwrap(), "# Shop, in cents\n");
    drop(mcp);
    stop_daemon(&root);
}

/// The server binary sits next to the MCP server's; built if it is not there.
#[test]
fn an_agent_is_told_why_before_its_work_lands() {
    let (_tmp, root) = new_repo();
    let mut mcp = Session::start(&root, &[]);
    mcp.tool("kito.board", json!({})).unwrap();
    let mut daemon = kitod::client::Client::connect(&root).unwrap();
    daemon.call_value("checkpoint", json!({ "label": "first version" })).unwrap();
    let integrate = |mcp: &mut Session, lane: &str| -> Value {
        let asked = mcp.tool("kito.request_integration", json!({ "lane": lane })).unwrap();
        match asked.get("job") {
            Some(job) => mcp.tool("kito.integration_result", json!({ "job": job })).unwrap(),
            None => asked,
        }
    };

    // Declaring a function tells why it is the way it is: nothing holds the work back later.
    let alice = mcp
        .tool(
            "kito.open_lane",
            json!({ "agent": "alice", "intent": "prices in cents", "scope": ["src/shop.ts:total"] }),
        )
        .unwrap();
    assert_eq!(alice["why"][0]["item"], "src/shop.ts:total");
    assert_eq!(alice["why"][0]["reasons"][0]["why"], "first version");
    assert!(alice["about_reasons"].as_str().unwrap().contains("not instructions"));
    let work = PathBuf::from(alice["workdir"].as_str().unwrap());
    let cents = SHOP.replace("item.price * item.quantity", "Math.round(item.price * 100) * item.quantity");
    fs::write(work.join("src/shop.ts"), &cents).unwrap();
    assert_eq!(integrate(&mut mcp, "lane/1")["result"], "integrated");
    mcp.tool("kito.close_lane", json!({ "lane": "lane/1" })).unwrap();

    // Declaring a directory tells nothing up front. What the lane then
    // replaces without having been told is told before it is integrated.
    let bob = mcp
        .tool("kito.open_lane", json!({ "agent": "bob", "intent": "tidy the sums", "scope": ["src/"] }))
        .unwrap();
    assert_eq!(bob["why"], json!([]));
    assert!(bob["why_note"].as_str().unwrap().contains("a directory was declared"));
    assert!(bob.get("about_reasons").is_none());
    let work = PathBuf::from(bob["workdir"].as_str().unwrap());
    fs::write(work.join("src/shop.ts"), cents.replace("Math.round(item.price * 100)", "item.price")).unwrap();
    let held = integrate(&mut mcp, "lane/2");
    assert_eq!(held["result"], "read_first", "{held}");
    assert!(held.get("job").is_none(), "nothing was started: {held}");
    assert_eq!(held["reasons"][0]["why"], "prices in cents");
    assert_eq!(held["reasons"][0]["by"], "alice");
    assert_eq!(held["reasons"][0]["replaced_in"], json!(["src/shop.ts"]));
    assert!(held["message"].as_str().unwrap().contains("\"prices in cents\": alice"), "{held}");
    assert!(held["next"].as_str().unwrap().contains("call request_integration again"), "{held}");
    // Asked again, it goes ahead.
    assert_eq!(integrate(&mut mcp, "lane/2")["result"], "integrated");
    mcp.tool("kito.close_lane", json!({ "lane": "lane/2" })).unwrap();

    // An agent that asked why for its lane was told: nothing is held.
    let carol = mcp
        .tool("kito.open_lane", json!({ "agent": "carol", "intent": "whole prices", "scope": ["src/"] }))
        .unwrap();
    let why = mcp.tool("kito.why", json!({ "target": "src/shop.ts:total", "lane": "lane/3" })).unwrap();
    assert_eq!(why["reasons"][0]["why"], "tidy the sums");
    let work = PathBuf::from(carol["workdir"].as_str().unwrap());
    let now = fs::read_to_string(work.join("src/shop.ts")).unwrap();
    fs::write(work.join("src/shop.ts"), now.replace("item.price *", "Math.trunc(item.price) *")).unwrap();
    assert_eq!(integrate(&mut mcp, "lane/3")["result"], "integrated");

    drop(mcp);
    stop_daemon(&root);
}

#[test]
fn an_agent_runs_its_commands_confined_to_its_lane() {
    if !kitod::sandbox::available() {
        return;
    }
    let (_tmp, root) = new_repo();
    let mut mcp = Session::start(&root, &[]);
    let lane = mcp
        .tool("kito.open_lane", json!({ "agent": "alice", "intent": "a build", "scope": ["src/"] }))
        .unwrap();
    let work = PathBuf::from(lane["workdir"].as_str().unwrap());

    // In the lane's working copy, with its output and how it ended.
    let ran = mcp
        .tool(
            "kito.run",
            json!({ "lane": "lane/1", "command": "pwd; wc -l < src/shop.ts; echo built > src/out.txt" }),
        )
        .unwrap();
    assert_eq!((ran["exit_code"].as_i64(), ran["confined"].as_bool()), (Some(0), Some(true)), "{ran}");
    assert!(ran["output"].as_str().unwrap().starts_with(&format!("{}\n7", work.display())), "{ran}");
    assert_eq!(fs::read_to_string(work.join("src/out.txt")).unwrap(), "built\n");

    // Not outside it; a failure is the command's, told with what it said.
    let script = format!("echo x > {}/hacked.txt", root.display());
    let tried = mcp.tool("kito.run", json!({ "lane": "lane/1", "command": script })).unwrap();
    assert_ne!(tried["exit_code"], 0);
    assert!(tried["output"].as_str().unwrap().contains("Permission denied"), "{tried}");
    assert!(!root.join("hacked.txt").exists());

    // One that does not end is stopped.
    let slow =
        mcp.tool("kito.run", json!({ "lane": "lane/1", "command": "sleep 30", "timeout_s": 1 })).unwrap();
    assert_eq!(slow["exit_code"], Value::Null);
    assert!(slow["output"].as_str().unwrap().contains("still running after 1 s"), "{slow}");

    drop(mcp);
    stop_daemon(&root);
}

#[test]
fn an_agent_runs_a_command_confined_to_its_lane() {
    let (_tmp, root) = new_repo();
    let mut mcp = Session::start(&root, &[]);
    let lane = mcp
        .tool("kito.open_lane", json!({ "agent": "builder", "intent": "a build", "scope": ["src/"] }))
        .unwrap();
    let work = PathBuf::from(lane["workdir"].as_str().unwrap());
    let ran = mcp.tool(
        "kito.run",
        json!({ "lane": "lane/1", "command": "ls src && echo built > src/out.txt && echo ok" }),
    );
    let ran = match ran {
        Ok(ran) => ran,
        // A system that cannot confine a command says so, and runs nothing.
        Err(why) => {
            assert!(why.contains("cannot be confined here"), "{why}");
            drop(mcp);
            return stop_daemon(&root);
        }
    };
    assert_eq!((ran["exit_code"].as_i64(), ran["confined"].as_bool()), (Some(0), Some(true)), "{ran}");
    assert_eq!(ran["output"], "shop.ts\nok");
    assert_eq!(fs::read_to_string(work.join("src/out.txt")).unwrap(), "built\n");

    // Outside the lane nothing is written, and the command says how it ended.
    let script = format!("echo x > {}/hacked.txt", root.display());
    let tried = mcp.tool("kito.run", json!({ "lane": "lane/1", "command": script })).unwrap();
    assert_ne!(tried["exit_code"], 0, "{tried}");
    assert!(tried["output"].as_str().unwrap().contains("Permission denied"), "{tried}");
    assert!(!root.join("hacked.txt").exists());

    // A command that takes too long is stopped.
    let slow =
        mcp.tool("kito.run", json!({ "lane": "lane/1", "command": "sleep 30", "timeout_s": 1 })).unwrap();
    assert_eq!(slow["exit_code"], Value::Null, "{slow}");
    assert!(slow["output"].as_str().unwrap().contains("still running after 1 s"), "{slow}");

    // What the command wrote is a save of the lane like any other.
    let status = mcp.tool("kito.status", json!({ "lane": "lane/1" })).unwrap();
    assert!(status["files"].as_array().unwrap().contains(&json!("src/out.txt")), "{status}");
    drop(mcp);
    stop_daemon(&root);
}

fn kito_server() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    let path = Path::new(env!("CARGO_BIN_EXE_kito-mcp"))
        .with_file_name(format!("kito-server{}", std::env::consts::EXE_SUFFIX));
    BUILD.call_once(|| {
        if !path.is_file() {
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "kito-server", "--bin", "kito-server"])
                .status()
                .unwrap();
            assert!(status.success(), "building kito-server");
        }
    });
    path
}

#[test]
fn the_tools_reach_a_repository_kept_by_a_server() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let serverdir = base.join("server");
    let run = |args: &[&str]| -> String {
        let out = Command::new(kito_server())
            .arg("--dir")
            .arg(&serverdir)
            .args(args)
            .env("KITOD_BIN", kitod())
            .env("KITO_AUTHOR", "the server")
            .env("KITO_CONFIG_DIR", base.join(".absent-personal-config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "kito-server {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    // A git repository for the server to host.
    let src = base.join("src-repo");
    fs::create_dir_all(src.join("src")).unwrap();
    fs::write(src.join("src/shop.ts"), SHOP).unwrap();
    for args in [&["init", "-q", "-b", "main"][..], &["add", "-A"], &["commit", "-q", "-m", "start"]] {
        let done = Command::new("git")
            .current_dir(&src)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Ada")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .status()
            .unwrap();
        assert!(done.success(), "git {args:?}");
    }
    run(&["init", serverdir.to_str().unwrap()]);
    run(&["repo", "add", "shop", "--from", src.to_str().unwrap()]);
    let token = run(&["user", "add", "ada"]);
    let token = token.lines().find(|line| line.starts_with("kito_")).unwrap().trim().to_owned();

    /// The server, stopped when the test ends, however it ends.
    struct Serving(Child);
    impl Drop for Serving {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut serving = Serving(
        Command::new(kito_server())
            .arg("--dir")
            .arg(&serverdir)
            .args(["serve", "--listen", "127.0.0.1:0"])
            .env("KITOD_BIN", kitod())
            .env("KITO_AUTHOR", "the server")
            .env("KITO_CONFIG_DIR", base.join(".absent-personal-config"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(serving.0.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let address = line.trim().strip_prefix("kito-server: listening on ").unwrap().split(',').next().unwrap();

    // A directory connected to the repository: the MCP server started in it
    // speaks to the server, as the person the token belongs to.
    let here = base.join("connected");
    fs::create_dir(&here).unwrap();
    let remote = kitod::client::Remote {
        server: Some(address.to_owned()),
        command: Vec::new(),
        repo: "shop".to_owned(),
        token: Some(token),
        ..Default::default()
    };
    remote.save(&here).unwrap();
    let mut mcp = Session::start(&here, &[]);
    assert_eq!(mcp.tool("kito.board", json!({})).unwrap(), json!([]));
    let lane = mcp
        .tool(
            "kito.open_lane",
            json!({ "agent": "builder", "intent": "prices in cents", "scope": ["src/shop.ts:total"] }),
        )
        .unwrap();
    assert_eq!((lane["id"].as_str(), lane["owner"].as_str()), (Some("lane/1"), Some("ada")));
    let who = mcp.tool("kito.who_touches", json!({ "target": "src/shop.ts:total" })).unwrap();
    assert_eq!(who[0]["agent"], "builder");

    // The working copy the agent is told is on its own machine: it edits
    // there, and the lane on the server has it when it asks to integrate.
    let work = PathBuf::from(lane["workdir"].as_str().unwrap());
    assert!(work.starts_with(&here), "{}", work.display());
    let text = fs::read_to_string(work.join("src/shop.ts")).unwrap();
    assert_eq!(text, SHOP);
    let first = SHOP.lines().find(|line| line.contains("return")).unwrap();
    fs::write(work.join("src/shop.ts"), SHOP.replacen(first, &format!("{first} // in cents"), 1)).unwrap();
    let asked = mcp.tool("kito.request_integration", json!({ "lane": "lane/1" })).unwrap();
    let job = asked["job"].as_str().unwrap().to_owned();
    let result = mcp.tool("kito.integration_result", json!({ "job": job, "wait_ms": 20000 })).unwrap();
    assert_eq!(result["result"], "integrated", "{result}");
    let main = fs::read_to_string(serverdir.join("repos/shop/src/shop.ts")).unwrap();
    assert!(main.contains("// in cents"), "{main}");
    let status = mcp.tool("kito.status", json!({ "lane": "lane/1" })).unwrap();
    assert_eq!(status["workdir"], json!(work));

    let closed = mcp.tool("kito.close_lane", json!({ "lane": "lane/1" })).unwrap();
    assert_eq!(closed["state"], "closed");
    assert!(!work.exists());
    drop(mcp);
    stop_daemon(&here);
    drop(serving);
    stop_daemon(&serverdir.join("repos/shop"));
}
