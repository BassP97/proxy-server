use hyper::{Body, Client, Method, Request, Response, Server};
use serde::Serialize;
use std::{
    collections::HashMap,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    net::TcpStream,
    signal,
    sync::Mutex,
};

#[derive(Default, Clone, Serialize)]
struct SiteMetrics {
    visits: usize,
}

#[derive(Serialize)]
struct MetricsView {
    bandwidth_usage: String,
    top_sites: Vec<SiteVisit>,
}

#[derive(Serialize)]
struct SiteVisit {
    url: String,
    visits: usize,
}

struct ProxyMetrics {
    bandwidth_usage: Arc<Mutex<usize>>,
    site_visits: Arc<Mutex<HashMap<String, SiteMetrics>>>,
}

impl Default for ProxyMetrics {
    fn default() -> Self {
        Self {
            bandwidth_usage: Arc::new(Mutex::new(0)),
            site_visits: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

struct CountingStream<T> {
    inner: T,
    counter: Arc<Mutex<usize>>,
}

impl<T: AsyncRead + Unpin> AsyncRead for CountingStream<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let original_filled = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        let new_filled = buf.filled().len();
        let bytes_read = new_filled - original_filled;

        if result.is_ready() {
            let counter = self.counter.clone();
            tokio::spawn(async move {
                let mut guard = counter.lock().await;
                *guard += bytes_read;
            });
        }

        result
    }
}

impl ProxyMetrics {
    async fn to_view(&self) -> MetricsView {
        let bandwidth = *self.bandwidth_usage.lock().await;
        let sites = self.site_visits.lock().await;

        let mut top_sites: Vec<_> = sites
            .iter()
            .map(|(url, metrics)| SiteVisit {
                url: url.clone(),
                visits: metrics.visits,
            })
            .collect();

        top_sites.sort_by(|a, b| b.visits.cmp(&a.visits));
        let bandwidth_usage_mb = bandwidth / (1024 * 1024);
        let output_bandwidth = if bandwidth_usage_mb == 0 {
            format!("{}KB", bandwidth / 1024)
        } else {
            format!("{}MB", bandwidth_usage_mb)
        };

        MetricsView {
            bandwidth_usage: output_bandwidth,
            top_sites,
        }
    }
}

async fn authenticate(headers: &hyper::HeaderMap) -> bool {
    let auth_header = match headers.get("Proxy-Authorization") {
        Some(header) => header,
        None => return false,
    };

    let auth_str = match auth_header.to_str() {
        Ok(str) => str,
        Err(_) => return false,
    };

    if !auth_str.starts_with("Basic ") {
        return false;
    }

    let credentials = match base64::decode(&auth_str[6..]) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(str) => str,
            Err(_) => return false,
        },
        Err(_) => return false,
    };

    // Expected format: username:password
    let expected = "username:password"; // You should store this securely
    credentials == expected
}

async fn tunnel(
    req: Request<Body>,
    metrics: Arc<ProxyMetrics>,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr = req.uri().authority().unwrap().to_string();

    // make sure https://www.google.com is the same as http://www.google.com
    let site_visited = req
        .uri()
        .to_string()
        .replace(":443", "")
        .replace("https://", "")
        .replace("www.", "");

    metrics
        .site_visits
        .lock()
        .await
        .entry(site_visited)
        .or_default()
        .visits += 1;

    let client_io = hyper::upgrade::on(req).await?;
    let mut server = TcpStream::connect(&addr).await?;

    let (client_reader, mut client_writer) = tokio::io::split(client_io);
    let (server_reader, mut server_writer) = server.split();

    let mut counting_client_reader = CountingStream {
        inner: client_reader,
        counter: metrics.bandwidth_usage.clone(),
    };

    let mut counting_server_reader = CountingStream {
        inner: server_reader,
        counter: metrics.bandwidth_usage.clone(),
    };

    let client_to_server =
        tokio::io::copy(&mut counting_client_reader, &mut server_writer);

    let server_to_client =
        tokio::io::copy(&mut counting_server_reader, &mut client_writer);

    let _ = tokio::try_join!(client_to_server, server_to_client)?;

    Ok(())
}

