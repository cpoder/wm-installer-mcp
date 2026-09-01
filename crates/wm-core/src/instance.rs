//! Creating an Integration Server instance natively.
//!
//! This is the work the shipped installer's `ISLauncherInstallPanel` triggers
//! and that `IntegrationServer/instances/is_instance.xml` performs through Ant.
//! Reading that build file is what makes it reproducible: `create` is a sequence
//! of file operations plus one call into Java for the password hash, and the
//! hash format is known — see [`crate::password`].
//!
//! Reproduced here, in the order the Ant file runs them:
//!
//! | Ant target | What it does |
//! |---|---|
//! | `extractTemplate` | unpack `instances/template.zip`, dropping `*.bat` and `support/**` on Unix, then `chmod 755 bin/*.sh` |
//! | `copyCorePackages` | copy the packages named in `is_core_packages.properties` |
//! | `createServerCnfFile` | write `config/server.cnf` with the ports |
//! | `createAdminPassFile` | write `config/installerKeyFile` and `config/changeFlagFile` |
//! | `createSetEnvInstance-sh` | write `bin/setenv_instance.sh` |
//! | `invoke-instance-manager` | wrapper configuration, reproduced from the templates beside the instance |
//!
//! What is left out is deliberate: `notifyAPIGatewayOnInstanceCreate` and
//! `notifyAgileAppsOnInstanceCreate` poke products that may not be installed,
//! and `createJDBCPoolAlias` needs a database that does not exist yet.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{password, Error, Result};

/// Ports and identity of a new instance.
#[derive(Debug, Clone, Serialize)]
pub struct InstanceSpec {
    /// Instance name, e.g. `default`.
    pub name: String,
    /// Primary HTTP port.
    pub primary_port: u16,
    /// HTTPS port.
    pub secure_port: u16,
    /// Diagnostic port.
    pub diagnostic_port: u16,
    /// JMX port the service wrapper exposes.
    pub jmx_port: u16,
    /// Address to bind to; empty means every interface.
    pub bind_address: String,
    /// Namespace locking mode.
    pub lock_mode: String,
    /// Administrator password. Without one the instance starts with no usable
    /// credential and the first login cannot succeed.
    pub admin_password: Option<String>,
    /// Whether that password must be changed at first login.
    pub change_password_on_login: bool,
    /// Packages to copy in beyond the core set.
    pub extra_packages: Vec<String>,
}

impl Default for InstanceSpec {
    fn default() -> Self {
        // The defaults the Ant file declares.
        Self {
            name: "default".to_string(),
            primary_port: 5555,
            secure_port: 5543,
            diagnostic_port: 9999,
            jmx_port: 8075,
            bind_address: String::new(),
            lock_mode: "full".to_string(),
            admin_password: None,
            change_password_on_login: false,
            extra_packages: Vec::new(),
        }
    }
}

/// What creating an instance produced.
#[derive(Debug, Clone, Serialize)]
pub struct Created {
    /// Instance directory.
    pub path: PathBuf,
    /// Files unpacked from the template.
    pub template_files: usize,
    /// Packages copied in.
    pub packages: Vec<String>,
    /// Configuration files written.
    pub wrote: Vec<String>,
    /// Steps that were skipped, and why.
    pub skipped: Vec<String>,
}

