//! What a hosted server remembers about a user after their session is gone:
//! the Google grant their MCP connector reads with, and the connector itself.
//!
//! A session is a browser tab's worth of sign-in, in memory. A connector is
//! an assistant asking about somebody's numbers next Tuesday, with nobody at
//! a browser. So this is the one thing a hosted server keeps: one row per
//! Google account in Supabase's `hosted_grants`, holding the refresh token
//! *sealed* — ChaCha20-Poly1305 under a key that lives in Secret Manager and
//! never in the database — and the SHA-256 of the connector token, so the
//! table is not a list of working links either.
//!
//! The table is locked to the public key the CLI ships with (row-level
//! security, every grant revoked); this module reaches it with the service
//! key, which only the hosted server has.

use anyhow::{bail, Context, Result};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::auth::Account;

/// Who a presented connector token says is asking, before the store is asked.
pub(super) enum Holder {
    /// A sealed token: the account and the property, opened.
    Account { sub: String, property: String },
    /// A link from before the property was sealed in — the account's HMAC,
    /// found by its hash in the table, and the property it may have carried
    /// after a hyphen.
    Legacy {
        token: String,
        property: Option<String>,
    },
}

impl Holder {
    /// The property the token named, if it named one.
    pub fn property(&self) -> Option<&str> {
        match self {
            Holder::Account { property, .. } => Some(property.as_str()),
            Holder::Legacy { property, .. } => property.as_deref(),
        }
        .filter(|p| !p.is_empty())
    }
}

/// One user's row, as the server needs it back.
pub(super) struct Grant {
    pub account: Account,
    pub refresh_token: String,
}

pub(super) struct Grants {
    url: String,
    service_key: String,
    key: [u8; 32],
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct Row {
    user_id: String,
    #[serde(default)]
    email: Option<String>,
    refresh_sealed: String,
}

impl Grants {
    /// From the environment: `ANACRAFT_SUPABASE_SERVICE_KEY` and
    /// `ANACRAFT_GRANT_KEY` (32 bytes, base64). `None` when neither is set —
    /// a hosted server without them signs people in and hands out tags, and
    /// says plainly that its MCP connector is not switched on. One without
    /// the other is a misconfiguration, and refuses to start.
    pub fn from_env() -> Result<Option<Grants>> {
        let service = non_empty("ANACRAFT_SUPABASE_SERVICE_KEY");
        let sealing = non_empty("ANACRAFT_GRANT_KEY");
        let (service_key, sealing) = match (service, sealing) {
            (None, None) => return Ok(None),
            (Some(service), Some(sealing)) => (service, sealing),
            _ => bail!(
                "ANACRAFT_SUPABASE_SERVICE_KEY and ANACRAFT_GRANT_KEY come as a pair — \
                 the hosted MCP connector needs both, or neither to leave it off"
            ),
        };
        let key: [u8; 32] = STANDARD
            .decode(sealing.trim())
            .ok()
            .and_then(|raw| raw.try_into().ok())
            .context("ANACRAFT_GRANT_KEY must be 32 bytes, base64 (openssl rand -base64 32)")?;
        let (url, _) = crate::license::project()
            .context("the hosted MCP connector needs the Supabase project this build uses")?;
        Ok(Some(Grants {
            url,
            service_key,
            key,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
        }))
    }

    /// The connector token for an account: worked out, not minted, so the
    /// page can show it again any time without the server keeping it. An HMAC
    /// under the sealing key — the account id is public, the key is not.
    pub fn connector_token(&self, account: &Account) -> String {
        connector_token(&self.key, &account.sub)
    }

    /// The token a connector link carries: `<sub>:<property>`, sealed. Neither
    /// id is readable in it and neither can be changed without the key, so a
    /// link reads the one property it was handed out for.
    pub fn connector_link(&self, account: &Account, property: &str) -> String {
        seal_connector(&self.key, &account.sub, property.trim())
    }

    /// What a presented connector token says, without reaching the store.
    pub fn read(&self, presented: &str) -> Holder {
        if let Some((sub, property)) = open_connector(&self.key, presented) {
            return Holder::Account { sub, property };
        }
        // An account token is hex and a property id is digits, so the one
        // hyphen is unambiguous — and a token from before properties rode in
        // it has none.
        match presented.split_once('-') {
            Some((token, property)) if !property.is_empty() => Holder::Legacy {
                token: token.to_string(),
                property: Some(property.to_string()),
            },
            _ => Holder::Legacy {
                token: presented.trim_end_matches('-').to_string(),
                property: None,
            },
        }
    }

