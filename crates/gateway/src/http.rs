//! HTTP API and web UI.
//!
//! Two audiences with different credentials: a person holding the admin token,
//! and agents holding their node key. An agent can register its tunnel
//! identity and read its own forwards, and nothing else — if a home machine is
//! compromised, what leaks is the port list that machine was already serving.

use crate::cloudflare::Cloudflare;
use crate::config::Config;
use crate::dns::reconcile_service;
use crate::plan::{
    allocate_edge_port, describe_service, normalize_local_host, normalize_subdomain, srv_for,
    PlanError,
};
use crate::store::{Store, StoreError};
use crate::{nft, wgctl, PortAllocator};
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use portal_proto::api::{
    AddPortRequest, AgentAssignment, ApiError, CreateNodeRequest, CreateNodeResponse,
    CreateServiceRequest, RegisterRequest, RegisterResponse, ServiceView, TunnelConfig,
};
use portal_proto::model::{Node, PortMapping, Service};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub config: Arc<Config>,
    pub admin_token: Arc<String>,
    pub cloudflare: Option<Arc<Cloudflare>>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/nodes", get(list_nodes).post(create_node))
        .route("/api/nodes/{id}", delete(delete_node))
        .route("/api/services", get(list_services).post(create_service))
        .route("/api/services/{id}", delete(delete_service))
        .route("/api/services/{id}/enabled", post(set_service_enabled))
        .route("/api/services/{id}/ports", post(add_port))
        .route("/api/ports/{id}", delete(remove_port))
        .route("/api/register", post(register))
        .route("/api/assignment", get(assignment))
        .with_state(state)
}

// ---- errors -------------------------------------------------------------

/// An API error that has already decided what the caller should be told.
///
/// Auth failures are deliberately vague: "unauthorized" whether the token was
/// missing, malformed or simply wrong, so the endpoint cannot be used to
/// confirm that a guessed token exists.
pub struct ApiErr(StatusCode, String);

impl ApiErr {
    fn new(code: StatusCode, msg: impl Into<String>) -> Self {
        Self(code, msg.into())
    }
    fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized")
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(ApiError { error: self.1 })).into_response()
    }
}

impl From<StoreError> for ApiErr {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound => ApiErr::new(StatusCode::NOT_FOUND, "not found"),
            StoreError::Conflict(_) | StoreError::SubnetExhausted => {
                ApiErr::new(StatusCode::CONFLICT, e.to_string())
            }
            other => {
                tracing::error!(error = %other, "database error");
                ApiErr::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        }
    }
}

impl From<PlanError> for ApiErr {
    fn from(e: PlanError) -> Self {
        // These are the operator's to fix — a bad subdomain, a taken port — so
        // they are reported verbatim rather than hidden.
        ApiErr::new(StatusCode::BAD_REQUEST, e.to_string())
    }
}

type ApiResult<T> = Result<T, ApiErr>;

// ---- authentication -----------------------------------------------------

fn bearer(parts: &Parts) -> Option<&str> {
    parts
        .headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

/// Proof that the caller holds the admin token.
pub struct AdminAuth;

impl FromRequestParts<AppState> for AdminAuth {
    type Rejection = ApiErr;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let presented = bearer(parts).ok_or_else(ApiErr::unauthorized)?;
        if crate::token::verify(presented, &crate::token::hash(&state.admin_token)) {
            Ok(AdminAuth)
        } else {
            Err(ApiErr::unauthorized())
        }
    }
}

/// Proof that the caller is a specific node's agent.
pub struct NodeAuth(pub Node);

impl FromRequestParts<AppState> for NodeAuth {
    type Rejection = ApiErr;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let presented = bearer(parts).ok_or_else(ApiErr::unauthorized)?;
        match state.store.authenticate_node(presented) {
            Ok(node) => Ok(NodeAuth(node)),
            Err(StoreError::NotFound) => Err(ApiErr::unauthorized()),
            Err(other) => Err(other.into()),
        }
    }
}

// ---- handlers -----------------------------------------------------------

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

#[derive(Serialize)]
struct Status {
    zone: String,
    public_ip: String,
    edge_port_range: String,
    cloudflare_enabled: bool,
    nftables_enabled: bool,
}

