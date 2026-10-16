//! Redis without a connection: splitting command lines the way `redis-cli`
//! does (and quoting them back), telling which commands only read, which
//! writes deserve a second look and which keys they change, and grouping key
//! names into a tree by a delimiter (`user:1:name` → `user ▸ 1 ▸ name`).

use std::cmp::Ordering;
use std::collections::BTreeMap;

/// Splits a command line into arguments like `redis-cli`: blanks separate
/// them; `"…"` understands `\n`, `\r`, `\t`, `\b`, `\a`, `\\`, `\"` and
/// `\xHH`; `'…'` only `\'`. A closing quote must end the argument.
pub fn split_command(line: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else {
            return Ok(args);
        };
        let mut arg = String::new();
        match first {
            '"' => {
                chars.next();
                loop {
                    match chars.next() {
                        None => return Err("unbalanced double quotes".into()),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => arg.push('\n'),
                            Some('r') => arg.push('\r'),
                            Some('t') => arg.push('\t'),
                            Some('b') => arg.push('\u{8}'),
                            Some('a') => arg.push('\u{7}'),
                            Some('x') => {
                                let hex: String = chars.by_ref().take(2).collect();
                                match u8::from_str_radix(&hex, 16) {
                                    Ok(byte) if hex.len() == 2 => arg.push(byte as char),
                                    _ => {
                                        arg.push_str("\\x");
                                        arg.push_str(&hex);
                                    }
                                }
                            }
                            Some(other) => arg.push(other),
                            None => return Err("unbalanced double quotes".into()),
                        },
                        Some(c) => arg.push(c),
                    }
                }
                ensure_separated(&mut chars)?;
            }
            '\'' => {
                chars.next();
                loop {
                    match chars.next() {
                        None => return Err("unbalanced single quotes".into()),
                        Some('\'') => break,
                        Some('\\') if chars.peek() == Some(&'\'') => {
                            chars.next();
                            arg.push('\'');
                        }
                        Some(c) => arg.push(c),
                    }
                }
                ensure_separated(&mut chars)?;
            }
            _ => {
                while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                    arg.push(c);
                }
            }
        }
        args.push(arg);
    }
}

fn ensure_separated(chars: &mut std::iter::Peekable<std::str::Chars>) -> Result<(), String> {
    match chars.peek() {
        Some(c) if !c.is_whitespace() => Err("a closing quote must be followed by a space".into()),
        _ => Ok(()),
    }
}

/// The lines of a console script that hold a command, with their index:
/// blank lines and comments (`#` or `//`) are skipped.
pub fn command_lines(text: &str) -> Vec<(usize, &str)> {
    text.lines()
        .enumerate()
        .map(|(index, line)| (index, line.trim()))
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#') && !line.starts_with("//"))
        .collect()
}

/// What a command does, for deciding whether it may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Only reads.
    Read,
    /// Only reads, but walks the whole keyspace and blocks the server while
    /// it does (`KEYS`).
    Heavy,
    /// Changes data or the server, or is unknown to Kuery.
    Write,
    /// Not run from the console: it would change or hold the connection.
    Unsupported(&'static str),
}

