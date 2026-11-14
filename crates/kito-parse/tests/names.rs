//! Names and imports: what level 2 reads from the syntax.

use kito_parse::{parse_reusing, Dialect, Parsed};

fn parsed(dialect: Dialect, source: &str) -> Parsed {
    parse_reusing(dialect, source.as_bytes(), None).unwrap()
}

/// Every use of `name`: its text with what comes before the dot, if any,
/// and `.` in front of the ones reached through something else.
fn uses(parsed: &Parsed, name: &str) -> Vec<String> {
    let source = std::str::from_utf8(parsed.source()).unwrap();
    parsed
        .identifiers()
        .into_iter()
        .filter(|id| &source[id.range.clone()] == name)
        .map(|id| match (id.member, id.qualifier) {
            (false, _) => name.to_owned(),
            (true, Some(q)) => format!("{}.{name}", &source[q]),
            (true, None) => format!(".{name}"),
        })
        .collect()
}

fn imports(parsed: &Parsed) -> Vec<String> {
    let source = std::str::from_utf8(parsed.source()).unwrap();
    parsed
        .imports()
        .into_iter()
        .map(|import| {
            let names: Vec<String> = import
                .names
                .iter()
                .map(|n| match &n.alias {
                    Some(alias) => format!("{} as {}", &source[n.name.clone()], &source[alias.clone()]),
                    None => source[n.name.clone()].to_owned(),
                })
                .collect();
            let namespace = import.namespace.map(|r| format!(" = {}", &source[r])).unwrap_or_default();
            format!("{} [{}]{namespace}", import.module, names.join(", "))
        })
        .collect()
}

#[test]
fn typescript_names_and_imports() {
    let file = parsed(
        Dialect::TypeScript,
        r#"import { getUser, Session as S } from "./auth";
import * as audit from "../audit";
import config from "./config";
export { getUser as load } from "./auth";

// getUser is mentioned here, and in "getUser" below, but not used.
export function profile(id: string): string {
  const user = getUser(id);
  audit.log("getUser", user.getUser, this.getUser());
  return { getUser }.getUser.name;
}
"#,
    );
    assert_eq!(
        uses(&file, "getUser"),
        // the import, the re-export, the call, two members, the shorthand, a member
        ["getUser", "getUser", "getUser", "user.getUser", ".getUser", "getUser", ".getUser"]
    );
    assert_eq!(uses(&file, "log"), ["audit.log"]);
    assert_eq!(
        imports(&file),
        ["./auth [getUser, Session as S]", "../audit [] = audit", "./config []", "./auth [getUser as load]"]
    );
}

#[test]
fn python_names_and_imports() {
    let file = parsed(
        Dialect::Python,
        r#"from .models import get_user, Session as S
from app.audit import log
import app.config as config
import os

def profile(uid):
    # get_user in a comment
    user = get_user(uid)
    return user.get_user, config.get_user("get_user"), self.get_user
"#,
    );
    assert_eq!(
        uses(&file, "get_user"),
        ["get_user", "get_user", "user.get_user", "config.get_user", "self.get_user"]
    );
    assert_eq!(
        imports(&file),
        [".models [get_user, Session as S]", "app.audit [log]", "app.config [] = config", "os [] = os"]
    );
}

#[test]
fn go_names_and_imports() {
    let file = parsed(
        Dialect::Go,
        r#"package web

import (
	"fmt"
	u "app/users"
	"app/audit"
)

func Profile(id string) users.User {
	user := u.GetUser(id)
	audit.Log(GetUser, user.GetUser())
	var s u.Session
	fmt.Println(s)
	return user
}
"#,
    );
    assert_eq!(uses(&file, "GetUser"), ["u.GetUser", "GetUser", "user.GetUser"]);
    assert_eq!(uses(&file, "Session"), ["u.Session"]);
    assert_eq!(imports(&file), ["fmt []", "app/users [] = u", "app/audit []"]);
}

#[test]
fn the_name_of_a_symbol_is_where_its_definition_gives_it() {
    let source = "export class Session {\n  refresh(now: number): void {}\n}\nexport const LIMIT = 3;\n";
    let file = parsed(Dialect::TypeScript, source);
    let names: Vec<&str> = file.file.symbols.iter().map(|s| &source[s.name_range.clone()]).collect();
    assert_eq!(names, ["Session", "refresh", "LIMIT"]);
}
