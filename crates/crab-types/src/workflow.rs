//! Shared workflow identity contracts.

use serde::{Deserialize, Serialize};

/// 32-byte Blake3 digest identifying a fully-resolved workflow stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StageHash(pub [u8; 32]);

impl StageHash {
    /// All-zeros placeholder hash.
    ///
    /// Used only when serialized events need to keep a stable payload shape
    /// after dep resolution failed before a real stage hash could be computed.
    #[must_use]
    pub const fn zero() -> Self {
        Self([0u8; 32])
    }

    /// Return the lowercase hex representation.
    #[must_use]
    pub fn as_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in &self.0 {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }
}

impl std::fmt::Display for StageHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_hex())
    }
}