/// Create an instance under `<wm_home>/IntegrationServer/instances/<name>`.
pub fn create(wm_home: &Path, spec: &InstanceSpec) -> Result<Created> {
    validate_name(&spec.name)?;
    let instances = wm_home.join("IntegrationServer").join("instances");
    let template = instances.join("template.zip");
    if !template.is_file() {
        return Err(Error::NotFound {
            what: "instance template",
            path: template,
        });
    }
    let dir = instances.join(&spec.name);
    if dir.exists() {
        return Err(Error::Exec(format!(
            "instance {} already exists at {}",
            spec.name,
            dir.display()
        )));
    }

    let mut wrote = Vec::new();
    let mut skipped = Vec::new();

    let template_files = extract_template(&template, &dir)?;
    let packages = copy_core_packages(wm_home, &dir, &spec.extra_packages, &mut skipped)?;

    write_server_cnf(&dir, spec)?;
    wrote.push("config/server.cnf".into());

    match &spec.admin_password {
        Some(secret) => {
            let complaints = password::complaints(secret);
            if !complaints.is_empty() {
                skipped.push(format!(
                    "administrator password written, but the product rules object: {}",
                    complaints.join(", ")
                ));
            }
            let hashed = password::hash(password::DEFAULT_USER, secret)?;
            let flag = if spec.change_password_on_login {
                "Yes"
            } else {
                "No"
            };
            write(&dir.join("config").join("installerKeyFile"), &hashed)?;
            write(&dir.join("config").join("changeFlagFile"), flag)?;
            wrote.push("config/installerKeyFile".into());
            wrote.push("config/changeFlagFile".into());

            // A run of the shipped installer also leaves the pair under
            // IntegrationServer/conf, the installation-wide default a later
            // instance inherits. A natively placed tree has no conf directory
            // at all, so seed it — but never overwrite one that exists, since
            // it may belong to an instance already in use.
            let conf = wm_home.join("IntegrationServer").join("conf");
            let key_file = conf.join("installerKeyFile");
            if key_file.exists() {
                skipped.push(format!(
                    "{} already exists and was left alone",
                    key_file.display()
                ));
            } else {
                write(&key_file, &hashed)?;
                write(&conf.join("changeFlagFile"), flag)?;
                wrote.push("IntegrationServer/conf/installerKeyFile".into());
                wrote.push("IntegrationServer/conf/changeFlagFile".into());
            }
        }
        None => skipped.push(
            "no administrator password given, so config/installerKeyFile was not written; \
             the instance will have no usable credential"
                .into(),
        ),
    }

    let setenv = dir.join("bin").join("setenv_instance.sh");
    write(&setenv, &format!("\nINSTANCE_NAME={}\n", spec.name))?;
    set_mode(&setenv, 0o755);
    wrote.push("bin/setenv_instance.sh".into());

    wrote.extend(write_wrapper_config(
        wm_home,
        &instances,
        &dir,
        spec,
        &mut skipped,
    )?);

    // The template's scripts and wrapper configuration ship with {{TOKEN}}
    // placeholders. Leaving them is not a cosmetic defect: the launcher sources
    // `{{INSTALL_AREA}}/bin/custom_setenv.sh`, finds nothing, and the server
    // never starts.
    let tokens = Tokens::resolve(wm_home, &dir, &spec.name)?;
    let substituted = substitute(&dir, &tokens)?;

    // The template stores every entry as 0644, and the shipped Ant only chmods
    // `*.sh`. The service-wrapper launcher has no extension, so it would be left
    // unreadable as a program — `startup.sh` then reports nothing but
    // "Permission denied" and the server never starts.
    if let Ok(launcher) = service_name(&dir) {
        set_mode(&dir.join("bin").join(&launcher), 0o755);
    }
    if substituted.is_empty() {
        skipped.push("no template tokens found to substitute".into());
    }
    wrote.extend(substituted);

    Ok(Created {
        path: dir,
        template_files,
        packages,
        wrote,
        skipped,
    })
}

/// Instance names end up in file paths, service names and shell variables.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::Malformed("instance name is empty".into()));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(Error::Malformed(format!(
            "instance name {name:?} may only contain letters, digits, '_' and '-'"
        )));
    }
    Ok(())
}

/// Unpack `template.zip`, applying the platform filters the Ant file applies.
fn extract_template(template: &Path, dir: &Path) -> Result<usize> {
    let file = fs::File::open(template).map_err(|e| Error::io(template, e))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| Error::Exec(format!("{} unreadable: {e}", template.display())))?;
    let mut written = 0usize;

    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|e| {
            Error::Exec(format!(
                "cannot read entry {index} of {}: {e}",
                template.display()
            ))
        })?;
        let name = entry.name().to_string();
        if skip_on_this_platform(&name) {
            continue;
        }
        let Some(relative) = safe_relative(&name) else {
            return Err(Error::Exec(format!(
                "template entry escapes the instance: {name:?}"
            )));
        };
        let target = dir.join(&relative);
        if name.ends_with('/') {
            fs::create_dir_all(&target).map_err(|e| Error::io(&target, e))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(|e| {
            Error::Exec(format!(
                "cannot read {name} from {}: {e}",
                template.display()
            ))
        })?;
        fs::write(&target, &bytes).map_err(|e| Error::io(&target, e))?;
        // The Ant file chmods bin/*.sh; do the same for every script placed.
        if name.ends_with(".sh") {
            set_mode(&target, 0o755);
        }
        written += 1;
    }
    Ok(written)
}

