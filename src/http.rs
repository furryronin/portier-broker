use config::Config;
use error::BrokerError;
use futures::future::{self, Future, FutureResult};
use futures::Stream;
use handlers::return_to_relier;
use hyper::{self, StatusCode, Error as HyperError};
use hyper::header::{ContentType, StrictTransportSecurity, CacheControl, CacheDirective};
use hyper::server::{Request, Response, Service as HyperService};
use hyper_staticfile::Static;
use hyper_tls::HttpsConnector;
use std::cell::{RefCell, Ref};
use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_core::reactor::Handle;
use url::{Url, form_urlencoded};


header! { (ContentSecurityPolicy, "Content-Security-Policy") => [String] }
header! { (XContentSecurityPolicy, "X-Content-Security-Policy") => [String] }
header! { (XContentTypeOptions, "X-Content-Type-Options") => [String] }
header! { (XXSSProtection, "X-XSS-Protection") => [String] }
header! { (XFrameOptions, "X-Frame-Options") => [String] }


/// A boxed future. Unlike the regular `BoxFuture`, this is not `Send`.
/// This means we also do not use the `boxed()` method.
pub type BoxFuture<T, E> = Box<Future<Item=T, Error=E>>;

/// The default type of client we use for outgoing requests
pub type Client = hyper::Client<HttpsConnector<hyper::client::HttpConnector>>;

/// Helper function to create a HTTPS client
pub fn create_client(handle: &Handle) -> Client {
    // TODO: Better handle management
    let connector = HttpsConnector::new(4, handle)
        .expect("could not initialize https connector");
    hyper::Client::configure().connector(connector).build(handle)
}


/// Additional context for a request
pub struct Context {
    /// Redirect URI of the relying party
    pub redirect_uri: Option<Url>,
}


/// Short-hand
pub type ContextHandle = Arc<RefCell<Context>>;
/// Result type of handlers
pub type HandlerResult = BoxFuture<Response, BrokerError>;
/// Handler function type
pub type Handler = fn(&Service, Request, ContextHandle) -> HandlerResult;
/// Router function type
pub type Router = fn(&Request) -> Option<Handler>;


// HTTP service
pub struct Service {
    /// The application configuration
    pub app: Arc<Config>,
    /// A Tokio reactor handle
    pub handle: Handle,
    /// The routing function
    router: Router,
    /// The static file serving service
    static_: Static,
}

impl Service {
    pub fn new<P: Into<PathBuf>>(handle: &Handle, app: &Arc<Config>, router: Router, path: P) -> Service {
        Service {
            app: app.clone(),
            handle: handle.clone(),
            router: router,
            static_: Static::new(handle, path).with_cache_headers(app.static_ttl),
        }
    }
}

impl HyperService for Service {
    type Request = Request;
    type Response = Response;
    type Error = HyperError;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn call(&self, req: Request) -> Self::Future {
        info!("{} {}", req.method(), req.path());

        let handler = match (self.router)(&req) {
            Some(handler) => handler,
            None => return self.static_.call(req),
        };

        let ctx = Arc::new(RefCell::new(Context {
            redirect_uri: None,
        }));

        let f = handler(self, req, ctx.clone());

        let app = self.app.clone();
        let f = f.or_else(move |err| {
            let ctx = ctx.borrow();
            handle_error(&*app, &ctx, err)
        });

        let f = f.map(|mut res| {
            set_headers(&mut res);
            res
        });

        Box::new(f)
    }
}


