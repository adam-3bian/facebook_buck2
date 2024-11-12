/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::cmp::max;
use std::cmp::min;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::io::Write;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use anyhow::Context;
use async_trait::async_trait;
use buck2_cli_proto::command_result;
use buck2_common::build_count::BuildCount;
use buck2_common::build_count::BuildCountManager;
use buck2_common::convert::ProstDurationExt;
use buck2_core::fs::fs_util;
use buck2_core::fs::paths::abs_path::AbsPathBuf;
use buck2_core::soft_error;
use buck2_data::error::ErrorTag;
use buck2_data::ErrorReport;
use buck2_data::ProcessedErrorReport;
use buck2_data::SystemInfo;
use buck2_data::TargetCfg;
use buck2_error::classify::best_error;
use buck2_error::classify::best_tag;
use buck2_error::classify::ErrorLike;
use buck2_error::classify::ERROR_TAG_UNCLASSIFIED;
use buck2_error::AnyhowContextForError;
use buck2_event_log::ttl::manifold_event_log_ttl;
use buck2_event_observer::action_stats;
use buck2_event_observer::action_stats::ActionStats;
use buck2_event_observer::cache_hit_rate::total_cache_hit_rate;
use buck2_event_observer::last_command_execution_kind;
use buck2_event_observer::last_command_execution_kind::LastCommandExecutionKind;
use buck2_events::errors::create_error_report;
use buck2_events::sink::remote::new_remote_event_sink_if_enabled;
use buck2_events::BuckEvent;
use buck2_util::cleanup_ctx::AsyncCleanupContext;
use buck2_util::network_speed_average::NetworkSpeedAverage;
use buck2_util::sliding_window::SlidingWindow;
use buck2_wrapper_common::invocation_id::TraceId;
use dupe::Dupe;
use fbinit::FacebookInit;
use futures::FutureExt;
use gazebo::prelude::VecExt;
use gazebo::variants::VariantName;
use itertools::Itertools;
use termwiz::istty::IsTty;

use super::system_warning::check_memory_pressure;
use super::system_warning::check_remaining_disk_space;
use crate::client_ctx::ClientCommandContext;
use crate::client_metadata::ClientMetadata;
use crate::common::CommonEventLogOptions;
use crate::console_interaction_stream::SuperConsoleToggle;
use crate::subscribers::classify_server_stderr::classify_server_stderr;
use crate::subscribers::observer::ErrorObserver;
use crate::subscribers::subscriber::EventSubscriber;
use crate::subscribers::system_warning::check_cache_misses;
use crate::subscribers::system_warning::check_download_speed;
use crate::subscribers::system_warning::is_vpn_enabled;

pub fn process_memory(snapshot: &buck2_data::Snapshot) -> Option<u64> {
    // buck2_rss is the resident set size observed by daemon (exluding subprocesses).
    // On MacOS buck2_rss is not stored and also RSS in general is not a reliable indicator due to swapping which moves pages from resident set to disk.
    // Hence, we take max of buck2_rss and malloc_bytes_active (coming from jemalloc and is available on Macs as well).
    snapshot
        .malloc_bytes_active
        .into_iter()
        .chain(snapshot.buck2_rss)
        .max()
}

const MEMORY_PRESSURE_TAG: &str = "memory_pressure_warning";

pub(crate) struct InvocationRecorder<'a> {
    fb: FacebookInit,
    write_to_path: Option<AbsPathBuf>,
    command_name: &'static str,
    cli_args: Vec<String>,
    isolation_dir: String,
    start_time: Instant,
    async_cleanup_context: AsyncCleanupContext<'a>,
    build_count_manager: Option<BuildCountManager>,
    trace_id: TraceId,
    command_end: Option<buck2_data::CommandEnd>,
    command_duration: Option<prost_types::Duration>,
    re_session_id: Option<String>,
    re_experiment_name: Option<String>,
    critical_path_duration: Option<Duration>,
    tags: Vec<String>,
    run_local_count: u64,
    run_remote_count: u64,
    run_action_cache_count: u64,
    run_remote_dep_file_cache_count: u64,
    run_skipped_count: u64,
    run_fallback_count: u64,
    local_actions_executed_via_worker: u64,
    first_snapshot: Option<buck2_data::Snapshot>,
    last_snapshot: Option<buck2_data::Snapshot>,
    min_attempted_build_count_since_rebase: u64,
    min_build_count_since_rebase: u64,
    cache_upload_count: u64,
    cache_upload_attempt_count: u64,
    dep_file_upload_count: u64,
    dep_file_upload_attempt_count: u64,
    parsed_target_patterns: Option<buck2_data::ParsedTargetPatterns>,
    filesystem: String,
    watchman_version: Option<String>,
    eden_version: Option<String>,
    test_info: Option<String>,
    eligible_for_full_hybrid: bool,
    max_event_client_delay: Option<Duration>,
    max_malloc_bytes_active: Option<u64>,
    max_malloc_bytes_allocated: Option<u64>,
    run_command_failure_count: u64,
    event_count: u64,
    time_to_first_action_execution: Option<Duration>,
    materialization_output_size: u64,
    initial_materializer_entries_from_sqlite: Option<u64>,
    time_to_command_start: Option<Duration>,
    time_to_command_critical_section: Option<Duration>,
    time_to_first_analysis: Option<Duration>,
    time_to_load_first_build_file: Option<Duration>,
    time_to_first_command_execution_start: Option<Duration>,
    time_to_first_test_discovery: Option<Duration>,
    system_info: SystemInfo,
    file_watcher_stats: Option<buck2_data::FileWatcherStats>,
    file_watcher_duration: Option<Duration>,
    time_to_last_action_execution_end: Option<Duration>,
    initial_sink_success_count: Option<u64>,
    initial_sink_failure_count: Option<u64>,
    initial_sink_dropped_count: Option<u64>,
    initial_sink_bytes_written: Option<u64>,
    sink_max_buffer_depth: u64,
    soft_error_categories: HashSet<String>,
    concurrent_command_blocking_duration: Option<Duration>,
    metadata: HashMap<String, String>,
    analysis_count: u64,
    daemon_in_memory_state_is_corrupted: bool,
    daemon_materializer_state_is_corrupted: bool,
    enable_restarter: bool,
    restarted_trace_id: Option<TraceId>,
    has_command_result: bool,
    has_end_of_stream: bool,
    compressed_event_log_size_bytes: Option<Arc<AtomicU64>>,
    critical_path_backend: Option<String>,
    instant_command_is_success: Option<bool>,
    bxl_ensure_artifacts_duration: Option<prost_types::Duration>,
    install_duration: Option<prost_types::Duration>,
    install_device_metadata: Vec<buck2_data::DeviceMetadata>,
    initial_re_upload_bytes: Option<u64>,
    initial_re_download_bytes: Option<u64>,
    initial_zdb_download_queries: Option<u64>,
    initial_zdb_download_bytes: Option<u64>,
    initial_zdb_upload_queries: Option<u64>,
    initial_zdb_upload_bytes: Option<u64>,
    initial_zgateway_download_queries: Option<u64>,
    initial_zgateway_download_bytes: Option<u64>,
    initial_zgateway_upload_queries: Option<u64>,
    initial_zgateway_upload_bytes: Option<u64>,
    initial_manifold_download_queries: Option<u64>,
    initial_manifold_download_bytes: Option<u64>,
    initial_manifold_upload_queries: Option<u64>,
    initial_manifold_upload_bytes: Option<u64>,
    initial_hedwig_download_queries: Option<u64>,
    initial_hedwig_download_bytes: Option<u64>,
    initial_hedwig_upload_queries: Option<u64>,
    initial_hedwig_upload_bytes: Option<u64>,
    concurrent_command_ids: HashSet<String>,
    daemon_connection_failure: bool,
    /// Daemon started by this command.
    daemon_was_started: Option<buck2_data::DaemonWasStartedReason>,
    client_metadata: Vec<buck2_data::ClientMetadata>,
    client_errors: Vec<buck2_error::Error>,
    command_errors: Vec<ErrorReport>,
    /// To append to gRPC errors.
    server_stderr: String,
    target_rule_type_names: Vec<String>,
    re_max_download_speeds: Vec<SlidingWindow>,
    re_max_upload_speeds: Vec<SlidingWindow>,
    re_avg_download_speed: NetworkSpeedAverage,
    re_avg_upload_speed: NetworkSpeedAverage,
    peak_process_memory_bytes: Option<u64>,
    has_new_buckconfigs: bool,
    buckconfig_diff_count: Option<u64>,
    buckconfig_diff_size: Option<u64>,
    peak_used_disk_space_bytes: Option<u64>,
    active_networks_kinds: HashSet<i32>,
    target_cfg: Option<TargetCfg>,
    version_control_revision: Option<buck2_data::VersionControlRevision>,
    concurrent_commands: bool,
}

