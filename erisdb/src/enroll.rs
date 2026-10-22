//! Letting apps in: ErisDB's side of the mesh's
//! [`enroll`](erisoverlaynetwork::enroll) protocol.
//!
//! A device approves an app to use `database/1`, the person types ErisDB's
//! passphrase on the device, and the device brings both here over
//! `eris-enroll/0`, calling as itself. The mesh library's
//! [`EnrollService`](erisoverlaynetwork::enroll::EnrollService) lets in only
//! member devices bringing notes of their own that count and use the
//! database, and only while this core's own note counts: once the mesh
//! approves another database, devices find that one, and this one lets none
//! in. [`Gate`] does what is ErisDB's: the passphrase, with its per-device
//! lockout, and the app's installation.

use std::time::Duration;

use erisoverlaynetwork::devices::DeviceId;
use erisoverlaynetwork::enroll::Outcome;
use erisoverlaynetwork::Note;
use futures::future::BoxFuture;
use serde_json::json;
use sqlx::PgPool;

use crate::error::Error;
use crate::{installation, passphrase};

/// What letting an app in means to ErisDB.
#[derive(Clone)]
pub struct Gate {
    pool: PgPool,
    /// The first wait after a device's third wrong passphrase.
    lockout: Duration,
}

impl Gate {
    pub fn new(pool: PgPool, lockout: Duration) -> Self {
        Gate { pool, lockout }
    }
}

impl erisoverlaynetwork::enroll::Gate for Gate {
    /// Let in the app `note` approves when the passphrase is right: create
    /// its installation, or give the one it has the note's grants.
    fn enroll(
        &self,
        device: DeviceId,
        note: Note,
        guess: String,
    ) -> BoxFuture<'static, erisoverlaynetwork::Result<Outcome>> {
        let gate = self.clone();
        Box::pin(async move {
            let outcome = passphrase::check(&gate.pool, &device, &guess, gate.lockout).await?;
            if outcome == Outcome::Done {
                // What a device does it does in its own name.
                let source = json!({
                    "addr": format!("iroh:{device}"),
                    "user": null,
                    "client": null,
                    "installation": null,
                });
                let client = installation::enroll(&gate.pool, &note, source).await?;
                tracing::info!(installation = %client.id, app = %client.name, device = %device, "enrolled");
            }
            Ok(outcome)
        })
    }

    /// Make `new` the passphrase when `current` is right. A wrong `current`
    /// counts against the device like any other wrong guess; a `new` too
    /// short or too long costs it nothing.
    fn change(
        &self,
        device: DeviceId,
        current: String,
        new: String,
    ) -> BoxFuture<'static, erisoverlaynetwork::Result<Outcome>> {
        let gate = self.clone();
        Box::pin(async move {
            passphrase::validate(&new)?;
            let outcome = passphrase::check(&gate.pool, &device, &current, gate.lockout).await?;
            if outcome == Outcome::Done {
                passphrase::set(&gate.pool, &new).await?;
                tracing::info!(device = %device, "passphrase changed");
            }
            Ok(outcome)
        })
    }
}

/// How the device hears of a failure: a refusal it can act on as itself, or
/// the operator's business, which the log has.
impl From<Error> for erisoverlaynetwork::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::BadRequest(detail) => erisoverlaynetwork::Error::BadRequest(detail),
            other => {
                tracing::error!(error = %other, "enrolling");
                erisoverlaynetwork::Error::Internal("see ErisDB's log".into())
            }
        }
    }
}
