//! Session-scoped SSH credentials. They never enter manager state or JSON.
use std::{fmt, time::{SystemTime, UNIX_EPOCH}};
use super::provision::SecretBytes;

const SESSION_CREDENTIAL_TTL_SECS: u64 = 30 * 60;

pub struct SessionCredentials {
    worker_key: Option<SecretBytes>,
    model_token: Option<SecretBytes>,
    expires_at: Option<u64>,
}

impl Default for SessionCredentials {
    fn default() -> Self {
        Self { worker_key: None, model_token: None, expires_at: None }
    }
}

impl fmt::Debug for SessionCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("SessionCredentials")
            .field("worker_key", &self.worker_key.as_ref().map(|_| "[REDACTED]"))
            .field("model_token", &self.model_token.as_ref().map(|_| "[REDACTED]"))
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl SessionCredentials {
    pub fn set_worker_key(&mut self, worker_key: SecretBytes) { self.set_worker_key_at(worker_key, now()); }

    pub fn set_worker_key_at(&mut self, worker_key: SecretBytes, now: u64) {
        self.worker_key = Some(worker_key);
        self.refresh_expiry_at(now);
    }

    pub fn set_model_token(&mut self, model_token: &[u8]) { self.set_model_token_at(model_token, now()); }

    pub fn set_model_token_at(&mut self, model_token: &[u8], now: u64) {
        self.model_token = (!model_token.is_empty()).then(|| SecretBytes::copy_from(model_token));
        self.refresh_expiry_at(now);
    }

    pub fn is_available(&self) -> bool {
        self.worker_key.is_some()
    }

    /// Copies session-only secrets for one transport. The source session remains usable.
    pub fn take_for_dispatch(&self) -> Option<CredentialLease> {
        let worker_key = self.worker_key.as_ref()?;
        Some(CredentialLease {
            worker_key: SecretBytes::copy_from(worker_key),
            model_token: self.model_token.as_ref().map(|token| SecretBytes::copy_from(token)),
        })
    }

    pub fn clear(&mut self) {
        self.worker_key = None;
        self.model_token = None;
        self.expires_at = None;
    }

    pub fn clear_if_expired(&mut self, now: u64) -> bool {
        if self.expires_at.is_some_and(|expiry| now >= expiry) {
            self.clear();
            true
        } else {
            false
        }
    }

    fn refresh_expiry_at(&mut self, now: u64) {
        self.expires_at = Some(now.saturating_add(SESSION_CREDENTIAL_TTL_SECS));
    }
}

pub struct CredentialLease {
    worker_key: SecretBytes,
    model_token: Option<SecretBytes>,
}

impl CredentialLease {
    pub(crate) fn worker_key_len(&self) -> usize { self.worker_key.len() }
    pub(crate) fn has_model_token(&self) -> bool { self.model_token.is_some() }
    pub(crate) fn into_transport_secrets(mut self) -> (SecretBytes, Option<SecretBytes>) {
        let worker_key = std::mem::replace(&mut self.worker_key, SecretBytes::copy_from(&[]));
        let model_token = self.model_token.take();
        (worker_key, model_token)
    }
}

impl fmt::Debug for CredentialLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("CredentialLease")
            .field("worker_key", &"[REDACTED]")
            .field("model_token", &self.model_token.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl Drop for CredentialLease {
    fn drop(&mut self) {
        self.model_token = None;
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_secs())
}