/// Entries the Ant file excludes for the platform being installed on.
fn skip_on_this_platform(name: &str) -> bool {
    if cfg!(windows) {
        name.ends_with(".sh")
    } else {
        // `support/**` is Windows tooling and the batch files are unusable.
        name.ends_with(".bat") || name.starts_with("support/")
    }
}

/// The packages every instance carries, from `is_core_packages.properties`.
fn core_packages(instances: &Path) -> Vec<String> {
    let path = instances.join("is_core_packages.properties");
    let Ok(text) = fs::read_to_string(&path) else {
        // The Ant file also hard-codes the list; fall back to it so a tree
        // without the properties file still yields a working instance.
        return DEFAULT_CORE_PACKAGES
            .iter()
            .map(|s| s.to_string())
            .collect();
    };
    text.lines()
        .find_map(|l| l.trim().strip_prefix("core_packages="))
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_else(|| {
            DEFAULT_CORE_PACKAGES
                .iter()
                .map(|s| s.to_string())
                .collect()
        })
}

/// The list as it appears in `is_instance.xml`, used when the properties file
/// is missing.
const DEFAULT_CORE_PACKAGES: &[&str] = &[
    "Default",
    "WmART",
    "WmARTExtDC",
    "WmFlatFile",
    "WmISExtDC",
    "WmPublic",
    "WmRoot",
    "WmTomcat",
    "WmVCS",
    "WmXSLT",
    "WmCloud",
    "WmIntegrationLiveGit",
    "WmIntegrationLiveServer",
    "WmJSONAPI",
    "WmAdmin",
    "WmMonitor",
    "PSFT_E1_Adapter",
];

fn copy_core_packages(
    wm_home: &Path,
    dir: &Path,
    extra: &[String],
    skipped: &mut Vec<String>,
) -> Result<Vec<String>> {
    let source = wm_home.join("IntegrationServer").join("packages");
    let target = dir.join("packages");
    fs::create_dir_all(&target).map_err(|e| Error::io(&target, e))?;

    let instances = wm_home.join("IntegrationServer").join("instances");
    let mut wanted = core_packages(&instances);
    for name in extra {
        if !wanted.contains(name) {
            wanted.push(name.clone());
        }
    }

    let mut copied = Vec::new();
    for name in wanted {
        let from = source.join(&name);
        if !from.is_dir() {
            // The core list names packages that are not in every selection.
            continue;
        }
        copy_tree(&from, &target.join(&name))?;
        copied.push(name);
    }
    if copied.is_empty() {
        skipped.push(format!(
            "no packages copied: {} holds none of them",
            source.display()
        ));
    }
    Ok(copied)
}

/// Copy a directory recursively, preserving executable bits.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).map_err(|e| Error::io(to, e))?;
    let entries = fs::read_dir(from).map_err(|e| Error::io(from, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(from, e))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let kind = entry.file_type().map_err(|e| Error::io(&source, e))?;
        if kind.is_dir() {
            copy_tree(&source, &target)?;
        } else if kind.is_symlink() {
            // Copy the link, not the file it names: the target may be relative
            // to the tree being copied.
            #[cfg(unix)]
            {
                let dest = fs::read_link(&source).map_err(|e| Error::io(&source, e))?;
                let _ = fs::remove_file(&target);
                std::os::unix::fs::symlink(&dest, &target).map_err(|e| Error::io(&target, e))?;
            }
        } else {
            fs::copy(&source, &target).map_err(|e| Error::io(&target, e))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                if let Ok(meta) = fs::metadata(&source) {
                    let _ = fs::set_permissions(
                        &target,
                        fs::Permissions::from_mode(meta.permissions().mode()),
                    );
                }
            }
        }
    }
    Ok(())
}

fn write_server_cnf(dir: &Path, spec: &InstanceSpec) -> Result<()> {
    let mut text = String::from("#Default server.cnf file\n");
    text.push_str(&format!("watt.server.port={}\n", spec.primary_port));
    text.push_str(&format!("watt.server.securePort={}\n", spec.secure_port));
    text.push_str(&format!(
        "watt.server.diagnostic.port={}\n",
        spec.diagnostic_port
    ));
    // The compile line is a template the server fills in at runtime; the two
    // placeholders are positional and must survive verbatim.
    text.push_str("watt.server.compile=javac -classpath {0} -d {1} {2}\n");
    text.push_str("watt.server.extendedSettingsList=watt.server.compile;\n");
    text.push_str(&format!("watt.server.ns.lockingMode={}\n", spec.lock_mode));
    text.push_str(&format!("watt.server.inetaddress={}\n", spec.bind_address));
    write(&dir.join("config").join("server.cnf"), &text)
}

