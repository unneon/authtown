#![feature(backtrace, let_else)]

mod crypto;
mod error;
mod session;
mod user;
mod util;

use crate::crypto::Crypto;
use crate::session::Session;
use crate::user::UserStore;
use crate::util::env_var;
use cookie::Cookie;
use error::Error;
use hyper::header::{COOKIE, LOCATION, SET_COOKIE};
use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use serde::{Deserialize, Serialize};
use slog::{error, info, o, Drain, Logger};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tera::Tera;
use tokio_postgres::NoTls;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
struct AuthRegisterRequest {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct AuthLoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct Ctx {
    user: Option<CtxUser>,
}

#[derive(Serialize)]
struct CtxUser {
    id: i32,
}

fn main() {
    let log = init_logger();
    match run_async(log.clone()) {
        Ok(()) => (),
        Err(e) => {
            error!(log, "Critical server failure"; e.log_message(), e.log_backtrace());
        }
    }
}

fn init_logger() -> Logger {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::CompactFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    Logger::root(drain, o!())
}

fn run_async(log: Logger) -> Result<(), Error> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?;
    runtime.block_on(run(log))
}

async fn run(log: Logger) -> Result<(), Error> {
    let database_url = env_var("DATABASE_URL")?;
    let database_config = database_url.parse::<tokio_postgres::Config>()?;
    let (database, database_task) = database_config.connect(NoTls).await?;
    let database = Arc::new(database);
    let tera = Arc::new(Tera::new("templates/*.html")?);
    let crypto = Arc::new(Crypto::from_env()?);
    let address = SocketAddr::from(([127, 0, 0, 1], 8000));
    let service_factory = make_service_fn(|conn: &AddrStream| {
        let log = log.clone();
        let database = database.clone();
        let tera = tera.clone();
        let crypto = crypto.clone();
        let conn_ip = conn.remote_addr().ip();
        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                let req_id = Uuid::new_v4();
                let req_log = log.new(o!("request" => req_id.to_string()));
                info!(req_log, "HTTP request received"; "method" => req.method().to_string(), "endpoint" => req.uri().to_string(), "ip" => conn_ip.to_string());
                catcher(req, database.clone(), tera.clone(), crypto.clone(), req_log)
            }))
        }
    });
    let server = Server::bind(&address).serve(service_factory);
    tokio::spawn(database_task);
    info!(log, "Listening on http://{}", address);
    Ok(server.await?)
}

async fn catcher(
    req: Request<Body>,
    database: Arc<tokio_postgres::Client>,
    tera: Arc<Tera>,
    crypto: Arc<Crypto>,
    log: Logger,
) -> Result<Response<Body>, Error> {
    match router(req, database, tera, crypto, &log).await {
        Ok(resp) => {
            info!(log, "HTTP request successful"; "status" => resp.status().as_u16());
            Ok(resp)
        }
        Err(e) => {
            error!(log, "HTTP request failed"; "status" => 500, e.log_message(), e.log_backtrace());
            Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(e.to_string().into())
                .unwrap())
        }
    }
}

async fn router(
    mut req: Request<Body>,
    database: Arc<tokio_postgres::Client>,
    tera: Arc<Tera>,
    crypto: Arc<Crypto>,
    log: &Logger,
) -> Result<Response<Body>, Error> {
    let cookies = get_cookies(&req)?;
    let session = Session::from_cookies(&cookies, &*crypto)?;
    if let Some(session) = &session {
        info!(log, "User is logged in"; session, session.user());
    } else {
        info!(log, "User is not logged in");
    }
    let context = tera::Context::from_serialize(Ctx {
        user: session.as_ref().map(|session| CtxUser {
            id: session.user().id,
        }),
    })?;
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/") => Ok(Response::builder()
            .status(StatusCode::OK)
            .body(tera.render("index.html", &context)?.into())
            .unwrap()),
        (&Method::POST, "/auth/register") => {
            let body_bytes = hyper::body::to_bytes(req.body_mut()).await?;
            let body: AuthRegisterRequest = serde_urlencoded::from_bytes(&body_bytes)?;
            info!(log, "Registering a new account"; "username" => &body.username);
            let user_store = UserStore::new(&*database);
            let user = user_store.insert(&body.username, &body.password).await?;
            let session = Session::create(user, &*crypto);
            info!(log, "Logged in after registration"; &session);
            Ok(Response::builder()
                .status(StatusCode::SEE_OTHER)
                .header(LOCATION, "/")
                .header(SET_COOKIE, session.cookie_login().to_string())
                .body(Body::empty())
                .unwrap())
        }
        (&Method::POST, "/auth/login") => {
            let body_bytes = hyper::body::to_bytes(req.body_mut()).await?;
            let body: AuthLoginRequest = serde_urlencoded::from_bytes(&body_bytes)?;
            info!(log, "Logging in"; "username" => &body.username);
            let user_store = UserStore::new(&*database);
            let user = user_store
                .get_and_verify(&body.username, &body.password)
                .await?;
            let session = Session::create(user, &*crypto);
            info!(log, "Logged in"; user, &session);
            Ok(Response::builder()
                .status(StatusCode::SEE_OTHER)
                .header(LOCATION, "/")
                .header(SET_COOKIE, session.cookie_login().to_string())
                .body(Body::empty())
                .unwrap())
        }
        (&Method::POST, "/auth/logout") => {
            info!(log, "Logging out");
            Ok(Response::builder()
                .status(StatusCode::SEE_OTHER)
                .header(LOCATION, "/")
                .header(SET_COOKIE, Session::cookie_logout().to_string())
                .body(Body::empty())
                .unwrap())
        }
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap()),
    }
}

fn get_cookies(request: &Request<Body>) -> Result<HashMap<&str, Cookie>, Error> {
    let Some(header) = request.headers().get(COOKIE) else { return Ok(HashMap::new()); };
    Ok(header
        .to_str()?
        .split("; ")
        .map(|cookie| Cookie::parse(cookie).map(|cookie| (cookie.name_raw().unwrap(), cookie)))
        .collect::<Result<HashMap<_, _>, _>>()?)
}
