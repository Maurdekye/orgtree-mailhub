//! `POST /api/blocks {"address", "blocked": true|false, "slug"?}` and
//! `GET /api/blocks?slug=` (feature `blocking`, v2.6.0; `crate::blocks`).
//! Both answer the caller's list, oldest first: `{"blocked": [...]}`.

use std::sync::Arc;

use http::StatusCode;
use serde_json::{json, Value};

use super::mail::{authed, one_address, valid_slug};
use super::{ok, refuse, ApiResult, Hub, Req};
use crate::blocks::MAX_BLOCKS;
use crate::db;

#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn set(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let body = req.json_object_strict_max(4096).await?;
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let me = one_address(&slugs, body.get("slug"))?;
    let address = match body.get("address") {
        Some(Value::String(a)) if valid_slug(a) => a.clone(),
        _ => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "address must be an address on this hub"),
    };
    if address == me {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, "an address cannot block itself");
    }
    let Some(Value::Bool(on)) = body.get("blocked") else {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, "blocked must be true or false");
    };
    let mut c = hub.db.get().await?;
    let tx = c.transaction().await?;
    // The blocker's row, as a send to it holds it: a send that has not
    // checked yet waits, and then sees this block.
    let Some(row) = db::query_opt(&tx, "SELECT fingerprint FROM identities WHERE slug = $1 FOR UPDATE", &[&me]).await? else {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    };
    let fp: String = row.get(0);
    if *on {
        let had = db::query_opt(&tx, "SELECT 1 FROM blocks WHERE blocker = $1 AND blocked = $2", &[&me, &address]).await?.is_some();
        if !had {
            let n: i64 = db::query_one(&tx, "SELECT count(*) FROM blocks WHERE blocker = $1", &[&me]).await?.get(0);
            if n as usize >= MAX_BLOCKS {
                return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("an address may block at most {MAX_BLOCKS} others"));
            }
            db::execute(
                &tx,
                "INSERT INTO blocks (blocker, blocker_fp, blocked, created_at) VALUES ($1, $2, $3, $4)",
                &[&me, &fp, &address, &crate::clock::now()],
            )
            .await?;
        }
    } else {
        db::execute(&tx, "DELETE FROM blocks WHERE blocker = $1 AND blocked = $2", &[&me, &address]).await?;
    }
    tx.commit().await?;
    let list = hub.blocks.reload(&c, &me).await?;
    // both sides' prints moved: their devices in use hear it now
    hub.presence.wake_sync([me.as_str(), address.as_str()]);
    ok(json!({ "blocked": list }))
}

#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn list(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let asked = req.query("slug").map(|s| Value::String(s.to_string()));
    let me = one_address(&slugs, asked.as_ref())?;
    let c = hub.db.get().await?;
    let list = hub.blocks.reload(&c, &me).await?;
    ok(json!({ "blocked": list }))
}
