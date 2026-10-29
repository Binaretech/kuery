# MongoDB y la familia de bases de datos documentales: diseño

Fecha: 2026-10-28 · Estado: borrador para aprobación

## Intención

Hoy Kuery trabaja con PostgreSQL, MySQL/MariaDB, SQLite, SQL Server y Redis. El dueño quiere añadir MongoDB, pero no como un tercer caso suelto al estilo de Redis. Lo que busca es una base para las bases de datos documentales en general, de modo que más adelante entren CouchDB, Couchbase, Firestore, DynamoDB, Cosmos DB, Elasticsearch/OpenSearch, ArangoDB, RavenDB o SurrealDB sin rehacer la interfaz.

El éxito se mide así: se añade una conexión MongoDB y se navega, se consulta, se edita y se protege con las mismas garantías que una conexión SQL. Un segundo motor documental tiene que costar un crate y una línea en `drivers.rs`, sin tocar pestañas, árbol, revisión ni barra de estado.

## Objetivos

1. **Conectar**: host/puerto (27017 por defecto), `mongodb+srv`, base de datos inicial, usuario, contraseña en el llavero, TLS (rustls con ring), túnel SSH y políticas de entorno. "Probar conexión" muestra, por ejemplo, `MongoDB 8.0.4 · replica set rs0 · este usuario solo puede leer`.
2. **Explorar**: bases de datos, colecciones, vistas, time series y capped, con conteo estimado; índices bajo cada colección.
3. **Navegar una colección** con barra de filtro/proyección/orden/límite, paginación por clave y una vista de detalle del documento (texto y árbol).
4. **Consultar** en una consola con sintaxis mongosh (un subconjunto analizado, nunca evaluado), completado, diagnósticos, cancelación, timeouts y exportación.
5. **Escritura controlada**: solo lectura por defecto, revisión según la política del entorno, vista previa del impacto, auditoría de cada escritura, y edición de documentos por etapas con comprobación optimista.
6. **Esquema por muestreo**, validador `$jsonSchema`, índices y **planes** en la pestaña Plan existente.
7. **Actividad**: operaciones en curso (`$currentOp`) y `killOp`.
8. **Contrato documental** en core, con conformidad y pruebas comunes, para que el segundo motor solo aporte su driver.
9. **SQL de solo lectura** sobre una colección, traducido a un `aggregate` que siempre se ve, con la vista Tabla por defecto.

## No objetivos

- Evaluar JavaScript: no habrá bucles, variables, `forEach`, `load()` ni `db.eval`. Para scripts está mongosh.
- Change streams, Atlas Search, Queryable Encryption ni CSFLE.
- Administración de usuarios, roles, sharding o réplicas.
- Autenticación LDAP, Kerberos, AWS IAM u OIDC (por ahora solo SCRAM y X.509 por archivo).
- Un modo de escritura con transacción abierta y Commit/Rollback a mano (ver Preguntas abiertas).
- SQL ANSI completo sobre documentos: en la v1 no hay escrituras SQL, joins, `UNNEST` ni `DISTINCT` (ver "SQL sobre documentos").

## Anti-objetivos

- Kuery nunca muestra "solo lectura garantizada por el servidor" si el servidor no lo garantiza. MongoDB no tiene sesión de solo lectura, y la interfaz lo dice.
- No habrá ramas `if mongo` en código compartido. Las diferencias de motor entran por `Language` y `Capabilities`, igual que las SQL entran por `Dialect`.
- No se descartan futuros del driver `mongodb` a mitad de una operación, porque su documentación lo desaconseja.
- No se pierden tipos BSON al editar: `NumberLong` sigue siendo Int64 y `Decimal128` sigue siendo Decimal128.

## Arquitectura existente sobre la que se construye

- `core/src/driver.rs`: `enum Family { Sql, KeyValue }`, `DriverInfo` (descriptor de datos), `SqlDriver { info, dialect, open }` y `REDIS`.
- `core/src/adapter.rs` y `core/src/session.rs`: un `Adapter` bloqueante y una `Session` genérica que lo ejecuta en su propio hilo. La sesión aporta la cola de trabajos, el aislamiento de pánicos, el contador de ejecuciones, la cancelación con reintentos (40 × 250 ms), el streaming con lotes crecientes y `reopen`. De todo eso, solo `Shared.dialect` y el adaptador tienen tipos SQL.
- `crates/mssql`: un runtime tokio estático multi-thread con `block_on` desde el hilo de la sesión. Es el precedente para un cliente async sin parámetro nuevo en `app::init`.
- Redis no tiene trait. Son `Driver::Redis` y `Connection::KeyValue(RedisDb)`, con unas 40 ramas repartidas entre `drivers.rs`, `explorer.rs`, `explorer/rows.rs`, `workspace.rs` (unos 12 `match` sobre `Tab`), `session.rs` y `write_mode.rs`. De Redis se aprovecha el patrón: `core/src/redis.rs` (`classify`, en el que lo desconocido cuenta como escritura, y `danger`), `redis_write.rs` (aprobar → vista previa → revisión → auditoría), `RedisDb.writes_allowed` (desbloqueo por sesión) y `redis_key_tab.rs` (ediciones por etapas que se detienen en el primer fallo).
- `app/src/connection.rs`: `Environment::access()` (Production → ReadOnly) y `write_policy()` (ConfirmDangerous / ConfirmWrites / SessionUnlock), ambos neutrales respecto de la familia.
- La presentación ya es genérica: `ResultsDelegate`/`ResultsPane`, el inspector (`inspect::inspect` formatea JSON), `stats`, `compare`, la exportación CSV/TSV y `core::plan::Plan`, con `plan/showplan.rs` como precedente de un formato de plan ajeno. El crate `files` ya aplana JSON en columnas, con lo anidado como texto JSON compacto.
- gpui-kit `=0.7.3` ya compila el resaltado JSON. JavaScript requiere la feature `tree-sitter-javascript`. gpui-component trae un `Tree` que Kuery aún no usa.

## Decisiones

### Tomadas por el dueño

- Añadir MongoDB y, a la vez, una dinámica general para bases documentales.
- Ofrecer SQL sobre documentos en su versión mínima: solo lectura, traducido y visible, con el resto como trabajo futuro (ver "SQL sobre documentos").
- El resto queda pendiente; ver Preguntas abiertas.

### Propuestas (abiertas a cambio)

- **Primero el contrato, a la manera SQL y no a la manera Redis.** Se añaden `Family::Document`, un descriptor `DocumentDriver { info, language, open }`, un trait bloqueante `DocumentAdapter` y una `DocumentSession` en core que concentra todas las garantías. La app recibe una sola rama `Document` por punto de despacho y una sola interfaz genérica: árbol, pestaña Colección y pestaña Consola.
- **Crate nuevo `crates/mongo` (`kuery-mongo`, scope `mongo`)**, con driver id `mongodb` y alias `mongo`, `atlas`, `documentdb`, `cosmos`, `ferretdb`, `percona`. Cubre los productos compatibles con el protocolo de MongoDB mediante capacidades detectadas al conectar, sin drivers separados.
- **Cliente**: `mongodb = { version = "3.9", default-features = false, features = ["compat-3-3-0", "bson-3", "rustls-tls", "dns-resolver"] }`. Su `rustls-tls` usa ring y hickory no añade TLS. Hay que verificar en el spike que `cargo tree -i aws-lc-rs` solo muestra la ruta de tiberius que ya existe.
- **Lenguaje**: un subconjunto de mongosh analizado con un parser escrito a mano que nunca se evalúa. Se clasifica como `core/src/redis.rs` clasifica los comandos de Redis.
- **Opciones de conexión basadas en datos**: `DriverInfo.options: &[OptionField]` y `ConnectionSpec.options: BTreeMap<String, String>`, con claves limitadas a una lista permitida por el driver, de modo que las credenciales nunca terminen en `project.toml`. Pegar una URI `mongodb://` la reparte en campos y mueve la contraseña al llavero.
- **Puerto de desarrollo 27019**, siguiendo la convención X9 (54329, 63799, 33069, 14339).

## Arquitectura propuesta

### 1. Core: worker compartido (refactor previo)

Se mueven a `core/src/worker.rs`, genéricos sobre el tipo de adaptador, el hilo, la cola de trabajos, el aislamiento de pánicos, el contador de ejecuciones, los reintentos de cancelación, `begin_run`/`end_run`, `Unrun` y el streaming por lotes:

```rust
pub(crate) struct Handle<A: ?Sized + Send + 'static> { jobs: mpsc::Sender<Job<A>>, state: Arc<State>, canceller: Canceller }
impl<A: ?Sized + Send + 'static> Handle<A> {
    fn call<T: Send>(&self, f: impl FnOnce(&mut Worker<A>) -> Result<T, QueryError> + Send) -> impl Future<Output = Result<T, QueryError>>;
    fn stream<E: Send>(&self, f: impl FnOnce(&mut Worker<A>, &Sender<E>) + Send) -> async_channel::Receiver<E>;
    fn cancel(&self);
}
```