async fn status(_: AdminAuth, State(state): State<AppState>) -> ApiResult<Json<Status>> {
    let range = state
        .config
        .edge_port_range()
        .map_err(|e| ApiErr::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(Status {
        zone: state.config.gateway.zone.clone(),
        public_ip: state.config.gateway.public_ip.to_string(),
        edge_port_range: format!("{}-{}", range.start(), range.end()),
        cloudflare_enabled: state.cloudflare.is_some(),
        nftables_enabled: state.config.nftables.enabled,
    }))
}

#[derive(Serialize)]
struct NodeView {
    #[serde(flatten)]
    node: Node,
    online: bool,
    service_count: usize,
}

async fn list_nodes(_: AdminAuth, State(state): State<AppState>) -> ApiResult<Json<Vec<NodeView>>> {
    let now = OffsetDateTime::now_utc();
    let services = state.store.list_services()?;
    Ok(Json(
        state
            .store
            .list_nodes()?
            .into_iter()
            .map(|node| NodeView {
                online: node.is_online(now),
                service_count: services.iter().filter(|s| s.node_id == node.id).count(),
                node,
            })
            .collect(),
    ))
}

async fn create_node(
    _: AdminAuth,
    State(state): State<AppState>,
    Json(req): Json<CreateNodeRequest>,
) -> ApiResult<(StatusCode, Json<CreateNodeResponse>)> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "give the node a name"));
    }
    let (node, key) = state.store.create_node(
        name,
        state.config.tunnel.subnet,
        state.config.tunnel.gateway_ip,
        OffsetDateTime::now_utc(),
    )?;
    Ok((StatusCode::CREATED, Json(CreateNodeResponse { node, key })))
}

