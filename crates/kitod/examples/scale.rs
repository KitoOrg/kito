//! Measures the two targets of phase 3 on a large repository:
//! creating a lane, and integrating a hundred ops.
//!
//! ```text
//! cargo run --release -p kitod --example scale -- --files 50000 [--dir DIR]
//! ```
//!
//! The repository is made of `--files` TypeScript files of about 2 kB,
//! four functions each. No verification command is configured: the plan's
//! target for an integration excludes the verifier.

use kito_core::LaneId;
use kito_integrate::Outcome;
use kitod::Repo;
use std::path::PathBuf;
use std::time::Instant;

fn file(i: usize, mark: usize) -> String {
    (0..4)
        .map(|f| {
            format!(
                "export function compute{i}_{f}(input: number[], offset: number): number {{\n  let total = offset + {};\n  for (const value of input) {{\n    if (value % 2 === 0) {{\n      total += value * {f} + 1;\n    }} else {{\n      total -= value;\n    }}\n  }}\n  // file {i}, function {f}: keeps the running total within the expected bounds\n  return Math.max(Math.min(total, 1_000_000), -1_000_000);\n}}\n",
                if f == 0 { mark } else { f }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let files: usize = value("--files").and_then(|v| v.parse().ok()).unwrap_or(50_000);
    let dir = value("--dir").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let root = dir.join(format!("kito-scale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());

    std::fs::create_dir_all(&root)?;
    let path = |i: usize| format!("src/pkg{:03}/mod{:02}/file{i}.ts", i / 10_000, (i / 100) % 100);
    for i in 0..files {
        let full = root.join(path(i));
        std::fs::create_dir_all(full.parent().ok_or("no parent")?)?;
        std::fs::write(full, file(i, 0))?;
    }
    let mut repo = Repo::init(&root)?;
    // The export to git is not part of either target: it is on only with
    // `--git-export`, to see what the mirror costs.
    let export = args.iter().any(|a| a == "--git-export");
    std::fs::write(
        root.join(".kito/config.toml"),
        format!("[author]\nname = \"ada\"\n[git]\nexport_on_checkpoint = {export}\n"),
    )?;
    repo.reload_config()?;
    let started = Instant::now();
    let ops = repo.scan()?.len();
    println!(
        "{files} files ({} kB each) captured for the first time: {ops} ops in {:.1?}",
        file(0, 0).len() / 1000,
        started.elapsed()
    );
    let started = Instant::now();
    repo.checkpoint("start")?;
    if export {
        println!("first checkpoint, exported to git: {:.1?}", started.elapsed());
    }

    // What the first capture wrote is still on its way to the disk: the
    // measures that follow are not about that.
    let _ = std::process::Command::new("sync").status();

    // 1. Creating a lane.
    println!("\n| Creating a lane on {files} files | Time |\n| --- | --- |");
    let mut times = Vec::new();
    for round in 1..=5 {
        let started = Instant::now();
        let lane =
            repo.open_lane(&format!("agent-{round}"), None, None, "work", &kitod::scope::everything())?;
        let took = started.elapsed().as_secs_f64() * 1000.0;
        times.push(took);
        println!("| lane {round} ({}) | {took:.0} ms |", lane.id);
    }
    times.sort_by(f64::total_cmp);
    println!("| median | {:.0} ms |", times[times.len() / 2]);
    println!("| best | {:.0} ms |", times[0]);

    // 2. Integrating a hundred ops: fifty files, one function changed in
    //    each (a file-level op and a symbolic one per file). Then a
    //    thousand, and a hundred again from lanes the main lane has left
    //    behind meanwhile.
    println!("\n| Integrating a lane | Ops of the lane | Time |\n| --- | --- | --- |");
    for (lane, changed) in [(1u64, 50usize), (2, 500), (3, 50), (4, 50), (5, 50)] {
        let workdir = repo.lane_info(LaneId(lane))?.workdir.ok_or("no working copy")?;
        for i in 0..changed {
            let i = (i * 37 + lane as usize * 1000) % files;
            std::fs::write(workdir.join(path(i)), file(i, 7 + lane as usize))?;
        }
        let recorded = repo.scan_lane(LaneId(lane))?.len();
        // As the daemon does it: git's index is rewritten after the lane
        // has its answer, and is not part of the time.
        let started = Instant::now();
        let result = repo.integrate_leaving_index(LaneId(lane))?;
        let took = started.elapsed().as_secs_f64() * 1000.0;
        repo.refresh_git_index()?;
        if result.result != Outcome::Integrated {
            return Err(format!("not integrated: {result:?}").into());
        }
        println!(
            "| lane {lane}: {changed} files, one function each | {recorded} | {took:.0} ms ({:.0} ms per 100 ops) |",
            took * 100.0 / recorded as f64
        );
    }
    // 3. Level 2 with the syntax alone: who uses a function, in a lane.
    if args.iter().any(|a| a == "--rename") {
        std::fs::write(
            root.join(".kito/config.toml"),
            "[git]\nexport_on_checkpoint = false\n[semantic]\nservers = {}\n",
        )?;
        repo.reload_config()?;
        let lane = repo.open_lane("renamer", None, None, "work", &kitod::scope::everything())?.id;
        let lane: LaneId = lane.parse().map_err(|_| "lane id")?;
        let started = Instant::now();
        let done = repo.rename_symbol(lane, &format!("{}:compute7_2", path(7)), "renamed7_2")?;
        println!(
            "\nexplicit rename in a lane, uses looked for in the syntax of {files} files: {:.0} ms ({} listed)",
            started.elapsed().as_secs_f64() * 1000.0,
            done.possible_references.len()
        );
        let started = Instant::now();
        let result = repo.integrate(lane)?;
        println!(
            "integrating the rename (two questions to the provider): {:.0} ms, {:?}",
            started.elapsed().as_secs_f64() * 1000.0,
            result.result
        );
    }
    let verified = repo.store().verify()?;
    println!("\nstore: {} ops, verified: {}", verified.ops, verified.is_ok());
    repo.close()?;
    Ok(())
}
