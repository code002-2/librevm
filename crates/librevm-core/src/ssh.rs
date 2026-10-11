//! Keys for reaching a guest.
//!
//! One keypair for the whole host, in the store, rather than one per machine. A machine
//! gets its own entry in `known_hosts` and its own record of which address it was on, but
//! sharing the key means a new machine is reachable without generating anything, and there
//! is one private key to keep rather than a directory of them.
//!
//! The guest's address comes from the same forwarding scheme as `net.rs`: the machine takes
//! an address on the host's subnet, so the host reaches it directly with no forwarding rule
//! and no port mapping.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The keypair in the store.
pub const KEY_NAME: &str = "librevm_ed25519";

/// Where the private key lives.
pub fn private_key(store_root: &Path) -> PathBuf {
    store_root.join("ssh").join(KEY_NAME)
}

/// Where the public key lives.
pub fn public_key(store_root: &Path) -> PathBuf {
    store_root.join("ssh").join(format!("{KEY_NAME}.pub"))
}

/// Where this host's view of guests is kept, so a rebuilt guest does not trip the
/// host-key check.
pub fn known_hosts(store_root: &Path) -> PathBuf {
    store_root.join("ssh").join("known_hosts")
}

/// The account used to log in, which is always root: these are single-purpose machines and
/// the first-boot script only sets up one account.
pub const USER: &str = "root";

/// Whether a keypair is present.
pub fn exists(store_root: &Path) -> bool {
    private_key(store_root).is_file() && public_key(store_root).is_file()
}

/// Generate the keypair if it is not there, and return the public key text.
///
/// Called before deploying an image, so the key that goes into the image is the one that
/// will be used to reach it.
pub fn ensure_keypair(store_root: &Path) -> Result<String> {
    let dir = store_root.join("ssh");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok();
    }

    let private = private_key(store_root);
    if !private.is_file() {
        let out = Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-N",
                "", // no passphrase: this is for automation
                "-C",
                "librevm",
                "-f",
                &private.display().to_string(),
            ])
            .output()
            .context("running ssh-keygen; is openssh-client installed?")?;
        if !out.status.success() {
            bail!(
                "ssh-keygen failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }

    // ssh refuses to use a key anyone else can read, and a `chmod -R g+rwX` over the store
    // is exactly the kind of thing that widens this one by accident. Setting it here means
    // the file is correct whenever it is used, whatever happened to it in between.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600)).ok();
        // The public key is not a secret, but there is no reason for it to be group
        // writable either.
        std::fs::set_permissions(public_key(store_root), std::fs::Permissions::from_mode(0o644)).ok();
    }

    std::fs::read_to_string(public_key(store_root))
        .with_context(|| format!("reading {}", public_key(store_root).display()))
        .map(|s| s.trim().to_string())
}

/// A host key hash for a guest, so a rebuilt machine does not stop a connection.
fn forget_host_key(store_root: &Path, address: &str) {
    let kh = known_hosts(store_root);
    if !kh.is_file() {
        return;
    }
    let _ = Command::new("ssh-keygen")
        .args([
            "-R",
            address,
            "-f",
            &kh.display().to_string(),
        ])
        .output();
}

/// The arguments that reach a guest, whichever command is being run.
pub fn connection_args(store_root: &Path, address: &str) -> Vec<String> {
    vec![
        "-i".into(),
        private_key(store_root).display().to_string(),
        "-o".into(),
        "IdentitiesOnly=yes".into(),
        // Guests are rebuilt often and get a new host key each time, so the strict check
        // only gets in the way. The key is still pinned per address once accepted.
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        format!("UserKnownHostsFile={}", known_hosts(store_root).display()),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
        format!("{USER}@{address}"),
    ]
}

