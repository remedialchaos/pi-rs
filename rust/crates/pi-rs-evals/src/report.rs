//! Vitest JSON report parsing, observation modeling, pairing, and text formatting
//! (port of `report.ts`).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{EvalError, EvalResult};
use crate::plan::{DocumentationVariant, EvalTask};

/// Artifact name used for the session JSONL snapshot.
pub const PI_SESSION_SNAPSHOT_ARTIFACT: &str = "piSessionJsonl";

fn serialize_f64<S>(value: &f64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if value.is_finite() && value.fract() == 0.0 {
        if *value >= 0.0 && *value <= u64::MAX as f64 {
            return serializer.serialize_u64(*value as u64);
        }
        if *value < 0.0 && *value >= i64::MIN as f64 {
            return serializer.serialize_i64(*value as i64);
        }
    }
    serializer.serialize_f64(*value)
}

fn serialize_option_f64<S>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match value {
        Some(value) => serialize_f64(value, serializer),
        None => serializer.serialize_none(),
    }
}

const CONTROL: DocumentationVariant = DocumentationVariant::WithoutDocs;
const TREATMENT: DocumentationVariant = DocumentationVariant::WithDocs;
const VARIANTS: [DocumentationVariant; 2] = [CONTROL, TREATMENT];

/// Identity of one expected or observed evaluation run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EvalRunIdentity {
    #[serde(rename = "evalSet")]
    pub eval_set: String,
    #[serde(rename = "caseId")]
    pub case_id: String,
    pub variant: DocumentationVariant,
    pub model: String,
    #[serde(rename = "runNumber")]
    pub run_number: u64,
}

impl EvalRunIdentity {
    fn from_task(task: &EvalTask) -> Self {
        Self {
            eval_set: task.eval_set.clone(),
            case_id: task.case_id.clone(),
            variant: task.variant,
            model: task.model.clone(),
            run_number: task.run_number,
        }
    }

    fn without_variant(&self) -> IdentityWithoutVariant {
        IdentityWithoutVariant {
            eval_set: self.eval_set.clone(),
            case_id: self.case_id.clone(),
            model: self.model.clone(),
            run_number: self.run_number,
        }
    }
}

/// Alias matching upstream `ExpectedEvalRun` (structurally an identity).
pub type ExpectedEvalRun = EvalRunIdentity;

/// Outcome of an observed evaluation run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvalOutcome {
    Scored,
    Unscored,
    Skipped,
    Pending,
    Errored,
}

impl EvalOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Scored => "scored",
            Self::Unscored => "unscored",
            Self::Skipped => "skipped",
            Self::Pending => "pending",
            Self::Errored => "errored",
        }
    }
}

impl Display for EvalOutcome {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One observed run with optional operational metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalObservation {
    #[serde(rename = "evalSet")]
    pub eval_set: String,
    #[serde(rename = "caseId")]
    pub case_id: String,
    pub variant: DocumentationVariant,
    pub model: String,
    #[serde(rename = "runNumber")]
    pub run_number: u64,
    #[serde(
        rename = "inputTokens",
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_option_f64"
    )]
    pub input_tokens: Option<f64>,
    #[serde(
        rename = "outputTokens",
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_option_f64"
    )]
    pub output_tokens: Option<f64>,
    #[serde(
        rename = "cacheReadTokens",
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_option_f64"
    )]
    pub cache_read_tokens: Option<f64>,
    #[serde(
        rename = "cacheWriteTokens",
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_option_f64"
    )]
    pub cache_write_tokens: Option<f64>,
    #[serde(
        rename = "totalTokens",
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_option_f64"
    )]
    pub total_tokens: Option<f64>,
    #[serde(
        rename = "toolCalls",
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_option_f64"
    )]
    pub tool_calls: Option<f64>,
    #[serde(
        rename = "totalMs",
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_option_f64"
    )]
    pub total_ms: Option<f64>,
    #[serde(
        rename = "estimatedCostUsd",
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_option_f64"
    )]
    pub estimated_cost_usd: Option<f64>,
    pub outcome: EvalOutcome,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_option_f64"
    )]
    pub score: Option<f64>,
}

impl EvalObservation {
    fn from_identity(identity: EvalRunIdentity, outcome: EvalOutcome, score: Option<f64>) -> Self {
        Self {
            eval_set: identity.eval_set,
            case_id: identity.case_id,
            variant: identity.variant,
            model: identity.model,
            run_number: identity.run_number,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            total_tokens: None,
            tool_calls: None,
            total_ms: None,
            estimated_cost_usd: None,
            outcome,
            score,
        }
    }
}

