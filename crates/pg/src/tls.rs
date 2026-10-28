//! Connection TLS: rustls with libpq's `sslmode` semantics.
//!
//! | `sslmode`         | Encrypts                | Verifies the chain          | Verifies the host |
//! |-------------------|-------------------------|-----------------------------|-------------------|
//! | `disable`         | no                      | —                           | —                 |
//! | `allow`, `prefer` | if the server accepts   | no                          | no                |
//! | `require`         | yes                     | no                          | no                |
//! | `verify-ca`       | yes                     | yes                         | no                |
//! | `verify-full`     | yes                     | yes                         | yes               |
//!
//! Without `sslmode`, a local database (`localhost`, `127.0.0.1`, `::1` or a
//! Unix socket) connects without TLS, like the Postgres in `dev/compose.yaml`;
//! everything else uses `prefer`, libpq's default. The chain is verified
//! against the system root certificates.
//!
//! tokio-postgres only understands `disable`, `prefer` and `require`, so
//! `sslmode` is read here and taken out of the URL or `key=value` string
//! before handing it over.

use std::future::Future;
use std::str::FromStr as _;
use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use tokio_postgres::config::{Host, SslMode};
use tokio_postgres::{CancelToken, Client, Config, Connection, NoTls, Socket};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::ConnectError;

/// libpq's `sslmode` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl Mode {
    fn parse(value: &str) -> Result<Self, ConnectError> {
        Ok(match value {
            "disable" => Mode::Disable,
            // tokio-postgres cannot start without TLS and retry with it;
            // `prefer` also accepts both.
            "allow" | "prefer" => Mode::Prefer,
            "require" => Mode::Require,
            "verify-ca" => Mode::VerifyCa,
            "verify-full" => Mode::VerifyFull,
            other => {
                return Err(ConnectError::Config(format!(
                    "unknown sslmode: {other:?} (disable, allow, prefer, require, verify-ca or verify-full)"
                )));
            }
        })
    }
}

/// What it takes to open a connection from its URL.
pub(crate) struct Target {
    pub config: Config,
    pub mode: Mode,
}

impl Target {
    pub fn parse(url: &str) -> Result<Self, ConnectError> {
        let (url, ssl_mode) = take_ssl_mode(url);
        let mut config = Config::from_str(&url)?;
        let mode = match ssl_mode.as_deref().map(Mode::parse).transpose()? {
            Some(mode) => mode,
            None if is_local(&config) => Mode::Disable,
            None => match config.get_ssl_mode() {
                // `sslmode` in key=value format, which tokio-postgres already read.
                SslMode::Disable => Mode::Disable,
                SslMode::Require => Mode::Require,
                _ => Mode::Prefer,
            },
        };
        config.ssl_mode(match mode {
            Mode::Disable => SslMode::Disable,
            Mode::Prefer => SslMode::Prefer,
            Mode::Require | Mode::VerifyCa | Mode::VerifyFull => SslMode::Require,
        });
        Ok(Self { config, mode })
    }

    pub fn connector(&self) -> Result<Connector, ConnectError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier: Arc<dyn ServerCertVerifier> = match self.mode {
            Mode::Disable => return Ok(Connector::Plain),
            Mode::Prefer | Mode::Require => Arc::new(EncryptOnly(provider.clone())),
            Mode::VerifyCa => Arc::new(IgnoreHostname(self.webpki(&provider)?)),
            Mode::VerifyFull => self.webpki(&provider)?,
        };
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(tls_error)?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        Ok(Connector::Rustls(MakeRustlsConnect::new(config)))
    }

    fn webpki(
        &self,
        provider: &Arc<CryptoProvider>,
    ) -> Result<Arc<WebPkiServerVerifier>, ConnectError> {
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        if roots.is_empty() {
            return Err(ConnectError::Tls(
                "there are no root certificates to verify the server with".into(),
            ));
        }
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .map_err(tls_error)
    }
}

/// Takes `sslmode` out of a `postgres://` URL's query or a `key=value`
/// string.
fn take_ssl_mode(url: &str) -> (String, Option<String>) {
    let is_url = url.starts_with("postgres://") || url.starts_with("postgresql://");
    if !is_url {
        return take_key_value(url, "sslmode");
    }
    let Some((base, query)) = url.split_once('?') else {
        return (url.to_owned(), None);
    };
    let (mut ssl_mode, mut rest) = (None, Vec::new());
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        match pair.split_once('=') {
            Some(("sslmode", value)) => ssl_mode = Some(value.to_owned()),
            _ => rest.push(pair),
        }
    }
    let url = if rest.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", rest.join("&"))
    };
    (url, ssl_mode)
}

