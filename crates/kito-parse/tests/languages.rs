//! Symbol extraction in each of the ten languages.

use kito_parse::{parse, Dialect, ParsedFile};

/// One line per symbol: `Kind qualified.name`.
fn outline(file: &ParsedFile) -> Vec<String> {
    (0..file.symbols.len())
        .map(|i| format!("{:?} {}", file.symbols[i].kind, file.qualified_name(i)))
        .collect()
}

fn check(dialect: Dialect, source: &str, expected: &[&str]) {
    let file = parse(dialect, source.as_bytes()).expect("the parser returns a tree");
    assert!(!file.has_errors, "{dialect:?}: the sample has syntax errors");
    let got = outline(&file);
    if std::env::var_os("KITO_PRINT_OUTLINE").is_some() {
        println!(
            "== {dialect:?}\n{}",
            got.iter().map(|l| format!("        {l:?},")).collect::<Vec<_>>().join("\n")
        );
    }
    assert_eq!(got, expected, "{dialect:?}");
    // Every symbol has a non-empty name and a range inside the file.
    for s in &file.symbols {
        assert!(!s.name.is_empty() && s.range.end <= source.len() && s.line >= 1);
    }
}

const RUST: &str = r#"
use std::fmt;

pub const LIMIT: usize = 10;
static mut COUNTER: u32 = 0;
type Result<T> = std::result::Result<T, Error>;

/// A server.
pub struct Server { port: u16 }

pub enum State { On, Off }

pub trait Start {
    fn start(&mut self) -> Result<()>;
    fn stop(&mut self) {}
}

impl Server {
    pub fn new(port: u16) -> Self { Server { port } }
}

impl Start for Server {
    fn start(&mut self) -> Result<()> { Ok(()) }
}

mod util {
    pub fn helper(x: i32) -> i32 { x + 1 }
}

fn main() { let s = Server::new(80); }

macro_rules! square { ($x:expr) => { $x * $x }; }
"#;

const PYTHON: &str = r#"
import os

LIMIT = 10

def top(a, b=2) -> int:
    def inner(x):
        return x
    return inner(a) + b

class Server(Base):
    """A server."""
    port = 80

    def __init__(self, port):
        self.port = port

    @property
    def url(self) -> str:
        return f"http://localhost:{self.port}"

@decorator
def decorated():
    pass

@dataclass
class Point:
    x: int = 0
"#;

const JAVASCRIPT: &str = r#"
import fs from "fs";

const LIMIT = 10;
export const handler = async (req, res) => { res.end(); };
const legacy = function (x) { return x; };

function top(a, b) { return a + b; }
function* gen() { yield 1; }

export class Server extends Base {
  constructor(port) { super(); this.port = port; }
  start() { return this.port; }
  static create() { return new Server(80); }
  get url() { return "x"; }
}

export default function main() {}
"#;

const TYPESCRIPT: &str = r#"
import { db } from "./db";

export const LIMIT: number = 10;
export const fetchUser = async (id: string): Promise<User> => db.find(id);

export interface User { id: string; name: string }
export type Id = string | number;
export enum Color { Red, Green }

export function getUser(id: string): User { return db.find(id); }
declare function ambient(x: number): void;

export abstract class Repo<T> {
  constructor(private items: T[]) {}
  abstract find(id: Id): T;
  all(): T[] { return this.items; }
}

namespace Util {
  export function helper(x: number) { return x + 1; }
}
"#;

const TSX: &str = r#"
import React from "react";

export function App(props: { name: string }) {
  return <div className="app">{props.name}</div>;
}

export const Button = ({ label }: { label: string }) => <button>{label}</button>;
"#;

const JAVA: &str = r#"
package demo;

import java.util.List;

public class Server implements Runnable {
    private int port;

    public Server(int port) { this.port = port; }

    @Override
    public void run() { System.out.println(port); }

    static int add(int a, int b) { return a + b; }

    interface Listener { void onStart(Server s); }

    enum State { ON, OFF }
}

record Point(int x, int y) {
    int sum() { return x + y; }
}

@interface Marker {}
"#;

const CSHARP: &str = r#"
using System;

namespace Demo.App
{
    public class Server : IDisposable
    {
        private int port;
        public Server(int port) { this.port = port; }
        public void Start() { Console.WriteLine(port); }
        public int Port => port;
        public void Dispose() { }
        int Twice(int x) { int Local(int y) => y * 2; return Local(x); }
    }

    public interface IStart { void Start(); }
    public struct Point { public int X; }
    public enum State { On, Off }
    public record User(string Name);
}
"#;

const C: &str = r#"
#include <stdio.h>

#define LIMIT 10
#define SQUARE(x) ((x) * (x))

struct point { int x; int y; };
enum state { ON, OFF };
typedef struct node { struct node *next; } node_t;
typedef unsigned long id_t;

