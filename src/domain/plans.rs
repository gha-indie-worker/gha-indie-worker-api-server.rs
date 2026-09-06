#![forbid(unsafe_code)]

//! Workflow → fixed-profile planning, mirroring `gha-clone-server`'s
//! independent lane.
//!
//! This is deliberately *not* a YAML interpreter. It classifies a bounded
//! static subset by evidence, maps each job to an operator-reviewed profile,
//! and reports everything it cannot execute rather than approximating it. The
//! caller-selected shell, action, runner image and manifest never leave this
//! module — only `(repository, immutable SHA, profile)` does.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{is_commit_sha, is_portable_identifier, is_repository_slug};

/// Operator-reviewed execution profiles. Adding one is a source change, never
/// a request field.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    RustVerify,
    NodeVerify,
    PythonVerify,
    FlutterVerify,
    FlutterAndroidDebug,
    FlutterWebRelease,
    FlutterLinuxRelease,
    Playwright,
    Puppeteer,
}

impl Profile {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RustVerify => "rust-verify",
            Self::NodeVerify => "node-verify",
            Self::PythonVerify => "python-verify",
            Self::FlutterVerify => "flutter-verify",
            Self::FlutterAndroidDebug => "flutter-android-debug",
            Self::FlutterWebRelease => "flutter-web-release",
            Self::FlutterLinuxRelease => "flutter-linux-release",
            Self::Playwright => "playwright",
            Self::Puppeteer => "puppeteer",
        }
    }

    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::RustVerify,
            Self::NodeVerify,
            Self::PythonVerify,
            Self::FlutterVerify,
            Self::FlutterAndroidDebug,
            Self::FlutterWebRelease,
            Self::FlutterLinuxRelease,
            Self::Playwright,
            Self::Puppeteer,
        ]
    }

    /// # Errors
    /// Returns [`PlanError::UnknownProfile`] for a name outside [`Profile::all`].
    pub fn parse(value: &str) -> Result<Self, PlanError> {
        Self::all()
            .iter()
            .copied()
            .find(|profile| profile.as_str() == value)
            .ok_or(PlanError::UnknownProfile)
    }
}

/// Why a job cannot run on the independent lane. Every reason is reported to
/// the caller; nothing is silently approximated.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exclusion {
    SecretExpression,
    DynamicMatrix,
    ConditionalExecution,
    MarketplaceAction,
    JobOrServiceContainer,
    NonLinuxRunner,
    ReusableWorkflow,
    EnvironmentOrDeployment,
    NoRecognisedEvidence,
}