/// Takes the `key` pair out of a libpq `key=value` string: blanks may
/// surround `=`, and a value in single quotes escapes `'` and `\\` with a
/// backslash. Anything it cannot read is left for tokio-postgres to report.
fn take_key_value(conninfo: &str, key: &str) -> (String, Option<String>) {
    let (mut kept, mut value, mut rest) = (String::new(), None, conninfo);
    while let Some((pair, found, after)) = next_pair(rest) {
        match found {
            Some((name, found)) if name == key => value = Some(found),
            _ => kept.push_str(pair),
        }
        rest = after;
    }
    kept.push_str(rest);
    (kept, value)
}

/// A pair of a `key=value` string: its text with the blanks before it, its
/// key and value (`None` if malformed), and what follows.
type Pair<'a> = (&'a str, Option<(&'a str, String)>, &'a str);

fn next_pair(text: &str) -> Option<Pair<'_>> {
    let start = text.len() - text.trim_start().len();
    let body = &text[start..];
    if body.is_empty() {
        return None;
    }
    let key_end = body
        .find(|c: char| c == '=' || c.is_whitespace())
        .unwrap_or(body.len());
    let after_key = body[key_end..].trim_start();
    let Some(after_equals) = after_key.strip_prefix('=') else {
        return Some((text, None, ""));
    };
    let value_text = after_equals.trim_start();
    let (value, after) = match value_text.strip_prefix('\'') {
        Some(quoted) => {
            let mut value = String::new();
            let mut chars = quoted.char_indices();
            let mut end = None;
            while let Some((index, c)) = chars.next() {
                match c {
                    '\\' => value.extend(chars.next().map(|(_, escaped)| escaped)),
                    '\'' => {
                        end = Some(index + 1);
                        break;
                    }
                    c => value.push(c),
                }
            }
            let Some(end) = end else {
                return Some((text, None, ""));
            };
            (value, &quoted[end..])
        }
        None => {
            let end = value_text
                .find(char::is_whitespace)
                .unwrap_or(value_text.len());
            (value_text[..end].to_owned(), &value_text[end..])
        }
    };
    let length = text.len() - after.len();
    Some((&text[..length], Some((&body[..key_end], value)), after))
}

fn is_local(config: &Config) -> bool {
    config.get_hosts().iter().all(|host| match host {
        Host::Tcp(name) => matches!(name.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]"),
        #[cfg(unix)]
        Host::Unix(_) => true,
    })
}

fn tls_error(error: impl std::fmt::Display) -> ConnectError {
    ConnectError::Tls(error.to_string())
}

/// A session's connector; also needed to cancel its queries.
#[derive(Clone)]
pub(crate) enum Connector {
    Plain,
    Rustls(MakeRustlsConnect),
}

impl Connector {
    /// Connects and leaves the connection running on `handle`.
    pub async fn connect(
        &self,
        config: &Config,
        handle: &tokio::runtime::Handle,
    ) -> Result<Client, tokio_postgres::Error> {
        match self {
            Connector::Plain => {
                let (client, connection) = config.connect(NoTls).await?;
                drive(connection, handle);
                Ok(client)
            }
            Connector::Rustls(tls) => {
                let (client, connection) = config.connect(tls.clone()).await?;
                drive(connection, handle);
                Ok(client)
            }
        }
    }

    pub fn cancel(
        &self,
        token: CancelToken,
    ) -> impl Future<Output = Result<(), tokio_postgres::Error>> + Send + use<> {
        let tls = self.clone();
        async move {
            match tls {
                Connector::Plain => token.cancel_query(NoTls).await,
                Connector::Rustls(tls) => token.cancel_query(tls).await,
            }
        }
    }
}

fn drive<T>(connection: Connection<Socket, T>, handle: &tokio::runtime::Handle)
where
    T: tokio_postgres::tls::TlsStream + Unpin + Send + 'static,
{
    handle.spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("kuery-pg: connection closed: {error}");
        }
    });
}

/// `prefer` and `require`: encrypts, but accepts any certificate (like libpq).
/// Handshake signatures are still checked.
#[derive(Debug)]
struct EncryptOnly(Arc<CryptoProvider>);