`Session` pasa a ser `Handle<dyn Adapter>` más sus reglas SQL. `session/tests.rs` (pánico, cancelación que no alcanza la siguiente ejecución, lector que se va) y las conformidades SQL deben pasar sin cambios.

### 2. Core: modelo documental (`core/src/document/`)

```rust
// value.rs: neutral respecto del motor, conserva el orden de los campos
pub enum Value {
    Null, Bool(bool), Int32(i32), Int64(i64), Double(f64), Decimal(String),
    String(String), Binary { subtype: u8, bytes: Vec<u8> }, DateTime(i64 /* ms UTC */),
    Array(Vec<Value>), Object(Document),
    Ext(Ext),                       // ObjectId, Timestamp, Regex, MinKey, conjuntos Dynamo, GeoPoint…
}
pub struct Document(pub Vec<(String, Value)>);
pub struct Ext { pub kind: &'static str, pub display: String, pub raw: serde_json::Value /* EJSON canónico, sin pérdida */ }
pub struct FieldPath(pub Vec<PathPart>);            // a.b.0.c
pub struct DocKey(pub Value);                       // _id de Mongo; {pk, sk} de Dynamo; ruta de Firestore
pub struct CollectionRef { pub path: Vec<String> }  // [db, coll] | [bucket, scope, coll] | [db]

// catalog.rs
pub struct Container { pub name: String, pub kind: ContainerKind, pub leaf: bool, pub estimated: Option<u64>, pub size: Option<u64> }
pub struct CollectionInfo { pub kind: ContainerKind, pub indexes: Vec<DocIndex>, pub validator: Option<Document>, pub options: Document }

// change.rs: lo que genera Kuery; cada driver lo traduce
pub enum DocChange {
    Update  { key: DocKey, version: Option<Value>, set: Vec<(FieldPath, Value)>, unset: Vec<FieldPath>,
              expect: Vec<(FieldPath, Option<Value>)> /* valor anterior; None = ausente */ },
    Replace { key: DocKey, version: Option<Value>, document: Document },
    Insert  { document: Document },
    Delete  { key: DocKey, version: Option<Value>, expect: Vec<(FieldPath, Option<Value>)> },
}
pub struct BrowseSpec { pub filter: Option<String>, pub quick: Vec<browse::Filter>, pub sort: Vec<(FieldPath, Order)>,
                        pub projection: Option<String>, pub limit: usize, pub after: Option<Continuation> }

// language.rs: datos estáticos y funciones puras, como sql::Dialect; se puede usar en el hilo de la UI
pub struct Language {
    pub name: &'static str, pub editor: &'static str,                  // "javascript"
    pub analyze: fn(&str) -> Result<Vec<Statement>, Rejection>,        // cortar y clasificar, sin evaluar
    pub parse_document: fn(&str) -> Result<Document, ParseError>,
    pub render: fn(&Value, Style) -> String,                           // literal tipado: NumberLong(5), ObjectId("…")
    pub context: fn(&str, usize) -> CompletionContext,
    pub methods: &'static [&'static str], pub operators: &'static [&'static str], pub stages: &'static [&'static str],
}
pub struct Statement { pub span: Range<usize>, pub info: OpInfo }
pub struct OpInfo { pub kind: analysis::StatementKind, pub target: Option<CollectionRef>, pub summary: String,
                    pub dangers: Vec<Danger>, pub heavy: bool, pub explainable: bool }

// capabilities.rs: se detectan al abrir, no se suponen
pub struct Capabilities {
    pub levels: &'static [&'static str],      // ["Database", "Collection"]
    pub transactions: Transactions,           // None | Multi { lifetime: Duration }
    pub read_only: Enforcement,               // Server | ClientOnly(&'static str)
    pub preview: Preview,                     // Undone | Estimated | None
    pub explain: Option<ExplainKind>, pub activity: bool, pub kill: bool,
    pub sampling: Sampling,                   // Random | FirstN
    pub version_token: bool,
}
```

### 3. Core: el contrato (`core/src/document/adapter.rs`)

```rust
pub trait DocumentSink { fn documents(&mut self, batch: Vec<Document>) -> Result<(), QueryError>; }
pub struct Outcome { pub matched: u64, pub modified: u64, pub inserted: u64, pub deleted: u64, pub upserted: u64, pub returned: u64 }

pub trait DocumentAdapter: Send + 'static {
    fn language(&self) -> &'static Language;
    fn server(&self) -> String;
    fn capabilities(&self) -> Capabilities;
    fn notice(&self) -> Option<String> { None }             // protecciones que no puede dar
    fn limit(&mut self, o: &RunOptions) -> Result<(), QueryError>;   // → maxTimeMS, batchSize
    fn canceller(&self) -> Canceller;
    fn execute(&mut self, statement: &str, sink: &mut dyn DocumentSink) -> Result<Outcome, QueryError>;
    fn begin(&mut self) -> Result<(), QueryError> { unsupported() }
    fn commit(&mut self) -> Result<(), QueryError> { unsupported() }
    fn abort(&mut self) -> Result<(), QueryError> { unsupported() }
    fn children(&mut self, parent: &[String]) -> Result<Vec<Container>, QueryError>;
    fn collection(&mut self, c: &CollectionRef) -> Result<CollectionInfo, QueryError>;
    fn sample(&mut self, c: &CollectionRef, n: usize, sink: &mut dyn DocumentSink) -> Result<(), QueryError>;
    fn browse(&mut self, c: &CollectionRef, s: &BrowseSpec, sink: &mut dyn DocumentSink) -> Result<Option<Continuation>, QueryError>;
    fn count(&mut self, c: &CollectionRef, filter: Option<&str>, cap: u64) -> Result<Count /* Exact | AtLeast(n) | Unknown */, QueryError>;
    fn apply(&mut self, c: &CollectionRef, change: &DocChange) -> Result<Outcome, QueryError>;
    fn estimate(&mut self, statement: &str) -> Result<Option<Count>, QueryError> { Ok(None) }
    fn explain(&mut self, statement: &str, execute: bool) -> Result<plan::Plan, QueryError> { unsupported() }
    fn activity(&mut self) -> Result<Activity, QueryError> { unsupported() }
    fn signal(&mut self, id: &BackendId, s: Signal) -> Result<bool, QueryError> { unsupported() }
}
```

`driver.rs` gana `Family::Document` (etiqueta "Document"), `OpenDocument = fn(&ConnectOptions, Access) -> Result<Box<dyn DocumentAdapter>, String>`, `DocumentDriver { info, language, open }`, `DriverInfo.options` (vacío en los drivers actuales) y `ConnectOptions.options`.

### 4. Core: `DocumentSession` sobre `Handle<dyn DocumentAdapter>`

- `run(text, RunMode::{Read, Write}, RunOptions) -> Receiver<DocEvent>`, más `preview`, `browse`, `apply`, `children`, `collection`, `sample`, `count`, `explain`, `activity`, `signal`, `cancel` y `reopen`.
- `DocEvent { Documents(Vec<Document>), Outcome(Outcome), Finished(Summary), Failed(QueryError) }`.
- `language.analyze` se ejecuta antes de que nada llegue al adaptador. Con `ReadOnly`, o en una ejecución `Read`, todo lo que no sea `StatementKind::Read` se rechaza con `QueryError::Rejected`, y lo `heavy` solo se permite si quien llama lo autoriza (la app lo autoriza solo en Development).
- `apply` comprueba lo esperado **según el tipo de cambio**: `Update`, `Replace` y `Delete` exigen `matched == 1`; `Insert` exige `inserted == 1`. Si no se cumple, devuelve `RowMismatch`. Cuando hay `Transactions::Multi`, todo va en una sola transacción; si no, se detiene en el primer fallo e informa de cuáles se aplicaron.
- Ninguna transacción queda abierta mientras el usuario piensa.
- Funciones puras de apoyo: `flatten`, `schema::infer`, `diff(&Document, &Document) -> DocChange` y `export::write_documents`. `document_conformance` va detrás de la feature `conformance`.

### 5. `crates/mongo`

- **Runtime**: `static OnceLock<Runtime>` "kuery-mongo", multi-thread con 2 workers y `enable_all`, como en mssql. Tiene que ser multi-thread porque `Client` deja tareas (monitor SDAM, pool, `killCursors` al soltar un cursor) que deben seguir corriendo entre ejecuciones.
- **Ejecución sin descartar futuros**: cada operación se lanza con `runtime().spawn(op)` y el hilo de la sesión espera con `block_on(select!{ &mut handle, stop.requested(), sleep(timeout) })`. Al recibir la parada, ejecuta `killOp` por etiqueta y **sigue esperando** el `JoinHandle` durante un margen acotado, hasta que termina con `Interrupted` (11601) → `Cancelled`. Solo si vence el margen se desvincula la tarea y se informa con un aviso.
- **`open`**: construye `ClientOptions` desde los campos y opciones dentro de `block_on` y ejecuta `hello`, `buildInfo` y `connectionStatus { showPrivileges: true }`. De ahí salen las `Capabilities`:
  - `transactions` si hay `setName` o `msg == "isdbgrid"`;
  - `activity` y `kill` probando `$currentOp { ownOps: true }`;
  - el sabor (MongoDB, Atlas, DocumentDB, Cosmos, FerretDB);
  - `read_only`, según "Seguridad".
