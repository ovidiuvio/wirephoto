//! API keys and signed URLs (api.md section 2).
//!
//! A key is 32 random bytes shown once; the index keeps only its SHA-256. Signed URLs carry
//! `sig=<key id>.<mac>`, an HMAC over the key and the entry under a server secret: stable for the key's life
//! (so browsers cache by URL), dead when the key is revoked.

use hmac::{Hmac, KeyInit, Mac};
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use wirephoto_api::Scope;

use crate::db::Db;
use crate::util;

#[derive(Debug, Clone)]
pub struct KeyRow {
    pub id: String,
    pub name: String,
    pub scope: Scope,
    /// Library API IDs the key may use; `None` = all.
    pub libraries: Option<Vec<String>>,
    pub origin: Option<String>,
    pub created: i64,
    pub last_used: Option<i64>,
}

impl KeyRow {
    pub fn may_use(&self, library: &str) -> bool {
        self.libraries
            .as_ref()
            .is_none_or(|l| l.iter().any(|x| x == library))
    }
}

pub struct Keys {
    secret: [u8; 32],
}

fn scope_str(s: Scope) -> &'static str {
    match s {
        Scope::Read => "read",
        Scope::Write => "write",
    }
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<KeyRow> {
    let libs: Option<String> = r.get(3)?;
    Ok(KeyRow {
        id: r.get(0)?,
        name: r.get(1)?,
        scope: if r.get::<_, String>(2)? == "write" {
            Scope::Write
        } else {
            Scope::Read
        },
        libraries: libs.map(|s| {
            s.split(',')
                .filter(|x| !x.is_empty())
                .map(String::from)
                .collect()
        }),
        origin: r.get(4)?,
        created: r.get(5)?,
        last_used: r.get(6)?,
    })
}

const COLS: &str = "id, name, scope, libraries, origin, created, last_used";

/// "https://Lith.example.com/" -> "https://lith.example.com": the form browsers send in `Origin`.
pub fn normalize_origin(o: &str) -> Option<String> {
    let o = o.trim().trim_end_matches('/');
    let (scheme, rest) = o.split_once("://")?;
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        || rest.is_empty()
        || rest.contains('/')
    {
        return None;
    }
    Some(format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        rest.to_ascii_lowercase()
    ))
}

impl Keys {
    /// Loads the server secret, creating it on first start.
    pub fn load(db: &Db) -> anyhow::Result<Keys> {
        let secret = match db.meta_get("secret")? {
            Some(v) if v.len() == 32 => v.try_into().unwrap(),
            _ => {
                let s: [u8; 32] = util::random_bytes();
                db.meta_set("secret", &s)?;
                s
            }
        };
        Ok(Keys { secret })
    }

    pub fn create(
        db: &Db,
        name: &str,
        scope: Scope,
        libraries: Option<Vec<String>>,
        origin: Option<String>,
    ) -> anyhow::Result<(KeyRow, String)> {
        let token = format!("wp_{}", util::b64url(&util::random_bytes::<32>()));
        let hash = Sha256::digest(token.as_bytes()).to_vec();
        let id = util::random_id("key_");
        let created = util::now_secs();
        let libs = libraries.as_ref().map(|l| l.join(","));
        db.write(|t| {
            t.execute(
                "INSERT INTO keys (id, name, hash, scope, libraries, origin, created) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![id, name, hash, scope_str(scope), libs, origin, created],
            )?;
            Ok(())
        })?;
        Ok((
            KeyRow {
                id,
                name: name.into(),
                scope,
                libraries,
                origin,
                created,
                last_used: None,
            },
            token,
        ))
    }

