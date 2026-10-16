//! The contract every driver meets in the app, and the registry that pairs
//! each descriptor in [`kuery_core::driver`] with its implementation.
//!
//! A driver tests and opens connections from a [`ConnectionSpec`]. What it
//! opens is a [`Connection`] of its family, and the family decides the rest:
//! the sidebar tree, the tabs and the write rules (SQL: schemas, query and
//! table tabs; key-value: databases and keys, key tabs and a console).
//!
//! Adding a driver: describe it in `kuery_core::driver`, implement
//! [`Driver`] here (usually over a crate of its own), and list it in
//! [`all`]. A driver of an existing family only needs a session that the
//! family's views can use.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use gpui_kit::component::{Icon, IconName};
use kuery_core::driver::{self, DriverInfo, Family};

use crate::assets::AppIcon;
use crate::connection::{Db, RedisDb};
use crate::project::ConnectionSpec;
use crate::sql_session::{Reconnect, SqlSession};
use crate::tunnel;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// The background runtimes the drivers run on.
#[derive(Clone)]
pub struct Engines {
    pub postgres: Arc<kuery_pg::Engine>,
    pub redis: Arc<kuery_redis::Engine>,
    pub mysql: Arc<kuery_mysql::Engine>,
}

/// An open connection, by family.
pub enum Connection {
    Sql(Db),
    KeyValue(RedisDb),
}

pub trait Driver: Send + Sync + 'static {
    fn info(&self) -> &'static DriverInfo;

    /// Connects read-only with the form's settings and says what it
    /// reached, such as "PostgreSQL 16.4".
    fn test(
        &self,
        spec: &ConnectionSpec,
        password: Option<String>,
        engines: &Engines,
    ) -> BoxFuture<Result<String, String>>;

    /// Opens the connection the sidebar keeps and the tabs share.
    fn connect(
        &self,
        spec: &ConnectionSpec,
        password: Option<String>,
        engines: &Engines,
    ) -> BoxFuture<Result<Connection, String>>;
}

/// Every driver, in the order of [`kuery_core::driver::ALL`].
pub fn all() -> &'static [&'static dyn Driver] {
    &[&Postgres, &Mysql, &Sqlite, &Redis]
}

/// The icon of a driver's family, in the sidebar and the driver list.
pub fn family_icon(family: Family) -> Icon {
    match family {
        Family::Sql => Icon::from(AppIcon::Database),
        Family::KeyValue => Icon::from(IconName::MemoryStick),
    }
}

pub fn find(id: &str) -> Option<&'static dyn Driver> {
    all().iter().copied().find(|driver| driver.info().id == id)
}

/// The driver a connection names, or why there is none.
pub fn for_spec(spec: &ConnectionSpec) -> Result<&'static dyn Driver, String> {
    find(&spec.driver).ok_or_else(|| {
        format!(
            "This version of Kuery has no \u{201c}{}\u{201d} driver",
            spec.driver
        )
    })
}

struct Postgres;

impl Driver for Postgres {
    fn info(&self) -> &'static DriverInfo {
        &driver::POSTGRES
    }

    fn test(
        &self,
        spec: &ConnectionSpec,
        password: Option<String>,
        engines: &Engines,
    ) -> BoxFuture<Result<String, String>> {
        let (spec, engine) = (spec.clone(), engines.postgres.clone());
        Box::pin(async move {
            let tunnel = tunnel::open_for(&spec).await?;
            let conninfo =
                spec.conninfo(password.as_deref(), tunnel.as_ref().map(|t| t.local_port()));
            engine
                .connect_with(conninfo, kuery_pg::Access::ReadOnly)
                .await
                .map(|session| {
                    format!(
                        "Connected{} · PostgreSQL {}",
                        through(&spec),
                        session.server_version()
                    )
                })
                .map_err(|error| error.to_string())
        })
    }

    fn connect(
        &self,
        spec: &ConnectionSpec,
        password: Option<String>,
        engines: &Engines,
    ) -> BoxFuture<Result<Connection, String>> {
        let (spec, engine) = (spec.clone(), engines.postgres.clone());
        Box::pin(async move {
            let tunnel = tunnel::open_for(&spec).await?;
            let conninfo =
                spec.conninfo(password.as_deref(), tunnel.as_ref().map(|t| t.local_port()));
            let session = engine
                .connect_with(conninfo.clone(), spec.environment().access())
                .await
                .map_err(|error| error.to_string())?;
            Ok(Connection::Sql(Db::new(
                spec.name.clone().into(),
                spec.environment(),
                SqlSession::Postgres(session),
                spec.target().into(),
                Reconnect::Postgres {
                    conninfo: conninfo.into(),
                },
                tunnel,
            )))
        })
    }
}

/// MySQL and MariaDB.
struct Mysql;

