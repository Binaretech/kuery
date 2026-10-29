//! `mongodb://` and `mongodb+srv://` connection strings, read into the
//! connection form's fields. The password comes out on its own (the app
//! keeps it in the keychain), and only the options Kuery keeps
//! ([`OPTIONS`]) survive: any other is refused by name (never with its
//! value, which may be a secret), so nothing a pasted string carries
//! reaches the project file unseen.

use std::collections::BTreeMap;

use kuery_core::driver::{OptionField, OptionKind, ParsedUri};

/// The options a MongoDB connection keeps, besides host, database, user and
/// TLS, as the connection form asks for them. `srv` stands for the
/// `mongodb+srv://` scheme.
pub const OPTIONS: &[OptionField] = &[
    OptionField {
        key: "srv",
        label: "SRV",
        kind: OptionKind::Toggle,
        placeholder: "",
        help: Some("The host names a cluster whose members come from DNS, as Atlas does."),
        production: true,
    },
    OptionField {
        key: "authSource",
        label: "Auth database",
        kind: OptionKind::Text,
        placeholder: "admin",
        help: None,
        production: true,
    },
    OptionField {
        key: "authMechanism",
        label: "Mechanism",
        kind: OptionKind::Choice(MECHANISMS),
        placeholder: "",
        help: None,
        production: true,
    },
    OptionField {
        key: "replicaSet",
        label: "Replica set",
        kind: OptionKind::Text,
        placeholder: "rs0",
        help: None,
        production: true,
    },
    OptionField {
        key: "directConnection",
        label: "Direct",
        kind: OptionKind::Choice(&["true", "false"]),
        placeholder: "",
        help: Some(
            "By default, straight to a single host without a replica set, else to the members it lists.",
        ),
        production: true,
    },
    OptionField {
        key: "readPreference",
        label: "Read preference",
        kind: OptionKind::Choice(&[
            "primary",
            "primaryPreferred",
            "secondary",
            "secondaryPreferred",
            "nearest",
        ]),
        placeholder: "",
        help: Some("Where reads go. It is not a protection: writes still reach the primary."),
        production: true,
    },
    OptionField {
        key: "tlsCAFile",
        label: "CA file",
        kind: OptionKind::File,
        placeholder: "/path/to/ca.pem",
        help: Some("For a private CA, such as Amazon DocumentDB's bundle."),
        production: true,
    },
    OptionField {
        key: "tlsCertificateKeyFile",
        label: "Client certificate",
        kind: OptionKind::File,
        placeholder: "/path/to/client.pem",
        help: Some("The certificate and its key in one file, for X.509."),
        production: true,
    },
    OptionField {
        key: "tlsAllowInvalidCertificates",
        label: "Invalid certificates",
        kind: OptionKind::Toggle,
        placeholder: "",
        help: Some("Connects without checking the server's certificate. Only outside production."),
        production: false,
    },
];

/// Why a string whose user or password holds a bare `/` is refused.
const UNESCAPED: &str =
    "The connection string has an @ after its first /: a / in the user or password is written %2F";

/// The authentication mechanisms Kuery connects with.
const MECHANISMS: &[&str] = &["SCRAM-SHA-256", "SCRAM-SHA-1", "MONGODB-X509"];

/// Options that are the client's defaults anyway: dropped without a word.
const DEFAULTS: &[(&str, Option<&str>)] = &[
    ("retryWrites", None),
    ("w", Some("majority")),
    ("appName", None),
];

/// Options that hold secrets, which never go in the project file.
const SECRETS: &[&str] = &["tlsCertificateKeyFilePassword", "sslPEMKeyPassword"];

/// Whether an option holds `true` or `false`, which MongoDB reads in any
/// case.
fn boolean(field: &OptionField) -> bool {
    matches!(field.kind, OptionKind::Toggle) || field.key == "directConnection"
}

