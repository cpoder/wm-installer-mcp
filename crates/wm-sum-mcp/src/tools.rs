//! Tool registry for the Update Manager server.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mcp_rt::args::{flag, opt_i32, opt_str, opt_usize, req_str, str_list};
use mcp_rt::{Server, Tool, ToolError, ToolResult};
use serde_json::{json, Value};
use wm_core::diag;
use wm_core::runner::{self, Environment};
use wm_core::sum::{self, Action, FixScript, FixStep, SumCommand};

/// Which product's failure signatures this server's jobs are matched against.
const TOOL: diag::Tool = diag::Tool::UpdateManager;

/// Name of the environment variable holding the entitlement key.
///
/// Jobs reference it by name so the value never reaches the wrapper script.
const KEY_VAR: &str = "WM_EMPOWER_KEY";

/// Build the configured server.
pub fn server() -> Server {
    Server::new("wm-sum", env!("CARGO_PKG_VERSION"))
        .instructions(
            "Drives IBM webMethods Update Manager without its console wizard. `fixes_installed` \
             reads what is patched and needs no credentials. `fix_script_generate` writes an \
             unattended script — one step or a batch — and `fix_run` executes it, returning a \
             job id to poll with `job_status`, which returns a failed run's cause, remedy and \
             log tail in the text. When something fails, `sum_result` decodes the base64 \
             fields of bin/result.json and `sum_locks` clears the stale lock that makes Update \
             Manager exit 211 in silence.\n\n\
             Credentials come from $WM_EMPOWER_USER and $WM_EMPOWER_KEY, or from the \
             encrypted store the installer server's `credential_set` fills; the environment \
             wins where both have a value. Either way the value is referenced by variable \
             name and never written into a script, a wrapper or a log.\n\n\
             An installation registered with the installer server's `install_register` can be \
             named with `install` wherever `install_dir` or `sum_home` is taken — the record \
             carries the Update Manager home that patches it. This server and the installer \
             server agree on where jobs and caches live, so a job id from either is pollable \
             by either.",
        )
        .tool(crate::native::fixes_available())
        .tool(crate::native::fix_apply())
        .tool(crate::native::fix_inspect())
        .tool(crate::native::fixes_download())
        .tool(crate::native::fixes_inventory())
        .tool(crate::native::fixes_parse_metadata())
        .tool(fixes_installed())
        .tool(fix_script_generate())
        .tool(fix_run())
        .tool(sum_locks())
        .tool(sum_result())
        .tool(job_status())
        .tool(diagnose_log())
}

fn sum_home(args: &Value) -> Result<PathBuf, ToolError> {
    // A registered installation records the Update Manager home that patches
    // it, which is the one fact about SUM that is per-installation rather than
    // per-machine.
    let from_registry = opt_str(args, "install")
        .and_then(|name| wm_core::registry::get(&name).ok())
        .and_then(|i| i.sum_home);
    let path = opt_str(args, "sum_home")
        .map(PathBuf::from)
        .or(from_registry)
        .or_else(|| std::env::var("WM_SUM_HOME").map(PathBuf::from).ok())
        .ok_or_else(|| {
            ToolError::invalid(
                "no sum_home given, no registered installation named, and WM_SUM_HOME is not set",
            )
        })?;
    if !path.join("bin").join("UpdateManagerCMD.sh").is_file() {
        return Err(ToolError::invalid(format!(
            "{} does not look like an Update Manager home (no bin/UpdateManagerCMD.sh)",
            path.display()
        )));
    }
    Ok(path)
}

fn install_dir(args: &Value) -> Result<PathBuf, ToolError> {
    if let Some(name) = opt_str(args, "install") {
        return wm_core::registry::get(&name)
            .map(|i| i.wm_home)
            .map_err(ToolError::invalid);
    }
    let given = opt_str(args, "install_dir")
        .or_else(|| std::env::var("WM_HOME").ok())
        .or_else(|| {
            wm_core::config::Defaults::load()
                .ok()
                .and_then(|d| d.install)
        })
        .ok_or_else(|| {
            ToolError::invalid(
                "no installation named: pass install_dir (a path or a registered name) or \
                 install (a registered name), or set $WM_HOME",
            )
        })?;
    // A path or a registered name, as everywhere else.
    wm_core::registry::resolve(&given).map_err(ToolError::invalid)
}

/// Write a generated script to a private scratch file.
///
/// Update Manager only reads scripts from disk, so a tool that generates one on
/// the fly still has to put it somewhere; it is removed once the run returns.
fn scratch_script(script: &FixScript, label: &str) -> Result<PathBuf, ToolError> {
    let dir = jobs_dir().join("scratch");
    std::fs::create_dir_all(&dir)
        .map_err(|e| ToolError::failed(format!("cannot create {}: {e}", dir.display())))?;
    let path = dir.join(format!("{label}-{}.script", std::process::id()));
    script.write(&path).map_err(ToolError::failed)?;
    Ok(path)
}

