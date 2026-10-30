//! EC2 instance-profile keys from the Instance Metadata Service, v2
//! only:
//!
//! 1. `PUT /latest/api/token` with `X-aws-ec2-metadata-token-ttl-seconds`
//!    starts a session.
//! 2. `GET /latest/meta-data/iam/security-credentials/` names the role.
//! 3. `GET /latest/meta-data/iam/security-credentials/<role>` returns the
//!    keys, in the JSON shape that `keys_from_json` reads.
//!
//! Every fetch starts a new session. Keys last hours and are fetched
//! again only near their expiry, so a cached session would save little.
//!
//! IMDS is the last source in the chain and is tried whenever nothing
//! before it is set. Off EC2 the address does not answer, so each step
//! has a short timeout: a host without credentials fails at boot soon
//! and says what it tried.
//! `AWS_EC2_METADATA_DISABLED=true` turns IMDS off, which the SDKs also
//! offer for networks where the address could be someone else.
//!
//! Inside a container on EC2 the token answer needs a hop limit of 2.
//! The EC2 default is 1, and then step 1 times out. The error names the
//! hop limit only for a timeout: another cloud's metadata service, which
//! answers the same address, gets a plain error.

use std::time::Duration;

use super::creds::{
    CredError, Fault, KeysFuture, Source, client_for, host_is_local, keys_from_json, send,
};

const DEFAULT_ENDPOINT: &str = "http://169.254.169.254";
const STEP_TIMEOUT: Duration = Duration::from_secs(2);
/// Six hours, the IMDS maximum. The session is used for two requests.
const TOKEN_TTL: &str = "21600";
const CREDENTIALS_PATH: &str = "/latest/meta-data/iam/security-credentials/";
const NO_ROLE: &str = "the instance has no IAM role. Attach an instance profile, or set \
                       another source";

struct Imds {
    endpoint: String,
}

pub(super) fn from_env() -> Result<Option<Box<dyn Source>>, String> {
    if let Some(v) = super::aws_var(std::env::var("AWS_EC2_METADATA_DISABLED")) {
        if v.eq_ignore_ascii_case("true") {
            return Ok(None);
        }
        if !v.eq_ignore_ascii_case("false") {
            return Err(format!(
                "AWS_EC2_METADATA_DISABLED={v:?} must be true or false"
            ));
        }
    }
    let endpoint = match super::aws_var(std::env::var("AWS_EC2_METADATA_SERVICE_ENDPOINT")) {
        None => DEFAULT_ENDPOINT.to_string(),
        Some(raw) => {
            let (scheme, host) = super::parse_endpoint("AWS_EC2_METADATA_SERVICE_ENDPOINT", &raw)?;
            format!("{scheme}://{host}")
        }
    };
    Ok(Some(Box::new(Imds { endpoint })))
}

impl Imds {
    async fn step(&self, req: reqwest::RequestBuilder, what: &str) -> Result<Vec<u8>, CredError> {
        send(req, STEP_TIMEOUT)
            .await
            .map_err(|e| e.context(format_args!("{what} at {}", self.endpoint)))
    }
}

/// What to add when the first step fails. Only IMDS was left to try,
/// so the operator likely meant another source.
fn token_hint(fault: Fault) -> &'static str {
    match fault {
        Fault::TimedOut => {
            ". No other credentials are set. Off EC2, set static keys or a container \
             endpoint; inside a container on EC2, the instance needs a metadata hop \
             limit of 2"
        }
        _ => ". No other credentials are set. Off EC2, set static keys or a container endpoint",
    }
}

impl Source for Imds {
    fn name(&self) -> &'static str {
        "the EC2 instance metadata service (IMDSv2)"
    }

    fn warning(&self) -> Option<String> {
        (self.endpoint.starts_with("http://") && !host_is_local(&self.endpoint)).then(|| {
            "AWS_EC2_METADATA_SERVICE_ENDPOINT uses http:// to a host that is not \
             link-local or loopback, so the session token and the keys are not encrypted. \
             Use https:// outside a trusted network."
                .to_string()
        })
    }

    fn fetch(&self) -> KeysFuture<'_> {
        Box::pin(async move {
            let client = client_for(&self.endpoint);
            let token = self
                .step(
                    client
                        .put(format!("{}/latest/api/token", self.endpoint))
                        .header("x-aws-ec2-metadata-token-ttl-seconds", TOKEN_TTL),
                    "session token",
                )
                .await
                .map_err(|e| CredError {
                    message: format!("{}{}", e.message, token_hint(e.fault)),
                    ..e
                })?;
            let token = String::from_utf8_lossy(&token).trim().to_string();
            let get = |path: String| {
                client
                    .get(format!("{}{path}", self.endpoint))
                    .header("x-aws-ec2-metadata-token", &token)
            };
            // IMDS answers 404 here when the instance has no profile.
            let roles = self
                .step(get(CREDENTIALS_PATH.to_string()), "role name")
                .await
                .map_err(|e| match e.status {
                    Some(404) => CredError::settings(NO_ROLE),
                    _ => e,
                })?;
            let roles = String::from_utf8_lossy(&roles);
            let role = roles
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .ok_or_else(|| CredError::settings(NO_ROLE))?;
            if role.contains(['/', '?', '#']) {
                return Err(CredError::settings(format!(
                    "role name {role:?} is not a name"
                )));
            }
            let body = self
                .step(get(format!("{CREDENTIALS_PATH}{role}")), "keys")
                .await?;
            keys_from_json(&body)
                .map_err(|e| CredError::settings(format!("keys for role {role:?}: {e}")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_to_a_remote_host_warns() {
        let warns = |endpoint: &str| {
            Imds {
                endpoint: endpoint.into(),
            }
            .warning()
            .is_some()
        };
        assert!(warns("http://10.0.0.1"));
        assert!(!warns(DEFAULT_ENDPOINT));
        assert!(!warns("http://[fd00:ec2::254]"));
        assert!(!warns("http://127.0.0.1:1338"));
        assert!(!warns("https://imds.example.com"));
    }
}
