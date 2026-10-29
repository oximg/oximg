//! The container credential endpoint: ECS task roles and EKS Pod
//! Identity. ECS sets `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`, a path
//! on `169.254.170.2`. EKS Pod Identity sets
//! `AWS_CONTAINER_CREDENTIALS_FULL_URI` (`http://169.254.170.23/v1/credentials`)
//! and `AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE`.
//!
//! The endpoint answers one GET with the JSON that `keys_from_json`
//! reads. The rules below follow the AWS SDKs, with one difference:
//!
//! - A full URI over `http://` may only name a loopback address or one
//!   of the ECS and EKS agent addresses. Keys are sent to whatever this
//!   URL names, so a free choice would let a wrong setting leak them.
//!   Unlike the SDKs, we do not resolve host names for this check:
//!   `localhost` is allowed, other names need `https://`.
//! - The token file is read again on every fetch. EKS rotates it, so a
//!   token read once at boot stops working within a day.
//!
//! The agent answers on the host, so a short timeout is enough. Errors
//! show the URL without its user info and query, in case a full URI
//! carries a secret there.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

use super::creds::{
    CredError, EKS_AGENT_V6, KeysFuture, Source, client_for, host_ip, keys_from_json, send,
};

const STEP_TIMEOUT: Duration = Duration::from_secs(5);

/// The ECS agent, where the relative URI points.
const ECS_IP: Ipv4Addr = Ipv4Addr::new(169, 254, 170, 2);
/// The EKS Pod Identity Agent.
const EKS_AGENT_V4: Ipv4Addr = Ipv4Addr::new(169, 254, 170, 23);

enum Token {
    File(PathBuf),
    Value(String),
}

struct Container {
    url: String,
    /// `url` as errors show it.
    shown: String,
    token: Option<Token>,
}

