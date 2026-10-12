//! Isolated picture protocol proof; uses the existing disposable PostgreSQL harness.
mod support;
use std::sync::Arc;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use mailhub::Hub;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use support::*;

fn image(n: u64, color: u8) -> Value {
    let mut bytes = Vec::new();
    {
        let mut e = png::Encoder::new(&mut bytes, 2, 2);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.write_header().unwrap().write_image_data(&[color;16]).unwrap();
    }
    json!({"revision":format!("{n:020}-00000000000000000000000000000001"),"sha256":hex::encode(Sha256::digest(&bytes)),"width":2,"height":2,"bytes":bytes.len(),"data":B64.encode(bytes)})
}
async fn put(hub: &Arc<Hub>, auth: &str, slug: &str, picture: Value) -> Resp {
    Call::new("POST","/api/profile-picture").auth(auth).json(json!({"slug":slug,"picture":picture})).send(hub).await
}
async fn get(hub: &Arc<Hub>, auth: &str, slug: &str) -> Resp {
    Call::new("GET",&format!("/api/profile-picture?slug={slug}")).auth(auth).send(hub).await
}

#[test]
fn bounded_normalized_png_and_revision_validation() {
    use mailhub::api::pictures::{validate, valid_revision};
    let good = image(1, 200);
    assert_eq!(validate(&good).unwrap(),good);
    for bad in ["", "1-abc", "00000000000000000000-00000000000000000000000000000001", "18446744073709551616-00000000000000000000000000000001"] {
        assert!(!valid_revision(bad));
    }
    for (field,value) in [("sha256",json!("bad")),("width",json!(513)),("height",json!(1)),("bytes",json!(999)),("data",json!("not png"))] {
        let mut bad=good.clone(); bad[field]=value; assert!(validate(&bad).is_err(),"{field}");
    }
    let mut big=good.clone(); big["data"]=json!("a".repeat(400_000)); assert!(validate(&big).is_err());
    let tomb=json!({"revision":good["revision"],"data":null});
    assert!(validate(&tomb).unwrap()["sha256"].is_null());
}

