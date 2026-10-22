//! A profile: one app's key and where its database is, kept on this machine.
//!
//! The key is the app's credential — ErisDB knows the app by it alone — so
//! the file is the user's own (mode 0600 on Unix), is replaced atomically,
//! and never appears in tool results. Signing in again keeps the key, so
//! the app stays the same installation with whatever grants the person
//! approved this time.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use erisdb_client::SignIn;
use iroh::{EndpointAddr, SecretKey};
use serde::{Deserialize, Serialize};

/// Profiles are names, not paths; normal setup never exposes credential storage.
pub fn path(explicit: Option<PathBuf>, profile: &str) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if profile.is_empty()
        || profile.len() > 64
        || !profile.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        bail!("profile must contain 1–64 letters, digits, underscores or hyphens");
    }
    Ok(dirs::config_dir()
        .context("cannot locate user configuration directory; use --profile-file")?
        .join("erisdb/mcp")
        .join(format!("{profile}.json")))
}

#[derive(Deserialize, Serialize)]
pub struct Profile {
    /// The app's key, 64 hex characters.
    pub key: String,
    /// The mesh's database, as the device said when the app signed in.
    pub server: EndpointAddr,
}

impl Profile {
    pub fn read(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).with_context(|| {
            format!("no saved sign-in at {}; run erisdb-mcp sign-in --grant PERMISSIONS first", path.display())
        })?;
        if !file.metadata()?.is_file() {
            bail!("the profile must be a regular file");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if file.metadata()?.permissions().mode() & 0o077 != 0 {
                bail!("{} holds this app's key: chmod 600 before use", path.display());
            }
        }
        let profile: Self = serde_json::from_reader(file).with_context(|| format!("reading {}", path.display()))?;
        profile.secret()?;
        Ok(profile)
    }

    pub fn secret(&self) -> Result<SecretKey> {
        let bytes = erisdb_client::blocking::decode_key_hex(&self.key).context("the profile's key is not 64 hex characters")?;
        Ok(SecretKey::from_bytes(&bytes))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Sign in through this machine's eon and keep the profile. Runs outside
/// MCP: the person answers on eon's screen and types the database's
/// passphrase there. The saved profile is replaced only once the database has
/// taken the app; a denial or a cancel leaves it as it was.
pub async fn sign_in(path: &Path, name: &str, grants: &[String]) -> Result<()> {
    if grants.is_empty() {
        bail!("ask for at least one permission with --grant");
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut directory = std::fs::DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(parent)?;
    // Keep the lock inode stable across atomic replacements and failed processes.
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(path.with_extension("lock"))?;
    lock.try_lock().context("another sign-in is already using this profile")?;
    let previous = match std::fs::symlink_metadata(path) {
        Ok(_) => Some(Profile::read(path)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let key = match &previous {
        Some(previous) => previous.secret()?,
        None => SecretKey::generate(),
    };
    // Prove local storage is writable before asking anyone. On failure the
    // old profile stays intact; readers see either the old or the new one.
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    eprintln!("Asking eon to let {name} use the database. Approve it on the device and type the passphrase there.");
    let answer = tokio::select! {
        answer = erisdb_client::sign_in(key.clone(), name, grants) => answer?,
        _ = tokio::signal::ctrl_c() => bail!("cancelled; the request is withdrawn"),
    };
    match answer {
        None => bail!("no eon is listening here ($XDG_RUNTIME_DIR/eon/agent.sock): sign in where eon runs"),
        Some(SignIn::Denied(why)) => bail!("eon said no: {why}"),
        Some(SignIn::Approved { client, note, .. }) => {
            let profile = Profile { key: hex(&key.to_bytes()), server: client.server().clone() };
            file.write_all(&serde_json::to_vec_pretty(&profile)?)?;
            file.as_file().sync_all()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            if previous.is_some() {
                file.persist(path)?;
            } else {
                file.persist_noclobber(path)?;
            }
            #[cfg(unix)]
            std::fs::File::open(parent)?.sync_all()?;
            let grants = note.uses(erisdb_client::CAPABILITY).unwrap_or_default();
            eprintln!("Signed in. The database lets {name} {}.", grants.join(", "));
            Ok(())
        }
    }
}
