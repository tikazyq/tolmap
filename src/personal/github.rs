//! GitHub transport and the read-only Git credential helper bridge.
use anyhow::{anyhow, bail, Result};
use std::{
    ffi::{OsStr, OsString},
    io::{self, BufRead, Read, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            process::CommandExt,
        },
    },
    path::Path,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

const ACCESS_ERROR: &str = "GitHub access denied or repository not found. Configure a Git credential helper or run gh auth login, and check repository access.";
const GIT_ERROR: &str = "GitHub Git operation failed";
const PROVIDER_PATH: &str = "TOLMAP_PERSONAL_PROVIDER_PATH";
const PROVIDER_HOME: &str = "TOLMAP_PERSONAL_PROVIDER_HOME";
const SESSION_ENV: &[&str] = &[
    "USER",
    "LOGNAME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
    "GPG_AGENT_INFO",
    "GPG_TTY",
    "GNUPGHOME",
    "GNOME_KEYRING_CONTROL",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "PASSWORD_STORE_DIR",
];
const GH_ENV: &[&str] = &["GH_TOKEN", "GITHUB_TOKEN", "GH_HOST", "GH_CONFIG_DIR"];
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(15);
const PROTOCOL_LIMIT: usize = 16 * 1024;

pub(super) fn remote_git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    anonymous_then_authenticated(|authenticated| {
        let mut command = crate::source::git(dir);
        command
            // Keep libcurl from loading the user's .netrc alongside the bridge.
            .env("HOME", "/dev/null")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .args(["-c", "credential.helper=", "-c", "core.askPass="]);
        if authenticated {
            // The network Git keeps its existing PATH. Only providers run from
            // the user's PATH, after removing empty and relative entries.
            let path = std::env::var_os("PATH")
                .and_then(|path| absolute_path(&path))
                .unwrap_or_default();
            command
                .envs(inherited(SESSION_ENV))
                .envs(inherited(GH_ENV))
                .env(PROVIDER_HOME, std::env::var_os("HOME").unwrap_or_default())
                .env(PROVIDER_PATH, path)
                .arg("-c")
                .arg(helper_setting(&std::env::current_exe()?))
                .args(["-c", "http.followRedirects=false"]);
        }
        command.args(args).output()
    })
}

fn anonymous_then_authenticated(run: impl Fn(bool) -> io::Result<Output>) -> Result<Vec<u8>> {
    let first = run(false).map_err(|_| anyhow!(GIT_ERROR))?;
    if first.status.success() {
        return Ok(first.stdout);
    }
    if !access_refused(&first.stderr) {
        bail!(GIT_ERROR);
    }
    let retry = run(true).map_err(|_| anyhow!(GIT_ERROR))?;
    if retry.status.success() {
        Ok(retry.stdout)
    } else if access_refused(&retry.stderr) {
        bail!(ACCESS_ERROR);
    } else {
        bail!(GIT_ERROR);
    }
}

fn access_refused(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr).lines().any(|line| {
        let line = line.trim_end();
        line == "remote: Repository not found."
            || line == "remote: Write access to repository not granted."
            || (line.starts_with("fatal: repository '") && line.ends_with("' not found"))
            || line.starts_with("fatal: Authentication failed for '")
            || (line.starts_with("fatal: could not read Username for '")
                && line.ends_with(": terminal prompts disabled"))
            || (line.starts_with("fatal: unable to access '")
                && (line.ends_with("The requested URL returned error: 401")
                    || line.ends_with("The requested URL returned error: 404")))
    })
}

fn helper_setting(exe: &Path) -> OsString {
    // A Git shell helper appends the operation to this command. Quote the
    // absolute executable even when the installed path contains shell syntax.
    let mut setting = b"credential.helper=!'".to_vec();
    for &byte in exe.as_os_str().as_bytes() {
        if byte == b'\'' {
            setting.extend_from_slice(b"'\\''");
        } else {
            setting.push(byte);
        }
    }
    setting.extend_from_slice(b"' personal-github-credential");
    OsString::from_vec(setting)
}

fn inherited(keys: &'static [&'static str]) -> impl Iterator<Item = (&'static str, OsString)> {
    keys.iter()
        .filter_map(|&key| std::env::var_os(key).map(|value| (key, value)))
}

fn absolute_path(path: &OsStr) -> Option<OsString> {
    let paths: Vec<_> = std::env::split_paths(path)
        .filter(|p| p.is_absolute())
        .collect();
    (!paths.is_empty()).then(|| std::env::join_paths(paths).expect("split PATH entries"))
}

/// Entry point for the hidden CLI command. Git interprets empty output as
/// absence of a credential. The bridge emits no diagnostics.
pub(super) fn credential_helper(operation: &str) {
    if operation != "get" {
        return;
    }
    let Some(request) = github_request(io::stdin().lock()) else {
        return;
    };
    let Some(path) = std::env::var_os(PROVIDER_PATH)
        .or_else(|| std::env::var_os("PATH"))
        .and_then(|path| absolute_path(&path))
    else {
        return;
    };
    let mut git = provider("git", &path);
    git.args(["-c", "core.askPass=", "credential", "fill"]);
    let mut gh = provider("gh", &path);
    gh.envs(inherited(GH_ENV))
        .env("GH_PROMPT_DISABLED", "1")
        .args(["auth", "git-credential", "get"]);
    let supports_authtype = fields(&request).any(|line| line == b"capability[]=authtype");
    for mut command in [git, gh] {
        let response = run_provider(&mut command, &request, PROVIDER_TIMEOUT)
            .and_then(|out| credential_response(&out, supports_authtype));
        if let Some(response) = response {
            let _ = io::stdout().lock().write_all(&response);
            return;
        }
    }
}