#[tokio::test]
async fn picture_authority_convergence_and_lifetime() {
    let pg=TestPg::start(); let url=pg.fresh_db("pictures").await;
    let root=std::env::var_os("HUB_PICTURE_TEST_DATA").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let data=root.join(format!("hub-pictures-{}",token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub=hub(&url,&data,&[]).await;
    let ids=[("person.pic","person"),("org.pic","org"),("session.pic","chat")];
    for (slug,kind) in ids {
        let r=Call::new("POST","/api/register").auth(pair(slug,"picture-proof-secret")).json(json!({"slug":slug,"kind":kind})).send(&hub).await;
        assert_eq!(r.code(),200,"{}",r.text());
    }
    let pa=pair("person.pic","picture-proof-secret"); let oa=pair("org.pic","picture-proof-secret");
    let initial=image(1,10);
    assert_eq!(get(&hub,"","person.pic").await.code(),401);
    assert_eq!(put(&hub,&pa,"org.pic",initial.clone()).await.code(),401);
    assert_eq!(put(&hub,&pair("session.pic","picture-proof-secret"),"session.pic",initial.clone()).await.code(),403);
    assert_eq!(get(&hub,&pa,"session.pic").await.code(),404);
    assert_eq!(put(&hub,&pa,"person.pic",initial.clone()).await.json()["changed"],true);
    assert_eq!(put(&hub,&oa,"org.pic",initial.clone()).await.code(),200);
    assert_eq!(put(&hub,&pa,"person.pic",initial.clone()).await.json()["changed"],false);
    assert_eq!(get(&hub,&oa,"person.pic").await.json()["picture"],initial);
    assert_eq!(put(&hub,&pa,"person.pic",image(1,11)).await.code(),409);
    let roster=Call::new("GET","/api/roster").auth(&pa).send(&hub).await.json();
    let row=roster["roster"].as_array().unwrap().iter().find(|v|v["slug"]=="person.pic").unwrap();
    let identity_token=row["picture_owner"].as_str().unwrap().to_string();
    assert_eq!(identity_token.len(),64);
    assert_ne!(identity_token,mailhub::auth::fingerprint("picture-proof-secret"));
    assert_eq!(get(&hub,&oa,"person.pic").await.json()["picture_owner"],identity_token);
    assert_eq!(row["picture"]["revision"],initial["revision"]);
    assert!(row["picture"].get("data").is_none());
    let snap=Call::new("POST","/api/sync").auth(&oa).json(json!({"device_id":"picture-reader","wait":0})).send(&hub).await.json();
    let tomb=json!({"revision":image(3,0)["revision"],"data":null});
    assert_eq!(put(&hub,&pa,"person.pic",tomb.clone()).await.code(),200);
    let (low,high)=tokio::join!(put(&hub,&pa,"person.pic",image(2,20)),put(&hub,&pa,"person.pic",tomb.clone()));
    assert_eq!((low.code(),high.code()),(200,200));
    let current=get(&hub,&pa,"person.pic").await.json();
    assert!(current["picture"]["data"].is_null());
    assert_eq!(current["picture"]["revision"],tomb["revision"]);
    let changed=Call::new("POST","/api/sync").auth(&oa).json(json!({"device_id":"picture-reader","cursor":snap["cursor"],"wait":0})).send(&hub).await.json();
    assert!(changed["roster"].as_array().unwrap().iter().any(|r|r["slug"]=="person.pic"&&r["picture"]["revision"]==tomb["revision"]));
    // An old client's text registration cannot reset/remove a picture revision.
    let register=||Call::new("POST","/api/register").auth(&pa).json(json!({"slug":"person.pic","kind":"person"}));
    assert_eq!(register().send(&hub).await.code(),200);
    assert_eq!(get(&hub,&pa,"person.pic").await.json()["picture"],current["picture"]);
    assert_eq!(Call::new("POST","/api/unregister").auth(&pa).send(&hub).await.code(),200);
    assert_eq!(get(&hub,&oa,"person.pic").await.code(),404);
    assert_eq!(register().send(&hub).await.code(),200);
    assert_eq!(put(&hub,&pa,"person.pic",initial.clone()).await.json()["changed"],false);
    assert_eq!(get(&hub,&oa,"person.pic").await.json()["picture_owner"],identity_token);
    assert!(get(&hub,&pa,"person.pic").await.json()["picture"]["data"].is_null());
    // An explicit observed later edit can replace removal.
    assert_eq!(put(&hub,&pa,"person.pic",image(4,40)).await.json()["changed"],true);
    // A released address can be claimed by another identity with independent
    // ownership and version history, including no picture at registration.
    assert_eq!(Call::new("POST","/api/unregister").auth(&pa).send(&hub).await.code(),200);
    let new_auth=pair("person.pic","different-picture-proof-secret");
    assert_eq!(Call::new("POST","/api/register").auth(&new_auth).json(json!({"slug":"person.pic","kind":"person","org_name":"new owner"})).send(&hub).await.code(),200);
    let replacement=get(&hub,&oa,"person.pic").await.json();
    assert!(replacement["picture"].is_null());
    assert_ne!(replacement["picture_owner"],identity_token);
    assert_eq!(put(&hub,&new_auth,"person.pic",image(1,90)).await.json()["changed"],true);
    let changed=Call::new("POST","/api/sync").auth(&oa).json(json!({"device_id":"picture-reader","cursor":changed["cursor"],"wait":0})).send(&hub).await.json();
    assert!(changed["roster"].as_array().unwrap().iter().any(|r|r["slug"]=="person.pic"&&r["picture_owner"]==replacement["picture_owner"]));
    // Picture endpoints use the same device credentials as the rest of the API.
    let identity = SigningKey::from_bytes(&[42; 32]);
    let device = SigningKey::from_bytes(&[43; 32]);
    let public = |key: &SigningKey| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes());
    let sign = |key: &SigningKey, message: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.sign(message.as_bytes()).to_bytes());
    assert_eq!(Call::new("POST","/api/identity").auth(&oa).json(json!({"identity_key":public(&identity)})).send(&hub).await.code(),200);
    let created = "2026-10-08T18:00:00.000Z";
    let certificate = mailhub::api::keys::device_certificate("org.pic", "picture-device", &public(&device), created);
    assert_eq!(Call::new("POST","/api/devices").json(json!({"slug":"org.pic","device_id":"picture-device","public_key":public(&device),"created":created,"signature":sign(&identity,&certificate),"name":"picture-device"})).send(&hub).await.code(),200);
    let device_auth = |key: &SigningKey| {
        let ms = chrono::Utc::now().timestamp_millis();
        format!("org.pic:dev1.picture-device.{ms}.{}",sign(key,&mailhub::auth::call_message("org.pic","picture-device",ms)))
    };
    assert_eq!(get(&hub,&device_auth(&device),"person.pic").await.code(),200);
    assert_eq!(put(&hub,&device_auth(&device),"org.pic",image(5,50)).await.json()["changed"],true);
    assert_eq!(put(&hub,&device_auth(&device),"person.pic",image(6,60)).await.code(),401);
    assert_eq!(put(&hub,&device_auth(&identity),"org.pic",image(6,60)).await.code(),401);
    assert_eq!(Call::new("POST","/api/identity").auth(&oa).json(json!({"shared_key":false})).send(&hub).await.code(),200);
    assert_eq!(get(&hub,&oa,"org.pic").await.code(),401);
    assert_eq!(put(&hub,&oa,"org.pic",image(6,60)).await.code(),401);
    assert_eq!(put(&hub,&device_auth(&device),"org.pic",image(6,60)).await.json()["changed"],true);
    assert_eq!(get(&hub,&device_auth(&device),"org.pic").await.json()["picture"]["revision"],image(6,60)["revision"]);
    let c=sql_client(&url).await;
    assert_eq!(c.query_one("SELECT count(*) FROM mailhub.messages",&[]).await.unwrap().get::<_,i64>(0),0);
    assert_eq!(c.query_one("SELECT count(*) FROM mailhub.attachments",&[]).await.unwrap().get::<_,i64>(0),0);
    assert_eq!(mailhub::db::latest_schema(),10,"pictures add no schema: 10 is the reserved question-card message meta");
    drop(hub); drop(c); drop(pg); std::fs::remove_dir_all(data).unwrap();
}