/// Run a command in a guest and return its output.
pub fn run(store_root: &Path, address: &str, command: &str) -> Result<String> {
    let mut args = connection_args(store_root, address);
    args.push(command.to_string());
    let out = Command::new("ssh")
        .args(&args)
        .output()
        .context("running ssh; is openssh-client installed?")?;
    if !out.status.success() {
        bail!(
            "ssh to {address} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// An interactive session.
///
/// On Unix this replaces the process, which is what an interactive shell over ssh wants:
/// the real terminal, with no wrapper in between to get the window size or the signals
/// wrong. Anywhere else it is an ordinary child.
pub fn connect(store_root: &Path, address: &str, command: Option<&str>) -> Result<()> {
    // A guest that was redeployed has a new host key, and a stale entry turns a working
    // login into a warning that reads like a failure.
    forget_host_key(store_root, address);

    let args = connection_args(store_root, address);
    let mut cmd = Command::new("ssh");
    cmd.args(&args);
    if let Some(c) = command {
        cmd.arg(c);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `exec` only returns if it failed.
        let err = cmd.exec();
        Err(anyhow::anyhow!("could not start ssh: {err}"))
    }

    #[cfg(not(unix))]
    {
        let status = cmd.status().context("running ssh")?;
        if !status.success() {
            bail!("ssh exited with {status}");
        }
        Ok(())
    }
}

/// Copy a file to a guest.
pub fn copy(store_root: &Path, address: &str, local: &Path, remote: &str) -> Result<()> {
    let mut args = connection_args(store_root, address);
    // scp takes the paths after the options, and the destination is host:path.
    let destination = args.pop().context("no destination")?;
    let out = Command::new("scp")
        .args(&args)
        .arg(local)
        .arg(format!("{destination}:{remote}"))
        .output()
        .context("running scp")?;
    if !out.status.success() {
        bail!(
            "scp failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Write the SSH config so plain `ssh librevm-demo` works outside this tool as well.
///
/// Kept in the store rather than in `/etc/ssh/ssh_config.d`, so nothing here needs root and
/// nothing is written outside the tool's own directory.
pub fn write_client_config(store_root: &Path, machines: &[(String, String)]) -> Result<PathBuf> {
    let dir = store_root.join("ssh");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config");
    let key = private_key(store_root);
    let kh = known_hosts(store_root);

    let mut text = String::from(
        "# Written by librevm. Include this from ~/.ssh/config if you want to reach a\n\
         # guest by name:\n\
         #\n\
         #     Include /var/lib/librevm/ssh/config\n\
         #\n\
         # Regenerated whenever the machine list changes.\n\n",
    );
    for (name, address) in machines {
        text.push_str(&format!(
            "Host librevm-{name}\n\
             \x20   HostName {address}\n\
             \x20   User {USER}\n\
             \x20   IdentityFile {}\n\
             \x20   IdentitiesOnly yes\n\
             \x20   StrictHostKeyChecking accept-new\n\
             \x20   UserKnownHostsFile {}\n\
             \x20   ConnectTimeout 10\n\n",
            key.display(),
            kh.display()
        ));
    }
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Whether `ssh` and `ssh-keygen` are present.
pub fn tools_available() -> (bool, bool) {
    let have = |p: &str| {
        std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).any(|d| d.join(p).is_file()))
            .unwrap_or(false)
    };
    (have("ssh"), have("ssh-keygen"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_paths_sit_under_the_store() {
        let root = Path::new("/var/lib/librevm");
        assert!(private_key(root).starts_with(root));
        assert!(public_key(root).starts_with(root));
        assert!(known_hosts(root).starts_with(root));
        // The private key must not be the public one.
        assert_ne!(private_key(root), public_key(root));
    }

    #[test]
    fn the_public_key_name_is_derived_from_the_private_one() {
        let root = Path::new("/x");
        let priv_name = private_key(root).file_name().unwrap().to_string_lossy().to_string();
        let pub_name = public_key(root).file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(pub_name, format!("{priv_name}.pub"));
    }

    #[test]
    fn connection_arguments_carry_the_key_and_the_address() {
        let root = Path::new("/var/lib/librevm");
        let args = connection_args(root, "192.168.1.201");
        let joined = args.join(" ");
        assert!(joined.contains("-i /var/lib/librevm/ssh/librevm_ed25519"), "{joined}");
        assert!(joined.contains("IdentitiesOnly=yes"), "{joined}");
        assert!(joined.contains("UserKnownHostsFile="), "{joined}");
        // The destination is last, which is what scp's argument order depends on.
        assert_eq!(args.last().unwrap(), "root@192.168.1.201");
        assert!(args.last().unwrap().starts_with(&format!("{USER}@")));
    }

    #[test]
    fn accept_new_is_used_rather_than_no_checking_at_all() {
        let args = connection_args(Path::new("/x"), "10.0.0.1");
        let joined = args.join(" ");
        assert!(joined.contains("accept-new"), "{joined}");
        assert!(!joined.contains("StrictHostKeyChecking=no"), "{joined}");
    }

    #[test]
    fn the_client_config_lists_each_machine() {
        let dir = std::env::temp_dir().join(format!("librevm-ssh-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let machines = vec![
            ("demo".to_string(), "192.168.1.201".to_string()),
            ("other".to_string(), "192.168.1.202".to_string()),
        ];
        let path = write_client_config(&dir, &machines).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("Host librevm-demo"), "{text}");
        assert!(text.contains("HostName 192.168.1.201"), "{text}");
        assert!(text.contains("Host librevm-other"), "{text}");
        assert!(text.contains("HostName 192.168.1.202"), "{text}");
        assert!(text.contains("Include /var/lib/librevm/ssh/config"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_machine_list_still_writes_a_usable_file() {
        let dir = std::env::temp_dir().join(format!("librevm-ssh-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_client_config(&dir, &[]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("Include"), "{text}");
        assert!(!text.contains("Host librevm-"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
