//! Known server settings for the big mail providers, so the Add Account
//! dialog can fill the server fields in from the address alone.

use crate::models::Security;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderSettings {
    pub imap_host: &'static str,
    pub imap_port: u16,
    pub imap_security: Security,
    pub smtp_host: &'static str,
    pub smtp_port: u16,
    pub smtp_security: Security,
}

const fn provider(
    imap_host: &'static str,
    imap_port: u16,
    imap_security: Security,
    smtp_host: &'static str,
    smtp_port: u16,
    smtp_security: Security,
) -> ProviderSettings {
    ProviderSettings {
        imap_host,
        imap_port,
        imap_security,
        smtp_host,
        smtp_port,
        smtp_security,
    }
}

const GMAIL: ProviderSettings = provider(
    "imap.gmail.com",
    993,
    Security::Tls,
    "smtp.gmail.com",
    587,
    Security::StartTls,
);
const OUTLOOK: ProviderSettings = provider(
    "outlook.office365.com",
    993,
    Security::Tls,
    "smtp-mail.outlook.com",
    587,
    Security::StartTls,
);
const YAHOO: ProviderSettings = provider(
    "imap.mail.yahoo.com",
    993,
    Security::Tls,
    "smtp.mail.yahoo.com",
    465,
    Security::Tls,
);
const ICLOUD: ProviderSettings = provider(
    "imap.mail.me.com",
    993,
    Security::Tls,
    "smtp.mail.me.com",
    587,
    Security::StartTls,
);

const PROVIDERS: &[(&str, ProviderSettings)] = &[
    ("gmail.com", GMAIL),
    ("googlemail.com", GMAIL),
    ("outlook.com", OUTLOOK),
    ("hotmail.com", OUTLOOK),
    ("live.com", OUTLOOK),
    ("msn.com", OUTLOOK),
    ("yahoo.com", YAHOO),
    ("yahoo.co.uk", YAHOO),
    ("yahoo.in", YAHOO),
    ("ymail.com", YAHOO),
    (
        "aol.com",
        provider(
            "imap.aol.com",
            993,
            Security::Tls,
            "smtp.aol.com",
            465,
            Security::Tls,
        ),
    ),
    ("icloud.com", ICLOUD),
    ("me.com", ICLOUD),
    ("mac.com", ICLOUD),
    (
        "fastmail.com",
        provider(
            "imap.fastmail.com",
            993,
            Security::Tls,
            "smtp.fastmail.com",
            465,
            Security::Tls,
        ),
    ),
    (
        "zoho.com",
        provider(
            "imap.zoho.com",
            993,
            Security::Tls,
            "smtp.zoho.com",
            465,
            Security::Tls,
        ),
    ),
    (
        "yandex.com",
        provider(
            "imap.yandex.com",
            993,
            Security::Tls,
            "smtp.yandex.com",
            465,
            Security::Tls,
        ),
    ),
];

/// The known server settings for an address' domain, or None if unknown.
pub fn settings_for_email(address: &str) -> Option<ProviderSettings> {
    let (_, domain) = address.trim().rsplit_once('@')?;
    let domain = domain.to_lowercase();
    PROVIDERS
        .iter()
        .find(|(known, _)| *known == domain)
        .map(|(_, settings)| *settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_up_by_domain() {
        assert_eq!(settings_for_email("ada@GMAIL.com"), Some(GMAIL));
        assert_eq!(settings_for_email("ada@example.org"), None);
        assert_eq!(settings_for_email("no-at-sign"), None);
    }
}
