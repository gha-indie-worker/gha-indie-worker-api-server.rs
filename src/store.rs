#![forbid(unsafe_code)]

//! The effect boundary in front of the domain.
//!
//! Every mutation here is "read state → call a pure domain function → write the
//! result", so the invariants live in [`crate::domain`] and this module only
//! owns concurrency and identity allocation.
//!
//! The current implementation is an in-memory store behind one `RwLock`. It is
//! the *seam*, not the destination: when `gha-indie-worker-orm-core` publishes
//! the entities described in `docs/orm-core-entities.md`, each method here
//! becomes a SeaORM query against the canonical pool and no caller changes.
//! Services never run DDL at boot, so the schema arrives through the
//! declarative migration path, never from this process.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use tokio::sync::RwLock;
use uuid::Uuid;

use crate::domain::embeddings::StoredEmbedding;
use crate::domain::onboarding::{OrgOnboardingState, UserOnboardingState};
use crate::domain::orgs::{Invitation, Org, OrgMember, OrgRole};
use crate::domain::runs::{Job, LifecycleState, LogChunk, Run};
use crate::domain::sync::CausalEnvelope;
use crate::domain::users::User;
use crate::domain::webhooks::DeliveryLedger;
use crate::domain::workers::Worker;

#[derive(Debug, Default)]
struct Tables {
    orgs: HashMap<Uuid, Org>,
    org_by_slug: HashMap<String, Uuid>,
    members: HashMap<(Uuid, Uuid), OrgMember>,
    invitations: HashMap<Uuid, Invitation>,
    users: HashMap<Uuid, User>,
    user_by_subject: HashMap<String, Uuid>,
    org_onboarding: HashMap<Uuid, OrgOnboardingState>,
    user_onboarding: HashMap<Uuid, UserOnboardingState>,
    runs: HashMap<Uuid, Run>,
    jobs: HashMap<Uuid, Job>,
    logs: BTreeMap<(Uuid, u64), LogChunk>,
    workers: HashMap<Uuid, Worker>,
    embeddings: Vec<StoredEmbedding>,
    sync_log: Vec<CausalEnvelope>,
}

/// Shared application store. Cloning is cheap: it is one `Arc`.
#[derive(Clone)]
pub struct Store {
    tables: Arc<RwLock<Tables>>,
    deliveries: Arc<RwLock<DeliveryLedger>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    #[must_use]
    pub fn new(delivery_ttl_ms: u64, max_deliveries: usize) -> Self {
        Self {
            tables: Arc::new(RwLock::new(Tables::default())),
            deliveries: Arc::new(RwLock::new(DeliveryLedger::new(
                delivery_ttl_ms,
                max_deliveries,
            ))),
        }
    }

    // ---- organisations -----------------------------------------------------

    pub async fn slug_taken(&self, slug: &str) -> bool {
        self.tables.read().await.org_by_slug.contains_key(slug)
    }

    pub async fn insert_org(&self, org: Org, owner: OrgMember) {
        let mut tables = self.tables.write().await;
        tables.org_by_slug.insert(org.slug.clone(), org.id);
        tables
            .org_onboarding
            .insert(org.id, OrgOnboardingState::Created);
        tables.members.insert((org.id, owner.user_id), owner);
        tables.orgs.insert(org.id, org);
    }

    pub async fn org(&self, id: Uuid) -> Option<Org> {
        self.tables.read().await.orgs.get(&id).cloned()
    }

    pub async fn set_org_seats(&self, id: Uuid, seats: u32) {
        if let Some(org) = self.tables.write().await.orgs.get_mut(&id) {
            org.seats = seats;
        }
    }

    pub async fn set_org_domain(&self, id: Uuid, domain: String) {
        if let Some(org) = self.tables.write().await.orgs.get_mut(&id) {
            org.verified_domain = Some(domain);
        }
    }

    pub async fn membership(&self, org_id: Uuid, user_id: Uuid) -> Option<OrgRole> {
        self.tables
            .read()
            .await
            .members
            .get(&(org_id, user_id))
            .map(|member| member.role)
    }

    pub async fn members(&self, org_id: Uuid) -> Vec<OrgMember> {
        let mut members: Vec<OrgMember> = self
            .tables
            .read()
            .await
            .members
            .values()
            .filter(|member| member.org_id == org_id)
            .cloned()
            .collect();
        members.sort_by(|a, b| a.user_id.cmp(&b.user_id));
        members
    }

