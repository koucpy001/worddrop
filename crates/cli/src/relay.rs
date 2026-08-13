//! Unified relay-mode parsing shared by CLI and GUI.
//!
//! User-facing relay configuration accepts three states:
//!
//! - `public` / `default` → [`iroh::RelayMode::Default`]: iroh's built-in
//!   public relay fleet (`default.iroh.network`).
//! - `disabled` / `off` / `none` → [`iroh::RelayMode::Disabled`]: no relay at
//!   all (loopback direct only) — the escape hatch used by tests.
//! - anything else → parsed as a relay URL → [`iroh::RelayMode::Custom`]:
//!   a self-hosted relay (`https://relay.example.com`).
//!
//! The CLI (Todo 2) and the Flutter GUI (Todo 3) both resolve their
//! `relay_url` config through this one function so the accepted spellings and
//! error wording stay identical across platforms.

use std::str::FromStr;

/// Map a user-supplied `relay_url` config value to an [`iroh::RelayMode`].
///
/// Matching is case-insensitive. A value that is not one of the keywords must
/// be a valid relay URL, otherwise the original string is echoed back in the
/// error for easy diagnosis.
pub fn relay_mode_from_url(url: &str) -> Result<iroh::RelayMode, String> {
    match url.to_ascii_lowercase().as_str() {
        "public" | "default" => Ok(iroh::RelayMode::Default),
        "disabled" | "off" | "none" => Ok(iroh::RelayMode::Disabled),
        _ => {
            let relay = iroh::RelayUrl::from_str(url)
                .map_err(|err| format!("invalid relay URL {url:?}: {err}"))?;
            // iroh 1.0.3's RelayUrl parser accepts any URL scheme, but the
            // relay protocol only speaks http(s)/ws(s). Reject the rest up
            // front so a typo like `mqtts://...` fails loudly at config time
            // instead of being silently re-dialed as wss at connect time.
            if !matches!(relay.scheme(), "http" | "https" | "ws" | "wss") {
                return Err(format!(
                    "invalid relay URL {url:?}: unsupported scheme {:?} (expected http, https, ws or wss)",
                    relay.scheme()
                ));
            }
            Ok(iroh::RelayMode::Custom(relay.into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_map_to_the_three_modes() {
        assert_eq!(relay_mode_from_url("public"), Ok(iroh::RelayMode::Default));
        assert_eq!(relay_mode_from_url("DEFAULT"), Ok(iroh::RelayMode::Default));
        assert_eq!(
            relay_mode_from_url("disabled"),
            Ok(iroh::RelayMode::Disabled)
        );
        assert_eq!(relay_mode_from_url("off"), Ok(iroh::RelayMode::Disabled));
        assert_eq!(relay_mode_from_url("NONE"), Ok(iroh::RelayMode::Disabled));
    }

    #[test]
    fn a_relay_url_maps_to_custom() {
        assert!(matches!(
            relay_mode_from_url("https://relay.example.com"),
            Ok(iroh::RelayMode::Custom(_))
        ));
    }

    #[test]
    fn a_foreign_scheme_is_rejected() {
        // iroh relays only speak the default (https/ws) schemes.
        assert!(relay_mode_from_url("mqtts://x").is_err());
    }

    #[test]
    fn empty_and_hostless_urls_are_rejected() {
        assert!(relay_mode_from_url("").is_err());
        assert!(relay_mode_from_url("http://").is_err());
    }
}