/// Paired summary for a single operational metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairedMetricSummary {
    #[serde(rename = "eligiblePairs")]
    pub eligible_pairs: usize,
    #[serde(rename = "controlMean")]
    #[serde(serialize_with = "serialize_option_f64")]
    pub control_mean: Option<f64>,
    #[serde(rename = "treatmentMean")]
    #[serde(serialize_with = "serialize_option_f64")]
    pub treatment_mean: Option<f64>,
    #[serde(rename = "meanDelta")]
    #[serde(serialize_with = "serialize_option_f64")]
    pub mean_delta: Option<f64>,
}

/// Comparison flags emitted for a documentation eval set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComparisonFlag {
    NoLift,
    NegativeDelta,
    ControlSaturated,
    TreatmentSaturated,
    Flaky,
}

impl ComparisonFlag {
    fn as_str(self) -> &'static str {
        match self {
            Self::NoLift => "no-lift",
            Self::NegativeDelta => "negative-delta",
            Self::ControlSaturated => "control-saturated",
            Self::TreatmentSaturated => "treatment-saturated",
            Self::Flaky => "flaky",
        }
    }
}

impl Display for ComparisonFlag {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Paired comparison for one eval set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalSetComparison {
    #[serde(rename = "evalSet")]
    pub eval_set: String,
    #[serde(rename = "totalPairs")]
    pub total_pairs: usize,
    #[serde(rename = "eligiblePairs")]
    pub eligible_pairs: usize,
    #[serde(rename = "blockedPairs")]
    pub blocked_pairs: usize,
    #[serde(rename = "controlPassRate")]
    #[serde(serialize_with = "serialize_option_f64")]
    pub control_pass_rate: Option<f64>,
    #[serde(rename = "treatmentPassRate")]
    #[serde(serialize_with = "serialize_option_f64")]
    pub treatment_pass_rate: Option<f64>,
    #[serde(serialize_with = "serialize_option_f64")]
    pub lift: Option<f64>,
    pub flags: Vec<ComparisonFlag>,
    #[serde(rename = "totalTokens")]
    pub total_tokens: PairedMetricSummary,
    #[serde(rename = "toolCalls")]
    pub tool_calls: PairedMetricSummary,
    #[serde(rename = "totalMs")]
    pub total_ms: PairedMetricSummary,
    #[serde(rename = "estimatedCostUsd")]
    pub estimated_cost_usd: PairedMetricSummary,
}

/// A run pair that could not be scored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockedPair {
    #[serde(rename = "evalSet")]
    pub eval_set: String,
    #[serde(rename = "caseId")]
    pub case_id: String,
    pub model: String,
    #[serde(rename = "runNumber")]
    pub run_number: u64,
    pub reasons: Vec<String>,
}

/// Total and coverage for one operational metric in one variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationalMetricTotal {
    #[serde(rename = "availableRuns")]
    pub available_runs: usize,
    #[serde(serialize_with = "serialize_option_f64")]
    pub total: Option<f64>,
}

/// Operational totals for one documentation variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariantTotals {
    pub variant: DocumentationVariant,
    pub runs: usize,
    #[serde(rename = "inputTokens")]
    pub input_tokens: OperationalMetricTotal,
    #[serde(rename = "outputTokens")]
    pub output_tokens: OperationalMetricTotal,
    #[serde(rename = "cacheReadTokens")]
    pub cache_read_tokens: OperationalMetricTotal,
    #[serde(rename = "cacheWriteTokens")]
    pub cache_write_tokens: OperationalMetricTotal,
    #[serde(rename = "totalTokens")]
    pub total_tokens: OperationalMetricTotal,
    #[serde(rename = "toolCalls")]
    pub tool_calls: OperationalMetricTotal,
    #[serde(rename = "totalMs")]
    pub total_ms: OperationalMetricTotal,
    #[serde(rename = "estimatedCostUsd")]
    pub estimated_cost_usd: OperationalMetricTotal,
}

/// Full comparison report (`schemaVersion: 3`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalComparisonReport {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u8,
    #[serde(rename = "protocolDigest")]
    pub protocol_digest: String,
    pub control: DocumentationVariant,
    pub treatment: DocumentationVariant,
    pub comparisons: Vec<EvalSetComparison>,
    #[serde(rename = "blockedPairs")]
    pub blocked_pairs: Vec<BlockedPair>,
    #[serde(rename = "operationalTotals")]
    pub operational_totals: Vec<VariantTotals>,
}

/// Build the observation used when a task could not be executed.
pub fn errored_observation(task: &EvalTask) -> EvalObservation {
    EvalObservation::from_identity(EvalRunIdentity::from_task(task), EvalOutcome::Errored, None)
}

