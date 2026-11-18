//! Anonymous-first GitHub transport and an ephemeral Git credential helper.
use anyhow::{bail, ensure, Context, Result};
use std::{
    ffi::{OsStr, OsString},
    io::{Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

pub const ACCESS_ERROR: &str = "GitHub access failed; configure a Git credential helper or gh auth login with repository access, then retry";
const TRANSPORT_ERROR: &str = "GitHub transport failed";
const LIMIT: usize = 64 * 1024;
const DEADLINE: Duration = Duration::from_secs(15);
pub(super) const HEAD_REF: &str = "refs/remotes/tolmap/HEAD";

// Only the user's helper session is inherited, never Git configuration injection,
// tracing, askpass programs, or unrelated application secrets.
const SESSION: &[&str] = &[
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "GPG_TTY",
    "GNUPGHOME",
    "PASSWORD_STORE_DIR",
    "PASSWORD_STORE_GPG_OPTS",
];
const GH: &[&str] = &["GH_TOKEN", "GITHUB_TOKEN", "GH_HOST", "GH_CONFIG_DIR"];

fn absolute_path(value: &OsStr) -> OsString {
    std::env::join_paths(std::env::split_paths(value).filter(|p| p.is_absolute()))
        .expect("filtered PATH entries remain valid")
}

fn session(command: &mut Command, gh: bool) {
    for key in SESSION.iter().chain(if gh { GH } else { &[] }) {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.env(
        "PATH",
        absolute_path(&std::env::var_os("PATH").unwrap_or_default()),
    );
}

fn provider(program: &str, gh: bool) -> Command {
    let mut command = Command::new(program);
    command.env_clear().current_dir("/");
    session(&mut command, gh);
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/bin/false")
        .env("SSH_ASKPASS", "/bin/false")
        .env("GCM_INTERACTIVE", "never");
    command
}

struct ProviderChild(Child);
impl Drop for ProviderChild {
    fn drop(&mut self) {
        // Also kill descendants when a provider exits but leaves a pipe open.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

fn nonblocking(fd: i32) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
        ACCESS_ERROR
    );
    Ok(())
}

// No reader thread or blocking wait_with_output: even a setsid descendant holding
// stdout open cannot extend the deadline. All protocol buffers have fixed bounds.
fn exchange(command: &mut Command, input: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    ensure!(input.len() <= LIMIT, ACCESS_ERROR);
    // An empty PATH means the current directory to execvp, rather than no
    // search directories. Do not spawn a provider when filtering removed all.
    ensure!(
        Path::new(command.get_program()).is_absolute()
            || !command
                .get_envs()
                .any(|(key, value)| key == "PATH" && value.is_some_and(OsStr::is_empty)),
        ACCESS_ERROR
    );
    let end = Instant::now() + timeout;
    let mut child = ProviderChild(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|_| anyhow::anyhow!(ACCESS_ERROR))?,
    );
    let mut stdin = child.0.stdin.take();
    let mut stdout = child.0.stdout.take().unwrap();
    nonblocking(stdin.as_ref().unwrap().as_raw_fd())?;
    nonblocking(stdout.as_raw_fd())?;
    let mut sent = 0;
    let mut output = Vec::new();
    let mut eof = false;
    let mut status = None;
    loop {
        ensure!(Instant::now() < end, ACCESS_ERROR);
        if let Some(pipe) = stdin.as_mut() {
            match pipe.write(&input[sent..]) {
                Ok(n) => sent += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                    stdin.take();
                }
                Err(_) => bail!(ACCESS_ERROR),
            }
            if sent == input.len() {
                stdin.take();
            }
        }
        if !eof {
            let mut buffer = [0; 4096];
            match stdout.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(n) => {
                    ensure!(output.len() + n <= LIMIT, ACCESS_ERROR);
                    output.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => bail!(ACCESS_ERROR),
            }
        }
        if status.is_none() {
            status = child
                .0
                .try_wait()
                .map_err(|_| anyhow::anyhow!(ACCESS_ERROR))?;
        }
        if eof {
            if let Some(status) = status {
                return Ok(if status.success() { output } else { Vec::new() });
            }
        }
        let mut fds = [
            libc::pollfd {
                fd: if eof { -1 } else { stdout.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stdin.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLOUT,
                revents: 0,
            },
        ];
        let millis = end
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(20) as i32;
        unsafe {
            libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis);
        }
    }
}

fn credential(output: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(output).ok()?;
    let mut username = None;
    let mut password = None;
    for line in text.lines().take_while(|s| !s.is_empty()) {
        let (key, value) = line.split_once('=')?;
        if value.contains(['\r', '\0']) {
            return None;
        }
        match key {
            "username" => username = Some(value),
            "password" => password = Some(value),
            _ => {}
        }
    }
    let (username, password) = (username?, password?);
    if username.is_empty() || password.is_empty() {
        return None;
    }
    Some(format!("username={username}\npassword={password}\n\n").into_bytes())
}

fn lookup(input: &[u8], git: &mut Command, gh: &mut Command, timeout: Duration) -> Result<Vec<u8>> {
    if let Some(value) = credential(&exchange(git, input, timeout)?) {
        return Ok(value);
    }
    credential(&exchange(gh, input, timeout)?).context(ACCESS_ERROR)
}

/// Hidden Git helper entry point. `store` and `erase` deliberately do nothing.
pub fn helper(operation: &str) -> Result<()> {
    if operation != "get" {
        return Ok(());
    }
    let mut input = Vec::new();
    std::io::stdin()
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut input)
        .map_err(|_| anyhow::anyhow!(ACCESS_ERROR))?;
    ensure!(input.len() <= LIMIT, ACCESS_ERROR);
    // Git owns the request. Restrict this bridge to the one supported provider.
    let text = std::str::from_utf8(&input).map_err(|_| anyhow::anyhow!(ACCESS_ERROR))?;
    ensure!(
        text.lines().any(|l| l == "protocol=https") && text.lines().any(|l| l == "host=github.com"),
        ACCESS_ERROR
    );
    let output = lookup(
        &input,
        provider("git", false).args(["credential", "fill"]),
        provider("gh", true).args(["auth", "git-credential", "get"]),
        DEADLINE,
    )?;
    std::io::stdout()
        .write_all(&output)
        .map_err(|_| anyhow::anyhow!(ACCESS_ERROR))
}

