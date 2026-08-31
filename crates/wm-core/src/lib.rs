//! Domain model for driving IBM webMethods installations without the shipped UIs.
//!
//! The installer and the Update Manager are both Java applications whose
//! unattended interface is a Java `.properties` script plus a set of
//! command-line switches. Everything here is a typed model of that interface,
//! reverse-engineered from `sagInstaller.jar` (`com.wm.distman.install`) and the
//! `com.webmethods.fixinstall.*` OSGi bundles of Update Manager 12.0 — see
//! `docs/installer-protocol.md` and `docs/sum-protocol.md`.
//!
//! The point of the crate is that the two products fail late and unhelpfully: a
//! missing prerequisite surfaces an hour into a download, a missing
//! `adminPassword` exits with code 30 after the licence prompt. Everything that
//! can be decided statically — dependency closure, script validity, mandatory
//! base products — is decided here, before a process is started.

pub mod catalog;
pub mod database;
pub mod deps;
pub mod diag;
pub mod fix;
pub mod fixes;
pub mod install;
pub mod instance;
pub mod inventory;
pub mod password;
pub mod profile;
pub mod resolve;
pub mod runner;
pub mod script;
pub mod sdc;
pub mod sum;
pub mod tree;

mod error;

pub use error::{Error, Result};