// ---------------------------------------------------------------------------
// Vitest JSON report parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum VitestStatus {
    Pending,
    Passed,
    Failed,
    Skipped,
    Todo,
    Disabled,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VitestReport {
    #[serde(default)]
    test_results: Vec<VitestFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VitestFile {
    #[serde(default)]
    assertion_results: Vec<VitestAssertion>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VitestAssertion {
    full_name: String,
    status: VitestStatus,
    meta: Option<Value>,
}

struct CollectedCase {
    full_name: String,
    status: VitestStatus,
    eval: Option<Value>,
    harness: Option<Value>,
}

fn classify_case_status(status: VitestStatus) -> Option<EvalOutcome> {
    match status {
        VitestStatus::Failed => Some(EvalOutcome::Errored),
        VitestStatus::Skipped | VitestStatus::Todo | VitestStatus::Disabled => {
            Some(EvalOutcome::Skipped)
        }
        VitestStatus::Pending => Some(EvalOutcome::Pending),
        VitestStatus::Passed => None,
    }
}

fn optional_metric(value: Option<&Value>, name: &str) -> EvalResult<Option<f64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(number) = value.as_f64() else {
        return Err(EvalError::message(format!(
            "{name} must be a finite non-negative number."
        )));
    };
    if !number.is_finite() || number < 0.0 {
        return Err(EvalError::message(format!(
            "{name} must be a finite non-negative number."
        )));
    }
    Ok(Some(number))
}

fn validate_score(value: Option<&Value>) -> EvalResult<Option<f64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(score) = value.as_f64() else {
        return Err(EvalError::message("Eval score must be between 0 and 1."));
    };
    if !score.is_finite() || !(0.0..=1.0).contains(&score) {
        return Err(EvalError::message("Eval score must be between 0 and 1."));
    }
    Ok(Some(score))
}

#[cfg(unix)]
fn apply_mode(path: &Path, mode: u32) -> EvalResult<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn apply_mode(_path: &Path, _mode: u32) -> EvalResult<()> {
    Ok(())
}

fn persist_session(
    case: &CollectedCase,
    task: &EvalTask,
    artifact_directory: &Path,
) -> EvalResult<()> {
    let Some(harness) = case.harness.as_ref() else {
        return Ok(());
    };
    let Some(session) = harness
        .get("run")
        .and_then(|run| run.get("artifacts"))
        .and_then(|artifacts| artifacts.get(PI_SESSION_SNAPSHOT_ARTIFACT))
        .and_then(Value::as_str)
    else {
        return Ok(());
    };
    let directory = artifact_directory
        .join(task.variant.as_str())
        .join("sessions")
        .join(hex_sha256(task.identity_json().as_bytes()));
    std::fs::create_dir_all(&directory)?;
    apply_mode(&directory, 0o700)?;
    let session_path = directory.join("session.jsonl");
    std::fs::write(&session_path, session)?;
    apply_mode(&session_path, 0o600)?;
    Ok(())
}

