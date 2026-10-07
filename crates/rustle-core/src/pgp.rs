//! OpenPGP through the system's `gpg`, whose agent holds the keys and asks
//! for passphrases. Reading: PGP/MIME (RFC 3156) and inline messages are
//! decrypted, and signatures checked. Writing: a message body is signed
//! and/or encrypted into RFC 3156 form. gpg runs without a shell, its input
//! on stdin or in private temporary files.
//!
//! A decrypted message is only ever held in memory: the database keeps the
//! encrypted copy, and nothing from inside it reaches the search index.

use mail_parser::MimeHeaders;
use std::io::Write;
use std::process::{Command, Stdio};

/// What protection a raw message carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
    None,
    /// PGP/MIME `multipart/encrypted`.
    Encrypted,
    /// An inline `-----BEGIN PGP MESSAGE-----` block in a text body.
    InlineEncrypted,
    /// PGP/MIME `multipart/signed`.
    Signed,
}

/// The outcome of checking a signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignatureStatus {
    /// Good, by `signer`; `is_trusted` when the key is (fully or
    /// ultimately) valid in the keyring, not just present.
    Good { signer: String, is_trusted: bool },
    /// The signature doesn't match: changed after signing, or forged.
    Bad { signer: String },
    /// Can't be checked: the key `key_id` isn't in the keyring.
    UnknownKey { key_id: String },
}

