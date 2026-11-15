//! The swarm simulator: scripted agents on one repository, through the
//! real daemon.
//!
//! Every agent opens a lane and, round after round, edits its working copy
//! and asks for integration, all at the same time as the others. The script
//! is built so that agents never want different things for the same symbol
//! and yet keep touching the same files, the same places of those files
//! and the same class: a function each in one shared file, a method and a
//! function added at the same spot of another, explicit renames, a function
//! moved to a new file, one removed. A line-based merge would stop on most
//! rounds; here every conflict on that part of the script is a fake one,
//! and is counted as a failure.
//!
//! A second part stages the conflicts that are real (two different renames
//! of one function, a removal against a change, the same line changed
//! twice) and checks that each is found, of the right kind, and settled by
//! a decision.

use crate::client::Client;
use kito_integrate::{IntegrationResult, JobState, Outcome};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What to simulate.
#[derive(Clone, Debug)]
pub struct Plan {
    pub agents: usize,
    pub rounds: usize,
    /// Stage the real conflicts too (needs at least 6 agents).
    pub real_conflicts: bool,
    /// Where to keep each lane's working copy as it is when it asks for an
    /// integration (`agent-<k>/round-<n>`): what another merge tool can be
    /// given to do the same work.
    pub snapshots: Option<PathBuf>,
}

/// One integration of the part of the script that has no real conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub agent: usize,
    pub round: usize,
    /// The git commit the integration was exported as.
    pub git_commit: Option<String>,
}

/// What happened.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub agents: usize,
    pub rounds: usize,
    /// Integrations asked on the part of the script with no real conflict.
    pub integrations: usize,
    pub integrated: usize,
    /// Conflicts found where the script has none.
    pub fake_conflicts: Vec<String>,
    /// Real conflicts staged, and the kinds found for them.
    pub real_conflicts_staged: usize,
    pub real_conflicts_found: Vec<String>,
    pub verification_failures: usize,
    /// Symbols both sides changed that merged by themselves.
    pub merged_by_themselves: usize,
    pub lane_open_ms: Vec<f64>,
    /// From the request to the result, queueing and verification included.
    pub integration_ms: Vec<f64>,
    pub ops_on_main: u64,
    pub ops_on_lanes: u64,
    pub checkpoints: u64,
    /// Anything that is not as the script says it must be.
    pub problems: Vec<String>,
    /// The integrations of the rounds, by agent and round.
    pub records: Vec<Record>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty() && self.fake_conflicts.is_empty() && self.integrated == self.integrations
    }

    /// The report as a Markdown table.
    pub fn to_markdown(&self) -> String {
        let stat = |samples: &[f64]| {
            let mut sorted = samples.to_vec();
            sorted.sort_by(|a, b| a.total_cmp(b));
            match sorted.len() {
                0 => "—".to_owned(),
                n => format!("median {:.0} ms, max {:.0} ms", sorted[n / 2], sorted[n - 1]),
            }
        };
        let mut out = String::from("| Measure | Value |\n| --- | --- |\n");
        let mut row = |name: &str, value: String| out.push_str(&format!("| {name} | {value} |\n"));
        row("Agents, each in its own lane", self.agents.to_string());
        row("Rounds of edits per agent", self.rounds.to_string());
        row("Integrations asked", self.integrations.to_string());
        row("Integrated", self.integrated.to_string());
        row("Fake conflicts (changes to different symbols)", self.fake_conflicts.len().to_string());
        row("Symbols changed on both sides and merged by themselves", self.merged_by_themselves.to_string());
        row(
            "Real conflicts staged / found and settled",
            format!("{} / {}", self.real_conflicts_staged, self.real_conflicts_found.len()),
        );
        row("Kinds of the real conflicts", self.real_conflicts_found.join(", "));
        row("Verification failures", self.verification_failures.to_string());
        row("Opening a lane", stat(&self.lane_open_ms));
        row("One integration, queue and verification included", stat(&self.integration_ms));
        row("Ops recorded on the lanes", self.ops_on_lanes.to_string());
        row("Ops on the main lane", self.ops_on_main.to_string());
        row("Checkpoints (one per integration, each a git commit)", self.checkpoints.to_string());
        row("Problems", if self.problems.is_empty() { "none".to_owned() } else { self.problems.join("; ") });
        out
    }
}