async fn handle_proxy_request(
    req: Request<Body>,
    metrics: Arc<ProxyMetrics>,
) -> Result<Response<Body>, hyper::Error> {
    if !authenticate(req.headers()).await {
        return Ok(Response::builder()
            .status(407)
            .header("Proxy-Authenticate", "Basic realm=\"Proxy\"")
            .body(Body::from("Proxy Authentication Required"))
            .unwrap());
    }

    // https case
    if req.method() == Method::CONNECT {
        if let Some(addr) =
            req.uri().authority().map(|auth| auth.to_string())
        {
            tokio::task::spawn(async move {
                match tunnel(req, metrics).await {
                    Ok(_) => (),
                    Err(e) => eprintln!("Failed to tunnel: {}", e),
                }
            });

            return Ok(Response::new(Body::empty()));
        }

        return Ok(Response::builder()
            .status(400)
            .body(Body::from("Bad Request"))
            .unwrap());
    }

    let request_body_size = req
        .headers()
        .get("content-length")
        .and_then(|v| {
            v.to_str().ok().and_then(|s| s.parse::<usize>().ok())
        })
        .unwrap_or(0);
    let request_header_size = req
        .headers()
        .iter()
        .map(|(k, v)| {
            k.as_str().len()
                + v.to_str().unwrap().len()
                + ": ".len()
                + "\r\n".len()
        })
        .sum::<usize>()
        + "\r\n".len();

    let site_visited = req
        .uri()
        .to_string()
        .replace("http://", "")
        .replace("www.", "")
        .replace(":8080", "")
        .split("/")
        .next()
        .unwrap()
        .to_string();

    metrics
        .site_visits
        .lock()
        .await
        .entry(site_visited)
        .or_default()
        .visits += 1;

    let client = Client::new();
    let res = client.request(req).await?;

    let (parts, body) = res.into_parts();
    let response_header_size = parts
        .headers
        .iter()
        .map(|(k, v)| {
            k.as_str().len()
                + v.to_str().unwrap().len()
                + ": ".len()
                + "\r\n".len()
        })
        .sum::<usize>()
        // CLRF after headers, before body
        + "\r\n".len();

    let total_size = parts
        .headers
        .get("content-length")
        .and_then(|v| {
            v.to_str().ok().and_then(|s| s.parse::<usize>().ok())
        })
        .unwrap_or(0)
        + response_header_size
        + request_header_size
        + request_body_size;

    metrics
        .bandwidth_usage
        .lock()
        .await
        .checked_add(total_size)
        .expect("bandwidth overflow");

    // hacky way to appease the borrow checker... todo: refactor
    Ok(Response::from_parts(parts, body))
}

async fn handle_metrics_request(
    metrics: Arc<ProxyMetrics>,
) -> Result<Response<Body>, hyper::Error> {
    let view = metrics.to_view().await;
    let body = serde_json::to_string(&view).unwrap();

    Ok(Response::new(body.into()))
}

#[tokio::main]
async fn main() {
    let metrics = Arc::new(ProxyMetrics::default());
    // we need this for later so we can print the final metrics on shutdown
    let metrics_clone = metrics.clone();

    let addr = ([127, 0, 0, 1], 3000).into();
    let make_service = hyper::service::make_service_fn(move |_| {
        let metrics = metrics.clone();
        async move {
            Ok::<_, hyper::Error>(hyper::service::service_fn(move |req| {
                let metrics = metrics.clone();
                async move {
                    if req.uri().path() == "/metrics" {
                        handle_metrics_request(metrics).await
                    } else {
                        handle_proxy_request(req, metrics).await
                    }
                }
            }))
        }
    });
    let server = Server::bind(&addr).serve(make_service);
    let graceful = server.with_graceful_shutdown(async {
        signal::ctrl_c().await.expect("failed to listen for ctrl+c");
        println!("Shutting down...");
    });

    println!("Proxy server running on http://{}", addr);

    if let Err(e) = graceful.await {
        eprintln!("server error: {}", e);
    }

    match metrics_clone.to_view().await {
        metrics_view => {
            if let Ok(json) = serde_json::to_string_pretty(&metrics_view) {
                println!("Final metrics:\n{}", json);
            }
        }
    }
}