/// Commands that only read, whatever their arguments.
const READS: &[&str] = &[
    "BITCOUNT",
    "BITFIELD_RO",
    "BITPOS",
    "DBSIZE",
    "DUMP",
    "ECHO",
    "EXISTS",
    "EXPIRETIME",
    "GEODIST",
    "GEOHASH",
    "GEOPOS",
    "GEORADIUSBYMEMBER_RO",
    "GEORADIUS_RO",
    "GEOSEARCH",
    "GET",
    "GETBIT",
    "GETRANGE",
    "HEXISTS",
    "HGET",
    "HGETALL",
    "HKEYS",
    "HLEN",
    "HMGET",
    "HRANDFIELD",
    "HSCAN",
    "HSTRLEN",
    "HVALS",
    "INFO",
    "LASTSAVE",
    "LCS",
    "LINDEX",
    "LLEN",
    "LOLWUT",
    "LPOS",
    "LRANGE",
    "MGET",
    "OBJECT",
    "PEXPIRETIME",
    "PFCOUNT",
    "PING",
    "PTTL",
    "RANDOMKEY",
    "ROLE",
    "SCAN",
    "SCARD",
    "SDIFF",
    "SINTER",
    "SINTERCARD",
    "SISMEMBER",
    "SMEMBERS",
    "SMISMEMBER",
    "SRANDMEMBER",
    "SSCAN",
    "STRLEN",
    "SUBSTR",
    "SUNION",
    "TIME",
    "TOUCH",
    "TTL",
    "TYPE",
    "XINFO",
    "XLEN",
    "XPENDING",
    "XRANGE",
    "XREVRANGE",
    "ZCARD",
    "ZCOUNT",
    "ZDIFF",
    "ZINTER",
    "ZINTERCARD",
    "ZLEXCOUNT",
    "ZMSCORE",
    "ZRANDMEMBER",
    "ZRANGE",
    "ZRANGEBYLEX",
    "ZRANGEBYSCORE",
    "ZRANK",
    "ZREVRANGE",
    "ZREVRANGEBYLEX",
    "ZREVRANGEBYSCORE",
    "ZREVRANK",
    "ZSCAN",
    "ZSCORE",
    "ZUNION",
];

/// Commands that only read with some subcommands: (command, subcommands).
const READ_SUBCOMMANDS: &[(&str, &[&str])] = &[
    (
        "ACL",
        &["CAT", "GETUSER", "LIST", "LOG", "USERS", "WHOAMI", "HELP"],
    ),
    ("CLIENT", &["GETNAME", "ID", "INFO", "LIST", "HELP"]),
    (
        "CLUSTER",
        &[
            "COUNTKEYSINSLOT",
            "INFO",
            "KEYSLOT",
            "MYID",
            "NODES",
            "SHARDS",
            "SLOTS",
            "HELP",
        ],
    ),
    (
        "COMMAND",
        &["COUNT", "DOCS", "GETKEYS", "INFO", "LIST", "HELP"],
    ),
    ("CONFIG", &["GET", "HELP"]),
    ("FUNCTION", &["DUMP", "LIST", "STATS", "HELP"]),
    (
        "LATENCY",
        &["DOCTOR", "GRAPH", "HISTOGRAM", "HISTORY", "LATEST", "HELP"],
    ),
    (
        "MEMORY",
        &["DOCTOR", "MALLOC-STATS", "STATS", "USAGE", "HELP"],
    ),
    ("MODULE", &["LIST", "HELP"]),
    (
        "PUBSUB",
        &[
            "CHANNELS",
            "NUMPAT",
            "NUMSUB",
            "SHARDCHANNELS",
            "SHARDNUMSUB",
            "HELP",
        ],
    ),
    ("SCRIPT", &["EXISTS", "HELP"]),
    ("SLOWLOG", &["GET", "LEN", "HELP"]),
];

