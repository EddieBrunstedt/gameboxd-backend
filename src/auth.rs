use std::sync::Arc;

use async_trait::async_trait;
use doorman::{
    core::{
        audit::{AuditEvent, AuditSink},
        email::{Email, EmailSender},
        error::{EmailError, EmailResult},
        memory::InMemoryRateLimiter,
        verification::TOKEN_PLACEHOLDER,
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

pub fn build(
    config: &Config,
    storage: &PostgresStorage,
    pool: PgPool,
) -> anyhow::Result<AuthService> {
    let email: Arc<dyn EmailSender> = match &config.smtp {
        None => Arc::new(TerminalEmailSender),
        Some(smtp) => Arc::new(SmtpSender {
            from: smtp.from.parse()?,
            transport: AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.host)?
                .port(smtp.port)
                .credentials(Credentials::new(
                    smtp.username.clone(),
                    smtp.password.clone(),
                ))
                .timeout(Some(std::time::Duration::from_secs(15)))
                .build(),
        }),
    };
    let base = config.frontend_origin.as_str().trim_end_matches('/');
    Ok(AuthService::builder(storage.all_stores())
        .email(email)
        .passkey(PasskeyConfig::new(
            &config.rp_id,
            config.frontend_origin.clone(),
            "Gameboxd",
        ))
        .audit(Arc::new(DatabaseAudit(pool)))
        .limiter(Arc::new(InMemoryRateLimiter::new(Arc::new(SystemClock))))
        .signup(SignupConfig {
            url_template: format!("{base}/signup?token={TOKEN_PLACEHOLDER}"),
            ..Default::default()
        })
        .recovery(RecoveryConfig {
            url_template: format!("{base}/recovery?token={TOKEN_PLACEHOLDER}"),
            ..Default::default()
        })
        .account(AccountConfig {
            url_template: format!("{base}/account/email?token={TOKEN_PLACEHOLDER}"),
            ..Default::default()
        })
        .reauthentication(ReauthPolicy::required_for([
            ReauthAction::DeleteAccount,
            ReauthAction::RemovePasskey,
            ReauthAction::RequestEmailChange,
        ]))
        .external_origin(config.frontend_origin.clone())
        .cookie(CookieConfig {
            session_cookie_name: if config.production {
                "__Host-gameboxd_session"
            } else {
                "gameboxd_session"
            }
            .into(),
            secure: config.frontend_origin.scheme() == "https",
            ..Default::default()
        })
        .build()?)
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
