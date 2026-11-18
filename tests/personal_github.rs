//! Exercise the hidden helper as Git would, using a private synthetic PATH.
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output, Stdio},
};

const SECRET: &str = "tolmap-synthetic-credential-marker";
const REQUEST: &[u8] = b"protocol=https\nhost=github.com\npath=owner/name.git\n";

fn executable(path: &Path, script: &str) {
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn bridge(root: &Path, operation: &str, request: &[u8], git_credential: bool) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tolmap"))
        .args(["personal-github-credential", operation])
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/dev/null")
        .env(
            "TOLMAP_PERSONAL_PROVIDER_PATH",
            format!(":relative:{}::.", root.display()),
        )
        .env("TOLMAP_PERSONAL_PROVIDER_HOME", root)
        .env("DBUS_SESSION_BUS_ADDRESS", "synthetic-session")
        .env("XDG_RUNTIME_DIR", root)
        .env("SSH_AUTH_SOCK", "synthetic-agent")
        .env("GH_TOKEN", SECRET)
        .env("GITHUB_TOKEN", "synthetic-alternate-token")
        .env("GH_HOST", "github.com")
        .env("GIT_TRACE", "excluded")
        .env("PROVIDER_TEST_MODE", "excluded")
        .env("GIT_ASKPASS", "excluded")
        // The fake user's config file selects whether Git can provide a credential.
        .env(
            "XDG_CONFIG_HOME",
            root.join(if git_credential {
                "configured"
            } else {
                "absent"
            }),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if !request.is_empty() {
        child.stdin.take().unwrap().write_all(request).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    output
}

#[test]
fn hidden_helper_protocol_precedence_and_isolated_provider_environments() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let check_env_and_request = r#"
set -eu
[ "$PATH" = "$HOME" ]
[ "$XDG_RUNTIME_DIR" = "$HOME" ]
[ "$DBUS_SESSION_BUS_ADDRESS" = synthetic-session ]
[ "$SSH_AUTH_SOCK" = synthetic-agent ]
[ -z "${GIT_TRACE+x}${PROVIDER_TEST_MODE+x}${GIT_ASKPASS+x}" ]
[ "$GIT_TERMINAL_PROMPT" = 0 ]
IFS= read -r protocol
IFS= read -r host
IFS= read -r path
IFS= read -r end
[ "$protocol" = 'protocol=https' ]
[ "$host" = 'host=github.com' ]
[ "$path" = 'path=owner/name.git' ]
[ -z "$end" ]
"#;
    executable(
        &root.join("git"),
        &format!(
            r#"#!/bin/sh
{check_env_and_request}
[ "$*" = '-c core.askPass= credential fill' ]
[ -z "${{GH_TOKEN+x}}${{GITHUB_TOKEN+x}}${{GH_HOST+x}}" ]
printf '%s\n' git >> "$HOME/calls"
printf '%s\n' 'provider diagnostic' >&2
case "$XDG_CONFIG_HOME" in
  */configured) printf 'protocol=https\nhost=github.com\nusername=git-user\npassword={SECRET}\n\n' ;;
  *) printf 'username=incomplete\n\n' ;;
esac
"#
        ),
    );
    executable(
        &root.join("gh"),
        &format!(
            r#"#!/bin/sh
{check_env_and_request}
[ "$*" = 'auth git-credential get' ]
[ "$GH_TOKEN" = '{SECRET}' ]
[ "$GITHUB_TOKEN" = synthetic-alternate-token ]
[ "$GH_HOST" = github.com ]
printf '%s\n' gh >> "$HOME/calls"
printf '%s\n' "$GH_TOKEN" >&2
printf 'username=gh-user\npassword=%s\n\n' "$GH_TOKEN"
"#
        ),
    );
    let output = bridge(root, "get", REQUEST, true);
    assert_eq!(
        output.stdout,
        format!("username=git-user\npassword={SECRET}\n\n").as_bytes()
    );
    assert_eq!(
        std::fs::read_to_string(root.join("calls")).unwrap(),
        "git\n"
    );

    let output = bridge(root, "get", REQUEST, false);
    assert_eq!(
        output.stdout,
        format!("username=gh-user\npassword={SECRET}\n\n").as_bytes()
    );
    for op in ["store", "erase"] {
        assert!(bridge(root, op, b"", true).stdout.is_empty());
    }
    assert!(
        bridge(root, "get", b"protocol=https\nhost=other.example\n\n", true)
            .stdout
            .is_empty()
    );
    assert!(bridge(
        root,
        "get",
        b"protocol=https\nhost=github.com\nurl=https://other.example\n\n",
        true
    )
    .stdout
    .is_empty());
    let calls = std::fs::read(root.join("calls")).unwrap();
    assert_eq!(calls, b"git\ngit\ngh\n");
    assert!(!calls
        .windows(SECRET.len())
        .any(|bytes| bytes == SECRET.as_bytes()));
}
