use super::*;
use crate::{
    personal::{Library, SourceArgs, REMOTE_HEAD},
    service::{application_config::ApplicationConfig, config::ServeConfig, store::Store},
};
use std::{cell::RefCell, os::unix::process::ExitStatusExt, process::ExitStatus};

const SECRET: &str = "tolmap-synthetic-credential-marker";

fn output(success: bool, stdout: &[u8], stderr: &[u8]) -> Output {
    Output {
        status: ExitStatus::from_raw(if success { 0 } else { 128 << 8 }),
        stdout: stdout.into(),
        stderr: stderr.into(),
    }
}

#[test]
fn retries_only_access_refusals() {
    for (stderr, expected_attempts) in [
        ("fatal: unable to access 'https://github.com/owner/name/': The requested URL returned error: 401", vec![false, true]),
        ("fatal: repository 'https://github.com/owner/name/' not found", vec![false, true]),
        ("fatal: could not read Username for 'https://github.com': terminal prompts disabled", vec![false, true]),
        ("fatal: unable to access 'https://github.com/owner/name/': Could not resolve host: github.com", vec![false]),
        ("fatal: unable to access 'https://github.com/owner/name/': The requested URL returned error: 500", vec![false]),
        ("fatal: unable to access 'https://github.com/owner/name/': The requested URL returned error: 403", vec![false]),
        ("fatal: destination path 'checkout' already exists", vec![false]),
    ] {
        let attempts = RefCell::new(Vec::new());
        let result = anonymous_then_authenticated(|authenticated| {
            attempts.borrow_mut().push(authenticated);
            Ok(output(authenticated, b"head", stderr.as_bytes()))
        });
        assert_eq!(attempts.into_inner(), expected_attempts);
        if expected_attempts.len() == 2 {
            assert_eq!(result.unwrap(), b"head");
        } else {
            assert_eq!(result.unwrap_err().to_string(), GIT_ERROR);
        }
    }
    let stdout = anonymous_then_authenticated(|authenticated| {
        assert!(!authenticated);
        Ok(output(true, b"public head", b""))
    });
    assert_eq!(stdout.unwrap(), b"public head");
}

#[test]
fn provider_deadline_kills_and_reaps_a_hanging_provider() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("provider.pid");
    let mut provider = Command::new("/bin/sh");
    // Keep stdin unread as well as stdout open.
    provider
        .args([
            "-c",
            "printf '%s' \"$$\" > \"$1\"; exec /bin/sleep 15",
            "provider",
        ])
        .arg(&pidfile);
    let start = Instant::now();
    assert!(run_provider(
        &mut provider,
        &[b'x'; PROTOCOL_LIMIT],
        Duration::from_millis(500)
    )
    .is_none());
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_reaped(&pidfile);
}

#[test]
#[cfg(target_os = "linux")]
fn provider_deadline_holds_when_an_escaped_descendant_keeps_stdout_open() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("provider.pid");
    let escaped = dir.path().join("escaped.pid");
    let mut provider = Command::new("/bin/sh");
    provider
        .args([
            "-c",
            r#"printf '%s' "$$" > "$1"
/usr/bin/setsid /bin/sh -c 'printf "%s" "$$" > "$1"; exec /bin/sleep 15' escaped "$2" &
exit 0
"#,
            "provider",
        ])
        .arg(&pidfile)
        .arg(&escaped);
    let start = Instant::now();
    let result = run_provider(&mut provider, b"request\n\n", Duration::from_millis(500));
    let elapsed = start.elapsed();
    let holder: libc::pid_t = std::fs::read_to_string(escaped).unwrap().parse().unwrap();
    // The test owns the escaped process, which is outside the provider's group.
    assert_eq!(unsafe { libc::kill(holder, libc::SIGKILL) }, 0);
    assert!(result.is_none());
    assert!(elapsed < Duration::from_secs(5));
    assert_reaped(&pidfile);
}