/// Reads a connection string. Option names are matched ignoring case, as
/// MongoDB does, and kept in their canonical spelling.
pub fn parse(uri: &str) -> Result<ParsedUri, String> {
    let uri = uri.trim();
    let (srv, rest) = if let Some(rest) = strip_prefix_ignoring_case(uri, "mongodb+srv://") {
        (true, rest)
    } else if let Some(rest) = strip_prefix_ignoring_case(uri, "mongodb://") {
        (false, rest)
    } else {
        return Err("A MongoDB connection string starts with mongodb:// or mongodb+srv://".into());
    };
    let (address, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, database) = address.split_once('/').unwrap_or((address, ""));
    let (credentials, hosts) = match authority.rsplit_once('@') {
        Some((credentials, hosts)) => (Some(credentials), hosts),
        None => (None, authority),
    };
    // A `/` the user or password did not escape ends the hosts early, and
    // the rest of the password would pass for the database. Neither is
    // repeated in the message.
    if database.contains('@') {
        return Err(UNESCAPED.into());
    }
    let (user, password) = match credentials {
        Some(credentials) => match credentials.split_once(':') {
            Some((user, password)) => (decode(user)?, Some(decode(password)?)),
            None => (decode(credentials)?, None),
        },
        None => (String::new(), None),
    };
    if hosts.trim().is_empty() {
        return Err("The connection string names no host".into());
    }
    let (host, port) = match hosts.contains(',') {
        true => (hosts.to_string(), None),
        false => {
            let (host, port) = split_port(hosts)?;
            (host.to_string(), port)
        }
    };
    if srv && (port.is_some() || hosts.contains(',')) {
        return Err("A mongodb+srv:// string names one host, without a port".into());
    }

    let database = decode(database)?;
    if database.contains(['/', '\\', '.', ' ', '"', '$']) {
        return Err(
            "A database name has no /, \\, ., space, \" or $: a / in the user or password is written %2F".into(),
        );
    }
    let mut parsed = ParsedUri {
        host,
        port,
        database,
        user,
        password: password.filter(|p| !p.is_empty()),
        tls: None,
        options: BTreeMap::new(),
    };
    if srv {
        parsed.options.insert("srv".into(), "true".into());
    }
    let mut refused = Vec::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (key, value) = (decode(key)?, decode(value)?);
        if key.eq_ignore_ascii_case("tls") || key.eq_ignore_ascii_case("ssl") {
            parsed.tls = Some(match value.to_ascii_lowercase().as_str() {
                "true" => "require".into(),
                "false" => "disable".into(),
                _ => return Err(format!("{key} is true or false, not {value}")),
            });
        } else if DEFAULTS.iter().any(|(name, only)| {
            name.eq_ignore_ascii_case(&key) && only.is_none_or(|only| only == value)
        }) {
            continue;
        } else if let Some(secret) = SECRETS.iter().find(|s| s.eq_ignore_ascii_case(&key)) {
            return Err(format!(
                "{secret} is a secret: Kuery does not keep it in the connection's options"
            ));
        } else if let Some(field) = OPTIONS.iter().find(|o| o.key.eq_ignore_ascii_case(&key)) {
            let value = match boolean(field) {
                true => value.to_ascii_lowercase(),
                false => value,
            };
            // A toggle that is off is not kept, as the form keeps it.
            if !(matches!(field.kind, OptionKind::Toggle) && value == "false") {
                parsed.options.insert(field.key.to_string(), value);
            }
        } else {
            // By name only: the value may be a secret.
            refused.push(key);
        }
    }
    if !refused.is_empty() {
        return Err(format!(
            "Kuery does not keep these options: {}",
            refused.join(", ")
        ));
    }
    crate::DRIVER.info.check_options(&parsed.options)?;
    // As the connection string specification says: a user signs in to the
    // database the string names, unless it says where. X.509 users live in
    // `$external`, which the driver knows.
    let x509 = parsed.options.get("authMechanism").map(String::as_str) == Some("MONGODB-X509");
    if !parsed.user.is_empty()
        && !parsed.database.is_empty()
        && !x509
        && !parsed.options.contains_key("authSource")
    {
        parsed
            .options
            .insert("authSource".into(), parsed.database.clone());
    }
    Ok(parsed)
}