pub(super) fn from_env() -> Result<Option<Box<dyn Source>>, String> {
    let Some(url) = endpoint_url(
        super::aws_var(std::env::var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")),
        super::aws_var(std::env::var("AWS_CONTAINER_CREDENTIALS_FULL_URI")),
    )?
    else {
        return Ok(None);
    };
    let token = match (
        super::aws_var(std::env::var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE")),
        super::aws_var(std::env::var("AWS_CONTAINER_AUTHORIZATION_TOKEN")),
    ) {
        (Some(path), _) => Some(Token::File(path.into())),
        (None, Some(value)) => Some(Token::Value(value)),
        (None, None) => None,
    };
    let shown = super::shown_url(&url);
    Ok(Some(Box::new(Container { url, shown, token })))
}

/// The endpoint from the two settings. The SDKs use the relative URI
/// when both are set.
fn endpoint_url(relative: Option<String>, full: Option<String>) -> Result<Option<String>, String> {
    match (relative, full) {
        (Some(path), _) => {
            if !path.starts_with('/') {
                return Err(format!(
                    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI={path:?} must start with /"
                ));
            }
            Ok(Some(format!("http://{ECS_IP}{path}")))
        }
        (None, Some(url)) => check_full_uri(&url).map(Some),
        (None, None) => Ok(None),
    }
}

fn check_full_uri(raw: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(raw)
        .map_err(|e| format!("AWS_CONTAINER_CREDENTIALS_FULL_URI is not a URL ({e})"))?;
    let bad = |why: &str| {
        format!(
            "AWS_CONTAINER_CREDENTIALS_FULL_URI={:?} {why}",
            super::shown_url(raw)
        )
    };
    match url.scheme() {
        "https" => return Ok(raw.to_string()),
        "http" => {}
        _ => return Err(bad("must be http:// or https://")),
    }
    let host = url.host_str().unwrap_or("");
    let allowed = host == "localhost" || host_ip(host).is_some_and(allowed_ip);
    if !allowed {
        return Err(bad(
            "must use https://, or http:// to a loopback address or the ECS or EKS agent \
             (169.254.170.2, 169.254.170.23, fd00:ec2::23)",
        ));
    }
    Ok(raw.to_string())
}

fn allowed_ip(ip: IpAddr) -> bool {
    ip.is_loopback()
        || ip == IpAddr::V4(ECS_IP)
        || ip == IpAddr::V4(EKS_AGENT_V4)
        || ip == IpAddr::V6(EKS_AGENT_V6)
}

impl Container {
    fn authorization(&self) -> Result<Option<String>, String> {
        let value = match &self.token {
            None => return Ok(None),
            Some(Token::Value(v)) => v.clone(),
            Some(Token::File(path)) => std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?
                .trim()
                .to_string(),
        };
        if value.is_empty() || value.chars().any(|c| c.is_control()) {
            return Err("the container authorization token is empty or not one line".into());
        }
        Ok(Some(value))
    }
}

impl Source for Container {
    fn name(&self) -> &'static str {
        "the container credential endpoint"
    }

    fn fetch(&self) -> KeysFuture<'_> {
        Box::pin(async move {
            let mut req = client_for(&self.url).get(&self.url);
            if let Some(auth) = self.authorization().map_err(CredError::settings)? {
                req = req.header("authorization", auth);
            }
            let body = send(req, STEP_TIMEOUT)
                .await
                .map_err(|e| e.context(&self.shown))?;
            keys_from_json(&body).map_err(|e| CredError::settings(e).context(&self.shown))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_uri_over_http_is_limited_to_agents_and_loopback() {
        for ok in [
            "http://169.254.170.23/v1/credentials",
            "http://169.254.170.2/v2/credentials/x",
            "http://[fd00:ec2::23]/v1/credentials",
            "http://127.0.0.1:8080/creds",
            "http://[::1]/creds",
            "http://localhost/creds",
            "https://creds.example.com/x",
        ] {
            assert_eq!(check_full_uri(ok).as_deref(), Ok(ok), "{ok}");
        }
        for bad in [
            "http://creds.example.com/x",
            "http://10.0.0.1/creds",
            "http://169.254.169.254/latest",
            "ftp://127.0.0.1/creds",
            "not a url",
        ] {
            assert!(check_full_uri(bad).is_err(), "{bad}");
        }
        // The message shows no user info or query.
        let e = check_full_uri("http://user:pw-secret@10.0.0.1/x?sig=q-secret").unwrap_err();
        assert!(e.contains("http://10.0.0.1/x"), "{e}");
        assert!(!e.contains("secret"), "{e}");
    }

    #[test]
    fn the_relative_uri_is_a_path_on_the_ecs_agent_and_wins() {
        let url = |relative: Option<&str>, full: Option<&str>| {
            endpoint_url(relative.map(Into::into), full.map(Into::into))
        };
        assert_eq!(
            url(Some("/v2/credentials/id"), None),
            Ok(Some("http://169.254.170.2/v2/credentials/id".into()))
        );
        assert_eq!(
            url(
                Some("/v2/credentials/id"),
                Some("https://creds.example.com/x")
            ),
            Ok(Some("http://169.254.170.2/v2/credentials/id".into()))
        );
        assert_eq!(
            url(None, Some("https://creds.example.com/x")),
            Ok(Some("https://creds.example.com/x".into()))
        );
        assert_eq!(url(None, None), Ok(None));
        assert!(url(Some("v2/credentials/id"), None).is_err());
    }

    #[test]
    fn the_token_file_is_read_on_every_call() {
        let path = std::env::temp_dir().join(format!("oximg-token-{}", std::process::id()));
        std::fs::write(&path, "first\n").unwrap();
        let c = Container {
            url: String::new(),
            shown: String::new(),
            token: Some(Token::File(path.clone())),
        };
        assert_eq!(c.authorization().unwrap().as_deref(), Some("first"));
        std::fs::write(&path, "second").unwrap();
        assert_eq!(c.authorization().unwrap().as_deref(), Some("second"));
        std::fs::write(&path, "two\nlines").unwrap();
        assert!(c.authorization().is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(c.authorization().is_err());
    }
}