- **`execute`**: vuelve a analizar la sentencia y la convierte en un `bson::Document`. En `ReadOnly` aplica `wire::guard`, añade `comment: "kuery:<session>:<run>"` y `maxTimeMS`, y envía al sink los resultados de `cursor.try_next()` en lotes crecientes.
- Módulos: `shell/` (lexer y parser recursivo descendente que produce `enum Op`), `classify.rs`, `literal.rs`, `convert.rs` (Bson ⇄ `Value`) y `wire.rs`. El adaptador se suelta dentro de `runtime().enter()`, para que los `Drop` de `ClientSession` y `Cursor` puedan abortar transacciones y cerrar cursores.

### 6. App

- `drivers.rs`: `static DOCUMENT: [&DocumentDriver; 1] = [&kuery_mongo::DRIVER]`, `Driver::Document(&'static DocumentDriver)` y `Connection::Document(DocDb)`. Las ramas `test`/`connect` copian las de SQL: `tunnel::open_for` y luego `DocumentSession::open(opener, spec.environment().access())`. `Engines` no cambia.
- `connection.rs`: `DocDb { name, environment, catalog: DocumentSession, target, writes_allowed: Arc<AtomicBool>, _tunnel }`. El árbol y el muestreo usan `catalog`. Cada consola y cada pestaña Colección abre su propia sesión con `reopen`, como ya hacen las pestañas de consulta (`query_tab.rs`, `Session::reopen`) y compartiendo el `Client` (un solo pool), para que la barra lateral no quede en cola detrás de una consulta larga.
- `project.rs`: `options` con `serde(default)`, `dialect() → None` y `target()` en la forma `host:port/db` o `srv:cluster/db`.
- `explorer/documents.rs`, hermano de `redis_keys.rs`: árbol perezoso basado en `children(path)` con etiquetas tomadas de `capabilities.levels`, insignias de tipo, conteos estimados, una fila "Cargar más" y el filtro con su debounce de 350 ms. Añade `ConnectionState::DocumentReady` y `ExplorerEvent::{DocumentConnected, OpenCollection, OpenDocumentConsole}`. El menú contextual hace `match` sobre la familia en lugar del booleano `redis`.
- `Tab::Collection` y `Tab::DocumentConsole`, con `TabState::{Collection { connection, path, filter, projection, sort }, DocumentConsole { connection, path, text }}`. La consola tiene su **propio key context** (`DocumentConsole`), no `"QueryTab …"` como hace `redis_console.rs:625`, para que Guardar, Formatear, Explain y Commit no queden enlazados sin hacer nada.
- Un refactor previo sustituye `Tab::redis()`/`Tab::db()` por `Tab::connection(&App) -> Option<ConnMeta>`, de modo que la barra de estado, la franja de producción y el tooltip dejan de depender de la familia. `write_review.rs` sale de `redis_write.rs` (con un `Planned` neutral) y `Scope::Redis` pasa a ser `Scope::NoTransactions { label, note }`.

## La dinámica general de las bases documentales

**Modelo común** (lo que comparten todos los motores investigados):

- Una conexión tiene de 1 a 3 niveles de contenedor por encima de algo parecido a una colección: Mongo `db > coll`; Couchbase `bucket > scope > collection`; Firestore, rutas de subcolecciones a cualquier profundidad (`leaf`); CouchDB, DynamoDB y Elastic, un único nivel.
- Documentos que forman un árbol de valores más amplio que JSON, con `Ext` para los tipos propios de cada motor.
- Una identidad opaca (`DocKey`) y, en muchos motores, un token de versión: `_rev`, CAS, `_etag`, `seq_no/primary_term`, change vector.
- Operaciones que todos pueden servir: listar hijos, navegar con continuación (cursor, bookmark, `LastEvaluatedKey`, `search_after`), obtener por clave, muestrear, listar índices, insertar, reemplazar o borrar por clave, y explain como árbol de plan.

**Qué es genérico y qué es de cada motor**

| Genérico (core y app, se escribe una vez) | Propio del motor (driver, detrás de `Language`/`Capabilities`) |
| --- | --- |
| `DocumentSession`: solo lectura, política, revisión, auditoría, timeouts, cancelación, pánicos | Lenguaje de consulta, su parser y su clasificador |
| `Value`, `flatten`, `schema::infer`, `diff`, exportación NDJSON | Sintaxis de actualización parcial (`$set`, PUT con `_rev`, UpdateExpression) |
| Árbol, pestaña Colección, Consola, visor de documento, completado | Transacciones (ninguna, por partición, multi-documento con 60 s) |
| `write_review`, desbloqueo por sesión, `AuditRecord` | Cómo se cancela (killOp, abortar HTTP, DELETE de requests activas) |
| Pestaña Plan (`core::plan::Plan`), pestaña Actividad | Formato de explain, `$currentOp`, TLS y autenticación |
| `document_conformance`, filtrada por `Capabilities` | Cómo se impone el solo lectura (servidor o solo cliente) |

**Cómo entra un segundo motor**

1. Crear un crate `crates/<motor>` que exporte `pub static DRIVER: DocumentDriver`, con su `Language` (parser, clasificador, `render`, `parse_document`).
2. Implementar `DocumentAdapter` y la detección de capacidades.
3. Pasar `document_conformance` contra un contenedor de `dev/compose.yaml`.
4. Añadir una línea en `drivers.rs DOCUMENT`, el `[[package]]` y la entrada en `changelog_include` de `release-plz.toml`, y su scope en `AGENTS.md`.

**Reglas de estabilidad del contrato**:

- `DocumentAdapter` se considera **inestable** hasta que un motor que no hable el protocolo de MongoDB pase `document_conformance`.
- Si ese motor muestra fugas, las correcciones entran como commits `refactor(core)`.
- **Regla de salida**: un motor que necesite su propia pestaña de colección, o más de unas dos capacidades que solo él usa, no es un motor documental para Kuery. Recibe su propia familia, como Redis, y el trait no se deforma para encajarlo.

**Coste estimado por motor**:

- DocumentDB, Cosmos DB para MongoDB, FerretDB y Percona: días, sobre todo de pruebas. Son alias o entradas que reutilizan `kuery_mongo::open`.
- CouchDB: unas 1,5–2,5k líneas. Un solo nivel; `_find` con bookmark; `_rev` como token, lo que hace gratis la comprobación optimista; sin transacciones; `_explain`.
- SurrealDB: unas 2k líneas. Crate async oficial, tres niveles, SurrealQL, transacciones, rol Viewer.
- DynamoDB: más de 3k líneas, con riesgo. La autenticación no es host/usuario; no tiene explain; y el TLS por defecto del SDK (aws-lc-rs) choca con la política de solo ring.

Los puntos que podrían forzar cambios en el contrato son las actualizaciones por script (Elastic `_update`), las transacciones limitadas a una partición (Cosmos) y los documentos sin clave estable (data streams).

## Lenguaje de consulta

Se usa un **subconjunto de mongosh que se analiza y nunca se evalúa**: la gente pega `db.orders.find({status: "paid"}).sort({_id: -1})` desde la documentación y su propio código. Incrustar un motor JS (boa, QuickJS) haría imposible clasificar lecturas y escrituras, y el solo lectura en producción sería falso.

- **Sentencias**: separadas por `;` o por saltos de línea a profundidad 0, con comentarios `//` y `/* */`. Incluyen `use x`, `show dbs|collections`, `db.<coll>`, `db.getCollection("a.b")` y `db.getSiblingDB("x")`.
- **Lecturas**: `find`/`findOne` con `.sort .limit .skip .project .hint .collation .maxTimeMS .batchSize .explain()`, además de `aggregate`, `countDocuments`, `estimatedDocumentCount`, `distinct`, `getIndexes` y `stats`.
- **Escrituras**: `insertOne/Many`, `updateOne/Many`, `replaceOne`, `deleteOne/Many`, `findOneAnd*` y `bulkWrite`. Como esquema: `createIndex(es)`, `dropIndex(es)`, `createCollection`, `createView`, `drop`, `renameCollection` y `db.dropDatabase()`.
- **`db.runCommand` / `db.adminCommand`**: hay una lista permitida de lectura (`ping`, `hello`, `buildInfo`, `serverStatus`, `dbStats`, `collStats`, `listCollections`, `listIndexes`, `connectionStatus`, `hostInfo`). Todo lo demás cuenta como escritura y, en producción, como `Unsupported`.
- **Literales**: JSON5 (claves sin comillas, comillas simples, comas finales), `/regex/flags`, `ObjectId()`, `ISODate()`, `new Date()`, `NumberInt/Long/Decimal()`, `UUID()`, `BinData()`, `Timestamp()`, `MinKey/MaxKey()` y Extended JSON canónico o relajado.
- **Regla numérica**: `42` es Int32 (Int64 si no cabe) y `42.0` o `4.2e1` es Double. El impresor escribe siempre Double con punto decimal, Int64 como `NumberLong("…")` y Decimal128 como `NumberDecimal("…")`. Una prueba de propiedad exige que **imprimir y volver a analizar dé el mismo valor para todo tipo BSON**. Divergencia que hay que documentar: en mongosh, js-bson guarda `42.0` como Int32 porque para JavaScript es el mismo número que `42`; en Kuery es Double.
- **Se rechaza con mensaje claro**: variables, funciones, bucles, `forEach`, `load()`, `db.eval`, `mapReduce` y `Mongo()`. El mensaje es "Kuery ejecuta comandos de MongoDB, no JavaScript: un comando por sentencia".