/// The files of the TypeScript project the agents work on.
pub fn project(agents: usize) -> Vec<(String, String)> {
    let mut files = vec![
        (
            "tsconfig.json".to_owned(),
            "{\n  \"compilerOptions\": {\n    \"strict\": true,\n    \"noEmit\": true,\n    \"target\": \"es2020\",\n    \"module\": \"commonjs\"\n  },\n  \"include\": [\"src\"]\n}\n".to_owned(),
        ),
        (
            "src/types.ts".to_owned(),
            "export interface Item {\n  id: string;\n  name: string;\n  price: number;\n  quantity: number;\n}\n".to_owned(),
        ),
        (
            "src/shared/registry.ts".to_owned(),
            "import { Item } from \"../types\";\n\nexport class Registry {\n  private items: Item[] = [];\n\n  size(): number {\n    return this.items.length;\n  }\n}\n\nexport function emptyRegistry(): Registry {\n  return new Registry();\n}\n".to_owned(),
        ),
        (
            "src/shared/contested.ts".to_owned(),
            "export function contested(value: number): number {\n  const doubled = value * 2;\n  return doubled + 1;\n}\n\nexport function doomed(value: number): number {\n  const half = value / 2;\n  return Math.floor(half);\n}\n\nexport function crowded(value: number): number {\n  const base = value + 1;\n  return base * 3;\n}\n".to_owned(),
        ),
    ];
    // One function per agent, all in one file, one after the other.
    let math: Vec<String> = (0..agents)
        .map(|k| {
            format!(
                "export function op{k}(value: number): number {{\n  const scale = {};\n  const bias = 0;\n  return value * scale + bias;\n}}\n",
                k + 2
            )
        })
        .collect();
    files.push(("src/shared/math.ts".to_owned(), math.join("\n")));
    for k in 0..agents {
        files.push((
            format!("src/agents/agent{k}.ts"),
            format!(
                "import {{ Item }} from \"../types\";\n\nexport function work{k}_0(items: Item[]): number {{\n  let sum = 0;\n  for (const item of items) {{\n    sum += item.price * item.quantity + {k};\n  }}\n  return sum;\n}}\n\nexport function work{k}_1(items: Item[]): string {{\n  const names = items.map((item) => item.name + \"-{k}\");\n  return names.join(\", \");\n}}\n\nexport function work{k}_2(item: Item): boolean {{\n  const limit = {k} + 10;\n  return item.quantity > limit;\n}}\n\nexport function work{k}_3(item: Item): string {{\n  const tag = \"r0\";\n  return item.id + tag;\n}}\n"
            ),
        ));
    }
    files
}

fn read(dir: &Path, path: &str) -> Result<String, String> {
    fs::read_to_string(dir.join(path)).map_err(|e| format!("{path}: {e}"))
}

fn write(dir: &Path, path: &str, content: &str) -> Result<(), String> {
    let full = dir.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{path}: {e}"))?;
    }
    fs::write(&full, content).map_err(|e| format!("{path}: {e}"))
}

/// Replaces what is between `before` and `after` in the block that starts at `anchor`.
fn replace_in(text: &str, anchor: &str, before: &str, after: &str, with: &str) -> Result<String, String> {
    let start = text.find(anchor).ok_or_else(|| format!("{anchor:?} is not there"))?;
    let from = start
        + text[start..].find(before).ok_or_else(|| format!("{before:?} after {anchor:?}"))?
        + before.len();
    let to = from + text[from..].find(after).ok_or_else(|| format!("{after:?} after {anchor:?}"))?;
    Ok(format!("{}{with}{}", &text[..from], &text[to..]))
}

/// The text of the function that starts at `anchor`, up to its closing brace.
fn function_range(text: &str, anchor: &str) -> Result<std::ops::Range<usize>, String> {
    let start = text.find(anchor).ok_or_else(|| format!("{anchor:?} is not there"))?;
    let end = start + text[start..].find("\n}\n").ok_or_else(|| format!("end of {anchor:?}"))? + 3;
    Ok(start..end)
}

struct Agent {
    name: String,
    lane: String,
    dir: PathBuf,
    client: Client,
}

