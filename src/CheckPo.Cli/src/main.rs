use checkpo_core as core;
use clap::{Args, Parser, Subcommand};
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
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TransactionsCommand {
    List { project_path: PathBuf },
    Recover { project_path: PathBuf },
}

#[derive(Debug, Subcommand)]
enum MaintenanceCommand {
    CleanupJournals { project_path: PathBuf },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
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
        } => {
            let value = if start_as_separate {
                core::start_as_separate_project(project_path)
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
            let context = core::load_project(&project_path).map_err(to_message)?;
            let project = core::project_view(&context).map_err(to_message)?;
            let pending_transactions =
                core::pending_transactions_for_project(&context).map_err(to_message)?;
            let mut warnings = Vec::new();
            let checkpoints = match core::list_checkpoints_for_project(&context) {
                Ok(checkpoints) => checkpoints,
                Err(error)
                    if context.location_status == core::ProjectLocationStatus::CopiedSuspected
                        || !pending_transactions.is_empty() =>
                {
                    warnings.push(format!("Failed to load checkpoints: {error}"));
                    Vec::new()
                }
                Err(error) => return Err(to_message(error)),
            };
            let storage = match core::storage_summary_from_index(&context) {
                Ok(storage) => Some(storage),
                Err(error)
                    if context.location_status == core::ProjectLocationStatus::CopiedSuspected
                        || !pending_transactions.is_empty() =>
                {
                    warnings.push(format!("Failed to load storage summary: {error}"));
                    None
                }
                Err(error) => return Err(to_message(error)),
            };
            let value = serde_json::json!({
                "project": project,
                "checkpoints": checkpoints,
                "storage": storage,
                "warnings": warnings
            });
            print_or_json(cli.json, &value, || {
                println!("Project: {}", project.project_root_path.display());
                println!("Storage: {}", project.storage_root_path.display());
                println!("Checkpoints: {}", checkpoints.len());
                for warning in &project.warnings {
                    println!("Warning: {}", project_warning_text(warning));
                }
                for warning in &warnings {
                    println!("Warning: {warning}");
                }
            })?;
        }
        Command::Checkpoint { command } => match command {
            CheckpointCommand::Create {
                project_path,
                name,
                init_if_needed,
            } => {
                let summary = core::create_checkpoint(
                    project_path,
                    &name,
                    core::CreateCheckpointOptions {
                        init_if_needed,
                        ..Default::default()
                    },
                )
                .map_err(to_message)?;
                print_or_json(cli.json, &summary, || {
                    println!("Created checkpoint: {}", summary.checkpoint_id);
                    println!("Files: {}", summary.file_count);
                    for warning in &summary.warnings {
                        println!("Warning: {warning}");
                    }
                })?;
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
                    println!("Objects: {}", result.object_count);
                    println!("Missing objects: {}", result.missing_object_count);
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
                        println!("Reclaimable bytes: {}", plan.unreferenced_logical_bytes);
                    })?;
                    return Ok(if plan.has_integrity_problems { 1 } else { 0 });
                }
                StorageGcCommand::Apply { project_path, yes } => {
                    if !yes {
                        return Err("storage gc apply requires --yes.".to_string());
                    }
                    let result = core::apply_gc(project_path).map_err(to_message)?;
                    print_or_json(cli.json, &result, || {
                        println!("GC applied.");
                        println!("Deleted objects: {}", result.deleted_blob_count);
                        println!("Deleted bytes: {}", result.deleted_bytes);
                    })?;
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
                    println!("Failed: {}", result.failed_transaction_count);
                })?;
                return Ok(if ok { 0 } else { 1 });
            }
        },
        Command::Maintenance { command } => match command {
            MaintenanceCommand::CleanupJournals { project_path } => {
                let result = core::cleanup_journals(project_path).map_err(to_message)?;
                print_or_json(cli.json, &result, || {
                    println!("Deleted journals: {}", result.deleted_directory_count);
                    println!("Deleted bytes: {}", result.deleted_bytes);
                })?;
            }
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
    println!("Unchanged: {}", result.unchanged_count);
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
    let text = fs::read_to_string(path)
        .map_err(|error| format!("failed to read expected plan {}: {error}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("failed to parse expected plan {}: {error}", path.display()))
}

fn to_message(error: core::CheckPoError) -> String {
    error.to_string()
}
