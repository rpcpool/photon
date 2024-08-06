use async_std::stream::StreamExt;
use clap::Parser;
use hyper::body::Bytes;
use log::{error, info};
use photon_indexer::common::{
    fetch_block_parent_slot, get_network_start_slot, setup_logging, setup_metrics, LoggingFormat,
};
use photon_indexer::ingester::fetchers::BlockStreamConfig;
use photon_indexer::snapshot::{
    get_snapshot_files_with_slots, load_byte_stream_from_snapshot_directory,
};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};
use std::convert::Infallible;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

/// Photon Snapshotter: a utility to create snapshots of Photon's state at regular intervals.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Port to expose the local snapshotter API
    #[arg(short, long, default_value_t = 8825)]
    port: u16,

    /// URL of the RPC server
    #[arg(short, long, default_value = "http://127.0.0.1:8899")]
    rpc_url: String,

    /// The start slot to begin indexing from
    #[arg(short, long)]
    start_slot: Option<u64>,

    /// Logging format
    #[arg(short, long, default_value_t = LoggingFormat::Standard)]
    logging_format: LoggingFormat,

    /// Max number of blocks to fetch concurrently
    #[arg(short, long)]
    max_concurrent_block_fetches: Option<usize>,

    /// Snapshot directory
    #[arg(long)]
    snapshot_dir: String,

    /// Incremental snapshot slots
    #[arg(long, default_value_t = 1000)]
    incremental_snapshot_interval_slots: u64,

    /// Full snapshot slots
    #[arg(long, default_value_t = 100_000)]
    snapshot_interval_slots: u64,

    /// Yellowstone gRPC URL
    #[arg(short, long, default_value = None)]
    grpc_url: Option<String>,

    /// Metrics endpoint in the format `host:port`
    #[arg(long, default_value = None)]
    metrics_endpoint: Option<String>,
}

async fn continously_run_snapshotter(
    block_stream_config: BlockStreamConfig,
    full_snapshot_interval_slots: u64,
    incremental_snapshot_interval_slots: u64,
    snapshot_dir: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        photon_indexer::snapshot::update_snapshot(
            block_stream_config,
            incremental_snapshot_interval_slots,
            full_snapshot_interval_slots,
            Path::new(&snapshot_dir),
        )
        .await;
    })
}

async fn stream_bytes(snapshot_dir: String) -> Result<Response<Body>, hyper::http::Error> {
    let snapshot_dir = PathBuf::new().join(snapshot_dir);
    let byte_stream = load_byte_stream_from_snapshot_directory(&snapshot_dir);

    // Convert byte_stream to a stream of bytes for hyper Body
    let byte_body = byte_stream.map(|result| match result {
        Ok(byte) => Ok::<hyper::body::Bytes, std::io::Error>(Bytes::from(vec![byte])),
        Err(err) => {
            error!("Error reading byte: {:?}", err);
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Stream Error",
            ))
        }
    });

    // Create a response with the byte stream as the body
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/octet-stream")
        .body(Body::wrap_stream(byte_body))
}

/// Handle HTTP requests and route to the appropriate handler
async fn handle_request(
    req: Request<Body>,
    snapshot_dir: String,
) -> Result<Response<Body>, hyper::http::Error> {
    match req.uri().path() {
        "/download" => match stream_bytes(snapshot_dir).await {
            Ok(response) => Ok(response),
            Err(e) => {
                error!("Error creating stream: {:?}", e);
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("Internal Server Error"))
            }
        },
        "/health" | "/readiness" => Response::builder()
            .status(StatusCode::OK)
            .body(Body::from("OK")),
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("404 Not Found")),
    }
    .map_err(|e| {
        error!("Error building response: {:?}", e);
        e
    })
}
/// Create the server and bind it to the specified port, returning a JoinHandle
async fn create_server(port: u16, snapshot_dir: String) -> tokio::task::JoinHandle<()> {
    // Define the address to bind the server to
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    // Spawn the server task
    tokio::spawn(async move {
        // Create a make_service function to handle incoming requests
        let make_svc = make_service_fn(move |_conn| {
            let layer = ServiceBuilder::new().layer(TraceLayer::new_for_http());
            let snapshot_dir = snapshot_dir.clone();
            async move {
                Ok::<_, Infallible>(layer.service(service_fn(move |req| {
                    handle_request(req, snapshot_dir.clone())
                })))
            }
        });

        // Create the server using hyper
        let server = Server::bind(&addr).serve(make_svc);
        info!("Listening on http://{}", addr);

        // Run the server and handle errors
        if let Err(e) = server.await {
            error!("Server error: {}", e);
        }
    })
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    setup_logging(args.logging_format);
    setup_metrics(args.metrics_endpoint);

    let rpc_client = Arc::new(RpcClient::new_with_timeout_and_commitment(
        args.rpc_url.clone(),
        Duration::from_secs(10),
        CommitmentConfig::confirmed(),
    ));

    // Create snapshot directory if it doesn't exist
    if !Path::new(&args.snapshot_dir).exists() {
        std::fs::create_dir_all(&args.snapshot_dir).unwrap();
    }

    info!("Starting snapshotter...");
    let last_indexed_slot = match args.start_slot {
        Some(start_slot) => fetch_block_parent_slot(rpc_client.clone(), start_slot).await,
        None => {
            let snapshot_files =
                get_snapshot_files_with_slots(Path::new(&args.snapshot_dir)).unwrap();
            if snapshot_files.is_empty() {
                get_network_start_slot(rpc_client.clone()).await
            } else {
                snapshot_files.last().unwrap().end_slot
            }
        }
    };
    info!("Starting from slot {}", last_indexed_slot + 1);

    let snapshotter_handle = continously_run_snapshotter(
        BlockStreamConfig {
            rpc_client: rpc_client.clone(),
            max_concurrent_block_fetches: args.max_concurrent_block_fetches.unwrap_or(20),
            last_indexed_slot: last_indexed_slot,
            geyser_url: args.grpc_url.clone(),
        },
        args.incremental_snapshot_interval_slots,
        args.snapshot_interval_slots,
        args.snapshot_dir.clone(),
    )
    .await;

    // Start the server on the specified port
    let server_handle = create_server(args.port, args.snapshot_dir.clone()).await;

    // Handle shutdown signal
    match tokio::signal::ctrl_c().await {
        Ok(()) => {
            snapshotter_handle.abort();
            snapshotter_handle
                .await
                .expect_err("Snapshotter should have been aborted");
            server_handle.abort();
            server_handle
                .await
                .expect_err("Server should have been aborted");
        }
        Err(err) => {
            error!("Unable to listen for shutdown signal: {}", err);
        }
    }
}
