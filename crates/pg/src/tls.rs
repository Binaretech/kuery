//! TLS de las conexiones: rustls con la semántica de `sslmode` de libpq.
//!
//! | `sslmode`         | Cifra                 | Verifica la cadena          | Verifica el host |
//! |-------------------|-----------------------|-----------------------------|------------------|
//! | `disable`         | no                    | —                           | —                |
//! | `allow`, `prefer` | si el servidor acepta | no                          | no               |
//! | `require`         | sí                    | solo con `sslrootcert`      | no               |
//! | `verify-ca`       | sí                    | sí                          | no               |
//! | `verify-full`     | sí                    | sí                          | sí               |
//!
//! Sin `sslmode`, una base de datos local (`localhost`, `127.0.0.1`, `::1` o
//! un socket Unix) va sin TLS, como el Postgres de `dev/compose.yaml`; el resto
//! usa `prefer`, el valor por defecto de libpq. La cadena se verifica contra
//! los certificados raíz del sistema o, si la URL trae `sslrootcert`, contra
//! los de ese fichero PEM.
//!
//! tokio-postgres solo entiende `disable`, `prefer` y `require` y rechaza
//! `sslrootcert`, así que esos dos parámetros se leen aquí y se quitan de la
//! URL antes de pasársela. Solo se interpretan en URLs (`postgres://…`); en el
//! formato `clave=valor` la cadena llega tal cual a tokio-postgres.

use std::future::Future;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use tokio_postgres::config::{Host, SslMode};
use tokio_postgres::{CancelToken, Client, Config, Connection, NoTls, Socket};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::ConnectError;

/// Los valores de `sslmode` de libpq.
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
            // tokio-postgres no sabe empezar sin TLS y reintentar con él;
            // `prefer` también admite las dos cosas.
            "allow" | "prefer" => Mode::Prefer,
            "require" => Mode::Require,
            "verify-ca" => Mode::VerifyCa,
            "verify-full" => Mode::VerifyFull,
            other => {
                return Err(ConnectError::Config(format!(
                    "sslmode desconocido: {other:?} (disable, allow, prefer, require, verify-ca o verify-full)"
                )));
            }
        })
    }
}

/// Lo que hace falta para abrir una conexión a partir de su URL.
pub(crate) struct Target {
    pub config: Config,
    pub mode: Mode,
    pub root_cert: Option<PathBuf>,
}

impl Target {
    pub fn parse(url: &str) -> Result<Self, ConnectError> {
        let (url, ssl_mode, root_cert) = take_tls_params(url);
        let mut config = Config::from_str(&url)?;
        let root_cert = root_cert.map(PathBuf::from);
        let mode = match ssl_mode.as_deref().map(Mode::parse).transpose()? {
            // Como libpq: con un fichero de raíces, `require` también verifica la cadena.
            Some(Mode::Require) if root_cert.is_some() => Mode::VerifyCa,
            Some(mode) => mode,
            None if is_local(&config) => Mode::Disable,
            None => match config.get_ssl_mode() {
                // `sslmode` en formato clave=valor, que tokio-postgres ya leyó.
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
        Ok(Self {
            config,
            mode,
            root_cert,
        })
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
        match &self.root_cert {
            Some(path) => {
                let certs = CertificateDer::pem_file_iter(path)
                    .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
                    .map_err(|error| {
                        ConnectError::Tls(format!("no se pudo leer {}: {error}", path.display()))
                    })?;
                roots.add_parsable_certificates(certs);
            }
            None => {
                roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
            }
        }
        if roots.is_empty() {
            return Err(ConnectError::Tls(
                "no hay certificados raíz con los que verificar el servidor".into(),
            ));
        }
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .map_err(tls_error)
    }
}

/// Saca `sslmode` y `sslrootcert` de la query de una URL `postgres://`.
fn take_tls_params(url: &str) -> (String, Option<String>, Option<String>) {
    let is_url = url.starts_with("postgres://") || url.starts_with("postgresql://");
    let Some((base, query)) = url.split_once('?').filter(|_| is_url) else {
        return (url.to_owned(), None, None);
    };
    let (mut ssl_mode, mut root_cert, mut rest) = (None, None, Vec::new());
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "sslmode" => ssl_mode = Some(percent_decode(value)),
            "sslrootcert" => root_cert = Some(percent_decode(value)),
            _ => rest.push(pair),
        }
    }
    let url = if rest.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", rest.join("&"))
    };
    (url, ssl_mode, root_cert)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes
            .get(index + 1..index + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[index], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                index += 3;
            }
            (byte, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

/// Conector de una sesión; también hace falta para cancelar sus queries.
#[derive(Clone)]
pub(crate) enum Connector {
    Plain,
    Rustls(MakeRustlsConnect),
}

impl Connector {
    /// Conecta y deja la conexión corriendo en `handle`.
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
            eprintln!("dbadmin-pg: conexión cerrada: {error}");
        }
    });
}

/// `prefer` y `require`: cifra, pero acepta cualquier certificado (como libpq).
/// Las firmas del handshake sí se comprueban.
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

/// `verify-ca`: la cadena tiene que ser válida, el nombre del host da igual.
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
    fn a_root_certificate_makes_require_verify_the_chain() {
        let target =
            Target::parse("postgres://u:p@h/db?sslmode=require&sslrootcert=%2Ftmp%2Fca%20dev.pem")
                .unwrap();
        assert_eq!(target.mode, Mode::VerifyCa);
        assert_eq!(target.root_cert, Some(PathBuf::from("/tmp/ca dev.pem")));
        assert!(matches!(target.connector(), Err(ConnectError::Tls(_))));
    }

    #[test]
    fn other_parameters_reach_tokio_postgres() {
        let target = Target::parse(
            "postgres://u:p@h:6543/db?application_name=dbadmin&sslmode=require&connect_timeout=5",
        )
        .unwrap();
        assert_eq!(target.config.get_application_name(), Some("dbadmin"));
        assert_eq!(
            target.config.get_connect_timeout(),
            Some(&std::time::Duration::from_secs(5))
        );
        assert_eq!(target.config.get_ports(), &[6543]);
    }

    #[test]
    fn rejects_unknown_modes() {
        let Err(error) = Target::parse("postgres://u:p@h/db?sslmode=always") else {
            panic!("debería fallar");
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