fn assert_reaped(pidfile: &Path) {
    let pid: libc::pid_t = std::fs::read_to_string(pidfile).unwrap().parse().unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

#[test]
fn refresh_resolves_the_explicit_ref_and_persists_only_fixed_access_errors() {
    use crate::personal::git;
    use crate::service::store::PersonalSnapshot;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let db_path = root.join("library.db");
    let service = ServeConfig {
        db_path: db_path.clone(),
        cache_dir: root.join("cache"),
        ..ServeConfig::from_env()
    };
    let library = Library {
        store: Store::open(&db_path).unwrap(),
        config: ApplicationConfig {
            data: root.into(),
            service,
            retain_commits_per_repo: None,
        },
    };
    super::super::private(&library.locks()).unwrap();
    let repo = library.register("owner/name", None).unwrap();
    let url = "https://github.com/owner/name.git";
    let sources = SourceArgs::default();
    let denied = |_: &Path, args: &[&str]| {
        assert_eq!(args, ["ls-remote", "--symref", "--", url, "HEAD"]);
        anonymous_then_authenticated(|authenticated| {
            let stderr = if authenticated {
                format!("fatal: Authentication failed for '{url}'\nprovider diagnostic {SECRET}")
            } else {
                "remote: Repository not found.\n".into()
            };
            Ok(output(false, b"", stderr.as_bytes()))
        })
    };
    assert_eq!(
        library
            .refresh_with_remote(&repo, None, &sources, None, denied)
            .unwrap_err()
            .to_string(),
        ACCESS_ERROR
    );
    assert!(library.store.personal_history(&repo.id).unwrap().is_empty());

    let upstream = root.join("upstream");
    std::fs::create_dir(&upstream).unwrap();
    git(&upstream, &["init", "--initial-branch=main"]).unwrap();
    let commit = |message| {
        git(
            &upstream,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                message,
            ],
        )
        .unwrap();
        git(&upstream, &["rev-parse", "HEAD"]).unwrap()
    };
    let snapshot = |id: &str, commit: &str| {
        let retained = root.join(id);
        std::fs::create_dir(&retained).unwrap();
        let map = retained.join("map.json");
        std::fs::write(&map, "{}").unwrap();
        std::fs::write(
            retained.join("selection.json"),
            serde_json::to_vec(&sources).unwrap(),
        )
        .unwrap();
        library.store.personal_begin(&repo.id, id).unwrap();
        library
            .store
            .personal_finish(
                id,
                Ok(&PersonalSnapshot {
                    id: id.into(),
                    repository: repo.id.clone(),
                    commit: commit.into(),
                    map_path: map,
                    source_path: retained,
                    indexed_at: String::new(),
                }),
            )
            .unwrap();
    };
    let first = commit("first");
    snapshot("first", &first);
    let refspec = format!("+refs/heads/main:{REMOTE_HEAD}");
    let local_remote = |dir: &Path, args: &[&str]| match args {
        ["ls-remote", "--symref", "--", remote, "HEAD"] if *remote == url => {
            Ok(b"ref: refs/heads/main\tHEAD\n".to_vec())
        }
        ["clone", "--no-checkout", "--", remote, "checkout"] if *remote == url => {
            git(
                dir,
                &[
                    "clone",
                    "--no-checkout",
                    "--",
                    upstream.to_str().unwrap(),
                    "checkout",
                ],
            )?;
            Ok(Vec::new())
        }
        ["fetch", "--no-tags", "--no-write-fetch-head", "--", remote, spec]
            if *remote == url && *spec == refspec =>
        {
            git(
                dir,
                &[
                    "fetch",
                    "--no-tags",
                    "--no-write-fetch-head",
                    "--",
                    upstream.to_str().unwrap(),
                    spec,
                ],
            )?;
            Ok(Vec::new())
        }
        _ => panic!("unexpected Git arguments: {args:?}"),
    };
    assert_eq!(
        library
            .refresh_with_remote(&repo, None, &sources, None, local_remote)
            .unwrap()
            .id,
        "first"
    );
    let checkout = library.repo_dir(&repo).join("checkout");
    // A stale FETCH_HEAD must have no effect on selection after a refresh.
    let fetch_head = checkout.join(".git/FETCH_HEAD");
    let stale = format!("{first}\t\tbranch 'main' of fixture\n");
    std::fs::write(&fetch_head, &stale).unwrap();
    let second = commit("second");
    snapshot("second", &second);
    assert_eq!(
        library
            .refresh_with_remote(&repo, None, &sources, None, local_remote)
            .unwrap()
            .id,
        "second"
    );
    assert_eq!(std::fs::read_to_string(fetch_head).unwrap(), stale);
    assert_eq!(
        library
            .refresh_with_remote(&repo, None, &sources, None, denied)
            .unwrap_err()
            .to_string(),
        ACCESS_ERROR
    );
    assert_eq!(library.snapshot(&repo, None, None).unwrap().id, "second");
    assert_eq!(
        library.store.personal_attempt(&repo.id).unwrap(),
        Some(("failed".into(), Some(ACCESS_ERROR.into())))
    );
    for path in [db_path, root.join("library.db-wal")] {
        let bytes = std::fs::read(path).unwrap();
        assert!(!bytes
            .windows(SECRET.len())
            .any(|bytes| bytes == SECRET.as_bytes()));
    }
}
