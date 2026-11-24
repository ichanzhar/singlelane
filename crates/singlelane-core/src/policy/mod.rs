//! Policy: trusted-author filtering (G1), the protected-path guard (G4) and redaction (G8).

pub mod protected;
pub mod redact;
pub mod trust;

pub use protected::{ProtectedPaths, ProtectedPathsError};
pub use redact::Redactor;
pub use trust::TrustPolicy;