impl Driver for Mysql {
    fn info(&self) -> &'static DriverInfo {
        &driver::MYSQL
    }

    fn test(
        &self,
        spec: &ConnectionSpec,
        password: Option<String>,
        engines: &Engines,
    ) -> BoxFuture<Result<String, String>> {
        let (spec, engine) = (spec.clone(), engines.mysql.clone());
        Box::pin(async move {
            let tunnel = tunnel::open_for(&spec).await?;
            let target = spec.mysql_target(password, tunnel.as_ref().map(|t| t.local_port()));
            let session = engine.connect(target, kuery_pg::Access::ReadOnly).await?;
            Ok(format!(
                "Connected{} · {} {}",
                through(&spec),
                if session.is_mariadb() {
                    "MariaDB"
                } else {
                    "MySQL"
                },
                session.server_version()
            ))
        })
    }

    fn connect(
        &self,
        spec: &ConnectionSpec,
        password: Option<String>,
        engines: &Engines,
    ) -> BoxFuture<Result<Connection, String>> {
        let (spec, engine) = (spec.clone(), engines.mysql.clone());
        Box::pin(async move {
            let tunnel = tunnel::open_for(&spec).await?;
            let target = spec.mysql_target(password, tunnel.as_ref().map(|t| t.local_port()));
            let session = engine
                .connect(target.clone(), spec.environment().access())
                .await?;
            Ok(Connection::Sql(Db::new(
                spec.name.clone().into(),
                spec.environment(),
                SqlSession::Mysql(session),
                spec.target().into(),
                Reconnect::Mysql {
                    target: Arc::new(target),
                },
                tunnel,
            )))
        })
    }
}

/// A database file. `database` is its path, already made absolute (see
/// [`ConnectionSpec::resolved`]); production opens it read-only.
struct Sqlite;

impl Driver for Sqlite {
    fn info(&self) -> &'static DriverInfo {
        &driver::SQLITE
    }

    fn test(
        &self,
        spec: &ConnectionSpec,
        _: Option<String>,
        _: &Engines,
    ) -> BoxFuture<Result<String, String>> {
        let open =
            kuery_sqlite::Session::open(spec.database.clone().into(), kuery_pg::Access::ReadOnly);
        Box::pin(async move {
            let session = open.await?;
            Ok(format!("Opened · SQLite {}", session.server_version()))
        })
    }

    fn connect(
        &self,
        spec: &ConnectionSpec,
        _: Option<String>,
        _: &Engines,
    ) -> BoxFuture<Result<Connection, String>> {
        let path: std::path::PathBuf = spec.database.clone().into();
        let open = kuery_sqlite::Session::open(path.clone(), spec.environment().access());
        let spec = spec.clone();
        Box::pin(async move {
            let session = open.await?;
            Ok(Connection::Sql(Db::new(
                spec.name.clone().into(),
                spec.environment(),
                SqlSession::Sqlite(session),
                spec.target().into(),
                Reconnect::Sqlite { path },
                None,
            )))
        })
    }
}

struct Redis;

impl Driver for Redis {
    fn info(&self) -> &'static DriverInfo {
        &driver::REDIS
    }

    fn test(
        &self,
        spec: &ConnectionSpec,
        password: Option<String>,
        engines: &Engines,
    ) -> BoxFuture<Result<String, String>> {
        let (spec, engine) = (spec.clone(), engines.redis.clone());
        Box::pin(async move {
            let tunnel = tunnel::open_for(&spec).await?;
            let target =
                spec.redis_target(password.as_deref(), tunnel.as_ref().map(|t| t.local_port()));
            engine
                .connect(target)
                .await
                .map(|session| {
                    let server = session.server();
                    format!(
                        "Connected{} · Redis {} · {}",
                        through(&spec),
                        server.version,
                        server.mode
                    )
                })
                .map_err(|error| error.to_string())
        })
    }

    fn connect(
        &self,
        spec: &ConnectionSpec,
        password: Option<String>,
        engines: &Engines,
    ) -> BoxFuture<Result<Connection, String>> {
        let (spec, engine) = (spec.clone(), engines.redis.clone());
        Box::pin(async move {
            let tunnel = tunnel::open_for(&spec).await?;
            let target =
                spec.redis_target(password.as_deref(), tunnel.as_ref().map(|t| t.local_port()));
            let session = engine
                .connect(target)
                .await
                .map_err(|error| error.to_string())?;
            Ok(Connection::KeyValue(RedisDb::new(
                spec.name.clone().into(),
                spec.environment(),
                session,
                spec.target().into(),
                tunnel,
            )))
        })
    }
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

    #[test]
    fn every_descriptor_has_a_driver_and_back() {
        let ids: Vec<&str> = all().iter().map(|d| d.info().id).collect();
        let described: Vec<&str> = driver::ALL.iter().map(|d| d.id).collect();
        assert_eq!(ids, described);
        assert!(find("redis").is_some());
        assert!(find("oracle").is_none());
    }

    /// Needs the dev Postgres, Redis and bastion
    /// (`docker compose -f dev/compose.yaml up -d`).
    #[test]
    #[ignore]
    fn connects_through_the_dev_bastion() {
        use crate::project::SshSpec;
        let engines = Engines {
            postgres: Arc::new(kuery_pg::Engine::new().unwrap()),
            redis: Arc::new(kuery_redis::Engine::new().unwrap()),
            mysql: Arc::new(kuery_mysql::Engine::new().unwrap()),
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
        };
        let reached =
            futures::executor::block_on(Postgres.test(&postgres, None, &engines)).unwrap();
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
            ..postgres
        };
        let reached = futures::executor::block_on(Redis.test(&redis, None, &engines)).unwrap();
        assert!(
            reached.starts_with("Connected through localhost · Redis 7"),
            "{reached}"
        );
        let mysql = ConnectionSpec {
            driver: "mysql".into(),
            host: "mysql".into(),
            port: 3306,
            database: "shop".into(),
            user: "kuery".into(),
            ..redis
        };
        let reached = futures::executor::block_on(Mysql.test(&mysql, None, &engines)).unwrap();
        assert!(
            reached.starts_with("Connected through localhost · MySQL 8"),
            "{reached}"
        );
    }
}