async fn delete_node(
    _: AdminAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    state.store.delete_node(id)?;
    reconcile_edge(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_services(
    _: AdminAuth,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<ServiceView>>> {
    let now = OffsetDateTime::now_utc();
    let nodes = state.store.list_nodes()?;
    let mut views = Vec::new();
    for service in state.store.list_services()? {
        views.push(service_view(&state, service, &nodes, now)?);
    }
    Ok(Json(views))
}

/// Step one of adding a server: claim a subdomain on a node.
///
/// Deliberately does nothing else. Ports are a separate step, because the
/// question "which machine is this?" and the question "which ports does it
/// listen on?" are answered at different times by different people.
async fn create_service(
    _: AdminAuth,
    State(state): State<AppState>,
    Json(req): Json<CreateServiceRequest>,
) -> ApiResult<(StatusCode, Json<ServiceView>)> {
    state.store.get_node(req.node_id)?;
    let subdomain = normalize_subdomain(&req.subdomain)?;
    let name = if req.name.trim().is_empty() {
        subdomain.clone()
    } else {
        req.name.trim().to_string()
    };

    let service = Service {
        id: Uuid::new_v4(),
        node_id: req.node_id,
        name,
        subdomain,
        enabled: true,
        created_at: OffsetDateTime::now_utc(),
    };
    state.store.insert_service(&service)?;
    sync_dns(&state, service.id).await;

    let nodes = state.store.list_nodes()?;
    let view = service_view(&state, service, &nodes, OffsetDateTime::now_utc())?;
    Ok((StatusCode::CREATED, Json(view)))
}

/// Step two: where the traffic actually goes.
async fn add_port(
    _: AdminAuth,
    State(state): State<AppState>,
    Path(service_id): Path<Uuid>,
    Json(req): Json<AddPortRequest>,
) -> ApiResult<(StatusCode, Json<ServiceView>)> {
    let service = state.store.get_service(service_id)?;
    let local_host = normalize_local_host(&req.local_host)?;

    // Rebuilt from the database every time rather than kept in memory: the
    // stored mappings are the truth about what is taken, and rebuilding is a
    // single cheap query.
    let range = state
        .config
        .edge_port_range()
        .map_err(|e| ApiErr::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut taken = state.store.taken_edge_ports()?;
    taken.extend(state.config.reserved_ports());
    let mut allocator = PortAllocator::with_taken(range, taken);
    let edge_port = allocate_edge_port(&mut allocator, &req)?;

    state.store.add_port(&PortMapping {
        id: Uuid::new_v4(),
        service_id,
        protocol: req.protocol,
        local_host,
        local_port: req.local_port,
        edge_port,
        srv: srv_for(&req),
    })?;

    reconcile_edge(&state).await;
    sync_dns(&state, service_id).await;

    let nodes = state.store.list_nodes()?;
    let view = service_view(&state, service, &nodes, OffsetDateTime::now_utc())?;
    Ok((StatusCode::CREATED, Json(view)))
}

async fn remove_port(
    _: AdminAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let service_id = state.store.delete_port(id)?;
    reconcile_edge(&state).await;
    sync_dns(&state, service_id).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SetEnabledRequest {
    enabled: bool,
}

async fn set_service_enabled(
    _: AdminAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<SetEnabledRequest>,
) -> ApiResult<Json<ServiceView>> {
    state.store.set_service_enabled(id, req.enabled)?;
    reconcile_edge(&state).await;
    let service = state.store.get_service(id)?;
    let nodes = state.store.list_nodes()?;
    Ok(Json(service_view(
        &state,
        service,
        &nodes,
        OffsetDateTime::now_utc(),
    )?))
}

async fn delete_service(
    _: AdminAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let service = state.store.get_service(id)?;
    let mappings = state.store.ports_for_service(id)?;
    state.store.delete_service(id)?;
    reconcile_edge(&state).await;

    // Retract the records too: a name still resolving to the VPS after the
    // service is gone is a black hole players keep trying to connect to.
    if let Some(cf) = &state.cloudflare {
        let described = describe_service(
            &service,
            &mappings,
            &state.config.gateway.zone,
            state.config.gateway.public_ip,
        );
        match cf.list_records().await {
            Ok(existing) => {
                let plan = reconcile_service(&described.fqdn, &[], &existing);
                if let Err(e) = cf.apply(&plan).await {
                    tracing::warn!(error = %e, fqdn = %described.fqdn, "failed to retract DNS");
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to list DNS records"),
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// An agent announcing itself on boot.
///
/// Idempotent by design: agents keep no state and generate a fresh keypair
/// every start, so this is called on every restart and simply overwrites the
/// stored public key. The tunnel address was fixed when the node was created.
async fn register(
    NodeAuth(node): NodeAuth,
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> ApiResult<Json<RegisterResponse>> {
    state
        .store
        .set_node_public_key(node.id, req.public_key.as_str())?;

    let gateway_public_key = state
        .gateway_public_key()
        .map_err(|e| ApiErr::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // The peer list changed, so WireGuard needs to hear about it before the
    // agent's first handshake arrives.
    reconcile_edge(&state).await;
    tracing::info!(node = %node.name, "agent registered");

    Ok(Json(RegisterResponse {
        node_id: node.id,
        tunnel: TunnelConfig {
            gateway_public_key,
            gateway_endpoint: state.config.tunnel.endpoint.clone(),
            tunnel_ip: node.tunnel_ip,
            tunnel_prefix_len: state.config.tunnel.subnet.prefix_len(),
            persistent_keepalive: state.config.tunnel.persistent_keepalive,
        },
    }))
}

async fn assignment(
    NodeAuth(node): NodeAuth,
    State(state): State<AppState>,
) -> ApiResult<Json<AgentAssignment>> {
    let forwards = state.store.forwards_for_node(node.id)?;
    Ok(Json(AgentAssignment {
        // The revision is a checksum of the content, not a counter: the agent
        // only needs to know whether anything changed, and deriving it means a
        // gateway restart does not make every agent think it missed an update.
        revision: revision_of(&forwards),
        forwards,
    }))
}

fn revision_of(forwards: &[portal_proto::api::Forward]) -> u64 {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for f in forwards {
        hasher.update(f.protocol.as_str().as_bytes());
        hasher.update(f.tunnel_port.to_be_bytes());
        hasher.update(f.local_host.as_bytes());
        hasher.update(f.local_port.to_be_bytes());
    }
    u64::from_be_bytes(
        hasher.finalize()[..8]
            .try_into()
            .expect("sha256 is 32 bytes"),
    )
}

// ---- shared work --------------------------------------------------------

impl AppState {
    fn gateway_public_key(&self) -> Result<portal_proto::wg::PublicKey, String> {
        let private = wgctl::load_or_create_private_key(&self.config.private_key_file())
            .map_err(|e| format!("could not read the gateway's WireGuard key: {e}"))?;
        portal_proto::wg::public_from_private(&private)
            .map_err(|e| format!("the gateway's WireGuard key is not valid: {e}"))
    }
}

fn service_view(
    state: &AppState,
    service: Service,
    nodes: &[Node],
    now: OffsetDateTime,
) -> ApiResult<ServiceView> {
    let mappings = state.store.ports_for_service(service.id)?;
    let described = describe_service(
        &service,
        &mappings,
        &state.config.gateway.zone,
        state.config.gateway.public_ip,
    );
    let node = nodes.iter().find(|n| n.id == service.node_id);
    Ok(ServiceView {
        fqdn: described.fqdn,
        node_name: node.map(|n| n.name.clone()).unwrap_or_default(),
        node_online: node.is_some_and(|n| n.is_online(now)),
        ports: described.ports,
        dns_synced: state.store.is_dns_synced(service.id).unwrap_or(false),
        service,
    })
}

/// Push the current database state into nftables and WireGuard.
///
/// Failures are logged, not returned: the change is already committed, and the
/// periodic reconcile will try again. Refusing the whole request because `nft`
/// was briefly unhappy would leave the operator with no record of what they
/// asked for.
pub async fn reconcile_edge(state: &AppState) {
    if let Err(e) = reconcile_nftables(state) {
        tracing::error!(error = %e, "failed to program nftables");
    }
    if let Err(e) = reconcile_wireguard(state) {
        tracing::error!(error = %e, "failed to program WireGuard peers");
    }
}

fn reconcile_nftables(state: &AppState) -> anyhow::Result<()> {
    let forwards = state.store.active_forwards()?;
    let rules = nft::ruleset(
        &state.config.nftables.table,
        &state.config.tunnel.subnet.to_string(),
        &forwards,
    );
    if !state.config.nftables.enabled {
        tracing::debug!(rules = %rules, "nftables disabled; ruleset not applied");
        return Ok(());
    }
    nft::apply(&rules)?;
    tracing::info!(forwards = forwards.len(), "nftables ruleset applied");
    Ok(())
}

fn reconcile_wireguard(state: &AppState) -> anyhow::Result<()> {
    let nodes = state.store.list_nodes()?;
    let private_key = wgctl::load_or_create_private_key(&state.config.private_key_file())?;
    let iface = wgctl::InterfaceConfig {
        private_key,
        listen_port: state.config.tunnel.listen_port,
        address: state.config.tunnel.gateway_ip,
        prefix_len: state.config.tunnel.subnet.prefix_len(),
    };
    let config = wgctl::render_config(&iface, &nodes);
    let path = state
        .config
        .gateway
        .data_dir
        .join(format!("{}.conf", state.config.tunnel.interface));
    wgctl::apply_config(&state.config.tunnel.interface, &path, &config)?;
    tracing::info!(peers = nodes.len(), "WireGuard peers synced");
    Ok(())
}

/// Publish one service's DNS, recording whether it worked so the UI can say so
/// and the periodic reconcile can retry.
pub async fn sync_dns(state: &AppState, service_id: Uuid) {
    let Some(cf) = &state.cloudflare else {
        return;
    };
    let result = async {
        let service = state.store.get_service(service_id)?;
        let mappings = state.store.ports_for_service(service_id)?;
        let described = describe_service(
            &service,
            &mappings,
            &state.config.gateway.zone,
            state.config.gateway.public_ip,
        );
        let existing = cf.list_records().await?;
        let plan = reconcile_service(&described.fqdn, &described.dns, &existing);
        if !plan.is_empty() {
            cf.apply(&plan).await?;
            tracing::info!(
                fqdn = %described.fqdn,
                created = plan.create.len(),
                updated = plan.update.len(),
                deleted = plan.delete.len(),
                "DNS reconciled"
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;

    match result {
        Ok(()) => {
            let _ = state.store.set_dns_synced(service_id, true);
        }
        Err(e) => {
            tracing::warn!(error = %e, %service_id, "DNS sync failed; will retry");
            let _ = state.store.set_dns_synced(service_id, false);
        }
    }
}