impl ServerCertVerifier for EncryptOnly {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// `verify-ca`: the chain must be valid; the host name does not matter.
#[derive(Debug)]
struct IgnoreHostname(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for IgnoreHostname {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(url: &str) -> Mode {
        Target::parse(url).unwrap().mode
    }

    #[test]
    fn local_databases_default_to_plain_connections() {
        assert_eq!(mode("postgres://u:p@localhost:54329/db"), Mode::Disable);
        assert_eq!(mode("postgres://u:p@127.0.0.1/db"), Mode::Disable);
        assert_eq!(mode("postgres://u:p@[::1]:5432/db"), Mode::Disable);
        assert!(matches!(
            Target::parse("postgres://u:p@localhost/db")
                .unwrap()
                .connector()
                .unwrap(),
            Connector::Plain
        ));
    }

    #[test]
    fn remote_databases_default_to_prefer_like_libpq() {
        let target = Target::parse("postgres://u:p@db.example.com/db").unwrap();
        assert_eq!(target.mode, Mode::Prefer);
        assert_eq!(target.config.get_ssl_mode(), SslMode::Prefer);
        assert!(matches!(target.connector().unwrap(), Connector::Rustls(_)));
    }

    #[test]
    fn an_explicit_sslmode_wins() {
        assert_eq!(
            mode("postgres://u:p@db.example.com/db?sslmode=disable"),
            Mode::Disable
        );
        assert_eq!(
            mode("postgres://u:p@localhost/db?sslmode=require"),
            Mode::Require
        );
        assert_eq!(mode("postgres://u:p@h/db?sslmode=allow"), Mode::Prefer);
        assert_eq!(
            mode("postgres://u:p@h/db?sslmode=verify-ca"),
            Mode::VerifyCa
        );
        let target = Target::parse("postgresql://u:p@h/db?sslmode=verify-full").unwrap();
        assert_eq!(target.mode, Mode::VerifyFull);
        assert_eq!(target.config.get_ssl_mode(), SslMode::Require);
    }

    #[test]
    fn key_value_strings_keep_the_modes_tokio_postgres_knows() {
        assert_eq!(
            mode("host=db.example.com user=u sslmode=require"),
            Mode::Require
        );
        assert_eq!(
            mode("host=db.example.com user=u sslmode=disable"),
            Mode::Disable
        );
        assert_eq!(mode("host=localhost user=u"), Mode::Disable);
    }

    #[test]
    fn every_mode_of_the_connection_form_connects() {
        for (tls, expected) in [
            ("disable", Mode::Disable),
            ("prefer", Mode::Prefer),
            ("require", Mode::Require),
            ("verify-ca", Mode::VerifyCa),
            ("verify-full", Mode::VerifyFull),
        ] {
            let options = kuery_core::driver::ConnectOptions {
                host: "db.example.com".into(),
                port: 5432,
                database: "shop".into(),
                user: "o'brien".into(),
                password: Some(r"p\ss".into()),
                tls: Some(tls.into()),
                tunnel: None,
                options: Default::default(),
            };
            let target = Target::parse(&crate::conninfo(&options)).unwrap();
            assert_eq!(target.mode, expected, "{tls}");
            assert_eq!(target.config.get_user(), Some("o'brien"));
            assert_eq!(target.config.get_password(), Some(&br"p\ss"[..]));
        }
    }

    #[test]
    fn key_value_strings_read_every_libpq_mode() {
        assert_eq!(
            mode("host=db.example.com user=u sslmode=verify-full"),
            Mode::VerifyFull
        );
        assert_eq!(
            mode("host=db.example.com sslmode = 'verify-ca' user=u"),
            Mode::VerifyCa
        );
        assert_eq!(mode("host=db.example.com sslmode=allow"), Mode::Prefer);
        assert_eq!(mode("host=localhost sslmode='require'"), Mode::Require);
    }

    #[test]
    fn other_parameters_reach_tokio_postgres() {
        let target = Target::parse(
            "postgres://u:p@h:6543/db?application_name=kuery&sslmode=require&connect_timeout=5",
        )
        .unwrap();
        assert_eq!(target.config.get_application_name(), Some("kuery"));
        assert_eq!(
            target.config.get_connect_timeout(),
            Some(&std::time::Duration::from_secs(5))
        );
        assert_eq!(target.config.get_ports(), &[6543]);
    }

    #[test]
    fn rejects_unknown_modes() {
        let Err(error) = Target::parse("postgres://u:p@h/db?sslmode=always") else {
            panic!("should fail");
        };
        assert!(error.to_string().contains("always"), "{error}");
    }

    #[test]
    fn verifying_modes_build_with_the_system_roots() {
        for url in [
            "postgres://u:p@h/db?sslmode=verify-ca",
            "postgres://u:p@h/db?sslmode=verify-full",
        ] {
            assert!(matches!(
                Target::parse(url).unwrap().connector().unwrap(),
                Connector::Rustls(_)
            ));
        }
    }
}