/// Read one task observation from a Vitest JSON report, persisting any session
/// snapshot artifact under `artifact_directory`.
pub fn read_task_observation(
    task: &EvalTask,
    report_path: &Path,
    artifact_directory: &Path,
) -> EvalResult<EvalObservation> {
    let identity = EvalRunIdentity::from_task(task);
    let report_text = match std::fs::read_to_string(report_path) {
        Ok(text) => text,
        Err(_) => {
            return Ok(EvalObservation::from_identity(
                identity,
                EvalOutcome::Errored,
                None,
            ));
        }
    };
    let report: VitestReport = match serde_json::from_str(&report_text) {
        Ok(report) => report,
        Err(_) => {
            return Ok(EvalObservation::from_identity(
                identity,
                EvalOutcome::Errored,
                None,
            ));
        }
    };

    let assertions: Vec<&VitestAssertion> = report
        .test_results
        .iter()
        .flat_map(|file| file.assertion_results.iter())
        .filter(|assertion| assertion.full_name == format!("{} {}", task.eval_set, task.case_id))
        .collect();
    if assertions.len() != 1 {
        return Ok(EvalObservation::from_identity(
            identity,
            EvalOutcome::Errored,
            None,
        ));
    }
    let assertion = assertions[0];
    let status_outcome = classify_case_status(assertion.status);
    if matches!(
        status_outcome,
        Some(EvalOutcome::Skipped | EvalOutcome::Pending)
    ) {
        return Ok(EvalObservation::from_identity(
            identity,
            status_outcome.unwrap_or(EvalOutcome::Errored),
            None,
        ));
    }

    let mut cases: Vec<CollectedCase> = Vec::new();
    for file in &report.test_results {
        for candidate in &file.assertion_results {
            let Some(meta) = candidate.meta.as_ref().filter(|meta| meta.is_object()) else {
                continue;
            };
            let eval = meta.get("eval").cloned();
            let harness = meta.get("harness").cloned();
            if eval.is_none() && harness.is_none() {
                continue;
            }
            cases.push(CollectedCase {
                full_name: candidate.full_name.clone(),
                status: candidate.status,
                eval,
                harness,
            });
        }
    }
    let cases: Vec<CollectedCase> = cases
        .into_iter()
        .filter(|case| case.full_name == format!("{} {}", task.eval_set, task.case_id))
        .collect();
    if cases.len() != 1 {
        return Ok(EvalObservation::from_identity(
            identity,
            EvalOutcome::Errored,
            None,
        ));
    }
    let case = &cases[0];
    if case.status != assertion.status {
        return Ok(EvalObservation::from_identity(
            identity,
            EvalOutcome::Errored,
            None,
        ));
    }
    let Some(run) = case.harness.as_ref().and_then(|harness| harness.get("run")) else {
        return Ok(EvalObservation::from_identity(
            identity,
            EvalOutcome::Errored,
            None,
        ));
    };
    persist_session(case, task, artifact_directory)?;
    let usage = run.get("usage");
    let actual_model = usage
        .and_then(|usage| usage.get("provider"))
        .and_then(Value::as_str)
        .zip(
            usage
                .and_then(|usage| usage.get("model"))
                .and_then(Value::as_str),
        )
        .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
        .map(|(provider, model)| format!("{provider}/{model}"));
    if actual_model.as_deref() != Some(task.model.as_str()) {
        return Ok(EvalObservation::from_identity(
            identity,
            EvalOutcome::Errored,
            None,
        ));
    }
    let metadata = usage.and_then(|usage| usage.get("metadata"));
    let metrics = {
        let input_tokens = optional_metric(usage.and_then(|u| u.get("inputTokens")), "inputTokens");
        let output_tokens =
            optional_metric(usage.and_then(|u| u.get("outputTokens")), "outputTokens");
        let cache_read_tokens = optional_metric(
            metadata.and_then(|m| m.get("cacheReadTokens")),
            "cacheReadTokens",
        );
        let cache_write_tokens = optional_metric(
            metadata.and_then(|m| m.get("cacheWriteTokens")),
            "cacheWriteTokens",
        );
        let total_tokens = optional_metric(usage.and_then(|u| u.get("totalTokens")), "totalTokens");
        let tool_calls = optional_metric(usage.and_then(|u| u.get("toolCalls")), "toolCalls");
        let total_ms =
            optional_metric(run.get("timings").and_then(|t| t.get("totalMs")), "totalMs");
        let estimated_cost_usd = optional_metric(
            metadata.and_then(|m| m.get("estimatedCostUsd")),
            "estimatedCostUsd",
        );
        match (
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            total_tokens,
            tool_calls,
            total_ms,
            estimated_cost_usd,
        ) {
            (
                Ok(input_tokens),
                Ok(output_tokens),
                Ok(cache_read_tokens),
                Ok(cache_write_tokens),
                Ok(total_tokens),
                Ok(tool_calls),
                Ok(total_ms),
                Ok(estimated_cost_usd),
            ) => Metrics {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                total_tokens,
                tool_calls,
                total_ms,
                estimated_cost_usd,
            },
            _ => {
                return Ok(EvalObservation::from_identity(
                    identity,
                    EvalOutcome::Errored,
                    None,
                ));
            }
        }
    };
    let has_errors = run
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errors| !errors.is_empty());
    if status_outcome == Some(EvalOutcome::Errored) || has_errors {
        return Ok(metrics.into_observation(identity, EvalOutcome::Errored, None));
    }
    let score = match validate_score(case.eval.as_ref().and_then(|eval| eval.get("avgScore"))) {
        Ok(score) => score,
        Err(_) => return Ok(metrics.into_observation(identity, EvalOutcome::Errored, None)),
    };
    match score {
        Some(score) => Ok(metrics.into_observation(identity, EvalOutcome::Scored, Some(score))),
        None => Ok(metrics.into_observation(identity, EvalOutcome::Unscored, None)),
    }
}

