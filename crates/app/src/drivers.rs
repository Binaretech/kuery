//! The drivers Kuery has, and the one way the app connects with each
//! family.
//!
//! A SQL driver is a crate that exports a `DRIVER`
//! ([`kuery_core::driver::SqlDriver`]): its descriptor, the SQL it speaks,
//! and how it opens an [`kuery_core::adapter::Adapter`]. Listing it in
//! [`SQL`] is all the app needs: testing, connecting, SSH tunnels and the
//! extra connections write mode opens all go through [`Driver`] the same
//! way for every one. Redis is the key-value family, with its own session.
//! A document driver exports a `DRIVER` too
//! ([`kuery_core::driver::DocumentDriver`]), listed in [`DOCUMENT`]: it
//! connects like a SQL one, into a
//! [`kuery_core::document::session::DocumentSession`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use gpui_kit::component::{Icon, IconName};
use kuery_core::document::capabilities::Enforcement;
use kuery_core::document::session::{DocumentOpener, DocumentSession};
use kuery_core::driver::{self, ConnectOptions, DocumentDriver, DriverInfo, Family, SqlDriver};
use kuery_core::session::{Access, Opener, Session};

use crate::assets::AppIcon;
use crate::connection::{Db, DocDb, RedisDb};
use crate::project::ConnectionSpec;
use crate::tunnel::{self, Tunnel};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Every SQL driver, in the order the list shows them.
pub static SQL: [&SqlDriver; 4] = [
    &kuery_pg::DRIVER,
    &kuery_mysql::DRIVER,
    &kuery_mssql::DRIVER,
    &kuery_sqlite::DRIVER,
];

/// Every document driver, in the order the list shows them.
pub static DOCUMENT: [&DocumentDriver; 1] = [&kuery_mongo::DRIVER];

/// Makes ring the process's rustls provider, before any connection.
/// Redis and MySQL need a process-wide default once a second provider is compiled in.
pub fn install_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The background runtimes drivers outside the SQL family run on.
#[derive(Clone)]
pub struct Engines {
    pub redis: Arc<kuery_redis::Engine>,
}

/// An open connection, by family.
pub enum Connection {
    Sql(Db),
    KeyValue(RedisDb),
    Document(DocDb),
}

