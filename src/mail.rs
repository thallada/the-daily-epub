//! Minimal plain-text SMTP delivery.

#[cfg(test)]
use std::sync::Arc;

use anyhow::Context as _;
use lettre::message::{Mailbox, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport as _, Tokio1Executor};

use crate::config::MailConfig;

/// A plain-text outbound message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Recipient mailbox, either an address or `Name <address>`.
    pub to: String,
    /// Message subject.
    pub subject: String,
    /// Plain-text message body.
    pub body: String,
}

#[derive(Clone)]
enum Transport {
    Smtp(AsyncSmtpTransport<Tokio1Executor>),
    #[cfg(test)]
    Recording(Arc<std::sync::Mutex<Vec<Message>>>),
    #[cfg(test)]
    Failing,
}

/// A reusable SMTP sender built from the startup configuration.
#[derive(Clone)]
pub struct Mailer {
    from: Mailbox,
    transport: Transport,
}

impl Mailer {
    /// Build an SMTP sender, or return `None` when mail is not fully active.
    pub fn from_config(config: &MailConfig) -> anyhow::Result<Option<Self>> {
        if !config.is_active() {
            return Ok(None);
        }

        let from = parse_mailbox(&config.from, "mail.from")?;
        let user = config
            .smtp_user
            .as_deref()
            .expect("active mail config has an SMTP username");
        let pass = config
            .smtp_pass
            .as_deref()
            .expect("active mail config has an SMTP password");
        let builder = if config.smtp_starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.smtp_host)
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&config.smtp_host)
        }
        .with_context(|| format!("invalid SMTP host {:?}", config.smtp_host))?;
        let transport = builder
            .port(config.smtp_port)
            .credentials(Credentials::new(user.to_string(), pass.to_string()))
            .build();

        Ok(Some(Self {
            from,
            transport: Transport::Smtp(transport),
        }))
    }

    /// Send one plain-text message.
    pub async fn send(&self, message: Message) -> anyhow::Result<()> {
        #[cfg(test)]
        if let Transport::Recording(messages) = &self.transport {
            messages
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(message);
            return Ok(());
        }
        #[cfg(test)]
        if let Transport::Failing = &self.transport {
            anyhow::bail!("test mail delivery failed");
        }

        let to = parse_mailbox(&message.to, "message recipient")?;
        let email = lettre::Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(message.subject)
            .header(ContentType::TEXT_PLAIN)
            .body(message.body)
            .context("building plain-text email")?;
        match &self.transport {
            Transport::Smtp(transport) => {
                transport
                    .send(email)
                    .await
                    .context("sending email through SMTP")?;
            }
            #[cfg(test)]
            Transport::Recording(_) => unreachable!("recording transport returned above"),
            #[cfg(test)]
            Transport::Failing => unreachable!("failing transport returned above"),
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn recording() -> (Self, Arc<std::sync::Mutex<Vec<Message>>>) {
        let messages = Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Self {
                from: "daily@example.com".parse().expect("valid test mailbox"),
                transport: Transport::Recording(Arc::clone(&messages)),
            },
            messages,
        )
    }

    #[cfg(test)]
    pub(crate) fn failing() -> Self {
        Self {
            from: "daily@example.com".parse().expect("valid test mailbox"),
            transport: Transport::Failing,
        }
    }
}

fn parse_mailbox(value: &str, field: &str) -> anyhow::Result<Mailbox> {
    value
        .parse::<Mailbox>()
        .with_context(|| format!("{field} is not a valid email mailbox"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_named_and_bare_mailboxes() {
        assert!(parse_mailbox("The Daily EPUB <daily@example.com>", "from").is_ok());
        assert!(parse_mailbox("operator@example.com", "to").is_ok());
        assert!(parse_mailbox("not an address", "to").is_err());
    }

    #[test]
    fn inactive_config_builds_no_mailer() {
        assert!(
            Mailer::from_config(&MailConfig::default())
                .unwrap()
                .is_none()
        );

        let mut config = MailConfig {
            enabled: true,
            smtp_host: "smtp.example.com".into(),
            from: "daily@example.com".into(),
            smtp_user: Some("user".into()),
            smtp_pass: Some("  ".into()),
            ..MailConfig::default()
        };
        assert!(Mailer::from_config(&config).unwrap().is_none());

        config.smtp_pass = Some("secret".into());
        config.from = "not an address".into();
        assert!(Mailer::from_config(&config).is_err());
    }
}
