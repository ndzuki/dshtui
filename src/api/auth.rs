//! 认证：token → 内存 cookie（Notes/03 §1）。
//!
//! - `GET {base}/?token=<token>` → `303 See Other` + `Set-Cookie: dsh-auth-<n>`；
//! - cookie 只存 reqwest 内存 jar（cookie_store），不落盘；
//! - 捕获 Set-Cookie 原文供 WS handshake 使用（WS 握手不走 reqwest jar）；
//! - token 绝不进入日志（脱敏）。

use reqwest::header::SET_COOKIE;

use super::envelope::ClientError;

/// 认证会话：cookie 存于 reqwest jar（内存），此处只保留 WS 握手所需的原始值。
#[derive(Debug, Clone, Default)]
pub struct AuthSession {
    /// 原始 Set-Cookie 的 `name=value` 部分（不含属性）。
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
        .map_err(|e| ClientError::Transport(format!("认证请求失败（{url}）: {e}")))?;

    let status = resp.status();
    if !status.is_redirection() {
        return Err(ClientError::Auth(format!(
            "认证入口应返回 303 重定向，实际 {status}（请检查 token 与 dsh web 版本）"
        )));
    }

    let mut session = AuthSession::default();
    for v in resp.headers().get_all(SET_COOKIE) {
        if let Ok(raw) = v.to_str() {
            // 只保留 name=value 段；属性（HttpOnly/Path/…）不进入 Cookie 头。
            let pair = raw.split(';').next().unwrap_or(raw).trim();
            if !pair.is_empty() {
                session.cookies.push(pair.to_string());
            }
        }
    }
    if !session
        .cookies
        .iter()
        .any(|c| c.starts_with("dsh-auth-"))
    {
        return Err(ClientError::Auth(
            "认证响应缺少 dsh-auth-<n> cookie（官方认证形态可能已变化）".to_string(),
        ));
    }
    tracing::debug!(cookie_count = session.cookies.len(), "认证成功（cookie 仅内存）");
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