**Clasificador** (`classify(&Op) -> OpInfo`; lo desconocido cuenta como Write):

- **Read**: find, aggregate sin `$out`/`$merge`, count, distinct, listados, `explain` y comandos permitidos.
- **Data**: escrituras CRUD, `findAndModify`, `bulkWrite`, y aggregate con `$out`/`$merge`. Estos se buscan **de forma recursiva** dentro de `$lookup`, `$unionWith` y `$facet`, y en el comando interno de `explain`, tanto en el clasificador como en `wire::guard`.
- **Schema**: crear, borrar o renombrar colecciones e índices, `createView` y `collMod`.
- **Danger**: `updateMany`/`deleteMany` con filtro vacío, `drop`, `dropDatabase`, `dropIndexes("*")`, `$out` sobre una colección existente, `renameCollection` con `dropTarget`, `replaceOne` sin `_id` en el filtro, y comandos de usuarios o roles.
- **Heavy**: JS en el servidor (`$where`, `$function`, `$accumulator`), construcciones de `createIndex`, `validate` y `compact`. `$lookup` no es heavy por sí mismo.

**Tres superficies con una sola gramática de literales**:

1. La Consola: el subconjunto completo, resaltado con `tree-sitter-javascript`, una feature nueva en la línea fijada `=0.7.3`.
2. La barra de filtro de la pestaña Colección, al estilo Compass (Filter, Projection, Sort, Limit), analizada como expresiones.
3. El editor de documentos (`parse_document`/`render`).

Los errores de análisis llegan al editor como diagnósticos con su span. Los demás motores ocupan el mismo hueco `Language`: Mango JSON y `METHOD /path {json}` para CouchDB, SQL++, Query DSL, AQL o SurrealQL.

## Resultados, visor y edición

- **Rejilla**:
  - `document::flatten` reparte los campos de primer nivel en columnas: `_id` primero, luego por presencia y por primera aparición, con un máximo de 200. Los escalares se muestran con `render(Compact)` y lo anidado como literal compacto, que el inspector ya formatea.
  - `ColumnKind` se deduce del tipo dominante y se añaden `ColumnKind::Json` e `Id`. Stats, Profile, compare y CSV/TSV siguen funcionando.
  - Un campo ausente se pinta como "—" atenuado y un `null` explícito como el NULL en cursiva de siempre.
- **Columnas que aparecen tarde**: `ResultSet::extend_columns(&[String])` y `Event::MoreColumns` rellenan con arrays nulos los lotes ya recibidos. El `debug_assert` de `result_set.rs:161` se sustituye por un append comprobado, y `ResultsDelegate::set_columns` deja de reiniciar al extender.
- **Documentos originales junto a la rejilla**: un `DocumentDelegate` guarda `Vec<Document>` en paralelo al `ResultSet` (fila i ↔ documento i). Es la fuente de verdad para el detalle, la edición, la copia y la exportación; el texto de la celda nunca lo es.
- **Detalle**: en el hueco del inspector de `ResultsPane` hay dos modos.
  - Texto: un `Editor` de solo lectura (javascript) con `render(Pretty)`, plegado y guías.
  - Árbol: el `Tree` de gpui-component, con insignias de tipo, Copiar ruta, Copiar valor y **Filtrar por este valor**, que escribe `{path: value}` en la barra.
- **Copiar y exportar**:
  - *Copiar documento* en tres formas: literal, EJSON relajado y EJSON canónico.
  - Exportación NDJSON y array JSON a partir de los documentos originales. La exportación de una consulta completa **recorre el cursor en streaming**, no solo las filas cargadas.
  - En estas rejillas, "Copy Row as JSON" copia el documento real.
- **Pestaña Colección**:
  - Paginación por clave sobre `_id` cuando no hay otro orden; si lo hay, skip/limit con un aviso.
  - Los filtros rápidos de cabecera reutilizan `core::browse::Filter`.
  - Conteo: `estimatedDocumentCount` sin filtro; con filtro, `countDocuments(filter, {limit: N+1, maxTimeMS: 5 s})`, que muestra "más de N" cuando llega al límite y "desconocido" cuando vence el tiempo.
  - Las vistas se abren en solo lectura y muestran su pipeline.
- **Edición por etapas**:
  1. *Editar documento* abre el detalle editable sobre el texto literal tipado. *Insertar* parte de una plantilla del esquema muestreado. *Duplicar* y *Borrar* son acciones de fila.
  2. Al preparar el cambio, `parse_document` y `diff` producen `DocChange::Update`: `set` con las hojas cambiadas (un array cambiado se escribe entero), `unset` con lo eliminado (nunca se convierte en `null`) y `expect` con los valores anteriores. Cambiar `_id` se rechaza y se ofrece Duplicar.
  3. Los cambios pendientes se marcan en ámbar y rojo tachado (`grid.rs`). `has_pending` alimenta las guardas de cierre y reinicio.
  4. Al aplicar, el filtro es `{_id, …expect}`, con `$exists: false` para lo ausente. Si `matched` es 0 → `RowMismatch` "cambió desde que lo cargaste". Con transacciones, todo o nada. Sin ellas, se detiene en el primer fallo y lo demás queda pendiente.
- **Completado** (`DocumentCompletions`, mismo patrón que `RedisCompletions`):
  - tras `db.`, las colecciones;
  - tras `db.x.`, los métodos;
  - en posición de clave con `$`, los operadores (o las etapas dentro de `aggregate([`);
  - en el resto de claves, las rutas muestreadas y declaradas.

## Seguridad

- **Solo lectura en capas.** Production → `Access::ReadOnly`, igual que en SQL.
  1. *Clasificador* (`DocumentSession`, en todos los motores): todo lo que no sea Read se rechaza antes de enviarse.
  2. *Guardia de protocolo* (adaptador Mongo, independiente del parser): inspecciona el `bson::Document` final y comprueba que el nombre del comando esté en la lista permitida, que ningún pipeline contenga `$out`/`$merge` (con búsqueda recursiva) y que `begin`, `apply` e `insert*` queden rechazados. Así un fallo del parser no se convierte en una escritura.
  3. *Servidor, detectado y nunca supuesto*: `connectionStatus { showPrivileges: true }` solo cuenta como `Enforcement::Server` si **hay al menos un usuario autenticado** (`authInfo.authenticatedUsers` no vacío) y sus privilegios no incluyen ninguna acción de escritura. Sin autenticación (el mongo de Docker por defecto) las listas vienen vacías aunque cualquiera puede escribir, así que eso es `ClientOnly`, con el aviso "este servidor no tiene autenticación". El mensaje dice "al conectar", porque los privilegios pueden cambiar durante la sesión.
- **Avisos visibles** en Probar conexión, en el tooltip de la barra de estado y en el diálogo de escritura: "MongoDB no tiene sesión de solo lectura: Kuery rechaza las escrituras por su cuenta. Para una garantía, conéctate con un usuario con el rol `read`". También "conectado a un secundario" y "standalone: sin transacciones, las escrituras no son atómicas". `readPreference` nunca se presenta como protección.
- **Escrituras**:
  - Políticas sin cambios: Development → ConfirmDangerous; Staging → ConfirmWrites; Production → SessionUnlock (escribir el nombre de la conexión), con `writes_allowed` compartido por las pestañas y nunca guardado.
  - La consola tiene un conmutador Read-only/Write por pestaña.
  - Los peligros siempre piden confirmación.
  - Lo Heavy se rechaza en producción.
- **Revisión** (`write_review` compartido con Redis): por cada sentencia se muestran resumen, destino, peligros y la vista previa con su etiqueta.
  - "Exacta (ejecutada y deshecha en una transacción)" donde hay `Preview::Undone`: una vista previa es corta y cabe en los 60 s de una transacción.
  - Si no, "Estimada" con `countDocuments(filter, {limit: 10001, maxTimeMS})` más los primeros 20 documentos.
  - Si no se puede contar, "Desconocida", y la escritura se trata como peligrosa.
  - Un script Write va en una sola transacción cuando el servidor las tiene, y el DDL se rechaza dentro de ella (el análogo de `refuse_implicit_commits`). Si no, va sentencia a sentencia hasta el primer fallo, y la revisión lo dice.
