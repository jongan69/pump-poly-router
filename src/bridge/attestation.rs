/// Circle Iris attestation API client.
///
/// After a CCTP `depositForBurn` on Solana, the message hash must be polled
/// here until the attestation status is "complete".  The returned attestation
/// bytes are then passed to `receiveMessage` on the destination chain.
use crate::error::{Result, RouterError};
use reqwest::Client as HttpClient;
use serde::Deserialize;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Response from `GET /v1/attestations/{messageHash}`.
#[derive(Debug, Deserialize)]
pub struct AttestationResponse {
    /// `"complete"` when ready; `"pending_confirmations"` or `"pending"` otherwise.
    pub status: String,
    /// Hex-encoded attestation bytes (present only when `status == "complete"`).
    pub attestation: Option<String>,
}

impl AttestationResponse {
    pub fn is_complete(&self) -> bool {
        self.status == "complete"
    }
}

pub struct CircleAttestationClient {
    base_url: String,
    http: HttpClient,
}

impl CircleAttestationClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        CircleAttestationClient { base_url: base_url.into(), http: HttpClient::new() }
    }

    /// Fetch the current attestation status for a message hash.
    pub async fn get_attestation(&self, message_hash: &str) -> Result<AttestationResponse> {
        let url = format!("{}/v1/attestations/{}", self.base_url, message_hash);
        let resp = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()
            .map_err(|e| RouterError::CctpAttestation(e.to_string()))?;
        let att: AttestationResponse = resp
            .json()
            .await
            .map_err(|e| RouterError::CctpAttestation(e.to_string()))?;
        Ok(att)
    }

    /// Poll until the attestation is complete or `timeout_secs` elapses.
    ///
    /// Returns the hex-encoded attestation bytes on success.
    pub async fn poll_until_complete(
        &self,
        message_hash: &str,
        timeout_secs: u64,
        poll_interval_secs: u64,
    ) -> Result<String> {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            let att = self.get_attestation(message_hash).await?;
            if att.is_complete() {
                let bytes = att.attestation.ok_or_else(|| {
                    RouterError::CctpAttestation(
                        "status=complete but attestation field missing".to_string(),
                    )
                })?;
                info!("CCTP attestation complete for {}", message_hash);
                return Ok(bytes);
            }

            debug!("CCTP attestation status={} for {}", att.status, message_hash);

            if Instant::now() >= deadline {
                warn!("CCTP attestation timed out for {}", message_hash);
                return Err(RouterError::CctpAttestationTimeout {
                    secs: timeout_secs,
                    hash: message_hash.to_string(),
                });
            }

            tokio::time::sleep(Duration::from_secs(poll_interval_secs)).await;
        }
    }
}