impl Agent {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.client
            .call_value(method, params)
            .map_err(|e| format!("{} ({}): {method}: {e}", self.name, self.lane))
    }

    /// Asks for integration and waits for the result.
    fn integrate(&mut self) -> Result<(IntegrationResult, Duration), String> {
        let started = Instant::now();
        let lane = self.lane.clone();
        let asked = self.call("request_integration", json!({ "lane": lane }))?;
        let job = asked["job"].as_str().unwrap_or_default().to_owned();
        loop {
            let state = self.call("integration_result", json!({ "job": job, "wait_ms": 2000 }))?;
            let state: JobState = serde_json::from_value(state).map_err(|e| e.to_string())?;
            if let JobState::Done { result } = state {
                return Ok((*result, started.elapsed()));
            }
            if started.elapsed() > Duration::from_secs(600) {
                return Err(format!("{job} did not finish"));
            }
        }
    }

    /// The edits of one round. Returns what must be true of the main lane
    /// once they are integrated: (file, text, must be there).
    fn round(&mut self, k: usize, round: usize) -> Result<Vec<(String, String, bool)>, String> {
        let dir = self.dir.clone();
        let mut expect = Vec::new();

        // 1. Its own function in the file all agents share.
        let math = read(&dir, "src/shared/math.ts")?;
        let bias = 1000 * (k + 1) + round;
        let math =
            replace_in(&math, &format!("export function op{k}("), "const bias = ", ";", &bias.to_string())?;
        write(&dir, "src/shared/math.ts", &math)?;
        expect.push(("src/shared/math.ts".to_owned(), format!("const bias = {bias};"), true));
        if round > 0 {
            expect.push(("src/shared/math.ts".to_owned(), format!("const bias = {};", bias - 1), false));
        }

        // 2. A method at the end of the shared class and a function at the
        //    end of its file: where every other agent adds its own.
        let registry = read(&dir, "src/shared/registry.ts")?;
        let method = format!(
            "\n  m{k}_{round}(): number {{\n    return this.items.length + {};\n  }}\n",
            k * 100 + round
        );
        let close = "}\n\nexport function emptyRegistry";
        let at = registry.find(close).ok_or("the end of the class Registry is not where it was")?;
        let function = format!(
            "\nexport function f{k}_{round}(registry: Registry): number {{\n  return registry.size() + {};\n}}\n",
            k * 100 + round
        );
        let registry = format!("{}{method}{}{function}", &registry[..at], &registry[at..]);
        write(&dir, "src/shared/registry.ts", &registry)?;
        expect.push(("src/shared/registry.ts".to_owned(), format!("m{k}_{round}(): number"), true));
        expect.push(("src/shared/registry.ts".to_owned(), format!("export function f{k}_{round}("), true));

        // 3. Its own module: an edit every round, and once each a rename, a
        //    move to a new file and a removal.
        let module = format!("src/agents/agent{k}.ts");
        let text = read(&dir, &module)?;
        let text = replace_in(
            &text,
            &format!("export function work{k}_3("),
            "const tag = \"",
            "\";",
            &format!("r{round}"),
        )?;
        write(&dir, &module, &text)?;
        expect.push((module.clone(), format!("const tag = \"r{round}\";"), true));
        if round > 0 {
            expect.push((module.clone(), format!("const tag = \"r{}\";", round - 1), false));
        }
        match round {
            1 => {
                // An explicit rename: asked, not guessed.
                let lane = self.lane.clone();
                self.call("sync", json!({ "lane": lane }))?;
                self.call(
                    "rename_symbol",
                    json!({ "lane": lane, "symbol": format!("{module}:work{k}_0"), "new_name": format!("total{k}") }),
                )?;
                expect.push((module.clone(), format!("export function total{k}("), true));
                expect.push((module.clone(), format!("work{k}_0"), false));
            }
            2 => {
                let text = read(&dir, &module)?;
                let range = function_range(&text, &format!("export function work{k}_1("))?;
                let function = text[range.clone()].to_owned();
                let rest = format!("{}{}", &text[..range.start], text[range.end..].trim_start_matches('\n'));
                write(&dir, &module, &rest)?;
                let extra = format!("src/agents/extra{k}.ts");
                write(&dir, &extra, &format!("import {{ Item }} from \"../types\";\n\n{function}"))?;
                expect.push((extra, format!("export function work{k}_1("), true));
                expect.push((module.clone(), format!("work{k}_1"), false));
            }
            3 => {
                let text = read(&dir, &module)?;
                let range = function_range(&text, &format!("export function work{k}_2("))?;
                let rest = format!("{}{}", &text[..range.start], text[range.end..].trim_start_matches('\n'));
                write(&dir, &module, &rest)?;
                expect.push((module.clone(), format!("work{k}_2"), false));
            }
            _ => {}
        }
        Ok(expect)
    }
}

/// An edit of a text.
type Edit<'a> = &'a dyn Fn(&str) -> Result<String, String>;

