# dbadmin

Gestor de bases de datos de escritorio (macOS, Windows y Linux) con gpui.

Estado: prueba técnica. Barra de título propia, editor SQL, tabla virtualizada
con resultados en streaming y ejecución siempre en solo lectura.

## Estructura

| Crate | Qué hace |
|---|---|
| `crates/core` | Política de solo lectura (`sqlparser`) y resultados columnares (Arrow). Sin interfaz. |
| `crates/pg` | Driver de PostgreSQL en un runtime de tokio propio: streaming por lotes, `BEGIN TRANSACTION READ ONLY`, tiempo máximo, cancelación y TLS con rustls. |
| `crates/app` | La app gpui (`gpui-kit`, versión fijada). Fuentes e iconos propios en `crates/app/assets`. |

## Desarrollo

```bash
docker compose -p dbadmin-dev -f dev/compose.yaml up -d   # Postgres con 1 000 000 de pedidos
cargo run -p dbadmin                                      # la app
cargo test --workspace                                    # tests sin base de datos
cargo test -p dbadmin-pg -- --ignored                     # tests contra el Postgres de desarrollo
cargo run --release -p dbadmin-pg --example stream_bench  # streaming sin interfaz
```

`DBADMIN_DATABASE_URL` cambia la base de datos (ver `.env.example`).

## TLS

El `sslmode` de la URL sigue la semántica de libpq (`disable`, `allow`, `prefer`,
`require`, `verify-ca`, `verify-full`), con `sslrootcert` para un fichero PEM de
certificados raíz; sin él se usan los del sistema. Sin `sslmode`, una base de
datos local (`localhost`, `127.0.0.1`, `::1` o socket Unix) va sin TLS y el resto
usa `prefer`. Detalles en `crates/pg/src/tls.rs`.

Los tests de TLS necesitan un Postgres con `ssl = on`; si
`DBADMIN_TLS_DATABASE_URL` no está definida, no hacen nada:

```bash
docker run -d --rm --name dbadmin-tls -p 54330:5432 \
  -e POSTGRES_USER=dbadmin -e POSTGRES_PASSWORD=dbadmin -e POSTGRES_DB=dbadmin \
  postgres:16 -c ssl=on \
  -c ssl_cert_file=/etc/ssl/certs/ssl-cert-snakeoil.pem \
  -c ssl_key_file=/etc/ssl/private/ssl-cert-snakeoil.key
DBADMIN_TLS_DATABASE_URL=postgres://dbadmin:dbadmin@localhost:54330/dbadmin \
  cargo test -p dbadmin-pg -- --ignored
```

## Fuentes e iconos

La interfaz usa IBM Plex Sans y el editor y la tabla JetBrains Mono, ambas con
licencia SIL Open Font License 1.1 (`crates/app/assets/fonts/*/OFL.txt`). Van
embebidas en el binario y se registran en el text system de gpui al arrancar.
Los iconos que faltan en el set por defecto de gpui-kit (candado, base de datos,
tabla) son de Lucide (ISC, `crates/app/assets/icons/LICENSE-LUCIDE`) y los sirve
`AppAssets`, que delega el resto en `gpui_kit::assets::Assets`.

## CI

`.github/workflows/ci.yml` pasa `cargo fmt`, `clippy -D warnings` y
`cargo test --workspace` en macOS, Windows y Linux, y los tests ignorados de
`dbadmin-pg` contra `postgres:16` (y un segundo Postgres con TLS) en Linux.
El toolchain de Rust está fijado en `rust-toolchain.toml` (gpui usa APIs estabilizadas después de Rust 1.93).
