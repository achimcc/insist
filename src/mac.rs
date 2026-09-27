//! The body an Acknowledge button posts: `a2.<instance>.<expiry>.<tag>`.
//!
//! The button's ntfy token can only write to the acknowledgement topic, and
//! it is visible to anyone who can read a notification. The tag is what makes
//! a leaked token useless for silencing alerts: only bodies this program
//! signed are accepted, and each one names exactly one instance.
//!
//! Since 0.4.0 (audit 3, B123) the tag also covers an expiry, in Unix
//! seconds: a button seen once no longer acknowledges its instance forever.
//! `a1.<instance>.<tag>` — the format without expiry that 0.1 to 0.3 sent —
//! is still accepted, so a button already on a phone keeps working across
//! the upgrade. An `a1` body cannot be made from an `a2` one: the tags are
//! over different texts, and only the key makes either.
//!
//! Two keys verify: the current one and, during a rotation, the previous
//! one (`ACK_HMAC_KEY_PREVIOUS`). No key id travels in the body — trying
//! both costs one more HMAC, and a body names nothing about the keys.
use crate::instance::InstanceId;
use crate::secret::Secret;
use hmac::{Hmac, KeyInit, Mac};
use jiff::Timestamp;
use sha2::Sha256;

const LEGACY: &str = "a1";
const VERSION: &str = "a2";

fn mac_for(key: &Secret, text: &str) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key.expose().as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(text.as_bytes());
    mac
}

fn signed_text(id: &InstanceId, expires: Option<i64>) -> String {
    match expires {
        None => format!("{LEGACY}|{}", id.as_str()),
        Some(exp) => format!("{VERSION}|{}|{exp}", id.as_str()),
    }
}

pub fn button_body(key: &Secret, id: &InstanceId, expires: Timestamp) -> String {
    let exp = expires.as_second();
    let tag = mac_for(key, &signed_text(id, Some(exp)))
        .finalize()
        .into_bytes();
    format!(
        "{VERSION}.{}.{exp}.{}",
        id.as_str(),
        hex::encode(&tag[..16])
    )
}

/// The instance a body acknowledges, or None: a wrong tag, an unknown
/// version, or an `a2` body past its expiry. The comparison runs in
/// constant time (`verify_truncated_left`).
pub fn verify(
    key: &Secret,
    previous: Option<&Secret>,
    body: &str,
    now: Timestamp,
) -> Option<InstanceId> {
    let parts: Vec<&str> = body.trim().split('.').collect();
    let (id, expires, tag) = match parts.as_slice() {
        [LEGACY, id, tag] => (*id, None, *tag),
        [VERSION, id, exp, tag] => {
            // Digits only: `+5` or ` 5` would parse, and are not what we wrote.
            if exp.is_empty() || !exp.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            (*id, Some(exp.parse::<i64>().ok()?), *tag)
        }
        _ => return None,
    };
    if tag.len() != 32 {
        return None;
    }
    let id = InstanceId::parse(id)?;
    let tag = hex::decode(tag).ok()?;
    let text = signed_text(&id, expires);
    let signed = std::iter::once(key)
        .chain(previous)
        .any(|k| mac_for(k, &text).verify_truncated_left(&tag).is_ok());
    if !signed || expires.is_some_and(|exp| now.as_second() > exp) {
        return None;
    }
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
    fn old_key() -> Secret {
        Secret::from("0b1d3f5a7c9e3f9a0c1e5b7d2f4a6c8e".to_string())
    }
    fn id(s: &str) -> InstanceId {
        InstanceId::parse(s).unwrap()
    }
    fn now() -> Timestamp {
        "2026-09-27T12:00:00Z".parse().unwrap()
    }
    fn later(secs: i64) -> Timestamp {
        now() + jiff::SignedDuration::from_secs(secs)
    }
    fn body() -> String {
        button_body(&key(), &id("0123456789abcdef"), later(3600))
    }
    fn check(body: &str) -> Option<InstanceId> {
        verify(&key(), None, body, now())
    }

    /// The format 0.1 to 0.3 sent, written out here from its definition
    /// (HMAC-SHA256 over `a1|<id>`, first 16 bytes, hex) rather than through
    /// this module, so a change to it cannot agree with itself.
    fn legacy_body(key: &Secret, id: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(key.expose().as_bytes()).unwrap();
        mac.update(format!("a1|{id}").as_bytes());
        let tag = mac.finalize().into_bytes();
        format!("a1.{id}.{}", hex::encode(&tag[..16]))
    }

    #[test]
    fn a_body_verifies_to_its_instance_until_it_expires() {
        let b = body();
        let exp = later(3600).as_second();
        assert!(b.starts_with(&format!("a2.0123456789abcdef.{exp}.")), "{b}");
        assert_eq!(check(&b), Some(id("0123456789abcdef")));
        let at = |secs| verify(&key(), None, &b, later(secs));
        assert_eq!(at(3600), Some(id("0123456789abcdef")));
        assert_eq!(at(3601), None, "a button past its expiry");
    }

    #[test]
    fn a_changed_expiry_is_rejected() {
        let b = body();
        let exp = later(3600).as_second().to_string();
        let pushed = b.replace(&exp, &later(99_999).as_second().to_string());
        assert_eq!(check(&pushed), None);
        assert_eq!(check(&b.replace(&exp, &format!("+{exp}"))), None);
    }

    #[test]
    fn a_button_from_before_the_upgrade_still_acknowledges() {
        let old = legacy_body(&key(), "0123456789abcdef");
        assert_eq!(check(&old), Some(id("0123456789abcdef")));
        // ...but a forged one does not, and neither does a legacy body
        // turned into the new format.
        assert_eq!(
            check("a1.0123456789abcdef.00000000000000000000000000000000"),
            None
        );
        let tag = old.rsplit('.').next().unwrap();
        let exp = later(60).as_second();
        assert_eq!(check(&format!("a2.0123456789abcdef.{exp}.{tag}")), None);
    }

    #[test]
    fn during_a_rotation_the_previous_key_still_verifies() {
        let signed_before = button_body(&old_key(), &id("0123456789abcdef"), later(60));
        assert_eq!(check(&signed_before), None, "one key alone");
        assert_eq!(
            verify(&key(), Some(&old_key()), &signed_before, now()),
            Some(id("0123456789abcdef"))
        );
        let legacy = legacy_body(&old_key(), "0123456789abcdef");
        assert!(verify(&key(), Some(&old_key()), &legacy, now()).is_some());
        let stranger = Secret::from("ffffffffffffffffffffffffffffffff".to_string());
        assert_eq!(verify(&key(), Some(&stranger), &signed_before, now()), None);
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        assert!(check(&format!(" {}\n", body())).is_some());
    }

    #[test]
    fn a_changed_tag_is_rejected() {
        let mut b = body();
        let last = b.pop().unwrap();
        b.push(if last == '0' { '1' } else { '0' });
        assert_eq!(check(&b), None);
    }

    #[test]
    fn a_tag_does_not_carry_over_to_another_instance() {
        let moved = body().replace("0123456789abcdef", "fedcba9876543210");
        assert_eq!(check(&moved), None);
    }

    #[test]
    fn another_key_or_version_is_rejected() {
        let b = body();
        assert_eq!(
            verify(&Secret::from("other".to_string()), None, &b, now()),
            None
        );
        assert_eq!(check(&b.replacen("a2.", "a3.", 1)), None);
        assert_eq!(check(&b.replacen("a2.", "a1.", 1)), None);
        assert_eq!(check("a2.0123456789abcdef"), None);
        assert_eq!(check(""), None);
    }
}
