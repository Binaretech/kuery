//! Driver de PostgreSQL.
//!
//! Corre en su propio runtime de tokio, separado del hilo de la interfaz. Las
//! filas llegan en streaming y se entregan por lotes columnares a través de un
//! canal acotado: si la interfaz no consume, el driver deja de leer del socket.
//!
//! Toda query pasa por dos capas de solo lectura:
//! 1. [`dbadmin_core::read_only::check`] rechaza el SQL que podría escribir.
//! 2. La query se ejecuta dentro de `BEGIN TRANSACTION READ ONLY`, de modo que
//!    la propia base de datos la bloquea si algo se escapa del análisis.
//!
//! El TLS respeta el `sslmode` de la URL (ver [`tls`]).

mod tls;

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dbadmin_core::read_only::{self, Rejection};
use dbadmin_core::result_set::{Batch, BatchBuilder};
use futures::{TryStreamExt as _, pin_mut};
use tokio::runtime::Runtime;
use tokio::sync::Mutex;
use tokio_postgres::{CancelToken, Client, SimpleQueryMessage};

use crate::tls::{Connector, Target};

/// Runtime de fondo donde viven las conexiones.
pub struct Engine {
    runtime: Runtime,
}

impl Engine {
    pub fn new() -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("dbadmin-pg")
            .enable_all()
            .build()?;
        Ok(Self { runtime })
    }

    /// Conecta en el runtime de fondo. El futuro devuelto se puede esperar
    /// desde cualquier executor, también el de la interfaz.
    pub fn connect(
        &self,
        url: String,
    ) -> impl Future<Output = Result<Session, ConnectError>> + Send + use<> {
        let handle = self.runtime.handle().clone();
        let task = self.runtime.spawn(async move {
            let target = Target::parse(&url)?;
            let connector = target.connector()?;
            let client = connector.connect(&target.config, &handle).await?;
            let server_version = client
                .simple_query("show server_version")
                .await?
                .into_iter()
                .find_map(|message| match message {
                    SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
                    _ => None,
                })
                .unwrap_or_default();
            Ok(Session {
                inner: Arc::new(Inner {
                    cancel: client.cancel_token(),
                    connector,
                    client,
                    busy: Mutex::new(()),
                    handle,
                    server_version,
                }),
            })
        });
        async move {
            match task.await {
                Ok(result) => result,
                Err(join_error) => Err(ConnectError::Runtime(join_error.to_string())),
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("{}", with_causes(.0))]
    Postgres(#[from] tokio_postgres::Error),
    #[error("URL de conexión no válida: {0}")]
    Config(String),
    #[error("TLS: {0}")]
    Tls(String),
    #[error("el runtime de la conexión falló: {0}")]
    Runtime(String),
}

/// "error performing TLS handshake" no dice nada sin la causa de rustls.
fn with_causes(error: &tokio_postgres::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text = format!("{text}: {cause_text}");
        }
        source = cause.source();
    }
    text
}

/// Conexión abierta. Clonarla es barato; ejecuta una query cada vez.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    cancel: CancelToken,
    connector: Connector,
    busy: Mutex<()>,
    handle: tokio::runtime::Handle,
    server_version: String,
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Tiempo máximo de ejecución en el servidor (`statement_timeout`).
    pub statement_timeout: Duration,
    /// Filas del primer lote: pequeño para que aparezca algo enseguida.
    pub first_batch_rows: usize,
    /// Tamaño máximo de lote; los lotes crecen hasta aquí.
    pub max_batch_rows: usize,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            statement_timeout: Duration::from_secs(30),
            first_batch_rows: 500,
            max_batch_rows: 50_000,
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Columns(Vec<String>),
    Batch(Batch),
    Finished(Summary),
    Failed(QueryError),
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub rows: usize,
    pub elapsed: Duration,
    /// Tiempo hasta que llegó el primer lote.
    pub first_batch_after: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("{0}")]
    Rejected(Rejection),
    #[error("{message}")]
    Database { code: String, message: String },
    #[error("Query cancelada")]
    Cancelled,
    #[error("Conexión perdida: {0}")]
    Connection(String),
}

impl From<tokio_postgres::Error> for QueryError {
    fn from(error: tokio_postgres::Error) -> Self {
        match error.as_db_error() {
            Some(db) => QueryError::Database {
                code: db.code().code().to_string(),
                message: match db.hint() {
                    Some(hint) => format!("{} ({hint})", db.message()),
                    None => db.message().to_string(),
                },
            },
            None => QueryError::Connection(error.to_string()),
        }
    }
}

