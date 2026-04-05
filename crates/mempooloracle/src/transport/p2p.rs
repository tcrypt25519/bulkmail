use crate::{P2pTransportConfig, TrackerConfig, TrackerError, TrackerRuntime};

#[cfg(feature = "reth-p2p")]
pub async fn connect_with_config(
    _config: P2pTransportConfig,
    _tracker_config: TrackerConfig,
) -> Result<TrackerRuntime, TrackerError> {
    Err(TrackerError::UnsupportedTransport(
        "embedded reth p2p transport is not implemented yet",
    ))
}

#[cfg(not(feature = "reth-p2p"))]
pub async fn connect_with_config(
    _config: P2pTransportConfig,
    _tracker_config: TrackerConfig,
) -> Result<TrackerRuntime, TrackerError> {
    Err(TrackerError::FeatureDisabled("reth-p2p"))
}