/// Classifies a command by its arguments (the first is the command).
pub fn classify(args: &[String]) -> Class {
    let Some(command) = args.first() else {
        return Class::Write;
    };
    let command = command.to_ascii_uppercase();
    let sub = args
        .get(1)
        .map(|s| s.to_ascii_uppercase())
        .unwrap_or_default();
    match command.as_str() {
        "SELECT" => return Class::Unsupported("pick the database in the toolbar instead"),
        "AUTH" | "HELLO" | "RESET" | "QUIT" => {
            return Class::Unsupported("authentication comes from the connection settings");
        }
        "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "MONITOR" | "SYNC" | "PSYNC" => {
            return Class::Unsupported("it would hold the connection");
        }
        "BLPOP" | "BRPOP" | "BLMOVE" | "BRPOPLPUSH" | "BLMPOP" | "BZPOPMIN" | "BZPOPMAX"
        | "BZMPOP" | "WAIT" | "WAITAOF" => {
            return Class::Unsupported("blocking commands would hold the connection");
        }
        "MULTI" | "EXEC" | "DISCARD" | "WATCH" | "UNWATCH" => {
            return Class::Unsupported("transactions are not supported in the console");
        }
        "KEYS" => return Class::Heavy,
        "XREAD" => {
            return if args.iter().any(|a| a.eq_ignore_ascii_case("BLOCK")) {
                Class::Unsupported("blocking commands would hold the connection")
            } else {
                Class::Read
            };
        }
        _ => {}
    }
    if READS.contains(&command.as_str()) {
        return Class::Read;
    }
    match READ_SUBCOMMANDS.iter().find(|(name, _)| *name == command) {
        Some((_, subs)) if subs.contains(&sub.as_str()) => Class::Read,
        _ => Class::Write,
    }
}

/// `DEL`/`UNLINK` with this many keys or more counts as dangerous.
pub const MANY_KEYS: usize = 10;

/// Why a command that writes deserves a second look, if it does: it loses
/// a lot of data at once, or changes the server rather than its data.
/// (`RENAME` over an existing key is caught by looking at the target, see
/// [`overwritten_key`].)
pub fn danger(args: &[String]) -> Option<String> {
    let command = args.first()?.to_ascii_uppercase();
    let sub = args
        .get(1)
        .map(|s| s.to_ascii_uppercase())
        .unwrap_or_default();
    let reason = match command.as_str() {
        "FLUSHDB" => "deletes every key in the database".to_string(),
        "FLUSHALL" => "deletes every key in every database".to_string(),
        "DEL" | "UNLINK" if args.len() > MANY_KEYS => {
            format!("deletes {} keys at once", args.len() - 1)
        }
        "CONFIG" if sub != "GET" && sub != "HELP" => "changes the server's configuration".into(),
        "SHUTDOWN" => "stops the server".into(),
        "DEBUG" => "is a debugging command that can crash or stall the server".into(),
        "SWAPDB" => "swaps two whole databases".into(),
        "MIGRATE" => "moves keys to another server".into(),
        "REPLICAOF" | "SLAVEOF" => "changes replication".into(),
        "CLIENT" if sub == "KILL" || sub == "PAUSE" => "disconnects or pauses clients".into(),
        "SCRIPT" if sub == "FLUSH" || sub == "KILL" => "removes or stops server scripts".into(),
        "FUNCTION" if matches!(sub.as_str(), "FLUSH" | "DELETE" | "KILL" | "RESTORE") => {
            "changes the server's functions".into()
        }
        "ACL" if matches!(sub.as_str(), "SETUSER" | "DELUSER" | "LOAD" | "SAVE") => {
            "changes users and permissions".into()
        }
        "MODULE" if sub != "LIST" && sub != "HELP" => "loads or unloads server modules".into(),
        _ => return None,
    };
    Some(reason)
}

/// Commands whose first argument is the one key they change.
const ONE_KEY: &[&str] = &[
    "APPEND",
    "DECR",
    "DECRBY",
    "EXPIRE",
    "EXPIREAT",
    "GETDEL",
    "GETEX",
    "GETSET",
    "HDEL",
    "HINCRBY",
    "HINCRBYFLOAT",
    "HMSET",
    "HSET",
    "HSETNX",
    "INCR",
    "INCRBY",
    "INCRBYFLOAT",
    "LINSERT",
    "LPOP",
    "LPUSH",
    "LPUSHX",
    "LREM",
    "LSET",
    "LTRIM",
    "PERSIST",
    "PEXPIRE",
    "PEXPIREAT",
    "PFADD",
    "PSETEX",
    "RPOP",
    "RPUSH",
    "RPUSHX",
    "SADD",
    "SET",
    "SETBIT",
    "SETEX",
    "SETNX",
    "SETRANGE",
    "SPOP",
    "SREM",
    "XADD",
    "XDEL",
    "XTRIM",
    "ZADD",
    "ZINCRBY",
    "ZPOPMAX",
    "ZPOPMIN",
    "ZREM",
    "ZREMRANGEBYLEX",
    "ZREMRANGEBYRANK",
    "ZREMRANGEBYSCORE",
];

