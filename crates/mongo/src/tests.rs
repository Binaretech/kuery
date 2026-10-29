//! Need the dev MongoDB (`docker compose -f dev/compose.yaml up -d mongo`),
//! or one at `KUERY_MONGO_URI` with the sample `shop` and its read-only user
//! `kuery_ro`. The standalone checks also need `mongo-standalone` (`docker
//! compose --profile standalone up -d mongo-standalone`) and pass without
//! checking anything when it is not running, unless
//! `KUERY_MONGO_STANDALONE=required` (as in CI) makes them fail instead.

use kuery_core::browse::Direction;
use kuery_core::document::capabilities::{Enforcement, Preview, Transactions};
use kuery_core::document::language::Style;
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
    assert!(server.starts_with("MongoDB "), "{server}");
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
    assert!(adapter.server().starts_with("MongoDB "));
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
    });
}
