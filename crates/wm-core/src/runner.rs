//! Running the installer and Update Manager.
//!
//! Two very different shapes of work live here. Inventory queries finish in
//! seconds and are run synchronously. An installation or an image build runs for
//! the better part of an hour, so it is started as a detached job that writes to
//! a log and an exit-code file, and polled afterwards — a tool call must not
//! block for an hour.
//!
//! The environment matters as much as the arguments. `-debug` is deprecated and
//! sends its diagnostics to **stderr**, which is why piping stdout to a file
//! yields a log containing nothing but the final failure; `-debugLvl` with
//! `-debugFile` is the usable form. And on hosts whose CPUID is reported
//! inconsistently — common under a hypervisor that masks features — the bundled
//! OpenJ9 aborts inside the JIT before any installer code runs, which
//! `TR_DisableCPUDetectionTest` suppresses.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::{Error, Result};

/// Environment applied to installer and Update Manager processes.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    /// Scratch directory. The installer downloads before assembling, so this
    /// needs roughly twice the image size free.
    pub tmpdir: Option<PathBuf>,
    /// Value for `SAG_JAVA_OPTIONS`, injected into the bundled JVM's command line.
    pub java_options: Option<String>,
    /// Set `TR_DisableCPUDetectionTest=1` to stop OpenJ9's JIT aborting when the
    /// host reports inconsistent CPU features.
    pub disable_cpu_detection_test: bool,
    /// Extra variables, e.g. the `$NAME$` placeholders a script refers to.
    pub extra: Vec<(String, String)>,
    /// Text piped to the process on stdin.
    ///
    /// Update Manager's console wizard takes its *values* from a script but
    /// still renders one page at a time and reads a keystroke to advance. With
    /// stdin closed that read hits EOF and it aborts with
    /// "Terminating IBM webMethods Update Manager exit code:-1", so a run driven
    /// by a script still has to supply page advances.
    pub stdin_feed: Option<String>,
    /// Variable names an argument may reference as `$NAME` without the value
    /// ever being written to disk.
    ///
    /// A detached job runs from a wrapper script, so any secret passed as an
    /// argument would be persisted in it. Naming the variable here instead
    /// emits `"$NAME"` in the wrapper, which the shell expands from the
    /// environment this process already has.
    pub passthrough: Vec<String>,
}

impl Environment {
    /// The variables this environment contributes.
    pub fn vars(&self) -> Vec<(String, String)> {
        let mut vars = Vec::new();
        if let Some(dir) = &self.tmpdir {
            vars.push(("TMPDIR".to_string(), dir.display().to_string()));
        }
        if let Some(options) = &self.java_options {
            vars.push(("SAG_JAVA_OPTIONS".to_string(), options.clone()));
        }
        if self.disable_cpu_detection_test {
            vars.push(("TR_DisableCPUDetectionTest".to_string(), "1".to_string()));
        }
        vars.extend(self.extra.iter().cloned());
        vars
    }
}

/// Outcome of a synchronous run.
#[derive(Debug, Clone, Serialize)]
pub struct Output {
    /// Process exit code, or `None` if it was killed by a signal.
    pub exit_code: Option<i32>,
    /// Merged stdout and stderr.
    pub output: String,
    /// Whether the process was killed for exceeding its timeout.
    pub timed_out: bool,
}

