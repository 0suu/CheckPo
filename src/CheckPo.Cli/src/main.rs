use checkpo_core as core;
use clap::{ArgGroup, Args, Parser, Subcommand};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(name = "checkpo")]
#[command(about = "Safe local checkpoints for Unity projects")]
struct Cli {
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init {
        project_path: PathBuf,
        #[arg(long)]
        start_as_separate: bool,
        #[arg(long)]
        yes: bool,
    },
    Status {
        project_path: PathBuf,
    },
    Checkpoint {
        #[command(subcommand)]
        command: CheckpointCommand,
    },
    Diff {
        project_path: PathBuf,
        #[arg(long)]
        checkpoint: String,
    },
    Restore {
        #[command(subcommand)]
        command: RestoreCommand,
    },
    Discard {
        #[command(subcommand)]
        command: DiscardCommand,
    },
    Verify {
        project_path: PathBuf,
        #[arg(long)]
        checkpoint: Option<String>,
        #[arg(long)]
        quick: bool,
    },
    Index {
        #[command(subcommand)]
        command: IndexCommand,
    },
    Storage {
        #[command(subcommand)]
        command: StorageCommand,
    },
    Transactions {
        #[command(subcommand)]
        command: TransactionsCommand,
    },
    Maintenance {
        #[command(subcommand)]
        command: MaintenanceCommand,
    },
}

#[derive(Debug, Subcommand)]
enum CheckpointCommand {
    Create {
        project_path: PathBuf,
        #[arg(long)]
        name: String,
        #[arg(long)]
        init_if_needed: bool,
        /// Include detailed checkpoint creation timings in the result.
        #[arg(long)]
        timings: bool,
    },
    List {
        project_path: PathBuf,
    },
    Delete {
        project_path: PathBuf,
        checkpoint_id: String,
        #[arg(long)]
        yes: bool,
    },
    Rename {
        project_path: PathBuf,
        checkpoint_id: String,
        #[arg(long)]
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum RestoreCommand {
    Preview {
        project_path: PathBuf,
        #[arg(long)]
        checkpoint: String,
    },
    Apply {
        project_path: PathBuf,
        #[arg(long)]
        checkpoint: String,
        #[arg(long)]
        expected_plan: PathBuf,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DiscardCommand {
    Preview(DiscardArgs),
    Apply(DiscardApplyArgs),
}

#[derive(Debug, Args)]
struct DiscardArgs {
    project_path: PathBuf,
    #[arg(long = "path", required = true)]
    paths: Vec<String>,
    #[arg(long)]
    checkpoint: Option<String>,
}

#[derive(Debug, Args)]
struct DiscardApplyArgs {
    project_path: PathBuf,
    #[arg(long = "path", required = true)]
    paths: Vec<String>,
    #[arg(long)]
    checkpoint: Option<String>,
    #[arg(long)]
    expected_plan: PathBuf,
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Subcommand)]
enum IndexCommand {
    Rebuild { project_path: PathBuf },
}

#[derive(Debug, Subcommand)]
enum StorageCommand {
    SetRoot {
        project_path: PathBuf,
        #[arg(long)]
        storage_root: PathBuf,
        #[arg(long)]
        yes: bool,
    },
    Gc {
        #[command(subcommand)]
        command: StorageGcCommand,
    },
}

#[derive(Debug, Subcommand)]
enum StorageGcCommand {
    Analyze {
        project_path: PathBuf,
    },
    Apply {
        project_path: PathBuf,
        #[arg(long)]
        expected_plan: PathBuf,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TransactionsCommand {
    List {
        project_path: PathBuf,
    },
    Recover {
        project_path: PathBuf,
    },
    Quarantine {
        project_path: PathBuf,
        transaction_id: String,
        #[arg(long)]
        yes: bool,
    },
    Conflicts {
        #[command(subcommand)]
        command: TransactionConflictsCommand,
    },
}

#[derive(Debug, Subcommand)]
enum TransactionConflictsCommand {
    Analyze {
        project_path: PathBuf,
        transaction_id: String,
    },
    Apply(TransactionConflictApplyArgs),
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("recovery_mode")
        .required(true)
        .args(["paths", "without_export"])
))]
struct TransactionConflictApplyArgs {
    project_path: PathBuf,
    transaction_id: String,
    #[arg(long)]
    expected_plan: PathBuf,
    #[arg(
        long = "path",
        requires = "export_root",
        conflicts_with = "without_export"
    )]
    paths: Vec<String>,
    #[arg(long, conflicts_with = "without_export")]
    export_root: Option<PathBuf>,
    #[arg(long, conflicts_with_all = ["paths", "export_root"])]
    without_export: bool,
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Subcommand)]
enum MaintenanceCommand {
    CleanupJournals {
        #[command(subcommand)]
        command: CleanupJournalsCommand,
    },
    TempFiles {
        #[command(subcommand)]
        command: TempFilesCommand,
    },
}

