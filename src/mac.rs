//! The body an Acknowledge button posts: `a1.<instance>.<tag>`.
//!
//! The button's ntfy token can only write to the acknowledgement topic, and
//! it is visible to anyone who can read a notification. The tag is what makes
//! a leaked token useless for silencing alerts: only bodies this program
//! signed are accepted, and each one names exactly one instance.
use crate::instance::InstanceId;
use crate::secret::Secret;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

const VERSION: &str = "a1";

fn mac_for(key: &Secret, id: &InstanceId) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key.expose().as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(format!("{VERSION}|{}", id.as_str()).as_bytes());
    mac
}

pub fn button_body(key: &Secret, id: &InstanceId) -> String {
    let tag = mac_for(key, id).finalize().into_bytes();
    format!("{VERSION}.{}.{}", id.as_str(), hex::encode(&tag[..16]))
}

/// The instance a body acknowledges, or None. The comparison runs in
/// constant time (`verify_truncated_left`).
pub fn verify(key: &Secret, body: &str) -> Option<InstanceId> {
    let mut parts = body.trim().splitn(3, '.');
    let (version, id, tag) = (parts.next()?, parts.next()?, parts.next()?);
    if version != VERSION || tag.len() != 32 {
        return None;
    }
    let id = InstanceId::parse(id)?;
    let tag = hex::decode(tag).ok()?;
    mac_for(key, &id).verify_truncated_left(&tag).ok()?;
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::InstanceId;
    use crate::secret::Secret;

    fn key() -> Secret {
        Secret::from("3f9a0c1e5b7d2f4a6c8e0b1d3f5a7c9e".to_string())
    }
    fn id(s: &str) -> InstanceId {
        InstanceId::parse(s).unwrap()
    }

    #[test]
    fn a_body_verifies_to_its_instance() {
        let body = button_body(&key(), &id("0123456789abcdef"));
        assert!(body.starts_with("a1.0123456789abcdef."));
        assert_eq!(body.len(), 3 + 16 + 1 + 32);
        assert_eq!(verify(&key(), &body), Some(id("0123456789abcdef")));
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let body = format!(" {}\n", button_body(&key(), &id("0123456789abcdef")));
        assert!(verify(&key(), &body).is_some());
    }

    #[test]
    fn a_changed_tag_is_rejected() {
        let mut body = button_body(&key(), &id("0123456789abcdef"));
        let last = body.pop().unwrap();
        body.push(if last == '0' { '1' } else { '0' });
        assert_eq!(verify(&key(), &body), None);
    }

    #[test]
    fn a_tag_does_not_carry_over_to_another_instance() {
        let body = button_body(&key(), &id("0123456789abcdef"));
        let moved = body.replace("0123456789abcdef", "fedcba9876543210");
        assert_eq!(verify(&key(), &moved), None);
    }

    #[test]
    fn another_key_or_version_is_rejected() {
        let body = button_body(&key(), &id("0123456789abcdef"));
        assert_eq!(verify(&Secret::from("other".to_string()), &body), None);
        assert_eq!(verify(&key(), &body.replacen("a1.", "a2.", 1)), None);
        assert_eq!(verify(&key(), "a1.0123456789abcdef"), None);
        assert_eq!(verify(&key(), ""), None);
    }
}
