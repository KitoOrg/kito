//! What the syntax alone tells about who uses a symbol.

mod common;

use common::World;
use kito_semantic::{reference, SemanticProvider, TreeSitterHeuristic};
use std::path::Path;

const AUTH: &str = r#"export function getUser(id: string): User {
  return db.find(id);
}

export function current(): User {
  return getUser(session.id);
}
"#;

fn typescript() -> World {
    World::new(&[
        ("src/auth.ts", AUTH),
        ("src/web.ts", "import { getUser } from \"./auth\";\n\nexport function profile(id: string): string {\n  return getUser(id).name;\n}\n"),
        (
            "src/admin.ts",
            "import { getUser as load } from \"./auth\";\nimport * as auth from \"./auth\";\n\nexport function admin(id: string) {\n  return [load(id), auth.getUser(id), other.getUser(id)];\n}\n",
        ),
        // Another `getUser`, from another module: not ours.
        ("src/users.ts", "export function getUser(n: number): number {\n  return n;\n}\n"),
        ("src/other.ts", "import { getUser } from \"./users\";\n\nexport const first = getUser(1);\n"),
        ("README.md", "Call getUser to get a user.\n"),
    ])
}

#[test]
fn uses_in_the_same_file_and_through_imports() {
    let world = typescript();
    let found = TreeSitterHeuristic.usages_of(&world.snapshot(), world.id("src/auth.ts", "getUser")).unwrap();
    assert_eq!(
        world.show(&found),
        [
            "src/admin.ts:1 `import { getUser as load } from \"./auth\";` 0.8",
            "src/admin.ts:5 `return [load(id), auth.getUser(id), other.getUser(id)];` 0.7",
            "src/auth.ts:6 `return getUser(session.id);` 0.8",
            "src/web.ts:1 `import { getUser } from \"./auth\";` 0.8",
            "src/web.ts:4 `return getUser(id).name;` 0.7",
        ]
    );
    // Of the two `getUser` on that line of admin.ts, the one through `auth`.
    let admin = &world.files[Path::new("src/admin.ts")];
    assert_eq!(&admin[found[1].range.start - 5..found[1].range.end], b"auth.getUser");

    // The other `getUser` has its own users, and only those.
    let found =
        TreeSitterHeuristic.usages_of(&world.snapshot(), world.id("src/users.ts", "getUser")).unwrap();
    assert_eq!(
        world.show(&found),
        [
            "src/other.ts:1 `import { getUser } from \"./users\";` 0.8",
            "src/other.ts:3 `export const first = getUser(1);` 0.7",
        ]
    );
    // Nothing is ever sure enough to act on without asking.
    assert!(found.iter().all(|usage| usage.confidence < kito_semantic::SURE));
}

#[test]
fn members_are_reached_through_something() {
    let world = World::new(&[(
        "src/session.ts",
        "export class Session {\n  refresh(now: number): void {\n    this.expires = now + 60;\n  }\n\n  touch(now: number): void {\n    this.refresh(now);\n  }\n}\n\nexport function refresh(session: Session): void {\n  session.refresh(Date.now());\n}\n\nexport function again(): void {\n  refresh(new Session());\n}\n",
    )]);
    let method = TreeSitterHeuristic
        .usages_of(&world.snapshot(), world.id("src/session.ts", "Session.refresh"))
        .unwrap();
    assert_eq!(
        world.show(&method),
        ["src/session.ts:7 `this.refresh(now);` 0.7", "src/session.ts:12 `session.refresh(Date.now());` 0.5"]
    );
    // The function of the same name: the plain uses, less sure for the namesake.
    let function =
        TreeSitterHeuristic.usages_of(&world.snapshot(), world.id("src/session.ts", "refresh")).unwrap();
    assert_eq!(world.show(&function), ["src/session.ts:16 `refresh(new Session());` 0.6"]);
}

#[test]
fn python_and_go_imports() {
    let world = World::new(&[
        ("app/models.py", "def get_user(uid):\n    return uid\n"),
        ("app/views.py", "from .models import get_user\n\n\ndef show(uid):\n    return get_user(uid)\n"),
        ("app/jobs.py", "from app.models import get_user as load\nimport app.models as models\n\n\ndef run(uid):\n    return load(uid), models.get_user(uid)\n"),
        ("app/unrelated.py", "def get_user(uid):\n    return None\n\n\ndef twice(uid):\n    return get_user(get_user(uid))\n"),
        ("users/users.go", "package users\n\nfunc GetUser(id string) string {\n\treturn id\n}\n"),
        ("users/more.go", "package users\n\nfunc Twice(id string) string {\n\treturn GetUser(GetUser(id))\n}\n"),
        ("web/web.go", "package web\n\nimport \"app/users\"\n\nfunc Profile(id string) string {\n\treturn users.GetUser(id)\n}\n"),
        ("web/local.go", "package web\n\nfunc GetUser(id string) string {\n\treturn \"local \" + id\n}\n\nfunc Local(id string) string {\n\treturn GetUser(id)\n}\n"),
    ]);
    let found =
        TreeSitterHeuristic.usages_of(&world.snapshot(), world.id("app/models.py", "get_user")).unwrap();
    assert_eq!(
        world.show(&found),
        [
            "app/jobs.py:1 `from app.models import get_user as load` 0.6",
            "app/jobs.py:6 `return load(uid), models.get_user(uid)` 0.5",
            "app/views.py:1 `from .models import get_user` 0.8",
            "app/views.py:5 `return get_user(uid)` 0.7",
        ]
    );
    let found =
        TreeSitterHeuristic.usages_of(&world.snapshot(), world.id("users/users.go", "GetUser")).unwrap();
    assert_eq!(
        world.show(&found),
        [
            "users/more.go:4 `return GetUser(GetUser(id))` 0.7",
            "users/more.go:4 `return GetUser(GetUser(id))` 0.7",
            "web/web.go:6 `return users.GetUser(id)` 0.7",
        ]
    );
}

#[tokio::test]
async fn references_leave_the_symbols_of_a_file() {
    let world = typescript();
    let snap = world.snapshot();
    let name = |id| world.table.qualified_name(id).unwrap();
    let refs = TreeSitterHeuristic.references(&snap, Path::new("src/web.ts")).await.unwrap();
    let shown: Vec<String> = refs
        .iter()
        .map(|r| format!("{} -> {} line {} {:.1}", name(r.from), name(r.to), r.site.line, r.confidence))
        .collect();
    assert_eq!(shown, ["profile -> getUser line 4 0.7"]);
    assert_eq!(refs[0].to, world.id("src/auth.ts", "getUser"));
    assert_eq!(world.table.get(refs[0].to).unwrap().file, Path::new("src/auth.ts"));

    let refs = TreeSitterHeuristic.references(&snap, Path::new("src/auth.ts")).await.unwrap();
    let shown: Vec<String> = refs.iter().map(|r| format!("{} -> {}", name(r.from), name(r.to))).collect();
    assert_eq!(shown, ["current -> getUser"]);

    // A usage is a reference from the symbol it is written in; an import is in none.
    let get_user = world.id("src/auth.ts", "getUser");
    let found = TreeSitterHeuristic.usages(&snap, get_user).await.unwrap();
    let from: Vec<Option<String>> =
        found.iter().map(|usage| reference(&snap, get_user, usage).map(|r| name(r.from))).collect();
    assert_eq!(
        from,
        [None, Some("admin".to_owned()), Some("current".to_owned()), None, Some("profile".to_owned())]
    );
}
