//! Need the dev MongoDB (`docker compose -f dev/compose.yaml up -d mongo`),
//! or one at `KUERY_MONGO_URI` with the sample `shop` and its read-only user
//! `kuery_ro`. The standalone checks also need `mongo-standalone` (`docker
//! compose --profile standalone up -d mongo-standalone`) and pass without
//! checking anything when it is not running, unless
//! `KUERY_MONGO_STANDALONE=required` (as in CI) makes them fail instead.

use kuery_core::browse::Direction;
use kuery_core::document::capabilities::{Enforcement, Preview, Transactions};
use kuery_core::document::language::Style;
use kuery_core::document::session::{
    DocEvent, DocSummary, DocumentSession, Estimate, Heavy, Impact, Left, RunMode,
};
use kuery_core::document::value::{FieldPath, Value};
use kuery_core::driver::ConnectOptions;

use super::*;

const DEV: &str =
    "mongodb://kuery:kuery@127.0.0.1:27019/shop?authSource=admin&directConnection=true";

fn target() -> Target {
    let uri = std::env::var("KUERY_MONGO_URI").unwrap_or_else(|_| DEV.into());
    let parsed = uri::parse(&uri).expect("KUERY_MONGO_URI is a MongoDB connection string");
    Target::from_options(&ConnectOptions {
        host: parsed.host,
        port: parsed.port.unwrap_or(27017),
        database: parsed.database,
        user: parsed.user,
        password: parsed.password,
        tls: parsed.tls,
        tunnel: None,
        proxy: None,
        options: parsed.options,
    })
    .expect("KUERY_MONGO_URI has only options Kuery keeps")
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn connects_and_names_the_server() {
    let adapter = open(&target(), Access::ReadOnly).expect("connects to the dev MongoDB");
    let server = adapter.server();
    assert!(server.starts_with("MongoDB 8.0"), "{server}");
    assert!(server.contains("replica set rs0"), "{server}");
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_wrong_password_says_so() {
    let wrong = Target {
        password: Some("not the password".into()),
        ..target()
    };
    let error = open(&wrong, Access::ReadOnly).err().expect("refused");
    assert!(error.contains("Authentication failed"), "{error}");
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_server_that_does_not_answer_says_so_in_one_line() {
    let nowhere = Target {
        hosts: vec![("127.0.0.1".into(), 1)],
        ..target()
    };
    let error = open(&nowhere, Access::ReadOnly).err().expect("refused");
    assert_eq!(error, "127.0.0.1:1 did not answer in 10 seconds");
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn two_adapters_with_one_target_share_a_client() {
    // A target no other test opens, so its client is this test's alone.
    let only_here = Target {
        database: "kuery_pools".into(),
        ..target()
    };
    let first = connect(&only_here, Access::ReadOnly).unwrap();
    let second = connect(&only_here, Access::ReadWrite).unwrap();
    assert!(Arc::ptr_eq(&first.pool, &second.pool));
    let other = connect(&target(), Access::ReadOnly).unwrap();
    assert!(!Arc::ptr_eq(&first.pool, &other.pool));
    // The client goes away with its last adapter.
    let pool = Arc::downgrade(&*first.pool);
    drop((first, second));
    assert!(pool.upgrade().is_none());
}

/// A SOCKS5 proxy on a local port, as `ssh -D` opens one: no
/// authentication, CONNECT only, names resolved by the proxy. Returns its
/// port and the addresses it was asked for.
fn socks_proxy() -> (u16, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};

    async fn serve(mut client: TcpStream, asked: Arc<Mutex<Vec<String>>>) -> std::io::Result<()> {
        let mut greeting = [0u8; 2];
        client.read_exact(&mut greeting).await?;
        let mut methods = vec![0u8; usize::from(greeting[1])];
        client.read_exact(&mut methods).await?;
        client.write_all(&[5, 0]).await?;
        let mut request = [0u8; 4];
        client.read_exact(&mut request).await?;
        let host = match request[3] {
            1 => {
                let mut ip = [0u8; 4];
                client.read_exact(&mut ip).await?;
                std::net::Ipv4Addr::from(ip).to_string()
            }
            3 => {
                let mut name = vec![0u8; usize::from(client.read_u8().await?)];
                client.read_exact(&mut name).await?;
                String::from_utf8_lossy(&name).into_owned()
            }
            _ => return Ok(()),
        };
        let port = client.read_u16().await?;
        asked.lock().unwrap().push(format!("{host}:{port}"));
        let mut server = TcpStream::connect((host.as_str(), port)).await?;
        client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        tokio::io::copy_bidirectional(&mut client, &mut server).await?;
        Ok(())
    }

    let listener = runtime()
        .block_on(TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    runtime().spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            tokio::spawn(serve(client, log.clone()));
        }
    });
    (port, asked)
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn reaches_the_server_by_name_through_a_socks_proxy() {
    let (port, asked) = socks_proxy();
    let options = ConnectOptions {
        host: "localhost".into(),
        port: 27019,
        proxy: Some(port),
        ..ConnectOptions::default()
    };
    let through = Target {
        user: target().user,
        password: target().password,
        options: target().options,
        ..Target::from_options(&options).unwrap()
    };
    let adapter = open(&through, Access::ReadOnly).expect("connects through the proxy");
    assert!(adapter.server().starts_with("MongoDB 8.0"));
    // The proxy dials the name, as the SSH host would resolve it.
    let asked = asked.lock().unwrap();
    assert!(!asked.is_empty());
    assert!(
        asked.iter().all(|asked| asked == "localhost:27019"),
        "{asked:?}"
    );
}

/// `kuery_ro`, with only the `read` role on `shop`.
fn reader() -> Target {
    let mut target = Target {
        user: "kuery_ro".into(),
        password: Some("kuery".into()),
        ..target()
    };
    target.options.insert("authSource".into(), "shop".into());
    target
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_pasted_string_signs_in_to_the_database_it_names() {
    // As mongosh reads it: `kuery_ro` is a user of `shop`.
    let parsed =
        uri::parse("mongodb://kuery_ro:kuery@127.0.0.1:27019/shop?directConnection=true").unwrap();
    let pasted = Target::from_options(&ConnectOptions {
        host: parsed.host,
        port: parsed.port.unwrap_or(27017),
        database: parsed.database,
        user: parsed.user,
        password: parsed.password,
        tls: parsed.tls,
        tunnel: None,
        proxy: None,
        options: parsed.options,
    })
    .unwrap();
    let pasted = Target {
        hosts: target().hosts,
        ..pasted
    };
    let adapter = open(&pasted, Access::ReadOnly).expect("signs in as kuery_ro");
    assert_eq!(adapter.capabilities().read_only, Enforcement::Server);
}

/// The `mongo-standalone` service (`--profile standalone`): no replica
/// set, no authentication. `None`, with a message, when it is not running;
/// a failure instead when `KUERY_MONGO_STANDALONE=required`.
fn standalone() -> Option<Target> {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], 27029));
    if std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_err() {
        let required = std::env::var("KUERY_MONGO_STANDALONE").as_deref() == Ok("required");
        assert!(
            !required,
            "mongo-standalone does not answer on 127.0.0.1:27029, and KUERY_MONGO_STANDALONE=required"
        );
        eprintln!("skipped: start mongo-standalone with docker compose --profile standalone");
        return None;
    }
    Some(Target {
        hosts: vec![("127.0.0.1".into(), 27029)],
        srv: false,
        database: "shop".into(),
        user: String::new(),
        password: None,
        tls: TlsMode::Default,
        options: Default::default(),
        tunnelled: false,
        forwarded: None,
        proxy: None,
    })
}

