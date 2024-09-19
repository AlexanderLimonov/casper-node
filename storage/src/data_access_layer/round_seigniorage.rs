use crate::tracking_copy::TrackingCopyError;
use casper_types::{Digest, ProtocolVersion, U512};
use num_rational::Ratio;

/// Request to get the current round seigniorage rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundSeigniorageRateRequest {
    state_hash: Digest,
    protocol_version: ProtocolVersion,
    enable_addressable_entity: bool,
}

impl RoundSeigniorageRateRequest {
    /// Create instance of RoundSeigniorageRateRequest.
    pub fn new(
        state_hash: Digest,
        protocol_version: ProtocolVersion,
        enable_addressable_entity: bool,
    ) -> Self {
        RoundSeigniorageRateRequest {
            state_hash,
            protocol_version,
            enable_addressable_entity,
        }
    }

    /// Returns state root hash.
    pub fn state_hash(&self) -> Digest {
        self.state_hash
    }

    /// Returns the protocol version.
    pub fn protocol_version(&self) -> ProtocolVersion {
        self.protocol_version
    }

    pub fn enable_to_entity(&self) -> bool {
        self.enable_addressable_entity
    }
}

/// Represents a result of a `round_seigniorage_rate` request.
#[derive(Debug)]
pub enum RoundSeigniorageRateResult {
    /// Invalid state root hash.
    RootNotFound,
    /// The mint is not found.
    MintNotFound,
    /// Value not found.
    ValueNotFound(String),
    /// The round seigniorage rate at the specified state hash.
    Success {
        /// The current rate.
        rate: Ratio<U512>,
    },
    Failure(TrackingCopyError),
}