struct ErrorsReport {
    errors: Vec<ProcessedErrorReport>,
    best_error_tag: Option<String>,
    best_error_category_key: Option<String>,
    error_category: Option<String>,
}

impl<'a> InvocationRecorder<'a> {
    pub fn new(
        fb: FacebookInit,
        async_cleanup_context: AsyncCleanupContext<'a>,
        write_to_path: Option<AbsPathBuf>,
        command_name: &'static str,
        sanitized_argv: Vec<String>,
        trace_id: TraceId,
        isolation_dir: String,
        build_count_manager: Option<BuildCountManager>,
        filesystem: String,
        restarted_trace_id: Option<TraceId>,
        log_size_counter_bytes: Option<Arc<AtomicU64>>,
        client_metadata: Vec<buck2_data::ClientMetadata>,
    ) -> Self {
        Self {
            fb,
            write_to_path,
            command_name,
            cli_args: sanitized_argv,
            isolation_dir,
            start_time: Instant::now(),
            async_cleanup_context,
            build_count_manager,
            trace_id,
            command_end: None,
            command_duration: None,
            re_session_id: None,
            re_experiment_name: None,
            critical_path_duration: None,
            tags: vec![],
            run_local_count: 0,
            run_remote_count: 0,
            run_action_cache_count: 0,
            run_remote_dep_file_cache_count: 0,
            run_skipped_count: 0,
            run_fallback_count: 0,
            local_actions_executed_via_worker: 0,
            first_snapshot: None,
            last_snapshot: None,
            min_attempted_build_count_since_rebase: 0,
            min_build_count_since_rebase: 0,
            cache_upload_count: 0,
            cache_upload_attempt_count: 0,
            dep_file_upload_count: 0,
            dep_file_upload_attempt_count: 0,
            parsed_target_patterns: None,
            filesystem,
            watchman_version: None,
            eden_version: None,
            test_info: None,
            eligible_for_full_hybrid: false,
            max_event_client_delay: None,
            max_malloc_bytes_active: None,
            max_malloc_bytes_allocated: None,
            run_command_failure_count: 0,
            event_count: 0,
            time_to_first_action_execution: None,
            materialization_output_size: 0,
            initial_materializer_entries_from_sqlite: None,
            time_to_command_start: None,
            time_to_command_critical_section: None,
            time_to_first_analysis: None,
            time_to_load_first_build_file: None,
            time_to_first_command_execution_start: None,
            time_to_first_test_discovery: None,
            system_info: SystemInfo::default(),
            file_watcher_stats: None,
            file_watcher_duration: None,
            time_to_last_action_execution_end: None,
            initial_sink_success_count: None,
            initial_sink_failure_count: None,
            initial_sink_dropped_count: None,
            initial_sink_bytes_written: None,
            sink_max_buffer_depth: 0,
            soft_error_categories: HashSet::new(),
            concurrent_command_blocking_duration: None,
            metadata: buck2_events::metadata::collect(),
            analysis_count: 0,
            daemon_in_memory_state_is_corrupted: false,
            daemon_materializer_state_is_corrupted: false,
            enable_restarter: false,
            restarted_trace_id,
            has_command_result: false,
            has_end_of_stream: false,
            compressed_event_log_size_bytes: log_size_counter_bytes,
            critical_path_backend: None,
            instant_command_is_success: None,
            bxl_ensure_artifacts_duration: None,
            install_duration: None,
            install_device_metadata: Vec::new(),
            initial_re_upload_bytes: None,
            initial_re_download_bytes: None,
            initial_zdb_download_queries: None,
            initial_zdb_download_bytes: None,
            initial_zdb_upload_queries: None,
            initial_zdb_upload_bytes: None,
            initial_zgateway_download_queries: None,
            initial_zgateway_download_bytes: None,
            initial_zgateway_upload_queries: None,
            initial_zgateway_upload_bytes: None,
            initial_manifold_download_queries: None,
            initial_manifold_download_bytes: None,
            initial_manifold_upload_queries: None,
            initial_manifold_upload_bytes: None,
            initial_hedwig_download_queries: None,
            initial_hedwig_download_bytes: None,
            initial_hedwig_upload_queries: None,
            initial_hedwig_upload_bytes: None,
            concurrent_command_ids: HashSet::new(),
            daemon_connection_failure: false,
            daemon_was_started: None,
            client_metadata,
            client_errors: Vec::new(),
            command_errors: Vec::new(),
            server_stderr: String::new(),
            target_rule_type_names: Vec::new(),
            re_max_download_speeds: vec![
                SlidingWindow::new(Duration::from_secs(1)),
                SlidingWindow::new(Duration::from_secs(5)),
                SlidingWindow::new(Duration::from_secs(10)),
            ],
            re_max_upload_speeds: vec![
                SlidingWindow::new(Duration::from_secs(1)),
                SlidingWindow::new(Duration::from_secs(5)),
                SlidingWindow::new(Duration::from_secs(10)),
            ],
            re_avg_download_speed: NetworkSpeedAverage::default(),
            re_avg_upload_speed: NetworkSpeedAverage::default(),
            peak_process_memory_bytes: None,
            has_new_buckconfigs: false,
            buckconfig_diff_count: None,
            buckconfig_diff_size: None,
            peak_used_disk_space_bytes: None,
            active_networks_kinds: HashSet::new(),
            target_cfg: None,
            version_control_revision: None,
            concurrent_commands: false,
        }
    }

    pub fn instant_command_outcome(&mut self, is_success: bool) {
        self.instant_command_is_success = Some(is_success);
    }

    async fn build_count(
        &mut self,
        is_success: bool,
        command_name: &str,
    ) -> anyhow::Result<Option<BuildCount>> {
        if let Some(stats) = &self.file_watcher_stats {
            if let Some(merge_base) = &stats.branched_from_revision {
                match &self.parsed_target_patterns {
                    None => {
                        if is_success {
                            return Err(anyhow::anyhow!(
                                "successful {} commands should have resolved target patterns",
                                command_name
                            ));
                        }
                        // fallthrough to 0 below
                    }
                    Some(v) => {
                        return if let Some(build_count) = &self.build_count_manager {
                            Some(
                                build_count
                                    .increment(merge_base, v, is_success)
                                    .await
                                    .context("Error recording build count"),
                            )
                            .transpose()
                        } else {
                            Ok(None)
                        };
                    }
                };
            }
        }

        Ok(Default::default())
    }

