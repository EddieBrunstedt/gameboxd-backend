use std::{
    env,
    net::{IpAddr, SocketAddr},
};

use anyhow::{Context, bail};
use cookie::SameSite;
use url::Url;

#[derive(Clone)]
pub struct Config {
    /// Exact peers allowed to supply a single overwritten X-Forwarded-For.
    pub trusted_proxy_ips: Vec<IpAddr>,
    pub bind_addr: SocketAddr,
    pub database_url: String,
    pub frontend_origin: Url,
    pub rp_id: String,
    /// `SameSite` on the session cookie.
    ///
    /// Configurable because the default silently constrains where the API may
    /// be deployed. The cookie is set by the API and has to be attached to
    /// `fetch` calls the SPA makes from `FRONTEND_ORIGIN`; under `Lax` the
    /// browser only does that when the two are *same-site*, which is a
    /// comparison of registrable domains, not origins. Ports and schemes do not
    /// enter into it, which is why local development never reveals the problem:
    /// `localhost:5173` and `localhost:3000` are same-site, so `Lax` works, and
    /// then an API deployed on an unrelated domain drops every session cookie
    /// in production with nothing in the logs to say why.
    pub cookie_same_site: SameSite,
    pub production: bool,
    pub igdb_credentials: Option<(String, String)>,
    pub smtp: Option<SmtpConfig>,
}

#[derive(Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let production = match env::var("APP_ENV").as_deref().unwrap_or("development") {
            "production" => true,
            "development" => false,
            _ => bail!("APP_ENV must be development or production"),
        };
        let credentials = match (optional("IGDB_CLIENT_ID"), optional("IGDB_CLIENT_SECRET")) {
            (None, None) => None,
            (Some(id), Some(secret)) => Some((id, secret)),
            _ => bail!("Set both IGDB_CLIENT_ID and IGDB_CLIENT_SECRET, or neither"),
        };
        let smtp = match env::var("EMAIL_TRANSPORT").as_deref().unwrap_or("terminal") {
            "terminal" if !production => None,
            "smtp" => Some(SmtpConfig {
                host: required("SMTP_HOST")?,
                port: env::var("SMTP_PORT").unwrap_or("587".into()).parse()?,
                username: required("SMTP_USERNAME")?,
                password: required("SMTP_PASSWORD")?,
                from: required("SMTP_FROM")?,
            }),
            _ => {
                bail!("EMAIL_TRANSPORT must be smtp in production, or terminal/smtp in development")
            }
        };
        let config = Self {
            trusted_proxy_ips: optional("TRUSTED_PROXY_IPS")
                .map(|s| {
                    s.split(',')
                        .map(|v| v.trim().parse::<IpAddr>())
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()
                .context("TRUSTED_PROXY_IPS must be comma-separated IP addresses (no CIDRs)")?
                .unwrap_or_default(),
            bind_addr: env::var("BIND_ADDR")
                .unwrap_or("127.0.0.1:3000".into())
                .parse()?,
            database_url: required("DATABASE_URL")?,
            frontend_origin: env::var("FRONTEND_ORIGIN")
                .unwrap_or("http://localhost:5173".into())
                .parse()?,
            rp_id: env::var("RP_ID").unwrap_or("localhost".into()),
            cookie_same_site: match env::var("COOKIE_SAME_SITE").as_deref().unwrap_or("lax") {
                "lax" => SameSite::Lax,
                "none" => SameSite::None,
                other => bail!(
                    "COOKIE_SAME_SITE must be lax or none, got {other:?}; \
                     use none only when the API is not same-site with FRONTEND_ORIGIN"
                ),
            },
            production,
            igdb_credentials: credentials,
            smtp,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let origin = &self.frontend_origin;
        let host = origin.host_str().context("FRONTEND_ORIGIN needs a host")?;
        if !matches!(origin.scheme(), "http" | "https")
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
            || !origin.username().is_empty()
            || origin.password().is_some()
        {
            bail!(
                "FRONTEND_ORIGIN must be an HTTP(S) origin without path, credentials, query or fragment"
            );
        }
        if origin.scheme() != "https" && (self.production || host != "localhost") {
            bail!("HTTPS is required except for localhost development");
        }
        if self.rp_id.is_empty()
            || !(host == self.rp_id || host.ends_with(&format!(".{}", self.rp_id)))
        {
            bail!("RP_ID must equal the frontend hostname or a parent domain");
        }
        // Browsers reject `SameSite=None` outright unless the cookie is also
        // `Secure`, and `Secure` is derived from this scheme in `auth::build`.
        // Without this check the pair would be accepted here and the cookie
        // discarded by the browser, which looks exactly like a broken login.
        if self.cookie_same_site == SameSite::None && origin.scheme() != "https" {
            bail!(
                "COOKIE_SAME_SITE=none requires an https FRONTEND_ORIGIN, because a browser ignores a SameSite=None cookie that is not Secure"
            );
        }
        if self.production && self.smtp.is_none() {
            bail!("Production requires SMTP configuration");
        }
        Ok(())
    }
}

fn optional(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn required(name: &str) -> anyhow::Result<String> {
    optional(name).with_context(|| format!("Missing {name}"))
}
