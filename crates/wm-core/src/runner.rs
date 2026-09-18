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
    /// Variables handed to the spawned process rather than written into the
    /// wrapper, and referenced by name like [`Environment::passthrough`].
    ///
    /// [`Environment::extra`] becomes `export NAME=value` in a file that stays
    /// on disk for the life of the job, which is the right place for a
    /// `$NAME$` placeholder and the wrong place for a credential. A secret
    /// that this process holds but has not got in its own environment — one
    /// read out of the encrypted store — goes here: the child gets it through
    /// its environment, the wrapper holds only the name.
    pub secret_env: Vec<(String, String)>,
    /// Variables removed from the program's environment once the command line
    /// has been expanded.
    ///
    /// A program that takes a secret as an argument does not need it in its
    /// environment as well, and Update Manager copies its whole environment
    /// into `UpdateManager/logs/debug/*.log`, mode 644, at `debugLevel=DEBUG`.
    /// Naming a variable here means the wrapper expands `"$NAME"` into the
    /// argument list and then unsets it, so the value reaches argv and nothing
    /// else. Not for the installer's `$NAME$` placeholders, which are resolved
    /// from the environment by the program itself.
    pub scrub: Vec<String>,
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

    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.vars())
        .stdin(stdin)
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(errors));
    for name in &env.scrub {
        command.env_remove(name);
    }
    let mut child = command
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

    // A secret's name is referenceable exactly like a passthrough variable; the
    // only difference is where the value comes from.
    let mut referenceable = env.passthrough.clone();
    referenceable.extend(env.secret_env.iter().map(|(name, _)| name.clone()));

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
    // The command line is bound with `set --` first, so that a variable named
    // in `scrub` can be unset between its expansion into an argument and the
    // program starting: argv keeps the value, the environment does not.
    script.push_str(&format!(
        "set -- {} {}\n",
        shell_quote(&program.display().to_string()),
        args.iter()
            .map(|a| quote_arg(a, &referenceable))
            .collect::<Vec<_>>()
            .join(" "),
    ));
    if !env.scrub.is_empty() {
        script.push_str(&format!(
            "unset {}\n",
            env.scrub
                .iter()
                .map(|name| shell_quote(name))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    script.push_str(&format!(
        "\"$@\" {} >> {} 2>&1\necho $? > {}\n",
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
        .envs(
            env.secret_env
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
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

/// Everything known about a job, in one read.
///
/// Both servers used to assemble this by hand and put the useful half — the log
/// tail and the diagnoses — only in the structured payload. A client that
/// renders the text block, which is most of them, got "failed with exit code 1,
/// 0 known cause(s)" and had to guess where the log was. The evidence belongs in
/// the sentence a reader actually sees, so [`Report::summary`] puts it there.
#[derive(Debug, Serialize)]
pub struct Report {
    /// The job's identifier.
    pub job_id: String,
    /// Its directory: the log, the wrapper it ran, the exit code.
    pub job_dir: PathBuf,
    /// The log file.
    pub log: PathBuf,
    /// Running, or finished with a code.
    pub state: JobState,
    /// The exit code once there is one.
    pub exit_code: Option<i32>,
    /// The last lines of the log.
    pub tail: String,
    /// How many lines that is.
    pub tail_lines: usize,
    /// Whether the log held more than the tail shows.
    pub tail_truncated: bool,
    /// Progress, while the job publishes any.
    pub progress: Option<crate::progress::Progress>,
    /// Known failures matching the tail and the exit code.
    pub diagnoses: Vec<crate::diag::Diagnosis>,
}

impl Report {
    /// Read the job `id` under `jobs_dir`, diagnosing failures as `tool`.
    pub fn read(jobs_dir: &Path, id: &str, lines: usize, tool: crate::diag::Tool) -> Result<Self> {
        let job_dir = jobs_dir.join(id);
        if !job_dir.is_dir() {
            return Err(Error::NotFound {
                what: "job",
                path: job_dir,
            });
        }
        let log = job_dir.join("output.log");
        let state = job_state(&job_dir);
        let exit_code = match state {
            JobState::Finished { exit_code } => Some(exit_code),
            JobState::Running => None,
        };
        let text = tail(&log, lines)?;
        let total = std::fs::read_to_string(&log).map_or(0, |t| t.lines().count());
        let shown = text.lines().count();
        // A failing run is diagnosed against the whole log, not the tail: the
        // installer prints its own epilogue after the sentence that explains
        // what went wrong, so the evidence is routinely off the end of 40 lines.
        let diagnoses = match exit_code {
            Some(code) if code != 0 => {
                let whole = std::fs::read_to_string(&log).unwrap_or_else(|_| text.clone());
                crate::diag::diagnose(&whole, Some(code), Some(tool))
            }
            _ => Vec::new(),
        };
        Ok(Self {
            job_id: id.to_string(),
            job_dir,
            log,
            state,
            exit_code,
            tail: text,
            tail_lines: shown,
            tail_truncated: total > shown,
            progress: crate::progress::Progress::read(jobs_dir.join(id).as_path()),
            diagnoses,
        })
    }

    /// One block of text carrying the state, the diagnosis and the evidence.
    pub fn summary(&self) -> String {
        let mut out = match self.exit_code {
            None => match &self.progress {
                Some(p) => format!(
                    "{}: {} — {:.0}% ({} of {}), {} elapsed{}",
                    self.job_id,
                    p.phase,
                    p.fraction() * 100.0,
                    crate::progress::human_bytes(p.bytes_done),
                    crate::progress::human_bytes(p.bytes_total),
                    crate::progress::human_time(p.elapsed()),
                    match p.remaining() {
                        Some(left) => format!(", about {} left", crate::progress::human_time(left)),
                        None => String::new(),
                    }
                ),
                None => format!("{}: running", self.job_id),
            },
            Some(0) => format!("{}: finished successfully", self.job_id),
            Some(code) => format!("{}: failed with exit code {code}", self.job_id),
        };
        let failed = self.exit_code.is_some_and(|c| c != 0);
        if failed {
            out.push_str(&format!("\n\njob directory: {}", self.job_dir.display()));
            if self.diagnoses.is_empty() {
                out.push_str(
                    "\n\nNo known failure signature matched. The log tail below is the \
                     evidence; diagnose_log takes a larger log, and the installer's own \
                     -debugFile, if one was written, holds more than this.",
                );
            } else {
                for diagnosis in &self.diagnoses {
                    out.push_str(&format!(
                        "\n\n{} (matched on {})\n  cause:  {}\n  remedy: {}",
                        diagnosis.signature.id,
                        diagnosis.matched_on.join(", "),
                        diagnosis.signature.cause,
                        diagnosis.signature.remedy,
                    ));
                }
            }
        }
        if !self.tail.trim().is_empty() && (failed || self.exit_code.is_none()) {
            out.push_str(&format!(
                "\n\nlast {} line(s) of {}{}:\n{}",
                self.tail_lines,
                self.log.display(),
                if self.tail_truncated {
                    ", truncated"
                } else {
                    ""
                },
                self.tail,
            ));
        }
        out
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
            secret_env: Vec::new(),
            scrub: Vec::new(),
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
    fn a_scrubbed_variable_reaches_the_argument_and_not_the_child() {
        // Update Manager takes the entitlement key as an argument and then
        // copies its whole environment into a world-readable debug log. The
        // wrapper expands the reference into argv and unsets the variable
        // before the program starts: the value arrives once, as an argument.
        let base = std::env::temp_dir().join(format!("wm-core-scrub-{}", unique_suffix()));
        let env = Environment {
            secret_env: vec![("WM_TEST_SCRUB".to_string(), "arrives-once".to_string())],
            scrub: vec!["WM_TEST_SCRUB".to_string()],
            ..Environment::default()
        };
        let job = spawn(
            &base,
            "scrub",
            Path::new("/bin/sh"),
            &[
                "-c".into(),
                "printf '%s|%s' \"$1\" \"${WM_TEST_SCRUB:-unset}\"".into(),
                "probe".into(),
                "$WM_TEST_SCRUB".into(),
            ],
            &env,
        )
        .expect("spawn");
        let wrapper = fs::read_to_string(job.dir.join("run.sh")).expect("wrapper");
        assert!(wrapper.contains("unset 'WM_TEST_SCRUB'"), "{wrapper}");
        assert!(!wrapper.contains("arrives-once"), "{wrapper}");
        for _ in 0..50 {
            if matches!(job_state(&job.dir), JobState::Finished { .. }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        let out = fs::read_to_string(&job.log).unwrap_or_default();
        assert_eq!(out.trim(), "arrives-once|unset", "{out:?}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_synchronous_run_can_scrub_too() {
        let env = Environment {
            secret_env: vec![("WM_TEST_SCRUB_SYNC".to_string(), "value".to_string())],
            scrub: vec!["WM_TEST_SCRUB_SYNC".to_string()],
            ..Environment::default()
        };
        // `run` takes literal arguments and inherits this process's
        // environment; a scrubbed name is removed from the child's.
        // SAFETY: tests in this module do not read this variable concurrently.
        unsafe { std::env::set_var("WM_TEST_SCRUB_SYNC", "value") };
        let out = run(
            Path::new("/bin/sh"),
            &[
                "-c".into(),
                "printf '%s' \"${WM_TEST_SCRUB_SYNC:-unset}\"".into(),
            ],
            &env,
            Duration::from_secs(10),
        )
        .expect("run");
        unsafe { std::env::remove_var("WM_TEST_SCRUB_SYNC") };
        assert_eq!(out.output.trim(), "unset", "{:?}", out.output);
    }

    #[test]
    fn a_stored_secret_reaches_the_job_without_touching_the_wrapper() {
        // The whole reason `secret_env` is not just another `extra`: `extra`
        // becomes an `export NAME=value` line in a file that stays on disk for
        // the life of the job, and an entitlement key is a bearer token.
        let base = std::env::temp_dir().join(format!("wm-core-stored-{}", unique_suffix()));
        let env = Environment {
            secret_env: vec![("WM_TEST_STORED".to_string(), "a-bearer-token".to_string())],
            ..Environment::default()
        };
        let job = spawn(
            &base,
            "stored",
            Path::new("/bin/echo"),
            &["$WM_TEST_STORED".into()],
            &env,
        )
        .expect("spawn");
        let wrapper = fs::read_to_string(job.dir.join("run.sh")).expect("wrapper");
        assert!(
            wrapper.contains("\"$WM_TEST_STORED\""),
            "a secret_env name is referenceable like a passthrough one: {wrapper}"
        );
        assert!(
            !wrapper.contains("a-bearer-token"),
            "the value reached the wrapper: {wrapper}"
        );
        assert!(
            !wrapper.contains("WM_TEST_STORED="),
            "no assignment written: {wrapper}"
        );
        // And it really does arrive: the job echoes what the shell expanded.
        for _ in 0..50 {
            if matches!(job_state(&job.dir), JobState::Finished { .. }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        let out = fs::read_to_string(&job.log).unwrap_or_default();
        assert!(
            out.contains("a-bearer-token"),
            "job did not receive it: {out:?}"
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
    for name in &env.scrub {
        command.env_remove(name);
    }

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