/// Reproduce what the instance manager adds to `configuration/`.
fn write_wrapper_config(
    wm_home: &Path,
    instances: &Path,
    dir: &Path,
    spec: &InstanceSpec,
    skipped: &mut Vec<String>,
) -> Result<Vec<String>> {
    let mut wrote = Vec::new();
    let configuration = dir.join("configuration");
    fs::create_dir_all(&configuration).map_err(|e| Error::io(&configuration, e))?;

    // The service wrapper's licence travels with the installation, not the
    // instance, so it is copied rather than generated.
    let license = instances.join("wrapper-is-license.conf");
    if license.is_file() {
        fs::copy(&license, configuration.join("wrapper-license.conf"))
            .map_err(|e| Error::io(&configuration, e))?;
        wrote.push("configuration/wrapper-license.conf".into());
    } else {
        skipped.push(format!(
            "no {} to copy, so the service wrapper has no licence",
            license.display()
        ));
    }

    let template = instances.join("custom_wrapper.conf.template");
    if template.is_file() {
        let text = fs::read_to_string(&template).map_err(|e| Error::io(&template, e))?;
        let service = service_name(dir).unwrap_or_else(|_| "sagis".to_string());
        let rendered = render_wrapper_template(&text, wm_home, dir, &service, spec.jmx_port);
        write(&configuration.join("custom_wrapper.conf"), &rendered)?;
        wrote.push("configuration/custom_wrapper.conf".into());
    } else {
        skipped.push("no custom_wrapper.conf.template beside the instances directory".into());
    }
    Ok(wrote)
}

