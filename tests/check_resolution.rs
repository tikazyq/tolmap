//! Registered check resolution needs no extraction. Exercise linked worktrees
//! through the CLI so the process's cwd does not race other tests.
use std::path::{Path, PathBuf};
use std::process::Command;

use tolmap::personal::Library;
use tolmap::service::store::{PersonalSnapshot, Repository};

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(path)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

struct Fixture {
    _temp: tempfile::TempDir,
    data: PathBuf,
    checkout: PathBuf,
    worktree: PathBuf,
    commit: String,
    map: PathBuf,
}

impl Fixture {
    fn new(kind: &str, publish: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let checkout = temp.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        git(&checkout, &["init", "-q"]);
        std::fs::write(checkout.join("README.md"), "base\n").unwrap();
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-qm", "base"]);
        let commit = git(&checkout, &["rev-parse", "HEAD"]);
        let worktree = temp.path().join("worktree");
        git(
            &checkout,
            &[
                "worktree",
                "add",
                "--detach",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        let data = temp.path().join("data");
        let library = Library::open(Some(&data)).unwrap();
        library
            .store
            .register(&Repository {
                id: "fixture".into(),
                name: "owner/name".into(),
                kind: kind.into(),
                location: if kind == "local" {
                    checkout.to_string_lossy().into()
                } else {
                    "github.com/owner/name".into()
                },
                observed_head: None,
                observed_at: None,
            })
            .unwrap();
        let map = temp.path().join("map.json");
        if publish {
            // An intentionally incomplete map: resolution succeeds, then check
            // rejects the document before running extraction.
            std::fs::write(&map, format!("{{\"commit\":\"{commit}\"}}")).unwrap();
            library.store.personal_begin("fixture", "snapshot").unwrap();
            library
                .store
                .personal_finish(
                    "snapshot",
                    Ok(&PersonalSnapshot {
                        id: "snapshot".into(),
                        repository: "fixture".into(),
                        commit: commit.clone(),
                        map_path: map.clone(),
                        source_path: checkout.clone(),
                        indexed_at: String::new(),
                    }),
                )
                .unwrap();
        }
        Self {
            _temp: temp,
            data,
            checkout,
            worktree,
            commit,
            map,
        }
    }

    fn run(&self, extra: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_tolmap"))
            .current_dir(&self.worktree)
            .arg("--data")
            .arg(&self.data)
            .args(["check", "--repo", "owner/name", "--report-only"])
            .args(extra)
            .output()
            .unwrap()
    }
}

#[test]
fn remote_registration_from_worktree_names_missing_head_and_flags() {
    let f = Fixture::new("github", true);
    let output = f.run(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    for text in [
        "registered as a remote",
        "--head REF",
        "tolmap check . --base",
        "--base-map",
    ] {
        assert!(error.contains(text), "{error}");
    }
    let output = f.run(&["--head", "not-a-revision"]);
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        error.contains("--head revision \"not-a-revision\" is unavailable"),
        "{error}"
    );
}

#[test]
fn missing_retained_source_names_path_and_recovery_flags() {
    let f = Fixture::new("github", true);
    let library = Library::open(Some(&f.data)).unwrap();
    let mut snapshot = library
        .store
        .personal_snapshot("snapshot")
        .unwrap()
        .unwrap();
    snapshot.id = "missing-source".into();
    snapshot.source_path = f.data.join("missing-source");
    library
        .store
        .personal_begin("fixture", &snapshot.id)
        .unwrap();
    library
        .store
        .personal_finish(&snapshot.id, Ok(&snapshot))
        .unwrap();
    let output = f.run(&["--head", &f.commit]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    for text in [
        "retained source",
        "missing-source",
        "--base REF",
        "--base-map FILE",
    ] {
        assert!(error.contains(text), "{error}");
    }
}

#[test]
fn missing_snapshot_and_map_name_recovery_flags() {
    let f = Fixture::new("local", false);
    let output = f.run(&[]);
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8(output.stderr).unwrap();
    for text in [
        "indexed base snapshot",
        "--commit SHA",
        "--snapshot ID",
        "--data ROOT",
        "--base REF",
    ] {
        assert!(error.contains(text), "{error}");
    }
    let f = Fixture::new("local", true);
    std::fs::remove_file(&f.map).unwrap();
    let output = f.run(&[]);
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("indexed base map"), "{error}");
    assert!(error.contains("--base-map FILE"), "{error}");
}

#[test]
fn local_registration_resolves_head_in_callers_linked_worktree() {
    let f = Fixture::new("local", true);
    git(&f.worktree, &["checkout", "-qb", "worktree-head"]);
    std::fs::write(f.worktree.join("README.md"), "head\n").unwrap();
    git(&f.worktree, &["commit", "-qam", "head"]);
    // This branch is shared, but HEAD~1 differs between the two worktrees.
    // Requiring --base HEAD~1 to agree with the snapshot proves which is used.
    let output = f.run(&["--base", "HEAD~1"]);
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("--base-map"), "{error}");
    assert!(!error.contains("--base must agree"), "{error}");
    assert!(!error.contains("Git rev-parse failed"), "{error}");
    assert_eq!(git(&f.checkout, &["rev-parse", "HEAD"]), f.commit);
}