    fn finalize_errors(&mut self) -> ErrorsReport {
        // Add stderr to GRPC connection errors if available
        let connection_errors: Vec<buck2_error::Error> = self
            .client_errors
            .extract_if(|e| e.has_tag(ErrorTag::ClientGrpc))
            .collect();

        for error in connection_errors {
            let error = classify_server_stderr(error, &self.server_stderr);

            let error = if self.server_stderr.is_empty() {
                let error = error.context("buckd stderr is empty");
                // Likely buckd received SIGKILL, may be due to memory pressure
                if self.tags.iter().any(|s| s == MEMORY_PRESSURE_TAG) {
                    error
                        .context("memory pressure detected")
                        .tag([ErrorTag::ServerMemoryPressure])
                } else {
                    error
                }
            } else if error.has_tag(ErrorTag::ServerSigterm) {
                error.context("buckd killed by SIGTERM")
            } else {
                // Scribe sink truncates messages, but here we can do it better:
                // - truncate even if total message is not large enough
                // - truncate stderr, but keep the error message
                let server_stderr = truncate_stderr(&self.server_stderr);
                error.context(format!("buckd stderr:\n{}", server_stderr))
            };

            self.client_errors.push(error);
        }

        let mut errors =
            std::mem::take(&mut self.client_errors).into_map(|e| create_error_report(&e));
        let command_errors = std::mem::take(&mut self.command_errors);
        errors.extend(command_errors);

        let best_error = best_error(&errors);
        let best_error_category_key = best_error.map(|e| e.category_key.clone()).flatten();
        let best_tag = best_error.map(|e| e.best_tag()).flatten();
        let error_category = best_error.map(|error| error.category());

        let errors = errors.into_map(process_error_report);

        // `None` if no errors, `Some("UNCLASSIFIED")` if no tags.
        let best_error_tag = if errors.is_empty() {
            None
        } else {
            Some(
                best_tag
                    .map_or(
                        // If we don't have tags on the errors,
                        // we still want to add a tag to Scuba column.
                        ERROR_TAG_UNCLASSIFIED,
                        |t| t.as_str_name(),
                    )
                    .to_owned(),
            )
        };

        ErrorsReport {
            errors,
            best_error_tag,
            best_error_category_key,
            error_category,
        }
    }

    fn send_it(&mut self) -> Option<impl Future<Output = ()> + 'static + Send> {
        let mut sink_success_count = None;
        let mut sink_failure_count = None;
        let mut sink_dropped_count = None;
        let mut sink_bytes_written = None;
        let mut re_upload_bytes = None;
        let mut re_download_bytes = None;

        let mut zdb_download_queries = None;
        let mut zdb_download_bytes = None;
        let mut zdb_upload_queries = None;
        let mut zdb_upload_bytes = None;

        let mut zgateway_download_queries = None;
        let mut zgateway_download_bytes = None;
        let mut zgateway_upload_queries = None;
        let mut zgateway_upload_bytes = None;

        let mut manifold_download_queries = None;
        let mut manifold_download_bytes = None;
        let mut manifold_upload_queries = None;
        let mut manifold_upload_bytes = None;

        let mut hedwig_download_queries = None;
        let mut hedwig_download_bytes = None;
        let mut hedwig_upload_queries = None;
        let mut hedwig_upload_bytes = None;

        if let Some(snapshot) = &self.last_snapshot {
            sink_success_count =
                calculate_diff_if_some(&snapshot.sink_successes, &self.initial_sink_success_count);
            sink_failure_count =
                calculate_diff_if_some(&snapshot.sink_failures, &self.initial_sink_failure_count);
            sink_dropped_count =
                calculate_diff_if_some(&snapshot.sink_dropped, &self.initial_sink_dropped_count);
            sink_bytes_written = calculate_diff_if_some(
                &snapshot.sink_bytes_written,
                &self.initial_sink_bytes_written,
            );
            re_upload_bytes = calculate_diff_if_some(
                &Some(snapshot.re_upload_bytes),
                &self.initial_re_upload_bytes,
            );
            re_download_bytes = calculate_diff_if_some(
                &Some(snapshot.re_download_bytes),
                &self.initial_re_download_bytes,
            );
            zdb_download_queries = calculate_diff_if_some(
                &Some(snapshot.zdb_download_queries),
                &self.initial_zdb_download_queries,
            );
            zdb_download_bytes = calculate_diff_if_some(
                &Some(snapshot.zdb_download_bytes),
                &self.initial_zdb_download_bytes,
            );
            zdb_upload_queries = calculate_diff_if_some(
                &Some(snapshot.zdb_upload_queries),
                &self.initial_zdb_upload_queries,
            );
            zdb_upload_bytes = calculate_diff_if_some(
                &Some(snapshot.zdb_upload_bytes),
                &self.initial_zdb_upload_bytes,
            );
            zgateway_download_queries = calculate_diff_if_some(
                &Some(snapshot.zgateway_download_queries),
                &self.initial_zgateway_download_queries,
            );
            zgateway_download_bytes = calculate_diff_if_some(
                &Some(snapshot.zgateway_download_bytes),
                &self.initial_zgateway_download_bytes,
            );
            zgateway_upload_queries = calculate_diff_if_some(
                &Some(snapshot.zgateway_upload_queries),
                &self.initial_zgateway_upload_queries,
            );
            zgateway_upload_bytes = calculate_diff_if_some(
                &Some(snapshot.zgateway_upload_bytes),
                &self.initial_zgateway_upload_bytes,
            );
            manifold_download_queries = calculate_diff_if_some(
                &Some(snapshot.manifold_download_queries),
                &self.initial_manifold_download_queries,
            );
            manifold_download_bytes = calculate_diff_if_some(
                &Some(snapshot.manifold_download_bytes),
                &self.initial_manifold_download_bytes,
            );
            manifold_upload_queries = calculate_diff_if_some(
                &Some(snapshot.manifold_upload_queries),
                &self.initial_manifold_upload_queries,
            );
            manifold_upload_bytes = calculate_diff_if_some(
                &Some(snapshot.manifold_upload_bytes),
                &self.initial_manifold_upload_bytes,
            );
            hedwig_download_queries = calculate_diff_if_some(
                &Some(snapshot.hedwig_download_queries),
                &self.initial_hedwig_download_queries,
            );
            hedwig_download_bytes = calculate_diff_if_some(
                &Some(snapshot.hedwig_download_bytes),
                &self.initial_hedwig_download_bytes,
            );
            hedwig_upload_queries = calculate_diff_if_some(
                &Some(snapshot.hedwig_upload_queries),
                &self.initial_hedwig_upload_queries,
            );
            hedwig_upload_bytes = calculate_diff_if_some(
                &Some(snapshot.hedwig_upload_bytes),
                &self.initial_hedwig_upload_bytes,
            );

            // We show memory/disk warnings in the console but we can't emit a tag event there due to having no access to dispatcher.
            // Also, it suffices to only emit a single tag per invocation, not one tag each time memory pressure is exceeded.
            // Each snapshot already keeps track of the peak memory/disk usage, so we can use that to check if we ever reported a warning.
            if check_memory_pressure(Some(snapshot), &self.system_info).is_some() {
                self.tags.push(MEMORY_PRESSURE_TAG.to_owned());
            }
            if check_remaining_disk_space(Some(snapshot), &self.system_info).is_some() {
                self.tags.push("low_disk_space".to_owned());
            }
            if check_download_speed(
                &self.first_snapshot,
                self.last_snapshot.as_ref(),
                &self.system_info,
                self.re_avg_download_speed.avg_per_second(),
                self.concurrent_commands,
            ) {
                self.tags.push("slow_network_speed_ui_only".to_owned());
            }
            if is_vpn_enabled() {
                self.tags.push("vpn_enabled".to_owned());
            }
            if check_cache_misses(
                &ActionStats {
                    local_actions: self.run_local_count,
                    remote_actions: self.run_remote_count,
                    cached_actions: self.run_action_cache_count,
                    fallback_actions: self.run_fallback_count,
                    remote_dep_file_cached_actions: self.run_remote_dep_file_cache_count,
                },
                &self.system_info,
                self.min_build_count_since_rebase < 2,
                None,
            ) {
                self.tags.push("low_cache_hits".to_owned());
            }
        }

        let mut metadata = Self::default_metadata();
        metadata.strings.extend(std::mem::take(&mut self.metadata));

        let errors_report = self.finalize_errors();

