// Package identity: what keep-at calls itself on the wire and in HTTP.
//
// VERSION prefers the KEEPAT_VERSION_OVERRIDE env var at compile time (set by
// scripts/build-release.sh from the git tag), falling back to Cargo.toml.
macro_rules! version_str {
    () => {
        match option_env!("KEEPAT_VERSION_OVERRIDE") {
            Some(v) => v,
            None => env!("CARGO_PKG_VERSION"),
        }
    };
}
pub const VERSION: &str = version_str!();

pub const CLIENT_NAME: &str = "keep-at";
pub const ROLE_SEEDER: &str = "seeder";
pub const ROLE_SCRAPER: &str = "scraper";

/// Azureus-style peer ID prefix, independent of the extended handshake string.
pub const PEER_ID_PREFIX: [u8; 8] = *b"-KA0100-";

fn role_client_version(role: &str) -> String {
    format!("{CLIENT_NAME}/{VERSION} ({role})")
}

/// BEP 10 extended handshake "v" string for the main (seeding) client.
pub fn extended_handshake_version() -> String {
    role_client_version(ROLE_SEEDER)
}

/// BEP 10 extended handshake "v" string for the probe (scraper) client.
pub fn scraper_extended_handshake_version() -> String {
    role_client_version(ROLE_SCRAPER)
}

/// HTTP User-Agent for requests to academictorrents.com.
pub fn user_agent() -> String {
    format!("{CLIENT_NAME}/{VERSION} (+https://github.com/tweedge/keep-at)")
}

/// User-Agent on HTTP tracker announces by the main (seeding) client.
pub fn seeder_user_agent() -> String {
    format!("{CLIENT_NAME}/{VERSION} ({ROLE_SEEDER}) (+https://github.com/tweedge/keep-at)")
}

/// User-Agent on scrapes / probe announces. Distinct so AT logs and other
/// keep-at nodes can tell a prober from a real seeder.
pub fn scraper_user_agent() -> String {
    format!("{CLIENT_NAME}/{VERSION} ({ROLE_SCRAPER}) (+https://github.com/tweedge/keep-at)")
}

/// Reports whether a peer's advertised client string identifies it as a
/// keep-at node that is actually seeding (not merely probing). Matches the
/// Go IsKeepAtSeeder semantics: prefix match on "keep-at", excluding the
/// scraper role; the seeder role suffix is not required so older versions
/// advertising the bare "keep-at/version" string still count.
pub fn is_keep_at_seeder(client_name: &str) -> bool {
    if !client_name.starts_with(CLIENT_NAME) {
        return false;
    }
    !client_name.contains(&format!("({ROLE_SCRAPER})"))
}

/// Build a 20-byte peer ID from our 8-byte prefix plus randomness.
pub fn make_peer_id(rng: &mut impl rand::RngCore) -> [u8; 20] {
    let mut id = [0u8; 20];
    id[..8].copy_from_slice(&PEER_ID_PREFIX);
    rng.fill_bytes(&mut id[8..]);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeder_matching() {
        assert!(is_keep_at_seeder("keep-at/0.7.4-beta (seeder)"));
        assert!(is_keep_at_seeder("keep-at/0.6.0"));
        assert!(!is_keep_at_seeder("keep-at/0.7.4-beta (scraper)"));
        assert!(!is_keep_at_seeder("rqbit 9.0.1"));
        assert!(!is_keep_at_seeder(""));
    }

    #[test]
    fn peer_id_prefix() {
        let mut rng = rand::thread_rng();
        let id = make_peer_id(&mut rng);
        assert_eq!(&id[..8], b"-KA0100-");
    }
}
