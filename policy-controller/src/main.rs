#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use futures::{future, prelude::*};
use linkerd_policy_controller::k8s::DefaultAllow;
use linkerd_policy_controller_core::IpNet;
use std::net::SocketAddr;
use structopt::StructOpt;
use tokio::{sync::watch, time};
use tracing::{debug, info, instrument};
use warp::Filter;

#[derive(Debug, StructOpt)]
#[structopt(name = "policy", about = "A policy resource prototype")]
struct Args {
    #[structopt(long, default_value = "0.0.0.0:8080")]
    admin_addr: SocketAddr,

    #[structopt(long, default_value = "0.0.0.0:8090")]
    grpc_addr: SocketAddr,

    #[structopt(long, default_value = "0.0.0.0:8443")]
    admission_addr: SocketAddr,

    /// Network CIDRs of pod IPs.
    ///
    /// The default includes all private networks.
    #[structopt(
        long,
        default_value = "10.0.0.0/8,100.64.0.0/10,172.16.0.0/12,192.168.0.0/16"
    )]
    cluster_networks: IpNets,

    #[structopt(long, default_value = "cluster.local")]
    identity_domain: String,

    #[structopt(long, default_value = "all-unauthenticated")]
    default_allow: DefaultAllow,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let Args {
        admin_addr,
        grpc_addr,
        admission_addr,
        identity_domain,
        cluster_networks: IpNets(cluster_networks),
        default_allow,
    } = Args::from_args();

    let (drain_tx, drain_rx) = drain::channel();

    let client = kube::Client::try_default()
        .await
        .context("failed to initialize kubernetes client")?;

    let (ready_tx, ready_rx) = watch::channel(false);
    let admin = tokio::spawn(linkerd_policy_controller::admin::serve(
        admin_addr, ready_rx,
    ));

    const DETECT_TIMEOUT: time::Duration = time::Duration::from_secs(10);
    let (handle, index_task) = linkerd_policy_controller::k8s::index(
        client.clone(),
        ready_tx,
        cluster_networks,
        identity_domain,
        default_allow,
        DETECT_TIMEOUT,
    );
    let index_task = tokio::spawn(index_task);

    let grpc = tokio::spawn(grpc(grpc_addr, handle, drain_rx));

    let admission_handler = linkerd_policy_controller::admission::Admission(client);
    let routes = warp::path::end()
        .and(warp::body::json())
        .and(warp::any().map(move || admission_handler.clone()))
        .and_then(linkerd_policy_controller::admission::mutate_handler)
        .with(warp::trace::request());

    let admission = tokio::spawn(warp::serve(warp::post().and(routes))
        .tls()
        .cert_path("/var/run/linkerd/tls/tls.crt")
        .key_path("/var/run/linkerd/tls/tls.key")
        .run(admission_addr));

    tokio::select! {
       _ = shutdown(drain_tx) => Ok(()),
       res = grpc => match res {
           Ok(res) => res.context("grpc server failed"),
           Err(e) if e.is_cancelled() => Ok(()),
           Err(e) => Err(e).context("grpc server panicked"),
       },
       res = index_task => match res {
           Ok(e) => Err(e).context("indexer failed"),
           Err(e) if e.is_cancelled() => Ok(()),
           Err(e) => Err(e).context("indexer panicked"),
       },
       res = admin => match res {
           Ok(res) => res.context("admin server failed"),
           Err(e) if e.is_cancelled() => Ok(()),
           Err(e) => Err(e).context("admin server panicked"),
       },
       res = admission => res.context("admission server failed"),
    }
}

#[derive(Debug)]
struct IpNets(Vec<IpNet>);

impl std::str::FromStr for IpNets {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        s.split(',')
            .map(|n| n.parse().map_err(Into::into))
            .collect::<Result<Vec<IpNet>>>()
            .map(Self)
    }
}

#[instrument(skip(handle, drain))]
async fn grpc(
    addr: SocketAddr,
    handle: linkerd_policy_controller_k8s_index::Reader,
    drain: drain::Watch,
) -> Result<()> {
    let server = linkerd_policy_controller_grpc::Server::new(handle, drain.clone());
    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    tokio::pin! {
        let srv = server.serve(addr, close_rx.map(|_| {}));
    }
    info!(%addr, "gRPC server listening");
    tokio::select! {
        res = (&mut srv) => res?,
        handle = drain.signaled() => {
            let _ = close_tx.send(());
            handle.release_after(srv).await?
        }
    }
    Ok(())
}

async fn shutdown(drain: drain::Signal) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            debug!("Received ctrl-c");
        },
        _ = sigterm() => {
            debug!("Received SIGTERM");
        }
    }
    info!("Shutting down");
    drain.drain().await;
}

async fn sigterm() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => term.recv().await,
        _ => future::pending().await,
    };
}
