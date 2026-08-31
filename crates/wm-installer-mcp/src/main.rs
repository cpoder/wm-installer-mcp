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