/// Collects a cursor's documents.
fn collect(mongo: &mut Mongo, command: BsonDocument) -> Result<Vec<BsonDocument>, QueryError> {
    let mut documents = Vec::new();
    mongo.cursor("shop", command, &mut |batch| {
        documents.extend(batch);
        Ok(())
    })?;
    Ok(documents)
}

/// Operations still on the server with this adapter's tags.
fn left_behind(mongo: &mut Mongo) -> Vec<BsonDocument> {
    let mine = Tag::prefix(mongo.id);
    let mut found = Vec::new();
    mongo
        .cursor(
            "admin",
            doc! {
                "aggregate": 1,
                "pipeline": [
                    { "$currentOp": { "idleCursors": true } },
                    { "$match": { "$or": [
                        { "command.comment": { "$regex": format!("^{mine}") } },
                        { "cursor.originatingCommand.comment": { "$regex": format!("^{mine}") } },
                    ] } },
                ],
                "cursor": {},
            },
            &mut |batch| {
                found.extend(batch);
                Ok(())
            },
        )
        .unwrap();
    // The search itself.
    found.retain(|op| run::name(op.get_document("command").unwrap_or(&doc! {})) != "aggregate");
    found
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn detects_a_replica_set_with_transactions() {
    let adapter = open(&target(), Access::ReadWrite).unwrap();
    let capabilities = adapter.capabilities();
    assert!(matches!(
        capabilities.transactions,
        Transactions::Multi { .. }
    ));
    assert_eq!(capabilities.preview, Preview::Undone);
    assert!(capabilities.activity && capabilities.kill);
    assert_eq!(adapter.notice(), None);
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn the_read_user_is_read_only_on_the_server() {
    let adapter = open(&reader(), Access::ReadOnly).unwrap();
    assert_eq!(adapter.capabilities().read_only, Enforcement::Server);
    assert_eq!(adapter.notice(), None);
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn root_is_read_only_only_in_kuery() {
    let adapter = open(&target(), Access::ReadOnly).unwrap();
    assert!(matches!(
        adapter.capabilities().read_only,
        Enforcement::ClientOnly(_)
    ));
    let notice = adapter.notice().expect("says only Kuery refuses writes");
    assert!(
        notice.starts_with("MongoDB has no read-only session"),
        "{notice}"
    );
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn the_wire_guard_stops_writes_the_session_never_saw() {
    let mut mongo = connect(&target(), Access::ReadOnly).unwrap();
    let insert = doc! { "insert": "orders", "documents": [{ "kuery": "must never be written" }] };
    assert_eq!(
        mongo.command("shop", insert),
        Err(QueryError::WritesDisabled)
    );
    let out = doc! { "aggregate": "orders", "pipeline": [{ "$facet": { "a": [{ "$out": "copy" }] } }], "cursor": {} };
    assert_eq!(collect(&mut mongo, out), Err(QueryError::WritesDisabled));
    assert_eq!(mongo.begin(), Err(QueryError::WritesDisabled));
    let collections = mongo
        .command(
            "shop",
            doc! { "listCollections": 1, "filter": { "name": "copy" } },
        )
        .unwrap();
    assert!(run::first_batch(&collections).is_empty());
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn cancels_a_slow_find_and_keeps_working() {
    let mut mongo = connect(&target(), Access::ReadOnly).unwrap();
    let cancel = DocumentAdapter::canceller(&mongo);
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        cancel();
    });
    let started = std::time::Instant::now();
    let slow =
        doc! { "find": "big", "filter": { "$where": "sleep(100) || true" }, "batchSize": 10 };
    assert_eq!(collect(&mut mongo, slow), Err(QueryError::Cancelled));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    canceller.join().unwrap();
    assert_eq!(left_behind(&mut mongo), Vec::<BsonDocument>::new());
    mongo.command("admin", doc! { "ping": 1 }).unwrap();
    // A cancel that comes when nothing runs does not reach the next run.
    DocumentAdapter::canceller(&mongo)();
    let some = collect(&mut mongo, doc! { "find": "big", "limit": 3 }).unwrap();
    assert_eq!(some.len(), 3);
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_cancel_reaches_no_operation_of_another_kuery() {
    let mut mongo = connect(&target(), Access::ReadOnly).unwrap();
    // Another Kuery signed in as the same user numbers its adapters and
    // commands alike: only the process in its tags differs.
    let theirs = format!(
        "kuery:{}:{}:{}",
        bson::oid::ObjectId::new().to_hex(),
        mongo.id,
        mongo.commands + 1
    );
    let db = mongo.pool.client.database("shop");
    let other = runtime().spawn(async move {
        db.run_command(doc! {
            "find": "big",
            "filter": { "$where": "sleep(100) || false" },
            "maxTimeMS": 3000,
            "comment": theirs,
        })
        .await
    });
    let cancel = DocumentAdapter::canceller(&mongo);
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        cancel();
    });
    let slow = doc! { "find": "big", "filter": { "$where": "sleep(100) || false" } };
    assert_eq!(collect(&mut mongo, slow), Err(QueryError::Cancelled));
    canceller.join().unwrap();
    // Theirs ran on to its own time limit.
    let other = runtime().block_on(other).unwrap().map_err(run::error);
    assert!(
        matches!(&other, Err(QueryError::Database { code, .. }) if code == "50"),
        "{other:?}"
    );
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn cancels_an_operation_that_had_not_reached_the_server_yet() {
    let mut mongo = connect(&target(), Access::ReadOnly).unwrap();
    let slow = doc! { "find": "big", "filter": { "$where": "sleep(100) || false" } };
    let (command, tag) = mongo.prepare(slow).unwrap();
    let db = mongo.pool.client.database("shop");
    let cancel = DocumentAdapter::canceller(&mongo);
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let retries = std::thread::spawn({
        let done = done.clone();
        // As the session does: at once, then every quarter second.
        move || {
            std::thread::sleep(Duration::from_millis(50));
            while !done.load(Ordering::SeqCst) {
                cancel();
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    });
    let started = std::time::Instant::now();
    let stopped = run::wait(&mongo.pool.client, &tag, &mongo.stop, None, async move {
        // Still on its way: picking a server, waiting for a connection.
        tokio::time::sleep(Duration::from_millis(300)).await;
        db.run_command(command).await
    });
    done.store(true, Ordering::SeqCst);
    retries.join().unwrap();
    assert_eq!(stopped, Err(QueryError::Cancelled));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(left_behind(&mut mongo), Vec::<BsonDocument>::new());
}

/// A relay to `to` that can stop passing bytes on, as a link that went
/// quiet without closing (a dropped VPN, a hung tunnel). Returns its port
/// and the switch that quiets it.
fn relay(to: (String, u16)) -> (u16, Arc<std::sync::atomic::AtomicBool>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let quiet = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let switch = quiet.clone();
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let Ok(server) = std::net::TcpStream::connect((to.0.as_str(), to.1)) else {
                continue;
            };
            let pipes = [
                (client.try_clone().unwrap(), server.try_clone().unwrap()),
                (server, client),
            ];
            for (mut from, mut into) in pipes {
                let quiet = quiet.clone();
                std::thread::spawn(move || {
                    let mut buffer = [0; 16 * 1024];
                    while let Ok(read @ 1..) = from.read(&mut buffer) {
                        while quiet.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        if into.write_all(&buffer[..read]).is_err() {
                            break;
                        }
                    }
                });
            }
        }
    });
    (port, switch)
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn cancelling_takes_the_grace_at_most_when_the_server_stops_answering() {
    let (port, quiet) = relay(target().hosts[0].clone());
    let through = Target {
        hosts: vec![("127.0.0.1".into(), port)],
        ..target()
    };
    let mut mongo = connect(&through, Access::ReadOnly).unwrap();
    let cancel = DocumentAdapter::canceller(&mongo);
    let quieting = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(1));
        quiet.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(500));
        cancel();
    });
    let started = std::time::Instant::now();
    // The server ends it by itself once the test is over.
    let slow = doc! {
        "find": "big",
        "filter": { "$where": "sleep(100) || false" },
        "maxTimeMS": 20_000,
    };
    let stopped = mongo.command("shop", slow);
    let elapsed = started.elapsed();
    quieting.join().unwrap();
    assert!(
        matches!(&stopped, Err(QueryError::Unsupported(detached)) if detached == run::DETACHED),
        "{stopped:?}"
    );
    assert!(elapsed < Duration::from_secs(8), "{elapsed:?}");
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_reader_that_goes_away_closes_the_cursor() {
    let mut mongo = connect(&target(), Access::ReadOnly).unwrap();
    mongo
        .limit(&RunOptions {
            first_batch_rows: 10,
            max_batch_rows: 10,
            ..RunOptions::default()
        })
        .unwrap();
    let mut seen = 0;
    let stopped = mongo.cursor("shop", doc! { "find": "big" }, &mut |batch| {
        seen += batch.len();
        Err(QueryError::Cancelled)
    });
    assert_eq!(stopped, Err(QueryError::Cancelled));
    assert_eq!(seen, 10);
    assert_eq!(left_behind(&mut mongo), Vec::<BsonDocument>::new());
}