/// Render `custom_wrapper.conf` from the shipped template.
///
/// The template is a sample: it carries Windows example paths
/// (`c:\webMethods`, and an instance called `i1`) that the shipped instance
/// manager rewrites, and it omits the dozen settings that manager appends. Left
/// as shipped, the service wrapper resolves its working directory to the sample
/// path and stops before the server starts.
fn render_wrapper_template(
    text: &str,
    wm_home: &Path,
    instance_dir: &Path,
    service: &str,
    jmx_port: u16,
) -> String {
    let home = wm_home.display().to_string();
    let area = instance_dir.display().to_string();

    let mut out = String::with_capacity(text.len() + 1024);
    for line in text.lines() {
        let rewritten = match line.split_once('=') {
            // `wrapper.working.dir` names an instance in the sample; point it at
            // this one rather than translating the sample's name.
            Some((key, _)) if key.trim() == "wrapper.working.dir" => {
                format!("wrapper.working.dir={area}")
            }
            Some((key, value)) if value.contains("c:\\webMethods") => {
                format!("{key}={}", rewrite_sample_path(value, &home))
            }
            _ => line.to_string(),
        };
        out.push_str(&rewritten);
        out.push('\n');
    }

    // What the instance manager appends. Written after the template so a
    // hand-edited template cannot leave the wrapper without them.
    out.push_str("\n# --- resolved for this installation ---\n");
    for line in [
        format!("wrapper.working.dir={area}"),
        "wrapper.console.flush=TRUE".to_string(),
        "wrapper.app.parameter.2=4".to_string(),
        "wrapper.app.parameter.5=-service".to_string(),
        format!("wrapper.app.parameter.6={service}"),
        "wrapper.java.additional.204=-Dlog4j.configurationFile=\"config/log4j2.properties,         .tc.custom.log4j2.properties,config/event-streaming-log4j2.properties,         packages/WmMFT/config/ActiveTransfer_log4j2.properties\""
            .to_string(),
        "wrapper.java.additional.204.stripquotes=TRUE".to_string(),
        format!("wrapper.java.additional.205=-Dcom.sun.management.jmxremote.port={jmx_port}"),
        "wrapper.java.additional.206=-Dcom.sun.management.jmxremote.ssl=false".to_string(),
        "wrapper.java.additional.207=-Dcom.sun.management.jmxremote.authenticate=false".to_string(),
        format!("wrapper.java.classpath.3={home}/common/lib/wm-converters.jar"),
        format!("wrapper.java.library.path.11={area}/lib"),
    ] {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Turn a `c:\webMethods\…` sample path into one under the real installation.
fn rewrite_sample_path(value: &str, home: &str) -> String {
    let replaced = value.replace("c:\\webMethods", home);
    // The sample uses Windows separators throughout; the rest of the value is
    // a path, so normalising them is safe and keeps the file readable.
    if cfg!(windows) {
        replaced
    } else {
        replaced.replace('\\', "/")
    }
}

/// The `{{TOKEN}}` values the shipped instance manager substitutes.
///
/// Every value is discovered from the installation rather than assumed: the JVM
/// directory, the service-wrapper version and the wrapper launcher's own name
/// all vary between releases, and a wrong guess produces a launcher that
/// silently does nothing.
#[derive(Debug, Clone)]
pub struct Tokens {
    values: std::collections::BTreeMap<&'static str, String>,
}

impl Tokens {
    /// Work out every substitution for one instance.
    pub fn resolve(wm_home: &Path, instance_dir: &Path, name: &str) -> Result<Self> {
        let home = wm_home.display().to_string();
        let install_area = instance_dir.display().to_string();
        let java = java_home(wm_home)?;
        let wrapper_version = wrapper_version(wm_home)?;
        let service = service_name(instance_dir)?;
        let release = release_label(wm_home);
        let display = format!("IBM webMethods Integration Server {release} ({name})");

        let mut values = std::collections::BTreeMap::new();
        values.insert("INSTALL_AREA", install_area);
        // In both setenv.sh and wrapper.conf this is joined with
        // SECURITY_LIB_DIR, and the pair resolves to
        // <home>/common/security/ssx/lib.
        values.insert("ROOT_PATH", format!("{home}/common"));
        values.insert("SECURITY_LIB_DIR", "security/ssx/lib".to_string());
        values.insert("JAVA_EXEC", format!("{}/bin/java", java.display()));
        values.insert("JAVA_EXEC_PATH", format!("{}/bin/java", java.display()));
        values.insert(
            "WRAPPER_EXEC",
            format!("{home}/common/bin/wrapper-{wrapper_version}"),
        );
        values.insert("WRAPPER_EXEC_VER", wrapper_version.clone());
        values.insert(
            "WRAPPER_LIB",
            format!("{home}/common/lib/tw-{wrapper_version}"),
        );
        values.insert(
            "STARTUP_JAR",
            format!("{home}/IntegrationServer/lib/wm-isproxy.jar"),
        );
        values.insert("INI_CNF", "/bin/ini.cnf".to_string());
        values.insert("SERVICE_NAME", service);
        values.insert("SERVICE_DISP_NAME", display.clone());
        values.insert("PRODUCT_DISPLAY_NAME", display);
        values.insert(
            "SERVICE_DESCR",
            format!("IBM webMethods Integration Server {release}"),
        );
        values.insert(
            "LIBPATH",
            format!(
                "LD_LIBRARY_PATH={j}/jre/lib/amd64/server:{j}/jre/lib/amd64:${{LD_LIBRARY_PATH}}",
                j = java.display()
            ),
        );
        values.insert("EXPORT_LD_LIB", "export LD_LIBRARY_PATH".to_string());
        Ok(Self { values })
    }

    /// Replace every known token in `text`.
    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (token, value) in &self.values {
            out = out.replace(&format!("{{{{{token}}}}}"), value);
        }
        out
    }

    /// The resolved values, for reporting.
    pub fn values(&self) -> &std::collections::BTreeMap<&'static str, String> {
        &self.values
    }
}

/// The JVM the installation carries, e.g. `<wm_home>/jvm/jvm`.
fn java_home(wm_home: &Path) -> Result<PathBuf> {
    let root = wm_home.join("jvm");
    let entries = fs::read_dir(&root).map_err(|e| Error::io(&root, e))?;
    let mut candidates: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("bin").join("java").is_file())
        .collect();
    candidates.sort();
    candidates.pop().ok_or(Error::NotFound {
        what: "bundled JVM",
        path: root,
    })
}

