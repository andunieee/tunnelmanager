#[cfg(not(target_os = "android"))]
use anyhow::Context;

#[cfg(not(target_os = "android"))]
const SERVICE: &str = "alt-sendme";
#[cfg(not(target_os = "android"))]
const USER: &str = "iroh-secret";

/// Android has no real OS keychain backend in `keyring` (mock only). Identity
/// persistence uses `identity.key` exclusively, skip keychain I/O here.
#[cfg(target_os = "android")]
pub fn load_secret_hex() -> anyhow::Result<Option<String>> {
    Ok(None)
}

#[cfg(not(target_os = "android"))]
pub fn load_secret_hex() -> anyhow::Result<Option<String>> {
    let entry = keyring::Entry::new(SERVICE, USER).context("keyring entry")?;
    match entry.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

#[cfg(target_os = "android")]
pub fn save_secret_hex(_hex: &str) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(not(target_os = "android"))]
pub fn save_secret_hex(hex: &str) -> anyhow::Result<()> {
    let entry = keyring::Entry::new(SERVICE, USER).context("keyring entry")?;
    entry
        .set_password(hex)
        .context("failed to save secret to keyring")
}

#[cfg(target_os = "android")]
pub fn delete_secret() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(not(target_os = "android"))]
pub fn delete_secret() -> anyhow::Result<()> {
    let entry = keyring::Entry::new(SERVICE, USER).context("keyring entry")?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(err.into()),
    }
}
