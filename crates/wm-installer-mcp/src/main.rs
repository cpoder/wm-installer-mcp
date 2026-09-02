//! MCP server for the IBM webMethods installer.
//!
//! The shipped installer is a Swing wizard with an unattended mode that is only
//! documented in outline: a `.properties` script, a set of switches, and a
//! product identifier format you can only learn by reading an existing
//! installation. This server exposes that mode as tools — discover products,
//! close the selection over its prerequisites, generate and validate a script,
//! build an image, run the install, and turn a failed log into an action.
//!
//! Configuration comes from the environment:
//!
//! | Variable | Meaning |
//! |---|---|
//! | `WM_INSTALLER_BIN` | the installer, e.g. `/home/cpo/IBM_webM_Install_Linux_x64.bin` |
//! | `WM_HOME` | default reference installation for catalogue lookups |
//! | `WM_JOBS_DIR` | where long-running jobs keep their logs (default `~/.wm-mcp/jobs`) |

mod native;
mod tools;

use std::io;

fn main() -> io::Result<()> {
    // The server spawns itself to run a long install as a detached job; that
    // keeps the tool call non-blocking without a second binary to deploy.
    let args: Vec<String> = std::env::args().collect();
    // `--watch <job-id>` renders a job's progress until it stops. It is the
    // human-facing half of what job_status returns to an agent.
    if let Some(index) = args.iter().position(|a| a == "--watch") {
        let id = args
            .get(index + 1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--watch needs a job id"))?;
        return watch(id);
    }

    if let Some(index) = args.iter().position(|a| a == "--install-job") {
        let spec = args.get(index + 1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "--install-job needs a spec file",
            )
        })?;
        return match native::run_install_job(std::path::Path::new(spec)) {
            Ok(()) => Ok(()),
            Err(message) => {
                eprintln!("install failed: {message}");
                std::process::exit(1);
            }
        };
    }
    tools::server().run()
}

/// Draw a job's progress until it finishes.
fn watch(id: &str) -> io::Result<()> {
    let dir = native::jobs_dir().join(id);
    if !dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no such job: {id} (looked in {})", dir.display()),
        ));
    }
    let mut first = true;
    loop {
        let Some(progress) = wm_core::progress::Progress::read(&dir) else {
            std::thread::sleep(std::time::Duration::from_millis(300));
            continue;
        };
        // Redraw in place: move the cursor back over the last frame rather
        // than scrolling a wall of near-identical screens.
        let frame = progress.render(id, 64);
        if !first {
            print!("\x1b[{}A\x1b[J", frame.lines().count());
        }
        print!("{frame}");
        use io::Write as _;
        io::stdout().flush()?;
        first = false;
        if progress.finished.is_some() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}