#[derive(Debug, Subcommand)]
enum CleanupJournalsCommand {
    Analyze {
        project_path: PathBuf,
    },
    Apply {
        project_path: PathBuf,
        #[arg(long)]
        expected_plan: PathBuf,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TempFilesCommand {
    Analyze {
        project_path: PathBuf,
    },
    Apply {
        project_path: PathBuf,
        #[arg(long)]
        expected_plan: PathBuf,
        #[arg(long)]
        yes: bool,
    },
}

fn main() -> ExitCode {
    let _diagnostics = match core::init_diagnostics() {
        Ok(guard) => Some(guard),
        Err(error) => {
            eprintln!("Warning: diagnostic logging is unavailable: {error}");
            None
        }
    };
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            core::log_operation_error("cli", &error);
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<u8, String> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init {
            project_path,
            start_as_separate,
            yes,
        } => {
            let value = if start_as_separate {
                core::start_as_separate_project(project_path, core::ApplyOptions { yes })
            } else {
                core::init_project(project_path)
            }
            .map_err(to_message)?;
            print_or_json(cli.json, &value, || {
                println!("Initialized: {}", value.project_root_path.display());
                println!("Storage: {}", value.storage_root_path.display());
            })?;
        }
        Command::Status { project_path } => {
            let mut context = core::load_project(&project_path).map_err(to_message)?;
            if context.location_status != core::ProjectLocationStatus::CopiedSuspected {
                core::recover_checkpoint_deletions(&project_path).map_err(to_message)?;
                context = core::load_project(&project_path).map_err(to_message)?;
            }
            let project = core::project_view(&context).map_err(to_message)?;
            let pending_transactions =
                core::pending_transactions(&project_path).map_err(to_message)?;
            let unresolved_quarantines =
                core::unresolved_transaction_quarantines(&project_path).map_err(to_message)?;
            let mut warnings = Vec::new();
            let mut checkpoint_index =
                core::checkpoint_index_status(&context).map_err(to_message)?;
            let checkpoints = if checkpoint_index.state == core::CheckpointIndexState::Current {
                match core::list_checkpoints_with_warnings_for_project(&context) {
                    Ok(result) => {
                        warnings.extend(result.warnings);
                        Some(result.checkpoints)
                    }
                    Err(core::CheckPoError::IndexUnavailable(detail)) => {
                        checkpoint_index = core::CheckpointIndexStatus {
                            state: core::CheckpointIndexState::Corrupt,
                            rebuildable: true,
                            detail: Some(detail),
                        };
                        None
                    }
                    Err(error) => return Err(to_message(error)),
                }
            } else {
                None
            };
            let storage = if checkpoint_index.state == core::CheckpointIndexState::Current {
                match core::storage_summary_from_index(&context) {
                    Ok(storage) => Some(storage),
                    Err(core::CheckPoError::IndexUnavailable(detail)) => {
                        checkpoint_index = core::CheckpointIndexStatus {
                            state: core::CheckpointIndexState::Corrupt,
                            rebuildable: true,
                            detail: Some(detail),
                        };
                        None
                    }
                    Err(error) => return Err(to_message(error)),
                }
            } else {
                None
            };
            let value = serde_json::json!({
                "project": project,
                "checkpointIndex": checkpoint_index,
                "checkpoints": checkpoints,
                "storage": storage,
                "pendingTransactions": pending_transactions,
                "unresolvedQuarantines": unresolved_quarantines,
                "warnings": warnings
            });
            print_or_json(cli.json, &value, || {
                println!("Project: {}", project.project_root_path.display());
                println!("Storage: {}", project.storage_root_path.display());
                match &checkpoints {
                    Some(checkpoints) => println!("Checkpoints: {}", checkpoints.len()),
                    None => println!("Checkpoints: unavailable ({:?})", checkpoint_index.state),
                }
                for warning in &project.warnings {
                    println!("Warning: {}", project_warning_text(warning));
                }
                for warning in &warnings {
                    println!("Warning: {warning}");
                }
                for quarantine in &unresolved_quarantines {
                    println!(
                        "Warning: unresolved quarantined transaction {}. Restore a known good checkpoint before making changes.",
                        quarantine.transaction_id
                    );
                }
            })?;
        }
        Command::Checkpoint { command } => match command {
            CheckpointCommand::Create {
                project_path,
                name,
                init_if_needed,
                timings,
            } => {
                let options = core::CreateCheckpointOptions {
                    init_if_needed,
                    ..Default::default()
                };
                if timings {
                    let result = core::create_checkpoint_profiled(project_path, &name, options)
                        .map_err(to_message)?;
                    print_or_json(cli.json, &result, || {
                        println!("Created checkpoint: {}", result.summary.checkpoint_id);
                        println!("Files: {}", result.summary.file_count);
                        print_checkpoint_create_metrics(&result.create_metrics);
                        for warning in &result.summary.warnings {
                            println!("Warning: {warning}");
                        }
                    })?;
                } else {
                    let summary = core::create_checkpoint(project_path, &name, options)
                        .map_err(to_message)?;
                    print_or_json(cli.json, &summary, || {
                        println!("Created checkpoint: {}", summary.checkpoint_id);
                        println!("Files: {}", summary.file_count);
                        for warning in &summary.warnings {
                            println!("Warning: {warning}");
                        }
                    })?;
                }
            }
            CheckpointCommand::List { project_path } => {
                let summaries = core::list_checkpoints(project_path).map_err(to_message)?;
                print_or_json(cli.json, &summaries, || {
                    for summary in &summaries {
                        println!(
                            "{}  {}  {} files  {}",
                            summary.checkpoint_id,
                            summary.created_at_utc,
                            summary.file_count,
                            summary.name
                        );
                    }
                })?;
            }
            CheckpointCommand::Delete {
                project_path,
                checkpoint_id,
                yes,
            } => {
                if !yes {
                    return Err("checkpoint delete requires --yes.".to_string());
                }
                let result =
                    core::delete_checkpoint(project_path, &checkpoint_id).map_err(to_message)?;
                print_or_json(cli.json, &result, || {
                    println!("Deleted checkpoint: {}", result.deleted_checkpoint_id);
                    for warning in &result.warnings {
                        println!("Warning: {warning}");
                    }
                })?;
            }
            CheckpointCommand::Rename {
                project_path,
                checkpoint_id,
                name,
            } => {
                let summary = core::rename_checkpoint(project_path, &checkpoint_id, &name)
                    .map_err(to_message)?;
                print_or_json(cli.json, &summary, || {
                    println!("Renamed checkpoint: {}", summary.checkpoint_id);
                    println!("Name: {}", summary.name);
                    for warning in &summary.warnings {
                        println!("Warning: {warning}");
                    }
                })?;
            }
        },
        Command::Diff {
            project_path,
            checkpoint,
        } => {
            let result = core::diff_checkpoint(project_path, &checkpoint).map_err(to_message)?;
            print_or_json(cli.json, &result, || print_diff(&result))?;
        }
        Command::Restore { command } => match command {
            RestoreCommand::Preview {
                project_path,
                checkpoint,
            } => {
                let plan = core::preview_restore(project_path, &checkpoint).map_err(to_message)?;
                print_or_json(cli.json, &plan, || print_plan(&plan))?;
            }
            RestoreCommand::Apply {
                project_path,
                checkpoint,
                expected_plan,
                yes,
            } => {
                if !yes {
                    return Err("restore apply requires --yes.".to_string());
                }
                let plan = read_operation_plan(&expected_plan)?;
                let result = core::apply_restore_plan(
                    project_path,
                    &checkpoint,
                    plan,
                    core::ApplyOptions { yes },
                )
                .map_err(to_message)?;
                print_or_json(cli.json, &result, || {
                    println!("Restore applied: {}", result.applied);
                    print_plan(&result.plan);
                    for warning in &result.warnings {
                        println!("Warning: {warning}");
                    }
                })?;
            }
        },
        Command::Discard { command } => match command {
            DiscardCommand::Preview(args) => {
                let plan = core::preview_discard_files(
                    args.project_path,
                    &args.paths,
                    args.checkpoint.as_deref(),
                )
                .map_err(to_message)?;
                print_or_json(cli.json, &plan, || print_plan(&plan))?;
            }
            DiscardCommand::Apply(args) => {
                if !args.yes {
                    return Err("discard apply requires --yes.".to_string());
                }
                let plan = read_operation_plan(&args.expected_plan)?;
                let result = core::apply_discard_files_plan(
                    args.project_path,
                    &args.paths,
                    args.checkpoint.as_deref(),
                    plan,
                    core::ApplyOptions { yes: args.yes },
                )
                .map_err(to_message)?;
                print_or_json(cli.json, &result, || {
                    println!("Discard applied: {}", result.applied);
                    print_plan(&result.plan);
                    for warning in &result.warnings {
                        println!("Warning: {warning}");
                    }
                })?;
            }
        },
        Command::Verify {
            project_path,
            checkpoint,
            quick,
        } => {
            let full = !quick;
            let result = match checkpoint {
                Some(checkpoint) => core::verify_checkpoint(project_path, &checkpoint, full),
                None => core::verify_project(project_path, full),
            }
            .map_err(to_message)?;
            let ok = result.is_valid;
            print_or_json(cli.json, &result, || {
                println!("{}", if result.is_valid { "OK" } else { "FAILED" });
                for warning in &result.warnings {
                    println!("Warning: {warning}");
                }
                for error in &result.errors {
                    println!("Error: {error}");
                }
            })?;
            return Ok(if ok { 0 } else { 1 });
        }
        Command::Index { command } => match command {
            IndexCommand::Rebuild { project_path } => {
                let result = core::rebuild_index(project_path).map_err(to_message)?;
                let ok = result.errors.is_empty();
                print_or_json(cli.json, &result, || {
                    println!("Rebuilt index.");
                    println!("Snapshots: {}", result.snapshot_count);
                    println!("Referenced objects: {}", result.referenced_object_count);
                    println!(
                        "Unavailable referenced objects: {}",
                        result.unavailable_referenced_object_count
                    );
                })?;
                return Ok(if ok { 0 } else { 1 });
            }
        },
        Command::Storage { command } => match command {
            StorageCommand::SetRoot {
                project_path,
                storage_root,
                yes,
            } => {
                if !yes {
                    return Err("storage set-root requires --yes.".to_string());
                }
                let value = core::set_project_storage_root(project_path, storage_root)
                    .map_err(to_message)?;
                print_or_json(cli.json, &value, || {
                    println!("Storage: {}", value.storage_root_path.display());
                })?;
            }
            StorageCommand::Gc { command } => match command {
                StorageGcCommand::Analyze { project_path } => {
                    let plan = core::analyze_gc(project_path).map_err(to_message)?;
                    print_or_json(cli.json, &plan, || {
                        println!("GC analysis.");
                        println!("Checkpoints: {}", plan.checkpoint_count);
                        println!("Objects: {}", plan.object_file_count);
                        println!("Referenced: {}", plan.referenced_blob_count);
                        println!("Unreferenced: {}", plan.unreferenced_blob_count);
                        println!(
                            "Unreferenced manifest chunks: {}",
                            plan.unreferenced_manifest_chunk_count
                        );
                        println!(
                            "Unreferenced inventory nodes: {}",
                            plan.unreferenced_inventory_node_count
                        );
                        println!(
                            "Reclaimable object bytes: {}",
                            plan.unreferenced_logical_bytes
                        );
                        println!(
                            "Reclaimable manifest bytes: {}",
                            plan.unreferenced_manifest_chunk_bytes
                        );
                        println!(
                            "Reclaimable inventory bytes: {}",
                            plan.unreferenced_inventory_node_bytes
                        );
                        println!(
                            "Reclaimable bytes: {}",
                            plan.unreferenced_logical_bytes
                                .saturating_add(plan.unreferenced_manifest_chunk_bytes)
                                .saturating_add(plan.unreferenced_inventory_node_bytes)
                        );
                        if plan.details_truncated {
                            let displayed = plan
                                .unreferenced_blobs
                                .len()
                                .saturating_add(plan.unreferenced_manifest_chunks.len())
                                .saturating_add(plan.unreferenced_inventory_nodes.len());
                            let total = plan
                                .unreferenced_blob_count
                                .saturating_add(plan.unreferenced_manifest_chunk_count)
                                .saturating_add(plan.unreferenced_inventory_node_count);
                            println!(
                                "Candidate details are truncated: {displayed} shown, {} omitted. Applying this plan ID deletes all {total} candidates.",
                                total.saturating_sub(displayed)
                            );
                        }
                    })?;
                    return Ok(if plan.has_integrity_problems { 1 } else { 0 });
                }
                StorageGcCommand::Apply {
                    project_path,
                    expected_plan,
                    yes,
                } => {
                    if !yes {
                        return Err("storage gc apply requires --yes.".to_string());
                    }
                    let plan = read_storage_gc_plan(&expected_plan)?;
                    let result = core::apply_gc_with_expected_plan(project_path, &plan.plan_id)
                        .map_err(to_message)?;
                    print_or_json(cli.json, &result, || {
                        println!("GC applied.");
                        println!("Deleted objects: {}", result.deleted_blob_count);
                        println!(
                            "Deleted manifest chunks: {}",
                            result.deleted_manifest_chunk_count
                        );
                        println!(
                            "Deleted inventory nodes: {}",
                            result.deleted_inventory_node_count
                        );
                        println!("Deleted bytes: {}", result.deleted_bytes);
                        if !result.completed {
                            println!(
                                "GC stopped after a partial apply. Remaining candidates: {}",
                                result.remaining_candidate_count
                            );
                            if let Some(candidate) = &result.failed_candidate {
                                println!("Failed candidate: {}", candidate.display());
                            }
                            if let Some(error) = &result.failure {
                                println!("Error: {error}");
                            }
                        }
                    })?;
                    return Ok(if result.completed { 0 } else { 1 });
                }
            },
        },
        Command::Transactions { command } => match command {
            TransactionsCommand::List { project_path } => {
                let result = core::pending_transactions(project_path).map_err(to_message)?;
                print_or_json(cli.json, &result, || {
                    for tx in &result {
                        println!(
                            "{}  {}  {}",
                            tx.transaction_id,
                            tx.state,
                            tx.journal_path.display()
                        );
                    }
                })?;
            }
            TransactionsCommand::Recover { project_path } => {
                let result = core::recover_transactions(project_path).map_err(to_message)?;
                let ok = result.failed_transaction_count == 0;
                print_or_json(cli.json, &result, || {
                    println!("Recovered: {}", result.recovered_transaction_count);
                    for transaction_id in &result.recovered_transaction_ids {
                        println!("  recovered {transaction_id}");
                    }
                    println!("Failed: {}", result.failed_transaction_count);
                    for failure in &result.failed_transactions {
                        println!("  failed {}", failure.transaction_id);
                        println!("    awaiting Unity close: {}", failure.awaiting_unity);
                        println!(
                            "    recovery conflicts: {}",
                            failure.recovery_conflict_count
                        );
                        println!("    error: {}", failure.error);
                    }
                })?;
                return Ok(if ok { 0 } else { 1 });
            }
            TransactionsCommand::Quarantine {
                project_path,
                transaction_id,
                yes,
            } => {
                if !yes {
                    return Err("transaction quarantine requires --yes.".to_string());
                }
                let result = core::quarantine_transaction(
                    project_path,
                    &transaction_id,
                    core::ApplyOptions { yes },
                )
                .map_err(to_message)?;
                print_or_json(cli.json, &result, || {
                    println!("Quarantined transaction: {}", result.transaction_id);
                    println!("Preserved at: {}", result.quarantine_path.display());
                    println!("Preserved bytes: {}", result.preserved_bytes);
                    for warning in &result.warnings {
                        println!("Warning: {warning}");
                    }
                })?;
            }
            TransactionsCommand::Conflicts { command } => match command {
                TransactionConflictsCommand::Analyze {
                    project_path,
                    transaction_id,
                } => {
                    let plan =
                        core::analyze_transaction_recovery_conflicts(project_path, &transaction_id)
                            .map_err(to_message)?;
                    print_or_json(cli.json, &plan, || {
                        println!("Transaction: {}", plan.transaction_id);
                        println!("Checkpoint: {}", plan.checkpoint_id);
                        println!("Plan ID: {}", plan.plan_id);
                        println!("Conflicts: {}", plan.conflicts.len());
                        for conflict in &plan.conflicts {
                            println!(
                                "  {}  {} bytes  metadata-only={}  {}",
                                conflict.path,
                                conflict.size_bytes,
                                conflict.metadata_only,
                                conflict.current_hash
                            );
                        }
                    })?;
                }
                TransactionConflictsCommand::Apply(args) => {
                    if !args.yes {
                        return Err(
                            "transaction conflict recovery apply requires --yes.".to_string()
                        );
                    }
                    let plan = read_recovery_conflict_plan(&args.expected_plan)?;
                    if plan.transaction_id != args.transaction_id {
                        return Err(format!(
                            "expected plan transaction {} does not match requested transaction {}",
                            plan.transaction_id, args.transaction_id
                        ));
                    }
                    let selected_paths = if args.without_export {
                        Vec::new()
                    } else {
                        core::parse_tracked_paths(&args.paths).map_err(to_message)?
                    };
                    let export_root = args.export_root.unwrap_or_default();
                    let result = core::recover_transaction_with_conflict_export(
                        args.project_path,
                        &args.transaction_id,
                        &plan.plan_id,
                        &selected_paths,
                        &export_root,
                        core::ApplyOptions { yes: args.yes },
                    )
                    .map_err(to_message)?;
                    print_or_json(cli.json, &result, || {
                        println!("Recovered transaction: {}", result.transaction_id);
                        if let Some(export_directory) = &result.export_directory {
                            println!("Export directory: {}", export_directory.display());
                        } else {
                            println!("Export directory: none");
                        }
                        println!("Exported paths: {}", result.exported_paths.len());
                        for path in &result.exported_paths {
                            println!("  exported {path}");
                        }
                        println!(
                            "Restored without external export: {}",
                            result.restored_without_export_count
                        );
                    })?;
                }
            },
        },
        Command::Maintenance { command } => match command {
            MaintenanceCommand::CleanupJournals { command } => match command {
                CleanupJournalsCommand::Analyze { project_path } => {
                    let plan =
                        core::analyze_transaction_cleanup(project_path).map_err(to_message)?;
                    print_or_json(cli.json, &plan, || {
                        println!("Completed journals: {}", plan.directory_count);
                        println!("Files: {}", plan.file_count);
                        println!("Bytes: {}", plan.total_bytes);
                    })?;
                }
                CleanupJournalsCommand::Apply {
                    project_path,
                    expected_plan,
                    yes,
                } => {
                    if !yes {
                        return Err("journal cleanup apply requires --yes.".to_string());
                    }
                    let plan = read_json_file::<core::TransactionCleanupPlan>(&expected_plan)?;
                    let result = core::cleanup_journals_with_expected_plan(
                        project_path,
                        &plan,
                        core::ApplyOptions { yes },
                    )
                    .map_err(to_message)?;
                    print_or_json(cli.json, &result, || {
                        println!("Deleted journals: {}", result.deleted_directory_count);
                        println!("Deleted bytes: {}", result.deleted_bytes);
                    })?;
                }
            },
            MaintenanceCommand::TempFiles { command } => match command {
                TempFilesCommand::Analyze { project_path } => {
                    let plan = core::analyze_orphan_temp_files(project_path).map_err(to_message)?;
                    print_or_json(cli.json, &plan, || {
                        println!("Temporary files: {}", plan.file_count);
                        println!("Temporary bytes: {}", plan.total_bytes);
                        for warning in &plan.warnings {
                            println!("Warning: {warning}");
                        }
                    })?;
                }
                TempFilesCommand::Apply {
                    project_path,
                    expected_plan,
                    yes,
                } => {
                    if !yes {
                        return Err("temporary file cleanup requires --yes.".to_string());
                    }
                    let plan = read_temp_file_cleanup_plan(&expected_plan)?;
                    let result = core::cleanup_orphan_temp_files_with_expected_plan(
                        project_path,
                        &plan.plan_id,
                        core::ApplyOptions { yes },
                    )
                    .map_err(to_message)?;
                    print_or_json(cli.json, &result, || {
                        println!("Deleted temporary files: {}", result.deleted_file_count);
                        println!("Deleted bytes: {}", result.deleted_bytes);
                        for warning in result.plan.warnings.iter().chain(result.warnings.iter()) {
                            println!("Warning: {warning}");
                        }
                    })?;
                }
            },
        },
    }
    Ok(0)
}

fn print_or_json<T: Serialize>(
    json: bool,
    value: &T,
    print_text: impl FnOnce(),
) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
        );
    } else {
        print_text();
    }
    Ok(())
}

