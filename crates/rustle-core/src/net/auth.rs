//! How a session signs in. A `Credential` is handed to a session's `sign_in`,
//! which dispatches on the mechanism; anything else is an error rather than
//! an unauthenticated session.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mechanism {
    /// A keyring password, sent with LOGIN / AUTH PLAIN.
    Login,
    /// A GNOME Online Accounts access token, sent with SASL XOAUTH2.
    Xoauth2,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    pub user: String,
    pub secret: String,
    pub mechanism: Mechanism,
}

impl Credential {
    pub fn password(user: impl Into<String>, password: impl Into<String>) -> Self {
        Credential {
            user: user.into(),
            secret: password.into(),
            mechanism: Mechanism::Login,
        }
    }

    pub fn token(user: impl Into<String>, token: impl Into<String>) -> Self {
        Credential {
            user: user.into(),
            secret: token.into(),
            mechanism: Mechanism::Xoauth2,
        }
    }

    /// The SASL XOAUTH2 initial response (RFC 7628), before base64.
    pub fn xoauth2_response(&self) -> String {
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.secret)
    }
}

// A credential travels through worker threads; the derived Debug would put a
// password or access token in any log line that prints one.
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("user", &self.user)
            .field("secret", &"...")
            .field("mechanism", &self.mechanism)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_the_secret() {
        let credential = Credential::password("ada", "hunter2");
        assert!(!format!("{credential:?}").contains("hunter2"));
        assert_eq!(
            Credential::token("ada", "t").xoauth2_response(),
            "user=ada\x01auth=Bearer t\x01\x01"
        );
    }
}