/// The keys a command that writes would change, to show them before it
/// runs. Commands Kuery does not know list none.
pub fn written_keys(args: &[String]) -> Vec<String> {
    let Some(command) = args.first().map(|c| c.to_ascii_uppercase()) else {
        return Vec::new();
    };
    match command.as_str() {
        "DEL" | "UNLINK" => args[1..].to_vec(),
        "MSET" | "MSETNX" => args[1..].iter().step_by(2).cloned().collect(),
        "RENAME" | "RENAMENX" | "SMOVE" | "LMOVE" | "RPOPLPUSH" | "COPY" => {
            args.iter().skip(1).take(2).cloned().collect()
        }
        name if ONE_KEY.contains(&name) => args.get(1).cloned().into_iter().collect(),
        _ => Vec::new(),
    }
}

/// The existing key a command would replace, if it names one: the target of
/// `RENAME`, or of `COPY … REPLACE`.
pub fn overwritten_key(args: &[String]) -> Option<&str> {
    let command = args.first()?.to_ascii_uppercase();
    match command.as_str() {
        "RENAME" => args.get(2).map(String::as_str),
        "COPY" if args.iter().any(|a| a.eq_ignore_ascii_case("REPLACE")) => {
            args.get(2).map(String::as_str)
        }
        _ => None,
    }
}

/// Arguments back into one command line that [`split_command`] reads the
/// same way: plain when they can be, double-quoted with escapes otherwise.
pub fn join_command(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            let plain = !arg.is_empty()
                && arg.chars().all(|c| {
                    !c.is_whitespace() && !c.is_control() && c != '"' && c != '\'' && c != '\\'
                });
            if plain {
                return arg.clone();
            }
            let mut quoted = String::from("\"");
            for c in arg.chars() {
                match c {
                    '"' => quoted.push_str("\\\""),
                    '\\' => quoted.push_str("\\\\"),
                    '\n' => quoted.push_str("\\n"),
                    '\r' => quoted.push_str("\\r"),
                    '\t' => quoted.push_str("\\t"),
                    c if c.is_control() => quoted.push_str(&format!("\\x{:02x}", c as u32)),
                    c => quoted.push(c),
                }
            }
            quoted.push('"');
            quoted
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every command name Kuery knows, for completion in the console.
pub fn known_commands() -> Vec<&'static str> {
    let mut names: Vec<&str> = READS
        .iter()
        .copied()
        .chain(READ_SUBCOMMANDS.iter().map(|(name, _)| *name))
        .chain([
            "APPEND",
            "COPY",
            "DECR",
            "DECRBY",
            "DEL",
            "EXPIRE",
            "EXPIREAT",
            "FLUSHALL",
            "FLUSHDB",
            "GETDEL",
            "GETEX",
            "GETSET",
            "HDEL",
            "HINCRBY",
            "HINCRBYFLOAT",
            "HSET",
            "HSETNX",
            "INCR",
            "INCRBY",
            "INCRBYFLOAT",
            "KEYS",
            "LINSERT",
            "LMOVE",
            "LPOP",
            "LPUSH",
            "LREM",
            "LSET",
            "LTRIM",
            "MSET",
            "PERSIST",
            "PEXPIRE",
            "PFADD",
            "RENAME",
            "RENAMENX",
            "RPOP",
            "RPUSH",
            "SADD",
            "SET",
            "SETEX",
            "SETNX",
            "SETRANGE",
            "SMOVE",
            "SPOP",
            "SREM",
            "UNLINK",
            "XADD",
            "XDEL",
            "XTRIM",
            "XREAD",
            "ZADD",
            "ZINCRBY",
            "ZPOPMAX",
            "ZPOPMIN",
            "ZREM",
        ])
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Key names grouped by a delimiter. A node can be a key and a folder at
/// once (`user` and `user:1`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct KeyTree {
    children: BTreeMap<String, KeyTree>,
    /// The full name, when a key ends at this node.
    pub key: Option<String>,
    /// Keys at or under this node.
    pub count: usize,
}

