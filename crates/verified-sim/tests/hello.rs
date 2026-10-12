//! Two turmoil hosts, one HTTP request, and a double-run determinism check.

use std::{error::Error, net::Ipv4Addr, time::Duration};

use axum::{Router, routing::get};
use http_body_util::{BodyExt as _, Empty};
use hyper::body::Bytes;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use verified_server::shim::net;
use verified_sim::trace::{self, TraceEvent};

const PORT: u16 = 9999;
const TRACE_CAPACITY: usize = 16;

/// Runs one request from `client` to `server` and returns the trace.
fn hello(seed: u64) -> Result<Vec<TraceEvent>, Box<dyn Error>> {
    let (sink, reader) = trace::bounded(TRACE_CAPACITY);

    let mut sim = turmoil::Builder::new()
        .rng_seed(seed)
        .enable_random_order()
        .min_message_latency(Duration::from_millis(1))
        .max_message_latency(Duration::from_millis(40))
        .build();

    let server_sink = sink.clone();
    sim.host("server", move || {
        let sink = server_sink.clone();
        async move {
            let router = Router::new().route(
                "/hello",
                get(move || {
                    let sink = sink.clone();
                    async move {
                        sink.record("server", "GET /hello".to_owned());
                        "hello from the daemon"
                    }
                }),
            );
            let listener = net::bind((Ipv4Addr::UNSPECIFIED, PORT).into()).await?;
            axum::serve(listener, router).await?;
            Ok(())
        }
    });

    sim.client("client", async move {
        let client = Client::builder(TokioExecutor::new()).build::<_, Empty<Bytes>>(net::Connector);
        let request =
            hyper::Request::get(format!("http://server:{PORT}/hello")).body(Empty::new())?;
        let response = client.request(request).await?;
        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes();
        sink.record(
            "client",
            format!("{status} {}", String::from_utf8_lossy(&body)),
        );
        Ok(())
    });

    sim.run()?;
    Ok(reader.drain())
}

#[test]
fn client_reaches_server_over_simulated_network() -> Result<(), Box<dyn Error>> {
    let events = hello(7)?;
    let observations: Vec<(&str, &str)> = events
        .iter()
        .map(|event| (event.host, event.observation.as_str()))
        .collect();
    assert_eq!(
        observations,
        [
            ("server", "GET /hello"),
            ("client", "200 OK hello from the daemon"),
        ],
        "the server sees the request before the client sees the response",
    );
    Ok(())
}

#[test]
fn same_seed_same_trace() -> Result<(), Box<dyn Error>> {
    let mut traces = Vec::new();
    for seed in [1, 2, 3, 42, 0xDEAD_BEEF] {
        let first = hello(seed)?;
        let second = hello(seed)?;
        assert!(!first.is_empty(), "seed {seed} recorded no events");
        assert_eq!(first, second, "seed {seed} produced two different traces");
        traces.push(first);
    }
    // The check is only meaningful if the seed actually steers the run.
    assert!(
        traces.windows(2).any(|pair| pair.first() != pair.last()),
        "every seed produced the same trace, so the seed is not reaching the network"
    );
    Ok(())
}
