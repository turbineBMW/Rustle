//! One SMTP session, over lettre. lettre connects lazily on the first send,
//! so "connect" and "sign in" here only configure the transport; the socket,
//! TLS and AUTH all happen inside `send_raw`.

use super::auth::{Credential, Mechanism};
use super::errors::NetError;
use super::{is_loopback, NET_TIMEOUT};
use crate::models::Security;
use lettre::transport::smtp::authentication::{Credentials, Mechanism as LettreMechanism};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{Address, SmtpTransport, Transport};

pub type Result<T> = std::result::Result<T, NetError>;

pub struct SmtpSession {
    host: String,
    port: u16,
    security: Security,
    credential: Option<Credential>,
    transport: Option<SmtpTransport>,
}

impl SmtpSession {
    pub fn new(host: &str, port: u16, security: Security) -> Self {
        SmtpSession {
            host: host.to_string(),
            port,
            security,
            credential: None,
            transport: None,
        }
    }

    /// Validates the TLS configuration up front so a bad host shows up here
    /// rather than mid-send.
    pub fn connect(&mut self) -> Result<()> {
        self.tls()?;
        self.transport = Some(self.build(None)?);
        Ok(())
    }

    pub fn sign_in(&mut self, credential: &Credential) -> Result<()> {
        if self.transport.is_none() {
            return Err(NetError::Protocol(format!(
                "not connected to {}:{}",
                self.host, self.port
            )));
        }
        self.credential = Some(credential.clone());
        self.transport = Some(self.build(Some(credential))?);
        Ok(())
    }

    fn tls(&self) -> Result<Tls> {
        Ok(match self.security {
            Security::None => Tls::None,
            Security::StartTls => Tls::Required(self.tls_parameters()?),
            Security::Tls => Tls::Wrapper(self.tls_parameters()?),
        })
    }

    fn tls_parameters(&self) -> Result<TlsParameters> {
        let skip_verify = is_loopback(&self.host);
        Ok(TlsParameters::builder(self.host.clone())
            .dangerous_accept_invalid_certs(skip_verify)
            .dangerous_accept_invalid_hostnames(skip_verify)
            .build_native()?)
    }

    fn build(&self, credential: Option<&Credential>) -> Result<SmtpTransport> {
        let mut builder = SmtpTransport::builder_dangerous(self.host.as_str())
            .port(self.port)
            .tls(self.tls()?)
            .timeout(Some(NET_TIMEOUT));
        if let Some(credential) = credential {
            // lettre builds the XOAUTH2 initial response itself from (user, token).
            let mechanisms = match credential.mechanism {
                Mechanism::Login => vec![LettreMechanism::Plain, LettreMechanism::Login],
                Mechanism::Xoauth2 => vec![LettreMechanism::Xoauth2],
            };
            builder = builder
                .credentials(Credentials::new(
                    credential.user.clone(),
                    credential.secret.clone(),
                ))
                .authentication(mechanisms);
        }
        Ok(builder.build())
    }

    /// Hand a fully-built message to the server. Bcc recipients are on the
    /// envelope and nowhere in `raw`.
    pub fn send_raw(&mut self, from_addr: &str, recipients: &[String], raw: &[u8]) -> Result<()> {
        // Never send unauthenticated: a session that skipped sign_in would
        // otherwise be reported as sent by the caller.
        if self.credential.is_none() {
            return Err(NetError::Protocol(format!(
                "not signed in to {}:{}",
                self.host, self.port
            )));
        }
        let transport = self.transport.as_ref().ok_or_else(|| {
            NetError::Protocol(format!("not connected to {}:{}", self.host, self.port))
        })?;
        let from: Address = from_addr
            .parse()
            .map_err(|_| NetError::Protocol(format!("invalid sender address {from_addr:?}")))?;
        let mut to = Vec::with_capacity(recipients.len());
        for recipient in recipients {
            let address = recipient.parse::<Address>().map_err(|_| {
                NetError::Protocol(format!("invalid recipient address {recipient:?}"))
            })?;
            to.push(address);
        }
        let envelope = lettre::address::Envelope::new(Some(from), to).map_err(|error| {
            NetError::Protocol(format!("could not address the message: {error}"))
        })?;
        transport.send_raw(&envelope, raw)?;
        Ok(())
    }

    /// Same contract as `ImapSession::logout`: never fails over the real error.
    pub fn quit(&mut self) {
        self.transport = None;
        self.credential = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_to_send_before_sign_in() {
        let mut session = SmtpSession::new("localhost", 2525, Security::None);
        assert!(session.send_raw("a@b.c", &["d@e.f".into()], b"x").is_err());
        session.connect().unwrap();
        assert!(session.send_raw("a@b.c", &["d@e.f".into()], b"x").is_err());
    }
}
