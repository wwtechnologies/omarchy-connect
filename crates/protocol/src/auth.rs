//! PIN authentication.
//!
//! The host's TLS certificate is ephemeral and unpinned; the PIN is what
//! authenticates both ends. After `Hello`/`HelloAck`:
//!
//! 1. client → host `Auth { spake2 message A }`
//! 2. host → client `Auth { spake2 message B }`, or `AuthDenied`
//! 3. client → host `AuthConfirm { HMAC(K, "client" ‖ E) }`
//! 4. host → client `AuthConfirm { HMAC(K, "host" ‖ E) }`, or `AuthDenied`
//!
//! `K` is the SPAKE2 (Ed25519) key derived from the PIN. `E` is a TLS
//! exporter value, so a machine in the middle terminating two TLS sessions
//! cannot relay the confirmations: its two legs have different `E`. A wrong
//! PIN yields a different `K`, and the transcript gives an attacker one
//! online guess per connection. The host rate-limits failures.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

/// Label for `export_keying_material`. Both sides use 32 bytes and no context.
pub const TLS_EXPORTER_LABEL: &[u8] = b"EXPORTER-omarchy-connect-pin";
pub const PIN_MIN: usize = 6;
pub const PIN_MAX: usize = 32;

const ID_CLIENT: &[u8] = b"omarchy-connect client";
const ID_HOST: &[u8] = b"omarchy-connect host";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Client,
    Host,
}

impl Side {
    fn label(self) -> &'static [u8] {
        match self {
            Side::Client => b"client",
            Side::Host => b"host",
        }
    }
}

/// Checks a PIN before it is stored or sent. Leading and trailing whitespace
/// is not part of the PIN.
pub fn validate_pin(pin: &str) -> Result<&str, String> {
    let pin = pin.trim();
    let len = pin.chars().count();
    if len < PIN_MIN || len > PIN_MAX {
        return Err(format!("PIN must be {PIN_MIN} to {PIN_MAX} characters"));
    }
    if pin.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("PIN cannot contain spaces".into());
    }
    Ok(pin)
}

pub struct PinExchange {
    side: Side,
    state: Spake2<Ed25519Group>,
}

impl PinExchange {
    /// Returns the exchange and the outbound `Auth` payload.
    pub fn start(side: Side, pin: &str) -> (Self, Vec<u8>) {
        let password = Password::new(pin.trim().as_bytes());
        let (client, host) = (Identity::new(ID_CLIENT), Identity::new(ID_HOST));
        let (state, outbound) = match side {
            Side::Client => Spake2::<Ed25519Group>::start_a(&password, &client, &host),
            Side::Host => Spake2::<Ed25519Group>::start_b(&password, &client, &host),
        };
        (Self { side, state }, outbound)
    }

    /// Consumes the peer's `Auth` payload. A wrong PIN still succeeds here;
    /// it is caught when the confirmations disagree.
    pub fn finish(self, peer: &[u8], binding: &[u8]) -> Result<PinKeys, AuthFailure> {
        let key = self.state.finish(peer).map_err(|_| AuthFailure)?;
        Ok(PinKeys {
            side: self.side,
            key,
            binding: binding.to_vec(),
        })
    }
}

pub struct PinKeys {
    side: Side,
    key: Vec<u8>,
    binding: Vec<u8>,
}

impl PinKeys {
    pub fn confirmation(&self) -> [u8; 32] {
        self.mac(self.side).finalize().into_bytes().into()
    }

    pub fn verify_peer(&self, tag: &[u8]) -> Result<(), AuthFailure> {
        let peer = match self.side {
            Side::Client => Side::Host,
            Side::Host => Side::Client,
        };
        self.mac(peer).verify_slice(tag).map_err(|_| AuthFailure)
    }

    fn mac(&self, side: Side) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("hmac accepts any key");
        mac.update(side.label());
        mac.update(&self.binding);
        mac
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthFailure;

impl std::fmt::Display for AuthFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PIN did not match")
    }
}

impl std::error::Error for AuthFailure {}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(client_pin: &str, host_pin: &str, client_bind: &[u8], host_bind: &[u8]) -> bool {
        let (client, a) = PinExchange::start(Side::Client, client_pin);
        let (host, b) = PinExchange::start(Side::Host, host_pin);
        let client = client.finish(&b, client_bind).unwrap();
        let host = host.finish(&a, host_bind).unwrap();
        host.verify_peer(&client.confirmation()).is_ok()
            && client.verify_peer(&host.confirmation()).is_ok()
    }

    #[test]
    fn matching_pin_confirms_both_ways() {
        assert!(run("482913", "482913", b"tls", b"tls"));
        assert!(run(" 482913\n", "482913", b"tls", b"tls"));
    }

    #[test]
    fn wrong_pin_or_split_tls_session_fails() {
        assert!(!run("482914", "482913", b"tls", b"tls"));
        assert!(!run("482913", "482913", b"leg-one", b"leg-two"));
    }

    #[test]
    fn pin_rules() {
        assert!(validate_pin("12345").is_err());
        assert_eq!(validate_pin(" 123456 "), Ok("123456"));
        assert!(validate_pin("123 456").is_err());
        assert!(validate_pin(&"9".repeat(33)).is_err());
    }
}