impl Output {
    /// Whether the process exited zero.
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Run a command to completion, merging stderr into stdout.
///
/// `timeout` bounds the wait. Neither product is reliably quick even for a
/// read-only query — Update Manager checks for a self-update on the way in — and
/// a tool call that never returns is worse than one that reports a timeout.
pub fn run(
    program: &Path,
    args: &[String],
    env: &Environment,
    timeout: Duration,
) -> Result<Output> {
    let dir = std::env::temp_dir().join(format!("wm-core-run-{}", unique_suffix()));
    fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
    // The child needs to find its own directory to publish progress into; it
    // cannot derive it, because the id is minted here.
    let mut env = env.clone();
    env.extra
        .push(("WM_JOB_DIR".to_string(), dir.display().to_string()));
    let env = &env;

    let log = dir.join("output.log");
    let file = fs::File::create(&log).map_err(|e| Error::io(&log, e))?;
    let errors = file.try_clone().map_err(|e| Error::io(&log, e))?;

    let stdin = match &env.stdin_feed {
        Some(text) => {
            let feed = dir.join("stdin");
            fs::write(&feed, text).map_err(|e| Error::io(&feed, e))?;
            Stdio::from(fs::File::open(&feed).map_err(|e| Error::io(&feed, e))?)
        }
        None => Stdio::null(),
    };

    let mut child = Command::new(program)
        .args(args)
        .envs(env.vars())
        .stdin(stdin)
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(errors))
        .spawn()
        .map_err(|e| Error::Exec(format!("cannot run {}: {e}", program.display())))?;

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => {
                let _ = fs::remove_dir_all(&dir);
                return Err(Error::Exec(format!(
                    "waiting on {}: {e}",
                    program.display()
                )));
            }
        }
    };

    let mut output = fs::read_to_string(&log).unwrap_or_default();
    let _ = fs::remove_dir_all(&dir);
    let Some(status) = status else {
        output.push_str(&format!(
            "\n[wm-core] killed after {}s without finishing",
            timeout.as_secs()
        ));
        return Ok(Output {
            exit_code: None,
            output,
            timed_out: true,
        });
    };
    Ok(Output {
        exit_code: status.code(),
        output,
        timed_out: false,
    })
}

/// A detached long-running run.
#[derive(Debug, Clone, Serialize)]
pub struct Job {
    /// Identifier, also the job directory name.
    pub id: String,
    /// Directory holding the wrapper, log and exit code.
    pub dir: PathBuf,
    /// Combined output log.
    pub log: PathBuf,
    /// The command line, for the record.
    pub command: String,
}

/// State of a job.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JobState {
    /// Still running.
    Running,
    /// Finished with this exit code.
    Finished {
        /// Process exit code.
        exit_code: i32,
    },
}

/// Start a detached job under `jobs_dir`.
///
/// The command is written to a wrapper script rather than passed to a shell, so
/// arguments containing spaces or quotes survive intact.
pub fn spawn(
    jobs_dir: &Path,
    label: &str,
    program: &Path,
    args: &[String],
    env: &Environment,
) -> Result<Job> {
    let id = format!("{label}-{}", unique_suffix());
    let dir = jobs_dir.join(&id);
    fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;

    // The child needs to find its own directory to publish progress into; it
    // cannot derive it, because the id is minted here.
    let mut env = env.clone();
    env.extra
        .push(("WM_JOB_DIR".to_string(), dir.display().to_string()));
    let env = &env;

    let log = dir.join("output.log");
    let exit_file = dir.join("exit_code");
    let wrapper = dir.join("run.sh");

    let mut script = String::from("#!/bin/sh\n");
    for (key, value) in env.vars() {
        script.push_str(&format!("export {key}={}\n", shell_quote(&value)));
    }
    let redirect = match &env.stdin_feed {
        Some(text) => {
            let feed = dir.join("stdin");
            fs::write(&feed, text).map_err(|e| Error::io(&feed, e))?;
            format!("< {}", shell_quote(&feed.display().to_string()))
        }
        None => "< /dev/null".to_string(),
    };
    script.push_str(&format!(
        "{} {} {} >> {} 2>&1\necho $? > {}\n",
        shell_quote(&program.display().to_string()),
        args.iter()
            .map(|a| quote_arg(a, &env.passthrough))
            .collect::<Vec<_>>()
            .join(" "),
        redirect,
        shell_quote(&log.display().to_string()),
        shell_quote(&exit_file.display().to_string()),
    ));
    fs::write(&wrapper, script).map_err(|e| Error::io(&wrapper, e))?;
    restrict(&wrapper);

    // `setsid` detaches the job from this process group so it survives the MCP
    // server exiting. Where it is unavailable, a plain child is close enough.
    let (launcher, launch_args) = if which("setsid") {
        (
            "setsid",
            vec!["sh".to_string(), wrapper.display().to_string()],
        )
    } else {
        ("sh", vec![wrapper.display().to_string()])
    };
    Command::new(launcher)
        .args(&launch_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::Exec(format!("cannot start job {id}: {e}")))?;

    let command = format!("{} {}", program.display(), args.join(" "));
    Ok(Job {
        id,
        dir,
        log,
        command,
    })
}

