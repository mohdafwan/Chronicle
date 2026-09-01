//! Chronicle core — the parts that have no opinion about which operating
//! system they are running on.
//!
//! The observer feeds [`model::Observation`]s in, the [`store::Store`] keeps
//! them, [`sessionize`] turns them into sessions, and everything user-facing
//! reads back out. Redaction and capture policy sit on the write path, so the
//! sensitive form of a title or a URL never reaches disk at all.

pub mod fmt;
pub mod model;
pub mod policy;
pub mod redact;
pub mod sessionize;
pub mod store;

pub use model::{
    ArtifactId, ArtifactKind, ArtifactObs, Category, EndReason, Frame, Observation, Session,
    SessionArtifact, SessionDetail, SessionId, TitleSource,
};
pub use policy::{CapturePolicy, Policies};
pub use sessionize::SessionRules;
pub use store::Store;

/// Apply capture policy and redaction to a raw sighting, returning the
/// observation to store — or `None` when the app must not be recorded at all.
///
/// This is the single choke point between the observer and the disk. Every
/// platform observer goes through it.
pub fn normalise(
    policies: &Policies,
    app_id: &str,
    raw_title: &str,
    mut obs: Observation,
) -> Option<Observation> {
    match policies.policy_for(app_id) {
        CapturePolicy::Ignore => return None,
        CapturePolicy::TitlesOff => {
            obs.title = redact::redact_title(raw_title, &obs.app_name, false);
            // Without a title there is nothing to identify a document by, so
            // the app is recorded as having been open and nothing more.
            obs.artifacts.retain(|a| a.kind == ArtifactKind::App);
        }
        CapturePolicy::Full => {
            obs.title = redact::redact_title(raw_title, &obs.app_name, true);
            obs.artifacts.retain(|a| !redact::looks_secret(&a.uri));
        }
    }
    Some(obs)
}