impl Session {
    pub fn server_version(&self) -> &str {
        &self.inner.server_version
    }

    /// Ejecuta `sql` en solo lectura y devuelve los eventos por un canal.
    ///
    /// Si se suelta el receptor, la query se cancela en el servidor.
    pub fn run_read_only(
        &self,
        sql: String,
        options: RunOptions,
    ) -> async_channel::Receiver<Event> {
        let (tx, rx) = async_channel::bounded(4);
        if let Err(rejection) = read_only::check(&sql) {
            let _ = tx.try_send(Event::Failed(QueryError::Rejected(rejection)));
            return rx;
        }
        let inner = self.inner.clone();
        self.inner.handle.spawn(async move {
            let event = match stream_in_read_only_transaction(&inner, &sql, &options, &tx).await {
                Ok(summary) => Event::Finished(summary),
                Err(error) => Event::Failed(error),
            };
            let _ = tx.send(event).await;
        });
        rx
    }

    /// Pide al servidor que cancele la query en curso.
    pub fn cancel(&self) {
        let cancel = self.inner.connector.cancel(self.inner.cancel.clone());
        self.inner.handle.spawn(async move {
            let _ = cancel.await;
        });
    }
}

/// Segunda capa: ejecuta `sql` tal cual dentro de una transacción `READ ONLY`.
///
/// No vuelve a analizar el SQL; por eso es privada y solo la usa
/// [`Session::run_read_only`] después de [`read_only::check`] (y los tests,
/// para comprobar que esta capa resiste sola).
async fn stream_in_read_only_transaction(
    inner: &Inner,
    sql: &str,
    options: &RunOptions,
    tx: &async_channel::Sender<Event>,
) -> Result<Summary, QueryError> {
    let _busy = inner.busy.lock().await;
    let client = &inner.client;
    let started = Instant::now();

    // `select 1` toma el primer snapshot: a partir de ahí PostgreSQL no deja
    // cambiar la transacción a lectura y escritura.
    client
        .batch_execute(&format!(
            "begin transaction read only; set local statement_timeout = {}; select 1",
            options.statement_timeout.as_millis()
        ))
        .await?;

    let outcome = stream_rows(client, sql, options, tx, started).await;
    if matches!(outcome, Err(QueryError::Cancelled)) {
        let _ = inner.connector.cancel(inner.cancel.clone()).await;
    }
    // Nunca hay nada que confirmar: siempre se termina con rollback.
    let _ = client.batch_execute("rollback").await;
    outcome
}

async fn stream_rows(
    client: &Client,
    sql: &str,
    options: &RunOptions,
    tx: &async_channel::Sender<Event>,
    started: Instant,
) -> Result<Summary, QueryError> {
    let stream = client.simple_query_raw(sql).await?;
    pin_mut!(stream);

    let mut builder: Option<BatchBuilder> = None;
    let mut batch_rows = options.first_batch_rows.max(1);
    let mut rows = 0;
    let mut first_batch_after = None;

    while let Some(message) = stream.try_next().await? {
        match message {
            SimpleQueryMessage::RowDescription(columns) => {
                let names = columns
                    .iter()
                    .map(|c| c.name().to_string())
                    .collect::<Vec<_>>();
                builder = Some(BatchBuilder::new(names.len(), batch_rows));
                send(tx, Event::Columns(names)).await?;
            }
            SimpleQueryMessage::Row(row) => {
                let Some(builder) = builder.as_mut() else {
                    continue;
                };
                builder.push_row(|index| row.get(index));
                rows += 1;
                if builder.rows() >= batch_rows {
                    first_batch_after.get_or_insert_with(|| started.elapsed());
                    send(tx, Event::Batch(builder.finish())).await?;
                    batch_rows = (batch_rows * 4).min(options.max_batch_rows);
                }
            }
            _ => {}
        }
    }
    if let Some(builder) = builder.as_mut().filter(|b| b.rows() > 0) {
        first_batch_after.get_or_insert_with(|| started.elapsed());
        send(tx, Event::Batch(builder.finish())).await?;
    }

    Ok(Summary {
        rows,
        elapsed: started.elapsed(),
        first_batch_after,
    })
}