impl KeyTree {
    /// Groups `keys` by `delimiter`; an empty delimiter keeps them flat.
    pub fn build<'a>(keys: impl IntoIterator<Item = &'a str>, delimiter: &str) -> Self {
        let mut tree = KeyTree::default();
        for key in keys {
            tree.insert(key, delimiter);
        }
        tree
    }

    pub fn insert(&mut self, key: &str, delimiter: &str) {
        self.count += 1;
        let mut node = self;
        let segments: Vec<&str> = if delimiter.is_empty() {
            vec![key]
        } else {
            key.split(delimiter).collect()
        };
        for segment in segments {
            node = node.children.entry(segment.to_string()).or_default();
            node.count += 1;
        }
        node.key = Some(key.to_string());
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The node at `path` (segments from the root).
    pub fn get(&self, path: &[String]) -> Option<&KeyTree> {
        path.iter()
            .try_fold(self, |node, segment| node.children.get(segment))
    }

    /// Children in natural order (`user:2` before `user:10`).
    pub fn children(&self) -> Vec<(&str, &KeyTree)> {
        let mut children: Vec<(&str, &KeyTree)> = self
            .children
            .iter()
            .map(|(name, node)| (name.as_str(), node))
            .collect();
        children.sort_by(|a, b| natural_cmp(a.0, b.0));
        children
    }

    pub fn has_children(&self) -> bool {
        !self.children.is_empty()
    }
}