fn command(root: &Path, authenticated: bool, executable: &Path) -> Result<Command> {
    let mut command = crate::source::git(root);
    command.args([
        "-c",
        "credential.helper=",
        "-c",
        "core.askPass=",
        "-c",
        "credential.interactive=false",
    ]);
    if authenticated {
        session(&mut command, true);
        // The shell snippet contains only our executable path, never credentials.
        let exe = executable
            .to_str()
            .context(ACCESS_ERROR)?
            .replace('\'', "'\\''");
        command.arg("-c").arg(format!(
            "credential.helper=!'{}' personal-github-credential",
            exe
        ));
    }
    Ok(command)
}

fn refused(output: &Output) -> bool {
    let message = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    [
        "authentication failed",
        "could not read username",
        "could not read password",
        "repository not found",
        "repository not found.",
        "requested url returned error: 401",
        "requested url returned error: 403",
        "requested url returned error: 404",
    ]
    .iter()
    .any(|s| message.contains(s))
}

fn remote(root: &Path, args: &[&OsStr]) -> Result<Output> {
    remote_with(
        root,
        args,
        &std::env::current_exe().map_err(|_| anyhow::anyhow!(ACCESS_ERROR))?,
    )
}

fn remote_with(root: &Path, args: &[&OsStr], executable: &Path) -> Result<Output> {
    let output = command(root, false, executable)?
        .args(args)
        .output()
        .map_err(|_| anyhow::anyhow!(TRANSPORT_ERROR))?;
    if output.status.success() {
        return Ok(output);
    }
    ensure!(refused(&output), TRANSPORT_ERROR);
    let output = command(root, true, executable)?
        .args(args)
        .output()
        .map_err(|_| anyhow::anyhow!(ACCESS_ERROR))?;
    ensure!(output.status.success(), ACCESS_ERROR);
    Ok(output)
}

