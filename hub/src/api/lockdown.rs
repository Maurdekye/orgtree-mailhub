//! `POST /api/lockdown {"on": true|false, "allow"?: [...], "slug"?}` and
//! `GET /api/lockdown?slug=` (feature `lockdown`, v2.6.0;
//! `crate::lockdown`). Both answer the caller's own lockdown:
//! `{"on", "allow", "since"}`. `on: true` with `allow` replaces the list;
//! without it, keeps the list (a new lockdown starts with none). `on: false`
//! lifts it, list and all.

use std::sync::Arc;

use http::StatusCode;
use serde_json::Value;

use super::mail::{authed, one_address, valid_slug};
use super::{ok, refuse, ApiResult, Hub, Req};
use crate::db;
use crate::lockdown::MAX_ALLOWED;

#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn set(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let body = req.json_object_strict_max(64 * 1024).await?;
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let me = one_address(&slugs, body.get("slug"))?;
    let Some(Value::Bool(on)) = body.get("on") else {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, "on must be true or false");
    };
    let allow: Option<Vec<String>> = match body.get("allow") {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) => {
            let mut list = Vec::with_capacity(a.len());
            for v in a {
                match v {
                    Value::String(s) if valid_slug(s) && *s != me => list.push(s.clone()),
                    Value::String(s) if *s == me => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "an address cannot list itself"),
                    _ => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "allow must list addresses on this hub"),
                }
            }
            list.sort();
            list.dedup();
            if list.len() > MAX_ALLOWED {
                return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("a lockdown may list at most {MAX_ALLOWED} addresses"));
            }
            Some(list)
        }
        Some(_) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "allow must be a list of addresses"),
    };
    let mut c = hub.db.get().await?;
    let tx = c.transaction().await?;
    // The address's row, as a send to it holds it: a send that has not
    // checked yet waits, and then sees this lockdown.
    let Some(row) = db::query_opt(&tx, "SELECT fingerprint FROM identities WHERE slug = $1 FOR UPDATE", &[&me]).await? else {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    };
    let fp: String = row.get(0);
    if *on {
        db::execute(
            &tx,
            "INSERT INTO lockdowns (address, owner_fp, since) VALUES ($1, $2, $3) ON CONFLICT (address) DO NOTHING",
            &[&me, &fp, &crate::clock::now()],
        )
        .await?;
        if let Some(list) = &allow {
            db::execute(&tx, "DELETE FROM lockdown_allow WHERE address = $1", &[&me]).await?;
            db::execute(&tx, "INSERT INTO lockdown_allow (address, allowed) SELECT $1, unnest($2::text[])", &[&me, list]).await?;
        }
    } else {
        db::execute(&tx, "DELETE FROM lockdowns WHERE address = $1", &[&me]).await?;
    }
    // everyone's view of this address may move: its roster row goes out again
    db::execute(&tx, "UPDATE identities SET roster_seq = nextval('roster_seq') WHERE slug = $1", &[&me]).await?;
    tx.commit().await?;
    let now = hub.lockdown.reload(&c, &me).await?;
    // every parked sync answers: the row (and, folded into the print, the
    // `locked` view) moved
    hub.presence.roster_changed();
    ok(crate::lockdown::own(now.as_ref()))
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
    let now = hub.lockdown.reload(&c, &me).await?;
    ok(crate::lockdown::own(now.as_ref()))
}