static int add(int a, int b) { return a + b; }
char *name(void) { return "x"; }
int main(int argc, char **argv) { return add(1, 2); }
"#;

const CPP: &str = r#"
#include <string>

namespace demo {

class Server {
public:
    Server(int port) : port_(port) {}
    ~Server() {}
    int port() const { return port_; }
    void start();
private:
    int port_;
};

void Server::start() {}

struct Point { int x; };
enum class State { On, Off };
using Id = unsigned long;

template <typename T>
T twice(T x) { return x + x; }

int *make() { return nullptr; }

}  // namespace demo
"#;

const GO: &str = r#"
package demo

import "fmt"

const Limit = 10

var counter int

type Server struct {
	port int
}

type Starter interface {
	Start() error
}

type ID = string
type Name string

type (
	A struct{}
	B struct{}
)

func NewServer(port int) *Server { return &Server{port: port} }

func (s *Server) Start() error {
	fmt.Println(s.port)
	return nil
}

func (s Server) Port() int { return s.port }
"#;

const PHP: &str = r#"<?php
namespace Demo;

const LIMIT = 10;

function top(int $a, int $b = 2): int { return $a + $b; }

interface Starter { public function start(): void; }

trait Logs { public function log(string $m): void { echo $m; } }

enum State { case On; case Off; }

class Server implements Starter {
    use Logs;
    const PORT = 80;
    private int $port;
    public function __construct(int $port) { $this->port = $port; }
    public function start(): void { echo $this->port; }
    public static function create(): self { return new self(80); }
}
"#;

#[test]
fn rust() {
    check(
        Dialect::Rust,
        RUST,
        &[
            "Const LIMIT",
            "Var COUNTER",
            "TypeAlias Result",
            "Struct Server",
            "Enum State",
            "Interface Start",
            "Method Start.start",
            "Method Start.stop",
            "Class Server",
            "Method Server.new",
            "Class Start for Server",
            "Method Start for Server.start",
            "Module util",
            "Function util.helper",
            "Function main",
            "Function square",
        ],
    );
}

#[test]
fn python() {
    check(
        Dialect::Python,
        PYTHON,
        &[
            "Var LIMIT",
            "Function top",
            "Function top.inner",
            "Class Server",
            "Method Server.__init__",
            "Method Server.url",
            "Function decorated",
            "Class Point",
        ],
    );
}

#[test]
fn javascript() {
    check(
        Dialect::JavaScript,
        JAVASCRIPT,
        &[
            "Const LIMIT",
            "Function handler",
            "Function legacy",
            "Function top",
            "Function gen",
            "Class Server",
            "Method Server.constructor",
            "Method Server.start",
            "Method Server.create",
            "Method Server.url",
            "Function main",
        ],
    );
}

#[test]
fn typescript() {
    check(
        Dialect::TypeScript,
        TYPESCRIPT,
        &[
            "Const LIMIT",
            "Function fetchUser",
            "Interface User",
            "TypeAlias Id",
            "Enum Color",
            "Function getUser",
            "Function ambient",
            "Class Repo",
            "Method Repo.constructor",
            "Method Repo.find",
            "Method Repo.all",
            "Module Util",
            "Function Util.helper",
        ],
    );
}

#[test]
fn tsx() {
    check(Dialect::Tsx, TSX, &["Function App", "Function Button"]);
}

#[test]
fn java() {
    check(
        Dialect::Java,
        JAVA,
        &[
            "Class Server",
            "Method Server.Server",
            "Method Server.run",
            "Method Server.add",
            "Interface Server.Listener",
            "Method Server.Listener.onStart",
            "Enum Server.State",
            "Class Point",
            "Method Point.sum",
            "Interface Marker",
        ],
    );
}

#[test]
fn csharp() {
    check(
        Dialect::CSharp,
        CSHARP,
        &[
            "Module Demo.App",
            "Class Demo.App.Server",
            "Method Demo.App.Server.Server",
            "Method Demo.App.Server.Start",
            "Method Demo.App.Server.Dispose",
            "Method Demo.App.Server.Twice",
            "Function Demo.App.Server.Twice.Local",
            "Interface Demo.App.IStart",
            "Method Demo.App.IStart.Start",
            "Struct Demo.App.Point",
            "Enum Demo.App.State",
            "Class Demo.App.User",
        ],
    );
}

#[test]
fn c() {
    check(
        Dialect::C,
        C,
        &[
            "Const LIMIT",
            "Function SQUARE",
            "Struct point",
            "Enum state",
            "TypeAlias node_t",
            "Struct node_t.node",
            "TypeAlias id_t",
            "Function add",
            "Function name",
            "Function main",
        ],
    );
}

