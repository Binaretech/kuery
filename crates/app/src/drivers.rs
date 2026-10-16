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
use crate::connection::{Db, Reconnect, RedisDb};
use crate::project::ConnectionSpec;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// The background runtimes the drivers run on.
#[derive(Clone)]
pub struct Engines {
    pub postgres: Arc<kuery_pg::Engine>,
    pub redis: Arc<kuery_redis::Engine>,
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
    &[&Postgres, &Redis]
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
        let connect = engines.postgres.connect_with(
            spec.conninfo(password.as_deref()),
            kuery_pg::Access::ReadOnly,
        );
        Box::pin(async move {
            connect
                .await
                .map(|session| format!("Connected · PostgreSQL {}", session.server_version()))
                .map_err(|error| error.to_string())
        })
    }

    fn connect(
        &self,
        spec: &ConnectionSpec,
        password: Option<String>,
        engines: &Engines,
    ) -> BoxFuture<Result<Connection, String>> {
        let conninfo = spec.conninfo(password.as_deref());
        let connect = engines
            .postgres
            .connect_with(conninfo.clone(), spec.environment().access());
        let spec = spec.clone();
        Box::pin(async move {
            let session = connect.await.map_err(|error| error.to_string())?;
            Ok(Connection::Sql(Db::new(
                spec.name.clone().into(),
                spec.environment(),
                session,
                spec.target().into(),
                Reconnect::new(conninfo),
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
        let connect = engines
            .redis
            .connect(spec.redis_target(password.as_deref()));
        Box::pin(async move {
            connect
                .await
                .map(|session| {
                    let server = session.server();
                    format!("Connected · Redis {} · {}", server.version, server.mode)
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
        let connect = engines
            .redis
            .connect(spec.redis_target(password.as_deref()));
        let spec = spec.clone();
        Box::pin(async move {
            let session = connect.await.map_err(|error| error.to_string())?;
            Ok(Connection::KeyValue(RedisDb::new(
                spec.name.clone().into(),
                spec.environment(),
                session,
                spec.target().into(),
            )))
        })
    }
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
}