/// Whether a job has finished, and with what code.
pub fn job_state(dir: &Path) -> JobState {
    let exit_file = dir.join("exit_code");
    match fs::read_to_string(&exit_file) {
        Ok(text) => match text.trim().parse::<i32>() {
            Ok(exit_code) => JobState::Finished { exit_code },
            // The file exists but is not yet complete: the shell is mid-write.
            Err(_) => JobState::Running,
        },
        Err(_) => JobState::Running,
    }
}

/// The last `lines` lines of a job's log.
pub fn tail(log: &Path, lines: usize) -> Result<String> {
    if !log.is_file() {
        return Ok(String::new());
    }
    let text = fs::read_to_string(log).map_err(|e| Error::io(log, e))?;
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    Ok(all[start..].join("\n"))
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// Wrap a value in single quotes for `sh`, escaping any embedded single quote.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Quote one argument, letting `$NAME` through when `NAME` is a declared
/// passthrough variable so its value stays out of the wrapper.
fn quote_arg(value: &str, passthrough: &[String]) -> String {
    if let Some(name) = value.strip_prefix('$') {
        if passthrough.iter().any(|n| n == name) {
            return format!("\"${name}\"");
        }
    }
    shell_quote(value)
}

/// Make a file readable only by its owner. Best effort: a job that runs is
/// better than one refused because permissions could not be tightened.
fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// A suffix unique across processes, threads and calls within a millisecond.
///
/// A timestamp alone is not enough: two calls in the same millisecond produce
/// the same directory, and the first to finish deletes the second's output.
fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        now_millis(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_survive_shell_expansion() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("with space"), "'with space'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn environment_emits_only_what_is_set() {
        let empty = Environment::default();
        assert!(empty.vars().is_empty());

        let env = Environment {
            tmpdir: Some(PathBuf::from("/var/tmp/wm")),
            java_options: Some("-Xshareclasses:none".into()),
            disable_cpu_detection_test: true,
            extra: vec![("WM_ADMIN_PASSWORD".into(), "secret".into())],
            stdin_feed: None,
            passthrough: Vec::new(),
        };
        let vars = env.vars();
        assert!(vars.contains(&("TMPDIR".into(), "/var/tmp/wm".into())));
        assert!(vars.contains(&("TR_DisableCPUDetectionTest".into(), "1".into())));
        assert_eq!(vars.len(), 4);
    }

    #[test]
    fn passthrough_arguments_are_not_written_out() {
        let names = vec!["WM_EMPOWER_KEY".to_string()];
        assert_eq!(quote_arg("$WM_EMPOWER_KEY", &names), "\"$WM_EMPOWER_KEY\"");
        // Anything not declared is quoted literally, including a lookalike.
        assert_eq!(quote_arg("$OTHER", &names), "'$OTHER'");
        assert_eq!(quote_arg("plain", &names), "'plain'");
    }

    #[test]
    fn a_passthrough_secret_stays_out_of_the_wrapper() {
        let base = std::env::temp_dir().join(format!("wm-core-secret-{}", unique_suffix()));
        let env = Environment {
            passthrough: vec!["WM_TEST_SECRET".to_string()],
            ..Environment::default()
        };
        let job = spawn(
            &base,
            "secret",
            Path::new("/bin/echo"),
            &["$WM_TEST_SECRET".into()],
            &env,
        )
        .expect("spawn");
        let wrapper = fs::read_to_string(job.dir.join("run.sh")).expect("wrapper");
        assert!(
            wrapper.contains("\"$WM_TEST_SECRET\""),
            "reference, not value"
        );
        assert!(
            !wrapper.contains("WM_TEST_SECRET="),
            "no assignment written"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn runs_a_command_and_reports_its_code() {
        let out = run(
            Path::new("/bin/sh"),
            &["-c".into(), "echo hi; exit 3".into()],
            &Environment::default(),
            Duration::from_secs(30),
        )
        .expect("spawn");
        assert_eq!(out.exit_code, Some(3));
        assert!(out.output.contains("hi"));
        assert!(!out.success());
        assert!(!out.timed_out);
    }

    #[test]
    fn stdin_can_be_fed() {
        let env = Environment {
            stdin_feed: Some("first\nsecond\n".to_string()),
            ..Environment::default()
        };
        let out = run(
            Path::new("/bin/sh"),
            &["-c".into(), "read a; read b; echo \"$a-$b\"".into()],
            &env,
            Duration::from_secs(30),
        )
        .expect("spawn");
        assert!(out.output.contains("first-second"), "got {:?}", out.output);
    }

    #[test]
    fn a_command_that_overruns_is_killed() {
        let out = run(
            Path::new("/bin/sh"),
            &["-c".into(), "echo started; sleep 60".into()],
            &Environment::default(),
            // Generous, because the assertion is about the deadline being
            // honoured at all, not about how tight it is on a loaded machine.
            Duration::from_secs(3),
        )
        .expect("spawn");
        assert!(out.timed_out);
        assert_eq!(out.exit_code, None);
        assert!(
            out.output.contains("started"),
            "output written before the kill is kept"
        );
    }

    #[test]
    fn a_detached_job_reports_running_then_finished() {
        let base = std::env::temp_dir().join(format!("wm-core-job-{}", unique_suffix()));
        let job = spawn(
            &base,
            "test",
            Path::new("/bin/sh"),
            &["-c".into(), "echo working; exit 7".into()],
            &Environment::default(),
        )
        .expect("spawn");

        // Poll briefly: the wrapper writes the exit code as its last action.
        let mut state = job_state(&job.dir);
        for _ in 0..200 {
            if matches!(state, JobState::Finished { .. }) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
            state = job_state(&job.dir);
        }
        assert!(
            matches!(state, JobState::Finished { exit_code: 7 }),
            "got {state:?}"
        );
        assert!(tail(&job.log, 10).expect("tail").contains("working"));
        let _ = fs::remove_dir_all(&base);
    }
}

/// How a console-driven run is paced.
#[derive(Debug, Clone)]
pub struct Console {
    /// How long the output must stay unchanged before this is taken for a
    /// prompt waiting on input.
    pub quiet: Duration,
    /// Safety stop: give up after this many advances without the process
    /// exiting, rather than looping forever on a page that never progresses.
    pub max_advances: usize,
    /// What to send at each pause. An empty line accepts the displayed default,
    /// which at Update Manager's navigation prompt is `N` for Next.
    pub answer: String,
}

impl Default for Console {
    fn default() -> Self {
        // Update Manager is slow between pages — it contacts the update service
        // on the way in — so the quiet period has to be generous, or an advance
        // is sent into a page that is still rendering.
        Self {
            quiet: Duration::from_secs(8),
            max_advances: 200,
            answer: "\n".to_string(),
        }
    }
}

/// Run a program that insists on a terminal, advancing its pages automatically.
///
/// Update Manager reads its *values* from a `-readScript` file but still renders
/// one page at a time and waits for a keystroke to advance. Measured against
/// 12.0.0.0008, neither a closed stdin, nor newlines on a pipe, nor newlines
/// written up front through `script(1)` will do: the answers have to arrive on a
/// terminal, *after* the prompt is on screen. Anything written earlier is
/// consumed before the page exists and the wizard then aborts on EOF with
/// `Terminating IBM webMethods Update Manager exit code:-1`.
///
/// So the child gets a real pty and advances are paced by watching its output:
/// once nothing has been written for [`Console::quiet`], it is taken to be
/// waiting and one answer is sent. Because every value comes from the script,
/// the answer never has to be matched to a question — which is what makes this
/// robust where prompt-scraping breaks on a reworded prompt.
#[cfg(unix)]
pub fn run_console(
    program: &Path,
    args: &[String],
    env: &Environment,
    console: &Console,
    timeout: Duration,
) -> Result<Output> {
    use std::io::{Read as _, Write as _};
    use std::os::unix::process::CommandExt as _;

    let (master, slave) = open_pty()?;
    // Three identical messages here were indistinguishable in a log; name the
    // descriptor each clone was for.
    let clone = |which: &str| {
        slave
            .try_clone()
            .map_err(|e| Error::Exec(format!("cannot duplicate the pty for {which}: {e}")))
    };
    let child_in = clone("stdin")?;
    let child_out = clone("stdout")?;
    let child_err = clone("stderr")?;

    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.vars())
        // A terminal-driven wizard reads TERM; a dumb one keeps the transcript
        // free of cursor movement we would otherwise have to strip.
        .env("TERM", "dumb")
        .stdin(Stdio::from(child_in))
        .stdout(Stdio::from(child_out))
        .stderr(Stdio::from(child_err));

    // SAFETY: this closure runs in the forked child between the stdio fds being
    // installed and `exec`. It calls only async-signal-safe functions. `setsid`
    // detaches the child from this process group so it can acquire a
    // controlling terminal; `TIOCSCTTY` on descriptor 0 — the pty slave, already
    // dup'd into place by the standard library — makes that pty the controlling
    // terminal, which is what `isatty` and the wizard's reader require.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command
        .spawn()
        .map_err(|e| Error::Exec(format!("cannot run {}: {e}", program.display())))?;
    // The parent must not hold the slave open, or reads on the master never see
    // end-of-file once the child exits.
    drop(slave);

    set_nonblocking(&master)?;
    let mut master_file = std::fs::File::from(master);

    let deadline = Instant::now() + timeout;
    let mut transcript = String::new();
    let mut buffer = [0u8; 8192];
    let mut last_change = Instant::now();
    let mut advances = 0usize;
    let mut stop = None;

    let status = loop {
        let mut read_any = false;
        loop {
            match master_file.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    transcript.push_str(&String::from_utf8_lossy(&buffer[..n]));
                    read_any = true;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                // Once the child is gone the master reports EIO rather than EOF.
                Err(_) => break,
            }
        }
        if read_any {
            last_change = Instant::now();
        }

        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Err(e) => {
                return Err(Error::Exec(format!(
                    "waiting on {}: {e}",
                    program.display()
                )));
            }
            Ok(None) => {}
        }
        if Instant::now() >= deadline {
            stop = Some(format!(
                "killed after {}s without finishing",
                timeout.as_secs()
            ));
            break None;
        }
        if !read_any && last_change.elapsed() >= console.quiet {
            if advances >= console.max_advances {
                stop = Some(format!(
                    "stopped after {advances} advances with no progress"
                ));
                break None;
            }
            let _ = master_file.write_all(console.answer.as_bytes());
            let _ = master_file.flush();
            advances += 1;
            last_change = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(150));
    };

    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // A pty echoes input and terminates lines with CRLF; normalise so callers
    // and pattern matching see ordinary text.
    let transcript = transcript.replace("\r\n", "\n");

    match status {
        Some(status) => Ok(Output {
            exit_code: status.code(),
            output: transcript,
            timed_out: false,
        }),
        None => {
            let mut output = transcript;
            output.push_str(&format!("\n[wm-core] {}", stop.unwrap_or_default()));
            Ok(Output {
                exit_code: None,
                output,
                timed_out: true,
            })
        }
    }
}

/// Allocate a pseudo-terminal pair.
#[cfg(unix)]
fn open_pty() -> Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::{FromRawFd, OwnedFd};

    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: `openpty` writes two open descriptors into the out-parameters on
    // success and touches nothing else; the trailing name, termios and winsize
    // arguments are documented as optional and ignored when null.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(Error::Exec(format!(
            "cannot allocate a pseudo-terminal: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: both descriptors were just produced by `openpty`, are open, and
    // are not owned anywhere else, so transferring ownership here is sound.
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

/// Put a descriptor into non-blocking mode so the read loop can poll it.
#[cfg(unix)]
fn set_nonblocking(fd: &std::os::fd::OwnedFd) -> Result<()> {
    use std::os::fd::AsRawFd as _;

    // SAFETY: `fd` is a live descriptor for as long as the borrow lasts, and
    // both calls only read or replace its file-status flags.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(Error::Exec(format!(
            "fcntl: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: as above; `flags` came from `F_GETFL` on this same descriptor.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(Error::Exec(format!(
            "fcntl: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}
