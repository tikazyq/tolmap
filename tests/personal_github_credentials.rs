//! Synthetic helper programs only: no GitHub traffic or real user configuration.
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};

const SECRET: &str = "synthetic-private-github-secret-317";

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn helper(home: &Path, bin: &Path, operation: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tolmap"))
        .args(["personal-github-credential", operation])
        .env_clear()
        .env("HOME", home)
        .env(
            "PATH",
            format!("relative::{}:/usr/bin:/bin:.", bin.display()),
        )
        .env("GH_TOKEN", "synthetic-gh-token")
        .env("GITHUB_TOKEN", "synthetic-github-token")
        .env("GH_HOST", "github.com")
        .env("SSH_AUTH_SOCK", "/synthetic/session.sock")
        .env("DBUS_SESSION_BUS_ADDRESS", "synthetic-session")
        .env("UNRELATED_SECRET", "must-not-be-inherited")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if operation == "get" {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"protocol=https\nhost=github.com\npath=owner/private.git\n\n")
            .unwrap();
    } else {
        drop(child.stdin.take());
    }
    child.wait_with_output().unwrap()
}

#[test]
fn helper_protocol_git_first_gh_fallback_and_environment_isolation() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let bin = temp.path().join("bin");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&bin).unwrap();
    let git_helper = bin.join("git-provider");
    let gh = bin.join("gh");
    std::fs::write(
        home.join(".gitconfig"),
        format!(
            "[credential]\n useHttpPath = true\n helper = {}\n",
            git_helper.display()
        ),
    )
    .unwrap();
    let assertions = r#"
[ "$SSH_AUTH_SOCK" = /synthetic/session.sock ] || exit 1
[ "$DBUS_SESSION_BUS_ADDRESS" = synthetic-session ] || exit 1
[ -z "$UNRELATED_SECRET" ] || exit 1
case ":$PATH:" in *:relative:*|*::*|*:.:*) exit 1;; esac
request=$(/bin/cat)
case "$request" in *'protocol=https'*'host=github.com'*'path=owner/private.git'*) ;; *) exit 1;; esac
"#;
    script(
        &git_helper,
        &format!(
            r#"
[ "$1" = get ] || exit 1
[ -z "$GH_TOKEN$GITHUB_TOKEN$GH_HOST" ] || exit 1
{assertions}
printf 'username=synthetic\npassword={SECRET}\n\n'
"#
        ),
    );
    script(&gh, "exit 99");
    let output = helper(&home, &bin, "get");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        format!("username=synthetic\npassword={SECRET}\n\n").as_bytes()
    );
    assert!(output.stderr.is_empty());

    // A helper without credentials makes git credential fill fail; gh is next.
    script(&git_helper, &format!("{assertions}\nexit 0"));
    script(
        &gh,
        &format!(
            r#"
[ "$1 $2 $3" = 'auth git-credential get' ] || exit 1
[ "$GH_TOKEN" = synthetic-gh-token ] || exit 1
[ "$GITHUB_TOKEN" = synthetic-github-token ] || exit 1
[ "$GH_HOST" = github.com ] || exit 1
{assertions}
printf 'username=synthetic\npassword={SECRET}\n\n'
"#
        ),
    );
    let output = helper(&home, &bin, "get");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains(SECRET));
    assert!(output.stderr.is_empty());

    // Git must never ask tolmap to save or erase user credentials.
    script(&git_helper, "exit 99");
    script(&gh, "exit 99");
    for operation in ["store", "erase"] {
        let output = helper(&home, &bin, operation);
        assert!(output.status.success());
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
    }
}

#[test]
fn provider_diagnostics_are_fixed_and_no_secret_reaches_persistent_storage() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let bin = temp.path().join("bin");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&bin).unwrap();
    let git_helper = bin.join("git-provider");
    std::fs::write(
        home.join(".gitconfig"),
        format!(
            "[credential]\n useHttpPath = true\n helper = {}\n",
            git_helper.display()
        ),
    )
    .unwrap();
    script(&git_helper, &format!("printf '{SECRET}\\n' >&2\nexit 1"));
    script(
        &bin.join("gh"),
        &format!("printf '{SECRET}\\n' >&2\nexit 1"),
    );
    let output = helper(&home, &bin, "get");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr.clone()).unwrap().trim(),
        format!("Error: {}", tolmap::personal::github::ACCESS_ERROR)
    );

    // Exercise the same persisted failure paths used by refresh and web jobs,
    // with WAL still open, and scan both database and diagnostics verbatim.
    let data = temp.path().join("data");
    let library = tolmap::personal::Library::open(Some(&data)).unwrap();
    let repo = tolmap::service::store::Repository {
        id: "synthetic-repo".into(),
        name: "private".into(),
        kind: "github".into(),
        location: "github.com/owner/private".into(),
        observed_head: None,
        observed_at: None,
    };
    library.store.register(&repo).unwrap();
    library
        .store
        .personal_begin(&repo.id, "previous-attempt")
        .unwrap();
    let previous = tolmap::service::store::PersonalSnapshot {
        id: "previous-snapshot".into(),
        repository: repo.id.clone(),
        commit: "a".repeat(40),
        map_path: data.join("previous-map"),
        source_path: data.join("previous-source"),
        indexed_at: String::new(),
    };
    library
        .store
        .personal_finish("previous-attempt", Ok(&previous))
        .unwrap();
    library
        .store
        .personal_begin(&repo.id, "synthetic-attempt")
        .unwrap();
    let error = String::from_utf8(output.stderr).unwrap();
    library
        .store
        .personal_finish("synthetic-attempt", Err(&error))
        .unwrap();
    assert_eq!(
        library.store.personal_history(&repo.id).unwrap()[0].id,
        previous.id
    );
    assert!(library
        .config
        .service
        .db_path
        .with_extension("sqlite3-wal")
        .is_file());
    std::fs::write(data.join("diagnostics.log"), error).unwrap();
    for entry in std::fs::read_dir(&data).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let bytes = std::fs::read(path).unwrap();
            assert!(!bytes.windows(SECRET.len()).any(|s| s == SECRET.as_bytes()));
        }
    }
}
