#[macro_use] extern crate lazy_static;
extern crate dotenv;
extern crate futures;
extern crate hyper;
extern crate regex;
extern crate rustc_serialize;
extern crate tokio_core as tokio;
extern crate uuid;
mod flow;

use dotenv::dotenv;
use flow::Flow;
use futures::{future, stream, Future, Stream};
use hyper::header::{ContentLength, ContentType};
use hyper::server::{Http, Service, Request, Response};
use hyper::{Method, StatusCode};
use regex::Regex;
use std::collections::HashMap;
use std::sync::{Arc, Barrier, RwLock};
use std::{env, thread};
use tokio::reactor::Core;

type SharedFlow = Arc<RwLock<Flow>>;

#[derive(Clone)]
struct FluxService {
    flow_bucket: Arc<RwLock<HashMap<String, SharedFlow>>>,
}

type ResponseFuture = Box<Future<Item = Response, Error = hyper::Error>>;

impl FluxService {
    fn new() -> Self {
        FluxService {
            flow_bucket: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn handle_new(&self, req: Request, _route: regex::Captures) -> ResponseFuture {
        let flow = Flow::new(None);
        let flow_id = flow.id.clone();
        {
            let mut bucket = self.flow_bucket.write().unwrap();
            bucket.insert(flow_id.clone(), Arc::new(RwLock::new(flow)));
        }
        let body = flow_id.into_bytes();
        future::ok(Response::new()
                   .with_header(ContentType::plaintext())
                   .with_header(ContentLength(body.len() as u64))
                   .with_body(body)).boxed()
    }

    fn handle_push(&self, req: Request, route: regex::Captures) -> ResponseFuture {
        let flow_id = route.get(1).unwrap().as_str();
        let flow = {
            let bucket = self.flow_bucket.read().unwrap();
            if let Some(flow) = bucket.get(flow_id) {
                flow.clone()
            } else {
                return future::ok(Response::new().with_status(StatusCode::NotFound)).boxed();
            }
        };

        // Chain a EOF on the stream to flush the remaining chunk.
        let body_stream = req.body()
            .map(|chunk| Some(chunk))
            .chain(stream::once(Ok(None)));
        // Read the body and push chunks.
        let init_chunk = Vec::<u8>::with_capacity(flow::MAX_SIZE);
        body_stream.fold(init_chunk, move |mut rem_chunk, chunk| {
            let (flush_chunk, ret_chunk) = if let Some(chunk) = chunk {
                if rem_chunk.len() + chunk.len() >= flow::MAX_SIZE {
                    let caplen = flow::MAX_SIZE - rem_chunk.len();
                    rem_chunk.extend_from_slice(&chunk[..caplen]);
                    (Some(rem_chunk), chunk[caplen..].to_vec())
                } else {
                    rem_chunk.extend_from_slice(&chunk);
                    (None, rem_chunk)
                }
            } else {
                // EOF, flush the remaining chunk.
                if rem_chunk.len() > 0 {
                    (Some(rem_chunk), Vec::new())
                } else {
                    (None, Vec::new())
                }
            };
            if let Some(flush_chunk) = flush_chunk {
                let mut flow = flow.write().unwrap();
                flow.push(&flush_chunk)
                    .map(|_| ret_chunk)
                    .map_err(|_| hyper::error::Error::Incomplete)
            } else {
                Ok(ret_chunk)
            }
        }).and_then(|_| {
            let body = "Ok";
            Ok(Response::new()
                .with_header(ContentType::plaintext())
                .with_header(ContentLength(body.len() as u64))
                .with_body(body))
        }).or_else(|_| {
            Ok(Response::new().with_status(StatusCode::InternalServerError))
        }).boxed()
    }

    fn handle_pull(&self, req: Request, route: regex::Captures) -> ResponseFuture {
        let flow_id = route.get(1).unwrap().as_str();
        future::ok(Response::new().with_status(StatusCode::InternalServerError)).boxed()
    }
}

impl Service for FluxService {
    type Request = Request;
    type Response = Response;
    type Error = hyper::Error;
    type Future = ResponseFuture;

    fn call(&self, req: Request) -> Self::Future {
        lazy_static! {
            static ref PATTERN_NEW: Regex = Regex::new(r"/new").unwrap();
            static ref PATTERN_PUSH: Regex = Regex::new(r"/([a-f0-9]{32})/push").unwrap();
            static ref PATTERN_PULL: Regex = Regex::new(r"/([a-f0-9]{32})/pull").unwrap();
        }

        match req.method() {
            &Method::Post => {
                let path = &req.path().to_owned();

                if let Some(route) = PATTERN_NEW.captures(path) {
                    self.handle_new(req, route)
                } else if let Some(route) = PATTERN_PUSH.captures(path) {
                    self.handle_push(req, route)
                } else if let Some(route) = PATTERN_PULL.captures(path) {
                    self.handle_pull(req, route)
                } else {
                    future::ok(Response::new().with_status(StatusCode::NotFound)).boxed()
                }
            }
            _ => future::ok(Response::new().with_status(StatusCode::MethodNotAllowed)).boxed()
        }
    }
}

fn start_service(addr: std::net::SocketAddr, num_worker: usize, block: bool)
                 -> Option<std::net::SocketAddr> {
    let service = FluxService::new();

    let upstream_listener = std::net::TcpListener::bind(&addr).unwrap();
    let mut workers = Vec::with_capacity(num_worker);
    let barrier = Arc::new(Barrier::new(num_worker.checked_add(1).unwrap()));

    for idx in 0..num_worker {
        let moved_addr = addr.clone();
        let moved_listener = upstream_listener.try_clone().unwrap();
        let moved_service = service.clone();
        let moved_barrier = barrier.clone();
        let worker = thread::spawn(move || {
            let mut core = Core::new().unwrap();
            let handle = core.handle();
            let listener = tokio::net::TcpListener::from_listener(moved_listener,
                                                                  &moved_addr,
                                                                  &handle).unwrap();
            let http = Http::new();
            let acceptor = listener.incoming().for_each(|(io, addr)| {
                http.bind_connection(&handle, io, addr, moved_service.clone());
                Ok(())
            });
            println!("Worker #{} is started.", idx);
            moved_barrier.wait();
            core.run(acceptor).unwrap();
        });
        workers.push(worker);
    }

    barrier.wait();

    if block {
        for worker in workers {
            worker.join().unwrap();
        }
        None
    } else {
        Some(upstream_listener.local_addr().unwrap())
    }
}

fn main() {
    dotenv().ok();
    let addr: std::net::SocketAddr = env::var("SERVER_ADDRESS").unwrap().parse().unwrap();
    let num_worker: usize = env::var("NUM_WORKER").unwrap().parse().unwrap();
    start_service(addr, num_worker, true);
}

#[cfg(test)]
mod tests {
    use futures::{Future, Stream};
    use hyper::Method::Post;
    use hyper::client::{Client, Request};
    use hyper::status::StatusCode;
    use regex::Regex;
    use std::str;
    use super::start_service;
    use tokio::reactor::Core;

    fn spawn_server() -> String {
        let port = start_service("127.0.0.1:0".parse().unwrap(), 1, false).unwrap().port();
        format!("http://127.0.0.1:{}", port)
    }

    #[test]
    fn handle_new() {
        let prefix = &spawn_server();
        let mut core = Core::new().unwrap();
        let client = Client::new(&core.handle());

        let req = Request::new(Post, format!("{}/new", prefix).parse().unwrap());
        core.run(client.request(req).and_then(|res| {
            assert_eq!(res.status(), StatusCode::Ok);
            res.body().concat().and_then(|body| {
                let flow_id = str::from_utf8(&body).unwrap();
                assert!(Regex::new("[a-f0-9]{32}").unwrap().find(flow_id).is_some());
                Ok(())
            })
        })).unwrap();
    }

    #[test]
    fn handle_push_fetch() {
        let prefix = &spawn_server();
        let mut core = Core::new().unwrap();
        let client = Client::new(&core.handle());

        let mut flow_id = String::new();

        let req = Request::new(Post, format!("{}/new", prefix).parse().unwrap());
        core.run(client.request(req).and_then(|res| {
            assert_eq!(res.status(), StatusCode::Ok);
            res.body().concat().and_then(|body| {
                flow_id = str::from_utf8(&body).unwrap().to_owned();
                Ok(())
            })
        })).unwrap();

        let mut req = Request::new(Post, format!("{}/{}/push", prefix, flow_id).parse().unwrap());
        req.set_body(b"The quick brown fox jumps over the lazy dog" as &[u8]);
        core.run(client.request(req).and_then(|res| {
            assert_eq!(res.status(), StatusCode::Ok);
            res.body().concat().and_then(|body| {
                let status = str::from_utf8(&body).unwrap().to_owned();
                assert_eq!(status, "Ok");
                Ok(())
            })
        })).unwrap();
    }
}