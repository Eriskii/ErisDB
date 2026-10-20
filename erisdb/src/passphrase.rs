//! The enrollment passphrase: what a person types on a device to let an app
//! into ErisDB, checked here and nowhere else.
//!
//! It is kept as an Argon2id hash in its own table, never in a facet, so no
//! grant reaches it. The operator sets the first one, and resets it, on the
//! server (`erisdb passphrase set`); a member device can change it by
//! knowing the current one.
//!
//! Guesses are counted per device. The first three wrong guesses in a row
//! cost nothing; the third locks the device out for a minute, and each wrong
//! guess after that doubles the wait, up to a day. A right guess clears the
//! device's count. One device's guesses never lock out another, so a stolen
//! phone hammering away leaves the person's laptop free to enroll — and the
//! laptop can remove the phone.

use std::time::Duration;

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use erisoverlaynetwork::devices::DeviceId;
use sqlx::PgPool;

use crate::error::{Error, Result};

/// The shortest passphrase ErisDB takes, in characters.
pub const MIN_LEN: usize = 8;

/// The longest, so hashing a guess costs what it should and no more.
pub const MAX_LEN: usize = 1024;

/// Wrong guesses a device makes before it waits.
pub const FREE_GUESSES: u32 = 3;

/// The first wait. Each wrong guess after it doubles the wait.
pub const LOCKOUT: Duration = Duration::from_secs(60);

/// The longest wait.
pub const LOCKOUT_CEILING: Duration = Duration::from_secs(86_400);

/// How long a device waits after its `wrong`th wrong guess in a row, when
/// the first wait is `first`: nothing before the third, `first` after it,
/// then double each time, never more than [`LOCKOUT_CEILING`].
pub fn lockout(wrong: u32, first: Duration) -> Option<Duration> {
    let doublings = wrong.checked_sub(FREE_GUESSES)?;
    let wait = 2u32
        .checked_pow(doublings)
        .and_then(|factor| first.checked_mul(factor))
        .unwrap_or(LOCKOUT_CEILING);
    Some(wait.min(LOCKOUT_CEILING))
}

/// A passphrase ErisDB will keep.
pub fn validate(passphrase: &str) -> Result<()> {
    let len = passphrase.chars().count();
    if len < MIN_LEN {
        return Err(Error::BadRequest(format!(
            "a passphrase is at least {MIN_LEN} characters"
        )));
    }
    if passphrase.len() > MAX_LEN {
        return Err(Error::BadRequest(format!(
            "a passphrase is at most {MAX_LEN} bytes"
        )));
    }
    Ok(())
}

async fn hash(passphrase: String) -> Result<String> {
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(passphrase.as_bytes(), &salt)
            .map(|hash| hash.to_string())
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))?
    .map_err(|e| Error::Internal(format!("hashing the passphrase: {e}")))
}

async fn verify(hash: String, guess: String) -> Result<bool> {
    tokio::task::spawn_blocking(move || {
        let hash = PasswordHash::new(&hash)
            .map_err(|e| Error::Internal(format!("the stored passphrase: {e}")))?;
        Ok(Argon2::default()
            .verify_password(guess.as_bytes(), &hash)
            .is_ok())
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))?
}

/// Set the passphrase, replacing any there was. Guesses already counted
/// stand; [`unlock`] clears them.
pub async fn set(pool: &PgPool, passphrase: &str) -> Result<()> {
    validate(passphrase)?;
    let hash = hash(passphrase.to_string()).await?;
    sqlx::query(
        "INSERT INTO passphrase (id, hash) VALUES (true, $1)
         ON CONFLICT (id) DO UPDATE SET hash = EXCLUDED.hash, set_at = now()",
    )
    .bind(hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Forget the wrong guesses of one device, or of every device; returns how
/// many devices had any.
pub async fn unlock(pool: &PgPool, device: Option<&DeviceId>) -> Result<u64> {
    let cleared =
        sqlx::query("DELETE FROM passphrase_guesses WHERE $1::text IS NULL OR device = $1")
            .bind(device.map(ToString::to_string))
            .execute(pool)
            .await?;
    Ok(cleared.rows_affected())
}

/// Check `guess` from `device`, counting it when it is wrong. While the
/// device is locked out nothing is checked at all: the answer is how long
/// it has left to wait. `first` is the first wait; see [`lockout`].
pub async fn check(pool: &PgPool, device: &DeviceId, guess: &str, first: Duration) -> Result<()> {
    let stored: Option<String> = sqlx::query_scalar("SELECT hash FROM passphrase")
        .fetch_optional(pool)
        .await?;
    let stored = stored.ok_or(Error::NoPassphrase)?;
    let device = device.to_string();
    // The device's row stays locked while its guess is hashed, so guesses
    // sent at once are counted one after another rather than all at once.
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO passphrase_guesses (device) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(&device)
        .execute(&mut *tx)
        .await?;
    let (wrong, left): (i32, Option<f64>) = sqlx::query_as(
        "SELECT wrong, extract(epoch FROM locked_until - clock_timestamp())::float8
         FROM passphrase_guesses WHERE device = $1 FOR UPDATE",
    )
    .bind(&device)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(left) = left.filter(|left| *left > 0.0) {
        return Err(Error::Locked {
            retry_after: left.ceil().max(1.0) as u64,
        });
    }
    if verify(stored, guess.to_string()).await? {
        sqlx::query("DELETE FROM passphrase_guesses WHERE device = $1")
            .bind(&device)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(());
    }
    let wrong = u32::try_from(wrong).unwrap_or(0).saturating_add(1);
    let wait = lockout(wrong, first);
    sqlx::query(
        "UPDATE passphrase_guesses
         SET wrong = $2, locked_until = clock_timestamp() + make_interval(secs => $3)
         WHERE device = $1",
    )
    .bind(&device)
    .bind(i32::try_from(wrong).unwrap_or(i32::MAX))
    .bind(wait.map_or(0.0, |wait| wait.as_secs_f64()))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::warn!(device = %device, wrong, "wrong passphrase");
    Err(Error::WrongPassphrase {
        retry_after: wait.map(|wait| wait.as_secs().max(1)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schedule people are told: three free, then a minute, doubling, to a day.
    #[test]
    fn three_wrong_guesses_are_free_then_the_wait_doubles_to_a_day() {
        let waits: Vec<Option<u64>> = (1..=15)
            .map(|n| lockout(n, LOCKOUT).map(|d| d.as_secs()))
            .collect();
        let minutes = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 1440, 1440];
        let expected: Vec<Option<u64>> = [None, None]
            .into_iter()
            .chain(minutes.map(|m| Some(m * 60)))
            .collect();
        assert_eq!(waits, expected);
        assert_eq!(lockout(u32::MAX, LOCKOUT), Some(LOCKOUT_CEILING));
    }
}