    pub fn list(db: &Db) -> anyhow::Result<Vec<KeyRow>> {
        db.read(|c| {
            let mut st = c.prepare(&format!("SELECT {COLS} FROM keys ORDER BY created"))?;
            let rows = st.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Revokes by name or ID. Returns whether a key was removed.
    pub fn revoke(db: &Db, name_or_id: &str) -> anyhow::Result<bool> {
        db.write(|t| {
            Ok(t.execute("DELETE FROM keys WHERE name = ?1 OR id = ?1", [name_or_id])? > 0)
        })
    }

    pub fn by_token(db: &Db, token: &str) -> anyhow::Result<Option<KeyRow>> {
        let hash = Sha256::digest(token.as_bytes()).to_vec();
        db.read(|c| {
            Ok(c.query_row(
                &format!("SELECT {COLS} FROM keys WHERE hash = ?"),
                [hash],
                row,
            )
            .optional()?)
        })
    }

    pub fn by_id(db: &Db, id: &str) -> anyhow::Result<Option<KeyRow>> {
        db.read(|c| {
            Ok(
                c.query_row(&format!("SELECT {COLS} FROM keys WHERE id = ?"), [id], row)
                    .optional()?,
            )
        })
    }

    /// Every origin that holds a key: the CORS allow-list.
    pub fn origins(db: &Db) -> anyhow::Result<Vec<String>> {
        db.read(|c| {
            let mut st = c.prepare("SELECT DISTINCT origin FROM keys WHERE origin IS NOT NULL")?;
            let rows = st
                .query_map([], |r| r.get(0))?
                .collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
    }

    pub fn touch(db: &Db, key: &KeyRow) {
        let now = util::now_secs();
        if key.last_used.is_none_or(|t| now - t >= 60) {
            let _ = db.write(|t| {
                Ok(t.execute(
                    "UPDATE keys SET last_used = ? WHERE id = ?",
                    params![now, key.id],
                )?)
            });
        }
    }

    fn mac(&self, key_id: &str, entry_id: &str) -> String {
        let mut m =
            <Hmac<Sha256> as KeyInit>::new_from_slice(&self.secret).expect("any key length");
        m.update(key_id.as_bytes());
        m.update(b":");
        m.update(entry_id.as_bytes());
        util::b64url(&m.finalize().into_bytes()[..16])
    }

    /// The `sig` for an entry's URLs, for a key.
    pub fn sign(&self, key_id: &str, entry_id: &str) -> String {
        format!(
            "{}.{}",
            key_id.trim_start_matches("key_"),
            self.mac(key_id, entry_id)
        )
    }

    /// The key ID a `sig` was made for, when it is valid for `entry_id` (the key itself must still exist).
    pub fn verify(&self, sig: &str, entry_id: &str) -> Option<String> {
        let (short, mac) = sig.split_once('.')?;
        let key_id = format!("key_{short}");
        let expected = self.mac(&key_id, entry_id);
        bool::from(expected.as_bytes().ct_eq(mac.as_bytes())).then_some(key_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_sigs() {
        let t = tempfile::tempdir().unwrap();
        let db = Db::open(&t.path().join("i.db")).unwrap();
        let keys = Keys::load(&db).unwrap();
        let (k, token) = Keys::create(
            &db,
            "lith",
            Scope::Write,
            None,
            Some("https://lith.example.com".into()),
        )
        .unwrap();
        assert_eq!(Keys::by_token(&db, &token).unwrap().unwrap().id, k.id);
        assert!(Keys::by_token(&db, "wp_nope").unwrap().is_none());
        let sig = keys.sign(&k.id, "ent_1");
        assert_eq!(keys.verify(&sig, "ent_1").as_deref(), Some(k.id.as_str()));
        assert_eq!(keys.verify(&sig, "ent_2"), None);
        assert_eq!(
            Keys::origins(&db).unwrap(),
            vec!["https://lith.example.com".to_string()]
        );
        assert!(Keys::revoke(&db, "lith").unwrap());
        assert!(Keys::by_id(&db, &k.id).unwrap().is_none());
        // The same secret after a restart: URLs stay valid.
        assert_eq!(Keys::load(&db).unwrap().sign(&k.id, "ent_1"), sig);
    }

    #[test]
    fn origins_normalize() {
        assert_eq!(
            normalize_origin("https://Lith.Example.com/").as_deref(),
            Some("https://lith.example.com")
        );
        assert_eq!(
            normalize_origin("http://localhost:8080").as_deref(),
            Some("http://localhost:8080")
        );
        assert_eq!(normalize_origin("ftp://x"), None);
        assert_eq!(normalize_origin("https://x/path"), None);
    }
}
