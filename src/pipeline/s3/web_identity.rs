//! Web identity: EKS IAM roles for service accounts (IRSA). EKS mounts
//! an OIDC token and sets `AWS_WEB_IDENTITY_TOKEN_FILE` and
//! `AWS_ROLE_ARN`. STS `AssumeRoleWithWebIdentity` trades the token for
//! keys. The call is not signed: the token is the proof.
//!
//! - The token goes in a POST form body, as the SDKs send it, so it does
//!   not end up in a URL that a proxy might log.
//! - The token file is read again on every fetch. EKS rotates it.
//! - STS is the regional endpoint for `AWS_REGION`, as the newer SDKs
//!   use: `sts.<region>.amazonaws.com`, `.amazonaws.com.cn` in China.
//!   `AWS_ENDPOINT_URL_STS` moves it, for tests and private endpoints.
//!   An `http://` value sends the token in clear, so boot warns.
//! - STS is a public endpoint, so the call honors `HTTPS_PROXY`, as any
//!   host that is not loopback or link-local does.
//!
//! The answer is XML. Like the S3 error bodies, the elements we need
//! never nest, so a plain search reads them.

use std::path::PathBuf;
use std::time::Duration;

use super::Credentials;
use super::creds::{
    CredError, Fault, Keys, KeysFuture, Source, client_for, parse_expiration, send,
};

const STEP_TIMEOUT: Duration = Duration::from_secs(5);

struct WebIdentity {
    token_file: PathBuf,
    role_arn: String,
    session_name: String,
    endpoint: String,
}

pub(super) fn from_env() -> Result<Option<Box<dyn Source>>, String> {
    let token_file = super::aws_var(std::env::var("AWS_WEB_IDENTITY_TOKEN_FILE"));
    let role_arn = super::aws_var(std::env::var("AWS_ROLE_ARN"));
    let (token_file, role_arn) = match (token_file, role_arn) {
        (Some(f), Some(r)) => (f, r),
        (None, None) => return Ok(None),
        _ => {
            return Err(
                "s3:// web identity needs both AWS_WEB_IDENTITY_TOKEN_FILE and AWS_ROLE_ARN".into(),
            );
        }
    };
    if !role_arn.starts_with("arn:") {
        return Err(format!("AWS_ROLE_ARN={role_arn:?} is not an ARN"));
    }
    let session_name = super::aws_var(std::env::var("AWS_ROLE_SESSION_NAME"))
        .unwrap_or_else(|| format!("oximg-{}", super::unix_now()));
    if !(2..=64).contains(&session_name.len())
        || !session_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_=,.@-+".contains(&b))
    {
        return Err(format!(
            "AWS_ROLE_SESSION_NAME={session_name:?} must be 2-64 of A-Z a-z 0-9 _=,.@-+"
        ));
    }
    let endpoint = match super::aws_var(std::env::var("AWS_ENDPOINT_URL_STS")) {
        Some(raw) => {
            let (scheme, host) = super::parse_endpoint("AWS_ENDPOINT_URL_STS", &raw)?;
            format!("{scheme}://{host}")
        }
        None => {
            // The region is checked in `load_settings`. Reading it here
            // keeps this source independent of that order.
            let region = super::aws_var(std::env::var("AWS_REGION")).unwrap_or_default();
            format!("https://sts.{region}.{}", super::aws_domain(&region))
        }
    };
    Ok(Some(Box::new(WebIdentity {
        token_file: token_file.into(),
        role_arn,
        session_name,
        endpoint,
    })))
}

/// STS answers 400 for two faults that are the service, not our
/// settings: `Throttling`, and `IDPCommunicationError` when it cannot
/// reach the OIDC provider. The AWS SDKs retry both. They are the
/// source being down (502), not a deployment fault (500).
fn sts_fault(mut e: CredError) -> CredError {
    if matches!(
        e.code.as_deref(),
        Some("Throttling" | "IDPCommunicationError")
    ) {
        e.fault = Fault::Unavailable;
    }
    e
}

/// The trimmed text of the first `<name>` element.
fn element<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    super::xml_element(xml, name).map(str::trim)
}

