//! `POST /api/lockdown {"on": true|false, "allow"?: [...], "rev"?, "slug"?}`
//! and `GET /api/lockdown?slug=` (feature `lockdown`, v2.6.0;
//! `crate::lockdown`). Both answer the caller's own lockdown in full:
//! `{"on", "since", "rev", "allow": [{"address", "owner", "counts"}]}`.
//!
//! - `on: true` with `allow` replaces the list; without it keeps the list (a
//!   new lockdown starts with none). `on: false` lifts it, list and all.
//! - An entry of `allow` is an address on this hub (registered or not yet),
//!   or `{"address", "owner"}` where `owner` is the `picture_owner` token its
//!   roster entry shows: the hub then refuses the entry unless that identity
//!   holds the address at the time of listing (review B3: the caller names
//!   the person it means). Once listed, the entry follows the ADDRESS: a new
//!   key or registration for it stays allowed (user 2026-11-25 14:08Z,
//!   answer 7). `counts` in the answer says whether it is registered now.
//! - `rev` (a whole number) orders the owner's changes: one lower than the
//!   stored revision is refused with 409 and the stored `rev` (review I4: a
//!   late or repeated reconcile never undoes a newer change). Without it the
//!   revision goes up by one.
//!
//! `POST /api/register` takes the same object as `lockdown` and applies it
//! with the registration (in the same transaction for a new address), and
//! its answer carries the address's lockdown, so a client re-applies at once
//! what it finds missing.

use std::collections::HashMap;
use std::sync::Arc;

use deadpool_postgres::GenericClient;
use http::StatusCode;
use serde_json::{json, Map, Value};

use super::mail::{authed, one_address, valid_slug};
use super::{ok, refuse, ApiError, ApiResult, Hub, Req};
use crate::db;
use crate::lockdown::MAX_ALLOWED;

/// One entry of `allow`: an address and, optionally, the owner token the
/// caller expects to hold it.
pub struct Entry {
    address: String,
    owner: Option<String>,
}

/// A change asked for, checked for shape before any lock is taken.
pub struct Change {
    on: bool,
    allow: Option<Vec<Entry>>,
    rev: Option<i64>,
}

/// The shape of a change (`POST /api/lockdown`, register's `lockdown`).
pub fn parse(body: &Map<String, Value>, me: &str) -> ApiResult<Change> {
    let Some(Value::Bool(on)) = body.get("on") else {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, "on must be true or false");
    };
    let rev = match body.get("rev") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_i64() {
            Some(r) if r >= 0 => Some(r),
            _ => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "rev must be a whole number"),
        },
    };
    let allow = match body.get("allow") {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) => {
            let mut list: Vec<Entry> = Vec::with_capacity(a.len());
            for v in a {
                let (address, owner) = match v {
                    Value::String(s) => (s.clone(), None),
                    Value::Object(o) => match (o.get("address"), o.get("owner")) {
                        (Some(Value::String(s)), None | Some(Value::Null)) => (s.clone(), None),
                        (Some(Value::String(s)), Some(Value::String(t))) => (s.clone(), Some(t.clone())),
                        _ => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "an allow entry is an address or {address, owner}"),
                    },
                    _ => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "allow must list addresses on this hub"),
                };
                if address == me {
                    return refuse(StatusCode::UNPROCESSABLE_ENTITY, "an address cannot list itself");
                }
                if !valid_slug(&address) {
                    return refuse(StatusCode::UNPROCESSABLE_ENTITY, "allow must list addresses on this hub");
                }
                if !list.iter().any(|e| e.address == address) {
                    list.push(Entry { address, owner });
                }
            }
            if list.len() > MAX_ALLOWED {
                return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("a lockdown may list at most {MAX_ALLOWED} addresses"));
            }
            Some(list)
        }
        Some(_) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "allow must be a list of addresses"),
    };
    Ok(Change { on: *on, allow, rev })
}

