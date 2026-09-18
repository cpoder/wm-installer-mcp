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
        "List the fixes Update Manager has recorded as installed, read straight from its \
         registry (the newest generation under install/fix/profile/…/self.profile): id, \
         version, product, when each was installed. Needs no credentials, no Update Manager \
         and no terminal, and changes nothing. That registry is what Update Manager itself \
         and the IBM fix service consult, so a fix applied natively by fix_apply is not in \
         it. Pass via_sum=true to run Update Manager's own 'View installed fixes' instead, \
         which takes minutes and a pseudo-terminal and needs sum_home.",
        json!({
            "type": "object",
            "properties": {
                "install": { "type": "string", "description": "A registered installation (the installer server's install_list shows the names); supplies its path and its recorded sum_home." },
                "install_dir": { "type": "string", "description": "Installation to inspect; defaults to $WM_HOME." },
                "via_sum": { "type": "boolean", "description": "Run Update Manager instead of reading its registry (default false)." },
                "sum_home": { "type": "string", "description": "Update Manager home, needed with via_sum." },
                "timeout_seconds": { "type": "integer", "description": "With via_sum: give up after this long (default 600). Update Manager checks for a self-update before answering, so allow minutes." }
            }
        }),
        Box::new(|args| {
            let target = install_dir(args)?;
            if !flag(args, "via_sum", false) {
                return fixes_from_registry(&target);
            }
            let sum = sum_home(args)?;
            let locks = sum::stale_locks(&sum, Some(&target));
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
            // Listing installed fixes needs no credentials, and Update Manager
            // copies its environment into its debug log: keep the account out.
            let env = Environment {
                scrub: vec![KEY_VAR.to_string(), "WM_EMPOWER_USER".to_string()],
                ..Environment::default()
            };
            let output = runner::run_console(
                &command.program,
                &command.args,
                &env,
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

/// Update Manager's registry of `target`, rendered for a reader.
fn fixes_from_registry(target: &Path) -> Result<ToolResult, ToolError> {
    let registry = wm_core::fixregistry::read(target).map_err(ToolError::failed)?;
    let Some(registry) = registry else {
        return Ok(ToolResult::structured(
            format!(
                "{} has no Update Manager registry ({}): either nothing was ever patched \
                 with Update Manager, or every fix was applied natively",
                target.display(),
                wm_core::fixregistry::REGISTRY_DIR
            ),
            json!({ "registry": Value::Null, "fixes": [] }),
        ));
    };
    let patches = registry
        .fixes
        .iter()
        .filter(|f| f.is_support_patch())
        .count();
    let mut summary = format!(
        "{} fix(es) and {} support patch(es) recorded by Update Manager in {} (registry \
         generation {}, {} kept)",
        registry.fixes.len() - patches,
        patches,
        target.display(),
        registry.timestamp,
        registry.generations
    );
    for fix in &registry.fixes {
        summary.push_str(&format!(
            "\n  {:<36} {:<20} {}  {}",
            fix.id,
            fix.version,
            fix.installed_at.as_deref().unwrap_or("-"),
            fix.display_name.as_deref().unwrap_or("")
        ));
    }
    Ok(ToolResult::structured(
        summary,
        json!({ "registry": registry, "fixes": registry.fixes }),
    ))
}

/// Build one step from a tool argument object.
/// The `UserInput` keys a step may carry beyond the common ones, and the
/// argument each comes from. Names are Update Manager's, spelled as its
/// `ScriptingSession` registers them.
const EXTRA_KEYS: &[(&str, &str)] = &[
    ("backup_delete_period", "backupDeletePeriod"),
    ("period_type", "periodType"),
    ("install_sp", "installSP"),
    ("sp_key", "spKey"),
    ("diagnoser_key", "diagnoserKey"),
    ("use_ssl", "useSSL"),
];

/// Build one step from a tool argument object. `defaults` supplies what the
/// object leaves out, so a batch names the installation once.
fn step_from(args: &Value, defaults: &Value) -> Result<FixStep, ToolError> {
    let action_name = req_str(args, "action")?;
    let action = Action::parse(&action_name)
        .ok_or_else(|| ToolError::invalid(format!("unknown action {action_name:?}")))?;
    let pick = |key: &str| opt_str(args, key).or_else(|| opt_str(defaults, key));
    let target = if opt_str(args, "install").is_some() || opt_str(args, "install_dir").is_some() {
        install_dir(args)?
    } else {
        install_dir(defaults)?
    };
    let mut extra = std::collections::BTreeMap::new();
    for (arg, key) in EXTRA_KEYS {
        if let Some(value) = pick(arg) {
            extra.insert((*key).to_string(), value);
        }
    }
    Ok(FixStep {
        action,
        install_dir: target.display().to_string(),
        selected_fixes: str_list(args, "fixes"),
        image_file: pick("image_file"),
        image_platform: pick("image_platform"),
        empower_user: pick("empower_user").or_else(|| std::env::var("WM_EMPOWER_USER").ok()),
        // Never put a key in the script: Update Manager wants it encrypted and
        // rejects plaintext. fix_run passes it on the command line instead.
        empower_password_encrypted: None,
        extra,
    })
}

fn fix_script_generate() -> Tool {
    Tool::new(
        "fix_script_generate",
        "Generate an unattended Update Manager script: one step from the top-level arguments, \
         or several from `steps`, which switches on batch mode with numeric key prefixes. \
         Validates first: an install or uninstall step must name its fixes (with an empty \
         selectedFixes Update Manager performs only its self-update and does nothing else), \
         an image step without fixes would produce a launcher-only image, and a batch holds \
         at most nine steps.",
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["install_from_empower", "install_from_image", "install_from_cache",
                             "create_image", "view_installed", "view_available",
                             "create_inventory", "uninstall", "revert", "delete_backup"],
                    "description": "The step's action, when generating a single step."
                },
                "steps": { "type": "array", "items": { "type": "object" }, "description": "Several steps, each an object with the same keys as the top level (action, fixes, image_file, …); the top-level values are the defaults for what a step leaves out." },
                "install": { "type": "string", "description": "A registered installation (the installer server's install_list shows the names); supplies its path and its recorded sum_home." },
                "install_dir": { "type": "string" },
                "fixes": { "type": "array", "items": { "type": "string" }, "description": "Fix names as Update Manager displays them (fixes_available lists them). Required for the install and uninstall actions: an empty selectedFixes is not 'all applicable'." },
                "image_file": { "type": "string" },
                "image_platform": { "type": "string", "description": "e.g. LNXAMD64." },
                "empower_user": { "type": "string", "description": "Defaults to $WM_EMPOWER_USER." },
                "backup_delete_period": { "type": "string", "description": "For delete_backup: how old a backup must be, 0 to 999." },
                "period_type": { "type": "string", "description": "For delete_backup: Days, Weeks or Months." },
                "install_sp": { "type": "string", "description": "true to install a support patch rather than fixes." },
                "sp_key": { "type": "string", "description": "Support patch key." },
                "diagnoser_key": { "type": "string", "description": "Diagnostic collector key." },
                "use_ssl": { "type": "string" },
                "write_to": { "type": "string", "description": "Also write the script here." }
            }
        }),
        Box::new(|args| {
            let steps: Vec<FixStep> = match args.get("steps").and_then(Value::as_array) {
                Some(items) if !items.is_empty() => items
                    .iter()
                    .map(|item| step_from(item, args))
                    .collect::<Result<_, _>>()?,
                _ => vec![step_from(args, args)?],
            };
            let script = FixScript { steps };
            let problems = script.validate();
            let rendered = script.render();
            if let Some(path) = opt_str(args, "write_to") {
                script.write(Path::new(&path)).map_err(ToolError::failed)?;
            }
            Ok(ToolResult::structured(
                if problems.is_empty() {
                    format!("script is consistent ({} step(s))", script.steps.len())
                } else {
                    format!("{} problem(s): {}", problems.len(), problems.join("; "))
                },
                json!({
                    "script": rendered,
                    "steps": script.steps.len(),
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
         Credentials come from $WM_EMPOWER_USER / $WM_EMPOWER_KEY or the credential store; \
         the key reaches Update Manager as a command-line argument by reference, is never \
         written into the job's wrapper, and is removed from the process environment before \
         Update Manager starts, because Update Manager copies its environment into its \
         debug log. It is still visible in the process list while the run lasts. Refuses to \
         start when a stale lock is present.",
        json!({
            "type": "object",
            "required": ["script"],
            "properties": {
                "script": { "type": "string", "description": "Path to the script." },
                "install": { "type": "string", "description": "A registered installation (the installer server's install_list shows the names); supplies its path and its recorded sum_home." },
                "with_credentials": { "type": "boolean", "description": "Pass IBM credentials (default true; set false for offline actions)." }
            }
        }),
        Box::new(|args| {
            let sum = sum_home(args)?;
            let script = req_str(args, "script")?;
            if !Path::new(&script).is_file() {
                return Err(ToolError::invalid(format!("no script at {script}")));
            }
            let target = script_install_dir(Path::new(&script));
            let locks = sum::stale_locks(&sum, target.as_deref());
            if let Some(held) = locks.iter().find(|l| l.held) {
                return Err(ToolError::failed(format!(
                    "refusing to start: an Update Manager process holds {}; wait for it to \
                     finish",
                    held.path.display()
                )));
            }
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
                // Update Manager copies its environment into
                // UpdateManager/logs/debug/*.log, mode 644. The key reaches it
                // as an argument and is unset before it starts.
                env.scrub.push(KEY_VAR.to_string());
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
        "Report — and optionally remove — Update Manager's lock files: the two under its own \
         home, and the one it leaves in the installation it worked on. While they exist the \
         next run exits 211 and prints nothing, which is a common cause of an automation that \
         worked yesterday. Each is checked for a live holder: a lock a running Update Manager \
         holds is reported as held and never removed.",
        json!({
            "type": "object",
            "properties": {
                "install": { "type": "string", "description": "A registered installation (the installer server's install_list shows the names); supplies its path and its recorded sum_home." },
                "install_dir": { "type": "string", "description": "Installation whose SumWorksOnThisDir.lock to include; defaults to $WM_HOME when set." },
                "sum_home": { "type": "string" },
                "remove": { "type": "boolean", "description": "Delete the stale ones (default false)." }
            }
        }),
        Box::new(|args| {
            let sum = sum_home(args)?;
            let target = install_dir(args).ok();
            let locks = sum::stale_locks(&sum, target.as_deref());
            let remove = flag(args, "remove", false);
            let held: Vec<String> = locks
                .iter()
                .filter(|l| l.held)
                .map(|l| l.path.display().to_string())
                .collect();
            let mut removed = Vec::new();
            if remove {
                for lock in locks.iter().filter(|l| !l.held) {
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
            let mut summary = match (locks.len(), remove) {
                (0, _) => "no lock files present".to_string(),
                (n, false) => {
                    format!(
                        "{n} lock file(s) present; re-run with remove=true to clear the stale ones"
                    )
                }
                (_, true) => format!("{} stale lock file(s) removed", removed.len()),
            };
            if !held.is_empty() {
                summary.push_str(&format!(
                    "; {} held by a running Update Manager and left alone: {}",
                    held.len(),
                    held.join(", ")
                ));
            }
            Ok(ToolResult::structured(
                summary,
                json!({ "locks": locks, "removed": removed, "held": held }),
            ))
        }),
    )
}

/// The `installDir` a script names, so the lock it would take in that
/// installation can be checked before Update Manager starts.
fn script_install_dir(script: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(script).ok()?;
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        let key = key.trim();
        (key == "installDir" || key.ends_with(".installDir")).then(|| PathBuf::from(value.trim()))
    })
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
