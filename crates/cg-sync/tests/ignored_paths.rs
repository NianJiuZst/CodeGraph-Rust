//! Git-tracked ignored sources must follow the same inventory policy on path-driven sync.
use cg_resolve::{build_seg, index_project, IndexOptions};
use cg_store::{dump, Store, View};
use cg_sync::{spawn_watcher, sync, SyncOptions};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git").arg("-C").arg(root).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn graph(root: &Path) -> Vec<String> {
    dump::canonical(&Store::at(root).open_snapshot().unwrap().view)
}

fn assert_fresh(root: &Path, opts: &IndexOptions) {
    // Build against the same Git inventory without replacing the incremental snapshot.
    let fresh = View::new(Arc::new(build_seg(root, opts).unwrap().seg));
    assert_eq!(graph(root), dump::canonical(&fresh));
}

#[test]
fn newly_tracked_ignored_sources_are_discovered_by_path_sync() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "--quiet"]);
    write(root, ".gitignore", "ignored/\n");
    write(root, "main.ts", "export function main() {}\n");
    write(root, "ignored/new.ts", "export function added() {}\n");
    write(root, "ignored/untracked.ts", "export function untracked() {}\n");
    write(root, "ignored/excluded.ts", "export function excluded() {}\n");
    write(root, "node_modules/dep/index.ts", "export function dependency() {}\n");
    write(root, "cgr.json", r#"{"exclude":["ignored/excluded.ts"]}"#);
    for i in 0..48 {
        write(root, &format!("filler{i}.ts"), &format!("export function filler{i}() {{}}\n"));
    }
    let opts = IndexOptions { threads: Some(2), ..Default::default() };
    index_project(root, &opts).unwrap();
    assert!(!graph(root).iter().any(|l| l.contains("ignored/new.ts")));

    git(root, &["add", "-f", "ignored/new.ts", "ignored/excluded.ts", "node_modules/dep/index.ts"]);
    let report = sync(
        root,
        &SyncOptions {
            index: opts.clone(),
            paths: Some(vec![root.join("ignored/new.ts"), root.join("ignored/excluded.ts"), root.join("node_modules/dep/index.ts")]),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(report.full_rebuild.is_none(), "{report:?}");
    assert_eq!(report.added, 1, "{report:?}");
    let actual = graph(root);
    assert!(actual.iter().any(|l| l.contains("ignored/new.ts")));
    for path in ["ignored/untracked.ts", "ignored/excluded.ts", "node_modules/dep/index.ts"] {
        assert!(!actual.iter().any(|l| l.contains(path)), "must exclude {path}");
    }
    assert_fresh(root, &opts);

    write(root, "ignored/new.ts", "export function renamed() { return 123; }\n");
    let report = sync(root, &SyncOptions { index: opts.clone(), paths: Some(vec![root.join("ignored/new.ts")]), ..Default::default() }).unwrap();
    assert!(report.full_rebuild.is_none(), "{report:?}");
    assert!(graph(root).iter().any(|l| l.ends_with(" name=renamed")));
    assert_fresh(root, &opts);

    std::fs::remove_file(root.join("ignored/new.ts")).unwrap();
    let report = sync(root, &SyncOptions { index: opts.clone(), paths: Some(vec![root.join("ignored/new.ts")]), ..Default::default() }).unwrap();
    assert!(report.full_rebuild.is_none(), "{report:?}");
    assert!(!graph(root).iter().any(|l| l.contains("ignored/new.ts")));
    assert_fresh(root, &opts);
}

#[test]
fn git_inventory_events_refresh_ignored_file_membership() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "--quiet"]);
    write(root, ".gitignore", "ignored/\n");
    write(root, "ignored/late.ts", "export function late() {}\n");
    write(root, "conditional.ts", "export function conditional() {}\n");
    for i in 0..48 {
        write(root, &format!("filler{i}.ts"), &format!("export function filler{i}() {{}}\n"));
    }
    let opts = IndexOptions { threads: Some(2), ..Default::default() };
    index_project(root, &opts).unwrap();
    let sync_event = |rel: &str| sync(root, &SyncOptions { index: opts.clone(), paths: Some(vec![root.join(rel)]), ..Default::default() }).unwrap();

    git(root, &["add", "-f", "ignored/late.ts"]);
    let report = sync_event(".git/index");
    assert_eq!(report.added, 1, "{report:?}");
    assert_fresh(root, &opts);

    git(root, &["rm", "--cached", "--quiet", "ignored/late.ts"]);
    let report = sync_event(".git/index");
    assert!(root.join("ignored/late.ts").is_file());
    assert!(report.full_rebuild.is_none(), "{report:?}");
    assert!(!graph(root).iter().any(|l| l.contains("ignored/late.ts")));
    assert_fresh(root, &opts);

    write(root, ".git/info/exclude", "conditional.ts\n");
    let report = sync_event(".git/info/exclude");
    assert_eq!(report.removed, 1, "{report:?}");
    assert_fresh(root, &opts);

    write(root, ".git/info/exclude", "");
    let report = sync_event(".git/info/exclude");
    assert!(report.full_rebuild.is_none(), "{report:?}");
    assert!(graph(root).iter().any(|l| l.contains("conditional.ts")));
    assert_fresh(root, &opts);
}

#[test]
fn watcher_observes_ignored_edits_and_git_index_changes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    git(&root, &["init", "--quiet"]);
    write(&root, ".gitignore", "ignored/\n");
    write(&root, "ignored/tracked.ts", "export function before() {}\n");
    git(&root, &["add", "-f", "ignored/tracked.ts"]);
    let opts = IndexOptions { threads: Some(2), ..Default::default() };
    index_project(&root, &opts).unwrap();
    let watcher = spawn_watcher(root.clone(), SyncOptions { index: opts.clone(), ..Default::default() });
    let await_graph = |predicate: &dyn Fn(&[String]) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let actual = graph(&root);
            if predicate(&actual) {
                break;
            }
            assert!(Instant::now() < deadline, "watcher did not update the graph: {actual:#?}");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_fresh(&root, &opts);
    };
    write(&root, "ignored/tracked.ts", "export function after_edit() {}\n");
    await_graph(&|g| g.iter().any(|l| l.ends_with(" name=after_edit")));

    write(&root, "ignored/late.ts", "export function late() {}\n");
    git(&root, &["add", "-f", "ignored/late.ts"]);
    await_graph(&|g| g.iter().any(|l| l.contains("ignored/late.ts")));
    // The filter must refresh after publication to admit edits to the newly tracked source.
    write(&root, "ignored/late.ts", "export function late_edit() {}\n");
    await_graph(&|g| g.iter().any(|l| l.ends_with(" name=late_edit")));

    git(&root, &["rm", "--cached", "--force", "--quiet", "ignored/late.ts"]);
    await_graph(&|g| !g.iter().any(|l| l.contains("ignored/late.ts")));
    std::fs::remove_file(root.join("ignored/tracked.ts")).unwrap();
    await_graph(&|g| !g.iter().any(|l| l.contains("ignored/tracked.ts")));
    watcher.stop();
}