fn print_diff(result: &core::DiffResult) {
    println!("Added: {}", result.added.len());
    println!("Modified: {}", result.modified.len());
    println!("Deleted: {}", result.deleted.len());
    println!("Unknown: {}", result.unknown.len());
    println!("Unchanged: {}", result.unchanged_count);
    println!("Complete: {}", result.complete);
    for warning in &result.warnings {
        println!("Warning: {warning}");
    }
}

fn print_plan(plan: &core::OperationPlan) {
    println!("Checkpoint: {}", plan.checkpoint_id);
    println!("Restore: {}", plan.restore_count);
    println!("Replace: {}", plan.replace_count);
    println!("Delete: {}", plan.delete_count);
    println!(
        "Estimated temporary bytes: {} (staged {}, backup {})",
        plan.estimated_temporary_bytes, plan.staged_bytes, plan.backup_bytes
    );
    for operation in &plan.operations {
        println!("{:?} {}", operation.operation_type, operation.path);
    }
    for warning in &plan.warnings {
        println!("Warning: {warning}");
    }
}

fn project_warning_text(warning: &core::ProjectWarning) -> String {
    if matches!(
        warning.kind,
        core::ProjectWarningKind::CopiedProjectSuspected | core::ProjectWarningKind::ProjectMoved
    ) {
        let title = if warning.previous_marker_has_same_project_id {
            "same project_id is registered at another path"
        } else {
            "registered project path changed"
        };
        return format!(
            "{}: previous={}, current={}",
            title,
            warning.previous_project_root_path.display(),
            warning.current_project_root_path.display()
        );
    }
    warning.message.clone()
}