fn keys_from_xml(body: &[u8]) -> Result<Keys, String> {
    let xml = String::from_utf8_lossy(body);
    let creds = element(&xml, "Credentials").ok_or("answer has no Credentials")?;
    let field = |name: &str| {
        element(creds, name)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("answer has no {name}"))
    };
    Ok(Keys {
        credentials: Credentials {
            access_key_id: field("AccessKeyId")?,
            secret_access_key: field("SecretAccessKey")?,
            session_token: Some(field("SessionToken")?),
        },
        expires_at: Some(parse_expiration(&field("Expiration")?)?),
    })
}

impl Source for WebIdentity {
    fn name(&self) -> &'static str {
        "web identity (STS AssumeRoleWithWebIdentity)"
    }

    fn warning(&self) -> Option<String> {
        self.endpoint.starts_with("http://").then(|| {
            "AWS_ENDPOINT_URL_STS uses http://, so the web identity token and the keys \
             STS returns are not encrypted. Use https:// outside a trusted network."
                .to_string()
        })
    }

    fn fetch(&self) -> KeysFuture<'_> {
        Box::pin(async move {
            let token = std::fs::read_to_string(&self.token_file).map_err(|e| {
                CredError::settings(format!("cannot read {}: {e}", self.token_file.display()))
            })?;
            let token = token.trim();
            if token.is_empty() {
                return Err(CredError::settings(format!(
                    "{} is empty",
                    self.token_file.display()
                )));
            }
            let url = format!("{}/", self.endpoint);
            let req = client_for(&url)
                .post(&url)
                .header("accept", "application/xml")
                .form(&[
                    ("Action", "AssumeRoleWithWebIdentity"),
                    ("Version", "2011-06-15"),
                    ("RoleArn", &self.role_arn),
                    ("RoleSessionName", &self.session_name),
                    ("WebIdentityToken", token),
                ]);
            let body = send(req, STEP_TIMEOUT)
                .await
                .map_err(|e| sts_fault(e).context(&self.endpoint))?;
            keys_from_xml(&body).map_err(|e| CredError::settings(e).context(&self.endpoint))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An STS error answer goes through the same read as the keys: a
    /// throttled call is the source being down, a refused token is ours.
    #[tokio::test]
    async fn sts_throttling_is_the_source_being_down() {
        let answer = |code: &str| {
            reqwest::Response::from(
                axum::http::Response::builder()
                    .status(400)
                    .body(format!(
                        "<ErrorResponse><Error><Type>Sender</Type><Code>{code}</Code>\
                         <Message>x</Message></Error></ErrorResponse>"
                    ))
                    .unwrap(),
            )
        };
        for (code, fault) in [
            ("Throttling", Fault::Unavailable),
            ("AccessDenied", Fault::Settings),
        ] {
            let e = super::super::creds::read_body(answer(code))
                .await
                .unwrap_err();
            assert_eq!(sts_fault(e).fault, fault, "{code}");
        }
    }

    #[test]
    fn keys_come_from_the_credentials_element() {
        // The sample response from the STS API reference.
        let body = br#"<AssumeRoleWithWebIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <AssumeRoleWithWebIdentityResult>
    <AssumedRoleUser>
      <Arn>arn:aws:sts::123456789012:assumed-role/FederatedWebIdentityRole/app1</Arn>
    </AssumedRoleUser>
    <Credentials>
      <SessionToken>AQoDYXdzEE0a8ANXXXXXXXXNO1ewxE5TijQyp+IEXAMPLE</SessionToken>
      <SecretAccessKey>wJalrXUtnFEMI/K7MDENG/bPxRfiCYzEXAMPLEKEY</SecretAccessKey>
      <Expiration>2014-10-24T23:00:23Z</Expiration>
      <AccessKeyId>ASgeIAIOSFODNN7EXAMPLE</AccessKeyId>
    </Credentials>
  </AssumeRoleWithWebIdentityResult>
</AssumeRoleWithWebIdentityResponse>"#;
        let keys = keys_from_xml(body).unwrap();
        assert_eq!(keys.credentials.access_key_id, "ASgeIAIOSFODNN7EXAMPLE");
        assert_eq!(
            keys.credentials.session_token.as_deref(),
            Some("AQoDYXdzEE0a8ANXXXXXXXXNO1ewxE5TijQyp+IEXAMPLE")
        );
        assert!(keys_from_xml(b"<ErrorResponse/>").is_err());
    }
}