#[derive(Default)]
struct Metrics {
    input_tokens: Option<f64>,
    output_tokens: Option<f64>,
    cache_read_tokens: Option<f64>,
    cache_write_tokens: Option<f64>,
    total_tokens: Option<f64>,
    tool_calls: Option<f64>,
    total_ms: Option<f64>,
    estimated_cost_usd: Option<f64>,
}

impl Metrics {
    fn into_observation(
        self,
        identity: EvalRunIdentity,
        outcome: EvalOutcome,
        score: Option<f64>,
    ) -> EvalObservation {
        let mut observation = EvalObservation::from_identity(identity, outcome, score);
        observation.input_tokens = self.input_tokens;
        observation.output_tokens = self.output_tokens;
        observation.cache_read_tokens = self.cache_read_tokens;
        observation.cache_write_tokens = self.cache_write_tokens;
        observation.total_tokens = self.total_tokens;
        observation.tool_calls = self.tool_calls;
        observation.total_ms = self.total_ms;
        observation.estimated_cost_usd = self.estimated_cost_usd;
        observation
    }
}

// ---------------------------------------------------------------------------
// Summarization
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct IdentityWithoutVariant {
    eval_set: String,
    case_id: String,
    model: String,
    run_number: u64,
}

fn identity_without_variant_json(identity: &IdentityWithoutVariant) -> String {
    serde_json::json!([
        identity.eval_set,
        identity.case_id,
        identity.model,
        identity.run_number
    ])
    .to_string()
}

struct PairGroup {
    identity: IdentityWithoutVariant,
    expected: HashMap<DocumentationVariant, u64>,
    observations: HashMap<DocumentationVariant, Vec<EvalObservation>>,
}

#[derive(Clone)]
struct Pair {
    control: EvalObservation,
    treatment: EvalObservation,
}

fn get_group<'a>(
    groups: &'a mut IndexMap<String, PairGroup>,
    identity: &EvalRunIdentity,
) -> &'a mut PairGroup {
    let key = identity_without_variant_json(&identity.without_variant());
    groups.entry(key).or_insert_with(|| PairGroup {
        identity: identity.without_variant(),
        expected: HashMap::new(),
        observations: HashMap::new(),
    })
}

fn group_pairs(
    expected_runs: &[ExpectedEvalRun],
    observations: &[EvalObservation],
) -> Vec<PairGroup> {
    let mut groups: IndexMap<String, PairGroup> = IndexMap::new();
    for expected in expected_runs {
        let group = get_group(&mut groups, expected);
        *group.expected.entry(expected.variant).or_insert(0) += 1;
    }
    for observation in observations {
        let group = get_group(
            &mut groups,
            &EvalRunIdentity {
                eval_set: observation.eval_set.clone(),
                case_id: observation.case_id.clone(),
                variant: observation.variant,
                model: observation.model.clone(),
                run_number: observation.run_number,
            },
        );
        group
            .observations
            .entry(observation.variant)
            .or_default()
            .push(observation.clone());
    }
    let mut groups: Vec<PairGroup> = groups.into_values().collect();
    groups.sort_by(|left, right| {
        locale_compare(&left.identity.eval_set, &right.identity.eval_set)
            .then_with(|| locale_compare(&left.identity.case_id, &right.identity.case_id))
            .then_with(|| locale_compare(&left.identity.model, &right.identity.model))
            .then_with(|| left.identity.run_number.cmp(&right.identity.run_number))
    });
    groups
}

/// Approximation of Node's default `localeCompare` for ASCII identifiers:
/// case-insensitive ordering with a deterministic tie-break.
fn locale_compare(left: &str, right: &str) -> Ordering {
    let left_lower = left.to_lowercase();
    let right_lower = right.to_lowercase();
    left_lower.cmp(&right_lower).then_with(|| left.cmp(right))
}

enum ResolvedPair {
    Pair(Box<Pair>),
    Blocked(BlockedPair),
}