#[test]
fn cpp() {
    check(
        Dialect::Cpp,
        CPP,
        &[
            "Module demo",
            "Class demo.Server",
            "Method demo.Server.Server",
            "Method demo.Server.~Server",
            "Method demo.Server.port",
            "Function demo.Server::start",
            "Struct demo.Point",
            "Enum demo.State",
            "TypeAlias demo.Id",
            "Function demo.twice",
            "Function demo.make",
        ],
    );
}

#[test]
fn go() {
    check(
        Dialect::Go,
        GO,
        &[
            "Const Limit",
            "Var counter",
            "Struct Server",
            "Interface Starter",
            "TypeAlias ID",
            "TypeAlias Name",
            "Struct A",
            "Struct B",
            "Function NewServer",
            "Method Server.Start",
            "Method Server.Port",
        ],
    );
}

#[test]
fn php() {
    check(
        Dialect::Php,
        PHP,
        &[
            "Const LIMIT",
            "Function top",
            "Interface Starter",
            "Method Starter.start",
            "Interface Logs",
            "Method Logs.log",
            "Enum State",
            "Class Server",
            "Const Server.PORT",
            "Method Server.__construct",
            "Method Server.start",
            "Method Server.create",
        ],
    );
}

fn symbol<'a>(file: &'a ParsedFile, qualified: &str) -> &'a kito_parse::ParsedSymbol {
    let index = (0..file.symbols.len())
        .find(|&i| file.qualified_name(i) == qualified)
        .unwrap_or_else(|| panic!("no symbol named {qualified}: {:?}", outline(file)));
    &file.symbols[index]
}

fn ts(source: &str) -> ParsedFile {
    parse(Dialect::TypeScript, source.as_bytes()).unwrap()
}

const SERVICE: &str = r#"
export class UserService {
  private cache = new Map<string, User>();

  getUser(id: string): User {
    const cached = this.cache.get(id);
    if (cached) {
      return cached;
    }
    const user = db.find(id);
    this.cache.set(id, user);
    return user;
  }

  clear(): void {
    this.cache.clear();
  }
}
"#;