    pub async fn owner_count(&self, org_id: Uuid) -> u32 {
        let count = self
            .tables
            .read()
            .await
            .members
            .values()
            .filter(|member| member.org_id == org_id && member.role.is_owner())
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    pub async fn occupied_seats(&self, org_id: Uuid) -> u32 {
        let count = self
            .tables
            .read()
            .await
            .members
            .values()
            .filter(|member| member.org_id == org_id)
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    pub async fn insert_member(&self, member: OrgMember) {
        self.tables
            .write()
            .await
            .members
            .insert((member.org_id, member.user_id), member);
    }

    pub async fn insert_invitation(&self, invitation: Invitation) {
        self.tables
            .write()
            .await
            .invitations
            .insert(invitation.id, invitation);
    }

    pub async fn invitations(&self, org_id: Uuid) -> Vec<Invitation> {
        let mut invitations: Vec<Invitation> = self
            .tables
            .read()
            .await
            .invitations
            .values()
            .filter(|invitation| invitation.org_id == org_id)
            .cloned()
            .collect();
        invitations.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        invitations
    }

    // ---- users -------------------------------------------------------------

    pub async fn user_by_subject(&self, subject: &str) -> Option<User> {
        let tables = self.tables.read().await;
        tables
            .user_by_subject
            .get(subject)
            .and_then(|id| tables.users.get(id))
            .cloned()
    }

    pub async fn insert_user(&self, user: User) {
        let mut tables = self.tables.write().await;
        tables.user_by_subject.insert(user.subject.clone(), user.id);
        tables
            .user_onboarding
            .insert(user.id, UserOnboardingState::Signup);
        tables.users.insert(user.id, user);
    }

    pub async fn set_display_name(&self, id: Uuid, display_name: String) {
        if let Some(user) = self.tables.write().await.users.get_mut(&id) {
            user.display_name = Some(display_name);
        }
    }

    pub async fn memberships_of(&self, user_id: Uuid) -> Vec<(Org, OrgRole)> {
        let tables = self.tables.read().await;
        let mut memberships: Vec<(Org, OrgRole)> = tables
            .members
            .values()
            .filter(|member| member.user_id == user_id)
            .filter_map(|member| {
                tables
                    .orgs
                    .get(&member.org_id)
                    .map(|org| (org.clone(), member.role))
            })
            .collect();
        memberships.sort_by(|a, b| a.0.slug.cmp(&b.0.slug));
        memberships
    }

    // ---- onboarding --------------------------------------------------------

    pub async fn org_onboarding(&self, org_id: Uuid) -> OrgOnboardingState {
        self.tables
            .read()
            .await
            .org_onboarding
            .get(&org_id)
            .copied()
            .unwrap_or_default()
    }

    pub async fn set_org_onboarding(&self, org_id: Uuid, state: OrgOnboardingState) {
        self.tables
            .write()
            .await
            .org_onboarding
            .insert(org_id, state);
    }

    pub async fn user_onboarding(&self, user_id: Uuid) -> UserOnboardingState {
        self.tables
            .read()
            .await
            .user_onboarding
            .get(&user_id)
            .copied()
            .unwrap_or_default()
    }

    pub async fn set_user_onboarding(&self, user_id: Uuid, state: UserOnboardingState) {
        let mut tables = self.tables.write().await;
        tables.user_onboarding.insert(user_id, state);
        if let Some(user) = tables.users.get_mut(&user_id) {
            user.onboarding = state;
        }
    }

    // ---- runs and jobs -----------------------------------------------------

    pub async fn insert_run(&self, run: Run, jobs: Vec<Job>) {
        let mut tables = self.tables.write().await;
        tables.runs.insert(run.id, run);
        for job in jobs {
            tables.jobs.insert(job.id, job);
        }
    }

    pub async fn run(&self, id: Uuid) -> Option<Run> {
        self.tables.read().await.runs.get(&id).cloned()
    }

    pub async fn runs_of(&self, org_id: Uuid, limit: usize) -> Vec<Run> {
        let mut runs: Vec<Run> = self
            .tables
            .read()
            .await
            .runs
            .values()
            .filter(|run| run.org_id == org_id)
            .cloned()
            .collect();
        runs.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        runs.truncate(limit);
        runs
    }

    pub async fn jobs_of(&self, run_id: Uuid) -> Vec<Job> {
        let mut jobs: Vec<Job> = self
            .tables
            .read()
            .await
            .jobs
            .values()
            .filter(|job| job.run_id == run_id)
            .cloned()
            .collect();
        jobs.sort_by_key(|job| job.sequence);
        jobs
    }

    pub async fn job(&self, id: Uuid) -> Option<Job> {
        self.tables.read().await.jobs.get(&id).cloned()
    }

    /// Apply an already-validated transition to a job and roll the run up.
    pub async fn apply_job_state(
        &self,
        job_id: Uuid,
        state: LifecycleState,
        updated_at: String,
    ) -> Option<Run> {
        let mut tables = self.tables.write().await;
        let run_id = {
            let job = tables.jobs.get_mut(&job_id)?;
            job.state = state;
            job.updated_at.clone_from(&updated_at);
            job.run_id
        };
        let states: Vec<LifecycleState> = tables
            .jobs
            .values()
            .filter(|job| job.run_id == run_id)
            .map(|job| job.state)
            .collect();
        let rolled = crate::domain::runs::rollup(&states);
        let run = tables.runs.get_mut(&run_id)?;
        run.state = rolled;
        run.updated_at = updated_at;
        Some(run.clone())
    }

    pub async fn set_run_state(&self, run_id: Uuid, state: LifecycleState, updated_at: String) {
        if let Some(run) = self.tables.write().await.runs.get_mut(&run_id) {
            run.state = state;
            run.updated_at = updated_at;
        }
    }

    // ---- log chunks --------------------------------------------------------

    pub async fn last_log_sequence(&self, job_id: Uuid) -> (Option<u64>, usize) {
        let tables = self.tables.read().await;
        let chunks: Vec<&LogChunk> = tables
            .logs
            .range((job_id, 0)..=(job_id, u64::MAX))
            .map(|(_, chunk)| chunk)
            .collect();
        (chunks.last().map(|chunk| chunk.sequence), chunks.len())
    }

    pub async fn append_log(&self, chunk: LogChunk) {
        self.tables
            .write()
            .await
            .logs
            .insert((chunk.job_id, chunk.sequence), chunk);
    }

    pub async fn logs_of(&self, job_id: Uuid, after: u64, limit: usize) -> Vec<LogChunk> {
        self.tables
            .read()
            .await
            .logs
            .range((job_id, after)..=(job_id, u64::MAX))
            .filter(|(_, chunk)| chunk.sequence > after || after == 0)
            .map(|(_, chunk)| chunk.clone())
            .take(limit)
            .collect()
    }

    // ---- workers -----------------------------------------------------------

    pub async fn insert_worker(&self, worker: Worker) {
        self.tables.write().await.workers.insert(worker.id, worker);
    }

    pub async fn worker(&self, id: Uuid) -> Option<Worker> {
        self.tables.read().await.workers.get(&id).cloned()
    }

    pub async fn workers_of(&self, org_id: Uuid) -> Vec<Worker> {
        let mut workers: Vec<Worker> = self
            .tables
            .read()
            .await
            .workers
            .values()
            .filter(|worker| worker.org_id == org_id)
            .cloned()
            .collect();
        workers.sort_by(|a, b| a.name.cmp(&b.name));
        workers
    }

    pub async fn update_worker(
        &self,
        id: Uuid,
        state: crate::domain::workers::WorkerState,
        heartbeat_at: String,
        heartbeat_ms: u64,
    ) -> Option<Worker> {
        let mut tables = self.tables.write().await;
        let worker = tables.workers.get_mut(&id)?;
        worker.state = state;
        worker.last_heartbeat_at = heartbeat_at;
        worker.last_heartbeat_ms = heartbeat_ms;
        Some(worker.clone())
    }

    // ---- webhook deliveries ------------------------------------------------

    /// Claim a GitHub delivery exactly once inside the retention window.
    ///
    /// # Errors
    /// Returns [`crate::domain::webhooks::WebhookError::DuplicateDelivery`]
    /// when this delivery was already claimed.
    pub async fn claim_delivery(
        &self,
        delivery: Uuid,
        now_ms: u64,
    ) -> Result<(), crate::domain::webhooks::WebhookError> {
        self.deliveries.write().await.claim(delivery, now_ms)
    }

    // ---- embeddings --------------------------------------------------------

    pub async fn upsert_embeddings(&self, embeddings: Vec<StoredEmbedding>) -> usize {
        let mut tables = self.tables.write().await;
        let mut written = 0;
        for embedding in embeddings {
            let existing = tables.embeddings.iter().position(|candidate| {
                candidate.tenant_id == embedding.tenant_id
                    && candidate.entity_kind == embedding.entity_kind
                    && candidate.entity_id == embedding.entity_id
                    && candidate.model == embedding.model
            });
            match existing {
                Some(index) => tables.embeddings[index] = embedding,
                None => tables.embeddings.push(embedding),
            }
            written += 1;
        }
        written
    }

    pub async fn embeddings_of(
        &self,
        tenant_id: Uuid,
        kind: Option<crate::domain::embeddings::EntityKind>,
    ) -> Vec<StoredEmbedding> {
        self.tables
            .read()
            .await
            .embeddings
            .iter()
            .filter(|embedding| embedding.tenant_id == tenant_id)
            .filter(|embedding| kind.is_none_or(|kind| embedding.entity_kind == kind))
            .cloned()
            .collect()
    }

    // ---- sync --------------------------------------------------------------

    pub async fn push_sync(&self, envelopes: Vec<CausalEnvelope>) -> (usize, usize) {
        let mut tables = self.tables.write().await;
        crate::domain::sync::merge(&mut tables.sync_log, envelopes)
    }

    pub async fn pull_sync(&self, since: u64, limit: usize) -> Vec<CausalEnvelope> {
        crate::domain::sync::since(&self.tables.read().await.sync_log, since, limit)
    }

    pub async fn sync_clock(&self) -> BTreeMap<String, u64> {
        crate::domain::sync::clock(&self.tables.read().await.sync_log)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::orgs::NewOrg;

    fn store() -> Store {
        Store::new(60_000, 128)
    }

    async fn seeded_org(store: &Store, owner: Uuid) -> Uuid {
        let org = Org::create(
            Uuid::new_v4(),
            NewOrg {
                slug: "acme".to_owned(),
                name: "Acme".to_owned(),
                verified_domain: None,
            },
            "2026-01-01T00:00:00Z".to_owned(),
        )
        .expect("valid org");
        let id = org.id;
        store
            .insert_org(
                org,
                OrgMember {
                    org_id: id,
                    user_id: owner,
                    role: OrgRole::Owner,
                    joined_at: "2026-01-01T00:00:00Z".to_owned(),
                },
            )
            .await;
        id
    }

    #[tokio::test]
    async fn an_org_starts_with_one_owner_and_a_created_onboarding_state() {
        let store = store();
        let owner = Uuid::new_v4();
        let org_id = seeded_org(&store, owner).await;

        assert!(store.slug_taken("acme").await);
        assert!(!store.slug_taken("other").await);
        assert_eq!(store.membership(org_id, owner).await, Some(OrgRole::Owner));
        assert_eq!(store.owner_count(org_id).await, 1);
        assert_eq!(store.occupied_seats(org_id).await, 1);
        assert_eq!(
            store.org_onboarding(org_id).await,
            OrgOnboardingState::Created
        );
    }

    #[tokio::test]
    async fn members_and_memberships_are_listed_deterministically() {
        let store = store();
        let owner = Uuid::from_u128(1);
        let org_id = seeded_org(&store, owner).await;
        store
            .insert_member(OrgMember {
                org_id,
                user_id: Uuid::from_u128(2),
                role: OrgRole::Member,
                joined_at: "2026-01-02T00:00:00Z".to_owned(),
            })
            .await;

        let members = store.members(org_id).await;
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].user_id, Uuid::from_u128(1));
        assert_eq!(members[1].user_id, Uuid::from_u128(2));

        let memberships = store.memberships_of(Uuid::from_u128(2)).await;
        assert_eq!(memberships.len(), 1);
        assert_eq!(memberships[0].1, OrgRole::Member);
    }

    #[tokio::test]
    async fn a_delivery_may_be_claimed_only_once() {
        let store = store();
        let delivery = Uuid::new_v4();
        assert!(store.claim_delivery(delivery, 0).await.is_ok());
        assert!(store.claim_delivery(delivery, 1).await.is_err());
        assert!(store.claim_delivery(Uuid::new_v4(), 1).await.is_ok());
    }

    #[tokio::test]
    async fn the_sync_log_merges_idempotently_through_the_store() {
        let store = store();
        let envelope = CausalEnvelope {
            id: Uuid::from_u128(1),
            actor: "sub".to_owned(),
            lamport: 1,
            kind: "run.updated".to_owned(),
            payload: serde_json::Value::Null,
            at: "2026-01-01T00:00:00Z".to_owned(),
        };
        assert_eq!(store.push_sync(vec![envelope.clone()]).await, (1, 0));
        assert_eq!(store.push_sync(vec![envelope]).await, (0, 1));
        assert_eq!(store.pull_sync(0, 10).await.len(), 1);
        assert_eq!(store.sync_clock().await.get("sub"), Some(&1));
    }
}