fn resolve_pair(group: &PairGroup) -> ResolvedPair {
    let mut reasons = Vec::new();
    for variant in VARIANTS {
        let expected = group.expected.get(&variant).copied().unwrap_or(0);
        let observed = group
            .observations
            .get(&variant)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if expected != 1 {
            reasons.push(format!(
                "{variant}: design expected 1 run, found {expected}"
            ));
        }
        if observed.len() as u64 != expected {
            let plural = if expected == 1 { "" } else { "s" };
            reasons.push(format!(
                "{variant}: expected {expected} observation{plural}, found {}",
                observed.len()
            ));
        }
        if expected == 1 && observed.len() == 1 && observed[0].outcome != EvalOutcome::Scored {
            reasons.push(format!("{variant}: {}", observed[0].outcome));
        }
    }
    if !reasons.is_empty() {
        return ResolvedPair::Blocked(BlockedPair {
            eval_set: group.identity.eval_set.clone(),
            case_id: group.identity.case_id.clone(),
            model: group.identity.model.clone(),
            run_number: group.identity.run_number,
            reasons,
        });
    }
    let Some(control) = group
        .observations
        .get(&CONTROL)
        .and_then(|values| values.first())
        .cloned()
    else {
        return ResolvedPair::Blocked(BlockedPair {
            eval_set: group.identity.eval_set.clone(),
            case_id: group.identity.case_id.clone(),
            model: group.identity.model.clone(),
            run_number: group.identity.run_number,
            reasons: vec!["without_docs: expected 1 observation, found 0".to_owned()],
        });
    };
    let Some(treatment) = group
        .observations
        .get(&TREATMENT)
        .and_then(|values| values.first())
        .cloned()
    else {
        return ResolvedPair::Blocked(BlockedPair {
            eval_set: group.identity.eval_set.clone(),
            case_id: group.identity.case_id.clone(),
            model: group.identity.model.clone(),
            run_number: group.identity.run_number,
            reasons: vec!["with_docs: expected 1 observation, found 0".to_owned()],
        });
    };
    ResolvedPair::Pair(Box::new(Pair { control, treatment }))
}

fn to_precision_15(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    format!("{value:.14e}").parse::<f64>().unwrap_or(value)
}

fn difference(treatment: f64, control: f64) -> f64 {
    to_precision_15(treatment - control)
}

fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<f64>() / values.len() as f64)
}

fn summarize_metric(
    pairs: &[Pair],
    select: impl Fn(&EvalObservation) -> Option<f64>,
) -> PairedMetricSummary {
    let mut control = Vec::new();
    let mut treatment = Vec::new();
    for pair in pairs {
        let (Some(control_value), Some(treatment_value)) =
            (select(&pair.control), select(&pair.treatment))
        else {
            continue;
        };
        control.push(control_value);
        treatment.push(treatment_value);
    }
    let control_mean = mean(&control);
    let treatment_mean = mean(&treatment);
    PairedMetricSummary {
        eligible_pairs: control.len(),
        control_mean,
        treatment_mean,
        mean_delta: match (control_mean, treatment_mean) {
            (Some(control_mean), Some(treatment_mean)) => {
                Some(difference(treatment_mean, control_mean))
            }
            _ => None,
        },
    }
}

fn operational_total(
    runs: &[EvalObservation],
    select: impl Fn(&EvalObservation) -> Option<f64>,
) -> OperationalMetricTotal {
    let values: Vec<f64> = runs.iter().filter_map(&select).collect();
    OperationalMetricTotal {
        available_runs: values.len(),
        total: if values.is_empty() {
            None
        } else {
            Some(values.iter().sum())
        },
    }
}

fn variant_totals(
    observations: &[EvalObservation],
    variant: DocumentationVariant,
) -> VariantTotals {
    let runs: Vec<&EvalObservation> = observations
        .iter()
        .filter(|o| o.variant == variant)
        .collect();
    let total = |select: fn(&EvalObservation) -> Option<f64>| {
        operational_total(
            &runs.iter().map(|o| (*o).clone()).collect::<Vec<_>>(),
            select,
        )
    };
    VariantTotals {
        variant,
        runs: runs.len(),
        input_tokens: total(|o| o.input_tokens),
        output_tokens: total(|o| o.output_tokens),
        cache_read_tokens: total(|o| o.cache_read_tokens),
        cache_write_tokens: total(|o| o.cache_write_tokens),
        total_tokens: total(|o| o.total_tokens),
        tool_calls: total(|o| o.tool_calls),
        total_ms: total(|o| o.total_ms),
        estimated_cost_usd: total(|o| o.estimated_cost_usd),
    }
}

fn comparison_flags(
    pairs: &[Pair],
    control_pass_rate: Option<f64>,
    treatment_pass_rate: Option<f64>,
) -> Vec<ComparisonFlag> {
    let mut flags = Vec::new();
    if let (Some(control), Some(treatment)) = (control_pass_rate, treatment_pass_rate) {
        if control == treatment {
            flags.push(ComparisonFlag::NoLift);
        }
        if treatment < control {
            flags.push(ComparisonFlag::NegativeDelta);
        }
        if control == 1.0 {
            flags.push(ComparisonFlag::ControlSaturated);
        }
        if treatment == 1.0 {
            flags.push(ComparisonFlag::TreatmentSaturated);
        }
    }
    let mut outcomes: IndexMap<String, HashSet<String>> = IndexMap::new();
    for pair in pairs {
        for observation in [&pair.control, &pair.treatment] {
            let key =
                serde_json::json!([observation.case_id, observation.variant.as_str()]).to_string();
            outcomes
                .entry(key)
                .or_default()
                .insert((observation.score.unwrap_or(0.0) >= 1.0).to_string());
        }
    }
    if outcomes.values().any(|values| values.len() > 1) {
        flags.push(ComparisonFlag::Flaky);
    }
    flags
}