#[test]
fn reformatting_and_comments_change_no_hash() {
    let original = ts(SERVICE);
    let reformatted = ts(r#"
// A service.
export class UserService
{
    private cache = new Map<string, User>();   // the cache

    /** Looks a user up. */
    getUser( id : string ) : User
    {
        const cached = this.cache.get( id );
        if ( cached ) { return cached; }

        /* miss */
        const user = db.find(id);
        this.cache.set(id, user); return user;
    }

    clear(): void { this.cache.clear(); }
}
"#);
    assert_eq!(outline(&original), outline(&reformatted));
    for (a, b) in original.symbols.iter().zip(&reformatted.symbols) {
        assert_eq!((a.signature, a.body, a.body_tokens), (b.signature, b.body, b.body_tokens), "{}", a.name);
        assert_eq!(a.fingerprint, b.fingerprint, "{}", a.name);
        assert_ne!(a.range, b.range);
    }

    // The same holds in an indentation-based language.
    let py = |s: &str| parse(Dialect::Python, s.as_bytes()).unwrap();
    let a = py("def area(w, h):\n    return w * h\n");
    let b =
        py("# geometry\ndef area( w,\n          h ):\n\n    # multiply\n    return (w *\n            h)\n");
    assert_eq!(a.symbols[0].signature, b.symbols[0].signature);
    assert_ne!(a.symbols[0].body, b.symbols[0].body, "added parentheses are real tokens");
    let c = py("def area(w, h):  # geometry\n\n    return w * h   # multiply\n");
    assert_eq!(a.symbols[0].body, c.symbols[0].body);
    assert_eq!(a.symbols[0].fingerprint, c.symbols[0].fingerprint);
}

#[test]
fn a_rename_keeps_body_signature_and_fingerprint() {
    let before = ts(SERVICE);
    let after = ts(&SERVICE.replace("getUser", "fetchUser"));
    let (old, new) = (symbol(&before, "UserService.getUser"), symbol(&after, "UserService.fetchUser"));
    assert_eq!((old.signature, old.body), (new.signature, new.body));
    assert_eq!(old.fingerprint.similarity(&new.fingerprint), 1.0);
    assert_eq!(old.fingerprint.sketch.len(), kito_parse::MINHASH_SIZE);
}

#[test]
fn a_small_edit_changes_the_body_but_stays_similar() {
    let before = ts(SERVICE);
    let after =
        ts(&SERVICE.replace("const user = db.find(id);", "const user = await db.find(id, { fresh: true });"));
    let (old, new) = (symbol(&before, "UserService.getUser"), symbol(&after, "UserService.getUser"));
    assert_eq!(old.signature, new.signature);
    assert_ne!(old.body, new.body);
    let similarity = old.fingerprint.similarity(&new.fingerprint);
    assert!(similarity > 0.5 && similarity < 1.0, "similarity {similarity}");
    // An unrelated method is far away.
    let other = symbol(&before, "UserService.clear");
    assert!(old.fingerprint.similarity(&other.fingerprint) < 0.3);
    // A changed signature leaves the body alone.
    let resigned =
        ts(&SERVICE.replace("getUser(id: string): User", "getUser(id: string, force = false): User"));
    let new = symbol(&resigned, "UserService.getUser");
    assert_ne!(old.signature, new.signature);
    assert_eq!(old.body, new.body);
}

#[test]
fn a_container_is_not_changed_by_what_its_members_do() {
    let before = ts(SERVICE);
    let class = symbol(&before, "UserService");
    // Editing a method, adding one, removing one: the class itself is the same.
    for edited in [
        SERVICE.replace("this.cache.clear();", "this.cache = new Map();"),
        SERVICE.replace(
            "  clear(): void {",
            "  size(): number { return this.cache.size; }\n\n  clear(): void {",
        ),
        SERVICE.replace("  clear(): void {\n    this.cache.clear();\n  }\n", ""),
    ] {
        let after = ts(&edited);
        assert_ne!(outline(&before).len() * 1000 + before.symbols[1].body_tokens, 0);
        let same = symbol(&after, "UserService");
        assert_eq!((class.body, &class.fingerprint), (same.body, &same.fingerprint));
    }
    // Its own members (here a field) do change it.
    let after = ts(&SERVICE.replace("new Map<string, User>()", "new LruCache<string, User>(100)"));
    assert_ne!(class.body, symbol(&after, "UserService").body);
    assert_eq!(symbol(&before, "UserService.getUser").body, symbol(&after, "UserService.getUser").body);
}

#[test]
fn syntax_errors_are_reported_not_fatal() {
    let broken = ts("export function ok() { return 1; }\nexport function broken( {\n");
    assert!(broken.has_errors);
    assert!(outline(&broken).contains(&"Function ok".to_owned()));
    let empty = ts("");
    assert!(!empty.has_errors && empty.symbols.is_empty());
    // Non-UTF-8 bytes do not crash the extraction.
    let odd = parse(Dialect::C, b"int f(void) { return 1; } /* \xff\xfe */ char *s = \"\xff\";").unwrap();
    assert_eq!(outline(&odd), ["Function f"]);
}

#[test]
fn overloads_and_duplicates_are_all_kept_in_order() {
    let file = ts("function f(a: string): void;\nfunction f(a: number): void;\nfunction f(a: any) {}\nconst a = 1, b = 2;\n");
    assert_eq!(outline(&file), ["Function f", "Function f", "Function f", "Const a", "Const b"]);
    let lines: Vec<u32> = file.symbols.iter().map(|s| s.line).collect();
    assert_eq!(lines, [1, 2, 3, 4, 4]);
}

mod incremental {
    use super::*;
    use kito_parse::parse_reusing;
    use proptest::prelude::*;

    /// A file is a list of lines taken from a small pool: enough to make
    /// functions appear, disappear, break and heal.
    const LINES: [&str; 10] = [
        "export function alpha(a: number) {",
        "  return a + 1;",
        "}",
        "export class Box {",
        "  open(): void { this.lid = true; }",
        "  // a comment",
        "const answer = () => 42;",
        "",
        "  if (x) { return \"brace } in a string\"; }",
        "interface Shape { area(): number }",
    ];

    fn render(lines: &[usize]) -> String {
        lines.iter().map(|&i| format!("{}\n", LINES[i])).collect()
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 200, ..ProptestConfig::default() })]

        /// Parsing a new version from the tree of the old one gives exactly
        /// what parsing it from scratch gives, whatever the two versions are.
        #[test]
        fn reusing_a_tree_changes_nothing(
            versions in prop::collection::vec(prop::collection::vec(0usize..LINES.len(), 0..14), 2..6),
        ) {
            let mut previous = None;
            for version in &versions {
                let source = render(version);
                let reused = parse_reusing(Dialect::TypeScript, source.as_bytes(), previous.as_ref()).unwrap();
                let fresh = parse(Dialect::TypeScript, source.as_bytes()).unwrap();
                prop_assert_eq!(&reused.file, &fresh);
                prop_assert_eq!(reused.source(), source.as_bytes());
                previous = Some(reused);
            }
        }
    }

    #[test]
    fn a_tree_of_another_language_is_ignored() {
        let rust = parse_reusing(Dialect::Rust, b"fn main() {}\n", None).unwrap();
        let ts = parse_reusing(Dialect::TypeScript, b"function main() {}\n", Some(&rust)).unwrap();
        assert_eq!(outline(&ts.file), ["Function main"]);
        assert_eq!(ts.file, parse(Dialect::TypeScript, b"function main() {}\n").unwrap());
    }
}