/// Applies a change for `me` (held by `fp`) inside `tx`, whose caller holds
/// `me`'s identity row (`FOR UPDATE`): the lock a send to it takes too.
pub async fn apply(tx: &impl GenericClient, me: &str, fp: &str, ch: &Change) -> ApiResult<()> {
    // a previous owner's lockdown is not this one's
    db::execute(tx, "DELETE FROM lockdowns WHERE address = $1 AND owner_fp <> $2", &[&me, &fp]).await?;
    let stored = db::query_opt(tx, "SELECT on_, rev FROM lockdowns WHERE address = $1 FOR UPDATE", &[&me]).await?;
    let (was_on, stored_rev) = stored.map(|r| (r.get::<_, bool>(0), r.get::<_, i64>(1))).unwrap_or((false, -1));
    let rev = match ch.rev {
        Some(r) if r < stored_rev => {
            return Err(ApiError::Body(
                StatusCode::CONFLICT,
                json!({ "detail": format!("a newer lockdown change (rev {stored_rev}) is already in force"), "rev": stored_rev }),
            ))
        }
        Some(r) => r,
        None => stored_rev + 1,
    };
    // the entries; one naming an owner token is checked against the holder now
    let mut bound: Vec<String> = Vec::new();
    if let (true, Some(list)) = (ch.on, &ch.allow) {
        let wanted: Vec<String> = list.iter().map(|e| e.address.clone()).collect();
        let rows = db::query(tx, "SELECT slug, fingerprint FROM identities WHERE slug = ANY($1)", &[&wanted]).await?;
        let held: HashMap<String, String> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
        for e in list {
            if let Some(t) = &e.owner {
                if held.get(&e.address).map(|efp| super::pictures::owner_token(&e.address, efp)).as_ref() != Some(t) {
                    return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("@{} is now held by a different identity than the one named", e.address));
                }
            }
            bound.push(e.address.clone());
        }
    }
    let now = crate::clock::now();
    if ch.on {
        db::execute(
            tx,
            "INSERT INTO lockdowns (address, owner_fp, on_, rev, since) VALUES ($1, $2, true, $3, $4)
             ON CONFLICT (address) DO UPDATE SET on_ = true, rev = $3,
               since = CASE WHEN lockdowns.on_ THEN lockdowns.since ELSE $4 END",
            &[&me, &fp, &rev, &now],
        )
        .await?;
        if !was_on {
            db::execute(tx, "DELETE FROM lockdown_allow WHERE address = $1", &[&me]).await?;
        }
        if ch.allow.is_some() {
            db::execute(tx, "DELETE FROM lockdown_allow WHERE address = $1", &[&me]).await?;
            db::execute(
                tx,
                "INSERT INTO lockdown_allow (address, allowed) SELECT $1, x.a FROM unnest($2::text[]) AS x(a)",
                &[&me, &bound],
            )
            .await?;
        }
    } else {
        db::execute(
            tx,
            "INSERT INTO lockdowns (address, owner_fp, on_, rev, since) VALUES ($1, $2, false, $3, NULL)
             ON CONFLICT (address) DO UPDATE SET on_ = false, rev = $3, since = NULL",
            &[&me, &fp, &rev],
        )
        .await?;
        db::execute(tx, "DELETE FROM lockdown_allow WHERE address = $1", &[&me]).await?;
    }
    // everyone's view of this address may move: its roster row goes out again
    db::execute(tx, "UPDATE identities SET roster_seq = nextval('roster_seq') WHERE slug = $1", &[&me]).await?;
    Ok(())
}

/// The full answer for `me`: every entry, with whether its address is
/// registered now (`counts`) and the owner token of whoever holds it.
pub async fn answer(c: &impl GenericClient, me: &str) -> ApiResult<Value> {
    let l = db::query_opt(
        c,
        "SELECT l.on_, l.since, l.rev FROM lockdowns l JOIN identities i ON i.slug = l.address AND i.fingerprint = l.owner_fp
          WHERE l.address = $1",
        &[&me],
    )
    .await?;
    let Some(l) = l else {
        return Ok(json!({ "on": false, "since": null, "rev": null, "allow": [] }));
    };
    let rows = db::query(
        c,
        "SELECT a.allowed, j.fingerprint FROM lockdown_allow a
           LEFT JOIN identities j ON j.slug = a.allowed
          WHERE a.address = $1 ORDER BY a.allowed",
        &[&me],
    )
    .await?;
    let allow: Vec<Value> = rows
        .iter()
        .map(|r| {
            let (a, f): (String, Option<String>) = (r.get(0), r.get(1));
            json!({ "address": a, "owner": f.as_ref().map(|f| super::pictures::owner_token(&a, f)), "counts": f.is_some() })
        })
        .collect();
    Ok(json!({
        "on": l.get::<_, bool>(0),
        "since": l.get::<_, Option<chrono::DateTime<chrono::Utc>>>(1).map(crate::clock::iso),
        "rev": l.get::<_, i64>(2),
        "allow": allow,
    }))
}

/// After a committed change of `me`'s lockdown: the mirror, and every
/// parked sync (the row and, folded into the print, the `locked` view moved).
pub async fn after(hub: &Hub, c: &impl GenericClient, me: &str) -> ApiResult<()> {
    hub.lockdown.reload(c, me).await?;
    hub.presence.roster_changed();
    Ok(())
}

#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn set(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let body = req.json_object_strict_max(256 * 1024).await?;
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let me = one_address(&slugs, body.get("slug"))?;
    let ch = parse(&body, &me)?;
    let mut c = hub.db.get().await?;
    let tx = c.transaction().await?;
    // The address's row, as a send to it holds it: a send that has not
    // checked yet waits, and then sees this lockdown.
    let Some(row) = db::query_opt(&tx, "SELECT fingerprint FROM identities WHERE slug = $1 FOR UPDATE", &[&me]).await? else {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    };
    let fp: String = row.get(0);
    apply(&tx, &me, &fp, &ch).await?;
    tx.commit().await?;
    after(hub, &c, &me).await?;
    ok(answer(&c, &me).await?)
}

#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn get(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let asked = req.query("slug").map(|s| Value::String(s.to_string()));
    let me = one_address(&slugs, asked.as_ref())?;
    let c = hub.db.get().await?;
    ok(answer(&c, &me).await?)
}