/// Summarize expected runs and observations into a schema-v3 comparison report.
pub fn summarize_eval_observations(
    protocol_digest: &str,
    expected_runs: &[ExpectedEvalRun],
    observations: &[EvalObservation],
) -> EvalComparisonReport {
    let groups = group_pairs(expected_runs, observations);
    let mut blocked_pairs = Vec::new();
    let mut pairs_by_eval_set: IndexMap<String, Vec<Pair>> = IndexMap::new();
    let mut totals_by_eval_set: IndexMap<String, usize> = IndexMap::new();
    for group in groups {
        *totals_by_eval_set
            .entry(group.identity.eval_set.clone())
            .or_insert(0) += 1;
        match resolve_pair(&group) {
            ResolvedPair::Blocked(blocked) => blocked_pairs.push(blocked),
            ResolvedPair::Pair(pair) => pairs_by_eval_set
                .entry(group.identity.eval_set.clone())
                .or_default()
                .push(*pair),
        }
    }
    let mut eval_sets: Vec<(String, usize)> = totals_by_eval_set.into_iter().collect();
    eval_sets.sort_by(|left, right| locale_compare(&left.0, &right.0));
    let mut comparisons = Vec::with_capacity(eval_sets.len());
    for (eval_set, total_pairs) in eval_sets {
        let pairs = pairs_by_eval_set
            .get(&eval_set)
            .cloned()
            .unwrap_or_default();
        let blocked = total_pairs - pairs.len();
        let publish_headline = blocked == 0 && !pairs.is_empty();
        let control_pass_rate = publish_headline.then(|| {
            pairs
                .iter()
                .filter(|pair| pair.control.score.unwrap_or(0.0) >= 1.0)
                .count() as f64
                / pairs.len() as f64
        });
        let treatment_pass_rate = publish_headline.then(|| {
            pairs
                .iter()
                .filter(|pair| pair.treatment.score.unwrap_or(0.0) >= 1.0)
                .count() as f64
                / pairs.len() as f64
        });
        comparisons.push(EvalSetComparison {
            eval_set,
            total_pairs,
            eligible_pairs: pairs.len(),
            blocked_pairs: blocked,
            control_pass_rate,
            treatment_pass_rate,
            lift: match (control_pass_rate, treatment_pass_rate) {
                (Some(control), Some(treatment)) => Some(difference(treatment, control)),
                _ => None,
            },
            flags: comparison_flags(&pairs, control_pass_rate, treatment_pass_rate),
            total_tokens: summarize_metric(&pairs, |o| o.total_tokens),
            tool_calls: summarize_metric(&pairs, |o| o.tool_calls),
            total_ms: summarize_metric(&pairs, |o| o.total_ms),
            estimated_cost_usd: summarize_metric(&pairs, |o| o.estimated_cost_usd),
        });
    }
    EvalComparisonReport {
        schema_version: 3,
        protocol_digest: protocol_digest.to_owned(),
        control: CONTROL,
        treatment: TREATMENT,
        comparisons,
        blocked_pairs,
        operational_totals: vec![
            variant_totals(observations, CONTROL),
            variant_totals(observations, TREATMENT),
        ],
    }
}

// ---------------------------------------------------------------------------
// Text formatting
// ---------------------------------------------------------------------------

fn percentage(value: Option<f64>) -> String {
    match value {
        None => "unavailable".to_owned(),
        Some(value) => format!("{:.1}%", value * 100.0),
    }
}

fn signed(value: f64, digits: usize) -> String {
    let sign = if value >= 0.0 { "+" } else { "" };
    format!("{sign}{value:.digits$}")
}

fn paired_metric(label: &str, metric: &PairedMetricSummary, unit: &str) -> String {
    match (
        metric.mean_delta,
        metric.control_mean,
        metric.treatment_mean,
    ) {
        (Some(delta), Some(control), Some(treatment)) => format!(
            "    {:>10}  {}{unit} (with {:.1}{unit}, without {:.1}{unit}, {} pairs)",
            label,
            signed(delta, 1),
            treatment,
            control,
            metric.eligible_pairs
        ),
        _ => format!("    {:>10}  unavailable", label),
    }
}