- **Auditoría**: cada escritura (consola, aplicar ediciones, índices) y cada `killOp` genera un `AuditRecord` con modo "Document write", acción Run o Apply, el comando como literal y `changed` = recuento modificado. Las ediciones registran el diff, no documentos completos.
- **Timeouts y cancelación**:
  - `maxTimeMS` sale de `RunOptions.statement_timeout`; `serverSelectionTimeout` es de 10 s. DocumentDB reinicia `maxTimeMS` en cada `getMore`, y se anota.
  - La cancelación usa etiqueta de comentario → `$currentOp` → `killOp`, y espera a que la tarea termine.
  - Dónde se permite `$currentOp`/`killOp` se **detecta por conexión**, no por sabor: DocumentDB documenta ambos, y los casos que de verdad necesitan el respaldo son los planes compartidos/Flex de Atlas. Ahí el aviso dice "cancelar solo deja de esperar".
- **Credenciales y TLS**:
  - Contraseña solo en el llavero; claves de `options` limitadas a la lista permitida.
  - rustls con ring y raíces webpki. Las CA privadas (el bundle de RDS de DocumentDB) se dan como archivo `tlsCAFile`. "Permitir certificados inválidos" solo se ofrece fuera de producción y con aviso.
- **SSH y SRV**:
  - Un túnel reenvía un único host:puerto. Con túnel se fuerza `directConnection=true` y se rechazan `+srv` y las listas de varios hosts ("Por SSH Kuery llega a un solo miembro").
  - TLS a través del túnel no puede verificar el nombre original: la limitación es del crate `mongodb`, que no acepta un `ClientConfig` propio y solo ofrece `allow_invalid_hostnames` con openssl. rustls en sí permite elegir el `ServerName`. Es un spike antes de la fase de conexión.
  - Sin túnel, un único host sin `replicaSet` se conecta en modo directo, que es el comportamiento documentado de mongosh.
  - `*.mongodb.net` y los hosts SRV se clasifican como producción por defecto.
- **Redis** sigue abriendo sin nivel de acceso. Se corrige aparte (ver fases).

## Esquema, índices, explain y actividad

- **Esquema**: `sample(c, 1000)` con `$sample` y `maxTimeMS`, o las primeras N filas (`FirstN`, etiquetado) en vistas y en motores sin muestreo aleatorio. `schema::infer` produce rutas hasta profundidad 4 (`path[]` para arrays), porcentaje de presencia, histograma de tipos y marca de tipos mixtos ("price: Double 92 %, String 8 %"). Se muestra junto al validador `$jsonSchema` ("Declarado" frente a "Muestreado de 1.000 documentos"). Hay una caché por (conexión, colección) que alimenta el completado y el orden de columnas.
- **Índices**: en el árbol, bajo cada colección, y en una sección de la pestaña: claves, unique, sparse, partial, TTL, hidden, tamaño y uso (`$indexStats` cuando hay permiso). Se crean y borran desde la consola con revisión.
- **Explain**: `core/src/plan/mongo.rs`, como `showplan.rs`, convierte `queryPlanner` y `executionStats` en `Plan`: COLLSCAN → "Leer todo orders", IXSCAN → "Usar el índice status_1", FETCH, SORT, `$lookup`… Insights: COLLSCAN grande, SORT en memoria, `docsExamined` muy por encima de `nReturned`, planes rechazados. "Explain con estadísticas" ejecuta la lectura, así que obedece las reglas de solo lectura y heavy. El explain de una escritura nunca se ejecuta.
- **Actividad**: `$currentOp` (todos los usuarios con `inprog`, si no `ownOps` con aviso) se convierte en `Activity`. Hace falta que `Backend.pid: i32` pase a `BackendId(String)`, porque en mongos el opid es "shard:opid". Cancel → `killOp`; Terminate → Unsupported; `waitingForLock` se muestra como estado. Las operaciones propias se reconocen por la etiqueta, y un permiso denegado se explica en lugar de mostrar una lista vacía.

## SQL sobre documentos

Quien viene de SQL puede consultar MongoDB en SQL: Kuery lo traduce a un `aggregate` nativo, lo muestra y ejecuta **ese** comando. No hay una vía de ejecución propia de SQL, y SQL no es un lenguaje de primera clase de los documentos: es una comodidad de lectura con un dialecto documentado ("SQL de Kuery para documentos") que prueban las conformidades contra el servidor real. Lo que no se puede traducir con fidelidad se rechaza con un mensaje; nunca se aproxima en silencio.

Llega después de la consola mongosh y la edición de documentos, porque traduce al `Op`, al `render` y al `wire::guard` de esas fases.

### Forma

- **Front end, no `Language`**: `DocumentDriver` gana `frontends: &'static [QueryFrontend]`, con `QueryFrontend { name, editor, split, translate }`. `Language` sigue siendo el lenguaje nativo único (literales, `render`, completado), y SQL no tiene que fingir que analiza o imprime documentos.
- **El traductor vive en `crates/mongo/src/sql/`**, sobre el sqlparser 0.63 del workspace con `GenericDialect`. Se extrae a core solo cuando exista un segundo `lower` real; una IR "neutral" diseñada desde Mongo acabaría teniendo forma de Mongo.
- **Siempre baja a `aggregate`** (`$match`, `$group`, `$project`, `$match` del HAVING, `$sort`, `$skip`, `$limit`). Nunca a `find`, `countDocuments` ni `distinct`, que tienen semánticas distintas (`distinct` desenrolla arrays y omite ausentes). Optimizar a `find` queda para más adelante, con pruebas de equivalencia.
- **Lo que se ve es lo que se ejecuta**: la tira "Traducción" bajo el editor se actualiza con debounce, y Ejecutar traduce de forma síncrona, actualiza la tira y envía ese mismo texto a `DocumentSession::run`. Acciones: Copiar como mongosh, Abrir en mongosh, Explain (del nativo). No hay traducción inversa.
- **Spike previo de un día**: probar `mongodb/mongosql` (Apache-2.0, Rust, solo por git, versión 0.0.0, bson 2) con un `Catalog` construido desde `schema::infer`. Si encaja, aporta la semántica de MISSING ya resuelta; si no, se escribe el traductor propio sobre sqlparser.

### Subconjunto v1

- Una sola colección: `FROM coll [AS] o`, `FROM "a.b"` para nombres con punto. Si `a.b` sin comillas puede ser base y colección o una colección con punto, se rechaza como ambiguo.
- `SELECT` de rutas y alias, y `*` (documentos completos aplanados como en la rejilla). Se proyecta siempre con nombres sintéticos y `$literal` (`{c0: '$address.city', c1: {$literal: 1}}`) y las filas se construyen por nombre en el orden de `Translation.columns`, no en el que devuelve el servidor.
- `WHERE` con `= <> < <= > >=` contra literales, `AND`/`OR`/`NOT`, `IN`/`NOT IN` de literales, `BETWEEN`, `IS [NOT] NULL`, `missing(path)` y `LIKE`/`ILIKE` (con `ESCAPE`).
- `ORDER BY`, `LIMIT`, `OFFSET`; `GROUP BY` con `COUNT(*)`, `COUNT(path)`, `SUM`, `AVG`, `MIN`, `MAX` y `HAVING`.
- Literales con la gramática del spec como funciones: `ObjectId('…')`, `ISODate('…')`, `NumberLong(…)`, `NumberDecimal('…')`; `DECIMAL '9.99'` da Decimal128 y `TIMESTAMP '…'` da Date (documentado: no es el tipo BSON Timestamp).
- El AST se valida con una **lista permitida**, no con `Statement::Query`: una `Query` de sqlparser puede llevar escrituras (`SetExpr::Insert/Update/Delete/Merge`, `SELECT … INTO`, CTE, `FOR UPDATE`), como ya hace `crates/files/src/sql.rs`. Solo pasa `SetExpr::Select` sin `into`, sin `with` y sin locks.

### Semántica