fn read_operation_plan(path: &PathBuf) -> Result<core::OperationPlan, String> {
    read_json_file(path)
}

fn read_storage_gc_plan(path: &PathBuf) -> Result<core::StorageGcPlan, String> {
    let plan = read_json_file::<core::StorageGcPlan>(path)?;
    if plan.schema_version != core::STORAGE_GC_PLAN_SCHEMA_VERSION {
        return Err(format!(
            "unsupported storage GC plan schema version {}; expected {}",
            plan.schema_version,
            core::STORAGE_GC_PLAN_SCHEMA_VERSION
        ));
    }
    validate_maintenance_plan_id(&plan.plan_id)?;
    Ok(plan)
}

fn read_recovery_conflict_plan(
    path: &PathBuf,
) -> Result<core::TransactionRecoveryConflictPlan, String> {
    let plan = read_json_file::<core::TransactionRecoveryConflictPlan>(path)?;
    if plan.schema_version != core::TRANSACTION_RECOVERY_CONFLICT_PLAN_SCHEMA_VERSION {
        return Err(format!(
            "unsupported transaction recovery conflict plan schema version {}; expected {}",
            plan.schema_version,
            core::TRANSACTION_RECOVERY_CONFLICT_PLAN_SCHEMA_VERSION
        ));
    }
    validate_maintenance_plan_id(&plan.plan_id)?;
    if plan.transaction_id.len() != 32
        || !plan
            .transaction_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(
            "transaction recovery plan transactionId must be a 32 character lowercase hexadecimal value"
                .to_string(),
        );
    }
    Ok(plan)
}

