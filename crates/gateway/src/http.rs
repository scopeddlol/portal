//! HTTP API and web UI.
//!
//! Two audiences with different credentials: a person holding the admin token,
//! and agents holding the key they were issued at enrollment. An agent can
//! read its own assignment and nothing else — it cannot list services, create
//! them, or see another agent. If a home machine is compromised, what leaks is
//! the port list that machine was already forwarding.

use crate::cloudflare::Cloudflare;
use crate::config::Config;
use crate::dns::reconcile_service;
use crate::plan::{describe_service, PlanError, Planner};
use crate::store::{Store, StoreError};
use crate::{nft, wgctl, PortAllocator};
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use portal_proto::api::{
    AgentAssignment, ApiError, CreateServiceRequest, EnrollRequest, EnrollResponse, ServiceView,
    TunnelConfig,
};
use portal_proto::model::Agent;
use portal_proto::profile::{Profile, ProfileSet};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use time::OffsetDateTime;
use uuid::Uuid;

/// How long a freshly minted enrollment token is good for. Short, because it
/// is going to be pasted into a chat window and it buys a permanent place in
/// the tunnel.
const ENROLLMENT_TOKEN_LIFETIME: time::Duration = time::Duration::hours(1);

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub profiles: Arc<ProfileSet>,
    pub config: Arc<Config>,
    pub admin_token: Arc<String>,
    pub cloudflare: Option<Arc<Cloudflare>>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/profiles", get(list_profiles))
        .route("/api/agents", get(list_agents))
        .route("/api/agents/tokens", post(create_enrollment_token))
        .route("/api/agents/{id}", delete(delete_agent))
        .route("/api/services", get(list_services).post(create_service))
        .route("/api/services/{id}", delete(delete_service))
        .route("/api/services/{id}/enabled", post(set_service_enabled))
        .route("/api/enroll", post(enroll))
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
            StoreError::Conflict(_) => ApiErr::new(StatusCode::CONFLICT, e.to_string()),
            StoreError::BadEnrollmentToken => {
                ApiErr::new(StatusCode::FORBIDDEN, "enrollment token is not valid")
            }
            StoreError::SubnetExhausted => ApiErr::new(StatusCode::CONFLICT, e.to_string()),
            other => {
                tracing::error!(error = %other, "database error");
                ApiErr::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        }
    }
}

impl From<PlanError> for ApiErr {
    fn from(e: PlanError) -> Self {
        // Planning errors are the operator's to fix — an unknown profile, a
        // port collision — so they are reported verbatim rather than hidden.
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

/// Proof that the caller is a specific enrolled agent.
pub struct AgentAuth(pub Agent);

impl FromRequestParts<AppState> for AgentAuth {
    type Rejection = ApiErr;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let presented = bearer(parts).ok_or_else(ApiErr::unauthorized)?;
        match state.store.authenticate_agent(presented) {
            Ok(agent) => Ok(AgentAuth(agent)),
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
    tunnel_subnet: String,
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
        tunnel_subnet: state.config.tunnel.subnet.to_string(),
        edge_port_range: format!("{}-{}", range.start(), range.end()),
        cloudflare_enabled: state.cloudflare.is_some(),
        nftables_enabled: state.config.nftables.enabled,
    }))
}

async fn list_profiles(_: AdminAuth, State(state): State<AppState>) -> Json<Vec<Profile>> {
    Json(state.profiles.iter().cloned().collect())
}

#[derive(Serialize)]
struct AgentView {
    #[serde(flatten)]
    agent: Agent,
    online: bool,
    service_count: usize,
}

