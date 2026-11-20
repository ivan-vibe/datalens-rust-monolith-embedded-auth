//! First-run demo data, loaded by the monolith itself.
//!
//! The data is the official demo set from `ghcr.io/datalens-tech/datalens-postgres`
//! (`/init/demo-data`), compressed at build time and embedded here, so a fresh deployment
//! needs no extra container and no dumps in the repository. The seed runs once: when
//! `pg-us-db.workbooks` is empty, it creates the demo tables in `pg-demo-db` and inserts the
//! demo workbooks/datasets/connections into `pg-us-db`.
//!
//! The US file is a template: the connection entry needs the demo database's host, port,
//! name, user and fernet-encrypted password, substituted exactly like the official
//! `seed-demo-data.sh` does with `sed`.

use anyhow::{anyhow, Context, Result};
use tokio_postgres::NoTls;

/// `demo-data.sql` — demo tables and rows for `pg-demo-db` (official image content).
const DEMO_DATA: &[u8] = include_bytes!("../assets/demo/demo-data.sql.zst");
/// `us-d3-data.sql` — demo workbooks/datasets/connections for `pg-us-db`.
const US_DEMO: &[u8] = include_bytes!("../assets/demo/us-d3-data.sql.zst");

fn decompress(bytes: &[u8]) -> Result<String> {
    let raw = zstd::decode_all(bytes).context("decompress the embedded demo SQL")?;
    String::from_utf8(raw).context("the embedded demo SQL is not UTF-8")
}

fn flag(name: &str, default: bool) -> bool {
    std::env::var(name)
        .map(|value| !matches!(value.trim(), "0" | "false" | "off" | "no"))
        .unwrap_or(default)
}

/// `postgres://user:password@host:port/db` split into the parts the template needs.
struct Dsn {
    raw: String,
    user: String,
    password: String,
    host: String,
    port: String,
    db: String,
}

impl Dsn {
    fn parse(raw: &str) -> Result<Self> {
        let rest = raw
            .strip_prefix("postgres://")
            .or_else(|| raw.strip_prefix("postgresql://"))
            .ok_or_else(|| anyhow!("POSTGRES_DSN_LIST is not a postgres URL"))?;
        let (authority, db) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index + 1..]),
            None => (rest, "postgres"),
        };
        let db = db.split('?').next().unwrap_or(db);
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((userinfo, hostport)) => (userinfo, hostport),
            None => ("", authority),
        };
        let (user, password) = match userinfo.split_once(':') {
            Some((user, password)) => (user, password),
            None => (userinfo, ""),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((host, port)) => (host, port),
            None => (hostport, "5432"),
        };
        Ok(Self {
            raw: raw.to_string(),
            user: percent_decode(user),
            password: percent_decode(password),
            host: if host.is_empty() { "localhost" } else { host }.to_string(),
            port: port.to_string(),
            db: db.to_string(),
        })
    }

    fn with_db(&self, db: &str) -> String {
        let rest = &self.raw[self.raw.find("://").map(|i| i + 3).unwrap_or(0)..];
        match rest.find('/') {
            Some(index) => {
                let base = &self.raw[..self.raw.find("://").unwrap_or(0) + 3 + index];
                format!("{base}/{db}")
            }
            None => format!("{}/{db}", self.raw.trim_end_matches('/')),
        }
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                out.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn render(template: &str, values: &[(String, String)]) -> String {
    let mut sql = template.to_string();
    for (key, value) in values {
        sql = sql.replace(&format!("{{{{{key}}}}}"), value);
    }
    sql
}

/// Loads the official demo data when the US database has no workbooks yet.
pub async fn ensure() -> Result<()> {
    if !flag("DL_SEED_DEMO", true) {
        tracing::info!("demo seed disabled by DL_SEED_DEMO");
        return Ok(());
    }
    let raw = std::env::var("POSTGRES_DSN_LIST")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .map_err(|_| anyhow!("no Postgres DSN for the demo seed"))?;
    let first = raw.split(',').next().unwrap_or(&raw).trim().to_string();
    let dsn = Dsn::parse(&first)?;

    let (us_client, us_connection) = tokio_postgres::connect(&dsn.raw, NoTls)
        .await
        .context("demo seed: connect pg-us-db")?;
    tokio::spawn(async move {
        let _ = us_connection.await;
    });

    let schema: bool = us_client
        .query_one("SELECT to_regclass('public.workbooks') IS NOT NULL", &[])
        .await
        .context("demo seed: check the US schema")?
        .get(0);
    if !schema {
        tracing::warn!("demo seed: pg-us-db has no workbooks table yet, skipping");
        return Ok(());
    }
    let workbooks: i64 = us_client
        .query_one("SELECT count(*) FROM workbooks", &[])
        .await
        .context("demo seed: count workbooks")?
        .get(0);
    if workbooks > 0 {
        tracing::info!(workbooks, "demo seed: data already present");
        return Ok(());
    }

    // 1. demo tables and rows in pg-demo-db
    let demo_db = std::env::var("DL_DEMO_DB").unwrap_or_else(|_| "pg-demo-db".to_string());
    let demo_dsn = dsn.with_db(&demo_db);
    let (demo_client, demo_connection) = tokio_postgres::connect(&demo_dsn, NoTls)
        .await
        .context("demo seed: connect the demo database")?;
    tokio::spawn(async move {
        let _ = demo_connection.await;
    });
    let demo_sql = decompress(DEMO_DATA)?;
    demo_client
        .batch_execute(&demo_sql)
        .await
        .context("demo seed: load the demo tables")?;

    // 2. workbooks/datasets/connections in pg-us-db
    let control_key = std::env::var("CONTROL_API_CRYPTO_KEY")
        .or_else(|_| std::env::var("DL_CRY_KEY_VAL_ID_KEY"))
        .unwrap_or_default();
    let encrypted_password = dl_us::fernet::FernetKey::from_base64(&control_key)
        .and_then(|key| key.encrypt(&dsn.password).ok())
        .ok_or_else(|| anyhow!("demo seed: cannot encrypt the demo connection password"))?;
    let values = vec![
        (
            "DEMO_DATA_NAME".to_string(),
            std::env::var("DL_DEMO_NAME").unwrap_or_else(|_| "DataLens Demo".to_string()),
        ),
        ("POSTGRES_HOST".to_string(), demo_host(&demo_dsn, &dsn)),
        ("POSTGRES_PORT".to_string(), dsn.port.clone()),
        ("POSTGRES_DB".to_string(), demo_db.clone()),
        ("POSTGRES_USER".to_string(), dsn.user.clone()),
        ("POSTGRES_PASSWORD".to_string(), encrypted_password),
        ("POSTGRES_SSL_CA".to_string(), "null".to_string()),
        ("POSTGRES_SSL_ENABLED".to_string(), "false".to_string()),
    ];
    let us_sql = render(&decompress(US_DEMO)?, &values);
    us_client
        .batch_execute(&us_sql)
        .await
        .context("demo seed: load the demo workbooks")?;
    tracing::info!("demo seed: official demo data loaded");
    Ok(())
}

/// The host the demo connection should point at: the demo DSN's host (identical to the US
/// one unless `DL_DEMO_DSN` overrides it).
fn demo_host(demo_dsn: &str, us: &Dsn) -> String {
    Dsn::parse(demo_dsn)
        .map(|parsed| parsed.host)
        .unwrap_or_else(|_| us.host.clone())
}