/// The service-wrapper version, from `common/bin/wrapper-<version>`.
fn wrapper_version(wm_home: &Path) -> Result<String> {
    let bin = wm_home.join("common").join("bin");
    let entries = fs::read_dir(&bin).map_err(|e| Error::io(&bin, e))?;
    let mut versions: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix("wrapper-").map(str::to_string)
        })
        // Only the plain versioned launcher; the tree also holds
        // platform-suffixed variants.
        .filter(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .collect();
    versions.sort_by_key(|v| version_key(v));
    versions.pop().ok_or(Error::NotFound {
        what: "service wrapper",
        path: bin,
    })
}

fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// The wrapper launcher script the template placed, e.g. `sagis121`.
fn service_name(instance_dir: &Path) -> Result<String> {
    let bin = instance_dir.join("bin");
    let entries = fs::read_dir(&bin).map_err(|e| Error::io(&bin, e))?;
    entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("sag") && !n.contains('.'))
        .ok_or(Error::NotFound {
            what: "wrapper launcher script",
            path: bin,
        })
}

/// Release label such as `12.1`, from the Integration Server product record.
fn release_label(wm_home: &Path) -> String {
    crate::catalog::Catalog::load(wm_home)
        .ok()
        .and_then(|catalog| {
            catalog.path_of("integrationServer").map(|p| {
                let version = p.version();
                let mut parts = version.split('.');
                match (parts.next(), parts.next()) {
                    (Some(major), Some(minor)) => format!("{major}.{minor}"),
                    _ => version.to_string(),
                }
            })
        })
        .unwrap_or_default()
}

/// Substitute tokens in every template file that carries them.
fn substitute(dir: &Path, tokens: &Tokens) -> Result<Vec<String>> {
    // The set the shipped template ships with placeholders in.
    const TARGETS: &[&str] = &[
        "bin/setenv.sh",
        "bin/custom_setenv.sh",
        "configuration/wrapper.conf",
    ];
    let mut changed = Vec::new();
    let mut candidates: Vec<PathBuf> = TARGETS.iter().map(|t| dir.join(t)).collect();
    // The launcher script is named after the service, so it cannot be listed.
    if let Ok(name) = service_name(dir) {
        candidates.push(dir.join("bin").join(name));
    }

    for path in candidates {
        if !path.is_file() {
            continue;
        }
        let text = fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
        if !text.contains("{{") {
            continue;
        }
        let rendered = tokens.apply(&text);
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt as _;
            fs::metadata(&path).ok().map(|m| m.permissions().mode())
        };
        fs::write(&path, rendered).map_err(|e| Error::io(&path, e))?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            set_mode(&path, mode);
        }
        changed.push(
            path.strip_prefix(dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned(),
        );
    }
    Ok(changed)
}

fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    fs::write(path, text).map_err(|e| Error::io(path, e))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

