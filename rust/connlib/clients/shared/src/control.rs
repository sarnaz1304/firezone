use async_compression::tokio::bufread::GzipEncoder;
use bimap::BiMap;
use connlib_shared::control::ChannelError;
use connlib_shared::control::KnownError;
use connlib_shared::control::Reason;
use connlib_shared::messages::{DnsServer, GatewayResponse, IpDnsServer};
use connlib_shared::IpProvider;
use ip_network::IpNetwork;
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::{io, sync::Arc};

use crate::messages::{
    BroadcastGatewayIceCandidates, Connect, ConnectionDetails, EgressMessages,
    GatewayIceCandidates, InitClient, Messages,
};
use connlib_shared::{
    control::{ErrorInfo, PhoenixSenderWithTopic, Reference},
    messages::{GatewayId, ResourceDescription, ResourceId},
    Callbacks,
    Error::{self},
    Result,
};

use firezone_tunnel::{ClientState, Request, Tunnel};
use hickory_resolver::config::{NameServerConfig, Protocol, ResolverConfig};
use hickory_resolver::TokioAsyncResolver;
use reqwest::header::{CONTENT_ENCODING, CONTENT_TYPE};
use tokio::io::BufReader;
use tokio::sync::Mutex;
use tokio_util::codec::{BytesCodec, FramedRead};
use url::Url;

const DNS_PORT: u16 = 53;
const DNS_SENTINELS_V4: &str = "100.100.111.0/24";
const DNS_SENTINELS_V6: &str = "fd00:2021:1111:8000:100:100:111:0/120";

pub struct ControlPlane<CB: Callbacks> {
    pub tunnel: Arc<Tunnel<CB, ClientState>>,
    pub phoenix_channel: PhoenixSenderWithTopic,
    pub tunnel_init: Mutex<bool>,
    // It's a Mutex<Option<_>> because we need the init message to initialize the resolver
    // also, in platforms with split DNS and no configured upstream dns this will be None.
    //
    // We could still initialize the resolver with no nameservers in those platforms...
    pub fallback_resolver: parking_lot::Mutex<HashMap<IpAddr, TokioAsyncResolver>>,
}

fn effective_dns_servers(
    upstream_dns: Vec<DnsServer>,
    default_resolvers: Vec<IpAddr>,
) -> Vec<DnsServer> {
    if !upstream_dns.is_empty() {
        return upstream_dns;
    }

    let mut dns_servers = default_resolvers
        .into_iter()
        .filter(|ip| !IpNetwork::from_str(DNS_SENTINELS_V4).unwrap().contains(*ip))
        .filter(|ip| !IpNetwork::from_str(DNS_SENTINELS_V6).unwrap().contains(*ip))
        .peekable();

    if dns_servers.peek().is_none() {
        tracing::error!("No system default DNS servers available! Can't initialize resolver. DNS will be broken.");
        return Vec::new();
    }

    dns_servers
        .map(|ip| {
            DnsServer::IpPort(IpDnsServer {
                address: (ip, DNS_PORT).into(),
            })
        })
        .collect()
}

fn create_resolvers(
    sentinel_mapping: BiMap<IpAddr, DnsServer>,
) -> HashMap<IpAddr, TokioAsyncResolver> {
    sentinel_mapping
        .iter()
        .map(|(sentinel, srv)| {
            let mut resolver_config = ResolverConfig::new();
            resolver_config.add_name_server(NameServerConfig::new(srv.address(), Protocol::Udp));
            (
                *sentinel,
                TokioAsyncResolver::tokio(resolver_config, Default::default()),
            )
        })
        .collect()
}

fn sentinel_dns_mapping(dns: &[DnsServer]) -> BiMap<IpAddr, DnsServer> {
    let mut ip_provider = IpProvider::new(
        DNS_SENTINELS_V4.parse().unwrap(),
        DNS_SENTINELS_V6.parse().unwrap(),
    );

    dns.iter()
        .cloned()
        .map(|i| {
            (
                ip_provider
                    .get_proxy_ip_for(&i.ip())
                    .expect("We only support up to 256 IpV4 DNS servers and 256 IpV6 DNS servers"),
                i,
            )
        })
        .collect()
}

