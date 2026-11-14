//! The provider that reads a SCIP index: the index in `fixtures/scip` was
//! made by `scip-typescript index` on the three files next to it.

mod common;

use common::World;
use kito_core::RefSource;
use kito_semantic::{ScipImporter, ScipIndex, ScipSource, Semantic};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scip").join(name)
}

fn read(name: &str) -> String {
    std::fs::read_to_string(fixture(name)).unwrap()
}

/// The project the index was made on, as a repository state.
fn project() -> World {
    let files = [
        ("src/auth.ts", read("src/auth.ts")),
        ("src/web.ts", read("src/web.ts")),
        ("src/other.ts", read("src/other.ts")),
    ];
    let files: Vec<(&str, &str)> = files.iter().map(|(path, content)| (*path, content.as_str())).collect();
    World::new(&files)
}

/// What the files held when the index was made: here, what they hold.
fn then(world: &World) -> HashMap<PathBuf, kito_core::BlobId> {
    world.tree.iter().map(|(path, entry)| (path.to_owned(), entry.blob)).collect()
}

#[test]
fn an_index_says_who_uses_a_symbol() {
    let world = project();
    let index = ScipIndex::parse(&std::fs::read(fixture("index.scip")).unwrap()).unwrap();
    assert_eq!(
        index.files().collect::<Vec<_>>(),
        [Path::new("src/auth.ts"), Path::new("src/other.ts"), Path::new("src/web.ts")]
    );
    let importer = ScipImporter::new(index, then(&world));

    let found = importer.usages_of(&world.snapshot(), world.id("src/auth.ts", "getUser")).unwrap().unwrap();
    assert_eq!(
        world.show(&found),
        [
            "src/auth.ts:6 `return getUser(\"me\");` 1.0",
            "src/web.ts:1 `import { getUser } from \"./auth\";` 1.0",
            "src/web.ts:4 `return getUser(id);` 1.0",
        ]
    );
    assert!(found.iter().all(|usage| usage.source == RefSource::Scip));
    // The other function of the same name has its own uses, and only those.
    let found = importer.usages_of(&world.snapshot(), world.id("src/other.ts", "getUser")).unwrap().unwrap();
    assert_eq!(world.show(&found), ["src/other.ts:5 `export const first = getUser(1);` 1.0"]);
    // A symbol nobody uses, and one the index was not told about.
    let found = importer.usages_of(&world.snapshot(), world.id("src/web.ts", "profile")).unwrap().unwrap();
    assert!(found.is_empty());

    assert!(ScipIndex::parse(b"not an index at all \xff\xff\xff").is_err());
}

#[tokio::test]
async fn an_index_answers_only_for_the_files_that_did_not_change() {
    let main = project();
    let files = then(&main);
    let index = ScipIndex::parse(&std::fs::read(fixture("index.scip")).unwrap()).unwrap();
    let importer = ScipImporter::new(index, files.clone());

    // A lane: web.ts gets one more use, and there is a new file.
    let web = read("src/web.ts").replace("return getUser(id);", "return getUser(id) + getUser(\"twice\");");
    let lane = main.with(&[
        ("src/web.ts", Some(&web)),
        (
            "src/fresh.ts",
            Some("import { getUser } from \"./auth\";\n\nexport const fresh = getUser(\"new\");\n"),
        ),
    ]);
    let get_user = lane.id("src/auth.ts", "getUser");
    assert!(importer.knows(&lane.snapshot(), Path::new("src/auth.ts")));
    assert!(!importer.knows(&lane.snapshot(), Path::new("src/web.ts")));
    assert!(!importer.knows(&lane.snapshot(), Path::new("src/fresh.ts")));
    let found = importer.usages_of(&lane.snapshot(), get_user).unwrap().unwrap();
    assert_eq!(lane.show(&found), ["src/auth.ts:6 `return getUser(\"me\");` 1.0"]);

    // Among the providers of a repository: the index for what it knows, the
    // syntax (less sure) for what changed since.
    let semantic = Semantic::new(Vec::new()).with_scip(ScipSource { index: fixture("index.scip"), files });
    let mut problems = Vec::new();
    let found = semantic.usages(&lane.snapshot(), get_user, &mut problems).await.unwrap();
    assert_eq!(
        lane.show(&found),
        [
            "src/auth.ts:6 `return getUser(\"me\");` 1.0",
            "src/fresh.ts:1 `import { getUser } from \"./auth\";` 0.8",
            "src/fresh.ts:3 `export const fresh = getUser(\"new\");` 0.7",
            "src/web.ts:1 `import { getUser } from \"./auth\";` 0.8",
            "src/web.ts:4 `return getUser(id) + getUser(\"twice\");` 0.7",
            "src/web.ts:4 `return getUser(id) + getUser(\"twice\");` 0.7",
        ]
    );
    assert!(problems.is_empty());

    // The file that defines the symbol changed: the index cannot say.
    let moved = main.with(&[("src/auth.ts", Some(&format!("// a new first line\n{}", read("src/auth.ts"))))]);
    let get_user = moved.id("src/auth.ts", "getUser");
    assert!(importer.usages_of(&moved.snapshot(), get_user).unwrap().is_none());
    let found = semantic.usages(&moved.snapshot(), get_user, &mut problems).await.unwrap();
    assert!(found.iter().all(|usage| usage.source == RefSource::TreeSitterHeuristic));
    assert_eq!(found.len(), 3);
}
