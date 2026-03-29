use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tower_lsp::lsp_types::Url;

use crate::salsa_db::ParseTreeOrigin;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub struct RequestMetrics {
    request_id: u64,
    request_kind: &'static str,
    uri: Arc<str>,
    index_view_acquisitions: AtomicUsize,
    semantic_context_lookups: AtomicUsize,
    artifact_class_projections: AtomicUsize,
    artifact_method_materializations: AtomicUsize,
    artifact_field_materializations: AtomicUsize,
    event_counts: Mutex<HashMap<&'static str, usize>>,
    phase_stats: Mutex<HashMap<&'static str, PhaseStat>>,
    hot_spots: Mutex<Vec<HotSpot>>,
    parse_snapshots: Mutex<HashMap<(&'static str, &'static str), usize>>,
}

#[derive(Clone, Copy, Default)]
struct PhaseStat {
    count: usize,
    total_ns: u64,
}

#[derive(Clone)]
struct HotSpot {
    phase: &'static str,
    offset: usize,
    elapsed_ns: u64,
}

impl RequestMetrics {
    pub fn new(request_kind: &'static str, uri: &Url) -> Arc<Self> {
        Arc::new(Self {
            request_id: NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
            request_kind,
            uri: Arc::from(uri.as_str()),
            index_view_acquisitions: AtomicUsize::new(0),
            semantic_context_lookups: AtomicUsize::new(0),
            artifact_class_projections: AtomicUsize::new(0),
            artifact_method_materializations: AtomicUsize::new(0),
            artifact_field_materializations: AtomicUsize::new(0),
            event_counts: Mutex::new(HashMap::new()),
            phase_stats: Mutex::new(HashMap::new()),
            hot_spots: Mutex::new(Vec::new()),
            parse_snapshots: Mutex::new(HashMap::new()),
        })
    }

    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    pub fn request_kind(&self) -> &'static str {
        self.request_kind
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn record_index_view_acquisition(
        &self,
        callsite: &'static str,
        module: u32,
        classpath: impl std::fmt::Debug,
        source_root: Option<u32>,
        nested: bool,
    ) {
        let count = self.index_view_acquisitions.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::debug!(
            request_id = self.request_id,
            request_kind = self.request_kind,
            uri = %self.uri,
            callsite,
            nested,
            acquisition_count = count,
            module,
            classpath = ?classpath,
            source_root = ?source_root,
            "request IndexView acquisition"
        );
    }

    pub fn record_semantic_context_lookup(&self, phase: &'static str, offset: usize) {
        let count = self
            .semantic_context_lookups
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        tracing::trace!(
            request_id = self.request_id,
            request_kind = self.request_kind,
            uri = %self.uri,
            phase,
            offset,
            lookup_count = count,
            "request semantic context lookup"
        );
    }

    pub fn index_view_acquisition_count(&self) -> usize {
        self.index_view_acquisitions.load(Ordering::Relaxed)
    }

    pub fn semantic_context_lookup_count(&self) -> usize {
        self.semantic_context_lookups.load(Ordering::Relaxed)
    }

    pub fn record_artifact_class_projection(&self, count: usize) {
        self.artifact_class_projections
            .fetch_add(count, Ordering::Relaxed);
    }

    pub fn record_artifact_method_materialization(&self, count: usize) {
        self.artifact_method_materializations
            .fetch_add(count, Ordering::Relaxed);
    }

    pub fn record_artifact_field_materialization(&self, count: usize) {
        self.artifact_field_materializations
            .fetch_add(count, Ordering::Relaxed);
    }

    pub fn artifact_class_projection_count(&self) -> usize {
        self.artifact_class_projections.load(Ordering::Relaxed)
    }

    pub fn artifact_method_materialization_count(&self) -> usize {
        self.artifact_method_materializations
            .load(Ordering::Relaxed)
    }

    pub fn artifact_field_materialization_count(&self) -> usize {
        self.artifact_field_materializations.load(Ordering::Relaxed)
    }