pub(super) fn acquire(root: &Path, path: &Path, url: &str) -> Result<PathBuf> {
    let observed = remote(
        root,
        &[
            "ls-remote".as_ref(),
            "--symref".as_ref(),
            "--".as_ref(),
            url.as_ref(),
            "HEAD".as_ref(),
        ],
    )?;
    let advertised = std::str::from_utf8(&observed.stdout).context(TRANSPORT_ERROR)?;
    let branch = advertised
        .lines()
        .find_map(|line| {
            line.strip_prefix("ref: ")
                .and_then(|s| s.strip_suffix("\tHEAD"))
        })
        .context("GitHub default branch is unavailable")?;
    ensure!(
        branch.starts_with("refs/heads/") && !branch.contains(char::is_whitespace),
        "invalid GitHub default branch"
    );
    let refspec = format!("+{branch}:{HEAD_REF}");
    if path.exists() {
        remote(
            root,
            &[
                "-C".as_ref(),
                path.as_os_str(),
                "fetch".as_ref(),
                "--no-tags".as_ref(),
                "origin".as_ref(),
                refspec.as_ref(),
            ],
        )?;
    } else {
        // No --filter or --depth: retained source must never trigger lazy reads.
        remote(
            root,
            &[
                "clone".as_ref(),
                "--no-checkout".as_ref(),
                "--".as_ref(),
                url.as_ref(),
                path.as_os_str(),
            ],
        )?;
        let source = format!(
            "refs/remotes/origin/{}",
            branch.strip_prefix("refs/heads/").unwrap()
        );
        let output = crate::source::git(path)
            .args(["update-ref", HEAD_REF, &source])
            .output()?;
        ensure!(output.status.success(), TRANSPORT_ERROR);
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn path_and_credential_bounds() {
        assert_eq!(
            absolute_path(OsStr::new(".:/usr/bin:relative::/bin")),
            "/usr/bin:/bin"
        );
        assert!(credential(b"password=secret\n\n").is_none());
        assert!(credential(b"username=u\npassword=\n\n").is_none());
        assert_eq!(
            credential(b"protocol=https\nusername=u\npassword=s\n\n"),
            Some(b"username=u\npassword=s\n\n".to_vec())
        );
        let mut producer = Command::new("/bin/sh");
        producer.args([
            "-c",
            "while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done",
        ]);
        assert_eq!(
            exchange(&mut producer, b"\n", Duration::from_secs(2))
                .unwrap_err()
                .to_string(),
            ACCESS_ERROR
        );
    }

    #[test]
    fn hanging_provider_is_killed_and_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "printf '%s' $$ > \"$1\"; exec /bin/sleep 30",
                "provider",
            ])
            .arg(&pid_file);
        let start = Instant::now();
        assert_eq!(
            exchange(&mut command, b"\n", Duration::from_millis(300))
                .unwrap_err()
                .to_string(),
            ACCESS_ERROR
        );
        assert!(start.elapsed() < Duration::from_secs(3));
        let pid: i32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn escaped_stdout_holder_cannot_extend_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("escaped");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "setsid /bin/sh -c 'printf \"%s\" $$ > \"$1\"; exec /bin/sleep 30' escaped \"$1\" & exit 0", "provider"]).arg(&pid_file);
        let start = Instant::now();
        let result = exchange(&mut command, b"\n", Duration::from_millis(500));
        // The escaped process is deliberately outside the provider group; clean
        // it up explicitly after proving it cannot keep exchange blocked.
        let pid: i32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        assert_eq!(result.unwrap_err().to_string(), ACCESS_ERROR);
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn anonymous_git_is_isolated_and_retry_has_only_bridge_configuration() {
        let root = Path::new("/tmp");
        let exe = Path::new("/tmp/has ' quote/tolmap");
        let anonymous = command(root, false, exe).unwrap();
        let env: std::collections::BTreeMap<_, _> = anonymous.get_envs().collect();
        assert_eq!(
            env.get(OsStr::new("PATH")),
            Some(&Some(OsStr::new("/usr/bin:/bin")))
        );
        assert!(!env.contains_key(OsStr::new("HOME")));
        assert!(!env.contains_key(OsStr::new("GH_TOKEN")));
        let authenticated = command(root, true, exe).unwrap();
        let args: Vec<_> = authenticated
            .get_args()
            .map(|s| s.to_string_lossy())
            .collect();
        assert!(args
            .iter()
            .any(|s| s
                == "credential.helper=!'/tmp/has '\\'' quote/tolmap' personal-github-credential"));
    }

    #[test]
    fn only_authentication_or_not_found_refusals_retry() {
        for message in [
            "fatal: Authentication failed",
            "fatal: could not read Username",
            "remote: Repository not found.",
            "fatal: requested URL returned error: 403",
        ] {
            let output = Command::new("/bin/sh")
                .args(["-c", "printf '%s' \"$1\" >&2; exit 128", "git", message])
                .output()
                .unwrap();
            assert!(refused(&output));
        }
        for message in [
            "Could not resolve host",
            "SSL certificate problem",
            "Connection timed out",
            "requested URL returned error: 500",
        ] {
            let output = Command::new("/bin/sh")
                .args(["-c", "printf '%s' \"$1\" >&2; exit 128", "git", message])
                .output()
                .unwrap();
            assert!(!refused(&output));
        }
    }

    #[test]
    fn full_clone_and_fetch_use_explicit_ref_even_with_stale_fetch_head() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let run = |args: &[&str]| {
            let output = crate::source::git(&source).args(args).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        run(&["init", "--initial-branch=main"]);
        run(&[
            "-c",
            "user.name=Synthetic",
            "-c",
            "user.email=synthetic@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "first",
        ]);
        let first = run(&["rev-parse", "HEAD"]);
        let checkout = dir.path().join("checkout");
        acquire(dir.path(), &checkout, source.to_str().unwrap()).unwrap();
        assert!(!checkout.join(".git/shallow").exists());
        assert!(!checkout
            .join(".git/objects/pack")
            .read_dir()
            .unwrap()
            .any(|e| e
                .unwrap()
                .path()
                .extension()
                .is_some_and(|s| s == "promisor")));
        run(&[
            "-c",
            "user.name=Synthetic",
            "-c",
            "user.email=synthetic@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "second",
        ]);
        let second = run(&["rev-parse", "HEAD"]);
        acquire(dir.path(), &checkout, source.to_str().unwrap()).unwrap();
        std::fs::write(
            checkout.join(".git/FETCH_HEAD"),
            format!("{first}\t\tstale\n"),
        )
        .unwrap();
        let output = crate::source::git(&checkout)
            .args(["rev-parse", HEAD_REF])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), second);
    }

    #[test]
    fn transport_retries_once_only_after_refusal_and_discards_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic-transport");
        let calls = dir.path().join("calls");
        // A Git shell alias exercises real command construction without a
        // transport, network, inherited environment mutation or real helpers.
        let alias = format!("alias.synthetic=!{}", path.display());
        let args: &[&OsStr] = &["-c".as_ref(), alias.as_ref(), "synthetic".as_ref()];
        let exe = Path::new("/synthetic/tolmap");
        script(
            &path,
            &format!(
                r#"
helpers=$(/usr/bin/git config --get-all credential.helper)
printf 'call\n' >> '{}'
case "$helpers" in
  *personal-github-credential*) printf 'authenticated\n'; exit 0;;
esac
[ -z "$GH_TOKEN$GITHUB_TOKEN$GH_HOST$HOME" ] || exit 99
printf 'fatal: Authentication failed: synthetic-secret\n' >&2
exit 128
"#,
                calls.display()
            ),
        );
        let output = remote_with(dir.path(), args, exe).unwrap();
        assert_eq!(output.stdout, b"authenticated\n");
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 2);
        std::fs::remove_file(&calls).unwrap();
        script(
            &path,
            &format!(
                "printf 'call\\n' >> '{}'\nprintf 'DNS failed: synthetic-secret\\n' >&2\nexit 128",
                calls.display()
            ),
        );
        assert_eq!(
            remote_with(dir.path(), args, exe).unwrap_err().to_string(),
            TRANSPORT_ERROR
        );
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);
        script(
            &path,
            "printf 'fatal: Repository not found: synthetic-secret\\n' >&2; exit 128",
        );
        assert_eq!(
            remote_with(dir.path(), args, exe).unwrap_err().to_string(),
            ACCESS_ERROR
        );
    }

    #[test]
    fn missing_program_returns_fixed_error() {
        let mut missing = Command::new("/nonexistent/tolmap-synthetic-provider");
        assert_eq!(
            exchange(&mut missing, b"\n", DEADLINE)
                .unwrap_err()
                .to_string(),
            ACCESS_ERROR
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("provider");
        script(&path, "printf 'secret-diagnostic' >&2; exit 1");
        assert!(exchange(&mut Command::new(path), b"\n", DEADLINE)
            .unwrap()
            .is_empty());
    }
}