- **Lógica de tres valores**: cada predicado se compila a dos filtros, `TRUE(p)` y `FALSE(p)`; lo desconocido no cumple ninguno. `NOT`, `AND` y `OR` se combinan sobre esos pares, porque `$ne`, `$nin`, `$not` y `$nor` de MongoDB incluyen ausentes y null: `a <> 5` es `{a: {$nin: [5, null]}}` más las guardas, no `{a: {$ne: 5}}`.
- **Null y ausente**: `IS NULL` es null o ausente, como `{a: null}`; `missing(a)` es `$exists: false`. `= NULL` y `<> NULL` se rechazan ("usa IS NULL"). `IN` quita los NULL de la lista y `NOT IN` con un NULL no coincide nunca. `GROUP BY` junta null y ausente con `$ifNull`.
- **Arrays**: un predicado escalar lleva `{$not: {$type: "array"}}` sobre **cada prefijo** de la ruta (`a` y `a.b`), porque `{"a.b": 5}` coincide atravesando un array intermedio. Solo `_id` cuenta como escalar demostrado; ni la muestra ni un validador `$jsonSchema` (no es retroactivo) quitan guardas. Los segmentos numéricos (`a.0`) se rechazan.
- **Tipos y orden**: igualdad entre tipos distintos es falsa; `<`, `>` y `BETWEEN` entre tipos distintos son desconocidos, como los corchetes de tipo de `$gt`/`$lt`. Se ordena como MongoDB: null y ausente primero en ASC, según la colación de la colección, que se lee de `CollectionInfo.options` y se anota.
- **LIKE**: `\A…\z` con la opción `s` y los metacaracteres escapados; `ILIKE` añade `i`. Si la colección tiene colación, se avisa de que `=` la usa y `LIKE` no.
- **Agregados**: `COUNT(path)` cuenta lo que no es null ni ausente; `SUM` devuelve NULL si no hubo ningún número (MongoDB daría 0).
- **Nombres**: los identificadores distinguen mayúsculas siempre. `"paid"` es un campo en SQL: si no existe en la muestra, el error dice "para texto usa 'paid'". Las palabras reservadas usadas como campo (`order`, `group`, `user`, `date`…) se entrecomillan en el completado y en la pista del error.
- **La muestra no bloquea**: los rechazos solo salen del texto y de `Capabilities`. Lo que sale de la muestra o del validador son avisos con "Ejecutar igualmente": "`_id` parece ObjectId: ¿ObjectId('…')?", "`tags` es un array en el 38 % de la muestra".

### Rechazos v1

Escrituras y DDL ("El SQL de Kuery sobre MongoDB solo lee: cambia a mongosh o edita en la rejilla"), `BEGIN`/`COMMIT`, joins, `UNNEST`, `DISTINCT`, subconsultas, CTE, `UNION`/`INTERSECT`/`EXCEPT`, funciones de ventana, funciones en `WHERE`, comparaciones campo-campo, `COLLATE` y `NULLS FIRST/LAST` contrarios al orden de MongoDB. Cada rechazo lleva su span como diagnóstico y, si hay una traducción parcial, "Abrir en mongosh".

### Seguridad

- El traductor v1 solo produce `aggregate` sin `$out`/`$merge` y sin JS de servidor. Si el clasificador mongosh no ve una lectura en la traducción, se rechaza como fallo de Kuery y no se ejecuta.
- El texto nativo pasa por las mismas capas que el escrito a mano: el clasificador de `DocumentSession` y `wire::guard`. Las garantías no dependen del front end SQL.
- **Recorridos completos**: con los índices de `CollectionInfo`, si ningún predicado ni orden puede usar un índice y no hay `LIMIT`, la tira anota "recorrerá toda la colección", y en Production se pide confirmación. `COUNT(*)` sin `WHERE` ofrece `estimatedDocumentCount`. `maxTimeMS` se aplica siempre.
- `AuditRecord` no cambia (solo lecturas); el historial guarda el SQL de origen y el nativo.

### Vista y edición

- **Vista por defecto según el lenguaje**: SQL abre en Tabla, mongosh en Documentos. Un conmutador cambia de vista sin volver a ejecutar y se recuerda por pestaña (`TabState::DocumentConsole { view }`).
- **Lo que se puede editar depende de la procedencia, no del lenguaje.** En la v1 los resultados SQL son de solo lectura. Editar desde filas SQL que salen 1:1 de un documento con `_id` es trabajo futuro (ver abajo).

### Futuro, solo con demanda

1. Editar desde filas SQL: `_id` como columna oculta; cada celda se decide mirando el `Document` real (ningún prefijo de la ruta es un array); el cambio es un `DocChange` tipado desde el documento original; un solo conjunto de pendientes por resultado.
2. `DISTINCT` (como `$group`), `x = ANY(arr)` (`$elemMatch`), `UNNEST … WITH ORDINALITY` (`$unwind`) y `[INNER|LEFT] JOIN` por igualdad de una clave (`$lookup` con guardas de null, ausente y array en ambos lados).
3. Filtro SQL en la barra de la pestaña Colección.
4. Escrituras SQL (`UPDATE … SET literal | a ± n`, `DELETE`, `INSERT VALUES`) por la revisión de escrituras normal, si el dueño lo decide.
5. Motores con un lenguaje nativo parecido a SQL (PartiQL, SQL++, Cosmos SQL, Elasticsearch SQL) no traducen: su `Language` ya es ese, con su propio analizador, porque sqlparser no tiene dialecto para ninguno.

## Pruebas

- **Unitarias, sin servidor**:
  - la identidad imprimir/analizar para todo tipo BSON (prueba de propiedad);
  - casos del parser y spans de error;
  - la tabla del clasificador, con todos los comandos (incluido `$out` dentro de `$facet`/`$lookup` y lo desconocido → Write);
  - `wire::guard`;
  - `diff` → `$set`/`$unset`/`expect`;
  - `flatten` (unión, orden, ausente frente a null) e `infer`;
  - `plan/mongo.rs` sobre fixtures de 7.0, 8.0 y sharded;
  - `FakeDocumentAdapter` sobre el worker: rechazo de solo lectura, rechazo de heavy, mismatch de `apply` por tipo, script que se detiene, pánico, cancelación y lector que se va.
- **`document_conformance`**, filtrada por `Capabilities`: lee y hace streaming; rechaza escrituras dos veces; aplica cambios comprobando cada uno; transacción todo o nada; vista previa sin cambiar datos; lee su catálogo; muestrea; cancela una operación lenta (comando `sleep`); conserva los tipos `NumberLong`, `Decimal128` y Date; informa del usuario de solo lectura.
- **Docker** (`dev/compose.yaml`):
  - `mongo:8.0` como replica set de un nodo `rs0` en **27019**, con `enableTestCommands=1` y autenticación activa (un keyfile generado en el entrypoint), para que la detección de solo lectura se pueda probar con un usuario `kuery_ro`;
  - `mongo-standalone` bajo un perfil, sin transacciones ni autenticación;
  - `dev/seed-mongo.js` con una tienda heterogénea: ObjectId y claves de texto, Decimal128, fechas, arrays anidados, un campo de tipo mixto, una vista, una time series y un validador.
  - Las pruebas de integración llevan `#[ignore = "needs the dev MongoDB"]` con `KUERY_MONGO_URI`, como en mssql.
- **SQL** (sin servidor): la tabla de traducciones SQL → `aggregate`; la tabla de rechazos, incluidas las escrituras escondidas en una `Query` (`SELECT … INTO`, `SetExpr::Insert`); `parse(render(op)) == op` para todo `Op` que produzca el traductor; propiedad "toda traducción se clasifica como Read y `wire::guard` la acepta".
- **SQL** (contra el servidor): cada consulta SQL de las fixtures se compara con su mongosh escrito a mano sobre el contenedor de 27019, con null y ausente, arrays (también intermedios), tipos mezclados, `NULL` dentro de `IN`, `LIKE` con saltos de línea y no ASCII, colación y `Decimal128` frente a Double. Las mismas fixtures generan los ejemplos de la documentación.
- **CI**: un job ejecuta la conformidad de Mongo, y una comprobación verifica que `cargo tree -i aws-lc-rs` solo muestra la ruta existente de tiberius.

## Entrega por fases

Cada fase es un PR, y el título de cada commit es la línea del changelog.

1. **Spec y spikes**:
   - Este documento.
   - Spikes:
     - features de `mongodb` 3.9 y la comprobación de solo ring;
     - `killOp` por comentario sobre la tarea lanzada en 8.0;
     - si el explain de update/delete devuelve `nMatched`;
     - TLS a través del túnel;
     - la detección de privilegios con y sin autenticación.
   - Commits: `docs: design MongoDB support and the document database family`.
2. **Worker compartido**, sin cambio de comportamiento. Commit: `refactor(core): run sessions on a worker any driver contract can share`.
3. **Resumen de conexión y revisión compartida**: `Tab::connection()`, `write_review.rs` y `Scope::NoTransactions`. Redis se comporta igual que antes. Commit: `refactor(app): share the connection status and write review between families`.
4. **Contrato y MongoDB: conectar y navegar en solo lectura** (contrato y primer motor en un mismo PR, para que la primera línea `feat` sea visible):
   - `core/src/document/`, `Family::Document`, `DriverInfo.options`;
   - `crates/mongo` con runtime, detección, guardia, árbol, pestaña Colección, rejilla aplanada, `extend_columns`, visor, exportación NDJSON y restauración de pestañas;
   - formulario con opciones y pegado de URI, reglas de túnel, servicios de Docker y CI;
   - `[[package]]` y `changelog_include` en `release-plz.toml`, scope `mongo` en `AGENTS.md`.
   - Commits: `refactor(core): add the document database contract, read-only by default`; `feat: connect to MongoDB and browse its databases and collections`; `chore: add a seeded MongoDB replica set to the dev services`; `docs: add the mongo scope to the agent instructions`.