    pub fn record_event(&self, event: &'static str) {
        let mut event_counts = self.event_counts.lock().expect("event counts poisoned");
        *event_counts.entry(event).or_insert(0) += 1;
    }

    pub fn record_parse_snapshot(&self, callsite: &'static str, origin: Option<ParseTreeOrigin>) {
        let origin = origin.map(ParseTreeOrigin::as_str).unwrap_or("missing");
        {
            let mut parse_snapshots = self
                .parse_snapshots
                .lock()
                .expect("parse snapshots poisoned");
            *parse_snapshots.entry((callsite, origin)).or_insert(0) += 1;
        }

        tracing::debug!(
            request_id = self.request_id,
            request_kind = self.request_kind,
            uri = %self.uri,
            callsite,
            parse_origin = origin,
            "request parse snapshot"
        );
    }

    pub fn record_phase_duration(&self, phase: &'static str, elapsed: Duration) {
        self.record_phase_duration_at(phase, None, elapsed);
    }

    pub fn record_phase_duration_at(
        &self,
        phase: &'static str,
        offset: Option<usize>,
        elapsed: Duration,
    ) {
        let elapsed_ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;
        {
            let mut phase_stats = self.phase_stats.lock().expect("phase stats poisoned");
            let stat = phase_stats.entry(phase).or_default();
            stat.count += 1;
            stat.total_ns = stat.total_ns.saturating_add(elapsed_ns);
        }

        if let Some(offset) = offset {
            let mut hot_spots = self.hot_spots.lock().expect("hot spots poisoned");
            hot_spots.push(HotSpot {
                phase,
                offset,
                elapsed_ns,
            });
            hot_spots.sort_by(|a, b| b.elapsed_ns.cmp(&a.elapsed_ns));
            hot_spots.truncate(8);
        }
    }

    pub fn log_summary(
        &self,
        module: u32,
        classpath: impl std::fmt::Debug,
        source_root: Option<u32>,
        elapsed_ms: f64,
    ) {
        let phase_breakdown = {
            let phase_stats = self.phase_stats.lock().expect("phase stats poisoned");
            let mut items: Vec<String> = phase_stats
                .iter()
                .map(|(phase, stat)| {
                    format!(
                        "{}={}ms/{}",
                        phase,
                        (stat.total_ns as f64) / 1_000_000.0,
                        stat.count
                    )
                })
                .collect();
            items.sort();
            items.join(",")
        };

        let hottest = {
            let hot_spots = self.hot_spots.lock().expect("hot spots poisoned");
            hot_spots
                .iter()
                .take(5)
                .map(|spot| {
                    format!(
                        "{}@{}={}ms",
                        spot.phase,
                        spot.offset,
                        (spot.elapsed_ns as f64) / 1_000_000.0
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        };

        let parse_snapshots = {
            let parse_snapshots = self
                .parse_snapshots
                .lock()
                .expect("parse snapshots poisoned");
            let mut items: Vec<String> = parse_snapshots
                .iter()
                .map(|((callsite, origin), count)| format!("{callsite}:{origin}={count}"))
                .collect();
            items.sort();
            items.join(",")
        };

        let event_counts = {
            let event_counts = self.event_counts.lock().expect("event counts poisoned");
            let mut items: Vec<String> = event_counts
                .iter()
                .map(|(event, count)| format!("{event}={count}"))
                .collect();
            items.sort();
            items.join(",")
        };

        tracing::debug!(
            request_id = self.request_id,
            request_kind = self.request_kind,
            uri = %self.uri,
            module,
            classpath = ?classpath,
            source_root = ?source_root,
            index_view_acquisitions = self.index_view_acquisition_count(),
            semantic_context_lookups = self.semantic_context_lookup_count(),
            artifact_class_projections = self.artifact_class_projection_count(),
            artifact_method_materializations = self.artifact_method_materialization_count(),
            artifact_field_materializations = self.artifact_field_materialization_count(),
            phase_breakdown,
            hottest,
            event_counts,
            parse_snapshots,
            elapsed_ms,
            "request analysis summary"
        );
    }
}