        let record = buck2_data::InvocationRecord {
            command_name: Some(self.command_name.to_owned()),
            command_end: self.command_end.take(),
            command_duration: self.command_duration.take(),
            client_walltime: self.start_time.elapsed().try_into().ok(),
            re_session_id: self.re_session_id.take().unwrap_or_default(),
            re_experiment_name: self.re_experiment_name.take().unwrap_or_default(),
            cli_args: self.cli_args.clone(),
            critical_path_duration: self.critical_path_duration.and_then(|x| x.try_into().ok()),
            metadata: Some(metadata),
            tags: self.tags.drain(..).collect(),
            run_local_count: self.run_local_count,
            run_remote_count: self.run_remote_count,
            run_action_cache_count: self.run_action_cache_count,
            run_remote_dep_file_cache_count: self.run_remote_dep_file_cache_count,
            cache_hit_rate: total_cache_hit_rate(
                self.run_local_count,
                self.run_remote_count,
                self.run_action_cache_count,
                self.run_remote_dep_file_cache_count,
            ) as f32,
            run_skipped_count: self.run_skipped_count,
            run_fallback_count: Some(self.run_fallback_count),
            local_actions_executed_via_worker: Some(self.local_actions_executed_via_worker),
            first_snapshot: self.first_snapshot.take(),
            last_snapshot: self.last_snapshot.take(),
            min_attempted_build_count_since_rebase: self.min_attempted_build_count_since_rebase,
            min_build_count_since_rebase: self.min_build_count_since_rebase,
            cache_upload_count: self.cache_upload_count,
            cache_upload_attempt_count: self.cache_upload_attempt_count,
            dep_file_upload_count: self.dep_file_upload_count,
            dep_file_upload_attempt_count: self.dep_file_upload_attempt_count,
            parsed_target_patterns: self.parsed_target_patterns.take(),
            filesystem: std::mem::take(&mut self.filesystem),
            watchman_version: self.watchman_version.take(),
            eden_version: self.eden_version.take(),
            test_info: self.test_info.take(),
            eligible_for_full_hybrid: Some(self.eligible_for_full_hybrid),
            max_event_client_delay_ms: self
                .max_event_client_delay
                .and_then(|d| u64::try_from(d.as_millis()).ok()),
            max_malloc_bytes_active: self.max_malloc_bytes_active.take(),
            max_malloc_bytes_allocated: self.max_malloc_bytes_allocated.take(),
            run_command_failure_count: Some(self.run_command_failure_count),
            event_count: Some(self.event_count),
            time_to_first_action_execution_ms: self
                .time_to_first_action_execution
                .and_then(|d| u64::try_from(d.as_millis()).ok()),
            materialization_output_size: Some(self.materialization_output_size),
            initial_materializer_entries_from_sqlite: self.initial_materializer_entries_from_sqlite,
            time_to_command_start_ms: self
                .time_to_command_start
                .and_then(|d| u64::try_from(d.as_millis()).ok()),
            time_to_command_critical_section_ms: self
                .time_to_command_critical_section
                .and_then(|d| u64::try_from(d.as_millis()).ok()),
            time_to_first_analysis_ms: self
                .time_to_first_analysis
                .and_then(|d| u64::try_from(d.as_millis()).ok()),
            time_to_load_first_build_file_ms: self
                .time_to_load_first_build_file
                .and_then(|d| u64::try_from(d.as_millis()).ok()),
            time_to_first_command_execution_start_ms: self
                .time_to_first_command_execution_start
                .and_then(|d| u64::try_from(d.as_millis()).ok()),
            time_to_first_test_discovery_ms: self
                .time_to_first_test_discovery
                .and_then(|d| u64::try_from(d.as_millis()).ok()),
            system_total_memory_bytes: self.system_info.system_total_memory_bytes,
            file_watcher_stats: self.file_watcher_stats.take(),
            file_watcher_duration_ms: self
                .file_watcher_duration
                .and_then(|d| u64::try_from(d.as_millis()).ok()),
            time_to_last_action_execution_end_ms: self
                .time_to_last_action_execution_end
                .and_then(|d| u64::try_from(d.as_millis()).ok()),
            isolation_dir: Some(self.isolation_dir.clone()),
            sink_success_count,
            sink_failure_count,
            sink_dropped_count,
            sink_bytes_written,
            sink_max_buffer_depth: Some(self.sink_max_buffer_depth),
            soft_error_categories: std::mem::take(&mut self.soft_error_categories)
                .into_iter()
                .collect(),
            concurrent_command_blocking_duration: self
                .concurrent_command_blocking_duration
                .and_then(|x| x.try_into().ok()),
            analysis_count: Some(self.analysis_count),
            restarted_trace_id: self.restarted_trace_id.as_ref().map(|t| t.to_string()),
            has_command_result: Some(self.has_command_result),
            has_end_of_stream: Some(self.has_end_of_stream),
            // At this point we expect the event log writer to have finished
            compressed_event_log_size_bytes: Some(
                self.compressed_event_log_size_bytes
                    .as_ref()
                    .map(|x| x.load(Ordering::Relaxed))
                    .unwrap_or_default(),
            ),
            critical_path_backend: self.critical_path_backend.take(),
            instant_command_is_success: self.instant_command_is_success.take(),
            bxl_ensure_artifacts_duration: self.bxl_ensure_artifacts_duration.take(),
            re_upload_bytes,
            re_download_bytes,
            concurrent_command_ids: std::mem::take(&mut self.concurrent_command_ids)
                .into_iter()
                .collect(),
            daemon_connection_failure: Some(self.daemon_connection_failure),
            daemon_was_started: self.daemon_was_started.map(|t| t as i32),
            client_metadata: std::mem::take(&mut self.client_metadata),
            errors: errors_report.errors,
            best_error_tag: errors_report.best_error_tag,
            best_error_category_key: errors_report.best_error_category_key,
            error_category: errors_report.error_category,
            target_rule_type_names: std::mem::take(&mut self.target_rule_type_names),
            new_configs_used: Some(
                self.has_new_buckconfigs || self.buckconfig_diff_size.map_or(false, |s| s > 0),
            ),
            re_max_download_speed: self
                .re_max_download_speeds
                .iter()
                .map(|w| w.max_per_second().unwrap_or_default())
                .max(),
            re_max_upload_speed: self
                .re_max_upload_speeds
                .iter()
                .map(|w| w.max_per_second().unwrap_or_default())
                .max(),
            re_avg_download_speed: self.re_avg_download_speed.avg_per_second(),
            re_avg_upload_speed: self.re_avg_upload_speed.avg_per_second(),
            install_duration: self.install_duration.take(),
            install_device_metadata: self.install_device_metadata.drain(..).collect(),
            peak_process_memory_bytes: self.peak_process_memory_bytes.take(),
            buckconfig_diff_count: self.buckconfig_diff_count.take(),
            buckconfig_diff_size: self.buckconfig_diff_size.take(),
            event_log_manifold_ttl_s: manifold_event_log_ttl().ok().map(|t| t.as_secs()),
            total_disk_space_bytes: self.system_info.total_disk_space_bytes.take(),
            peak_used_disk_space_bytes: self.peak_used_disk_space_bytes.take(),
            zdb_download_queries,
            zdb_download_bytes,
            zdb_upload_queries,
            zdb_upload_bytes,
            zgateway_download_queries,
            zgateway_download_bytes,
            zgateway_upload_queries,
            zgateway_upload_bytes,
            manifold_download_queries,
            manifold_download_bytes,
            manifold_upload_queries,
            manifold_upload_bytes,
            hedwig_download_queries,
            hedwig_download_bytes,
            hedwig_upload_queries,
            hedwig_upload_bytes,
            active_networks_kinds: std::mem::take(&mut self.active_networks_kinds)
                .into_iter()
                .collect(),
            target_cfg: self.target_cfg.take(),
            version_control_revision: self.version_control_revision.take(),
        };

        let event = BuckEvent::new(
            SystemTime::now(),
            self.trace_id.dupe(),
            None,
            None,
            buck2_data::RecordEvent {
                data: Some((Box::new(record)).into()),
            }
            .into(),
        );

        if let Some(path) = &self.write_to_path {
            let res = (|| {
                let out = fs_util::create_file(path).context("Error opening")?;
                let mut out = std::io::BufWriter::new(out);
                serde_json::to_writer(&mut out, event.event()).context("Error writing")?;
                out.flush().context("Error flushing")?;
                anyhow::Ok(())
            })();

            if let Err(e) = &res {
                tracing::warn!(
                    "Failed to write InvocationRecord to `{}`: {:#}",
                    path.as_path().display(),
                    e
                );
            }
        }