async fn send(tx: &async_channel::Sender<Event>, event: Event) -> Result<(), QueryError> {
    tx.send(event).await.map_err(|_| QueryError::Cancelled)
}

#[cfg(test)]
mod tests {
    //! Requieren el Postgres de `dev/compose.yaml`:
    //! `cargo test -p dbadmin-pg -- --ignored`
    //!
    //! Los de TLS necesitan además un Postgres con `ssl = on` en
    //! `DBADMIN_TLS_DATABASE_URL` (sin `sslmode`); si no está, no hacen nada.

    use super::*;

    fn database_url() -> String {
        std::env::var("DBADMIN_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://dbadmin:dbadmin@localhost:54329/dbadmin".into())
    }

    async fn session(engine: &Engine) -> Session {
        engine
            .connect(database_url())
            .await
            .expect("conecta al Postgres de desarrollo")
    }

    async fn collect(rx: async_channel::Receiver<Event>) -> (Vec<String>, usize, usize, Event) {
        let (mut columns, mut rows, mut batches) = (Vec::new(), 0, 0);
        loop {
            match rx
                .recv()
                .await
                .expect("el driver siempre termina con un evento final")
            {
                Event::Columns(names) => columns = names,
                Event::Batch(batch) => {
                    rows += batch.rows();
                    batches += 1;
                }
                last => return (columns, rows, batches, last),
            }
        }
    }

    /// Ejecuta sin el análisis previo, para probar la segunda capa sola.
    async fn run_unchecked(session: &Session, sql: &str) -> Result<Summary, QueryError> {
        let (tx, _rx) = async_channel::unbounded();
        stream_in_read_only_transaction(&session.inner, sql, &RunOptions::default(), &tx).await
    }

    async fn order_count(session: &Session) -> usize {
        scalar(session, "select count(*) from orders")
            .await
            .parse()
            .unwrap()
    }

    async fn scalar(session: &Session, sql: &str) -> String {
        let rx = session.run_read_only(sql.into(), RunOptions::default());
        let mut set = None;
        while let Ok(event) = rx.recv().await {
            match event {
                Event::Columns(names) => {
                    set = Some(dbadmin_core::result_set::ResultSet::new(names))
                }
                Event::Batch(batch) => set.as_mut().unwrap().push(batch),
                _ => {}
            }
        }
        set.unwrap().cell(0, 0).unwrap().unwrap().to_owned()
    }

    fn tls_database_url(sslmode: &str) -> Option<String> {
        match std::env::var("DBADMIN_TLS_DATABASE_URL") {
            Ok(url) => {
                let separator = if url.contains('?') { '&' } else { '?' };
                Some(format!("{url}{separator}sslmode={sslmode}"))
            }
            Err(_) => {
                eprintln!("DBADMIN_TLS_DATABASE_URL no está definida: se omite el test");
                None
            }
        }
    }

    const USES_TLS: &str = "select ssl from pg_stat_ssl where pid = pg_backend_pid()";

    #[test]
    #[ignore = "necesita un Postgres con TLS"]
    fn sslmode_require_encrypts_the_connection() {
        let Some(url) = tls_database_url("require") else {
            return;
        };
        let engine = Engine::new().unwrap();
        engine.runtime.block_on(async {
            let session = engine.connect(url).await.expect("conecta con TLS");
            assert_eq!(scalar(&session, USES_TLS).await, "t");
        });
    }

    #[test]
    #[ignore = "necesita un Postgres con TLS"]
    fn sslmode_disable_skips_tls() {
        let Some(url) = tls_database_url("disable") else {
            return;
        };
        let engine = Engine::new().unwrap();
        engine.runtime.block_on(async {
            let session = engine.connect(url).await.expect("conecta sin TLS");
            assert_eq!(scalar(&session, USES_TLS).await, "f");
        });
    }

    #[test]
    #[ignore = "necesita un Postgres con TLS"]
    fn verify_full_rejects_an_untrusted_certificate() {
        // El certificado del servidor de pruebas es autofirmado.
        let Some(url) = tls_database_url("verify-full") else {
            return;
        };
        let engine = Engine::new().unwrap();
        let result = engine.runtime.block_on(engine.connect(url));
        let Err(error) = result else {
            panic!("no debería aceptar un certificado autofirmado");
        };
        assert!(error.to_string().contains("certificate"), "{error}");
    }