fn provider(program: &str, path: &OsStr) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir("/")
        .env_clear()
        .envs(inherited(SESSION_ENV))
        .env("PATH", path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never");
    if let Some(home) = std::env::var_os(PROVIDER_HOME)
        .or_else(|| std::env::var_os("HOME"))
        .filter(|home| !home.is_empty())
    {
        command.env("HOME", home);
    }
    command
}

fn fields(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|&b| b == b'\n')
        .take_while(|line| !line.is_empty())
}

fn github_request(input: impl BufRead) -> Option<Vec<u8>> {
    let mut input = input.take(PROTOCOL_LIMIT as u64 + 1);
    let mut request = Vec::new();
    let mut line = Vec::new();
    let (mut protocol, mut host) = (false, false);
    let mut received = 0;
    loop {
        line.clear();
        let len = input.read_until(b'\n', &mut line).ok()?;
        received += len;
        if received > PROTOCOL_LIMIT {
            return None;
        }
        if len == 0 || line == b"\n" {
            request.push(b'\n');
            return (protocol && host).then_some(request);
        }
        let field = line.strip_suffix(b"\n").unwrap_or(&line);
        if field.contains(&0) || field.contains(&b'\r') {
            return None;
        }
        let (key, _) = field.split_at(field.iter().position(|&b| b == b'=')?);
        match key {
            b"protocol" if !protocol && field == b"protocol=https" => protocol = true,
            b"host" if !host && field == b"host=github.com" => host = true,
            // `url` could overwrite the fields already checked above.
            b"protocol" | b"host" | b"url" => return None,
            b"path" | b"username" | b"wwwauth[]" | b"capability[]" => (),
            _ => continue,
        }
        request.extend_from_slice(field);
        request.push(b'\n');
    }
}

fn credential_response(output: &[u8], supports_authtype: bool) -> Option<Vec<u8>> {
    // Keep provider diagnostics and unrecognized output out of Git's parser.
    let (mut username, mut password, mut authtype, mut credential) = (None, None, None, None);
    for line in fields(output) {
        let Some(index) = line.iter().position(|&b| b == b'=') else {
            continue;
        };
        let value = &line[index + 1..];
        if value.is_empty() || value.iter().any(|&b| b == 0 || b == b'\r') {
            continue;
        }
        match &line[..index] {
            b"username" => username = Some(line),
            b"password" => password = Some(line),
            b"authtype" => authtype = Some(line),
            b"credential" => credential = Some(line),
            _ => (),
        }
    }
    let mut response = Vec::new();
    if let (true, Some(authtype), Some(credential)) = (supports_authtype, authtype, credential) {
        response.extend_from_slice(b"capability[]=authtype\n");
        response.extend_from_slice(authtype);
        response.push(b'\n');
        response.extend_from_slice(credential);
    } else {
        response.extend_from_slice(username?);
        response.push(b'\n');
        response.extend_from_slice(password?);
    }
    response.extend_from_slice(b"\n\n");
    Some(response)
}

fn nonblocking(fd: RawFd) -> io::Result<()> {
    // These are newly created parent ends of the provider pipes.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn run_provider(command: &mut Command, request: &[u8], timeout: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let result = (|| {
        nonblocking(stdin.as_ref()?.as_raw_fd()).ok()?;
        nonblocking(stdout.as_ref()?.as_raw_fd()).ok()?;
        let mut pending = request;
        let mut output = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            if stdin.is_none() && stdout.is_none() {
                if let Some(status) = child.try_wait().ok()? {
                    return Some((status, output));
                }
                std::thread::sleep(remaining.min(Duration::from_millis(10)));
                continue;
            }
            // Keep the child unreaped until both ends close. A descendant can
            // hold a pipe after the child exits, even after leaving its group.
            let mut fds = [
                libc::pollfd {
                    fd: stdin.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                    events: libc::POLLOUT,
                    revents: 0,
                },
                libc::pollfd {
                    fd: stdout.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let ready = unsafe {
                libc::poll(
                    fds.as_mut_ptr(),
                    2,
                    remaining.as_millis().max(1) as libc::c_int,
                )
            };
            if ready < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return None;
            }
            if let Some(pipe) = stdin.as_mut().filter(|_| fds[0].revents != 0) {
                match pipe.write(pending) {
                    Ok(n) if n == pending.len() => stdin = None,
                    Ok(n) => pending = &pending[n..],
                    Err(e) if e.kind() == io::ErrorKind::BrokenPipe => stdin = None,
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => return None,
                }
            }
            if let Some(pipe) = stdout.as_mut().filter(|_| fds[1].revents != 0) {
                match pipe.read(&mut buffer) {
                    Ok(0) => stdout = None,
                    Ok(n) if output.len() + n <= PROTOCOL_LIMIT => {
                        output.extend_from_slice(&buffer[..n])
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    _ => return None,
                }
            }
        }
    })();
    match result {
        Some((status, output)) => status.success().then_some(output),
        None => {
            // Negative PID addresses the provider's process group. This also
            // works when the unreaped leader has already exited.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = child.wait();
            None
        }
    }
}

#[cfg(test)]
mod tests;