/// `host` or `host:port`, with IPv6 addresses in brackets (`[::1]:27017`).
pub(crate) fn split_port(address: &str) -> Result<(&str, Option<u16>), String> {
    let address = address.trim();
    let (host, port) = match address.strip_prefix('[') {
        Some(inner) => match inner.split_once(']') {
            Some((host, "")) => (host, None),
            Some((host, rest)) => match rest.strip_prefix(':') {
                Some(port) => (host, Some(port)),
                None => return Err(format!("{address} is not a host")),
            },
            None => return Err(format!("{address} is not a host")),
        },
        None => match address.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (address, None),
        },
    };
    if host.is_empty() {
        return Err(format!("{address} names no host"));
    }
    let port = port
        .map(|port| {
            port.parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(|| format!("{port} is not a port, in {address}"))
        })
        .transpose()?;
    Ok((host, port))
}

fn strip_prefix_ignoring_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .filter(|start| start.eq_ignore_ascii_case(prefix))
        .map(|_| &text[prefix.len()..])
}

/// Undoes `%40`-style escapes.
fn decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let byte = text
                .get(at + 1..at + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .ok_or("The connection string has a % that does not start an escape like %40")?;
            out.push(byte);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "The connection string's escapes are not UTF-8".into())
}

/// Escapes all but unreserved characters, for a value in a connection
/// string.
pub(crate) fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn reads_an_atlas_string() {
        let parsed = parse(
            "mongodb+srv://u:p%40ss@cluster0.x.mongodb.net/shop?retryWrites=true&w=majority&authSource=admin",
        )
        .unwrap();
        assert_eq!(
            parsed,
            ParsedUri {
                host: "cluster0.x.mongodb.net".into(),
                port: None,
                database: "shop".into(),
                user: "u".into(),
                password: Some("p@ss".into()),
                tls: None,
                options: options(&[("authSource", "admin"), ("srv", "true")]),
            }
        );
    }

    #[test]
    fn reads_one_host_with_its_port() {
        let parsed = parse("mongodb://localhost:27019/?directConnection=true").unwrap();
        assert_eq!(
            (parsed.host.as_str(), parsed.port),
            ("localhost", Some(27019))
        );
        assert_eq!(parsed.database, "");
        assert_eq!((parsed.user.as_str(), parsed.password), ("", None));
        assert_eq!(parsed.options, options(&[("directConnection", "true")]));
        let ipv6 = parse("mongodb://[::1]:27017").unwrap();
        assert_eq!((ipv6.host.as_str(), ipv6.port), ("::1", Some(27017)));
        let bare = parse("MONGODB://db.example.com").unwrap();
        assert_eq!((bare.host.as_str(), bare.port), ("db.example.com", None));
    }

    #[test]
    fn keeps_members_as_a_list() {
        let parsed =
            parse("mongodb://kuery@a:27017,b:27018/shop?replicaSet=rs0&readpreference=secondary")
                .unwrap();
        assert_eq!(
            (parsed.host.as_str(), parsed.port),
            ("a:27017,b:27018", None)
        );
        assert_eq!((parsed.user.as_str(), parsed.password), ("kuery", None));
        // Matched ignoring case, kept as MongoDB spells it.
        assert_eq!(
            parsed.options,
            options(&[
                ("authSource", "shop"),
                ("readPreference", "secondary"),
                ("replicaSet", "rs0")
            ])
        );
    }

    #[test]
    fn signs_in_to_the_database_the_string_names() {
        let parsed =
            parse("mongodb://kuery_ro:kuery@127.0.0.1:27019/shop?directConnection=true").unwrap();
        assert_eq!(parsed.options["authSource"], "shop");
        // Unless it says where.
        let admin = parse("mongodb://u:p@h/shop?authSource=admin").unwrap();
        assert_eq!(admin.options["authSource"], "admin");
        // Without a user or a database, the driver's default.
        assert!(
            !parse("mongodb://h/shop")
                .unwrap()
                .options
                .contains_key("authSource")
        );
        assert!(
            !parse("mongodb://u:p@h/")
                .unwrap()
                .options
                .contains_key("authSource")
        );
        // X.509 users live in $external.
        let x509 = parse("mongodb://CN=me@h/shop?authMechanism=MONGODB-X509").unwrap();
        assert!(!x509.options.contains_key("authSource"));
    }

    #[test]
    fn maps_tls_to_the_form_choice() {
        assert_eq!(
            parse("mongodb://h/?tls=true").unwrap().tls.as_deref(),
            Some("require")
        );
        assert_eq!(
            parse("mongodb://h/?ssl=false").unwrap().tls.as_deref(),
            Some("disable")
        );
        assert_eq!(parse("mongodb://h").unwrap().tls, None);
        assert!(parse("mongodb://h/?tls=maybe").is_err());
    }

    #[test]
    fn keeps_toggles_as_the_form_does() {
        let parsed =
            parse("mongodb://h/?directConnection=FALSE&tlsAllowInvalidCertificates=false").unwrap();
        // A choice keeps false; a toggle that is off is left out.
        assert_eq!(parsed.options, options(&[("directConnection", "false")]));
        let on = parse("mongodb://h/?tlsAllowInvalidCertificates=True").unwrap();
        assert_eq!(on.options["tlsAllowInvalidCertificates"], "true");
        assert!(parse("mongodb://h/?directConnection=maybe").is_err());
    }

    #[test]
    fn refuses_options_it_does_not_keep_by_name() {
        assert_eq!(
            parse("mongodb://h/?maxPoolSize=5").unwrap_err(),
            "Kuery does not keep these options: maxPoolSize"
        );
        assert_eq!(
            parse("mongodb://h/?w=1").unwrap_err(),
            "Kuery does not keep these options: w"
        );
        for secret in [
            "tlsCertificateKeyFilePassword=hunter2",
            "sslPEMKeyPassword=hunter2",
        ] {
            let refused = parse(&format!("mongodb://h/?{secret}")).unwrap_err();
            assert!(refused.contains("is a secret"), "{refused}");
            assert!(!refused.contains("hunter2"), "{refused}");
        }
        // Whatever an option it does not know carries stays out of the text.
        let token =
            parse("mongodb://h/?authMechanismProperties=AWS_SESSION_TOKEN:hunter2").unwrap_err();
        assert_eq!(
            token,
            "Kuery does not keep these options: authMechanismProperties"
        );
        assert!(parse("mongodb://h/?authMechanism=GSSAPI").is_err());
        assert!(parse("mongodb://h/?authMechanism=SCRAM-SHA-256").is_ok());
    }

    #[test]
    fn refuses_what_is_not_a_connection_string() {
        assert!(parse("postgres://h/db").is_err());
        assert!(parse("mongodb://").is_err());
        assert!(parse("mongodb://h:port").is_err());
        assert!(parse("mongodb+srv://h:27017").is_err());
        assert!(parse("mongodb://u:%zz@h").is_err());
    }

    #[test]
    fn refuses_a_password_with_a_bare_slash_without_repeating_it() {
        for uri in [
            "mongodb://u:2024/x@h/db",
            "mongodb://kuery:2024/s3cretTail@db.example.com/shop",
            "mongodb://kuery:ab/s3cretTail@db.example.com/shop",
            "mongodb://u:a@b/c@h",
        ] {
            let refused = parse(uri).unwrap_err();
            assert_eq!(refused, UNESCAPED, "{uri}");
        }
        // Escaped, the same password is read whole.
        let escaped = parse("mongodb://u:2024%2Fx@h/db").unwrap();
        assert_eq!(
            (escaped.host.as_str(), escaped.password.as_deref()),
            ("h", Some("2024/x"))
        );
        assert_eq!(escaped.database, "db");
        // A database MongoDB would not take is refused, without its name.
        for uri in [
            "mongodb://h/a%2Fb",
            "mongodb://h/shop.orders",
            "mongodb://h/a%24b",
        ] {
            let refused = parse(uri).unwrap_err();
            assert!(refused.starts_with("A database name has no /"), "{refused}");
        }
    }

    #[test]
    fn encodes_values_for_a_connection_string() {
        assert_eq!(
            encode("/etc/ssl/ca file.pem"),
            "%2Fetc%2Fssl%2Fca%20file.pem"
        );
        assert_eq!(decode(&encode("p@ss:wörd")).unwrap(), "p@ss:wörd");
    }
}