5. **Consola de lectura**: parser, clasificador, diagnósticos, ejecutar la sentencia bajo el cursor o todas, cancelación, completado, key context propio, `tree-sitter-javascript`, consola por defecto al conectar. Commit: `feat: query MongoDB in a console with completion`.
6. **Escrituras desde la consola**: conmutador Write, desbloqueo, vista previa, peligros, transacción por script o parada en el primer fallo, auditoría. Commit: `feat: write to MongoDB from the console with review and audit`.
7. **Editar documentos**: Editar, Insertar, Duplicar y Borrar, diff tipado, comprobación optimista, aplicación atómica donde se pueda. Commit: `feat: edit, insert and delete MongoDB documents with review`.
8. **Esquema, índices y planes**. Commit: `feat: show MongoDB schemas, indexes and query plans`.
9. **SQL de solo lectura**: spike de un día con `mongodb/mongosql` y con sqlparser 0.63 (rutas `o.a.b`, palabras reservadas como campo, spans); traductor en `crates/mongo/src/sql/`, `QueryFrontend` en el driver, selector `mongosh | SQL` en la Consola, tira Traducción, conmutador Tabla/Documentos y conformidad SQL. Commit: `feat: query MongoDB with read-only SQL and see the native query it runs`.
10. **IDs opacos de actividad**. Commit: `refactor(core): identify server sessions by an opaque id`.
11. **Actividad de MongoDB**. Commit: `feat: watch and stop running MongoDB operations`.
12. **Hueco de Redis**, independiente de las demás fases: solo lectura estructural en el driver para conexiones de producción. Commit: `fix(redis): refuse writes on production connections in the driver too`.
13. **Comprobación con un segundo motor (no se fusiona)**: un adaptador desechable de CouchDB o SurrealDB contra `document_conformance`. Las correcciones al contrato entran como `refactor(core): fit the document contract to a second engine`.
14. **Documentación**: README, sitio, nota de solo lectura, CA por archivo, límites del túnel y una página del SQL de Kuery para documentos (semántica, rechazos y ejemplos generados desde las fixtures). Commit: `docs: add MongoDB to the README and the site`.

## Alternativas consideradas

- **Mongo concreto primero, extraer el trait con el segundo motor** (estilo Redis): sería lo más rápido y sigue un precedente, pero añade unas 40 ramas con nombre de Mongo que luego habría que renombrar. También reimplementaría fuera de core la cancelación por número de ejecución y los timeouts, sin conformidad compartida, y si el segundo motor nunca llega queda otra familia hecha a medida. Se adoptan de esta alternativa las reglas de extracción y de salida, la prueba de identidad de literales y el puerto 27019.
- **Modelo de capacidades con cinco refactors previos** (`Tab::Dyn` con entidades borradas de tipo, navegador genérico que migra Redis, cierres de construcción en eventos): es el más completo, pero el que más radio de impacto tiene y el que más tarda en entregar valor, e introduce idiomas ajenos a un código que usa enums explícitos. Se toman de él `extend_columns`, el spawn sin descartar futuros, el key context propio, la detección recursiva y `Ext.raw`.
- **Un trait único para SQL y documentos**: transacciones, savepoints, texto de sentencias y filas frente a documentos son demasiado distintos. Solo se comparte el worker.
- **Motor JS incrustado (boa, QuickJS) u `oxc_parser`**: el primero impide clasificar. El segundo es una dependencia pesada que da peores mensajes para un subconjunto pequeño; el parser propio son unas mil líneas.
- **SQL como segundo `Language`**: obligaría a SQL a rellenar `parse_document`, `render` y el completado de documentos con valores falsos. Se usa un `QueryFrontend` aparte.
- **Traducir a la operación más simple (`find`, `countDocuments`, `distinct`)**: tres semánticas distintas que probar, y `distinct` no equivale a `DISTINCT`. La v1 siempre baja a `aggregate`.
- **Vista por defecto según la forma del resultado**: la heurística de la última etapa es frágil y contradice lo esperado en `SELECT *`. La decide el lenguaje, con un conmutador.
- **SQL evaluado en el cliente (GlueSQL, Polars de `crates/files`)**: recorre colecciones enteras y no admite colecciones heterogéneas. Todo filtro, orden y límite se envía al servidor o se rechaza.
- **Consola solo JSON (documentos de comando)**: es segura, pero nadie la escribe así. Su sintaxis queda cubierta como modo dentro de la misma gramática.
- **Aplanar con Polars (`crates/files`)**: rechaza colecciones heterogéneas ("value conflicts with the inferred schema").
- **Opciones en una URI sin contraseña dentro del campo host**: no exige cambiar el esquema, pero se descubre peor y obliga a validar la URI para que nunca entre una contraseña. Queda como pregunta.
- **`select!` que descarta el futuro (como mssql)**: contradice la recomendación del driver `mongodb`. Se usa spawn más espera del `JoinHandle`.

## Preguntas abiertas para el dueño

1. ¿Te comprometes a un segundo motor que no hable el protocolo de MongoDB (CouchDB o SurrealDB) dentro del horizonte del roadmap? Si no, el camino concreto cuesta menos y el contrato podría no validarse nunca.
2. ¿Aceptas que los refactors de las fases 2 y 3 retrasen la primera versión visible de MongoDB?
3. Concurrencia: ¿una sesión de catálogo más una sesión por pestaña, compartiendo el `Client` (lo propuesto), o una sola sesión por conexión como en SQL?
4. ¿Quieres un modo de escritura transaccional en replica sets (Commit/Rollback con la cuenta atrás de 60 s), o basta con la atomicidad por script y por lote?
5. Opciones de conexión: ¿campos en el formulario (`DriverInfo.options` y `ConnectionSpec.options`) o una URI sin contraseña en el host? Cambia el formato de `project.toml`.
6. ¿Es aceptable añadir `tree-sitter-javascript` a gpui-kit `=0.7.3` (tamaño del binario y tiempo de compilación), o empezamos con resaltado JSON?
7. En producción, cuando el usuario de Mongo puede escribir y hay autenticación, ¿basta con el aviso, se exige un reconocimiento explícito o se rechaza la conexión?
8. ¿Debe Redis pasar algún día a las capas compartidas (pestañas, árbol), o se queda como está salvo la revisión de escrituras ya extraída?
9. SQL sobre documentos: ¿se queda en solo lectura, o se planifica más adelante `UPDATE`/`DELETE` en SQL por la revisión de escrituras normal?

## Resultados de los spikes

Hechos el 2026-10-28 contra los servicios de desarrollo (`mongo`: `mongo:8.0`, replica set `rs0` con autenticación en el puerto 27019; `mongo-standalone`: sin replica set ni autenticación en el 27029) con `mongodb` 3.9.1 (`compat-3-3-0`, `bson-3`, `rustls-tls`, `dns-resolver`). El código del spike era desechable y no se conserva.

### Dependencias: solo ring

- `cargo tree -p kuery-mongo -e normal -i aws-lc-rs` no encuentra el paquete, y `cargo tree -i openssl-sys` tampoco en todo el workspace: `mongodb` con `rustls-tls` solo trae `ring` y `webpki-roots`.
- En el árbol del workspace, `cargo tree -i aws-lc-rs -e normal` sí lista `mongodb`, pero solo porque comparte el crate `rustls`, al que la ruta existente de tiberius (`tokio-rustls` con sus features por defecto) le activa `aws_lc_rs` por unificación de features. `mongodb` no activa ninguna feature de aws-lc. La comprobación de CI tiene que mirar quién activa la feature (`cargo tree -e features -i rustls`) o el árbol de `kuery-mongo` solo, no buscar `mongodb` en el árbol inverso.

### Cancelar por etiqueta de comentario

Cada operación se lanzó con `runtime().spawn` y se esperó su `JoinHandle` (con un `timeout` sobre `&mut handle`, que no suelta la tarea). A los 300 ms, `$currentOp` filtrado por `command.comment` encontró la operación y `killOp` la paró:

| Operación | Encontrada como | Termina tras `killOp` | Error |
| --- | --- | --- | --- |
| `find` en `big` con `$where: "sleep(5) \|\| true"`, matada desde otro `Client` | `op: query`, `find` | 71 ms | 11601 `Interrupted` |
| El mismo `find` con `sleep(100)`, matado desde el mismo `Client` | `op: query`, `find` | 19 ms | 11601 `Interrupted` |
| `find` con `batchSize: 10`, matado durante un `getMore` | `op: getmore`, con `cursorId` | inmediato | 11601 `Interrupted` |
| `{sleep: 1, secs: 20, lock: "none", comment}` | `op: command`, `sleep` | inmediato | 11601 `Interrupted` |
| `aggregate` con `$function` que duerme | `op: command`, `aggregate` | 3 ms | 11601 `Interrupted` ("JavaScript execution interrupted") |
| `find` dentro de una transacción | `op: query`, `find` | 10 ms | 11601; después `abortTransaction` funciona |