// Both servers ask `wm_core::config` where things live. This one used to work
// it out itself and ignored WM_STATE_DIR, so a job started here landed
// somewhere the other server's job_status did not look.
use wm_core::config::jobs_dir;

fn fixes_installed() -> Tool {
    Tool::new(
        "fixes_installed",
        "List the fixes installed in a webMethods installation, by running Update Manager's \
         read-only -viewInstalledFixes. Needs no IBM credentials and changes nothing. Runs to \
         completion, so it returns the output directly rather than a job id.",
        json!({
            "type": "object",
            "properties": {
                "install": { "type": "string", "description": "A registered installation (the installer server's install_list shows the names); supplies its path and its recorded sum_home." },
                "sum_home": { "type": "string" },
                "install_dir": { "type": "string", "description": "Installation to inspect; defaults to $WM_HOME." },
                "timeout_seconds": { "type": "integer", "description": "Give up after this long (default 600). Update Manager checks for a self-update before answering, so allow minutes." }
            }
        }),
        Box::new(|args| {
            let sum = sum_home(args)?;
            let target = install_dir(args)?;
            let locks = sum::stale_locks(&sum);
            if !locks.is_empty() {
                return Err(ToolError::failed(format!(
                    "a previous run left {} lock file(s); Update Manager would exit 211 without \
                     explanation. Clear them with sum_locks first: {}",
                    locks.len(),
                    locks
                        .iter()
                        .map(|l| l.path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            // -viewInstalledFixes on its own still launches the interactive
            // wizard; with stdin closed it walks the menus on defaults and
            // answers nothing. The script is the only non-interactive path.
            let script = FixScript::single(FixStep {
                action: Action::ViewInstalled,
                install_dir: target.display().to_string(),
                selected_fixes: Vec::new(),
                image_file: None,
                image_platform: None,
                empower_user: None,
                empower_password_encrypted: None,
                extra: Default::default(),
            });
            let script_path = scratch_script(&script, "view-installed")?;
            let command = SumCommand::read_script(&sum, &script_path, None);
            let timeout =
                Duration::from_secs(opt_usize(args, "timeout_seconds").unwrap_or(600) as u64);
            // A script supplies the values but not the page turns: Update
            // Manager still wants them on a terminal, after each prompt appears.
            let output = runner::run_console(
                &command.program,
                &command.args,
                &Environment::default(),
                &runner::Console::default(),
                timeout,
            )
            .map_err(ToolError::failed)?;
            let _ = std::fs::remove_file(&script_path);
            let diagnoses = if output.success() {
                Vec::new()
            } else {
                diag::diagnose(
                    &output.output,
                    output.exit_code,
                    Some(diag::Tool::UpdateManager),
                )
            };
            Ok(ToolResult::structured(
                match (output.success(), output.timed_out) {
                    (true, _) => format!("{} inspected", target.display()),
                    (false, true) => format!(
                        "Update Manager did not finish within {}s and was killed",
                        timeout.as_secs()
                    ),
                    (false, false) => format!("Update Manager exited {:?}", output.exit_code),
                },
                json!({
                    "exit_code": output.exit_code,
                    "timed_out": output.timed_out,
                    "output": output.output,
                    "diagnoses": diagnoses,
                }),
            ))
        }),
    )
}

/// Build one step from a tool argument object.
fn step_from(args: &Value) -> Result<FixStep, ToolError> {
    let action_name = req_str(args, "action")?;
    let action = Action::parse(&action_name)
        .ok_or_else(|| ToolError::invalid(format!("unknown action {action_name:?}")))?;
    Ok(FixStep {
        action,
        install_dir: install_dir(args)?.display().to_string(),
        selected_fixes: str_list(args, "fixes"),
        image_file: opt_str(args, "image_file"),
        image_platform: opt_str(args, "image_platform"),
        empower_user: opt_str(args, "empower_user")
            .or_else(|| std::env::var("WM_EMPOWER_USER").ok()),
        // Never put a key in the script: Update Manager wants it encrypted and
        // rejects plaintext. fix_run passes it on the command line instead.
        empower_password_encrypted: None,
        extra: Default::default(),
    })
}

fn fix_script_generate() -> Tool {
    Tool::new(
        "fix_script_generate",
        "Generate an unattended Update Manager script. One step, or several in one file — \
         more than one switches on batch mode with numeric key prefixes. Validates first: it \
         catches the create-image-with-no-fixes case that silently produces a launcher-only \
         image, and the batch limit of nine steps.",
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["install_from_empower", "install_from_image", "install_from_cache",
                             "create_image", "view_installed", "view_available",
                             "create_inventory", "uninstall", "revert", "delete_backup"]
                },
                "install": { "type": "string", "description": "A registered installation (the installer server's install_list shows the names); supplies its path and its recorded sum_home." },
                "install_dir": { "type": "string" },
                "fixes": { "type": "array", "items": { "type": "string" }, "description": "Fix names; empty means all applicable." },
                "image_file": { "type": "string" },
                "image_platform": { "type": "string", "description": "e.g. LNXAMD64." },
                "empower_user": { "type": "string", "description": "Defaults to $WM_EMPOWER_USER." },
                "sum_home": { "type": "string" },
                "write_to": { "type": "string", "description": "Also write the script here." }
            }
        }),
        Box::new(|args| {
            let script = FixScript::single(step_from(args)?);
            let problems = script.validate();
            let rendered = script.render();
            if let Some(path) = opt_str(args, "write_to") {
                script.write(Path::new(&path)).map_err(ToolError::failed)?;
            }
            Ok(ToolResult::structured(
                if problems.is_empty() {
                    "script is consistent".to_string()
                } else {
                    format!("{} problem(s): {}", problems.len(), problems.join("; "))
                },
                json!({
                    "script": rendered,
                    "problems": problems,
                    "written_to": opt_str(args, "write_to"),
                }),
            ))
        }),
    )
}

fn fix_run() -> Tool {
    Tool::new(
        "fix_run",
        "Run an Update Manager script (-readScript) as a detached job and return its id. \
         Credentials are taken from $WM_EMPOWER_USER and $WM_EMPOWER_KEY and passed on the \
         command line by reference, so the key is never written into the job's wrapper. \
         Refuses to start when a stale lock is present.",
        json!({
            "type": "object",
            "required": ["script"],
            "properties": {
                "script": { "type": "string", "description": "Path to the script." },
                "install": { "type": "string", "description": "A registered installation (the installer server's install_list shows the names); supplies its path and its recorded sum_home." },
                "sum_home": { "type": "string" },
                "with_credentials": { "type": "boolean", "description": "Pass IBM credentials (default true; set false for offline actions)." }
            }
        }),
        Box::new(|args| {
            let sum = sum_home(args)?;
            let script = req_str(args, "script")?;
            if !Path::new(&script).is_file() {
                return Err(ToolError::invalid(format!("no script at {script}")));
            }
            let locks = sum::stale_locks(&sum);
            if !locks.is_empty() {
                return Err(ToolError::failed(format!(
                    "refusing to start: {} stale lock(s) would make Update Manager exit 211 \
                     silently. Clear them with sum_locks.",
                    locks.len()
                )));
            }

            let mut env = Environment::default();
            let credentials = if flag(args, "with_credentials", true) {
                // Whichever source the value comes from, the command line gets
                // `$WM_EMPOWER_KEY` and never the value: the wrapper the job
                // runs from stays on disk for the life of the job.
                env.secret_env = wm_core::secrets::job_environment();
                let user = std::env::var("WM_EMPOWER_USER")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .or_else(|| wm_core::secrets::lookup(wm_core::secrets::EMPOWER_USER))
                    .ok_or_else(|| {
                        ToolError::invalid(
                            "no IBM account: $WM_EMPOWER_USER is not set and \"empower.user\" \
                             is not in the credential store. Set one, or pass \
                             with_credentials=false.",
                        )
                    })?;
                let have_key = std::env::var_os(KEY_VAR).is_some()
                    || env.secret_env.iter().any(|(name, _)| name == KEY_VAR);
                if !have_key {
                    return Err(ToolError::invalid(format!(
                        "no entitlement key: ${KEY_VAR} is not set and \"empower.key\" is not \
                         in the credential store"
                    )));
                }
                env.passthrough.push(KEY_VAR.to_string());
                Some((user, format!("${KEY_VAR}")))
            } else {
                None
            };

            let command = SumCommand::read_script(
                &sum,
                Path::new(&script),
                credentials.as_ref().map(|(u, p)| (u.as_str(), p.as_str())),
            );
            let job = runner::spawn(&jobs_dir(), "fix", &command.program, &command.args, &env)
                .map_err(ToolError::failed)?;
            Ok(ToolResult::structured(
                format!("Update Manager started as {}", job.id),
                json!({ "job_id": job.id, "job_dir": job.dir, "log": job.log }),
            ))
        }),
    )
}

fn sum_locks() -> Tool {
    Tool::new(
        "sum_locks",
        "Report — and optionally remove — the lock files a previous Update Manager run leaves \
         behind. While they exist the next run exits 211 and prints nothing, which is a common \
         cause of an automation that worked yesterday.",
        json!({
            "type": "object",
            "properties": {
                "install": { "type": "string", "description": "A registered installation (the installer server's install_list shows the names); supplies its path and its recorded sum_home." },
                "sum_home": { "type": "string" },
                "remove": { "type": "boolean", "description": "Delete them (default false). Make sure no Update Manager process is running." }
            }
        }),
        Box::new(|args| {
            let sum = sum_home(args)?;
            let locks = sum::stale_locks(&sum);
            let remove = flag(args, "remove", false);
            let mut removed = Vec::new();
            if remove {
                for lock in &locks {
                    match std::fs::remove_file(&lock.path) {
                        Ok(()) => removed.push(lock.path.display().to_string()),
                        Err(e) => {
                            return Err(ToolError::failed(format!(
                                "cannot remove {}: {e}",
                                lock.path.display()
                            )))
                        }
                    }
                }
            }
            let summary = match (locks.len(), remove) {
                (0, _) => "no lock files present".to_string(),
                (n, false) => format!("{n} lock file(s) present; re-run with remove=true to clear"),
                (n, true) => format!("{n} lock file(s) removed"),
            };
            Ok(ToolResult::structured(
                summary,
                json!({ "locks": locks, "removed": removed }),
            ))
        }),
    )
}

fn sum_result() -> Tool {
    Tool::new(
        "sum_result",
        "Read bin/result.json from the last Update Manager run and decode it. The message and \
         exception fields are base64, so the raw file tells you nothing; this returns the exit \
         code per section along with the decoded text and any matching diagnosis.",
        json!({
            "type": "object",
            "properties": { "sum_home": { "type": "string" } }
        }),
        Box::new(|args| {
            let sum = sum_home(args)?;
            let sections = sum::read_result(&sum).map_err(ToolError::failed)?;
            let combined: String = sections
                .iter()
                .filter_map(|s| {
                    let message = s.message.clone().unwrap_or_default();
                    let exception = s.exception.clone().unwrap_or_default();
                    (!message.is_empty() || !exception.is_empty())
                        .then(|| format!("{message}\n{exception}"))
                })
                .collect::<Vec<_>>()
                .join("\n");
            let worst = sections
                .iter()
                .filter_map(|s| s.exit_code)
                .find(|c| *c != 0);
            let diagnoses = diag::diagnose(&combined, worst, Some(diag::Tool::UpdateManager));
            let summary = sections
                .iter()
                .map(|s| format!("{}: exit {:?}", s.name, s.exit_code))
                .collect::<Vec<_>>()
                .join(", ");
            Ok(ToolResult::structured(
                summary,
                json!({ "sections": sections, "diagnoses": diagnoses }),
            ))
        }),
    )
}

fn job_status() -> Tool {
    Tool::new(
        "job_status",
        "Poll a job: whether it is still running, its progress, its exit code, the matching \
         failure signature when it failed, and the tail of its log. A failed job returns the \
         cause, the remedy and the evidence in the text itself, along with the job directory \
         — there is nothing further to go and find.",
        json!({
            "type": "object",
            "required": ["job_id"],
            "properties": {
                "job_id": { "type": "string" },
                "lines": { "type": "integer", "description": "Log lines to return (default 40)." }
            }
        }),
        Box::new(|args| {
            let id = req_str(args, "job_id")?;
            let report = runner::Report::read(
                &jobs_dir(),
                &id,
                opt_usize(args, "lines").unwrap_or(40),
                TOOL,
            )
            .map_err(|e| ToolError::invalid(e.to_string()))?;
            let structured = serde_json::to_value(&report)
                .map_err(|e| ToolError::failed(format!("cannot serialise the job report: {e}")))?;
            Ok(ToolResult::structured(report.summary(), structured))
        }),
    )
}

fn diagnose_log() -> Tool {
    Tool::new(
        "diagnose_log",
        "Match Update Manager output against known failure signatures: the silent 211 from a \
         stale lock, the rejected plaintext password, the launcher-only image, the token \
         failure behind an authentication error, and the OpenJ9 JIT abort.",
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string" },
                "path": { "type": "string" },
                "exit_code": { "type": "integer" }
            }
        }),
        Box::new(|args| {
            let text = match (opt_str(args, "text"), opt_str(args, "path")) {
                (Some(text), _) => text,
                (None, Some(path)) => std::fs::read_to_string(&path)
                    .map_err(|e| ToolError::failed(format!("cannot read {path}: {e}")))?,
                (None, None) => return Err(ToolError::invalid("give either text or path")),
            };
            let found = diag::diagnose(
                &text,
                opt_i32(args, "exit_code"),
                Some(diag::Tool::UpdateManager),
            );
            let summary = if found.is_empty() {
                "no known signature matched".to_string()
            } else {
                format!(
                    "{} known cause(s): {}",
                    found.len(),
                    found[0].signature.cause
                )
            };
            Ok(ToolResult::structured(
                summary,
                json!({ "diagnoses": found }),
            ))
        }),
    )
}