    /// Remember this account's grant, sealed, and the connector that reads
    /// with it. Called on every sign-in that came back with a refresh token.
    pub async fn save(&self, account: &Account, refresh_token: &str) -> Result<()> {
        let token = self.connector_token(account);
        let body = json!({
            "user_id": account.sub,
            "email": account.email,
            "refresh_sealed": seal(&self.key, refresh_token)?,
            "connector_hash": digest(&token),
            "updated_at": chrono::Utc::now().to_rfc3339(),
        });
        let res = self
            .http
            .post(format!("{}/rest/v1/hosted_grants", self.url))
            .header("apikey", &self.service_key)
            .bearer_auth(&self.service_key)
            .header("Prefer", "resolution=merge-duplicates,return=minimal")
            .json(&body)
            .send()
            .await
            .context("reaching the grant store")?;
        if !res.status().is_success() {
            bail!("the grant store answered {}", res.status());
        }
        Ok(())
    }

    /// Whether this account already has a grant stored — so a returning
    /// visitor whose sign-in came back without a refresh token (Google only
    /// issues one on consent) is not sent round the consent screen again.
    pub async fn has(&self, account: &Account) -> Result<bool> {
        Ok(self.find("user_id", &account.sub).await?.is_some())
    }

    /// The grant a presented connector token opens, if any.
    pub async fn open(&self, holder: &Holder) -> Result<Option<Grant>> {
        let row = match holder {
            // The seal already proved the token is this server's. The row
            // still has to be there: forgetting an account turns its links off.
            Holder::Account { sub, .. } => self.find("user_id", sub).await?,
            Holder::Legacy { token, .. } => {
                let Some(row) = self.find("connector_hash", &digest(token)).await? else {
                    return Ok(None);
                };
                // The hash matched, so the token was this server's once.
                // Re-derive it to be sure it still is: a rotated key turns
                // every old link off.
                if !super::same(&connector_token(&self.key, &row.user_id), token) {
                    return Ok(None);
                }
                Some(row)
            }
        };
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(Grant {
            refresh_token: unseal(&self.key, &row.refresh_sealed)?,
            account: Account {
                sub: row.user_id,
                email: row.email,
            },
        }))
    }

    /// Forget an account's grant: the connector stops, and the refresh token
    /// is gone from the table.
    pub async fn forget(&self, account: &Account) -> Result<()> {
        let res = self
            .http
            .delete(format!("{}/rest/v1/hosted_grants", self.url))
            .query(&[("user_id", format!("eq.{}", account.sub))])
            .header("apikey", &self.service_key)
            .bearer_auth(&self.service_key)
            .send()
            .await
            .context("reaching the grant store")?;
        if !res.status().is_success() {
            bail!("the grant store answered {}", res.status());
        }
        Ok(())
    }

    async fn find(&self, column: &str, value: &str) -> Result<Option<Row>> {
        let res = self
            .http
            .get(format!("{}/rest/v1/hosted_grants", self.url))
            .query(&[
                ("select", "user_id,email,refresh_sealed".to_string()),
                (column, format!("eq.{value}")),
                ("limit", "1".to_string()),
            ])
            .header("apikey", &self.service_key)
            .bearer_auth(&self.service_key)
            .send()
            .await
            .context("reaching the grant store")?;
        if !res.status().is_success() {
            bail!("the grant store answered {}", res.status());
        }
        let rows: Vec<Row> = res.json().await.context("reading the grant store")?;
        Ok(rows.into_iter().next())
    }
}

fn non_empty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty())
}