impl Exclusion {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SecretExpression => "secret_expression",
            Self::DynamicMatrix => "dynamic_matrix",
            Self::ConditionalExecution => "conditional_execution",
            Self::MarketplaceAction => "marketplace_action",
            Self::JobOrServiceContainer => "job_or_service_container",
            Self::NonLinuxRunner => "non_linux_runner",
            Self::ReusableWorkflow => "reusable_workflow",
            Self::EnvironmentOrDeployment => "environment_or_deployment",
            Self::NoRecognisedEvidence => "no_recognised_evidence",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobSupport {
    /// Executable on the independent lane with a fixed profile.
    Independent { profile: Profile },
    /// Not executable here; classified for an ARC self-hosted runner scale set.
    DelegatedToArc { lane: String },
    /// Reported, never approximated.
    Unsupported { reason: Exclusion },
}

impl JobSupport {
    #[must_use]
    pub const fn is_independent(&self) -> bool {
        matches!(self, Self::Independent { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlannedJob {
    pub name: String,
    pub needs: Vec<String>,
    pub support: JobSupport,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub repository: String,
    pub revision: String,
    pub workflow_path: String,
    pub jobs: Vec<PlannedJob>,
    /// Deterministic topological order of `jobs` by name.
    pub order: Vec<String>,
}

impl Plan {
    /// A plan may only be enqueued when every job is independently executable.
    #[must_use]
    pub fn fully_supported(&self) -> bool {
        !self.jobs.is_empty() && self.jobs.iter().all(|job| job.support.is_independent())
    }

    #[must_use]
    pub fn unsupported(&self) -> Vec<&PlannedJob> {
        self.jobs
            .iter()
            .filter(|job| !job.support.is_independent())
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct PlanRequest {
    pub repository: String,
    pub revision: String,
    pub workflow_path: String,
    pub workflow_yaml: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum PlanError {
    #[error("repository must be exactly owner/repo")]
    InvalidRepository,
    #[error("revision must be a 40-character lowercase commit SHA; branches and tags are refused")]
    InvalidRevision,
    #[error("workflowPath must be a .yml or .yaml file directly under .github/workflows")]
    InvalidWorkflowPath,
    #[error("workflow document exceeds the parser bound")]
    WorkflowTooLarge,
    #[error("workflow declares no jobs")]
    NoJobs,
    #[error("workflow declares more than the permitted number of jobs")]
    TooManyJobs,
    #[error("job name is not a bounded portable identifier")]
    InvalidJobName,
    #[error("a `needs` entry names a job that does not exist")]
    UnknownDependency,
    #[error("`needs` graph contains a cycle")]
    DependencyCycle,
    #[error("unknown execution profile")]
    UnknownProfile,
}

pub const MAX_WORKFLOW_BYTES: usize = 256 * 1024;
pub const MAX_JOBS: usize = 64;

/// Validate the immutable inputs of a plan request.
///
/// # Errors
/// Returns [`PlanError`] when the repository, revision, workflow path or
/// document size is outside the contract.
pub fn validate_request(request: &PlanRequest) -> Result<(), PlanError> {
    if !is_repository_slug(&request.repository) {
        return Err(PlanError::InvalidRepository);
    }
    if !is_commit_sha(&request.revision) {
        return Err(PlanError::InvalidRevision);
    }
    validate_workflow_path(&request.workflow_path)?;
    if request.workflow_yaml.len() > MAX_WORKFLOW_BYTES {
        return Err(PlanError::WorkflowTooLarge);
    }
    Ok(())
}

/// `.github/workflows/<name>.<yml|yaml>` with no traversal, no backslashes and
/// no nesting.
///
/// # Errors
/// Returns [`PlanError::InvalidWorkflowPath`] for anything else.
pub fn validate_workflow_path(path: &str) -> Result<(), PlanError> {
    const PREFIX: &str = ".github/workflows/";
    let invalid =
        !path.starts_with(PREFIX) || path.contains('\\') || path.contains("..") || path.len() > 256;
    if invalid {
        return Err(PlanError::InvalidWorkflowPath);
    }
    let name = &path[PREFIX.len()..];
    let stem = name
        .strip_suffix(".yml")
        .or_else(|| name.strip_suffix(".yaml"))
        .ok_or(PlanError::InvalidWorkflowPath)?;
    if is_portable_identifier(stem, 128) {
        Ok(())
    } else {
        Err(PlanError::InvalidWorkflowPath)
    }
}

/// Evidence extracted from one job's step text. Kept as data so the classifier
/// below stays a pure function of it and is exhaustively testable.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct JobEvidence {
    pub name: String,
    pub needs: Vec<String>,
    pub runs_on: String,
    pub commands: Vec<String>,
    pub uses: Vec<String>,
    pub has_matrix: bool,
    pub has_condition: bool,
    pub has_container: bool,
    pub has_environment: bool,
    pub references_secret: bool,
}

/// Classify one job. Exclusions are checked before evidence so an unsupported
/// construct can never be masked by a matching command.
#[must_use]
pub fn classify(evidence: &JobEvidence) -> JobSupport {
    if evidence.references_secret {
        return JobSupport::Unsupported {
            reason: Exclusion::SecretExpression,
        };
    }
    if evidence.has_matrix {
        return JobSupport::Unsupported {
            reason: Exclusion::DynamicMatrix,
        };
    }
    if evidence.has_condition {
        return JobSupport::Unsupported {
            reason: Exclusion::ConditionalExecution,
        };
    }
    if evidence.has_container {
        return JobSupport::Unsupported {
            reason: Exclusion::JobOrServiceContainer,
        };
    }
    if evidence.has_environment {
        return JobSupport::Unsupported {
            reason: Exclusion::EnvironmentOrDeployment,
        };
    }
    if let Some(lane) = delegated_lane(&evidence.runs_on) {
        return JobSupport::DelegatedToArc { lane };
    }
    if evidence
        .uses
        .iter()
        .any(|action| !is_trusted_action(action))
    {
        return JobSupport::Unsupported {
            reason: Exclusion::MarketplaceAction,
        };
    }
    match profile_for(&evidence.commands) {
        Some(profile) => JobSupport::Independent { profile },
        None => JobSupport::Unsupported {
            reason: Exclusion::NoRecognisedEvidence,
        },
    }
}

/// macOS/iOS and Windows are delegated to GitHub-hosted native runners rather
/// than claimed. Everything else is a Linux lane this service can reason about.
fn delegated_lane(runs_on: &str) -> Option<String> {
    let value = runs_on.to_ascii_lowercase();
    if value.contains("macos") || value.contains("windows") {
        return Some("github-hosted-native".to_owned());
    }
    None
}

/// Only the checkout and toolchain setup actions the operator has reviewed.
fn is_trusted_action(action: &str) -> bool {
    const TRUSTED: [&str; 5] = [
        "actions/checkout",
        "actions/setup-node",
        "actions/setup-python",
        "dtolnay/rust-toolchain",
        "subosito/flutter-action",
    ];
    let name = action.split('@').next().unwrap_or_default();
    TRUSTED.contains(&name)
}

/// Map command evidence to one fixed profile. The order encodes precedence:
/// the most specific artifact-producing profile wins over a plain verify.
#[must_use]
pub fn profile_for(commands: &[String]) -> Option<Profile> {
    let joined = commands.join("\n").to_ascii_lowercase();
    let has = |needle: &str| joined.contains(needle);

    if has("flutter build apk") || has("flutter build appbundle") {
        return Some(Profile::FlutterAndroidDebug);
    }
    if has("flutter build web") {
        return Some(Profile::FlutterWebRelease);
    }
    if has("flutter build linux") {
        return Some(Profile::FlutterLinuxRelease);
    }
    if has("flutter test") || has("flutter analyze") {
        return Some(Profile::FlutterVerify);
    }
    if has("playwright") {
        return Some(Profile::Playwright);
    }
    if has("puppeteer") {
        return Some(Profile::Puppeteer);
    }
    if has("cargo test") || has("cargo clippy") || has("cargo fmt") || has("cargo build") {
        return Some(Profile::RustVerify);
    }
    if has("pytest") || has("python -m compileall") || has("python3 -m pytest") {
        return Some(Profile::PythonVerify);
    }
    if has("npm ci") || has("npm test") || has("pnpm test") || has("yarn test") || has("npm run") {
        return Some(Profile::NodeVerify);
    }
    None
}

/// Extract per-job evidence from a workflow document.
///
/// This is deliberately **not** a YAML interpreter. It is a bounded, line-
/// oriented scanner over the static subset the independent lane supports: it
/// finds job names, their `needs`, their runner, their commands and their
/// actions, and it flags the constructs that make a job unsupported. Anything
/// it does not recognise contributes no evidence, and a job with no recognised
/// evidence is reported as [`Exclusion::NoRecognisedEvidence`] rather than
/// guessed at — which is exactly the fail-closed posture the independent lane
/// promises.
///
/// Full workflow semantics are ARC's job, not this scanner's.
///
/// # Errors
/// Returns [`PlanError::WorkflowTooLarge`] above [`MAX_WORKFLOW_BYTES`] and
/// [`PlanError::TooManyJobs`] above [`MAX_JOBS`].
pub fn scan_workflow(yaml: &str) -> Result<Vec<JobEvidence>, PlanError> {
    if yaml.len() > MAX_WORKFLOW_BYTES {
        return Err(PlanError::WorkflowTooLarge);
    }

    let mut jobs: Vec<JobEvidence> = Vec::new();
    let mut current: Option<JobEvidence> = None;
    let mut in_jobs = false;
    let mut run_block: Option<usize> = None;

    for raw in yaml.lines() {
        let trimmed = raw.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = raw.len() - trimmed.len();
        let trimmed = trimmed.trim_end();

        // A top-level key ends the current job and switches sections.
        if indent == 0 {
            if let Some(job) = current.take() {
                jobs.push(job);
            }
            run_block = None;
            in_jobs = trimmed.starts_with("jobs:");
            continue;
        }
        if !in_jobs {
            continue;
        }

        // A job name sits one level under `jobs:`.
        if indent <= 2 {
            if let Some(name) = trimmed.strip_suffix(':') {
                if let Some(job) = current.take() {
                    jobs.push(job);
                }
                if jobs.len() >= MAX_JOBS {
                    return Err(PlanError::TooManyJobs);
                }
                run_block = None;
                current = Some(JobEvidence {
                    name: name.trim().to_owned(),
                    ..JobEvidence::default()
                });
            }
            continue;
        }

        let Some(job) = current.as_mut() else {
            continue;
        };

        // Continuation lines of a block scalar are command text.
        if let Some(opened_at) = run_block {
            if indent > opened_at {
                job.commands
                    .push(trimmed.trim_start_matches("- ").to_owned());
                continue;
            }
            run_block = None;
        }

        if references_secret(trimmed) {
            job.references_secret = true;
        }

        let key = trimmed.trim_start_matches("- ");
        if let Some(value) = key.strip_prefix("run:") {
            let value = value.trim();
            if is_block_scalar(value) {
                run_block = Some(indent);
            } else if !value.is_empty() {
                job.commands.push(value.to_owned());
            }
            continue;
        }
        if let Some(value) = key.strip_prefix("uses:") {
            let value = value.trim();
            if !value.is_empty() {
                job.uses.push(value.to_owned());
            }
            continue;
        }
        if let Some(value) = key.strip_prefix("runs-on:") {
            job.runs_on = value.trim().to_owned();
            continue;
        }
        if let Some(value) = key.strip_prefix("needs:") {
            job.needs.extend(parse_needs(value));
            continue;
        }
        if key.starts_with("if:") {
            job.has_condition = true;
            continue;
        }
        if key.starts_with("strategy:") || key.starts_with("matrix:") {
            job.has_matrix = true;
            continue;
        }
        if key.starts_with("container:") || key.starts_with("services:") {
            job.has_container = true;
            continue;
        }
        if key.starts_with("environment:") {
            job.has_environment = true;
            continue;
        }
    }

    if let Some(job) = current {
        jobs.push(job);
    }
    if jobs.len() > MAX_JOBS {
        return Err(PlanError::TooManyJobs);
    }
    Ok(jobs)
}

/// `${{ secrets.* }}` and the implicit GitHub token are the two ways a workflow
/// asks for a credential. Either one makes a job unsupported here.
#[must_use]
fn references_secret(line: &str) -> bool {
    line.contains("${{")
        && (line.contains("secrets.")
            || line.contains("github.token")
            || line.contains("GITHUB_TOKEN"))
}

fn is_block_scalar(value: &str) -> bool {
    matches!(value, "|" | ">" | "|-" | ">-" | "|+" | ">+")
}

/// `needs: build`, `needs: [build, test]`, and the leading-dash block form.
fn parse_needs(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|entry| entry.trim().trim_matches('"').trim_matches('\'').to_owned())
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// Deterministic topological order over `needs`.
///
/// Ties are broken by name so two runs of the same plan always dispatch in the
/// same order — that determinism is what makes the run request ID stable.
///
/// # Errors
/// Returns [`PlanError::UnknownDependency`] when a `needs` entry names a job
/// that is not in the workflow, and [`PlanError::DependencyCycle`] when the
/// graph cannot be linearised.
pub fn topological_order(jobs: &[JobEvidence]) -> Result<Vec<String>, PlanError> {
    let names: BTreeSet<&str> = jobs.iter().map(|job| job.name.as_str()).collect();
    let mut pending: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for job in jobs {
        let mut dependencies = BTreeSet::new();
        for need in &job.needs {
            if !names.contains(need.as_str()) {
                return Err(PlanError::UnknownDependency);
            }
            dependencies.insert(need.as_str());
        }
        pending.insert(job.name.as_str(), dependencies);
    }

    let mut order = Vec::with_capacity(jobs.len());
    while !pending.is_empty() {
        // BTreeMap iteration is sorted, so the first ready job is the
        // lexicographically smallest one — deterministic by construction.
        let ready = pending
            .iter()
            .find(|(_, dependencies)| dependencies.is_empty())
            .map(|(name, _)| (*name).to_owned());
        let Some(ready) = ready else {
            return Err(PlanError::DependencyCycle);
        };
        pending.remove(ready.as_str());
        for dependencies in pending.values_mut() {
            dependencies.remove(ready.as_str());
        }
        order.push(ready);
    }
    Ok(order)
}

/// Compile validated evidence into a [`Plan`].
///
/// # Errors
/// Returns [`PlanError`] when the request, job names or dependency graph are
/// outside the contract.
pub fn compile(request: &PlanRequest, jobs: Vec<JobEvidence>) -> Result<Plan, PlanError> {
    validate_request(request)?;
    if jobs.is_empty() {
        return Err(PlanError::NoJobs);
    }
    if jobs.len() > MAX_JOBS {
        return Err(PlanError::TooManyJobs);
    }
    if !jobs
        .iter()
        .all(|job| is_portable_identifier(&job.name, 128))
    {
        return Err(PlanError::InvalidJobName);
    }
    let order = topological_order(&jobs)?;
    let planned = jobs
        .iter()
        .map(|job| PlannedJob {
            name: job.name.clone(),
            needs: job.needs.clone(),
            support: classify(job),
        })
        .collect();
    Ok(Plan {
        repository: request.repository.clone(),
        revision: request.revision.clone(),
        workflow_path: request.workflow_path.clone(),
        jobs: planned,
        order,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(name: &str, commands: &[&str]) -> JobEvidence {
        JobEvidence {
            name: name.to_owned(),
            needs: Vec::new(),
            runs_on: "ubuntu-latest".to_owned(),
            commands: commands.iter().map(|value| (*value).to_owned()).collect(),
            ..JobEvidence::default()
        }
    }

    fn request() -> PlanRequest {
        PlanRequest {
            repository: "gha-indie-worker/gha-clone-server.rs".to_owned(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            workflow_path: ".github/workflows/ci.yml".to_owned(),
            workflow_yaml: "jobs:\n  test:\n".to_owned(),
        }
    }

    #[test]
    fn profiles_round_trip_through_their_wire_form() {
        for profile in Profile::all() {
            assert_eq!(Profile::parse(profile.as_str()), Ok(*profile));
        }
        assert_eq!(Profile::parse("root-shell"), Err(PlanError::UnknownProfile));
    }

    #[test]
    fn requests_refuse_mutable_revisions_and_stray_paths() {
        let mut bad = request();
        bad.revision = "main".to_owned();
        assert_eq!(validate_request(&bad), Err(PlanError::InvalidRevision));

        let mut bad = request();
        bad.repository = "gha-indie-worker".to_owned();
        assert_eq!(validate_request(&bad), Err(PlanError::InvalidRepository));

        for path in [
            "ci.yml",
            ".github/workflows/../../etc/passwd",
            ".github/workflows/nested/ci.yml",
            ".github/workflows/ci.txt",
            ".github\\workflows\\ci.yml",
        ] {
            assert_eq!(
                validate_workflow_path(path),
                Err(PlanError::InvalidWorkflowPath),
                "{path}"
            );
        }
        assert_eq!(validate_workflow_path(".github/workflows/ci.yaml"), Ok(()));
    }

    #[test]
    fn command_evidence_maps_to_the_documented_profiles() {
        assert_eq!(
            profile_for(&["cargo test --all".to_owned()]),
            Some(Profile::RustVerify)
        );
        assert_eq!(
            profile_for(&["npm ci && npm test".to_owned()]),
            Some(Profile::NodeVerify)
        );
        assert_eq!(
            profile_for(&["python3 -m pytest".to_owned()]),
            Some(Profile::PythonVerify)
        );
        assert_eq!(
            profile_for(&["flutter build apk --debug".to_owned()]),
            Some(Profile::FlutterAndroidDebug)
        );
        assert_eq!(
            profile_for(&["flutter test".to_owned()]),
            Some(Profile::FlutterVerify)
        );
        assert_eq!(profile_for(&["make deploy".to_owned()]), None);
    }

    #[test]
    fn artifact_profiles_win_over_a_plain_flutter_verify() {
        assert_eq!(
            profile_for(&["flutter test".to_owned(), "flutter build web".to_owned()]),
            Some(Profile::FlutterWebRelease)
        );
    }

    #[test]
    fn every_exclusion_is_reported_rather_than_approximated() {
        let cases: [(fn(&mut JobEvidence), Exclusion); 5] = [
            (
                |job| job.references_secret = true,
                Exclusion::SecretExpression,
            ),
            (|job| job.has_matrix = true, Exclusion::DynamicMatrix),
            (
                |job| job.has_condition = true,
                Exclusion::ConditionalExecution,
            ),
            (
                |job| job.has_container = true,
                Exclusion::JobOrServiceContainer,
            ),
            (
                |job| job.has_environment = true,
                Exclusion::EnvironmentOrDeployment,
            ),
        ];
        for (mutate, expected) in cases {
            let mut job = evidence("test", &["cargo test"]);
            mutate(&mut job);
            assert_eq!(
                classify(&job),
                JobSupport::Unsupported { reason: expected },
                "{expected:?}"
            );
        }
    }

    #[test]
    fn native_runners_are_delegated_not_claimed() {
        let mut job = evidence("build", &["flutter build ios"]);
        job.runs_on = "macos-14".to_owned();
        assert_eq!(
            classify(&job),
            JobSupport::DelegatedToArc {
                lane: "github-hosted-native".to_owned()
            }
        );
    }

    #[test]
    fn untrusted_marketplace_actions_are_refused() {
        let mut job = evidence("test", &["cargo test"]);
        job.uses = vec!["evil/pwn-action@v1".to_owned()];
        assert_eq!(
            classify(&job),
            JobSupport::Unsupported {
                reason: Exclusion::MarketplaceAction
            }
        );

        let mut job = evidence("test", &["cargo test"]);
        job.uses = vec!["actions/checkout@v4".to_owned()];
        assert!(classify(&job).is_independent());
    }

    #[test]
    fn topological_order_is_deterministic_and_dependency_respecting() {
        let mut deploy = evidence("deploy", &["cargo build"]);
        deploy.needs = vec!["test".to_owned(), "lint".to_owned()];
        let jobs = vec![
            deploy,
            evidence("test", &["cargo test"]),
            evidence("lint", &["cargo clippy"]),
        ];
        let order = topological_order(&jobs).expect("acyclic");
        assert_eq!(order, vec!["lint", "test", "deploy"]);
        assert_eq!(topological_order(&jobs).expect("stable"), order);
    }

    #[test]
    fn unknown_and_cyclic_dependencies_are_typed_errors() {
        let mut orphan = evidence("a", &["cargo test"]);
        orphan.needs = vec!["missing".to_owned()];
        assert_eq!(
            topological_order(&[orphan]),
            Err(PlanError::UnknownDependency)
        );

        let mut first = evidence("a", &["cargo test"]);
        first.needs = vec!["b".to_owned()];
        let mut second = evidence("b", &["cargo test"]);
        second.needs = vec!["a".to_owned()];
        assert_eq!(
            topological_order(&[first, second]),
            Err(PlanError::DependencyCycle)
        );
    }

    #[test]
    fn a_plan_is_enqueueable_only_when_every_job_is_independent() {
        let plan = compile(
            &request(),
            vec![
                evidence("test", &["cargo test"]),
                evidence("web", &["npm ci"]),
            ],
        )
        .expect("compiles");
        assert!(plan.fully_supported());
        assert_eq!(plan.order, vec!["test", "web"]);

        let mut blocked = evidence("release", &["cargo build"]);
        blocked.has_matrix = true;
        let plan = compile(&request(), vec![evidence("test", &["cargo test"]), blocked])
            .expect("compiles");
        assert!(!plan.fully_supported());
        assert_eq!(plan.unsupported().len(), 1);
    }

    #[test]
    fn plans_are_bounded_in_job_count() {
        assert_eq!(compile(&request(), Vec::new()), Err(PlanError::NoJobs));
        let many = (0..=MAX_JOBS)
            .map(|index| evidence(&format!("job{index}"), &["cargo test"]))
            .collect();
        assert_eq!(compile(&request(), many), Err(PlanError::TooManyJobs));
    }

    const WORKFLOW: &str = r#"
name: ci
on:
  push:
    branches: [main]
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo clippy --all-targets
  test:
    runs-on: ubuntu-latest
    needs: [lint]
    steps:
      - uses: actions/checkout@v4
      - name: Run tests
        run: |
          cargo fmt --check
          cargo test --all
"#;

    #[test]
    fn the_scanner_finds_jobs_dependencies_runners_actions_and_commands() {
        let jobs = scan_workflow(WORKFLOW).expect("scans");
        assert_eq!(jobs.len(), 2);

        let lint = &jobs[0];
        assert_eq!(lint.name, "lint");
        assert_eq!(lint.runs_on, "ubuntu-latest");
        assert_eq!(lint.uses, vec!["actions/checkout@v4".to_owned()]);
        assert_eq!(lint.commands, vec!["cargo clippy --all-targets".to_owned()]);
        assert!(lint.needs.is_empty());

        let test = &jobs[1];
        assert_eq!(test.name, "test");
        assert_eq!(test.needs, vec!["lint".to_owned()]);
        // The block scalar's continuation lines are command evidence.
        assert!(test.commands.iter().any(|c| c.contains("cargo test --all")));
        assert!(test
            .commands
            .iter()
            .any(|c| c.contains("cargo fmt --check")));
    }

    #[test]
    fn a_scanned_workflow_compiles_to_an_executable_plan() {
        let jobs = scan_workflow(WORKFLOW).expect("scans");
        let plan = compile(&request(), jobs).expect("compiles");
        assert!(plan.fully_supported());
        assert_eq!(plan.order, vec!["lint", "test"]);
        assert!(plan.jobs.iter().all(|job| job.support
            == JobSupport::Independent {
                profile: Profile::RustVerify
            }));
    }

    #[test]
    fn the_scanner_flags_every_unsupported_construct() {
        let yaml = r#"
jobs:
  secret_job:
    runs-on: ubuntu-latest
    steps:
      - run: deploy --token ${{ secrets.DEPLOY_TOKEN }}
  matrix_job:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        node: [18, 20]
    steps:
      - run: npm test
  conditional_job:
    runs-on: ubuntu-latest
    if: github.ref == 'refs/heads/main'
    steps:
      - run: npm test
  container_job:
    runs-on: ubuntu-latest
    container: node:20
    steps:
      - run: npm test
  deploy_job:
    runs-on: ubuntu-latest
    environment: production
    steps:
      - run: npm test
"#;
        let jobs = scan_workflow(yaml).expect("scans");
        assert_eq!(jobs.len(), 5);
        assert!(jobs[0].references_secret);
        assert!(jobs[1].has_matrix);
        assert!(jobs[2].has_condition);
        assert!(jobs[3].has_container);
        assert!(jobs[4].has_environment);
        for job in &jobs {
            assert!(!classify(job).is_independent(), "{}", job.name);
        }
    }

    #[test]
    fn the_scanner_ignores_everything_outside_the_jobs_block() {
        let yaml = r#"
name: ci
env:
  run: this is not a job command
on:
  push:
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: cargo build
"#;
        let jobs = scan_workflow(yaml).expect("scans");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].commands, vec!["cargo build".to_owned()]);
    }

    #[test]
    fn the_scanner_is_bounded_by_size_and_job_count() {
        let oversized = "x".repeat(MAX_WORKFLOW_BYTES + 1);
        assert_eq!(scan_workflow(&oversized), Err(PlanError::WorkflowTooLarge));

        let mut yaml = String::from("jobs:\n");
        for index in 0..=MAX_JOBS {
            yaml.push_str(&format!("  job{index}:\n    runs-on: ubuntu-latest\n"));
        }
        assert_eq!(scan_workflow(&yaml), Err(PlanError::TooManyJobs));
    }

    #[test]
    fn needs_is_parsed_from_both_inline_forms() {
        assert_eq!(parse_needs(" build"), vec!["build".to_owned()]);
        assert_eq!(
            parse_needs(" [build, test]"),
            vec!["build".to_owned(), "test".to_owned()]
        );
        assert_eq!(parse_needs(" [\"build\"]"), vec!["build".to_owned()]);
        assert!(parse_needs("").is_empty());
    }
}
