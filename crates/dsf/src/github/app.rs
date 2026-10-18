//! The GitHub App's private key, and the short-lived JWTs it signs. A JWT
//! only buys installation tokens; it can't touch a repository itself.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use jiff::Timestamp;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use secrecy::{ExposeSecret, SecretSlice, SecretString};

pub struct AppKey {
    app_id: u64,
    key: RsaKeyPair,
}

impl AppKey {
    /// Reads the key GitHub generated for the app. It must be readable by
    /// its owner only.
    pub fn load(app_id: u64, path: &Path) -> Result<Self> {
        let mode = std::fs::metadata(path)
            .with_context(|| format!("reading the GitHub App key {}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            bail!(
                "{} can be read by other users; run `chmod 600` on it",
                path.display()
            );
        }
        let pem = SecretString::from(
            std::fs::read_to_string(path)
                .with_context(|| format!("reading the GitHub App key {}", path.display()))?,
        );
        Self::from_pem(app_id, pem.expose_secret())
            .with_context(|| format!("the GitHub App key {}", path.display()))
    }

    /// PKCS#1, as GitHub issues them, or PKCS#8.
    pub fn from_pem(app_id: u64, pem: &str) -> Result<Self> {
        let (label, der) = decode_pem(pem)?;
        let key = match label.as_str() {
            "RSA PRIVATE KEY" => RsaKeyPair::from_der(der.expose_secret()),
            "PRIVATE KEY" => RsaKeyPair::from_pkcs8(der.expose_secret()),
            other => bail!("expected an RSA private key, found a PEM `{other}`"),
        }
        .map_err(|error| anyhow!("not a usable RSA key: {error}"))?;
        Ok(Self { app_id, key })
    }

    pub fn app_id(&self) -> u64 {
        self.app_id
    }

    /// A JWT for the app endpoints. GitHub allows ten minutes; this one is
    /// issued a minute in the past to allow for clock drift, and lasts nine.
    pub fn jwt(&self, now: Timestamp) -> Result<SecretString> {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let now = now.as_second();
        let header = b64.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = serde_json::json!({ "iat": now - 60, "exp": now + 540, "iss": self.app_id });
        let payload = b64.encode(serde_json::to_vec(&claims)?);
        let message = format!("{header}.{payload}");
        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                message.as_bytes(),
                &mut signature,
            )
            .map_err(|_| anyhow!("signing the GitHub App JWT failed"))?;
        Ok(SecretString::from(format!(
            "{message}.{}",
            b64.encode(signature)
        )))
    }

    /// The public half, for tests that check signatures.
    pub fn public_key_der(&self) -> Vec<u8> {
        self.key.public().as_ref().to_vec()
    }
}

/// The label and contents of the first PEM block.
fn decode_pem(pem: &str) -> Result<(String, SecretSlice<u8>)> {
    let mut lines = pem.lines().map(str::trim);
    let label = lines
        .by_ref()
        .find_map(|line| {
            line.strip_prefix("-----BEGIN ")
                .and_then(|rest| rest.strip_suffix("-----"))
        })
        .context("no PEM block found")?
        .to_owned();
    let end = format!("-----END {label}-----");
    let mut body = String::new();
    let mut closed = false;
    for line in lines {
        if line == end {
            closed = true;
            break;
        }
        body.push_str(line);
    }
    if !closed {
        bail!("the PEM block `{label}` is not closed");
    }
    let der = base64::engine::general_purpose::STANDARD
        .decode(body.as_bytes())
        .map_err(|_| anyhow!("the PEM block `{label}` is not valid base64"))?;
    Ok((label, SecretSlice::from(der)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    /// A throwaway key from the system's openssl, so no private key is ever
    /// committed. PKCS#8 from `genpkey`; PKCS#1 converted from it.
    pub(crate) fn generated_keys() -> (String, String) {
        let dir = tempfile::tempdir().unwrap();
        let pkcs8 = dir.path().join("pkcs8.pem");
        let openssl = |args: &[&str]| {
            std::process::Command::new("openssl")
                .args(args)
                .output()
                .unwrap()
        };
        let out = openssl(&[
            "genpkey",
            "-algorithm",
            "RSA",
            "-pkeyopt",
            "rsa_keygen_bits:2048",
            "-out",
            pkcs8.to_str().unwrap(),
        ]);
        assert!(out.status.success(), "openssl genpkey failed");
        let pkcs8_pem = std::fs::read_to_string(&pkcs8).unwrap();
        // OpenSSL 3 needs -traditional for PKCS#1; LibreSSL writes it anyway.
        let mut out = openssl(&["rsa", "-in", pkcs8.to_str().unwrap(), "-traditional"]);
        if !out.status.success() {
            out = openssl(&["rsa", "-in", pkcs8.to_str().unwrap()]);
        }
        let pkcs1_pem = String::from_utf8(out.stdout).unwrap();
        assert!(pkcs1_pem.contains("BEGIN RSA PRIVATE KEY"));
        (pkcs1_pem, pkcs8_pem)
    }

    #[test]
    fn signs_a_verifiable_jwt_with_either_key_format() {
        let (pkcs1, pkcs8) = generated_keys();
        let now = Timestamp::from_second(1_800_000_000).unwrap();
        for pem in [pkcs1, pkcs8] {
            let key = AppKey::from_pem(42, &pem).unwrap();
            let jwt = key.jwt(now).unwrap();
            let parts: Vec<&str> = jwt.expose_secret().split('.').collect();
            assert_eq!(parts.len(), 3);
            let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
            let claims: serde_json::Value =
                serde_json::from_slice(&b64.decode(parts[1]).unwrap()).unwrap();
            assert_eq!(
                claims,
                serde_json::json!({ "iat": 1_799_999_940, "exp": 1_800_000_540, "iss": 42 })
            );
            let public = key.public_key_der();
            UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, &public)
                .verify(
                    format!("{}.{}", parts[0], parts[1]).as_bytes(),
                    &b64.decode(parts[2]).unwrap(),
                )
                .unwrap();
        }
    }

    #[test]
    fn rejects_what_is_not_an_rsa_key() {
        for pem in [
            "",
            "just text",
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
            "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n",
            "-----BEGIN RSA PRIVATE KEY-----\n!!!!\n-----END RSA PRIVATE KEY-----\n",
            "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\n",
        ] {
            assert!(AppKey::from_pem(1, pem).is_err(), "{pem:?}");
        }
    }

    #[test]
    fn refuses_a_key_others_can_read() {
        let (pkcs1, _) = generated_keys();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.pem");
        std::fs::write(&path, pkcs1).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = AppKey::load(1, &path).err().unwrap().to_string();
        assert!(error.contains("chmod 600"), "{error}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(AppKey::load(1, &path).unwrap().app_id(), 1);
    }
}