async fn list_agents(
    _: AdminAuth,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<AgentView>>> {
    let now = OffsetDateTime::now_utc();
    let services = state.store.list_services()?;
    let views = state
        .store
        .list_agents()?
        .into_iter()
        .map(|agent| AgentView {
            online: agent.is_online(now),
            service_count: services.iter().filter(|s| s.agent_id == agent.id).count(),
            agent,
        })
        .collect();
    Ok(Json(views))
}

#[derive(Deserialize)]
struct CreateTokenRequest {
    #[serde(default)]
    label: String,
}

#[derive(Serialize)]
struct CreateTokenResponse {
    /// Shown once. The gateway stores only a hash.
    token: String,
    expires_at: String,
}

async fn create_enrollment_token(
    _: AdminAuth,
    State(state): State<AppState>,
    Json(req): Json<CreateTokenRequest>,
) -> ApiResult<Json<CreateTokenResponse>> {
    let now = OffsetDateTime::now_utc();
    let token = state
        .store
        .create_enrollment_token(&req.label, now, ENROLLMENT_TOKEN_LIFETIME)?;
    let expires_at = (now + ENROLLMENT_TOKEN_LIFETIME)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    Ok(Json(CreateTokenResponse { token, expires_at }))
}

async fn delete_agent(
    _: AdminAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    state.store.delete_agent(id)?;
    reconcile_edge(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_services(
    _: AdminAuth,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<ServiceView>>> {
    let mut views = Vec::new();
    for service in state.store.list_services()? {
        views.push(service_view(&state, service)?);
    }
    Ok(Json(views))
}

async fn create_service(
    _: AdminAuth,
    State(state): State<AppState>,
    Json(req): Json<CreateServiceRequest>,
) -> ApiResult<(StatusCode, Json<ServiceView>)> {
    // The agent must exist before ports are handed out, so a typo in the
    // request cannot leave allocations pointing at nothing.
    state.store.get_agent(req.agent_id)?;

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

    let planner = Planner {
        profiles: &state.profiles,
        zone: &state.config.gateway.zone,
        edge_ip: state.config.gateway.public_ip,
    };
    let plan = planner.plan(&mut allocator, &req, OffsetDateTime::now_utc())?;
    state.store.insert_service(&plan.service, &plan.mappings)?;

    reconcile_edge(&state).await;
    sync_dns(&state, plan.service.id).await;

    let view = service_view(&state, plan.service)?;
    Ok((StatusCode::CREATED, Json(view)))
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
    Ok(Json(service_view(&state, service)?))
}

async fn delete_service(
    _: AdminAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let service = state.store.get_service(id)?;
    let mappings = state.store.mappings_for_service(id)?;
    state.store.delete_service(id)?;
    reconcile_edge(&state).await;

    // Retract the records too: a name still resolving to the VPS after the
    // service is gone is a black hole players keep trying to connect to.
    if let Some(cf) = &state.cloudflare {
        let described = describe_service(
            &state.profiles,
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

async fn enroll(
    State(state): State<AppState>,
    Json(req): Json<EnrollRequest>,
) -> ApiResult<Json<EnrollResponse>> {
    let (agent, agent_key) = state.store.enroll_agent(
        &req.token,
        &req.name,
        req.public_key.as_str(),
        state.config.tunnel.subnet,
        state.config.tunnel.gateway_ip,
        OffsetDateTime::now_utc(),
    )?;

    let gateway_public_key = state
        .gateway_public_key()
        .map_err(|e| ApiErr::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // The peer list changed, so WireGuard needs to hear about it before the
    // agent's first handshake arrives.
    reconcile_edge(&state).await;

    Ok(Json(EnrollResponse {
        agent_id: agent.id,
        agent_key,
        tunnel: TunnelConfig {
            gateway_public_key,
            gateway_endpoint: state.config.tunnel.endpoint.clone(),
            tunnel_ip: agent.tunnel_ip,
            tunnel_prefix_len: state.config.tunnel.subnet.prefix_len(),
            persistent_keepalive: state.config.tunnel.persistent_keepalive,
        },
    }))
}

async fn assignment(
    AgentAuth(agent): AgentAuth,
    State(state): State<AppState>,
) -> ApiResult<Json<AgentAssignment>> {
    let forwards = state.store.forwards_for_agent(agent.id)?;
    Ok(Json(AgentAssignment {
        // The revision is a checksum of the content, not a counter: the agent
        // only needs to know whether anything changed, and deriving it means
        // a gateway restart does not make every agent think it missed an
        // update.
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
    let digest = hasher.finalize();
    u64::from_be_bytes(digest[..8].try_into().expect("sha256 is 32 bytes"))
}

// ---- shared work --------------------------------------------------------

impl AppState {
    fn gateway_public_key(&self) -> Result<portal_proto::wg::PublicKey, String> {
        let private = wgctl::load_or_create_private_key(&self.config.tunnel.private_key_file)
            .map_err(|e| format!("could not read the gateway's WireGuard key: {e}"))?;
        portal_proto::wg::public_from_private(&private)
            .map_err(|e| format!("the gateway's WireGuard key is not valid: {e}"))
    }
}

fn service_view(state: &AppState, service: portal_proto::model::Service) -> ApiResult<ServiceView> {
    let mappings = state.store.mappings_for_service(service.id)?;
    let described = describe_service(
        &state.profiles,
        &service,
        &mappings,
        &state.config.gateway.zone,
        state.config.gateway.public_ip,
    );
    let dns_synced = state.store.is_dns_synced(service.id).unwrap_or(false);
    Ok(ServiceView {
        service,
        fqdn: described.fqdn,
        ports: mappings,
        endpoints: described.endpoints,
        config_actions: described.config_actions,
        dns_synced,
    })
}

/// Push the current database state into nftables and WireGuard.
///
/// Failures are logged, not returned: the service is already committed, and
/// the periodic reconcile will try again. Refusing the whole request because
/// `nft` was briefly unhappy would leave the operator with no record of what
/// they asked for.
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
    let agents = state.store.list_agents()?;
    let private_key = wgctl::load_or_create_private_key(&state.config.tunnel.private_key_file)?;
    let iface = wgctl::InterfaceConfig {
        private_key,
        listen_port: state.config.tunnel.listen_port,
        address: state.config.tunnel.gateway_ip,
        prefix_len: state.config.tunnel.subnet.prefix_len(),
    };
    let config = wgctl::render_config(&iface, &agents);
    let path = state
        .config
        .gateway
        .data_dir
        .join(format!("{}.conf", state.config.tunnel.interface));
    wgctl::apply_config(&state.config.tunnel.interface, &path, &config)?;
    tracing::info!(peers = agents.len(), "WireGuard peers synced");
    Ok(())
}

/// Publish one service's DNS, recording whether it worked so the UI can say
/// so and the periodic reconcile can retry.
pub async fn sync_dns(state: &AppState, service_id: Uuid) {
    let Some(cf) = &state.cloudflare else {
        return;
    };
    let result = async {
        let service = state.store.get_service(service_id)?;
        let mappings = state.store.mappings_for_service(service_id)?;
        let described = describe_service(
            &state.profiles,
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