impl<CB: Callbacks + 'static> ControlPlane<CB> {
    async fn init(
        &mut self,
        InitClient {
            interface,
            resources,
        }: InitClient,
    ) -> Result<()> {
        let effective_dns_servers = effective_dns_servers(
            interface.upstream_dns.clone(),
            self.tunnel
                .callbacks()
                .get_system_default_resolvers()
                .ok()
                .flatten()
                .unwrap_or_default(),
        );

        let sentinel_mapping = sentinel_dns_mapping(&effective_dns_servers);

        {
            let mut init = self.tunnel_init.lock().await;
            if !*init {
                if let Err(e) = self
                    .tunnel
                    .set_interface(&interface, sentinel_mapping.clone())
                {
                    tracing::error!(error = ?e, "Error initializing interface");
                    return Err(e);
                } else {
                    *init = true;
                    tracing::info!("Firezone Started!");
                }

                for resource_description in resources {
                    self.add_resource(resource_description);
                }

                // Note: watch out here we're holding 2 mutexes
                *self.fallback_resolver.lock() = create_resolvers(sentinel_mapping);
            } else {
                tracing::info!("Firezone reinitializated");
            }
        }
        Ok(())
    }

    pub fn connect(
        &mut self,
        Connect {
            gateway_payload,
            resource_id,
            gateway_public_key,
            ..
        }: Connect,
    ) {
        match gateway_payload {
            GatewayResponse::ConnectionAccepted(gateway_payload) => {
                if let Err(e) = self.tunnel.received_offer_response(
                    resource_id,
                    gateway_payload.ice_parameters,
                    gateway_payload.domain_response,
                    gateway_public_key.0.into(),
                ) {
                    tracing::debug!(error = ?e, "Error accepting connection: {e:#?}");
                }
            }
            GatewayResponse::ResourceAccepted(gateway_payload) => {
                if let Err(e) = self
                    .tunnel
                    .received_domain_parameters(resource_id, gateway_payload.domain_response)
                {
                    tracing::debug!(error = ?e, "Error accepting resource: {e:#?}");
                }
            }
        }
    }

    pub fn add_resource(&self, resource_description: ResourceDescription) {
        if let Err(e) = self.tunnel.add_resource(resource_description) {
            tracing::error!(message = "Can't add resource", error = ?e);
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn resource_deleted(&self, id: ResourceId) {
        // TODO
    }

    fn connection_details(
        &self,
        ConnectionDetails {
            gateway_id,
            resource_id,
            relays,
            ..
        }: ConnectionDetails,
        reference: Option<Reference>,
    ) {
        let tunnel = Arc::clone(&self.tunnel);
        let mut control_signaler = self.phoenix_channel.clone();
        tokio::spawn(async move {
            let err = match tunnel
                .request_connection(resource_id, gateway_id, relays, reference)
                .await
            {
                Ok(Request::NewConnection(connection_request)) => {
                    if let Err(err) = control_signaler
                        // TODO: create a reference number and keep track for the response
                        .send_with_ref(
                            EgressMessages::RequestConnection(connection_request),
                            resource_id,
                        )
                        .await
                    {
                        err
                    } else {
                        return;
                    }
                }
                Ok(Request::ReuseConnection(connection_request)) => {
                    if let Err(err) = control_signaler
                        // TODO: create a reference number and keep track for the response
                        .send_with_ref(
                            EgressMessages::ReuseConnection(connection_request),
                            resource_id,
                        )
                        .await
                    {
                        err
                    } else {
                        return;
                    }
                }
                Err(err) => err,
            };

            tunnel.cleanup_connection(resource_id);
            tracing::error!("Error request connection details: {err}");
        });
    }

    async fn add_ice_candidate(
        &self,
        GatewayIceCandidates {
            gateway_id,
            candidates,
        }: GatewayIceCandidates,
    ) {
        for candidate in candidates {
            if let Err(e) = self.tunnel.add_ice_candidate(gateway_id, candidate).await {
                tracing::error!(err = ?e,"add_ice_candidate");
            }
        }
    }

    #[tracing::instrument(level = "trace", skip(self, msg))]
    pub async fn handle_message(
        &mut self,
        msg: Messages,
        reference: Option<Reference>,
    ) -> Result<()> {
        match msg {
            Messages::Init(init) => self.init(init).await?,
            Messages::ConfigChanged(_update) => {
                tracing::info!("Runtime config updates not yet implemented");
            }
            Messages::ConnectionDetails(connection_details) => {
                self.connection_details(connection_details, reference)
            }
            Messages::Connect(connect) => self.connect(connect),
            Messages::ResourceCreatedOrUpdated(resource) => self.add_resource(resource),
            Messages::ResourceDeleted(resource) => self.resource_deleted(resource.0),
            Messages::IceCandidates(ice_candidate) => self.add_ice_candidate(ice_candidate).await,
            Messages::SignedLogUrl(url) => {
                let Some(path) = self.tunnel.callbacks().roll_log_file() else {
                    return Ok(());
                };

                tokio::spawn(async move {
                    if let Err(e) = upload(path.clone(), url).await {
                        tracing::warn!(
                            "Failed to upload log file at path {path_display}: {e}. Not retrying.",
                            path_display = path.display()
                        );
                    }
                });
            }
        }
        Ok(())
    }

    // Errors here means we need to disconnect
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn handle_error(
        &mut self,
        reply_error: ChannelError,
        reference: Option<Reference>,
        topic: String,
    ) -> Result<()> {
        match (reply_error, reference) {
            (ChannelError::ErrorReply(ErrorInfo::Offline), Some(reference)) => {
                let Ok(resource_id) = reference.parse::<ResourceId>() else {
                    tracing::warn!("The portal responded with an Offline error. Is the Resource associated with any online Gateways or Relays?");
                    return Ok(());
                };
                // TODO: Rate limit the number of attempts of getting the relays before just trying a local network connection
                self.tunnel.cleanup_connection(resource_id);
            }
            (
                ChannelError::ErrorReply(ErrorInfo::Reason(Reason::Known(
                    KnownError::UnmatchedTopic,
                ))),
                _,
            ) => {
                if let Err(e) = self.phoenix_channel.get_sender().join_topic(topic).await {
                    tracing::debug!(err = ?e, "couldn't join topic: {e:#?}");
                }
            }
            (
                ChannelError::ErrorReply(ErrorInfo::Reason(Reason::Known(
                    KnownError::TokenExpired,
                ))),
                _,
            )
            | (ChannelError::ErrorMsg(Error::ClosedByPortal), _) => {
                return Err(Error::ClosedByPortal);
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn stats_event(&mut self) {
        tracing::debug!(target: "tunnel_state", stats = ?self.tunnel.stats());
    }

    pub async fn request_log_upload_url(&mut self) {
        tracing::info!("Requesting log upload URL from portal");

        let _ = self
            .phoenix_channel
            .send(EgressMessages::CreateLogSink {})
            .await;
    }

    pub async fn handle_tunnel_event(&mut self, event: Result<firezone_tunnel::Event<GatewayId>>) {
        match event {
            Ok(firezone_tunnel::Event::SignalIceCandidate { conn_id, candidate }) => {
                if let Err(e) = self
                    .phoenix_channel
                    .send(EgressMessages::BroadcastIceCandidates(
                        BroadcastGatewayIceCandidates {
                            gateway_ids: vec![conn_id],
                            candidates: vec![candidate],
                        },
                    ))
                    .await
                {
                    tracing::error!("Failed to signal ICE candidate: {e}")
                }
            }
            Ok(firezone_tunnel::Event::ConnectionIntent {
                resource,
                connected_gateway_ids,
                reference,
            }) => {
                if let Err(e) = self
                    .phoenix_channel
                    .clone()
                    .send_with_ref(
                        EgressMessages::PrepareConnection {
                            resource_id: resource.id(),
                            connected_gateway_ids,
                        },
                        reference,
                    )
                    .await
                {
                    tracing::error!("Failed to prepare connection: {e}");

                    // TODO: Clean up connection in `ClientState` here?
                }
            }
            Ok(firezone_tunnel::Event::DnsQuery(query)) => {
                // Until we handle it better on a gateway-like eventloop, making sure not to block the loop
                let Some(resolver) = self
                    .fallback_resolver
                    .lock()
                    .get(&query.query.destination())
                    .cloned()
                else {
                    return;
                };

                let tunnel = self.tunnel.clone();
                tokio::spawn(async move {
                    let response = resolver.lookup(&query.name, query.record_type).await;
                    if let Err(err) = tunnel.write_dns_lookup_response(response, query.query) {
                        tracing::debug!(err = ?err, name = query.name, record_type = ?query.record_type, "DNS lookup failed: {err:#}");
                    }
                });
            }
            Ok(firezone_tunnel::Event::RefreshResources { connections }) => {
                let mut control_signaler = self.phoenix_channel.clone();
                tokio::spawn(async move {
                    for connection in connections {
                        let resource_id = connection.resource_id;
                        if let Err(err) = control_signaler
                            .send_with_ref(EgressMessages::ReuseConnection(connection), resource_id)
                            .await
                        {
                            tracing::warn!(%resource_id, ?err, "failed to refresh resource dns: {err:#?}");
                        }
                    }
                });
            }
            Err(e) => {
                tracing::error!("Tunnel failed: {e}");
            }
        }
    }
}

async fn upload(path: PathBuf, url: Url) -> io::Result<()> {
    tracing::info!(path = %path.display(), %url, "Uploading log file");

    let file = tokio::fs::File::open(&path).await?;

    let response = reqwest::Client::new()
        .put(url)
        .header(CONTENT_TYPE, "text/plain")
        .header(CONTENT_ENCODING, "gzip")
        .body(reqwest::Body::wrap_stream(FramedRead::new(
            GzipEncoder::new(BufReader::new(file)),
            BytesCodec::default(),
        )))
        .send()
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    let status_code = response.status();

    if !status_code.is_success() {
        let body = response
            .text()
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        tracing::warn!(%body, %status_code, "Failed to upload logs");

        return Err(io::Error::new(
            io::ErrorKind::Other,
            "portal returned non-successful exit code",
        ));
    }

    Ok(())
}