/// Edits `src/shared/contested.ts` in an agent's working copy.
fn contested(agent: &Agent, edit: impl FnOnce(&str) -> Result<String, String>) -> Result<(), String> {
    let text = read(&agent.dir, "src/shared/contested.ts")?;
    write(&agent.dir, "src/shared/contested.ts", &edit(&text)?)
}

/// Copies the files under `from` to `to`, directories included.
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    for entry in fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        match entry.file_type().map_err(|e| e.to_string())? {
            kind if kind.is_dir() => copy_tree(&source, &target)?,
            kind if kind.is_file() => {
                fs::copy(&source, &target).map_err(|e| format!("{}: {e}", source.display()))?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Runs the simulation on the repository in `root`, whose daemon is running
/// and whose main lane holds [`project`]`(plan.agents)`.
pub fn run(root: &Path, plan: &Plan) -> Result<Report, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let mut report = Report { agents: plan.agents, rounds: plan.rounds, ..Report::default() };

    // Every agent opens its lane.
    let mut agents = Vec::new();
    for k in 0..plan.agents {
        let mut client = Client::connect(&root).map_err(|e| e.to_string())?;
        let started = Instant::now();
        let lane = client
            .call_value("open_lane", json!({ "agent": format!("agent-{k}"), "task": format!("task #{k}") }))
            .map_err(|e| e.to_string())?;
        report.lane_open_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        agents.push(Agent {
            name: format!("agent-{k}"),
            lane: lane["id"].as_str().unwrap_or_default().to_owned(),
            dir: PathBuf::from(lane["workdir"].as_str().unwrap_or_default()),
            client,
        });
    }

    // The rounds: all agents at once, each on its own thread.
    type Expectations = BTreeMap<(String, String), bool>;
    type Rounds = (Agent, Vec<(IntegrationResult, Duration)>, Expectations);
    let outcomes: Vec<Result<Rounds, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = agents
            .into_iter()
            .enumerate()
            .map(|(k, mut agent)| {
                let rounds = plan.rounds;
                let snapshots = plan.snapshots.clone();
                scope.spawn(move || {
                    let mut results = Vec::new();
                    // The latest word on each (file, text) wins: what a round adds a later one may remove.
                    let mut expect: Expectations = BTreeMap::new();
                    for round in 0..rounds {
                        let lane = agent.lane.clone();
                        agent.call(
                            "set_intent",
                            json!({ "lane": lane, "summary": format!("round {round} of agent {k}") }),
                        )?;
                        for (file, text, there) in agent.round(k, round)? {
                            expect.insert((file, text), there);
                        }
                        if let Some(snapshots) = &snapshots {
                            copy_tree(&agent.dir, &snapshots.join(format!("agent-{k}/round-{round}")))?;
                        }
                        results.push(agent.integrate()?);
                    }
                    Ok((agent, results, expect))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("an agent panicked".to_owned())))
            .collect()
    });
    let mut agents = Vec::new();
    let mut expect: Expectations = BTreeMap::new();
    for (k, outcome) in outcomes.into_iter().enumerate() {
        let (agent, results, expected) = outcome?;
        for (round, (result, took)) in results.into_iter().enumerate() {
            report.records.push(Record { agent: k, round, git_commit: result.git_commit.clone() });
            report.integrations += 1;
            report.integration_ms.push(took.as_secs_f64() * 1000.0);
            report.merged_by_themselves += result.auto_resolutions.len();
            match result.result {
                Outcome::Integrated => report.integrated += 1,
                Outcome::Conflicts => report.fake_conflicts.extend(result.conflicts.iter().map(|c| {
                    format!(
                        "{} {} {} in {}",
                        agent.lane,
                        c.kind,
                        c.symbol.clone().unwrap_or_default(),
                        c.file.clone().unwrap_or_default()
                    )
                })),
                Outcome::VerificationFailed => {
                    report.verification_failures += 1;
                    report.problems.push(format!(
                        "{}: verification failed: {}",
                        agent.lane,
                        result.verification.and_then(|v| v.output).unwrap_or_default()
                    ));
                }
                other => report.problems.push(format!("{}: {other:?}: {:?}", agent.lane, result.message)),
            }
        }
        expect.extend(expected);
        agents.push(agent);
    }

    // The real conflicts, one pair of agents each.
    if plan.real_conflicts && agents.len() >= 6 {
        report.real_conflicts_staged = 3;
        let mut stage = |first: usize,
                         second: usize,
                         kind: &str,
                         edits: [Edit<'_>; 2],
                         decision: Value|
         -> Result<(), String> {
            contested(&agents[first], edits[0])?;
            contested(&agents[second], edits[1])?;
            let (went_in, _) = agents[first].integrate()?;
            if went_in.result != Outcome::Integrated {
                return Err(format!("{kind}: the first side did not integrate: {went_in:?}"));
            }
            let (blocked, _) = agents[second].integrate()?;
            let found: Vec<String> = blocked.conflicts.iter().map(|c| c.kind.clone()).collect();
            if blocked.result != Outcome::Conflicts || found != [kind] {
                return Err(format!(
                    "{kind}: expected exactly that conflict, found {found:?} ({:?})",
                    blocked.result
                ));
            }
            let mut params = decision.clone();
            params["conflict"] = json!(blocked.conflicts[0].id);
            agents[second].call("resolve_conflict", params)?;
            let (settled, _) = agents[second].integrate()?;
            if !matches!(settled.result, Outcome::Integrated | Outcome::NothingToIntegrate) {
                return Err(format!("{kind}: not settled by the decision: {settled:?}"));
            }
            report.real_conflicts_found.push(kind.to_owned());
            Ok(())
        };
        // Two names for the same function: the main lane's stays.
        stage(
            0,
            1,
            "rename_rename",
            [&|text| Ok(text.replace("contested", "firstName")), &|text| {
                Ok(text.replace("contested", "secondName"))
            }],
            json!({ "choice": "main" }),
        )?;
        // Removed by one, changed by the other: the change is kept.
        stage(
            2,
            3,
            "remove_modify",
            [
                &|text| {
                    let range = function_range(text, "export function doomed(")?;
                    Ok(format!("{}{}", &text[..range.start], text[range.end..].trim_start_matches('\n')))
                },
                &|text| Ok(text.replace("return Math.floor(half);", "return Math.ceil(half);")),
            ],
            json!({ "choice": "lane" }),
        )?;
        // The same line, two ways: a third text settles it.
        stage(
            4,
            5,
            "body_body",
            [&|text| Ok(text.replace("return base * 3;", "return base * 4;")), &|text| {
                Ok(text.replace("return base * 3;", "return base * 5;"))
            }],
            json!({ "text": "export function crowded(value: number): number {\n  const base = value + 1;\n  return base * 20;\n}\n" }),
        )?;
        for (text, there) in [
            ("export function firstName(", true),
            ("secondName", false),
            ("return Math.ceil(half);", true),
            ("return base * 20;", true),
        ] {
            expect.insert(("src/shared/contested.ts".to_owned(), text.to_owned()), there);
        }
    }

    // What the main lane must look like now.
    let mut client = Client::connect(&root).map_err(|e| e.to_string())?;
    client.call_value("sync", json!({})).map_err(|e| e.to_string())?;
    for ((file, text), there) in &expect {
        let content = fs::read_to_string(root.join(file)).unwrap_or_default();
        let count = content.matches(text.as_str()).count();
        match (there, count) {
            (true, 1) | (false, 0) => {}
            (true, n) => report.problems.push(format!("{file}: {text:?} is there {n} times instead of once")),
            (false, n) => {
                report.problems.push(format!("{file}: {text:?} should be gone, is there {n} times"))
            }
        }
    }
    let status = client.call_value("status", json!({})).map_err(|e| e.to_string())?;
    report.ops_on_main = status["repo"]["ops"].as_u64().unwrap_or(0);
    report.checkpoints = status["repo"]["checkpoints"].as_u64().unwrap_or(0);
    let lanes = client.call_value("lanes", json!({ "all": true })).map_err(|e| e.to_string())?;
    if lanes.as_array().map_or(0, Vec::len) != plan.agents {
        report.problems.push(format!(
            "{} lanes instead of {}",
            lanes.as_array().map_or(0, Vec::len),
            plan.agents
        ));
    }
    for agent in &mut agents {
        // Every lane ends where the main lane is, or behind it: never ahead.
        let lane = agent.lane.clone();
        let log = agent.call("log", json!({ "lane": lane, "limit": 100_000 }))?;
        report.ops_on_lanes += log.as_array().map_or(0, |items| items.len() as u64);
        let closed = agent.call("close_lane", json!({ "lane": lane }))?;
        if closed["state"] != "closed" {
            report.problems.push(format!("{} did not close", agent.lane));
        }
    }
    let open = client.call_value("conflicts", json!({})).map_err(|e| e.to_string())?;
    if open.as_array().is_some_and(|list| !list.is_empty()) {
        report.problems.push(format!("conflicts left open: {open}"));
    }
    Ok(report)
}