/// A driver Kuery has: a SQL one, Redis, or a document one.
#[derive(Clone, Copy)]
pub enum Driver {
    Sql(&'static SqlDriver),
    Redis,
    Document(&'static DocumentDriver),
}

/// Every driver, in the order the list shows them.
pub fn all() -> Vec<Driver> {
    SQL.iter()
        .map(|sql| Driver::Sql(sql))
        .chain([Driver::Redis])
        .chain(DOCUMENT.iter().map(|document| Driver::Document(document)))
        .collect()
}

pub fn find(id: &str) -> Option<Driver> {
    all().into_iter().find(|driver| driver.info().id == id)
}

/// The driver a connection names, or why there is none.
pub fn for_spec(spec: &ConnectionSpec) -> Result<Driver, String> {
    find(&spec.driver).ok_or_else(|| {
        format!(
            "This version of Kuery has no \u{201c}{}\u{201d} driver",
            spec.driver
        )
    })
}

/// The icon of a driver's family, in the sidebar and the driver list.
pub fn family_icon(family: Family) -> Icon {
    match family {
        Family::Sql => Icon::from(AppIcon::Database),
        Family::KeyValue => Icon::from(IconName::MemoryStick),
        Family::Document => Icon::from(IconName::FileText),
    }
}

impl Driver {
    pub fn info(self) -> &'static DriverInfo {
        match self {
            Driver::Sql(sql) => &sql.info,
            Driver::Redis => &driver::REDIS,
            Driver::Document(document) => &document.info,
        }
    }

    /// Connects read-only with the form's settings and says what it
    /// reached, such as "Connected · PostgreSQL 16.4".
    pub fn test(
        self,
        spec: &ConnectionSpec,
        password: Option<String>,
        engines: &Engines,
    ) -> BoxFuture<Result<String, String>> {
        let spec = spec.clone();
        match self {
            Driver::Sql(sql) => Box::pin(async move {
                let tunnel = tunnel::open_for(&spec).await?;
                let session =
                    open_sql(sql, &spec, password, tunnel.as_deref(), Access::ReadOnly).await?;
                let verb = if sql.info.file { "Opened" } else { "Connected" };
                Ok(format!(
                    "{verb}{} · {}",
                    through(&spec),
                    session.server_label()
                ))
            }),
            Driver::Redis => {
                let engine = engines.redis.clone();
                Box::pin(async move {
                    let tunnel = tunnel::open_for(&spec).await?;
                    let target = spec
                        .redis_target(password.as_deref(), tunnel.as_ref().map(|t| t.local_port()));
                    let session = engine
                        .connect(target, Access::ReadOnly)
                        .await
                        .map_err(|e| e.to_string())?;
                    let server = session.server();
                    Ok(format!(
                        "Connected{} · Redis {} · {}",
                        through(&spec),
                        server.version,
                        server.mode
                    ))
                })
            }
            Driver::Document(document) => Box::pin(async move {
                let tunnel = tunnel::open_for(&spec).await?;
                let access = spec.environment().access();
                let session =
                    open_document(document, &spec, password, tunnel.as_deref(), access).await?;
                Ok(reached(&spec, &session))
            }),
        }
    }

    /// Opens the connection the sidebar keeps and the tabs share.
    pub fn connect(
        self,
        spec: &ConnectionSpec,
        password: Option<String>,
        engines: &Engines,
    ) -> BoxFuture<Result<Connection, String>> {
        let spec = spec.clone();
        match self {
            Driver::Sql(sql) => Box::pin(async move {
                let tunnel = tunnel::open_for(&spec).await?;
                let access = spec.environment().access();
                let session = open_sql(sql, &spec, password, tunnel.as_deref(), access).await?;
                Ok(Connection::Sql(Db::new(
                    spec.name.clone().into(),
                    spec.environment(),
                    session,
                    spec.target().into(),
                    tunnel,
                )))
            }),
            Driver::Redis => {
                let engine = engines.redis.clone();
                Box::pin(async move {
                    let tunnel = tunnel::open_for(&spec).await?;
                    let target = spec
                        .redis_target(password.as_deref(), tunnel.as_ref().map(|t| t.local_port()));
                    let access = spec.environment().access();
                    let session = engine
                        .connect(target, access)
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(Connection::KeyValue(RedisDb::new(
                        spec.name.clone().into(),
                        spec.environment(),
                        session,
                        spec.target().into(),
                        tunnel,
                    )))
                })
            }
            Driver::Document(document) => Box::pin(async move {
                let tunnel = tunnel::open_for(&spec).await?;
                let access = spec.environment().access();
                let catalog =
                    open_document(document, &spec, password, tunnel.as_deref(), access).await?;
                let home = Some(spec.database.trim().to_string())
                    .filter(|database| !database.is_empty())
                    .into_iter()
                    .collect();
                Ok(Connection::Document(DocDb::new(
                    spec.name.clone().into(),
                    spec.environment(),
                    catalog,
                    spec.target().into(),
                    home,
                    tunnel,
                )))
            }),
        }
    }
}

/// What a driver connects with: the connection's settings, the password
/// (only ever in memory), and the local end of its tunnel if there is one,
/// as a port to the server or a proxy to dial it through.
pub fn connect_options(
    spec: &ConnectionSpec,
    password: Option<String>,
    tunnel: Option<&Tunnel>,
) -> ConnectOptions {
    let local = |proxy: bool| {
        tunnel
            .filter(|tunnel| tunnel.is_proxy() == proxy)
            .map(Tunnel::local_port)
    };
    ConnectOptions {
        host: spec.host.clone(),
        port: spec.port,
        database: spec.database.clone(),
        user: spec.user.clone(),
        password: password.filter(|p| !p.is_empty()),
        tls: spec.sslmode.clone(),
        tunnel: local(false),
        proxy: local(true),
        options: spec.options.clone(),
    }
}

/// Opens a session with a SQL driver, through the tunnel if there is one.
/// What it connects with (password included) stays in memory, for
/// [`Session::reopen`].
fn open_sql(
    sql: &'static SqlDriver,
    spec: &ConnectionSpec,
    password: Option<String>,
    tunnel: Option<&Tunnel>,
    access: Access,
) -> impl Future<Output = Result<Session, String>> + Send + use<> {
    let options = connect_options(spec, password, tunnel);
    let opener: Opener = Arc::new(move |access| (sql.open)(&options, access));
    Session::open(opener, access)
}

/// Opens a session with a document driver, through the tunnel if there is
/// one. What it connects with stays in memory, for
/// [`DocumentSession::reopen`].
fn open_document(
    document: &'static DocumentDriver,
    spec: &ConnectionSpec,
    password: Option<String>,
    tunnel: Option<&Tunnel>,
    access: Access,
) -> impl Future<Output = Result<DocumentSession, String>> + Send + use<> {
    let options = connect_options(spec, password, tunnel);
    let opener: DocumentOpener = Arc::new(move |access| (document.open)(&options, access));
    DocumentSession::open(opener, access)
}

/// What Test Connection says it reached: the server, whether the server
/// itself kept this user from writing when it connected (privileges can
/// change after), and what the driver says the user should know, on a line
/// of its own.
fn reached(spec: &ConnectionSpec, session: &DocumentSession) -> String {
    let mut text = format!("Connected{} · {}", through(spec), session.server_label());
    if session.capabilities().read_only == Enforcement::Server {
        text.push_str(" · at connect time, this user could only read");
    }
    if let Some(notice) = session.notice() {
        text.push('\n');
        text.push_str(notice);
    }
    text
}

/// ` through bastion` when the connection goes through SSH.
fn through(spec: &ConnectionSpec) -> String {
    spec.ssh
        .as_ref()
        .map(|ssh| format!(" through {}", ssh.host))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::SshSpec;

    #[test]
    fn rustls_configs_build_with_the_default_provider() {
        install_tls_provider();
        let _ = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
    }

    #[test]
    fn every_driver_is_found_by_its_id_once() {
        let ids: Vec<&str> = all().iter().map(|d| d.info().id).collect();
        assert_eq!(
            ids,
            ["postgres", "mysql", "mssql", "sqlite", "redis", "mongodb"]
        );
        assert!(find("mssql").unwrap().info().matches("sql server"));
        assert!(matches!(find("redis"), Some(Driver::Redis)));
        let mongodb = find("mongodb").unwrap().info();
        assert!(mongodb.matches("atlas") && mongodb.matches("document"));
        for document in DOCUMENT {
            assert_eq!(document.info.family, Family::Document);
        }
        assert!(find("oracle").is_none());
        for sql in SQL {
            assert_eq!(sql.info.family, Family::Sql);
        }
    }

    /// Needs the dev Postgres, MySQL, SQL Server, Redis, MongoDB and bastion
    /// (`docker compose -f dev/compose.yaml up -d`).
    #[test]
    #[ignore]
    fn connects_through_the_dev_bastion() {
        let engines = Engines {
            redis: Arc::new(kuery_redis::Engine::new().unwrap()),
        };
        let ssh = Some(SshSpec {
            host: "localhost".into(),
            port: Some(22229),
            user: "kuery".into(),
            identity_file: String::new(),
        });
        let postgres = ConnectionSpec {
            id: "t".into(),
            name: "t".into(),
            driver: "postgres".into(),
            host: "postgres".into(),
            port: 5432,
            database: "kuery".into(),
            user: "kuery".into(),
            sslmode: None,
            environment: None,
            ssh: ssh.clone(),
            options: Default::default(),
        };
        let test_with = |spec: &ConnectionSpec, password: Option<String>| {
            let driver = for_spec(spec).unwrap();
            futures::executor::block_on(driver.test(spec, password, &engines)).unwrap()
        };
        let test = |spec: &ConnectionSpec| test_with(spec, None);
        let reached = test(&postgres);
        assert!(
            reached.starts_with("Connected through localhost · PostgreSQL 16"),
            "{reached}"
        );
        let redis = ConnectionSpec {
            driver: "redis".into(),
            host: "redis".into(),
            port: 6379,
            database: "0".into(),
            user: String::new(),
            ..postgres.clone()
        };
        let reached = test(&redis);
        assert!(
            reached.starts_with("Connected through localhost · Redis 7"),
            "{reached}"
        );
        let mysql = ConnectionSpec {
            driver: "mysql".into(),
            host: "mysql".into(),
            port: 3306,
            database: "shop".into(),
            ..postgres.clone()
        };
        let reached = test(&mysql);
        assert!(
            reached.starts_with("Connected through localhost · MySQL 8"),
            "{reached}"
        );
        let mssql = ConnectionSpec {
            driver: "mssql".into(),
            host: "mssql".into(),
            port: 1433,
            database: "shop".into(),
            user: "sa".into(),
            sslmode: Some("trust server certificate".into()),
            ..postgres
        };
        let reached = test_with(&mssql, Some("Kuery_dev_2026!".into()));
        assert!(
            reached.starts_with("Connected through localhost · SQL Server 2022"),
            "{reached}"
        );
        let mongodb = ConnectionSpec {
            driver: "mongodb".into(),
            host: "mongo".into(),
            port: 27019,
            database: "shop".into(),
            user: "kuery".into(),
            sslmode: None,
            options: [("authSource".to_string(), "admin".to_string())].into(),
            ..mssql
        };
        let reached = test_with(&mongodb, Some("kuery".into()));
        assert!(
            reached.starts_with("Connected through localhost · MongoDB 8.0"),
            "{reached}"
        );
    }

    #[test]
    #[ignore = "needs the dev MongoDB"]
    fn testing_a_document_connection_says_what_the_user_should_know() {
        let engines = Engines {
            redis: Arc::new(kuery_redis::Engine::new().unwrap()),
        };
        let spec = ConnectionSpec {
            id: "m".into(),
            name: "m".into(),
            driver: "mongodb".into(),
            host: "127.0.0.1".into(),
            port: 27019,
            database: "shop".into(),
            user: "kuery".into(),
            sslmode: None,
            environment: None,
            ssh: None,
            options: [("authSource".to_string(), "admin".to_string())].into(),
        };
        let test = |spec: &ConnectionSpec, password: &str| {
            let driver = for_spec(spec).unwrap();
            futures::executor::block_on(driver.test(spec, Some(password.into()), &engines))
        };
        let reached = test(&spec, "kuery").unwrap();
        assert!(
            reached.starts_with("Connected · MongoDB 8.0") && !reached.contains('\n'),
            "{reached}"
        );
        // On production the root user is read-only only in Kuery, which says so.
        let production = ConnectionSpec {
            environment: Some("production".into()),
            ..spec.clone()
        };
        let reached = test(&production, "kuery").unwrap();
        assert!(!reached.contains("this user could only read"), "{reached}");
        assert!(
            reached.contains("\nMongoDB has no read-only session"),
            "{reached}"
        );
        // A user with only the read role: the server keeps it from writing.
        let reader = ConnectionSpec {
            user: "kuery_ro".into(),
            options: [("authSource".to_string(), "shop".to_string())].into(),
            ..production
        };
        let reached = test(&reader, "kuery").unwrap();
        assert!(
            reached.contains(" · at connect time, this user could only read"),
            "{reached}"
        );
        assert!(
            test(&spec, "wrong")
                .unwrap_err()
                .contains("Authentication failed")
        );
    }

    #[test]
    fn drivers_connect_with_the_options_and_no_empty_password() {
        let spec = ConnectionSpec {
            id: "m".into(),
            name: "m".into(),
            driver: "mongodb".into(),
            host: "127.0.0.1".into(),
            port: 27019,
            database: "shop".into(),
            user: "kuery".into(),
            sslmode: None,
            environment: None,
            ssh: None,
            options: [("authSource".to_string(), "admin".to_string())].into(),
        };
        let options = connect_options(&spec, Some(String::new()), None);
        assert_eq!(
            (options.password, options.tunnel, options.proxy),
            (None, None, None)
        );
        assert_eq!(options.options["authSource"], "admin");
    }
}
