use std::{env, net::SocketAddr};

use anyhow::{Context, bail};
use url::Url;

#[derive(Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub database_url: String,
    pub frontend_origin: Url,
    pub rp_id: String,
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
            bind_addr: env::var("BIND_ADDR")
                .unwrap_or("127.0.0.1:3000".into())
                .parse()?,
            database_url: required("DATABASE_URL")?,
            frontend_origin: env::var("FRONTEND_ORIGIN")
                .unwrap_or("http://localhost:5173".into())
                .parse()?,
            rp_id: env::var("RP_ID").unwrap_or("localhost".into()),
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