fn read_temp_file_cleanup_plan(path: &PathBuf) -> Result<core::TempFileCleanupPlan, String> {
    let plan = read_json_file::<core::TempFileCleanupPlan>(path)?;
    if plan.schema_version != core::TEMP_FILE_CLEANUP_PLAN_SCHEMA_VERSION {
        return Err(format!(
            "unsupported temporary file cleanup plan schema version {}; expected {}",
            plan.schema_version,
            core::TEMP_FILE_CLEANUP_PLAN_SCHEMA_VERSION
        ));
    }
    validate_maintenance_plan_id(&plan.plan_id)?;
    Ok(plan)
}

fn validate_maintenance_plan_id(plan_id: &str) -> Result<(), String> {
    if plan_id.len() == 64
        && plan_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err("maintenance planId must be a 64 character lowercase hexadecimal value".to_string())
    }
}

fn read_json_file<T: serde::de::DeserializeOwned>(path: &PathBuf) -> Result<T, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("failed to read expected plan {}: {error}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("failed to parse expected plan {}: {error}", path.display()))
}

fn to_message(error: core::CheckPoError) -> String {
    error.to_string()
}

fn print_checkpoint_create_metrics(metrics: &core::CheckpointCreateMetrics) {
    let millis = |micros: u64| micros as f64 / 1_000.0;
    println!("Timing total: {:.3} ms", millis(metrics.total_micros));
    println!(
        "Timing phases: setup {:.3} ms, baseline load {:.3} ms, object preload/integrity validation {:.3} ms, object store {:.3} ms (parallelism {}), object cache {:.3} ms, durability barrier {:.3} ms, final object readback {:.3} ms, snapshot root/journal/inventory/ref commit {:.3} ms, snapshot index {:.3} ms, fingerprints {:.3} ms, unattributed {:.3} ms",
        millis(metrics.setup_micros),
        millis(metrics.baseline_load_micros),
        millis(metrics.object_preload_micros),
        millis(metrics.object_store_micros),
        metrics.object_store_parallelism,
        millis(metrics.object_integrity_cache_update_micros),
        millis(metrics.durability_barrier_micros),
        millis(metrics.object_readback_micros),
        millis(metrics.root_journal_ref_commit_micros),
        millis(metrics.snapshot_index_update_micros),
        millis(metrics.file_fingerprint_update_micros),
        millis(metrics.unattributed_micros),
    );
    println!(
        "Timing scan: {:.3} ms (enumerate {:.3}, fingerprint {:.3}, hash {:.3}, finalize {:.3}; hashed {} files / {} bytes, reused {} files / {} bytes)",
        millis(metrics.scan_total_micros),
        millis(metrics.scan.enumerate_micros),
        millis(metrics.scan.fingerprint_assessment_micros),
        millis(metrics.scan.hash_wall_micros),
        millis(metrics.scan.finalize_micros),
        metrics.scan.hashed_file_count,
        metrics.scan.hashed_bytes,
        metrics.scan.reused_file_count,
        metrics.scan.reused_bytes,
    );
    println!(
        "Timing manifest: build {:.3} ms, store {:.3} ms",
        millis(metrics.manifest_build_micros),
        millis(metrics.manifest_store_micros),
    );
    for (label, io) in [
        ("loose objects", &metrics.io.loose_objects),
        ("manifest chunks", &metrics.io.manifest_chunks),
        ("snapshot root", &metrics.io.snapshot_root),
    ] {
        println!(
            "Timing {label}: exists {:.3} ms, dir-prepare {:.3} ms, source-read {:.3} ms, hash {:.3} ms, write {:.3} ms, file-fsync {:.3} ms, publish {:.3} ms, dir-fsync {:.3} ms, existing-read+hash {:.3} ms, readback {:.3} ms; checked {}, existing {}, written {}, repaired {}, dirs-created {}, hash-ops {}, file-fsyncs {}, dir-fsyncs {}, readbacks {}",
            millis(io.existence_check_micros),
            millis(io.directory_prepare_micros),
            millis(io.source_read_micros),
            millis(io.hash_micros),
            millis(io.write_micros),
            millis(io.file_fsync_micros),
            millis(io.publish_micros),
            millis(io.directory_fsync_micros),
            millis(io.existing_validation_read_micros),
            millis(io.post_write_readback_micros),
            io.checked_count,
            io.existing_count,
            io.written_count,
            io.repaired_count,
            io.directory_create_count,
            io.hash_operation_count,
            io.file_fsync_count,
            io.directory_fsync_count,
            io.post_write_readback_count,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_start_as_separate_accepts_explicit_confirmation() {
        let cli =
            Cli::try_parse_from(["checkpo", "init", "project", "--start-as-separate", "--yes"])
                .unwrap();

        let Command::Init {
            start_as_separate,
            yes,
            ..
        } = cli.command
        else {
            panic!("expected init command");
        };
        assert!(start_as_separate);
        assert!(yes);
    }

    #[test]
    fn cleanup_journals_analyze_parses_without_confirmation() {
        let cli = Cli::try_parse_from([
            "checkpo",
            "maintenance",
            "cleanup-journals",
            "analyze",
            "project",
        ])
        .unwrap();

        let Command::Maintenance {
            command:
                MaintenanceCommand::CleanupJournals {
                    command: CleanupJournalsCommand::Analyze { .. },
                },
        } = cli.command
        else {
            panic!("expected cleanup-journals analyze command");
        };
    }

    #[test]
    fn cleanup_journals_apply_requires_expected_plan_and_confirmation() {
        let cli = Cli::try_parse_from([
            "checkpo",
            "maintenance",
            "cleanup-journals",
            "apply",
            "project",
            "--expected-plan",
            "cleanup-plan.json",
            "--yes",
        ])
        .unwrap();

        let Command::Maintenance {
            command:
                MaintenanceCommand::CleanupJournals {
                    command:
                        CleanupJournalsCommand::Apply {
                            expected_plan, yes, ..
                        },
                },
        } = cli.command
        else {
            panic!("expected cleanup-journals apply command");
        };
        assert_eq!(expected_plan, PathBuf::from("cleanup-plan.json"));
        assert!(yes);
    }

    #[test]
    fn cleanup_journals_rejects_the_old_one_shot_syntax() {
        assert!(Cli::try_parse_from([
            "checkpo",
            "maintenance",
            "cleanup-journals",
            "project",
            "--yes",
        ])
        .is_err());
    }

    #[test]
    fn storage_gc_apply_requires_expected_plan_and_confirmation() {
        let cli = Cli::try_parse_from([
            "checkpo",
            "storage",
            "gc",
            "apply",
            "project",
            "--expected-plan",
            "gc-plan.json",
            "--yes",
        ])
        .unwrap();

        let Command::Storage {
            command:
                StorageCommand::Gc {
                    command:
                        StorageGcCommand::Apply {
                            expected_plan, yes, ..
                        },
                },
        } = cli.command
        else {
            panic!("expected storage gc apply command");
        };
        assert_eq!(expected_plan, PathBuf::from("gc-plan.json"));
        assert!(yes);

        assert!(
            Cli::try_parse_from(["checkpo", "storage", "gc", "apply", "project", "--yes"]).is_err()
        );
    }

    #[test]
    fn temporary_file_apply_requires_expected_plan_and_rejects_cleanup_verb() {
        let cli = Cli::try_parse_from([
            "checkpo",
            "maintenance",
            "temp-files",
            "apply",
            "project",
            "--expected-plan",
            "temp-plan.json",
            "--yes",
        ])
        .unwrap();

        let Command::Maintenance {
            command:
                MaintenanceCommand::TempFiles {
                    command:
                        TempFilesCommand::Apply {
                            expected_plan, yes, ..
                        },
                },
        } = cli.command
        else {
            panic!("expected temporary file apply command");
        };
        assert_eq!(expected_plan, PathBuf::from("temp-plan.json"));
        assert!(yes);

        assert!(Cli::try_parse_from([
            "checkpo",
            "maintenance",
            "temp-files",
            "cleanup",
            "project",
            "--yes",
        ])
        .is_err());
    }

    #[test]
    fn maintenance_plan_id_requires_lowercase_blake3_shape() {
        assert!(validate_maintenance_plan_id(&"a".repeat(64)).is_ok());
        assert!(validate_maintenance_plan_id(&"A".repeat(64)).is_err());
        assert!(validate_maintenance_plan_id(&"a".repeat(63)).is_err());
        assert!(validate_maintenance_plan_id(&"g".repeat(64)).is_err());
    }

    #[test]
    fn transaction_conflict_analyze_parses_transaction_id() {
        let cli = Cli::try_parse_from([
            "checkpo",
            "transactions",
            "conflicts",
            "analyze",
            "project",
            "0123456789abcdef0123456789abcdef",
        ])
        .unwrap();

        let Command::Transactions {
            command:
                TransactionsCommand::Conflicts {
                    command: TransactionConflictsCommand::Analyze { transaction_id, .. },
                },
        } = cli.command
        else {
            panic!("expected transaction conflicts analyze command");
        };
        assert_eq!(transaction_id, "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn transaction_conflict_apply_supports_export_or_explicit_no_export() {
        let exported = Cli::try_parse_from([
            "checkpo",
            "transactions",
            "conflicts",
            "apply",
            "project",
            "0123456789abcdef0123456789abcdef",
            "--expected-plan",
            "conflict-plan.json",
            "--path",
            "Assets/Foo.prefab",
            "--export-root",
            "exported",
            "--yes",
        ])
        .unwrap();
        let Command::Transactions {
            command:
                TransactionsCommand::Conflicts {
                    command: TransactionConflictsCommand::Apply(exported),
                },
        } = exported.command
        else {
            panic!("expected transaction conflicts apply command");
        };
        assert_eq!(exported.paths, vec!["Assets/Foo.prefab"]);
        assert_eq!(exported.export_root, Some(PathBuf::from("exported")));
        assert!(!exported.without_export);
        assert!(exported.yes);

        let no_export = Cli::try_parse_from([
            "checkpo",
            "transactions",
            "conflicts",
            "apply",
            "project",
            "0123456789abcdef0123456789abcdef",
            "--expected-plan",
            "conflict-plan.json",
            "--without-export",
            "--yes",
        ])
        .unwrap();
        let Command::Transactions {
            command:
                TransactionsCommand::Conflicts {
                    command: TransactionConflictsCommand::Apply(no_export),
                },
        } = no_export.command
        else {
            panic!("expected transaction conflicts apply command");
        };
        assert!(no_export.paths.is_empty());
        assert!(no_export.export_root.is_none());
        assert!(no_export.without_export);

        assert!(Cli::try_parse_from([
            "checkpo",
            "transactions",
            "conflicts",
            "apply",
            "project",
            "0123456789abcdef0123456789abcdef",
            "--expected-plan",
            "conflict-plan.json",
            "--yes",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "checkpo",
            "transactions",
            "conflicts",
            "apply",
            "project",
            "0123456789abcdef0123456789abcdef",
            "--expected-plan",
            "conflict-plan.json",
            "--path",
            "Assets/Foo.prefab",
            "--without-export",
            "--yes",
        ])
        .is_err());
    }

    #[test]
    fn transaction_quarantine_accepts_id_and_explicit_confirmation() {
        let cli = Cli::try_parse_from([
            "checkpo",
            "transactions",
            "quarantine",
            "project",
            "0123456789abcdef0123456789abcdef",
            "--yes",
        ])
        .unwrap();

        let Command::Transactions {
            command:
                TransactionsCommand::Quarantine {
                    transaction_id,
                    yes,
                    ..
                },
        } = cli.command
        else {
            panic!("expected transaction quarantine command");
        };
        assert_eq!(transaction_id, "0123456789abcdef0123456789abcdef");
        assert!(yes);
    }
}
