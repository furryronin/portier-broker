use crypto;
use error::BrokerResult;
use http::{Context, return_to_relier};
use hyper::server::Response;


// Session data stored by bridges.
#[derive(Serialize,Deserialize)]
#[serde(tag = "type")]
pub enum BridgeData {
    Email(email::EmailBridgeData),
    Oidc(oidc::OidcBridgeData),
}


// Once a bridge has authenticated the user, this function can be used to finished up the redirect
// to the relying party with a token generated by us.
pub fn complete_auth(ctx: &Context) -> BrokerResult<Response> {
    let data = ctx.session_data.as_ref().expect("complete_auth called without a session");
    ctx.app.store.remove_session(&ctx.session_id)?;
    let aud = data.redirect_uri.origin().ascii_serialization();
    let jwt = crypto::create_jwt(&ctx.app, &data.email, &data.email_addr, &aud, &data.nonce);
    Ok(return_to_relier(ctx, &[("id_token", &jwt)]))
}


pub mod email;
pub mod oidc;