/// Reject a template entry that would write outside the instance.
fn safe_relative(entry: &str) -> Option<PathBuf> {
    use std::path::Component;
    let mut clean = PathBuf::new();
    for component in Path::new(entry).components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!clean.as_os_str().is_empty()).then_some(clean)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_would_break_paths_are_refused() {
        assert!(validate_name("default").is_ok());
        assert!(validate_name("is_2").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("../evil").is_err());
        assert!(validate_name("has space").is_err());
    }

    #[test]
    fn platform_filters_match_the_ant_file() {
        // On Unix the batch files and the Windows support tree are dropped.
        assert_eq!(skip_on_this_platform("bin/server.bat"), !cfg!(windows));
        assert_eq!(skip_on_this_platform("support/win/x.exe"), !cfg!(windows));
        assert!(!skip_on_this_platform("bin/server.sh") || cfg!(windows));
        assert!(!skip_on_this_platform("config/server.cnf"));
    }

    #[test]
    fn server_cnf_carries_the_ports_and_keeps_the_compile_placeholders() {
        let dir = std::env::temp_dir().join(format!("wm-inst-{}", std::process::id()));
        let spec = InstanceSpec {
            primary_port: 6555,
            ..InstanceSpec::default()
        };
        write_server_cnf(&dir, &spec).expect("write");
        let text = fs::read_to_string(dir.join("config").join("server.cnf")).expect("read");
        assert!(text.contains("watt.server.port=6555"));
        assert!(text.contains("watt.server.securePort=5543"));
        assert!(text.contains("{0}") && text.contains("{1}") && text.contains("{2}"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_wrapper_template_is_pointed_at_the_real_installation() {
        let text = "wrapper.java.additional.100=-server\n\
                    wrapper.java.additional.201=-DWM_HOME=c:\\webMethods\n\
                    wrapper.java.additional.202=-Dwatt.server.prepend.classes=c:\\webMethods\\common\\lib\\wm-converters.jar\n\
                    wrapper.working.dir=c:\\webMethods\\IntegrationServer\\instances\\i1\n";
        let out = render_wrapper_template(
            text,
            Path::new("/opt/webmethods"),
            Path::new("/opt/webmethods/IntegrationServer/instances/default"),
            "sagis121",
            8075,
        );
        assert!(out.contains("-DWM_HOME=/opt/webmethods"));
        assert!(out.contains("/opt/webmethods/common/lib/wm-converters.jar"));
        // Untouched lines survive.
        assert!(out.contains("wrapper.java.additional.100=-server"));
        // The sample working directory is replaced, not translated.
        assert!(
            out.contains("wrapper.working.dir=/opt/webmethods/IntegrationServer/instances/default")
        );
        assert!(!out.contains("instances\\i1") && !out.contains("instances/i1"));
        // The settings the instance manager appends are present.
        assert!(out.contains("wrapper.app.parameter.6=sagis121"));
        assert!(out.contains("-Dcom.sun.management.jmxremote.port=8075"));
    }

    #[test]
    fn the_core_package_list_falls_back_when_the_file_is_missing() {
        let list = core_packages(Path::new("/nonexistent"));
        assert!(list.contains(&"WmPublic".to_string()));
        assert!(list.contains(&"WmRoot".to_string()));
    }

    #[test]
    fn tokens_are_replaced_everywhere_they_appear() {
        let mut values = std::collections::BTreeMap::new();
        values.insert(
            "INSTALL_AREA",
            "/opt/wm/IntegrationServer/instances/default".to_string(),
        );
        values.insert("ROOT_PATH", "/opt/wm/common".to_string());
        values.insert("SECURITY_LIB_DIR", "security/ssx/lib".to_string());
        let tokens = Tokens { values };
        let text = "INSTALL_AREA={{INSTALL_AREA}}\n                    path={{ROOT_PATH}}/{{SECURITY_LIB_DIR}}\n                    again={{INSTALL_AREA}}/bin\n";
        let out = tokens.apply(text);
        assert!(!out.contains("{{"), "no placeholder may survive: {out}");
        assert!(out.contains("path=/opt/wm/common/security/ssx/lib"));
        assert_eq!(
            out.matches("/opt/wm/IntegrationServer/instances/default")
                .count(),
            2
        );
    }

    #[test]
    fn an_unknown_token_is_left_alone_rather_than_blanked() {
        let tokens = Tokens {
            values: std::collections::BTreeMap::new(),
        };
        assert_eq!(tokens.apply("x={{SOMETHING_NEW}}"), "x={{SOMETHING_NEW}}");
    }

    #[test]
    fn wrapper_versions_are_ordered_numerically() {
        let mut v = [
            "3.5.9".to_string(),
            "3.5.60".to_string(),
            "3.5.53".to_string(),
        ];
        v.sort_by_key(|a| version_key(a));
        assert_eq!(v.last().map(String::as_str), Some("3.5.60"));
    }

    #[test]
    fn template_entries_cannot_escape_the_instance() {
        assert!(safe_relative("../../etc/passwd").is_none());
        assert!(safe_relative("/etc/passwd").is_none());
        assert_eq!(
            safe_relative("bin/server.sh"),
            Some(PathBuf::from("bin/server.sh"))
        );
    }
}

/// Create an instance with the product's own Ant script.
///
/// `IntegrationServer/instances/is_instance.sh` ships with the product, along
/// with the Ant that runs it (`common/lib/ant`) and the `is_instance.xml` that
/// holds the logic. It self-documents with its `help` target and takes exactly
/// the inputs this module used to reimplement.
///
/// Driving it is the supported way, for the same reason the p2 director and
/// `dbConfigurator.sh` are driven rather than reimplemented: an instance the
/// product's own tooling created is one it will recognise afterwards.
pub mod ant {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use serde::Serialize;

    use crate::error::{Error, Result};

    /// An `is_instance.sh` invocation, before it is run.
    #[derive(Debug, Clone, Serialize)]
    pub struct Invocation {
        pub program: PathBuf,
        pub args: Vec<String>,
        /// `JAVA_HOME`, which the script wants from `IntegrationServer/bin/setenv.sh`
        /// and refuses to run without. The product ships its own JVM, so there
        /// is no reason to depend on the caller's environment or on a setenv
        /// the installation may not have.
        pub java_home: PathBuf,
    }

    impl Invocation {
        /// Render the command, masking anything that looks like a secret.
        pub fn display(&self) -> String {
            let masked: Vec<String> = self
                .args
                .iter()
                .map(|arg| match arg.split_once('=') {
                    Some((key, _))
                        if key.ends_with("password") || key.ends_with("admin.password") =>
                    {
                        format!("{key}=******")
                    }
                    _ => arg.clone(),
                })
                .collect();
            format!("{} {}", self.program.display(), masked.join(" "))
        }
    }

    /// Optional settings the `create` target accepts.
    #[derive(Debug, Clone, Default)]
    pub struct Options {
        pub primary_port: Option<u16>,
        pub secure_port: Option<u16>,
        pub diagnostic_port: Option<u16>,
        pub jmx_port: Option<u16>,
        pub admin_password: Option<String>,
        pub bind_address: Option<String>,
        pub license_file: Option<String>,
        pub packages: Vec<String>,
        /// `ORACLE`, `DB2`, `SQLSERVER`, `MYSQLCE`, `MYSQLEE`, `POSTGRESQL`.
        /// Omitted entirely, the instance uses the embedded database.
        pub db_type: Option<String>,
        pub db_alias: Option<String>,
        pub db_url: Option<String>,
        pub db_username: Option<String>,
        pub db_password: Option<String>,
    }

    /// Build the invocation that creates `name`.
    pub fn create(wm_home: &Path, name: &str, options: &Options) -> Result<Invocation> {
        let program = wm_home
            .join("IntegrationServer")
            .join("instances")
            .join("is_instance.sh");
        if !program.is_file() {
            return Err(Error::Malformed(format!(
                "{} is missing; Integration Server is not installed here",
                program.display()
            )));
        }
        let mut args = vec!["create".to_string(), format!("-Dinstance.name={name}")];
        let mut push = |key: &str, value: Option<String>| {
            if let Some(value) = value {
                args.push(format!("-D{key}={value}"));
            }
        };
        push("primary.port", options.primary_port.map(|p| p.to_string()));
        push("secure.port", options.secure_port.map(|p| p.to_string()));
        push(
            "diagnostic.port",
            options.diagnostic_port.map(|p| p.to_string()),
        );
        push("jmx.port", options.jmx_port.map(|p| p.to_string()));
        push("admin.password", options.admin_password.clone());
        push("instance.ip", options.bind_address.clone());
        push("license.file", options.license_file.clone());
        push("db.type", options.db_type.clone());
        push("db.alias", options.db_alias.clone());
        push("db.url", options.db_url.clone());
        push("db.username", options.db_username.clone());
        push("db.password", options.db_password.clone());
        if !options.packages.is_empty() {
            args.push(format!("-Dpackage.list={}", options.packages.join(",")));
        }
        let java_home = wm_home.join("jvm").join("jvm");
        if !java_home.join("bin").join("java").is_file() {
            return Err(Error::Malformed(format!(
                "no JVM at {}; is_instance.sh cannot run",
                java_home.display()
            )));
        }
        Ok(Invocation {
            program,
            args,
            java_home,
        })
    }

    /// Run it, returning whether Ant succeeded and what it printed.
    pub fn run(invocation: &Invocation) -> Result<(bool, String)> {
        let output = Command::new(&invocation.program)
            .args(&invocation.args)
            .env("JAVA_HOME", &invocation.java_home)
            .env("JRE_HOME", &invocation.java_home)
            .current_dir(
                invocation
                    .program
                    .parent()
                    .unwrap_or_else(|| Path::new(".")),
            )
            .output()
            .map_err(|e| {
                Error::Exec(format!("cannot run {}: {e}", invocation.program.display()))
            })?;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        // Ant reports its own verdict; the exit code alone has been unreliable.
        let ok = output.status.success() && text.contains("BUILD SUCCESSFUL");
        Ok((ok, text))
    }
}