        if let Ok(Some(scribe_sink)) =
            new_remote_event_sink_if_enabled(self.fb, 1, Duration::from_millis(500), 5, None)
        {
            tracing::info!("Recording invocation to Scribe: {:?}", &event);
            Some(async move {
                scribe_sink.send_now(event).await;
            })
        } else {
            tracing::info!("Invocation record is not sent to Scribe: {:?}", &event);
            None
        }
    }

    // Collects client-side state and data, suitable for telemetry.
    // NOTE: If data is visible from the daemon, put it in cli::metadata::collect()
    fn default_metadata() -> buck2_data::TypedMetadata {
        let mut ints = HashMap::new();
        ints.insert("is_tty".to_owned(), std::io::stderr().is_tty() as i64);
        buck2_data::TypedMetadata {
            ints,
            strings: HashMap::new(),
        }
    }

    // Store the "client" field in the metadata for telemetry
    pub fn update_metadata_from_client_metadata(&mut self, client_metadata: &[ClientMetadata]) {
        if let Some(client_id_from_client_metadata) = client_metadata
            .iter()
            .find(|m| m.key == "id")
            .map(|m| m.value.clone())
        {
            self.metadata.insert(
                "client".to_owned(),
                client_id_from_client_metadata.to_owned(),
            );
        }
    }

    fn handle_command_start(
        &mut self,
        command: &buck2_data::CommandStart,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        self.metadata.extend(command.metadata.clone());
        self.time_to_command_start = Some(self.start_time.elapsed());
        Ok(())
    }

    async fn handle_command_end(
        &mut self,
        command: &buck2_data::CommandEnd,
        event: &BuckEvent,
    ) -> anyhow::Result<()> {
        let mut command = command.clone();
        self.command_errors
            .extend(std::mem::take(&mut command.errors));

        // Awkwardly unpacks the SpanEnd event so we can read its duration.
        let command_end = match event.data() {
            buck2_data::buck_event::Data::SpanEnd(ref end) => end.clone(),
            _ => {
                return Err(anyhow::anyhow!(
                    "handle_command_end was passed a CommandEnd not contained in a SpanEndEvent"
                ));
            }
        };
        self.command_duration = command_end.duration;
        let command_data = command.data.as_ref().context("Missing command data")?;
        let build_count = match command_data {
            buck2_data::command_end::Data::Build(..)
            | buck2_data::command_end::Data::Test(..)
            | buck2_data::command_end::Data::Install(..) => {
                match self
                    .build_count(command.is_success, command_data.variant_name())
                    .await
                {
                    Ok(Some(build_count)) => build_count,
                    Ok(None) => Default::default(),
                    Err(e) => {
                        let _ignored = soft_error!("build_count_error", e.into());
                        Default::default()
                    }
                }
            }
            // other events don't count builds
            _ => Default::default(),
        };
        self.min_attempted_build_count_since_rebase = build_count.attempted_build_count;
        self.min_build_count_since_rebase = build_count.successful_build_count;

        self.command_end = Some(command);
        Ok(())
    }
    fn handle_command_critical_start(
        &mut self,
        command: &buck2_data::CommandCriticalStart,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        self.metadata.extend(command.metadata.clone());
        self.time_to_command_critical_section = Some(self.start_time.elapsed());
        Ok(())
    }
    fn handle_command_critical_end(
        &mut self,
        command: &buck2_data::CommandCriticalEnd,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        self.metadata.extend(command.metadata.clone());
        Ok(())
    }

    fn handle_action_execution_start(
        &mut self,
        _action: &buck2_data::ActionExecutionStart,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        if self.time_to_first_action_execution.is_none() {
            self.time_to_first_action_execution = Some(self.start_time.elapsed());
        }
        Ok(())
    }
    fn handle_action_execution_end(
        &mut self,
        action: &buck2_data::ActionExecutionEnd,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        if action.kind == buck2_data::ActionKind::Run as i32 {
            if action_stats::was_fallback_action(action) {
                self.run_fallback_count += 1;
            }

            match last_command_execution_kind::get_last_command_execution_kind(action) {
                LastCommandExecutionKind::Local => {
                    self.run_local_count += 1;
                }
                LastCommandExecutionKind::LocalWorker => {
                    self.run_local_count += 1;
                    self.local_actions_executed_via_worker += 1;
                }
                LastCommandExecutionKind::Cached => {
                    self.run_action_cache_count += 1;
                }
                LastCommandExecutionKind::RemoteDepFileCached => {
                    self.run_remote_dep_file_cache_count += 1;
                }
                LastCommandExecutionKind::Remote => {
                    self.run_remote_count += 1;
                }
                LastCommandExecutionKind::NoCommand => {
                    self.run_skipped_count += 1;
                }
            }
        }

        if action.eligible_for_full_hybrid.unwrap_or_default() {
            self.eligible_for_full_hybrid = true;
        }

        if action.commands.iter().any(|c| {
            matches!(
                c.status,
                Some(buck2_data::command_execution::Status::Failure(..))
            )
        }) {
            self.run_command_failure_count += 1;
        }

        self.time_to_last_action_execution_end = Some(self.start_time.elapsed());

        Ok(())
    }

    fn handle_analysis_start(
        &mut self,
        _analysis: &buck2_data::AnalysisStart,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        self.time_to_first_analysis
            .get_or_insert_with(|| self.start_time.elapsed());
        Ok(())
    }

    fn handle_load_start(
        &mut self,
        _eval: &buck2_data::LoadBuildFileStart,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        self.time_to_load_first_build_file
            .get_or_insert_with(|| self.start_time.elapsed());
        Ok(())
    }

    fn handle_executor_stage_start(
        &mut self,
        executor_stage: &buck2_data::ExecutorStageStart,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        match &executor_stage.stage {
            Some(buck2_data::executor_stage_start::Stage::Re(re_stage)) => match &re_stage.stage {
                Some(buck2_data::re_stage::Stage::Execute(_)) => {
                    self.time_to_first_command_execution_start
                        .get_or_insert_with(|| self.start_time.elapsed());
                }
                _ => {}
            },
            Some(buck2_data::executor_stage_start::Stage::Local(local_stage)) => {
                match &local_stage.stage {
                    Some(buck2_data::local_stage::Stage::Execute(_)) => {
                        self.time_to_first_command_execution_start
                            .get_or_insert_with(|| self.start_time.elapsed());
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_cache_upload_end(
        &mut self,
        cache_upload: &buck2_data::CacheUploadEnd,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        if cache_upload.success {
            self.cache_upload_count += 1;
        }
        self.cache_upload_attempt_count += 1;
        Ok(())
    }

    fn handle_dep_file_upload_end(
        &mut self,
        upload: &buck2_data::DepFileUploadEnd,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        if upload.success {
            self.dep_file_upload_count += 1;
        }
        self.dep_file_upload_attempt_count += 1;
        Ok(())
    }

    fn handle_re_session_created(
        &mut self,
        session: &buck2_data::RemoteExecutionSessionCreated,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        self.re_session_id = Some(session.session_id.clone());
        self.re_experiment_name = Some(session.experiment_name.clone());
        Ok(())
    }

    fn handle_materialization_end(
        &mut self,
        materialization: &buck2_data::MaterializationEnd,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        self.materialization_output_size += materialization.total_bytes;
        Ok(())
    }

    fn handle_materializer_state_info(
        &mut self,
        materializer_state_info: &buck2_data::MaterializerStateInfo,
    ) -> anyhow::Result<()> {
        self.initial_materializer_entries_from_sqlite =
            Some(materializer_state_info.num_entries_from_sqlite);
        Ok(())
    }

    fn handle_bxl_ensure_artifacts_end(
        &mut self,
        _bxl_ensure_artifacts_end: &buck2_data::BxlEnsureArtifactsEnd,
        event: &BuckEvent,
    ) -> anyhow::Result<()> {
        let bxl_ensure_artifacts_end = match event.data() {
            buck2_data::buck_event::Data::SpanEnd(ref end) => end.clone(),
            _ => {
                return Err(anyhow::anyhow!(
                    "handle_bxl_ensure_artifacts_end was passed a BxlEnsureArtifacts not contained in a SpanEndEvent"
                ));
            }
        };

        self.bxl_ensure_artifacts_duration = bxl_ensure_artifacts_end.duration;
        Ok(())
    }

    fn handle_install_finished(
        &mut self,
        install_finished: &buck2_data::InstallFinished,
    ) -> anyhow::Result<()> {
        self.install_duration = install_finished.duration.clone();
        self.install_device_metadata = install_finished.device_metadata.clone();
        Ok(())
    }

    fn handle_system_info(&mut self, system_info: &buck2_data::SystemInfo) -> anyhow::Result<()> {
        self.system_info = system_info.clone();
        Ok(())
    }

    fn handle_test_discovery(
        &mut self,
        test_info: &buck2_data::TestDiscovery,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        match &test_info.data {
            Some(buck2_data::test_discovery::Data::Session(session_info)) => {
                self.test_info = Some(session_info.info.clone());
            }
            Some(buck2_data::test_discovery::Data::Tests(..)) | None => {}
        }

        Ok(())
    }

    fn handle_test_discovery_start(
        &mut self,
        _test_discovery: &buck2_data::TestDiscoveryStart,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        self.time_to_first_test_discovery
            .get_or_insert_with(|| self.start_time.elapsed());
        Ok(())
    }

    fn handle_build_graph_info(
        &mut self,
        info: &buck2_data::BuildGraphExecutionInfo,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        let mut duration = Duration::default();

        for node in &info.critical_path {
            if let Some(d) = &node.duration {
                duration += d.try_into_duration()?;
            }
        }

        for node in &info.critical_path2 {
            if let Some(d) = &node.duration {
                duration += d.try_into_duration()?;
            }
        }

        self.critical_path_duration = Some(duration);
        self.critical_path_backend = info.backend_name.clone();
        Ok(())
    }

    fn handle_io_provider_info(
        &mut self,
        io_provider_info: &buck2_data::IoProviderInfo,
    ) -> anyhow::Result<()> {
        self.eden_version = io_provider_info.eden_version.to_owned();
        Ok(())
    }

    fn handle_tag(&mut self, tag: &buck2_data::TagEvent) -> anyhow::Result<()> {
        self.tags.extend(tag.tags.iter().cloned());
        Ok(())
    }

    fn handle_concurrent_commands(
        &mut self,
        concurrent_commands: &buck2_data::ConcurrentCommands,
    ) -> anyhow::Result<()> {
        concurrent_commands.trace_ids.iter().for_each(|c| {
            self.concurrent_command_ids.insert(c.clone());
        });
        self.concurrent_commands =
            self.concurrent_commands || concurrent_commands.trace_ids.len() > 1;
        Ok(())
    }

    fn handle_snapshot(
        &mut self,
        update: &buck2_data::Snapshot,
        event: &BuckEvent,
    ) -> anyhow::Result<()> {
        self.max_malloc_bytes_active =
            max(self.max_malloc_bytes_active, update.malloc_bytes_active);
        self.max_malloc_bytes_allocated = max(
            self.max_malloc_bytes_allocated,
            update.malloc_bytes_allocated,
        );
        if self.first_snapshot.is_none() {
            self.first_snapshot = Some(update.clone());
        } else {
            self.last_snapshot = Some(update.clone());
        }
        if self.initial_sink_success_count.is_none() {
            self.initial_sink_success_count = update.sink_successes;
        }
        if self.initial_sink_failure_count.is_none() {
            self.initial_sink_failure_count = update.sink_failures;
        }
        if self.initial_sink_dropped_count.is_none() {
            self.initial_sink_dropped_count = update.sink_dropped;
        }
        if self.initial_sink_bytes_written.is_none() {
            self.initial_sink_bytes_written = update.sink_bytes_written;
        }
        self.sink_max_buffer_depth = max(self.sink_max_buffer_depth, update.sink_buffer_depth());

        if self.initial_re_upload_bytes.is_none() {
            self.initial_re_upload_bytes = Some(update.re_upload_bytes);
        }
        if self.initial_re_download_bytes.is_none() {
            self.initial_re_download_bytes = Some(update.re_download_bytes);
        }

        if self.initial_zdb_download_queries.is_none() {
            self.initial_zdb_download_queries = Some(update.zdb_download_queries);
        }
        if self.initial_zdb_download_bytes.is_none() {
            self.initial_zdb_download_bytes = Some(update.zdb_download_bytes);
        }
        if self.initial_zdb_upload_queries.is_none() {
            self.initial_zdb_upload_queries = Some(update.zdb_upload_queries);
        }
        if self.initial_zdb_upload_bytes.is_none() {
            self.initial_zdb_upload_bytes = Some(update.zdb_upload_bytes);
        }

        if self.initial_zgateway_download_queries.is_none() {
            self.initial_zgateway_download_queries = Some(update.zgateway_download_queries);
        }
        if self.initial_zgateway_download_bytes.is_none() {
            self.initial_zgateway_download_bytes = Some(update.zgateway_download_bytes);
        }
        if self.initial_zgateway_upload_queries.is_none() {
            self.initial_zgateway_upload_queries = Some(update.zgateway_upload_queries);
        }
        if self.initial_zgateway_upload_bytes.is_none() {
            self.initial_zgateway_upload_bytes = Some(update.zgateway_upload_bytes);
        }

        if self.initial_manifold_download_queries.is_none() {
            self.initial_manifold_download_queries = Some(update.manifold_download_queries);
        }
        if self.initial_manifold_download_bytes.is_none() {
            self.initial_manifold_download_bytes = Some(update.manifold_download_bytes);
        }
        if self.initial_manifold_upload_queries.is_none() {
            self.initial_manifold_upload_queries = Some(update.manifold_upload_queries);
        }
        if self.initial_manifold_upload_bytes.is_none() {
            self.initial_manifold_upload_bytes = Some(update.manifold_upload_bytes);
        }

        if self.initial_hedwig_download_queries.is_none() {
            self.initial_hedwig_download_queries = Some(update.hedwig_download_queries);
        }
        if self.initial_hedwig_download_bytes.is_none() {
            self.initial_hedwig_download_bytes = Some(update.hedwig_download_bytes);
        }
        if self.initial_hedwig_upload_queries.is_none() {
            self.initial_hedwig_upload_queries = Some(update.hedwig_upload_queries);
        }
        if self.initial_hedwig_upload_bytes.is_none() {
            self.initial_hedwig_upload_bytes = Some(update.hedwig_upload_bytes);
        }

        for s in self.re_max_download_speeds.iter_mut() {
            s.update(event.timestamp(), update.re_download_bytes);
        }

        for s in self.re_max_upload_speeds.iter_mut() {
            s.update(event.timestamp(), update.re_upload_bytes);
        }

        self.re_avg_download_speed
            .update(event.timestamp(), update.re_download_bytes);

        self.re_avg_upload_speed
            .update(event.timestamp(), update.re_upload_bytes);

        self.peak_process_memory_bytes =
            max(self.peak_process_memory_bytes, process_memory(update));
        self.peak_used_disk_space_bytes =
            max(self.peak_process_memory_bytes, update.used_disk_space_bytes);

        for stat in update.network_interface_stats.values() {
            if stat.rx_bytes > 0 || stat.tx_bytes > 0 {
                self.active_networks_kinds.insert(stat.network_kind.into());
            }
        }

        Ok(())
    }

    fn handle_file_watcher_end(
        &mut self,
        file_watcher: &buck2_data::FileWatcherEnd,
        duration: Option<&prost_types::Duration>,
        _event: &BuckEvent,
    ) -> anyhow::Result<()> {
        // We might receive this event twice, so ... deal with it by merging the two.
        // See: https://fb.workplace.com/groups/buck2dev/permalink/3396726613948720/
        self.file_watcher_stats =
            merge_file_watcher_stats(self.file_watcher_stats.take(), file_watcher.stats.clone());
        if let Some(duration) = duration.cloned().and_then(|x| Duration::try_from(x).ok()) {
            *self.file_watcher_duration.get_or_insert_default() += duration;
        }
        if let Some(stats) = &file_watcher.stats {
            self.watchman_version = stats.watchman_version.to_owned();
        }
        Ok(())
    }

    fn handle_parsed_target_patterns(
        &mut self,
        patterns: &buck2_data::ParsedTargetPatterns,
    ) -> anyhow::Result<()> {
        self.parsed_target_patterns = Some(patterns.clone());
        Ok(())
    }

    fn handle_structured_error(&mut self, err: &buck2_data::StructuredError) -> anyhow::Result<()> {
        if let Some(soft_error_category) = err.soft_error_category.as_ref() {
            self.soft_error_categories
                .insert(soft_error_category.to_owned());

            if err.daemon_in_memory_state_is_corrupted {
                self.daemon_in_memory_state_is_corrupted = true;
            }

            if err.daemon_materializer_state_is_corrupted {
                self.daemon_materializer_state_is_corrupted = true;
            }
        }

        Ok(())
    }

    fn handle_dice_block_concurrent_command_end(
        &mut self,
        _command: &buck2_data::DiceBlockConcurrentCommandEnd,
        event: &BuckEvent,
    ) -> anyhow::Result<()> {
        let block_concurrent_command = match event.data() {
            buck2_data::buck_event::Data::SpanEnd(ref end) => end.clone(),
            _ => {
                return Err(anyhow::anyhow!(
                    "handle_dice_block_concurrent_command_end was passed a DiceBlockConcurrentCommandEnd not contained in a SpanEndEvent"
                ));
            }
        };

        let mut duration = self
            .concurrent_command_blocking_duration
            .unwrap_or_default();
        if let Some(d) = &block_concurrent_command.duration {
            duration += d.try_into_duration()?;
        }

        self.concurrent_command_blocking_duration = Some(duration);

        Ok(())
    }

    fn handle_dice_cleanup_end(
        &mut self,
        _command: &buck2_data::DiceCleanupEnd,
        event: &BuckEvent,
    ) -> anyhow::Result<()> {
        let dice_cleanup_end = match event.data() {
            buck2_data::buck_event::Data::SpanEnd(ref end) => end.clone(),
            _ => {
                return Err(anyhow::anyhow!(
                    "handle_dice_cleanup_end was passed a DiceCleanupEnd not contained in a SpanEndEvent"
                ));
            }
        };

        let mut duration = self
            .concurrent_command_blocking_duration
            .unwrap_or_default();
        if let Some(d) = &dice_cleanup_end.duration {
            duration += d.try_into_duration()?;
        }

        self.concurrent_command_blocking_duration = Some(duration);

        Ok(())
    }

    async fn handle_event(&mut self, event: &Arc<BuckEvent>) -> anyhow::Result<()> {
        // TODO(nga): query now once in `EventsCtx`.
        let now = SystemTime::now();
        if let Ok(delay) = now.duration_since(event.timestamp()) {
            self.max_event_client_delay =
                Some(max(self.max_event_client_delay.unwrap_or_default(), delay));
        }
        self.event_count += 1;

        match event.data() {
            buck2_data::buck_event::Data::SpanStart(ref start) => {
                match start.data.as_ref().context("Missing `start`")? {
                    buck2_data::span_start_event::Data::Command(command) => {
                        self.handle_command_start(command, event)
                    }
                    buck2_data::span_start_event::Data::CommandCritical(command) => {
                        self.handle_command_critical_start(command, event)
                    }
                    buck2_data::span_start_event::Data::ActionExecution(action) => {
                        self.handle_action_execution_start(action, event)
                    }
                    buck2_data::span_start_event::Data::Analysis(analysis) => {
                        self.handle_analysis_start(analysis, event)
                    }
                    buck2_data::span_start_event::Data::Load(eval) => {
                        self.handle_load_start(eval, event)
                    }
                    buck2_data::span_start_event::Data::ExecutorStage(stage) => {
                        self.handle_executor_stage_start(stage, event)
                    }
                    buck2_data::span_start_event::Data::TestDiscovery(test_discovery) => {
                        self.handle_test_discovery_start(test_discovery, event)
                    }
                    _ => Ok(()),
                }
            }
            buck2_data::buck_event::Data::SpanEnd(ref end) => {
                match end.data.as_ref().context("Missing `end`")? {
                    buck2_data::span_end_event::Data::Command(command) => {
                        self.handle_command_end(command, event).await
                    }
                    buck2_data::span_end_event::Data::CommandCritical(command) => {
                        self.handle_command_critical_end(command, event)
                    }
                    buck2_data::span_end_event::Data::ActionExecution(action) => {
                        self.handle_action_execution_end(action, event)
                    }
                    buck2_data::span_end_event::Data::FileWatcher(file_watcher) => {
                        self.handle_file_watcher_end(file_watcher, end.duration.as_ref(), event)
                    }
                    buck2_data::span_end_event::Data::CacheUpload(cache_upload) => {
                        self.handle_cache_upload_end(cache_upload, event)
                    }
                    buck2_data::span_end_event::Data::DepFileUpload(dep_file_upload) => {
                        self.handle_dep_file_upload_end(dep_file_upload, event)
                    }
                    buck2_data::span_end_event::Data::Materialization(materialization) => {
                        self.handle_materialization_end(materialization, event)
                    }
                    buck2_data::span_end_event::Data::Analysis(..) => {
                        self.analysis_count += 1;
                        Ok(())
                    }
                    buck2_data::span_end_event::Data::DiceBlockConcurrentCommand(
                        block_concurrent_command,
                    ) => self
                        .handle_dice_block_concurrent_command_end(block_concurrent_command, event),
                    buck2_data::span_end_event::Data::DiceCleanup(dice_cleanup_end) => {
                        self.handle_dice_cleanup_end(dice_cleanup_end, event)
                    }
                    buck2_data::span_end_event::Data::BxlEnsureArtifacts(_bxl_ensure_artifacts) => {
                        self.handle_bxl_ensure_artifacts_end(_bxl_ensure_artifacts, event)
                    }
                    _ => Ok(()),
                }
            }
            buck2_data::buck_event::Data::Instant(ref instant) => {
                match instant.data.as_ref().context("Missing `data`")? {
                    buck2_data::instant_event::Data::ReSession(session) => {
                        self.handle_re_session_created(session, event)
                    }
                    buck2_data::instant_event::Data::BuildGraphInfo(info) => {
                        self.handle_build_graph_info(info, event)
                    }
                    buck2_data::instant_event::Data::TestDiscovery(discovery) => {
                        self.handle_test_discovery(discovery, event)
                    }
                    buck2_data::instant_event::Data::Snapshot(result) => {
                        self.handle_snapshot(result, event)
                    }
                    buck2_data::instant_event::Data::TagEvent(tag) => self.handle_tag(tag),
                    buck2_data::instant_event::Data::IoProviderInfo(io_provider_info) => {
                        self.handle_io_provider_info(io_provider_info)
                    }
                    buck2_data::instant_event::Data::TargetPatterns(tag) => {
                        self.handle_parsed_target_patterns(tag)
                    }
                    buck2_data::instant_event::Data::MaterializerStateInfo(materializer_state) => {
                        self.handle_materializer_state_info(materializer_state)
                    }
                    buck2_data::instant_event::Data::StructuredError(err) => {
                        self.handle_structured_error(err)
                    }
                    buck2_data::instant_event::Data::RestartConfiguration(conf) => {
                        self.enable_restarter = conf.enable_restarter;
                        Ok(())
                    }
                    buck2_data::instant_event::Data::ConcurrentCommands(concurrent_commands) => {
                        self.handle_concurrent_commands(concurrent_commands)
                    }
                    buck2_data::instant_event::Data::CellConfigDiff(conf) => {
                        if conf.new_config_indicator_only {
                            self.has_new_buckconfigs = true;
                            return Ok(());
                        }
                        self.buckconfig_diff_count = Some(
                            self.buckconfig_diff_count.unwrap_or_default() + conf.config_diff_count,
                        );
                        self.buckconfig_diff_size = Some(
                            self.buckconfig_diff_size.unwrap_or_default() + conf.config_diff_size,
                        );
                        Ok(())
                    }
                    buck2_data::instant_event::Data::InstallFinished(install_finished) => {
                        self.handle_install_finished(install_finished)
                    }
                    buck2_data::instant_event::Data::SystemInfo(system_info) => {
                        self.handle_system_info(system_info)
                    }
                    buck2_data::instant_event::Data::TargetCfg(target_cfg) => {
                        self.target_cfg = Some(target_cfg.clone());
                        Ok(())
                    }
                    buck2_data::instant_event::Data::VersionControlRevision(revision) => {
                        self.version_control_revision = Some(revision.clone());
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }
            buck2_data::buck_event::Data::Record(_) => Ok(()),
        }
    }
}

fn process_error_report(error: buck2_data::ErrorReport) -> buck2_data::ProcessedErrorReport {
    let best_tag = best_tag(error.tags.iter().filter_map(|tag|
    // This should never fail, but it is safer to just ignore incorrect integers.
    ErrorTag::from_i32(*tag)))
    .map(|t| t.as_str_name())
    .unwrap_or(ERROR_TAG_UNCLASSIFIED);
    buck2_data::ProcessedErrorReport {
        tier: error.tier,
        message: error.message,
        telemetry_message: error.telemetry_message,
        source_location: error.source_location,
        tags: error
            .tags
            .iter()
            .copied()
            .filter_map(buck2_data::error::ErrorTag::from_i32)
            .map(|t| t.as_str_name().to_owned())
            .collect(),
        best_tag: Some(best_tag.to_owned()),
        sub_error_categories: error.sub_error_categories,
        category_key: error.category_key,
    }
}

impl<'a> Drop for InvocationRecorder<'a> {
    fn drop(&mut self) {
        if let Some(fut) = self.send_it() {
            self.async_cleanup_context
                .register("sending invocation to Scribe", fut.boxed());
        }
    }
}

#[async_trait]
impl<'a> EventSubscriber for InvocationRecorder<'a> {
    async fn handle_events(&mut self, events: &[Arc<BuckEvent>]) -> anyhow::Result<()> {
        for event in events {
            self.handle_event(event).await?;
        }
        Ok(())
    }

    async fn handle_console_interaction(
        &mut self,
        c: &Option<SuperConsoleToggle>,
    ) -> anyhow::Result<()> {
        match c {
            Some(c) => self
                .tags
                .push(format!("superconsole-toggle:{}", c.key()).to_owned()),
            None => {}
        }
        Ok(())
    }

    async fn handle_command_result(
        &mut self,
        result: &buck2_cli_proto::CommandResult,
    ) -> anyhow::Result<()> {
        self.has_command_result = true;
        match &result.result {
            Some(command_result::Result::BuildResponse(res)) => {
                let mut built_rule_type_names: Vec<String> = res
                    .build_targets
                    .iter()
                    .map(|t| {
                        t.target_rule_type_name
                            .clone()
                            .unwrap_or_else(|| "NULL".to_owned())
                    })
                    .unique_by(|x| x.clone())
                    .collect();
                built_rule_type_names.sort();
                self.target_rule_type_names = built_rule_type_names;
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_error(&mut self, error: &buck2_error::Error) -> anyhow::Result<()> {
        self.client_errors.push(error.clone());
        Ok(())
    }

    async fn handle_tailer_stderr(&mut self, stderr: &str) -> anyhow::Result<()> {
        if self.server_stderr.len() > 100_000 {
            // Proper truncation of the head is tricky, and for practical purposes
            // discarding the whole thing is fine.
            self.server_stderr.clear();
        }

        if !stderr.is_empty() {
            // We don't know yet whether we will need stderr or not,
            // so we capture it unconditionally.
            self.server_stderr.push_str(stderr);
            self.server_stderr.push('\n');
        }

        Ok(())
    }

    async fn exit(&mut self) -> anyhow::Result<()> {
        self.has_end_of_stream = true;
        Ok(())
    }

    fn as_error_observer(&self) -> Option<&dyn ErrorObserver> {
        Some(self)
    }

    fn handle_daemon_connection_failure(&mut self, error: &buck2_error::Error) {
        self.daemon_connection_failure = true;
        self.client_errors.push(error.clone());
    }

    fn handle_daemon_started(&mut self, daemon_was_started: buck2_data::DaemonWasStartedReason) {
        self.daemon_was_started = Some(daemon_was_started);
    }
}

impl<'a> ErrorObserver for InvocationRecorder<'a> {
    fn daemon_in_memory_state_is_corrupted(&self) -> bool {
        self.daemon_in_memory_state_is_corrupted
    }

    fn daemon_materializer_state_is_corrupted(&self) -> bool {
        self.daemon_materializer_state_is_corrupted
    }

    fn restarter_is_enabled(&self) -> bool {
        self.enable_restarter
    }
}

fn calculate_diff_if_some(a: &Option<u64>, b: &Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(av), Some(bv)) => Some(max(av, bv) - min(av, bv)),
        _ => None,
    }
}

fn merge_file_watcher_stats(
    a: Option<buck2_data::FileWatcherStats>,
    b: Option<buck2_data::FileWatcherStats>,
) -> Option<buck2_data::FileWatcherStats> {
    let (mut a, b) = match (a, b) {
        (Some(a), Some(b)) => (a, b),
        (a, None) => return a,
        (None, b) => return b,
    };

    a.fresh_instance = a.fresh_instance || b.fresh_instance;
    a.events_total += b.events_total;
    a.events_processed += b.events_processed;
    a.branched_from_revision = a.branched_from_revision.or(b.branched_from_revision);
    a.branched_from_global_rev = a.branched_from_global_rev.or(b.branched_from_global_rev);
    a.branched_from_revision_timestamp = a
        .branched_from_revision_timestamp
        .or(b.branched_from_revision_timestamp);
    a.events.extend(b.events);
    a.incomplete_events_reason = a.incomplete_events_reason.or(b.incomplete_events_reason);
    a.watchman_version = a.watchman_version.or(b.watchman_version);
    Some(a)
}

pub(crate) fn try_get_invocation_recorder<'a>(
    ctx: &ClientCommandContext<'a>,
    opts: &CommonEventLogOptions,
    command_name: &'static str,
    sanitized_argv: Vec<String>,
    log_size_counter_bytes: Option<Arc<AtomicU64>>,
) -> anyhow::Result<Box<InvocationRecorder<'a>>> {
    let write_to_path = opts
        .unstable_write_invocation_record
        .as_ref()
        .map(|path| path.resolve(&ctx.working_dir));

    let paths = ctx.maybe_paths()?;

    let filesystem;
    #[cfg(fbcode_build)]
    {
        let is_eden = paths.map_or(false, |paths| {
            let root = std::path::Path::to_owned(paths.project_root().root().to_buf().as_ref());
            detect_eden::is_eden(root).unwrap_or(false)
        });
        if is_eden {
            filesystem = "eden".to_owned();
        } else {
            filesystem = "default".to_owned();
        }
    }
    #[cfg(not(fbcode_build))]
    {
        filesystem = "default".to_owned();
    }

    let build_count = paths.map(|p| BuildCountManager::new(p.build_count_dir()));

    let recorder = InvocationRecorder::new(
        ctx.fbinit(),
        ctx.async_cleanup_context().dupe(),
        write_to_path,
        command_name,
        sanitized_argv,
        ctx.trace_id.dupe(),
        ctx.isolation.to_string(),
        build_count,
        filesystem,
        ctx.restarted_trace_id.dupe(),
        log_size_counter_bytes,
        ctx.client_metadata
            .iter()
            .map(ClientMetadata::to_proto)
            .collect(),
    );
    Ok(Box::new(recorder))
}

fn truncate_stderr(stderr: &str) -> &str {
    // If server crashed, it means something is very broken,
    // and we don't really need nicely formatted stderr.
    // We only need to see it once, fix it, and never see it again.
    let max_len = 20_000;
    let truncate_at = stderr.len().saturating_sub(max_len);
    let truncate_at = stderr.ceil_char_boundary(truncate_at);
    &stderr[truncate_at..]
}

#[cfg(test)]
mod tests {
    use crate::subscribers::recorder::truncate_stderr;

    #[test]
    fn test_truncate_stderr() {
        let mut stderr = String::new();
        stderr.push_str("prefix");
        stderr.push('Ъ'); // 2 bytes, so asking to truncate in the middle of the char.
        for _ in 0..19_999 {
            stderr.push('a');
        }
        let truncated = truncate_stderr(&stderr);
        assert_eq!(truncated.len(), 19_999);
    }
}