/// Compares runs of digits by value, everything else by character.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let a_len = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let b_len = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let (a_digits, b_digits) = (&a[..a_len], &b[..b_len]);
                let trim = |digits: &[u8]| {
                    let zeros = digits.iter().take_while(|c| **c == b'0').count();
                    digits[zeros..].to_vec()
                };
                let (a_value, b_value) = (trim(a_digits), trim(b_digits));
                let order = a_value
                    .len()
                    .cmp(&b_value.len())
                    .then_with(|| a_value.cmp(&b_value))
                    .then_with(|| a_len.cmp(&b_len));
                if order != Ordering::Equal {
                    return order;
                }
                a = &a[a_len..];
                b = &b[b_len..];
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(y);
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        split_command(line).unwrap()
    }

    #[test]
    fn splits_like_redis_cli() {
        assert_eq!(args("  GET   user:1 "), ["GET", "user:1"]);
        assert_eq!(
            args(r#"SET greeting "hello \"world\"\n" 'it\'s'"#),
            ["SET", "greeting", "hello \"world\"\n", "it's"]
        );
        assert_eq!(args(r#"SET k "\x41\x42""#), ["SET", "k", "AB"]);
        assert_eq!(args(r#"SET k """#), ["SET", "k", ""]);
        assert!(args("").is_empty());
        assert!(split_command(r#"SET k "open"#).is_err());
        assert!(split_command(r#"SET k "a"b"#).is_err());
    }

    #[test]
    fn finds_the_lines_with_commands() {
        let script = "GET a\n\n# a comment\n  // another\nHGETALL user:1\n";
        assert_eq!(command_lines(script), [(0, "GET a"), (4, "HGETALL user:1")]);
    }

    #[test]
    fn classifies_commands() {
        let class = |line: &str| classify(&args(line));
        assert_eq!(class("get a"), Class::Read);
        assert_eq!(class("HGETALL user:1"), Class::Read);
        assert_eq!(class("config get maxmemory"), Class::Read);
        assert_eq!(class("CONFIG SET maxmemory 1"), Class::Write);
        assert_eq!(class("MEMORY USAGE k"), Class::Read);
        assert_eq!(class("SET a 1"), Class::Write);
        assert_eq!(class("FLUSHALL"), Class::Write);
        assert_eq!(class("SOMETHINGNEW x"), Class::Write);
        assert_eq!(class("KEYS *"), Class::Heavy);
        assert_eq!(class("XREAD STREAMS s 0"), Class::Read);
        assert!(matches!(
            class("XREAD BLOCK 0 STREAMS s $"),
            Class::Unsupported(_)
        ));
        assert!(matches!(class("SELECT 2"), Class::Unsupported(_)));
        assert!(matches!(class("SUBSCRIBE news"), Class::Unsupported(_)));
        assert!(known_commands().contains(&"HGETALL"));
    }

    #[test]
    fn flags_dangerous_writes() {
        let danger = |line: &str| danger(&args(line));
        assert!(danger("FLUSHDB").unwrap().contains("every key"));
        assert!(danger("flushall async").is_some());
        assert_eq!(danger("DEL a b c"), None);
        assert!(
            danger("DEL a b c d e f g h i j")
                .unwrap()
                .contains("10 keys")
        );
        assert!(danger("CONFIG SET maxmemory 1").is_some());
        assert_eq!(danger("CONFIG GET maxmemory"), None);
        assert_eq!(danger("SET a 1"), None);
        assert!(danger("SHUTDOWN NOSAVE").is_some());
    }

    #[test]
    fn finds_the_keys_a_write_changes() {
        let keys = |line: &str| written_keys(&args(line));
        assert_eq!(keys("SET a 1 EX 10"), ["a"]);
        assert_eq!(keys("del a b"), ["a", "b"]);
        assert_eq!(keys("MSET a 1 b 2"), ["a", "b"]);
        assert_eq!(keys("RENAME a b"), ["a", "b"]);
        assert_eq!(keys("HSET user:1 plan pro"), ["user:1"]);
        assert!(keys("FLUSHDB").is_empty());
        assert_eq!(overwritten_key(&args("RENAME a b")), Some("b"));
        assert_eq!(overwritten_key(&args("RENAMENX a b")), None);
        assert_eq!(overwritten_key(&args("COPY a b REPLACE")), Some("b"));
        assert_eq!(overwritten_key(&args("COPY a b")), None);
    }

    #[test]
    fn quotes_commands_back_into_lines() {
        let line = vec![
            "SET".to_string(),
            "key one".into(),
            "say \"hi\"\n".into(),
            "".into(),
            "plain".into(),
        ];
        let joined = join_command(&line);
        assert_eq!(joined, r#"SET "key one" "say \"hi\"\n" "" plain"#);
        assert_eq!(split_command(&joined).unwrap(), line);
    }

    #[test]
    fn groups_keys_by_delimiter() {
        let tree = KeyTree::build(
            [
                "user:1:name",
                "user:2",
                "user:10",
                "user",
                "app:config",
                "plain",
            ],
            ":",
        );
        assert_eq!(tree.count, 6);
        let names: Vec<&str> = tree.children().iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["app", "plain", "user"]);
        let user = tree.get(&["user".to_string()]).unwrap();
        assert_eq!((user.count, user.key.as_deref()), (4, Some("user")));
        let ids: Vec<&str> = user.children().iter().map(|(name, _)| *name).collect();
        assert_eq!(ids, ["1", "2", "10"]);
        let one = user.get(&["1".to_string()]).unwrap();
        assert!(one.key.is_none() && one.has_children());
        let flat = KeyTree::build(["a:b", "c"], "");
        assert_eq!(flat.children().len(), 2);
    }

    #[test]
    fn sorts_numbers_by_value() {
        let mut names = vec!["item10", "item2", "item02", "Item1", "item"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, ["Item1", "item", "item2", "item02", "item10"]);
    }
}
