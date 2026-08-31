//! MCP server for IBM webMethods Update Manager.
//!
//! Update Manager is usually automated by driving its console wizard through a
//! pseudo-terminal, because it refuses to run without a TTY and ignores piped
//! answers. That is unnecessary: it accepts `-readScript <file>`, where the file
//! is a `.properties` list whose keys are the wizard's own input fields. This
//! server generates those scripts, runs them, and reads back the results
//! Update Manager leaves in `bin/result.json` — which are base64-encoded, hence
//! useless to `tail`.
//!
//! Configuration comes from the environment:
//!
//! | Variable | Meaning |
//! |---|---|
//! | `WM_SUM_HOME` | Update Manager root, the directory holding `bin/UpdateManagerCMD.sh` |
//! | `WM_HOME` | default webMethods installation to act on |
//! | `WM_EMPOWER_USER` / `WM_EMPOWER_KEY` | IBM credentials, referenced by name and never written to disk |
//! | `WM_JOBS_DIR` | where long-running jobs keep their logs |

mod native;
mod tools;

use std::io;

fn main() -> io::Result<()> {
    tools::server().run()
}
