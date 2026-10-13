//! `POST /api/typing` (feature `typing`): one heartbeat. Its sender is
//! writing to these addresses right now; it sends another about every half
//! second while it writes and simply stops when it stops (`crate::typing`).
//! The state lives in memory only: never stored, never mail, gone within
//! seconds. Beyond the credentials nothing is read from the database: an
//! address that is not registered here has no syncs to wake.
//!
//! Body: `{"slug"?, "to": [address, ...]}` and nothing else, so no content
//! can ever ride along.

use std::sync::Arc;
use std::time::Instant;

use http::StatusCode;
use serde_json::{json, Value};

use super::mail::{authed_callers, one_address};
use super::{ok, refuse, ApiResult, Hub, Req};
use crate::typing::{TooMany, MAX_BODY, MAX_RECIPIENTS};

#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn typing(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let body = req.json_object_strict_max(MAX_BODY).await?;
    let callers = authed_callers(hub, req).await?;
    if callers.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    if let Some(k) = body.keys().find(|k| *k != "slug" && *k != "to") {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("a heartbeat carries only slug and to, not {k}"));
    }
    let slugs: Vec<String> = callers.iter().map(|c| c.slug.clone()).collect();
    let from = one_address(&slugs, body.get("slug"))?;
    let to = recipients(body.get("to"))?;
    let counted = match hub.typing.beat(&from, &to, Instant::now()) {
        Ok(c) => c,
        Err(TooMany) => return refuse(StatusCode::TOO_MANY_REQUESTS, "this address is typing to too many addresses at once"),
    };
    hub.presence.typing_changed(counted.iter().map(String::as_str));
    ok(json!({ "ok": true, "counted": counted.len() }))
}

/// `to`: 1-`MAX_RECIPIENTS` addresses, duplicates dropped.
fn recipients(v: Option<&Value>) -> ApiResult<Vec<String>> {
    let bad = || refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("to must be a list of 1-{MAX_RECIPIENTS} addresses"));
    let Some(Value::Array(list)) = v else { return bad() };
    let mut out: Vec<String> = Vec::new();
    for x in list {
        let Some(s) = x.as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 200) else { return bad() };
        if !out.iter().any(|o| o == s) {
            out.push(s.to_string());
        }
    }
    if out.is_empty() || out.len() > MAX_RECIPIENTS {
        return bad();
    }
    Ok(out)
}
