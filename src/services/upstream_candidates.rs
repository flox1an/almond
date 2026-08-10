//! Upstream candidate enumeration: tier order, SSRF normalisation, dedup.
//!
//! Three code paths in `handlers::upstream` ask the same question — which
//! upstream server should I try next, and in what order — and each used to
//! answer it itself. The three walks agreed on the tiers and disagreed on the
//! details: only the `?xs=` tier ever got scheme-candidate expansion, and each
//! walk fetched the author's server list independently, so one request that
//! fell through from a fetch into a proxy walked the last tier twice.
//!
//! The enumeration lives here now. The action taken on each candidate — GET
//! and stream, HEAD and redirect, or proxy — stays with the caller, because
//! that is the only thing the three walks genuinely differ in.

use std::collections::{HashSet, VecDeque};

use nostr_relay_pool::prelude::PublicKey;
use tracing::{debug, warn};

use crate::helpers::server_url_candidates;
use crate::models::AppState;
use crate::services::upload::validate_upstream_url;

/// Where a candidate came from. Carried so the logs still say which tier is
/// being tried without each caller re-deriving it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// `?origin=` on the request.
    CustomOrigin,
    /// `?xs=` on the request (BUD-01).
    XsServer,
    /// `UPSTREAM_SERVERS` from configuration.
    Configured,
    /// The author's own server list, fetched lazily (BUD-03).
    UserServer,
}

impl Tier {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CustomOrigin => "custom origin",
            Self::XsServer => "xs server",
            Self::Configured => "configured upstream",
            Self::UserServer => "user server",
        }
    }
}

/// One upstream worth trying, already validated and normalised.
pub struct Candidate {
    pub tier: Tier,
    /// The normalised server base, as the dedup set sees it.
    pub base: String,
    /// The blob URL to request.
    pub url: String,
}

/// Order the first three tiers into the raw candidates to try.
///
/// Pure: no network, no state, no SSRF resolution. Scheme-candidate expansion
/// applies to every tier here — it only ever expands a scheme-less hint, and
/// never rewrites an explicit `http://` or `https://`, so a configured HTTPS
/// upstream cannot be downgraded by it.
fn plan(
    custom_origin: Option<&str>,
    xs_servers: Option<&[String]>,
    configured: &[String],
) -> Vec<(Tier, String)> {
    let mut planned = Vec::new();
    let mut seen = HashSet::new();

    let mut push = |tier: Tier, hint: &str, planned: &mut Vec<(Tier, String)>| {
        for candidate in server_url_candidates(hint) {
            if seen.insert(candidate.clone()) {
                planned.push((tier, candidate));
            }
        }
    };

    if let Some(origin) = custom_origin {
        push(Tier::CustomOrigin, origin, &mut planned);
    }
    for server in xs_servers.unwrap_or_default() {
        push(Tier::XsServer, server, &mut planned);
    }
    for server in configured {
        push(Tier::Configured, server, &mut planned);
    }

    planned
}

/// Walks the upstream tiers in priority order, one candidate at a time.
///
/// Owns the dedup set and the lazily fetched author server list, so a caller
/// holds nothing but its loop.
pub struct Walk<'a> {
    state: &'a AppState,
    filename: &'a str,
    author_pubkey: Option<&'a PublicKey>,
    pending: VecDeque<(Tier, String)>,
    /// Normalised bases already yielded, so two tiers naming the same server
    /// only produce one request.
    tried: HashSet<String>,
    user_servers_pending: bool,
}

impl<'a> Walk<'a> {
    #[must_use]
    pub fn new(
        state: &'a AppState,
        filename: &'a str,
        custom_origin: Option<&str>,
        xs_servers: Option<&[String]>,
        author_pubkey: Option<&'a PublicKey>,
    ) -> Self {
        Self {
            state,
            filename,
            author_pubkey,
            pending: plan(custom_origin, xs_servers, &state.upstream_servers).into(),
            tried: HashSet::new(),
            user_servers_pending: author_pubkey.is_some(),
        }
    }