/// Handle an `BrokerError` and create a response.
///
/// Our service calls this on error. We handle all these errors, and always
/// return a response.
///
/// When we handle an error, we want to:
///
///  - Log internal errors and errors related to communcation with providers.
///  - Not log input errors, such as missing parameters.
///
///  - Hide the details of internal errors from the user.
///  - Show a description for input and provider errors.
///
///  - Return the error to the relier via redirect, as per the OAuth2 spec.
///  - Always show something to the user, even if we cannot redirect.
///
/// The large match-statement below handles all these scenario's properly, and
/// sets proper response codes for each category.
fn handle_error(app: &Config, ctx: &Ref<Context>, err: BrokerError) -> FutureResult<Response, HyperError> {
    match (err, ctx.redirect_uri.as_ref()) {
        (err @ BrokerError::Input(_), Some(_)) => {
            return_to_relier(app, ctx, &[
                ("error", "invalid_request"),
                ("error_description", err.description()),
            ])
        },
        (err @ BrokerError::Input(_), None) => {
            let res = Response::new()
                .with_status(StatusCode::BadRequest)
                .with_header(ContentType::html())
                .with_body(app.templates.error.render(&[
                    ("error", err.description()),
                ]));
            future::ok(res)
        },
        (err @ BrokerError::Provider(_), Some(_)) => {
            error!("{}", err);
            return_to_relier(app, ctx, &[
                ("error", "temporarily_unavailable"),
                ("error_description", &err.description().to_string()),
            ])
        },
        (err @ BrokerError::Provider(_), None) => {
            error!("{}", err);
            let res = Response::new()
                .with_status(StatusCode::ServiceUnavailable)
                .with_header(ContentType::html())
                .with_body(app.templates.error.render(&[
                    ("error", &err.description().to_string()),
                ]));
            future::ok(res)
        },
        (err, Some(_)) => {
            error!("{}", err);
            return_to_relier(app, ctx, &[
                ("error", "server_error"),
            ])
        },
        (err, None) => {
            error!("{}", err);
            let res = Response::new()
                .with_status(StatusCode::InternalServerError)
                .with_header(ContentType::html())
                .with_body(app.templates.error.render(&[
                    ("error", "internal server error"),
                ]));
            future::ok(res)
        },
    }
}


/// Mutate a response to set common headers.
fn set_headers(res: &mut Response) {
    let headers = res.headers_mut();

    // Specify a tight content security policy. We need to be able to POST
    // redirect anywhere, and run our own scripts.
    let csp = vec![
        "sandbox allow-scripts allow-forms",
        "default-src 'none'",
        "script-src 'self'",
        "style-src 'self'",
        "form-action *",
    ].join("; ");

    headers.set(StrictTransportSecurity::excluding_subdomains(31_536_000u64));
    headers.set(ContentSecurityPolicy(csp.clone()));
    headers.set(XContentSecurityPolicy(csp));
    headers.set(XContentTypeOptions("nosniff".to_string()));
    headers.set(XXSSProtection("1; mode=block".to_string()));
    headers.set(XFrameOptions("DENY".to_string()));

    // Default to disable caching completely.
    if !headers.has::<CacheControl>() {
        headers.set(CacheControl(vec![
            CacheDirective::NoCache,
            CacheDirective::NoStore,
        ]));
    }
}


/// Parse a form-encoded string into a `HashMap`.
pub fn parse_form_encoded(input: &[u8]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (key, value) in form_urlencoded::parse(input) {
        map.insert(key.into_owned(), value.into_owned());
    }
    map
}


/// Parse the request query string into a `HashMap`.
pub fn parse_query(req: &Request) -> HashMap<String, String> {
    if let Some(query) = req.query() {
        parse_form_encoded(query.as_bytes())
    } else {
        HashMap::new()
    }
}


/// Parse the request form-encoded body into a `HashMap`.
pub fn parse_form_encoded_body(req: Request) -> BoxFuture<HashMap<String, String>, BrokerError> {
    let f = req.body().concat2()
        .map_err(|err| err.into())
        .map(|chunk| parse_form_encoded(&chunk));
    Box::new(f)
}


#[cfg(test)]
mod tests {
    use hyper::server::Response;
    use super::set_headers;

    #[test]
    fn sets_expected_headers() {
        let mut res = Response::new();
        set_headers(&mut res);

        let headers = res.headers();
        assert!(headers.get_raw("Strict-Transport-Security").is_some());
        assert!(headers.get_raw("Content-Security-Policy").is_some());
        assert!(headers.get_raw("X-Content-Security-Policy").is_some());
        assert!(headers.get_raw("X-Content-Type-Options").is_some());
        assert!(headers.get_raw("X-XSS-Protection").is_some());
        assert!(headers.get_raw("X-Frame-Options").is_some());
        assert!(headers.get_raw("Cache-Control").is_some());
    }
}
