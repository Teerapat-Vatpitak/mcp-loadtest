//! Authorization callback parsing and an explicit loopback listener.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

use crate::secret::SecretValue;
use crate::{AuthError, AuthResult};

/// Parsed authorization response with an opaque authorization code.
#[derive(Clone)]
pub struct AuthorizationCallback {
    pub(crate) code: SecretValue,
    pub(crate) state: String,
    pub(crate) issuer: Option<String>,
}

impl std::fmt::Debug for AuthorizationCallback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthorizationCallback")
            .field("code", &"[REDACTED]")
            .field("state", &"[REDACTED]")
            .field("issuer", &self.issuer)
            .finish()
    }
}

impl AuthorizationCallback {
    /// Parse an OAuth redirect URL without exposing the authorization code.
    pub fn from_url(callback_url: &Url) -> AuthResult<Self> {
        if callback_url.fragment().is_some() {
            return Err(AuthError::InvalidAuthorizationCallback);
        }
        let mut code = None;
        let mut state = None;
        let mut issuer = None;
        let mut error = None;
        for (name, value) in callback_url.query_pairs() {
            match name.as_ref() {
                "code" if code.is_none() => code = Some(SecretValue::new(value.into_owned())),
                "state" if state.is_none() => state = Some(value.into_owned()),
                "iss" if issuer.is_none() => issuer = Some(value.into_owned()),
                "error" if error.is_none() => error = Some(value.into_owned()),
                "code" | "state" | "iss" | "error" => {
                    return Err(AuthError::InvalidAuthorizationCallback);
                }
                _ => {}
            }
        }
        if let Some(error) = error {
            return Err(AuthError::oauth_rejected(&error));
        }
        Ok(Self {
            code: code.ok_or(AuthError::MissingAuthorizationCode)?,
            state: state.ok_or(AuthError::StateMismatch)?,
            issuer,
        })
    }
}

/// One-shot HTTP listener for an OAuth loopback redirect.
///
/// Binding is explicit and this type never opens a browser.
#[derive(Debug)]
pub struct LoopbackCallback {
    listener: TcpListener,
    redirect_uri: Url,
}

impl LoopbackCallback {
    /// Bind a random port on IPv4 loopback using the `/callback` path.
    pub async fn bind() -> AuthResult<Self> {
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .map_err(|_| AuthError::LoopbackCallback { operation: "bind" })?;
        let address = listener
            .local_addr()
            .map_err(|_| AuthError::LoopbackCallback {
                operation: "inspect listener",
            })?;
        let redirect_uri = Url::parse(&format!("http://127.0.0.1:{}/callback", address.port()))
            .map_err(|_| AuthError::LoopbackCallback {
                operation: "build redirect URI",
            })?;
        Ok(Self {
            listener,
            redirect_uri,
        })
    }

    /// Return the exact redirect URI registered for this listener.
    pub fn redirect_uri(&self) -> &Url {
        &self.redirect_uri
    }

    /// Wait for one callback request and return its URL.
    pub async fn wait(self, timeout: Duration) -> AuthResult<Url> {
        tokio::time::timeout(timeout, self.wait_inner())
            .await
            .map_err(|_| AuthError::LoopbackCallback {
                operation: "wait timeout",
            })?
    }

    async fn wait_inner(self) -> AuthResult<Url> {
        let (mut stream, peer) =
            self.listener
                .accept()
                .await
                .map_err(|_| AuthError::LoopbackCallback {
                    operation: "accept",
                })?;
        if !peer.ip().is_loopback() {
            return Err(AuthError::LoopbackCallback {
                operation: "peer validation",
            });
        }

        let mut request = Vec::with_capacity(1024);
        let mut buffer = [0_u8; 1024];
        loop {
            let count = stream
                .read(&mut buffer)
                .await
                .map_err(|_| AuthError::LoopbackCallback { operation: "read" })?;
            if count == 0 || request.len().saturating_add(count) > 16 * 1024 {
                return Err(AuthError::LoopbackCallback {
                    operation: "parse request",
                });
            }
            request.extend_from_slice(&buffer[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }

        let request = std::str::from_utf8(&request).map_err(|_| AuthError::LoopbackCallback {
            operation: "parse request",
        })?;
        let request_line = request.lines().next().ok_or(AuthError::LoopbackCallback {
            operation: "parse request",
        })?;
        let mut pieces = request_line.split_ascii_whitespace();
        if pieces.next() != Some("GET") {
            return Err(AuthError::LoopbackCallback {
                operation: "validate method",
            });
        }
        let target = pieces.next().ok_or(AuthError::LoopbackCallback {
            operation: "parse target",
        })?;
        if !target.starts_with('/') || target.starts_with("//") {
            return Err(AuthError::LoopbackCallback {
                operation: "validate target",
            });
        }
        let callback_url =
            self.redirect_uri
                .join(target)
                .map_err(|_| AuthError::LoopbackCallback {
                    operation: "parse target",
                })?;

        let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\n\
                         Cache-Control: no-store\r\nContent-Length: 49\r\nConnection: close\r\n\r\n\
                         Authorization received. You may close this window.";
        stream
            .write_all(response)
            .await
            .map_err(|_| AuthError::LoopbackCallback {
                operation: "write response",
            })?;
        Ok(callback_url)
    }
}
