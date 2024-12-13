#![cfg(unix)]

use std::mem::ManuallyDrop;

use anyhow::Context as _;
use containerd_client::services::v1::leases_client::LeasesClient;
use containerd_client::services::v1::DeleteRequest;
use containerd_client::tonic::transport::Channel;
use containerd_client::{tonic, with_namespace};
use tonic::Request;

// Adds lease info to grpc header
// https://github.com/containerd/containerd/blob/8459273f806e068e1a6bacfaf1355bbbad738d5e/docs/garbage-collection.md#using-grpc
#[macro_export]
macro_rules! with_lease {
    ($req : ident, $ns: expr, $lease_id: expr) => {{
        let mut req = Request::new($req);
        let md = req.metadata_mut();
        // https://github.com/containerd/containerd/blob/main/namespaces/grpc.go#L27
        md.insert("containerd-namespace", $ns.parse().unwrap());
        md.insert("containerd-lease", $lease_id.parse().unwrap());
        req
    }};
}

#[derive(Debug)]
pub(crate) struct LeaseGuard {
    inner: Option<LeaseGuardInner>,
}

#[derive(Debug)]
pub(crate) struct LeaseGuardInner {
    client: LeasesClient<Channel>,
    req: tonic::Request<DeleteRequest>,
}

impl LeaseGuard {
    pub fn new(
        client: LeasesClient<Channel>,
        id: impl Into<String>,
        namespace: impl AsRef<str>,
    ) -> Self {
        let id = id.into();
        let req = DeleteRequest { id, sync: false };
        let req = with_namespace!(req, namespace.as_ref());
        let inner = Some(LeaseGuardInner { client, req });
        Self { inner }
    }

    // Release a LeaseGuard in a way that we can await for it to complete.
    // The alternative to `release` is to `drop` the LeaseGuard, but in that case we can't await for its completion.
    pub async fn release(self) -> anyhow::Result<()> {
        let mut this = ManuallyDrop::new(self);
        this.inner.take().unwrap().release().await?;
        Ok(())
    }

    pub fn id(&self) -> &'_ str {
        &self.inner.as_ref().unwrap().req.get_ref().id
    }
}

impl LeaseGuardInner {
    async fn release(mut self) -> anyhow::Result<()> {
        self.client
            .delete(self.req)
            .await
            .context("Failed to remove lease")?;
        Ok(())
    }
}

// Provides a best effort for dropping a lease of the content.  If the lease cannot be dropped, it will log a warning
impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let inner = self.inner.take().unwrap();
        tokio::spawn(async move {
            match inner.release().await {
                Ok(()) => log::info!("removed lease"),
                Err(err) => log::warn!("error removing lease: {err}"),
            }
        });
    }
}