fn operational_metric(
    metric: &OperationalMetricTotal,
    runs: usize,
    format: impl Fn(f64) -> String,
) -> String {
    match metric.total {
        None => format!("unavailable (0/{runs} measured)"),
        Some(total) => {
            let coverage = if metric.available_runs == runs {
                String::new()
            } else {
                format!(" ({}/{} measured)", metric.available_runs, runs)
            };
            format!("{}{coverage}", format(total))
        }
    }
}

/// Render the report as the exact upstream text layout.
pub fn format_eval_comparison_report(report: &EvalComparisonReport) -> String {
    format_eval_comparison_report_with_style(report, std::io::stdout().is_terminal())
}

/// Render the report with explicit ANSI bold title styling control.
pub fn format_eval_comparison_report_with_style(
    report: &EvalComparisonReport,
    bold: bool,
) -> String {
    if report.comparisons.is_empty() {
        return String::new();
    }
    let title = "Documentation Eval Comparisons";
    let title = if bold {
        format!("\x1b[1m{title}\x1b[22m")
    } else {
        title.to_owned()
    };
    let mut lines = vec![title];
    for comparison in &report.comparisons {
        lines.push(format!("  {}", comparison.eval_set));
        lines.push(format!(
            "         Pairs  {}/{} eligible",
            comparison.eligible_pairs, comparison.total_pairs
        ));
        match comparison.lift {
            None => lines.push(if comparison.blocked_pairs > 0 {
                "     Pass rate  withheld because pairs are blocked".to_owned()
            } else {
                "     Pass rate  unavailable".to_owned()
            }),
            Some(lift) => lines.push(format!(
                "     Pass rate  {} pp (with {}, without {})",
                signed(lift * 100.0, 1),
                percentage(comparison.treatment_pass_rate),
                percentage(comparison.control_pass_rate)
            )),
        }
        if !comparison.flags.is_empty() {
            let flags: Vec<String> = comparison.flags.iter().map(ToString::to_string).collect();
            lines.push(format!("         Flags  {}", flags.join(", ")));
        }
        lines.push(paired_metric("Tokens", &comparison.total_tokens, ""));
        lines.push(paired_metric("Tools", &comparison.tool_calls, ""));
        lines.push(paired_metric("Latency", &comparison.total_ms, "ms"));
        let cost = &comparison.estimated_cost_usd;
        match (cost.mean_delta, cost.control_mean, cost.treatment_mean) {
            (Some(delta), Some(control), Some(treatment)) => {
                let sign = if delta >= 0.0 { "+" } else { "-" };
                lines.push(format!(
                    "     Est. cost  {sign}${:.4} (with ${:.4}, without ${:.4}, {} pairs)",
                    delta.abs(),
                    treatment,
                    control,
                    cost.eligible_pairs
                ));
            }
            _ => lines.push("     Est. cost  unavailable".to_owned()),
        }
    }
    lines.push("  Operational totals".to_owned());
    for totals in &report.operational_totals {
        let tokens = operational_metric(&totals.total_tokens, totals.runs, |total| {
            format!("{total} tokens")
        });
        let tools = operational_metric(&totals.tool_calls, totals.runs, |total| {
            format!("{total} tools")
        });
        let latency = operational_metric(&totals.total_ms, totals.runs, |total| {
            format!("{:.2}s", total / 1000.0)
        });
        let cost = operational_metric(&totals.estimated_cost_usd, totals.runs, |total| {
            format!("${total:.4} cost")
        });
        lines.push(format!(
            "    {}: {} runs, {tokens}, {tools}, {latency}, {cost}",
            totals.variant, totals.runs
        ));
    }
    if !report.blocked_pairs.is_empty() {
        lines.push("  Blocked pairs".to_owned());
        for blocked in &report.blocked_pairs {
            lines.push(format!(
                "    {}/{}/{}/run-{}: {}",
                blocked.eval_set,
                blocked.case_id,
                blocked.model,
                blocked.run_number,
                blocked.reasons.join("; ")
            ));
        }
    }
    lines.join("\n")
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut text = String::with_capacity(digest.len() * 2);
    for byte in digest {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// Return the SHA-256 hex digest of a byte slice.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_sha256(bytes)
}

/// Resolve a path under an artifact directory for a persisted session snapshot.
pub fn session_snapshot_path(artifact_directory: &Path, task: &EvalTask) -> PathBuf {
    artifact_directory
        .join(task.variant.as_str())
        .join("sessions")
        .join(hex_sha256(task.identity_json().as_bytes()))
        .join("session.jsonl")
}