    /// The next upstream worth trying, or `None` once the tiers are exhausted.
    ///
    /// Candidates that fail SSRF validation, or that resolve to a server an
    /// earlier tier already offered, are skipped rather than yielded.
    pub async fn next(&mut self) -> Option<Candidate> {
        loop {
            let Some((tier, hint)) = self.pending.pop_front() else {
                if !self.take_user_servers().await {
                    return None;
                }
                continue;
            };

            let base = match validate_upstream_url(&hint).await {
                Ok(url) => url,
                Err(error) => {
                    warn!(
                        "{} URL validation failed (SSRF protection): {hint} - {error}",
                        tier.label()
                    );
                    continue;
                }
            };

            if !self.tried.insert(base.clone()) {
                debug!("Skipping already-tried server: {base}");
                continue;
            }

            let url = format!("{}/{}", base.trim_end_matches('/'), self.filename);
            debug!("Trying {}: {url}", tier.label());
            return Some(Candidate { tier, base, url });
        }
    }

    /// Append the author's server list, at most once per walk.
    ///
    /// Returns whether anything was added; `false` means the walk is over.
    async fn take_user_servers(&mut self) -> bool {
        if !self.user_servers_pending {
            return false;
        }
        self.user_servers_pending = false;

        let Some(pubkey) = self.author_pubkey else {
            return false;
        };

        debug!("Fetching user server list for pubkey: {}", pubkey.to_hex());
        match crate::services::blossom_servers::fetch_user_server_list(self.state, pubkey).await {
            Ok(servers) if !servers.is_empty() => {
                debug!("Fetched {} servers from the user's list (BUD-03)", servers.len());
                for server in &servers {
                    for candidate in server_url_candidates(server) {
                        self.pending.push_back((Tier::UserServer, candidate));
                    }
                }
                true
            }
            Ok(_) => {
                debug!("User server list is empty for pubkey: {}", pubkey.to_hex());
                false
            }
            Err(error) => {
                warn!(
                    "Failed to fetch user server list for pubkey {}: {error}",
                    pubkey.to_hex()
                );
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn servers(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn tiers_are_walked_in_priority_order() {
        let xs = servers(&["https://xs.example"]);
        let configured = servers(&["https://configured.example"]);
        let planned = plan(Some("https://origin.example"), Some(&xs), &configured);

        assert_eq!(
            planned,
            vec![
                (Tier::CustomOrigin, "https://origin.example".to_string()),
                (Tier::XsServer, "https://xs.example".to_string()),
                (Tier::Configured, "https://configured.example".to_string()),
            ]
        );
    }

    #[test]
    fn a_server_named_by_two_tiers_is_planned_once() {
        // The same host arriving as both ?xs= and UPSTREAM_SERVERS used to cost
        // two requests in some walks and one in others.
        let xs = servers(&["https://shared.example"]);
        let configured = servers(&["https://shared.example", "https://other.example"]);
        let planned = plan(None, Some(&xs), &configured);

        assert_eq!(
            planned,
            vec![
                (Tier::XsServer, "https://shared.example".to_string()),
                (Tier::Configured, "https://other.example".to_string()),
            ]
        );
    }

    #[test]
    fn a_scheme_less_hint_expands_in_every_tier() {
        // This expansion used to apply only to ?xs=, which is why a
        // scheme-less UPSTREAM_SERVERS entry silently never worked.
        let configured = servers(&["configured.example"]);
        let planned = plan(Some("origin.example"), None, &configured);

        assert_eq!(
            planned,
            vec![
                (Tier::CustomOrigin, "https://origin.example".to_string()),
                (Tier::CustomOrigin, "http://origin.example".to_string()),
                (Tier::Configured, "https://configured.example".to_string()),
                (Tier::Configured, "http://configured.example".to_string()),
            ]
        );
    }

    #[test]
    fn an_explicit_scheme_is_never_rewritten() {
        // Expanding uniformly must not downgrade a configured HTTPS upstream.
        let configured = servers(&["https://secure.example", "http://plain.example"]);
        let planned = plan(None, None, &configured);

        assert_eq!(
            planned,
            vec![
                (Tier::Configured, "https://secure.example".to_string()),
                (Tier::Configured, "http://plain.example".to_string()),
            ]
        );
    }

    #[test]
    fn absent_tiers_contribute_nothing() {
        assert!(plan(None, None, &[]).is_empty());
        assert!(plan(None, Some(&[]), &[]).is_empty());
    }
}
