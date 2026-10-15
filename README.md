# dbadmin

Gestor de bases de datos de escritorio (macOS, Windows y Linux) con gpui.

Estado: prueba técnica. Barra de título propia, editor SQL, tabla virtualizada
con resultados en streaming y ejecución siempre en solo lectura.

## Estructura

| Crate | Qué hace |
|---|---|
| `crates/core` | Política de solo lectura (`sqlparser`) y resultados columnares (Arrow). Sin interfaz. |
| `crates/pg` | Driver de PostgreSQL en un runtime de tokio propio: streaming por lotes, `BEGIN TRANSACTION READ ONLY`, tiempo máximo y cancelación. |
| `crates/app` | La app gpui (`gpui-kit`, versión fijada). |

## Desarrollo

```bash
docker compose -p dbadmin-dev -f dev/compose.yaml up -d   # Postgres con 1 000 000 de pedidos
cargo run -p dbadmin                                      # la app
cargo test --workspace                                    # tests sin base de datos
cargo test -p dbadmin-pg -- --ignored                     # tests contra el Postgres de desarrollo
cargo run --release -p dbadmin-pg --example stream_bench  # streaming sin interfaz
```

`DBADMIN_DATABASE_URL` cambia la base de datos (ver `.env.example`).
El toolchain de Rust está fijado en `rust-toolchain.toml` (gpui usa APIs estabilizadas después de Rust 1.93).