/// Takes documents up to `cap`, as the session's stream does with
/// `RunOptions::max_documents`.
struct Capped {
    documents: Vec<Document>,
    cap: usize,
}

impl DocumentSink for Capped {
    fn documents(&mut self, batch: Vec<Document>) -> Result<(), QueryError> {
        let room = self.cap.saturating_sub(self.documents.len());
        self.documents.extend(batch.into_iter().take(room));
        Ok(())
    }

    fn full(&self) -> bool {
        self.documents.len() >= self.cap
    }
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_full_sink_stops_the_read_and_closes_the_cursor() {
    let mut mongo = connect(&target(), Access::ReadOnly).unwrap();
    mongo
        .limit(&RunOptions {
            first_batch_rows: 100,
            max_batch_rows: 100,
            ..RunOptions::default()
        })
        .unwrap();
    let mut capped = Capped {
        documents: Vec::new(),
        cap: 250,
    };
    let outcome = mongo.execute("db.big.find()", &mut capped).unwrap();
    assert_eq!(capped.documents.len(), 250);
    // Read in groups of 100 until the sink was full, of `big`'s 20 000.
    assert!(
        (250..1_000).contains(&outcome.returned),
        "{}",
        outcome.returned
    );
    // The cursor closes as the read ends, on the server a moment later.
    let started = std::time::Instant::now();
    while !left_behind(&mut mongo).is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            left_behind(&mut mongo)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    console(&mut mongo, "db.runCommand({ping: 1})").unwrap();
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn times_out_with_max_time_ms() {
    let mut mongo = connect(&target(), Access::ReadOnly).unwrap();
    mongo
        .limit(&RunOptions {
            statement_timeout: Duration::from_secs(1),
            ..RunOptions::default()
        })
        .unwrap();
    let started = std::time::Instant::now();
    let slow = doc! { "find": "big", "filter": { "$where": "sleep(100) || true" } };
    let failed = collect(&mut mongo, slow).unwrap_err();
    assert!(
        matches!(&failed, QueryError::Database { code, message } if code == "50" && message == "Took longer than the time limit"),
        "{failed:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_server_without_auth_is_not_read_only_on_the_server() {
    let Some(standalone) = standalone() else {
        return;
    };
    let adapter = open(&standalone, Access::ReadOnly).unwrap();
    assert_eq!(
        adapter.capabilities().read_only,
        Enforcement::ClientOnly("this server has no authentication")
    );
    let notice = adapter.notice().unwrap();
    assert!(
        notice.contains("This server has no authentication"),
        "{notice}"
    );
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_read_user_on_a_server_without_auth_is_not_read_only_on_the_server() {
    let Some(standalone) = standalone() else {
        return;
    };
    // A server without authentication still signs users in and reports
    // their roles' privileges, and lets them write anyway.
    let mut admin = connect(&standalone, Access::ReadWrite).unwrap();
    let created = admin.command(
        "shop",
        doc! { "createUser": "kuery_ro", "pwd": "kuery", "roles": [{ "role": "read", "db": "shop" }] },
    );
    match created {
        Ok(_) => {}
        // Already there from an earlier run.
        Err(QueryError::Database { code, .. }) if code == "51003" => {}
        Err(error) => panic!("{error:?}"),
    }
    let mut reader = standalone.clone();
    reader.user = "kuery_ro".into();
    reader.password = Some("kuery".into());
    reader.options.insert("authSource".into(), "shop".into());
    let adapter = open(&reader, Access::ReadOnly).unwrap();
    assert_eq!(
        adapter.capabilities().read_only,
        Enforcement::ClientOnly("this server has no authentication")
    );
    let notice = adapter.notice().unwrap();
    assert!(
        notice.starts_with("MongoDB has no read-only session")
            && notice.contains("This server has no authentication"),
        "{notice}"
    );
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn standalone_has_no_transactions() {
    let Some(standalone) = standalone() else {
        return;
    };
    let adapter = open(&standalone, Access::ReadWrite).unwrap();
    assert!(
        adapter.server().ends_with("· standalone"),
        "{}",
        adapter.server()
    );
    let capabilities = adapter.capabilities();
    assert_eq!(capabilities.transactions, Transactions::None);
    assert_eq!(capabilities.preview, Preview::Estimated);
    assert!(adapter.notice().unwrap().contains("Standalone server"));
}

fn shop(collection: &str) -> CollectionRef {
    CollectionRef::new(&["shop", collection])
}

/// One page, as documents.
fn page(
    adapter: &mut dyn DocumentAdapter,
    collection: &str,
    spec: &BrowseSpec,
) -> (Vec<Document>, Option<Continuation>) {
    let mut documents = Vec::new();
    let next = adapter
        .browse(&shop(collection), spec, &mut documents)
        .unwrap();
    (documents, next)
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn lists_databases_and_collections_with_their_kinds() {
    use kuery_core::document::catalog::ContainerKind;
    let mut adapter = open(&reader(), Access::ReadOnly).unwrap();
    let databases = adapter.children(&[]).unwrap();
    assert!(
        databases.iter().any(|d| d.name == "shop" && !d.leaf),
        "{databases:?}"
    );
    let collections = adapter.children(&["shop".into()]).unwrap();
    let kind = |name: &str| {
        collections
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} is not listed: {collections:?}"))
            .kind
    };
    assert_eq!(kind("orders"), ContainerKind::Collection);
    assert_eq!(kind("paid_orders"), ContainerKind::View);
    assert_eq!(kind("readings"), ContainerKind::TimeSeries);
    assert_eq!(kind("events"), ContainerKind::Capped);
    assert!(
        collections
            .iter()
            .all(|c| c.leaf && !c.name.starts_with("system."))
    );
    assert!(
        adapter
            .children(&["shop".into(), "orders".into()])
            .unwrap()
            .is_empty()
    );
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn reads_indexes_and_the_validator() {
    use kuery_core::document::catalog::ContainerKind;
    let mut adapter = open(&reader(), Access::ReadOnly).unwrap();
    let customers = adapter.collection(&shop("customers")).unwrap();
    let email = customers
        .indexes
        .iter()
        .find(|index| index.name == "email_1")
        .expect("the email index");
    assert!(email.unique);
    assert!(email.size.is_some_and(|size| size > 0));
    let validator = customers.validator.expect("the $jsonSchema validator");
    assert_eq!(
        validator.get("bsonType"),
        Some(&Value::String("object".into()))
    );
    let view = adapter.collection(&shop("paid_orders")).unwrap();
    assert_eq!(view.kind, ContainerKind::View);
    assert!(view.indexes.is_empty());
    assert_eq!(
        view.options.get("viewOn"),
        Some(&Value::String("orders".into()))
    );
    assert!(matches!(
        adapter.collection(&shop("nowhere")),
        Err(QueryError::Database { .. })
    ));
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn pages_by_id_without_gaps() {
    let mut adapter = open(&target(), Access::ReadOnly).unwrap();
    let mut seen = std::collections::HashSet::new();
    let mut after = None;
    let mut pages = 0;
    loop {
        let spec = BrowseSpec {
            limit: 1000,
            after,
            ..BrowseSpec::default()
        };
        let (documents, next) = page(adapter.as_mut(), "big", &spec);
        for document in &documents {
            let Some(Value::Int32(id)) = document.get("_id") else {
                panic!("{document:?}");
            };
            assert!(seen.insert(*id), "{id} came twice");
        }
        pages += 1;
        match next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(seen.len(), 20_000);
    assert!(pages <= 21, "{pages} pages");
    // With a sort, pages skip what came before.
    let sorted = BrowseSpec {
        sort: vec![(FieldPath::parse("n"), Direction::Descending)],
        limit: 150,
        ..BrowseSpec::default()
    };
    let (first, next) = page(adapter.as_mut(), "big", &sorted);
    assert_eq!(next, Some(Continuation::Skip(150)));
    assert_eq!(first[0].get("n"), Some(&Value::Int32(99)));
    let (second, _) = page(
        adapter.as_mut(),
        "big",
        &BrowseSpec {
            after: next,
            ..sorted
        },
    );
    assert!(second.iter().all(|d| !first.contains(d)));
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_filter_by_null_finds_only_nulls() {
    let mut adapter = open(&target(), Access::ReadOnly).unwrap();
    let ids = |adapter: &mut dyn DocumentAdapter, value: Option<&Value>| {
        let filter = literal::field_filter(&FieldPath::field("value"), value);
        let spec = BrowseSpec {
            filter,
            limit: 100,
            ..BrowseSpec::default()
        };
        let (documents, _) = page(adapter, "types", &spec);
        documents
            .iter()
            .filter_map(|document| document.get("_id").cloned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(adapter.as_mut(), Some(&Value::Null)),
        [Value::String("null".into())]
    );
    assert_eq!(
        ids(adapter.as_mut(), None),
        [Value::String("absent".into())]
    );
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_computed_id_still_pages_forward() {
    let mut adapter = open(&target(), Access::ReadOnly).unwrap();
    let first = BrowseSpec {
        projection: Some("{_id: \"$status\", number: 1}".into()),
        sort: Vec::new(),
        limit: 100,
        ..BrowseSpec::default()
    };
    let (documents, next) = page(adapter.as_mut(), "orders", &first);
    assert_eq!(documents.len(), 100);
    assert_eq!(next, Some(Continuation::Skip(100)));
    let (second, _) = page(
        adapter.as_mut(),
        "orders",
        &BrowseSpec {
            after: next,
            ..first
        },
    );
    let number = |document: &Document| document.get("number").cloned();
    assert_eq!(second.first().and_then(number), Some(Value::Int32(101)));
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn pages_through_ids_of_every_type() {
    // A database of this test's own, gone when it ends.
    let mut writer = connect(&target(), Access::ReadWrite).unwrap();
    let ids = [
        Bson::MinKey,
        Bson::Null,
        Bson::Int32(1),
        Bson::Int64(2),
        Bson::Double(2.5),
        Bson::String("a".into()),
        Bson::String("b".into()),
        Bson::Document(doc! { "k": 1 }),
        Bson::ObjectId(bson::oid::ObjectId::new()),
        Bson::Boolean(false),
        Bson::Boolean(true),
        Bson::DateTime(bson::DateTime::from_millis(0)),
        Bson::MaxKey,
    ];
    let documents: Vec<BsonDocument> = ids.iter().map(|id| doc! { "_id": id.clone() }).collect();
    writer
        .command("kuery_mixed_ids", doc! { "dropDatabase": 1 })
        .unwrap();
    writer
        .command(
            "kuery_mixed_ids",
            doc! { "insert": "mixed", "documents": documents },
        )
        .unwrap();
    let mut adapter = open(&target(), Access::ReadOnly).unwrap();
    let mixed = CollectionRef::new(&["kuery_mixed_ids", "mixed"]);
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let spec = BrowseSpec {
            limit: 2,
            after,
            ..BrowseSpec::default()
        };
        let mut documents = Vec::new();
        let next = adapter.browse(&mixed, &spec, &mut documents).unwrap();
        seen.extend(documents.iter().filter_map(|d| d.get("_id").cloned()));
        match next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    writer
        .command("kuery_mixed_ids", doc! { "dropDatabase": 1 })
        .unwrap();
    // Every one once, in the order MongoDB sorts them.
    let expected: Vec<Value> = ids.iter().map(convert::value).collect();
    assert_eq!(seen, expected);
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn filters_projects_and_finds_by_quick_filters() {
    use kuery_core::browse::{Filter, FilterOp};
    let mut adapter = open(&reader(), Access::ReadOnly).unwrap();
    let spec = BrowseSpec {
        filter: Some("{status: 'paid'}".into()),
        quick: vec![Filter {
            column: "number".into(),
            op: FilterOp::LessOrEqual,
            value: "20".into(),
        }],
        projection: Some("{number: 1, status: 1, _id: 0}".into()),
        ..BrowseSpec::default()
    };
    let (documents, next) = page(adapter.as_mut(), "orders", &spec);
    assert_eq!(next, None);
    assert_eq!(documents.len(), 5, "{documents:?}");
    assert!(
        documents
            .iter()
            .all(|d| d.0.len() == 2 && d.get("status") == Some(&Value::String("paid".into())))
    );
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn counts_with_a_cap() {
    let mut adapter = open(&reader(), Access::ReadOnly).unwrap();
    let mut count = |collection: &str, filter: Option<&str>, cap| {
        adapter.count(&shop(collection), filter, cap).unwrap()
    };
    assert_eq!(count("orders", None, 0), Count::Exact(500));
    assert_eq!(
        count("orders", Some("{status: 'paid'}"), 1000),
        Count::Exact(125)
    );
    assert_eq!(
        count("orders", Some("{status: 'paid'}"), 10),
        Count::AtLeast(10)
    );
    assert_eq!(count("orders", Some(" "), 10), Count::Exact(500));
    assert_eq!(count("paid_orders", None, 0), Count::Exact(125));
    assert_eq!(count("readings", None, 0), Count::Exact(300));
    // Absent and null: `null` finds both.
    assert_eq!(
        count("orders", Some("{coupon: null}"), 0),
        Count::Exact(450)
    );
    assert_eq!(
        count("orders", Some("{coupon: {$exists: true}}"), 0),
        Count::Exact(100)
    );
    // Counting stops after five seconds.
    let started = std::time::Instant::now();
    let slow = Some(
        "{$expr: {$function: {body: 'function () { sleep(100); return true }', args: [], lang: 'js'}}}",
    );
    assert_eq!(count("big", slow, 1000), Count::Unknown);
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    // `$where` only works in the count command.
    let javascript = Some("{$where: 'this.n < 10'}");
    assert_eq!(count("big", javascript, 100), Count::AtLeast(100));
    // Uncapped over a small collection: `$where` over all of `big` takes
    // about as long as the five-second limit on a busy machine.
    assert_eq!(
        count("orders", Some("{$where: 'true'}"), 0),
        Count::Exact(500)
    );
    let nested = Some("{$or: [{number: {$lt: 0}}, {$where: 'this.number <= 10'}]}");
    assert_eq!(count("orders", nested, 0), Count::Exact(10));
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn samples_at_random_and_views_from_the_start() {
    let mut adapter = open(&reader(), Access::ReadOnly).unwrap();
    let mut sample = Vec::new();
    adapter.sample(&shop("orders"), 20, &mut sample).unwrap();
    assert_eq!(sample.len(), 20);
    let mut view = Vec::new();
    adapter.sample(&shop("paid_orders"), 5, &mut view).unwrap();
    assert_eq!(view.len(), 5);
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn keeps_bson_types() {
    let mut adapter = open(&reader(), Access::ReadOnly).unwrap();
    let (documents, _) = page(adapter.as_mut(), "types", &BrowseSpec::default());
    let value = |id: &str| {
        documents
            .iter()
            .find(|d| d.get("_id") == Some(&Value::String(id.into())))
            .unwrap_or_else(|| panic!("{id} is missing"))
            .get("value")
            .cloned()
    };
    assert_eq!(value("long"), Some(Value::Int64(9_007_199_254_740_993)));
    assert_eq!(value("int"), Some(Value::Int32(42)));
    assert_eq!(value("double"), Some(Value::Double(42.0)));
    assert_eq!(
        value("decimal"),
        Some(Value::Decimal("1234567890.123456789".into()))
    );
    assert_eq!(value("date"), Some(Value::DateTime(1_793_190_896_789)));
    assert_eq!(value("before 1970"), Some(Value::DateTime(-14_182_940_000)));
    assert_eq!(value("null"), Some(Value::Null));
    assert_eq!(value("absent"), None);
    let kind = |id: &str| value(id).map(|value| value.type_name());
    assert_eq!(kind("object id"), Some("ObjectId"));
    assert_eq!(kind("uuid"), Some("UUID"));
    assert_eq!(kind("binary"), Some("Binary"));
    assert_eq!(kind("timestamp"), Some("Timestamp"));
    assert_eq!(kind("regex"), Some("Regex"));
    assert_eq!(kind("min key"), Some("MinKey"));
    assert_eq!(kind("max key"), Some("MaxKey"));
    // Each prints as a literal that reads back as it came.
    for document in &documents {
        let text = literal::render(&Value::Object(document.clone()), Style::Pretty);
        assert_eq!(&literal::parse_document(&text).unwrap(), document, "{text}");
    }
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn works_like_any_document_database() {
    kuery_core::document::conformance::run(&kuery_core::document::conformance::Fixture {
        opener: Arc::new(|access| open(&target(), access)),
        collection: CollectionRef {
            path: vec!["shop".into(), "orders".into()],
        },
        types: shop("types"),
        reader: Some(Arc::new(|access| open(&reader(), access))),
        slow: r#"db.big.find({$where: "sleep(100) || true"})"#.into(),
        writes: Some(kuery_core::document::conformance::Writes {
            collection: CollectionRef::new(&["kuery_conformance", "notes"]),
            clear: r#"db.getSiblingDB("kuery_conformance").dropDatabase()"#.into(),
            insert: r#"db.getSiblingDB("kuery_conformance").notes.insertOne({_id: 1})"#.into(),
            half_failing: r#"db.getSiblingDB("kuery_conformance").notes.insertOne({_id: 1})
db.getSiblingDB("kuery_conformance").notes.insertOne({_id: 1})"#
                .into(),
        }),
    });
}

/// Runs one console statement, as the session would hand it over.
fn console(mongo: &mut Mongo, statement: &str) -> Result<(Vec<Document>, Outcome), QueryError> {
    let mut documents = Vec::new();
    let outcome = mongo.execute(statement, &mut documents)?;
    Ok((documents, outcome))
}

fn count_of(documents: &[Document]) -> u64 {
    match documents {
        [document] => match document.get("count") {
            Some(Value::Int32(n)) => *n as u64,
            Some(Value::Int64(n)) => *n as u64,
            other => panic!("not a count: {other:?}"),
        },
        _ => panic!("not one document: {documents:?}"),
    }
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn runs_each_read_of_the_console() {
    let mut mongo = connect(&target(), Access::ReadOnly).unwrap();
    let mut read = |statement: &str| {
        console(&mut mongo, statement).unwrap_or_else(|error| panic!("{statement}: {error}"))
    };
    let (paid, outcome) =
        read(r#"db.orders.find({status: "paid"}, {number: 1}).sort({number: 1}).limit(3)"#);
    let numbers: Vec<_> = paid
        .iter()
        .map(|order| order.get("number").cloned())
        .collect();
    assert_eq!(
        numbers,
        [1, 5, 9].map(|n| Some(Value::Int32(n))),
        "{paid:?}"
    );
    assert_eq!(outcome.returned, 3);
    assert_eq!(read("db.orders.findOne({number: 7})").0.len(), 1);
    let (groups, _) = read(
        r#"db.orders.aggregate([{$group: {_id: "$status", n: {$sum: 1}}}, {$sort: {_id: 1}}])"#,
    );
    assert_eq!(groups.len(), 4);
    assert_eq!(
        count_of(&read(r#"db.orders.countDocuments({status: "paid"})"#).0),
        125
    );
    assert_eq!(
        count_of(&read("db.orders.countDocuments({}, {limit: 10})").0),
        10
    );
    assert_eq!(count_of(&read("db.orders.estimatedDocumentCount()").0), 500);
    assert_eq!(
        count_of(&read("db.orders.countDocuments({number: {$gt: 1000}})").0),
        0
    );
    assert_eq!(read("db.orders.distinct('status')").0.len(), 4);
    assert_eq!(read("db.orders.getIndexes()").0.len(), 3);
    let (stats, _) = read("db.orders.stats()");
    assert_eq!(
        stats[0].get("ns"),
        Some(&Value::String("shop.orders".into()))
    );
    let (stats, _) = read("db.stats()");
    assert_eq!(stats[0].get("db"), Some(&Value::String("shop".into())));
    assert_eq!(read("db.runCommand({ping: 1})").0.len(), 1);
    let (plan, _) = read(r#"db.orders.find({status: "paid"}).explain()"#);
    assert!(plan[0].get("queryPlanner").is_some(), "{plan:?}");
    let (databases, _) = read("show dbs");
    assert!(
        databases
            .iter()
            .any(|database| database.get("name") == Some(&Value::String("shop".into())))
    );
    let (names, _) = read("db.getCollectionNames()");
    assert!(names.len() >= 6, "{names:?}");
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn use_changes_the_database_of_what_follows() {
    let nowhere = Target {
        database: String::new(),
        ..target()
    };
    let mut mongo = connect(&nowhere, Access::ReadOnly).unwrap();
    // mongosh's own, without a database.
    assert_eq!(DocumentAdapter::home(&mongo), ["test"]);
    let (collections, _) = console(&mut mongo, "show collections").unwrap();
    assert!(
        collections.is_empty(),
        "test holds nothing: {collections:?}"
    );
    console(&mut mongo, "use shop").unwrap();
    let (collections, _) = console(&mut mongo, "show collections").unwrap();
    assert!(
        collections
            .iter()
            .any(|collection| collection.get("name") == Some(&Value::String("orders".into()))),
        "{collections:?}"
    );
    let (count, _) = console(&mut mongo, "db.orders.estimatedDocumentCount()").unwrap();
    assert_eq!(count_of(&count), 500);
    // Where it opened stays the same.
    assert_eq!(DocumentAdapter::home(&mongo), ["test"]);
    let opened_in_shop = connect(&target(), Access::ReadOnly).unwrap();
    assert_eq!(DocumentAdapter::home(&opened_in_shop), ["shop"]);
    // Another database, named for one statement only.
    let (count, _) = console(
        &mut mongo,
        r#"db.getSiblingDB("admin").getCollection("system.version").countDocuments({})"#,
    )
    .unwrap();
    assert!(count_of(&count) >= 1);
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_read_only_console_cannot_write_even_unchecked() {
    let mut mongo = connect(&target(), Access::ReadOnly).unwrap();
    for statement in [
        r#"db.orders.insertOne({kuery: "must never be written"})"#,
        r#"db.orders.aggregate([{$facet: {a: [{$out: "copy"}]}}])"#,
        r#"db.orders.explain().deleteMany({})"#,
        r#"db.runCommand({insert: "orders", documents: [{}]})"#,
        r#"db.orders.renameCollection("copy")"#,
    ] {
        assert_eq!(
            console(&mut mongo, statement).map(|_| ()),
            Err(QueryError::WritesDisabled),
            "{statement}"
        );
    }
    let (count, _) = console(&mut mongo, "db.orders.estimatedDocumentCount()").unwrap();
    assert_eq!(count_of(&count), 500);
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn writes_from_the_console_report_what_they_did() {
    let mut mongo = connect(&target(), Access::ReadWrite).unwrap();
    let mut run = |statement: &str| console(&mut mongo, statement);
    run("use kuery_console").unwrap();
    run("db.dropDatabase()").unwrap();
    let (_, inserted) =
        run("db.notes.insertMany([{_id: 1, a: 1}, {_id: 2, a: 2}, {_id: 3, a: 3}])").unwrap();
    assert_eq!(inserted.inserted, 3);
    let (_, updated) =
        run("db.notes.updateMany({a: {$gte: 2}}, {$set: {b: NumberLong(1)}})").unwrap();
    assert_eq!((updated.matched, updated.modified), (2, 2));
    let (_, upserted) =
        run("db.notes.updateOne({_id: 9}, {$set: {a: 9}}, {upsert: true})").unwrap();
    assert_eq!((upserted.matched, upserted.upserted), (0, 1));
    let (found, deleted) = run("db.notes.findOneAndDelete({a: 1})").unwrap();
    assert_eq!((found.len(), deleted.deleted), (1, 1));
    let duplicate = run("db.notes.insertOne({_id: 2})").map(|_| ());
    assert!(
        matches!(&duplicate, Err(QueryError::Database { code, .. }) if code == "11000"),
        "{duplicate:?}"
    );
    let (kept, _) = run("db.notes.find({b: NumberLong(1)}).sort({_id: 1})").unwrap();
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0].get("b"), Some(&Value::Int64(1)));
    let (_, removed) = run("db.notes.deleteMany({})").unwrap();
    assert_eq!(removed.deleted, 3);
    run("db.dropDatabase()").unwrap();
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn cancels_a_sleeping_command_and_keeps_working() {
    let mut mongo = connect(&target(), Access::ReadWrite).unwrap();
    let cancel = DocumentAdapter::canceller(&mongo);
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        cancel();
    });
    let started = std::time::Instant::now();
    let slept = console(
        &mut mongo,
        r#"db.adminCommand({sleep: 1, secs: 20, lock: "none"})"#,
    );
    assert_eq!(slept.map(|_| ()), Err(QueryError::Cancelled));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    canceller.join().unwrap();
    assert_eq!(left_behind(&mut mongo), Vec::<BsonDocument>::new());
    console(&mut mongo, "db.runCommand({ping: 1})").unwrap();
}

/// A database the write tests may change, dropped before they start and
/// again when they end, even when they fail.
struct Scratch {
    target: Target,
    name: &'static str,
}

impl Scratch {
    fn new(target: Target, name: &'static str) -> Scratch {
        let scratch = Scratch { target, name };
        scratch.drop_database();
        scratch
    }

    fn drop_database(&self) {
        let mut mongo = connect(&self.target, Access::ReadWrite).unwrap();
        console(
            &mut mongo,
            &format!("db.getSiblingDB({:?}).dropDatabase()", self.name),
        )
        .unwrap();
    }

    /// A session that may write, working in the scratch database.
    fn session(&self) -> DocumentSession {
        let target = Target {
            database: self.name.into(),
            ..self.target.clone()
        };
        futures::executor::block_on(DocumentSession::open(
            Arc::new(move |access| open(&target, access)),
            Access::ReadWrite,
        ))
        .unwrap()
    }

    fn notes(&self) -> CollectionRef {
        CollectionRef::new(&[self.name, "notes"])
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        self.drop_database();
    }
}

fn write_script(session: &DocumentSession, text: &str) -> Result<(), QueryError> {
    let events = session.run(
        text.into(),
        RunMode::Write,
        Heavy::Refuse,
        RunOptions::default(),
    );
    futures::executor::block_on(async {
        while let Ok(event) = events.recv().await {
            match event {
                DocEvent::Finished(_) => return Ok(()),
                DocEvent::Failed(error) => return Err(error),
                DocEvent::Documents(_) | DocEvent::Outcome(_) | DocEvent::Left(_) => {}
            }
        }
        Err(QueryError::Cancelled)
    })
}

fn notes_held(session: &DocumentSession, scratch: &Scratch) -> Count {
    futures::executor::block_on(session.count(scratch.notes(), None, 0, Heavy::Refuse)).unwrap()
}

fn previewed(session: &DocumentSession, text: &str) -> Vec<Impact> {
    futures::executor::block_on(session.preview(text.into(), Heavy::Refuse, RunOptions::default()))
        .unwrap_or_else(|error| panic!("{text}: {error}"))
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_script_is_all_or_nothing_on_a_replica_set() {
    let scratch = Scratch::new(target(), "kuery_atomic");
    let session = scratch.session();
    write_script(
        &session,
        "db.notes.insertOne({_id: 1})\ndb.notes.insertOne({_id: 2})",
    )
    .unwrap();
    assert_eq!(notes_held(&session, &scratch), Count::Exact(2));
    // The third goes in, the duplicate fails: neither is kept.
    let failed = write_script(
        &session,
        "db.notes.insertOne({_id: 3})\ndb.notes.insertOne({_id: 1})",
    );
    assert!(
        matches!(&failed, Err(QueryError::Database { code, .. }) if code == "11000"),
        "{failed:?}"
    );
    assert_eq!(notes_held(&session, &scratch), Count::Exact(2));
    // A read in the script sees what the script wrote before it.
    let events = session.run(
        "db.notes.insertOne({_id: 4})\ndb.notes.find({_id: 4})".into(),
        RunMode::Write,
        Heavy::Refuse,
        RunOptions::default(),
    );
    let found = futures::executor::block_on(async {
        let mut found = Vec::new();
        while let Ok(event) = events.recv().await {
            match event {
                DocEvent::Documents(batch) => found.extend(batch),
                DocEvent::Finished(_) => break,
                DocEvent::Failed(error) => panic!("{error}"),
                DocEvent::Outcome(_) | DocEvent::Left(_) => {}
            }
        }
        found
    });
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(notes_held(&session, &scratch), Count::Exact(3));
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_preview_on_a_replica_set_is_exact_and_changes_nothing() {
    let scratch = Scratch::new(target(), "kuery_preview");
    let session = scratch.session();
    write_script(
        &session,
        "db.notes.insertMany([{_id: 1, a: 1}, {_id: 2, a: 2}, {_id: 3, a: 3}])",
    )
    .unwrap();
    let impacts = previewed(
        &session,
        "db.notes.insertOne({_id: 4, a: 4})
db.notes.find({a: {$gte: 3}})
db.notes.updateMany({a: {$gte: 2}}, {$set: {b: 1}})
db.notes.deleteMany({})",
    );
    let estimates: Vec<Estimate> = impacts
        .iter()
        .map(|impact| impact.estimate.clone())
        .collect();
    assert_eq!(
        estimates,
        [
            Estimate::Exact(Outcome {
                inserted: 1,
                ..Outcome::default()
            }),
            Estimate::Reads,
            Estimate::Exact(Outcome {
                matched: 3,
                modified: 3,
                ..Outcome::default()
            }),
            Estimate::Exact(Outcome {
                deleted: 4,
                ..Outcome::default()
            }),
        ]
    );
    // The read saw the insert before it, in the transaction.
    assert_eq!(impacts[1].sample.len(), 2);
    assert_eq!(notes_held(&session, &scratch), Count::Exact(3));
    // What cannot run in a transaction is counted instead, or unknown.
    let impacts = previewed(
        &session,
        "db.notes.aggregate([{$match: {}}, {$out: 'copy'}])",
    );
    assert_eq!(impacts[0].estimate, Estimate::Unknown);
    let impacts = previewed(&session, "db.notes.drop()");
    assert_eq!(impacts[0].estimate, Estimate::Unknown);
    let mut mongo = connect(&target(), Access::ReadWrite).unwrap();
    let (collections, _) = console(&mut mongo, "use kuery_preview")
        .and_then(|_| console(&mut mongo, "show collections"))
        .unwrap();
    assert_eq!(collections.len(), 1, "{collections:?}");
    // The session goes on, with no transaction left open.
    write_script(&session, "db.notes.deleteOne({_id: 1})").unwrap();
    assert_eq!(notes_held(&session, &scratch), Count::Exact(2));
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_preview_on_a_standalone_server_is_estimated() {
    let Some(standalone) = standalone() else {
        return;
    };
    let scratch = Scratch::new(standalone, "kuery_estimate");
    let session = scratch.session();
    assert_eq!(session.capabilities().preview, Preview::Estimated);
    write_script(
        &session,
        "db.notes.insertMany([{_id: 1, a: 1}, {_id: 2, a: 2}, {_id: 3, a: 3}])",
    )
    .unwrap();
    let impacts = previewed(
        &session,
        "db.notes.insertMany([{_id: 4}, {_id: 5}])
db.notes.updateMany({a: {$gte: 2}}, {$set: {b: 1}})
db.notes.updateOne({_id: 9}, {$set: {a: 9}}, {upsert: true})
db.notes.deleteOne({a: {$gte: 1}})
db.notes.find()
db.notes.drop()",
    );
    let estimates: Vec<Estimate> = impacts
        .iter()
        .map(|impact| impact.estimate.clone())
        .collect();
    assert_eq!(
        estimates,
        [
            Estimate::Estimated(Count::Exact(2)),
            Estimate::Estimated(Count::Exact(2)),
            Estimate::Estimated(Count::Exact(1)),
            Estimate::Estimated(Count::Exact(1)),
            Estimate::Reads,
            Estimate::Unknown,
        ]
    );
    assert_eq!(impacts[1].sample.len(), 2);
    assert_eq!(impacts[3].sample.len(), 1);
    assert_eq!(notes_held(&session, &scratch), Count::Exact(3));
    // Without transactions, a script stops at its first failure and keeps
    // what ran before it.
    let failed = write_script(
        &session,
        "db.notes.insertOne({_id: 4})\ndb.notes.insertOne({_id: 1})\ndb.notes.insertOne({_id: 5})",
    );
    assert!(failed.is_err());
    assert_eq!(notes_held(&session, &scratch), Count::Exact(4));
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn counts_a_write_where_the_user_may_not_explain_it() {
    // The read-only user, on a connection Kuery lets write: the server
    // refuses the explain of a write, so the filter is counted instead.
    let mut mongo = connect(&reader(), Access::ReadWrite).unwrap();
    let mut sample = Vec::new();
    let counted = DocumentAdapter::estimate(
        &mut mongo,
        r#"db.orders.deleteMany({status: "paid"})"#,
        &mut sample,
    )
    .unwrap();
    assert_eq!(counted, Some(Count::Exact(125)));
    assert_eq!(sample.len(), 20);
    // As root, the server explains it.
    let mut root = connect(&target(), Access::ReadWrite).unwrap();
    let explained = root.explain_write(
        &command::build(
            &one(r#"db.orders.updateMany({status: "paid"}, {$set: {status: "paid"}})"#).unwrap(),
            "shop",
        )
        .unwrap()
        .unwrap(),
    );
    assert_eq!(explained, Ok(Some(Count::Exact(125))));
    let (count, _) = console(&mut root, r#"db.orders.countDocuments({status: "paid"})"#).unwrap();
    assert_eq!(count_of(&count), 125);
    // Estimating is a write's business: refused on a read-only connection.
    let mut reading = connect(&target(), Access::ReadOnly).unwrap();
    assert_eq!(
        DocumentAdapter::estimate(&mut reading, "db.orders.deleteMany({})", &mut Vec::new()),
        Err(QueryError::WritesDisabled)
    );
}

/// What a run sent: the outcomes of the statements that ended, what a
/// failed write run said it left, and how it ended.
struct Ran {
    outcomes: Vec<Outcome>,
    left: Vec<Left>,
    end: Result<DocSummary, QueryError>,
}

fn ran(events: async_channel::Receiver<DocEvent>) -> Ran {
    futures::executor::block_on(async {
        let mut ran = Ran {
            outcomes: Vec::new(),
            left: Vec::new(),
            end: Err(QueryError::Cancelled),
        };
        while let Ok(event) = events.recv().await {
            match event {
                DocEvent::Outcome(outcome) => ran.outcomes.push(outcome),
                DocEvent::Left(left) => ran.left.push(left),
                DocEvent::Finished(summary) => {
                    ran.end = Ok(summary);
                    break;
                }
                DocEvent::Failed(error) => {
                    ran.end = Err(error);
                    break;
                }
                DocEvent::Documents(_) => {}
            }
        }
        ran
    })
}

fn write(session: &DocumentSession, text: &str, heavy: Heavy) -> async_channel::Receiver<DocEvent> {
    session.run(text.into(), RunMode::Write, heavy, RunOptions::default())
}

/// A `failCommand` fail point on the dev replica set, turned off when it
/// goes, even when the test fails.
struct FailPoint;

impl FailPoint {
    fn set(data: &str) -> FailPoint {
        FailPoint::configure(&format!("mode: {{times: 1}}, data: {data}"));
        FailPoint
    }

    fn configure(how: &str) {
        let mut admin = connect(&target(), Access::ReadWrite).unwrap();
        let command = format!(r#"db.adminCommand({{configureFailPoint: "failCommand", {how}}})"#);
        console(&mut admin, &command).unwrap_or_else(|error| panic!("{command}: {error}"));
    }
}

impl Drop for FailPoint {
    fn drop(&mut self) {
        FailPoint::configure(r#"mode: "off""#);
    }
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_commit_that_answers_late_is_kept_and_says_so() {
    let scratch = Scratch::new(target(), "kuery_late_commit");
    let session = scratch.session();
    write_script(&session, "db.notes.insertOne({_id: 0})").unwrap();
    // Past the 10 s a commit waits, inside the grace after them.
    let blocked = FailPoint::set(
        r#"{failCommands: ["commitTransaction"], blockConnection: true, blockTimeMS: 12000}"#,
    );
    let late = ran(write(
        &session,
        "db.notes.insertOne({_id: 1})\ndb.notes.insertOne({_id: 2})",
        Heavy::Refuse,
    ));
    drop(blocked);
    assert!(late.end.is_ok(), "{:?}", late.end);
    assert!(late.left.is_empty(), "{:?}", late.left);
    assert_eq!(notes_held(&session, &scratch), Count::Exact(3));
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_commit_without_a_known_result_is_unknown_not_undone() {
    let scratch = Scratch::new(target(), "kuery_unknown_commit");
    let session = scratch.session();
    // The driver marks a commit out of time UnknownTransactionCommitResult.
    let failing = FailPoint::set(r#"{failCommands: ["commitTransaction"], errorCode: 50}"#);
    let unknown = ran(write(
        &session,
        "db.notes.insertOne({_id: 1})\ndb.notes.insertOne({_id: 2})",
        Heavy::Refuse,
    ));
    drop(failing);
    assert!(unknown.end.is_err(), "{:?}", unknown.end);
    assert_eq!(unknown.left, [Left::Unknown]);
    // The transaction the commit left open goes, so the scratch database
    // can be dropped.
    let mut admin = connect(&target(), Access::ReadWrite).unwrap();
    console(&mut admin, "db.adminCommand({killAllSessions: []})").unwrap();
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_script_cannot_end_the_transaction_kuery_runs_it_in() {
    let scratch = Scratch::new(target(), "kuery_inner_commit");
    let session = scratch.session();
    for script in [
        "db.notes.insertOne({_id: 1})\ndb.adminCommand({commitTransaction: 1})\ndb.notes.insertOne({_id: 1})",
        "db.notes.insertOne({_id: 2})\ndb.adminCommand({abortTransaction: 1})\ndb.notes.insertOne({_id: 3})",
        "db.notes.insertOne({_id: 4})\ndb.adminCommand({endSessions: []})",
    ] {
        let refused = write_script(&session, script);
        assert!(
            matches!(&refused, Err(QueryError::Unsupported(message)) if message.starts_with("Kuery runs each script's transaction itself")),
            "{script}: {refused:?}"
        );
        let previewed = futures::executor::block_on(session.preview(
            script.into(),
            Heavy::Refuse,
            RunOptions::default(),
        ));
        assert!(previewed.is_err(), "{script}: {previewed:?}");
    }
    assert_eq!(notes_held(&session, &scratch), Count::Exact(0));
    // The adapter refuses them too while its transaction is open, whatever
    // the session checked.
    let mut mongo = connect(&scratch.target, Access::ReadWrite).unwrap();
    console(&mut mongo, "use kuery_inner_commit").unwrap();
    mongo.begin().unwrap();
    console(&mut mongo, "db.notes.insertOne({_id: 5})").unwrap();
    for command in ["commitTransaction", "abortTransaction", "killSessions"] {
        let refused = console(&mut mongo, &format!("db.adminCommand({{{command}: 1}})"));
        assert!(
            matches!(&refused, Err(QueryError::Unsupported(_))),
            "{command}: {refused:?}"
        );
    }
    mongo.abort().unwrap();
    assert_eq!(notes_held(&session, &scratch), Count::Exact(0));
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_one_statement_write_runs_in_a_transaction_as_its_preview_did() {
    let scratch = Scratch::new(target(), "kuery_one_statement");
    let session = scratch.session();
    let script = "db.notes.insertMany([{_id: 1}, {_id: 2}, {_id: 1}, {_id: 3}])";
    let statements = (LANGUAGE.analyze)(script).unwrap();
    assert!(session.all_or_nothing(&statements));
    let failed = ran(write(&session, script, Heavy::Refuse));
    assert!(failed.end.is_err(), "{:?}", failed.end);
    assert_eq!(failed.left, [Left::Undone]);
    assert_eq!(notes_held(&session, &scratch), Count::Exact(0));
    // What no transaction holds runs on its own, and the run says so.
    write_script(&session, "db.notes.insertOne({_id: 1})").unwrap();
    let out = "db.notes.aggregate([{$match: {}}, {$out: 'copy'}])";
    assert!(!session.all_or_nothing(&(LANGUAGE.analyze)(out).unwrap()));
    write_script(&session, out).unwrap();
    let copied = futures::executor::block_on(session.count(
        CollectionRef::new(&[scratch.name, "copy"]),
        None,
        0,
        Heavy::Refuse,
    ));
    assert_eq!(copied, Ok(Count::Exact(1)));
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_cancelled_write_in_a_transaction_is_undone_even_when_it_ended() {
    let scratch = Scratch::new(target(), "kuery_cancel_undone");
    let session = scratch.session();
    write_script(&session, "db.notes.insertOne({_id: 0})").unwrap();
    let blocked =
        FailPoint::set(r#"{failCommands: ["insert"], blockConnection: true, blockTimeMS: 2000}"#);
    let events = write(&session, "db.notes.insertOne({_id: 1})", Heavy::Refuse);
    std::thread::sleep(Duration::from_millis(500));
    session.cancel();
    let cancelled = ran(events);
    drop(blocked);
    assert_eq!(cancelled.end.err(), Some(QueryError::Cancelled));
    assert_eq!(cancelled.left, [Left::Undone]);
    assert_eq!(notes_held(&session, &scratch), Count::Exact(1));
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn a_failed_write_says_what_it_changed_before_it_failed() {
    let Some(standalone) = standalone() else {
        return;
    };
    let scratch = Scratch::new(standalone, "kuery_partial");
    let session = scratch.session();
    // Two go in before the duplicate fails, and nothing undoes them.
    let partial = ran(write(
        &session,
        "db.notes.insertMany([{_id: 1}, {_id: 2}, {_id: 1}, {_id: 3}])",
        Heavy::Refuse,
    ));
    assert!(
        matches!(&partial.end, Err(QueryError::Database { code, .. }) if code == "11000"),
        "{:?}",
        partial.end
    );
    assert_eq!(
        partial.left,
        [Left::Kept {
            partial: Some(Outcome {
                inserted: 2,
                ..Outcome::default()
            })
        }]
    );
    assert_eq!(notes_held(&session, &scratch), Count::Exact(2));
    // Stopped halfway, what it changed is unknown: never nothing.
    let many: Vec<String> = (10..1010).map(|n| format!("{{_id: {n}}}")).collect();
    write_script(
        &session,
        &format!("db.notes.insertMany([{}])", many.join(",")),
    )
    .unwrap();
    let events = write(
        &session,
        r#"db.notes.updateMany({$where: "sleep(2) || true"}, {$set: {x: 1}})"#,
        Heavy::Allow,
    );
    std::thread::sleep(Duration::from_millis(1000));
    session.cancel();
    let cancelled = ran(events);
    assert_eq!(cancelled.end.err(), Some(QueryError::Cancelled));
    assert_eq!(cancelled.left, [Left::Kept { partial: None }]);
    let changed = futures::executor::block_on(session.count(
        scratch.notes(),
        Some("{x: 1}".into()),
        0,
        Heavy::Refuse,
    ));
    assert!(
        matches!(changed, Ok(Count::Exact(n)) if n > 0),
        "{changed:?}"
    );
}
