//! Auth: token → in-memory cookie (Notes/03 §1).
//!
//! - `GET {base}/?token=<token>` → `303 See Other` + `Set-Cookie: dsh-auth-<n>`;
//! - the cookie lives only in the reqwest in-memory jar (cookie_store), never
//!   on disk;
//! - the raw Set-Cookie value is captured for the WS handshake (the WS
//!   handshake does not go through the reqwest jar);
//! - the token never enters logs (redaction).

use reqwest::header::SET_COOKIE;

use super::envelope::ClientError;

/// Auth session: the cookie lives in the reqwest jar (in memory); only the raw
/// value needed for the WS handshake is kept here.
#[derive(Debug, Clone, Default)]
pub struct AuthSession {
    /// The `name=value` part of the raw Set-Cookie (attributes excluded).
    pub cookies: Vec<String>,
}

impl AuthSession {
    pub fn cookie_header(&self) -> String {
        self.cookies.join("; ")
    }
}

pub async fn authenticate(
    http: &reqwest::Client,
    base: &str,
    token: &str,
) -> Result<AuthSession, ClientError> {
    let url = format!("{base}/");
    let resp = http
        .get(&url)
        .query(&[("token", token)])
        .send()
        .await
        .map_err(|e| {
            // without_url(): reqwest error Display embeds the full request URL
            // including the `?token=` query — strip it so the secret can never
            // reach the guidance screen or tracing logs (redaction invariant).
            ClientError::Transport(format!("认证请求失败（{url}）: {}", e.without_url()))
        })?;

    let status = resp.status();
    if !status.is_redirection() {
        return Err(ClientError::Auth(format!(
            "认证入口应返回 303 重定向，实际 {status}（请检查 token 与 dsh web 版本）"
        )));
    }

    let mut session = AuthSession::default();
    for v in resp.headers().get_all(SET_COOKIE) {
        if let Ok(raw) = v.to_str() {
            // Keep only the name=value segment; attributes (HttpOnly/Path/…)
            // never enter the Cookie header.
            let pair = raw.split(';').next().unwrap_or(raw).trim();
            if !pair.is_empty() {
                session.cookies.push(pair.to_string());
            }
        }
    }
    if !session.cookies.iter().any(|c| c.starts_with("dsh-auth-")) {
        return Err(ClientError::Auth(
            "认证响应缺少 dsh-auth-<n> cookie（官方认证形态可能已变化）".to_string(),
        ));
    }
    tracing::debug!(
        cookie_count = session.cookies.len(),
        "认证成功（cookie 仅内存）"
    );
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_header_joins_pairs() {
        let s = AuthSession {
            cookies: vec!["dsh-auth-0=v1.x.y".into(), "sid=abc".into()],
        };
        assert_eq!(s.cookie_header(), "dsh-auth-0=v1.x.y; sid=abc");
    }

    #[test]
    fn cookie_pair_strips_attributes() {
        let raw = "dsh-auth-0=v1.proto.sig; HttpOnly; SameSite=Strict; Max-Age=15552000";
        let pair = raw.split(';').next().unwrap().trim();
        assert_eq!(pair, "dsh-auth-0=v1.proto.sig");
    }
}
