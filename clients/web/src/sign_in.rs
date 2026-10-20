//! Signing in by ticket: the browser's side of the mesh's ceremony.
//!
//! The app redeems a device's `eris://pair/` ticket under its own key, asking for a note for
//! `erisdb` with the grants it wants. Both screens show the fingerprint of the request; the
//! person compares them and approves on the device, typing ErisDB's passphrase there. The
//! device enrolls the app at ErisDB and only then approves, so the note the app collects is
//! already good at ErisDB.
//!
//! A page can go away while the person is still deciding. What it keeps of a sign-in under way
//! — the ticket, what it asked for and the device's name — lets the next page wait for the same
//! answer: only the key that redeemed a code can collect it, and the page holds that key.

use std::time::Duration;

use erisoverlaynetwork::Result;
use erisoverlaynetwork::ceremony::{self, Approval, Ask};
use erisoverlaynetwork::ticket::Ticket;
use iroh::Endpoint;

/// How long a sign-in waits for the person, at most. The ticket usually ends first.
pub const PATIENCE: Duration = Duration::from_secs(3600);

/// The service the app asks to take part in.
pub const SERVICE: &str = "erisdb";

/// What the app asks for.
pub fn ask(name: &str, grants: Vec<String>) -> Ask {
    Ask::App { name: name.to_string(), service: SERVICE.to_string(), grants, serves: false }
}

/// A sign-in under way, redeemed by this page or by an earlier one under the same key.
pub struct Joining(ceremony::Joining);

impl Joining {
    pub fn fingerprint(&self) -> &str {
        &self.0.fingerprint
    }

    pub fn host(&self) -> &str {
        &self.0.host
    }

    /// The approval, checked to be a note for this app from the device that showed the ticket,
    /// or `None` when the person denied the request.
    pub async fn wait(&self) -> Result<Option<Approval>> {
        self.0.wait(PATIENCE).await
    }

    pub async fn withdraw(&self) -> Result<()> {
        self.0.withdraw().await
    }
}

/// Redeem `ticket` from `endpoint`, asking for `ask`.
pub async fn redeem(endpoint: &Endpoint, ticket: &Ticket, ask: Ask) -> Result<Joining> {
    ceremony::redeem(endpoint, ticket, ask).await.map(Joining)
}

/// Wait again for the answer to a redemption this key made on an earlier page.
pub fn resume(endpoint: &Endpoint, ticket: Ticket, ask: Ask, host: String) -> Result<Joining> {
    ceremony::resume(endpoint, &ticket, ask, host).map(Joining)
}