fn connector_token(key: &[u8; 32], sub: &str) -> String {
    use hmac::Mac;
    let mut mac =
        <hmac::Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac accepts a key of any length");
    mac.update(format!("anacraft-connector:{sub}").as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .take(20)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    use hmac::Mac;
    let mut mac =
        <hmac::Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac accepts a key of any length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// A connector token: `<sub>:<property>` under ChaCha20-Poly1305, url-safe.
///
/// Its own key, derived from the grant key, so these never share a key with
/// the sealed refresh tokens. The nonce is an HMAC of what is sealed rather
/// than random, because the page shows the same link again on every visit
/// without the server keeping it — the one thing that gives away is that two
/// links for the same account and property are the same link.
fn seal_connector(key: &[u8; 32], sub: &str, property: &str) -> String {
    let plain = format!("{sub}:{property}");
    let nonce = hmac(key, format!("anacraft-connector-nonce:{plain}").as_bytes());
    let nonce = Nonce::from_slice(&nonce[..12]);
    let cipher = ChaCha20Poly1305::new(&hmac(key, b"anacraft-connector-seal").into());
    let sealed = cipher
        .encrypt(nonce, plain.as_bytes())
        .expect("sealing a few bytes in memory does not fail");
    let mut out = nonce.to_vec();
    out.extend(sealed);
    URL_SAFE_NO_PAD.encode(out)
}

/// The account and property a connector token was sealed with, or `None`
/// for anything this key did not seal — an older link, or a forged one.
fn open_connector(key: &[u8; 32], presented: &str) -> Option<(String, String)> {
    let raw = URL_SAFE_NO_PAD.decode(presented.trim()).ok()?;
    if raw.len() < 12 + 16 {
        return None;
    }
    let (nonce, body) = raw.split_at(12);
    let cipher = ChaCha20Poly1305::new(&hmac(key, b"anacraft-connector-seal").into());
    let plain = cipher.decrypt(Nonce::from_slice(nonce), body).ok()?;
    let (sub, property) = String::from_utf8(plain)
        .ok()?
        .split_once(':')
        .map(|(s, p)| (s.to_string(), p.to_string()))?;
    (!sub.is_empty()).then_some((sub, property))
}

fn digest(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A fresh nonce each time, carried in front of the ciphertext.
fn seal(key: &[u8; 32], plain: &str) -> Result<String> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let sealed = cipher
        .encrypt(&nonce, plain.as_bytes())
        .map_err(|_| anyhow::anyhow!("sealing the grant failed"))?;
    let mut out = nonce.to_vec();
    out.extend(sealed);
    Ok(STANDARD.encode(out))
}

fn unseal(key: &[u8; 32], sealed: &str) -> Result<String> {
    let raw = STANDARD
        .decode(sealed)
        .context("a stored grant is not base64")?;
    if raw.len() < 12 {
        bail!("a stored grant is too short to be one");
    }
    let (nonce, body) = raw.split_at(12);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let plain = cipher
        .decrypt(Nonce::from_slice(nonce), body)
        .map_err(|_| anyhow::anyhow!("a stored grant would not open — was the key rotated?"))?;
    String::from_utf8(plain).context("a stored grant is not text")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_grant_opens_with_its_key_and_no_other() {
        let key = [7u8; 32];
        let sealed = seal(&key, "1//refresh-token").unwrap();
        assert!(!sealed.contains("refresh"));
        assert_eq!(unseal(&key, &sealed).unwrap(), "1//refresh-token");
        assert!(unseal(&[8u8; 32], &sealed).is_err());
        // Two seals of the same token differ: the nonce is fresh each time.
        assert_ne!(sealed, seal(&key, "1//refresh-token").unwrap());
    }

    #[test]
    fn a_connector_token_is_stable_per_account_and_keyed() {
        let key = [7u8; 32];
        let a = connector_token(&key, "110147");
        assert_eq!(a, connector_token(&key, "110147"));
        assert_eq!(a.len(), 40);
        assert_ne!(a, connector_token(&key, "110148"));
        assert_ne!(a, connector_token(&[8u8; 32], "110147"));
        assert_ne!(digest(&a), a, "the table holds the hash, not the token");
    }

    #[test]
    fn a_connector_link_seals_its_account_and_property() {
        let key = [7u8; 32];
        let link = seal_connector(&key, "110147", "552157097");
        assert!(!link.contains("110147") && !link.contains("552157097"));
        assert_eq!(link, seal_connector(&key, "110147", "552157097"), "stable");
        assert_ne!(link, seal_connector(&key, "110147", "552157098"));
        assert!(link
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(
            open_connector(&key, &link),
            Some(("110147".to_string(), "552157097".to_string()))
        );
        assert_eq!(open_connector(&[8u8; 32], &link), None, "another key");

        // One bit off anywhere and it opens to nothing, rather than to
        // somebody else's property.
        let mut raw = URL_SAFE_NO_PAD.decode(&link).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        assert_eq!(open_connector(&key, &URL_SAFE_NO_PAD.encode(raw)), None);

        // No property is still a link, one that names none.
        let bare = seal_connector(&key, "110147", "");
        assert_eq!(
            open_connector(&key, &bare),
            Some(("110147".to_string(), String::new()))
        );
    }

    #[test]
    fn a_link_from_before_the_seal_still_reads() {
        let grants = Grants {
            url: String::new(),
            service_key: String::new(),
            key: [7u8; 32],
            http: reqwest::Client::new(),
        };
        let old = connector_token(&[7u8; 32], "110147");
        let Holder::Legacy { token, property } = grants.read(&format!("{old}-552157097")) else {
            panic!("an old link read as sealed");
        };
        assert_eq!(
            (token.as_str(), property.as_deref()),
            (old.as_str(), Some("552157097"))
        );
        let Holder::Legacy { token, property } = grants.read(&old) else {
            panic!("an old link read as sealed");
        };
        assert_eq!((token, property), (old, None));

        let sealed = seal_connector(&[7u8; 32], "110147", "552157097");
        assert_eq!(grants.read(&sealed).property(), Some("552157097"));
    }
}