- En todos los casos, un `$currentOp` posterior con `idleCursors: true` no encuentra nada con la etiqueta: ni la operación ni el cursor quedan vivos, también cuando se mata un `getMore`.
- El driver envía el `comment` también en cada `getMore` (servidor 4.4 o posterior), así que la etiqueta sirve durante toda la vida del cursor, no solo en el primer lote.
- Matar desde el mismo `Client` funciona: el pool da otra conexión para `$currentOp` y `killOp`. No hace falta un segundo cliente.
- Ambos clientes siguen respondiendo a `ping` después de cada cancelación.
- **Se puede parar una operación sin soltar su futuro**: el diseño de "spawn, `killOp` por etiqueta y seguir esperando" se mantiene. El margen de 5 s del plan es holgado; lo observado está por debajo de 100 ms.
- **Corrección al plan**: `$currentOp { ownOps: true }` no existe como etapa de agregación (error 40415, "unknown field"). Las operaciones propias se piden con `$ownOps` en el comando antiguo `currentOp`. En la etapa, las operaciones propias son el valor por defecto (`allUsers: false`). La prueba de `activity`/`kill` de la Tarea 10 debe ser `$currentOp: {}`.
- Un usuario con solo el rol `read` puede ejecutar `$currentOp` sobre sus propias operaciones y matarlas con `killOp` sin el privilegio `killop` (probado con `kuery_ro`: 11601 en 6 ms). Con `allUsers: true` recibe 13 `Unauthorized`. La cancelación de Kuery funciona, por tanto, con usuarios de solo lectura; la vista de actividad de todos los usuarios necesita `inprog`.

### Explain de escrituras

Con el comando `explain` sobre `update` y `delete` (mongosh no ofrece `explain().updateMany`; el comando es lo que Kuery enviará):

- `queryPlanner` devuelve el plan (`UPDATE`/`FETCH`, `BATCHED_DELETE`/`COLLSCAN`) sin ningún recuento.
- `executionStats` devuelve en `executionStages` `nMatched`, `nWouldModify` y `nWouldUpsert` para `update` (también con `multi: false`, con pipeline de actualización y con `upsert: true`: `nWouldUpsert: 1`), `nWouldDelete` para `delete`, y lo mismo para `findAndModify`. Además da `totalDocsExamined` y `totalKeysExamined`.
- No escribe nada: los recuentos antes y después son iguales (`flag: true` sigue en 0 y `big` en 20 000). El explain de un `aggregate` con `$out` tampoco crea la colección.
- `nWouldModify` no detecta las actualizaciones que no cambian nada: `$set: {status: "paid"}` sobre los 125 pedidos pagados da `nWouldModify: 125`, mientras que la escritura real daría `modifiedCount: 0`.
- Con `maxTimeMS` el explain no falla: devuelve `ok: 1` con `executionSuccess: false`, `errorCode: 50` y recuentos parciales (`nWouldDelete: 7` de 2 000). Hay que leer `executionSuccess`; los recuentos parciales son un mínimo.
- Un usuario sin privilegio de escritura recibe 13 `Unauthorized` al pedir el explain de una escritura, incluso en `queryPlanner`.
- **Decisión**: donde no hay transacciones, `Preview::Estimated` usa `explain` con `executionStats` y `maxTimeMS`: `nMatched`/`nWouldModify`/`nWouldUpsert`/`nWouldDelete` son exactos para el filtro en ese instante, `executionSuccess: false` da `Count::AtLeast`, y cubre upserts, pipelines y `findAndModify`, que un `countDocuments` no distingue. Si el explain se rechaza (sin privilegio, o en motores compatibles que no lo implementan para escrituras), se usa `countDocuments(filter, {limit: 10001, maxTimeMS})`. Los primeros 20 documentos siguen saliendo de un `find` con el mismo filtro. La revisión dice "Estimada" en ambos casos, porque los datos pueden cambiar antes de escribir y porque `nWouldModify` cuenta también lo que no cambiaría.

### TLS a través del túnel

Montaje: un `mongod` con `--tlsMode requireTLS` y un certificado firmado por una CA propia para el nombre `mongo-tls`, en la red de `bastion`, alcanzado con el `ssh` del sistema como hace `app/src/tunnel.rs`.

- Por el extremo local de `ssh -L` (`127.0.0.1:<puerto>`) con `tls=true` y la CA: falla la selección de servidor con `invalid peer certificate: certificate not valid for name "127.0.0.1"; certificate is only valid for DnsName("mongo-tls")`.
- `mongodb` 3.9 con `rustls-tls` no ofrece forma de verificar contra otro nombre: `TlsOptions` solo tiene `allow_invalid_certificates`, `ca_file_path` y `cert_key_file_path`, `allow_invalid_hostnames` existe solo con `openssl-tls`, y no se puede pasar un `ClientConfig` ni un resolvedor propio para hosts que no son SRV. Con `allow_invalid_certificates` conecta, pero sin comprobar ni el nombre ni la cadena.
- **Lo que sí funciona**: la feature `socks5-proxy` de `mongodb` (solo añade `fast-socks5`, sin aws-lc ni openssl) con `ssh -D` (reenvío dinámico, que es un proxy SOCKS5). El cliente se conecta a `mongo-tls:27017`, el nombre tal cual, a través del proxy; el nombre se resuelve en el bastión y rustls verifica el certificado contra `mongo-tls` con la CA. `ping` respondió bien.
- **Decisión para la Tarea 12**: con TLS activo y túnel SSH, Kuery abre el túnel como reenvío dinámico (`ssh -D` en un puerto local, con las mismas opciones de `tunnel.rs`) y conecta con `socks5_proxy` al servidor tal como se llama, verificando su certificado. Hay que añadir `socks5-proxy` a las features de `mongodb`. Sin TLS se mantiene `ssh -L`. Nunca se activa `tlsAllowInvalidCertificates` por estar en un túnel; esa opción sigue siendo una elección explícita del usuario fuera de producción. Si la Tarea 12 no pudiera llevar el reenvío dinámico, la alternativa es rechazar TLS por túnel con el mensaje "Through SSH with TLS, Kuery cannot check the server's certificate", nunca conectar sin verificar.
- Por SOCKS, el descubrimiento de miembros y SRV pasarían también por el túnel, porque el driver se conecta a cada miembro por su nombre a través del proxy. La regla de un solo miembro por SSH se mantiene por ahora, porque es decisión del dueño, pero esta vía permitiría relajarla.

### Privilegios

`connectionStatus { showPrivileges: true }` en `admin`:

| Conexión | `authenticatedUsers` | Privilegios | Acciones | `insertOne` |
| --- | --- | --- | --- | --- |
| `kuery` (`root`) | `kuery@admin` | 26 | 137, entre ellas `insert`, `update`, `remove`, `dropDatabase`, `createUser`, `killop`, `inprog` | escribe |
| `kuery_ro` (`read` en `shop`) | `kuery_ro@shop` | 2 | `changeStream collStats dbHash dbStats find killCursors listCollections listIndexes listSearchIndexes planCacheRead` | 13 `Unauthorized` |
| `mongo-standalone` sin autenticación | vacío | 0 | ninguna | escribe |
| replica set con autenticación, sin usuario | vacío | 0 | ninguna | 13 `Unauthorized` ("requires authentication"), igual que `listDatabases` |

- Confirmado el riesgo de la "Revisión 2": sin autenticación, las listas vienen vacías aunque cualquiera puede escribir. Vacío nunca significa "solo lectura".
- Una conexión sin usuario a un servidor con autenticación devuelve exactamente lo mismo en `connectionStatus`. Para no decir "este servidor no tiene autenticación" cuando sí la tiene, la Tarea 10 distingue los dos casos con `listDatabases { nameOnly: true }`: si responde, no hay autenticación; si falla con 13, la conexión simplemente no tiene usuario. En ambos casos es `ClientOnly`.
- Acciones de los roles integrados de lectura, tomadas del servidor con usuarios temporales: `readAnyDatabase` añade `listDatabases` a las de `read`; `clusterMonitor` da 38. `readWrite` añade, entre otras, `insert`, `update`, `remove`, `createCollection`, `createIndex`, `dropCollection`, `dropIndex`, `renameCollectionSameDB`, `convertToCapped` y las de índices de búsqueda.
- **Lista de acciones de lectura** que exige `Enforcement::Server` (la unión de `read`, `readAnyDatabase` y `clusterMonitor`): `allCollectionStats`, `changeStream`, `collStats`, `connPoolStats`, `dbHash`, `dbStats`, `find`, `getCmdLineOpts`, `getDatabaseVersion`, `getDefaultRWConcern`, `getLog`, `getParameter`, `getShardMap`, `getShardVersion`, `hostInfo`, `indexStats`, `inprog`, `killCursors`, `listClusterCatalog`, `listCollections`, `listDatabases`, `listIndexes`, `listSampledQueries`, `listSearchIndexes`, `listSessions`, `listShards`, `netstat`, `operationMetrics`, `planCacheRead`, `queryStatsRead`, `queryStatsReadTransformed`, `replSetGetConfig`, `replSetGetStatus`, `serverStatus`, `shardedDataDistribution`, `shardingState`, `top` y `useUUID` (este último, de `clusterMonitor`, no permite escribir sin `applyOps`). Cualquier acción fuera de la lista, incluidas las que traigan versiones futuras, cuenta como escritura y da `ClientOnly`: en la duda se avisa.
