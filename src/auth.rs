use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use doorman::{
    core::{
        audit::{AuditEvent, AuditSink},
        email::{Email, EmailSender},
        error::{EmailError, EmailResult},
        memory::InMemoryRateLimiter,
    },
    prelude::*,
};
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, MultiPart},
    transport::smtp::authentication::Credentials,
};
use sqlx::PgPool;

use crate::config::Config;

/// How often expired sessions, email links and passkey challenges are purged.
const PURGE_EVERY: Duration = Duration::from_secs(300);

/// Where Doorman's emails go: SMTP when configured, otherwise the terminal.
pub fn email_sender(config: &Config) -> anyhow::Result<Arc<dyn EmailSender>> {
    Ok(match &config.smtp {
        None => Arc::new(TerminalEmailSender),
        Some(smtp) => Arc::new(SmtpSender {
            from: smtp.from.parse()?,
            transport: AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.host)?
                .port(smtp.port)
                .credentials(Credentials::new(
                    smtp.username.clone(),
                    smtp.password.clone(),
                ))
                .timeout(Some(Duration::from_secs(15)))
                .build(),
        }),
    })
}

/// Assembles Doorman. Must run inside a Tokio runtime: the service purges
/// expired authentication rows on a task of its own.
pub fn build(
    config: &Config,
    storage: &PostgresStorage,
    pool: PgPool,
    email: Arc<dyn EmailSender>,
) -> anyhow::Result<AuthService> {
    let base = config.frontend_origin.as_str().trim_end_matches('/');
    let link = |path: &str| LinkTemplate::parse(format!("{base}{path}?token={TOKEN_PLACEHOLDER}"));
    let relying_party = RelyingParty::builder(config.frontend_origin.clone(), "Gameboxd")
        .id(&config.rp_id)
        .build()?;
    // No `dpop_origin` in the session config: every session is a browser's
    // cookie session, and Doorman refuses to start a DPoP session.
    let core = AuthCore::builder(AuthCoreRequired {
        storage: storage.all_stores(),
        email_sender: email,
        audit_sink: Arc::new(DatabaseAudit(pool)),
        passkey: PasskeyConfig::new(relying_party),
        reauthentication: ReauthPolicy::required_for([
            ReauthAction::DeleteUser,
            ReauthAction::RemovePasskey,
            ReauthAction::RequestEmailChange,
        ]),
        rate_limiting: RateLimiting::enforced(Arc::new(InMemoryRateLimiter::new(Arc::new(
            SystemClock,
        )))),
        links: LinkTemplates {
            signup: link("/signup")?,
            recovery: link("/recovery")?,
            email_change: link("/account/email")?,
        },
    })
    .build();
    let mut cookie = CookieConfig::builder(if config.production {
        "__Host-gameboxd_session"
    } else {
        "gameboxd_session"
    })
    // See `Config::cookie_same_site`: `Lax` is right whenever the API and the
    // SPA are same-site, and wrong — silently — when they are not, so it is a
    // deployment decision rather than a default to inherit.
    .same_site(config.cookie_same_site);
    if config.frontend_origin.scheme() != "https" {
        cookie = cookie.insecure();
    }
    // The relying party's origin, `FRONTEND_ORIGIN`, is the one origin Doorman
    // trusts for state-changing browser requests.
    Ok(
        AuthService::builder(core, PurgeSchedule::in_process(PURGE_EVERY)?)
            .cookie(cookie.build()?)
            .build()?,
    )
}

struct DatabaseAudit(PgPool);

#[async_trait]
impl AuditSink for DatabaseAudit {
    async fn emit(&self, event: AuditEvent) {
        // Doorman's sink contract is best-effort and cannot reject the auth action.
        if let Err(error) =
            sqlx::query("INSERT INTO auth_audit_events (occurred_at,event) VALUES ($1,$2)")
                .bind(event.timestamp)
                .bind(sqlx::types::Json(event))
                .execute(&self.0)
                .await
        {
            tracing::error!(%error, "Failed to persist auth audit event");
        }
    }
}

struct SmtpSender {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

#[async_trait]
impl EmailSender for SmtpSender {
    async fn send(&self, email: Email) -> EmailResult<()> {
        let builder = Message::builder()
            .from(self.from.clone())
            .to(email.to.parse().map_err(EmailError::new)?)
            .subject(email.subject);
        let message = match email.body_html {
            Some(html) => {
                builder.multipart(MultiPart::alternative_plain_html(email.body_text, html))
            }
            None => builder.body(email.body_text),
        }
        .map_err(EmailError::new)?;
        self.transport
            .send(message)
            .await
            .map_err(EmailError::new)?;
        Ok(())
    }
}