    #[test]
    #[ignore = "necesita un Postgres con TLS"]
    fn cancels_over_tls() {
        let Some(url) = tls_database_url("require") else {
            return;
        };
        let engine = Engine::new().unwrap();
        engine.runtime.block_on(async {
            let session = engine.connect(url).await.expect("conecta con TLS");
            let rx = session.run_read_only("select pg_sleep(10)".into(), RunOptions::default());
            let started = Instant::now();
            tokio::time::sleep(Duration::from_millis(200)).await;
            session.cancel();
            let (_, _, _, last) = collect(rx).await;
            assert!(started.elapsed() < Duration::from_secs(5));
            assert!(
                matches!(&last, Event::Failed(QueryError::Database { code, .. }) if code == "57014"),
                "{last:?}"
            );
        });
    }

    #[test]
    #[ignore = "necesita el Postgres de desarrollo"]
    fn streams_a_million_rows_in_growing_batches() {
        let engine = Engine::new().unwrap();
        engine.runtime.block_on(async {
            let session = session(&engine).await;
            let rx = session.run_read_only("select * from orders".into(), RunOptions::default());
            let (columns, rows, batches, last) = collect(rx).await;
            assert_eq!(columns.first().map(String::as_str), Some("id"));
            assert_eq!(rows, 1_000_000);
            assert!(batches > 10, "{batches} lotes");
            let Event::Finished(summary) = last else {
                panic!("{last:?}")
            };
            assert_eq!(summary.rows, 1_000_000);
        });
    }

    #[test]
    #[ignore = "necesita el Postgres de desarrollo"]
    fn rejects_writes_before_reaching_the_server() {
        let engine = Engine::new().unwrap();
        engine.runtime.block_on(async {
            let session = session(&engine).await;
            let rx = session.run_read_only("delete from orders".into(), RunOptions::default());
            let (_, _, _, last) = collect(rx).await;
            assert!(matches!(
                last,
                Event::Failed(QueryError::Rejected(Rejection::Statement(_)))
            ));
        });
    }

    #[test]
    #[ignore = "necesita el Postgres de desarrollo"]
    fn the_transaction_blocks_writes_even_without_the_parser() {
        let engine = Engine::new().unwrap();
        engine.runtime.block_on(async {
            let session = session(&engine).await;
            let before = order_count(&session).await;

            let error = run_unchecked(&session, "delete from orders where id <= 10")
                .await
                .unwrap_err();
            assert!(
                matches!(&error, QueryError::Database { code, .. } if code == "25006"),
                "{error:?}"
            );

            // Tampoco se puede volver a lectura y escritura desde dentro.
            let error = run_unchecked(
                &session,
                "select set_config('transaction_read_only', 'off', true)",
            )
            .await
            .unwrap_err();
            assert!(matches!(error, QueryError::Database { .. }), "{error:?}");

            assert_eq!(order_count(&session).await, before);
        });
    }

    #[test]
    #[ignore = "necesita el Postgres de desarrollo"]
    fn statement_timeout_stops_long_queries() {
        let engine = Engine::new().unwrap();
        engine.runtime.block_on(async {
            let session = session(&engine).await;
            let options = RunOptions {
                statement_timeout: Duration::from_millis(200),
                ..RunOptions::default()
            };
            let rx = session.run_read_only("select pg_sleep(5)".into(), options);
            let started = Instant::now();
            let (_, _, _, last) = collect(rx).await;
            assert!(started.elapsed() < Duration::from_secs(2));
            assert!(
                matches!(&last, Event::Failed(QueryError::Database { code, .. }) if code == "57014"),
                "{last:?}"
            );
        });
    }

    #[test]
    #[ignore = "necesita el Postgres de desarrollo"]
    fn dropping_the_receiver_cancels_the_query() {
        let engine = Engine::new().unwrap();
        engine.runtime.block_on(async {
            let session = session(&engine).await;
            let rx = session.run_read_only("select * from orders".into(), RunOptions::default());
            // Lee un lote y abandona.
            loop {
                if let Event::Batch(_) = rx.recv().await.unwrap() {
                    break;
                }
            }
            drop(rx);
            // La sesión queda libre para la siguiente query.
            let started = Instant::now();
            assert_eq!(order_count(&session).await, 1_000_000);
            assert!(started.elapsed() < Duration::from_secs(5));
        });
    }
}