#[derive(Debug, thiserror::Error)]
pub enum PgpError {
    #[error("gpg isn't installed")]
    NotInstalled,
    #[error("there's no secret key here to decrypt it with")]
    NoSecretKey,
    #[error("no public key for {0}")]
    NoPublicKey(String),
    #[error("not an OpenPGP message")]
    NotPgp,
    #[error("gpg: {0}")]
    Gpg(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// A message once decrypted: the raw message to show in its place, and
/// the signature it carried inside, if any.
#[derive(Clone, Debug)]
pub struct Decrypted {
    pub raw: Vec<u8>,
    pub signature: Option<SignatureStatus>,
}

pub fn is_available() -> bool {
    Command::new("gpg")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether there's a secret key for `address`: it can sign, and read
/// what it encrypted to itself.
pub fn has_secret_key(address: &str) -> bool {
    !address.is_empty()
        && gpg_command()
            .args([
                "--list-secret-keys",
                "--with-colons",
                "--",
                &format!("<{address}>"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

fn gpg_command() -> Command {
    let mut command = Command::new("gpg");
    command.args(["--batch", "--no-tty", "--status-fd", "2", "--with-colons"]);
    command
}

pub fn detect(raw: &[u8]) -> Protection {
    let Some(message) = mail_parser::MessageParser::default().parse(raw) else {
        return Protection::None;
    };
    if let Some(content_type) = message.content_type() {
        let protocol = content_type
            .attribute("protocol")
            .unwrap_or("")
            .to_ascii_lowercase();
        let subtype = content_type.subtype().unwrap_or("").to_ascii_lowercase();
        if content_type.ctype().eq_ignore_ascii_case("multipart") {
            if subtype == "encrypted" && protocol == "application/pgp-encrypted" {
                return Protection::Encrypted;
            }
            if subtype == "signed" && protocol == "application/pgp-signature" {
                return Protection::Signed;
            }
        }
    }
    let text = message.body_text(0).unwrap_or_default();
    if text.contains("-----BEGIN PGP MESSAGE-----") {
        return Protection::InlineEncrypted;
    }
    Protection::None
}

/// Decrypt a PGP/MIME or inline-encrypted message into one that reads as
/// usual: the original's headers over what was inside.
pub fn decrypt(raw: &[u8]) -> Result<Decrypted, PgpError> {
    let protection = detect(raw);
    let (armored, is_mime) = match protection {
        Protection::Encrypted => (encrypted_payload(raw).ok_or(PgpError::NotPgp)?, true),
        Protection::InlineEncrypted => (inline_block(raw).ok_or(PgpError::NotPgp)?, false),
        _ => return Err(PgpError::NotPgp),
    };
    let (plain, status) = run_gpg(&["--decrypt"], &armored)?;
    if !status.contains("DECRYPTION_OKAY") && !status.contains("[GNUPG:] PLAINTEXT") {
        if status.contains("NO_SECKEY") {
            return Err(PgpError::NoSecretKey);
        }
        return Err(PgpError::Gpg(first_error(&status)));
    }
    let headers = outer_headers(raw);
    let mut out = headers;
    if is_mime {
        // The plaintext is a MIME entity with headers of its own.
        out.extend_from_slice(&plain);
    } else {
        out.extend_from_slice(b"Content-Type: text/plain; charset=utf-8\r\n\r\n");
        out.extend_from_slice(&plain);
    }
    Ok(Decrypted {
        raw: out,
        signature: signature_from_status(&status),
    })
}

/// Check a `multipart/signed` message's signature over its first part.
pub fn verify(raw: &[u8]) -> Result<SignatureStatus, PgpError> {
    let (signed, signature) = signed_parts(raw).ok_or(PgpError::NotPgp)?;
    let directory = private_dir()?;
    let data = directory.path().join("signed");
    let sig = directory.path().join("signature.asc");
    std::fs::write(&data, canonical(&signed))?;
    std::fs::write(&sig, signature)?;
    let output = gpg_command()
        .arg("--verify")
        .arg(&sig)
        .arg(&data)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(not_installed)?;
    let status = String::from_utf8_lossy(&output.stderr);
    signature_from_status(&status).ok_or_else(|| PgpError::Gpg(first_error(&status)))
}

/// Sign `entity` (a whole MIME part, headers and all) as `signer`: the
/// detached, armored signature.
pub fn sign(entity: &[u8], signer: &str) -> Result<Vec<u8>, PgpError> {
    let (signature, status) = run_gpg(
        &[
            "--detach-sign",
            "--armor",
            "--digest-algo",
            "SHA256",
            "--local-user",
            &format!("<{signer}>"),
        ],
        &canonical(entity),
    )?;
    if signature.is_empty() {
        return Err(PgpError::Gpg(first_error(&status)));
    }
    Ok(signature)
}

/// Encrypt `entity` to `recipients` (and `sender`, so the Sent copy stays
/// readable), signing it too when `also_sign`.
pub fn encrypt(
    entity: &[u8],
    sender: &str,
    recipients: &[String],
    also_sign: bool,
) -> Result<Vec<u8>, PgpError> {
    let mut args: Vec<String> = vec!["--encrypt".into(), "--armor".into()];
    if also_sign {
        args.extend([
            "--sign".into(),
            "--local-user".into(),
            format!("<{sender}>"),
        ]);
    }
    for recipient in recipients.iter().map(String::as_str).chain([sender]) {
        args.extend(["--recipient".into(), format!("<{recipient}>")]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (armored, status) = run_gpg(&args, entity)?;
    if armored.is_empty() {
        // INV_RECP 0 <recipient>: no usable key for them.
        if let Some(line) = status.lines().find(|line| line.contains("INV_RECP")) {
            let who = line
                .rsplit(' ')
                .next()
                .unwrap_or("")
                .trim_matches(['<', '>']);
            return Err(PgpError::NoPublicKey(who.to_string()));
        }
        return Err(PgpError::Gpg(first_error(&status)));
    }
    Ok(armored)
}

/// The gpg-backed operations a protected outgoing message needs.
pub struct Gpg {
    pub sender: String,
    /// Everyone it goes to, Bcc included: each needs a key to read it.
    pub recipients: Vec<String>,
}

impl crate::compose::Crypto for Gpg {
    fn sign(&self, entity: &[u8]) -> Result<Vec<u8>, String> {
        sign(entity, &self.sender).map_err(|error| error.to_string())
    }

    fn encrypt(&self, entity: &[u8], also_sign: bool) -> Result<Vec<u8>, String> {
        encrypt(entity, &self.sender, &self.recipients, also_sign)
            .map_err(|error| error.to_string())
    }
}

/// Run gpg over `input`: what it wrote, and its status lines.
fn run_gpg(args: &[&str], input: &[u8]) -> Result<(Vec<u8>, String), PgpError> {
    let mut child = gpg_command()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(not_installed)?;
    // Fed from a thread: gpg may fill its output pipe before it has read
    // all of a large input.
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let input = input.to_vec();
    let feeder = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output()?;
    let _ = feeder.join();
    Ok((
        output.stdout,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

fn not_installed(error: std::io::Error) -> PgpError {
    if error.kind() == std::io::ErrorKind::NotFound {
        PgpError::NotInstalled
    } else {
        PgpError::Io(error)
    }
}

fn private_dir() -> Result<tempfile::TempDir, PgpError> {
    Ok(tempfile::Builder::new().prefix("rustle-pgp-").tempdir()?)
}

/// What gpg's status lines say about a signature.
fn signature_from_status(status: &str) -> Option<SignatureStatus> {
    let field = |line: &str, skip: usize| -> String {
        line.split(' ').skip(skip).collect::<Vec<_>>().join(" ")
    };
    let mut result = None;
    let mut is_trusted = false;
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("[GNUPG:] ") else {
            continue;
        };
        if rest.starts_with("GOODSIG ") {
            result = Some(SignatureStatus::Good {
                signer: field(rest, 2),
                is_trusted: false,
            });
        } else if rest.starts_with("BADSIG ") {
            return Some(SignatureStatus::Bad {
                signer: field(rest, 2),
            });
        } else if rest.starts_with("ERRSIG ") || rest.starts_with("NO_PUBKEY ") {
            if result.is_none() {
                result = Some(SignatureStatus::UnknownKey {
                    key_id: rest.split(' ').nth(1).unwrap_or("").to_string(),
                });
            }
        } else if rest.starts_with("TRUST_FULLY") || rest.starts_with("TRUST_ULTIMATE") {
            is_trusted = true;
        }
    }
    match result {
        Some(SignatureStatus::Good { signer, .. }) => {
            Some(SignatureStatus::Good { signer, is_trusted })
        }
        other => other,
    }
}

fn first_error(status: &str) -> String {
    status
        .lines()
        .find(|line| line.starts_with("gpg: ") && !line.contains("WARNING"))
        .map(|line| line.trim_start_matches("gpg: ").to_string())
        .or_else(|| {
            status
                .lines()
                .find(|line| line.contains("FAILURE"))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "gpg failed".to_string())
}

/// RFC 3156: what's signed is the part in canonical form, CRLF line ends.
fn canonical(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 32);
    let mut previous = 0u8;
    for &byte in bytes {
        if byte == b'\n' && previous != b'\r' {
            out.push(b'\r');
        }
        out.push(byte);
        previous = byte;
    }
    out
}

/// The header block of a message, minus what described its body -- the
/// decrypted entity brings its own -- and with the blank line left off.
fn outer_headers(raw: &[u8]) -> Vec<u8> {
    let end = header_end(raw).unwrap_or(raw.len());
    let text = String::from_utf8_lossy(&raw[..end]);
    let mut out = String::new();
    let mut skipping = false;
    for line in text.split_inclusive('\n') {
        let is_continuation = line.starts_with([' ', '\t']);
        if !is_continuation {
            let name = line
                .split(':')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            skipping = name == "content-type"
                || name == "content-transfer-encoding"
                || name == "mime-version";
        }
        if !skipping && !line.trim().is_empty() {
            out.push_str(line);
        }
    }
    if !out.ends_with('\n') {
        out.push_str("\r\n");
    }
    out.push_str("MIME-Version: 1.0\r\n");
    out.into_bytes()
}

/// Where the header block ends (the start of the blank line).
fn header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| at + 2)
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|at| at + 1))
}

/// The armored ciphertext of a `multipart/encrypted` message: its second
/// part.
pub(crate) fn encrypted_payload(raw: &[u8]) -> Option<Vec<u8>> {
    let message = mail_parser::MessageParser::default().parse(raw)?;
    message
        .parts
        .iter()
        .find(|part| {
            let body = part.contents();
            body.windows(27)
                .any(|w| w == b"-----BEGIN PGP MESSAGE-----")
        })
        .map(|part| part.contents().to_vec())
}

/// The first inline PGP block of a text body.
fn inline_block(raw: &[u8]) -> Option<Vec<u8>> {
    let message = mail_parser::MessageParser::default().parse(raw)?;
    let text = message.body_text(0)?;
    let start = text.find("-----BEGIN PGP MESSAGE-----")?;
    let end_marker = "-----END PGP MESSAGE-----";
    let end = text[start..].find(end_marker)? + start + end_marker.len();
    Some(text[start..end].as_bytes().to_vec())
}

/// The exact bytes of a `multipart/signed` message's first part, and its
/// signature part's content. The first part has to come out byte for byte
/// as sent -- the signature covers it -- so it's cut from the raw message
/// at its boundaries rather than re-serialised.
pub(crate) fn signed_parts(raw: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let message = mail_parser::MessageParser::default().parse(raw)?;
    let boundary = message.content_type()?.attribute("boundary")?.to_string();
    let delimiter = format!("--{boundary}");
    let body_start = header_end(raw)?;
    let body = &raw[body_start..];
    let lines = split_lines(body);
    let marks: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, (line, _))| {
            let text = String::from_utf8_lossy(line);
            let text = text.trim_end();
            text == delimiter || text == format!("{delimiter}--")
        })
        .map(|(index, _)| index)
        .collect();
    let (&first, &second) = (marks.first()?, marks.get(1)?);
    // From after the first delimiter line to before the CRLF that precedes
    // the second (RFC 2046: that CRLF belongs to the delimiter).
    let start = lines[first].1 + lines[first].0.len();
    let start = start + newline_len(body, start);
    let end = lines[second].1;
    let end = end - preceding_newline_len(body, end);
    let signed = body.get(start..end)?.to_vec();
    let signature = message
        .parts
        .iter()
        .find(|part| {
            part.content_type().is_some_and(|ct| {
                ct.subtype()
                    .is_some_and(|sub| sub.eq_ignore_ascii_case("pgp-signature"))
            })
        })?
        .contents()
        .to_vec();
    Some((signed, signature))
}

/// Each line with its start offset, line ends excluded.
fn split_lines(body: &[u8]) -> Vec<(&[u8], usize)> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, &byte) in body.iter().enumerate() {
        if byte == b'\n' {
            let end = if index > start && body[index - 1] == b'\r' {
                index - 1
            } else {
                index
            };
            lines.push((&body[start..end], start));
            start = index + 1;
        }
    }
    if start < body.len() {
        lines.push((&body[start..], start));
    }
    lines
}

fn newline_len(body: &[u8], at: usize) -> usize {
    match body.get(at..at + 2) {
        Some(b"\r\n") => 2,
        _ if body.get(at) == Some(&b'\n') => 1,
        _ => 0,
    }
}

fn preceding_newline_len(body: &[u8], at: usize) -> usize {
    if at >= 2 && &body[at - 2..at] == b"\r\n" {
        2
    } else if at >= 1 && body[at - 1] == b'\n' {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_kinds_of_protection() {
        let encrypted = b"Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=b\r\n\r\n--b\r\nContent-Type: application/pgp-encrypted\r\n\r\nVersion: 1\r\n--b\r\nContent-Type: application/octet-stream\r\n\r\n-----BEGIN PGP MESSAGE-----\r\nx\r\n-----END PGP MESSAGE-----\r\n--b--\r\n";
        assert_eq!(detect(encrypted), Protection::Encrypted);
        assert_eq!(
            encrypted_payload(encrypted).unwrap(),
            b"-----BEGIN PGP MESSAGE-----\r\nx\r\n-----END PGP MESSAGE-----"
        );
        let inline = b"Content-Type: text/plain\r\n\r\nhi\r\n-----BEGIN PGP MESSAGE-----\r\nabc\r\n-----END PGP MESSAGE-----\r\n";
        assert_eq!(detect(inline), Protection::InlineEncrypted);
        assert_eq!(detect(b"Subject: x\r\n\r\nplain"), Protection::None);
    }

    #[test]
    fn cuts_the_signed_part_byte_for_byte() {
        let raw = b"From: a@x.y\r\nContent-Type: multipart/signed; micalg=pgp-sha256;\r\n protocol=\"application/pgp-signature\"; boundary=\"s\"\r\n\r\nThis is an OpenPGP/MIME signed message.\r\n--s\r\nContent-Type: text/plain\r\n\r\nSigned text.\r\n--s\r\nContent-Type: application/pgp-signature; name=\"signature.asc\"\r\n\r\n-----BEGIN PGP SIGNATURE-----\r\nsig\r\n-----END PGP SIGNATURE-----\r\n--s--\r\n";
        assert_eq!(detect(raw), Protection::Signed);
        let (signed, signature) = signed_parts(raw).unwrap();
        assert_eq!(signed, b"Content-Type: text/plain\r\n\r\nSigned text.");
        assert!(signature.starts_with(b"-----BEGIN PGP SIGNATURE-----"));
    }

    #[test]
    fn reads_status_lines() {
        let good = "[GNUPG:] NEWSIG\n[GNUPG:] GOODSIG 1234ABCD Ada <ada@x.y>\n[GNUPG:] VALIDSIG ...\n[GNUPG:] TRUST_ULTIMATE 0 pgp\n";
        assert_eq!(
            signature_from_status(good),
            Some(SignatureStatus::Good {
                signer: "Ada <ada@x.y>".into(),
                is_trusted: true
            })
        );
        let unknown = "[GNUPG:] ERRSIG 99AA 1 8 00 1700000000 9 -\n[GNUPG:] NO_PUBKEY 99AA\n";
        assert_eq!(
            signature_from_status(unknown),
            Some(SignatureStatus::UnknownKey {
                key_id: "99AA".into()
            })
        );
        assert_eq!(signature_from_status("[GNUPG:] DECRYPTION_OKAY\n"), None);
    }

    #[test]
    fn decrypted_messages_keep_the_outer_headers() {
        let raw = b"From: a@x.y\r\nSubject: hi\r\nMIME-Version: 1.0\r\nContent-Type: multipart/encrypted;\r\n boundary=b\r\n\r\nbody";
        let headers = String::from_utf8(outer_headers(raw)).unwrap();
        assert_eq!(
            headers,
            "From: a@x.y\r\nSubject: hi\r\nMIME-Version: 1.0\r\n"
        );
        assert_eq!(canonical(b"a\nb\r\nc"), b"a\r\nb\r\nc");
    }

    /// The whole loop against a real gpg with a throwaway key, when gpg is
    /// installed: encrypt-and-sign, decrypt, and sign-then-verify.
    #[test]
    fn round_trips_through_gpg() {
        if !is_available() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        // SAFETY: tests that touch GNUPGHOME run single-threaded within this
        // one test; nothing else in the suite reads it.
        unsafe { std::env::set_var("GNUPGHOME", home.path()) };
        let made = Command::new("gpg")
            .args([
                "--batch",
                "--passphrase",
                "",
                "--quick-gen-key",
                "Test <test@rustle.invalid>",
                "default",
                "default",
                "never",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(made.success());
        assert!(has_secret_key("test@rustle.invalid"));
        let entity = b"Content-Type: text/plain; charset=utf-8\r\n\r\nHello, secret.\r\n";
        let armored = encrypt(entity, "test@rustle.invalid", &[], true).unwrap();
        let raw = [
            b"From: test@rustle.invalid\r\nSubject: s\r\nMIME-Version: 1.0\r\nContent-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: application/pgp-encrypted\r\n\r\nVersion: 1\r\n--b\r\nContent-Type: application/octet-stream\r\n\r\n".as_slice(),
            &armored,
            b"\r\n--b--\r\n",
        ]
        .concat();
        let decrypted = decrypt(&raw).unwrap();
        let parsed = crate::mime::parse_message(&decrypted.raw);
        assert_eq!(
            parsed.text_body.as_deref().map(str::trim),
            Some("Hello, secret.")
        );
        assert_eq!(parsed.subject, "s");
        assert!(matches!(
            decrypted.signature,
            Some(SignatureStatus::Good { .. })
        ));

        // The CRLF before a boundary belongs to the boundary, not the part.
        let part = entity.strip_suffix(b"\r\n").unwrap();
        let signature = sign(part, "test@rustle.invalid").unwrap();
        let signed = [
            b"From: test@rustle.invalid\r\nContent-Type: multipart/signed; micalg=pgp-sha256; protocol=\"application/pgp-signature\"; boundary=\"s\"\r\n\r\n--s\r\n".as_slice(),
            part,
            b"\r\n--s\r\nContent-Type: application/pgp-signature\r\n\r\n",
            &signature,
            b"\r\n--s--\r\n",
        ]
        .concat();
        assert!(matches!(
            verify(&signed).unwrap(),
            SignatureStatus::Good { .. }
        ));
        let tampered = String::from_utf8(signed).unwrap().replace("Hello", "Jello");
        assert!(matches!(
            verify(tampered.as_bytes()).unwrap(),
            SignatureStatus::Bad { .. }
        ));
        // A whole composed message, signed then encrypted, read back.
        let to = vec!["test@rustle.invalid".to_string()];
        let outgoing = crate::compose::Outgoing {
            from: "test@rustle.invalid",
            to: &to,
            subject: "Composed",
            body_html: "<p>Composed and <b>signed</b></p>",
            ..Default::default()
        };
        let gpg = Gpg {
            sender: "test@rustle.invalid".into(),
            recipients: to.clone(),
        };
        let only_signed = crate::compose::build_protected_message(
            &outgoing,
            crate::compose::Protection {
                sign: true,
                encrypt: false,
            },
            &gpg,
        )
        .unwrap();
        assert!(matches!(
            verify(&only_signed).unwrap(),
            SignatureStatus::Good { .. }
        ));
        let sealed = crate::compose::build_protected_message(
            &outgoing,
            crate::compose::Protection {
                sign: true,
                encrypt: true,
            },
            &gpg,
        )
        .unwrap();
        let opened = decrypt(&sealed).unwrap();
        let parsed = crate::mime::parse_message(&opened.raw);
        assert!(parsed.html_body.unwrap().contains("<b>signed</b>"));
        assert!(matches!(
            opened.signature,
            Some(SignatureStatus::Good { .. })
        ));
        let _ = Command::new("gpgconf")
            .args(["--kill", "gpg-agent"])
            .status();
    }
}
