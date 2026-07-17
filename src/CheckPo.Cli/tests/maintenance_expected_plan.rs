use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

struct Harness {
    root: PathBuf,
    project: PathBuf,
    data: PathBuf,
    repo: PathBuf,
}

impl Harness {
    fn new(label: &str) -> Self {
        let unique = format!(
            "checkpo-cli-{label}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        );
        let root = std::env::temp_dir().join(unique);
        let project = root.join("UnityProject");
        let data = root.join("CheckPoData");
        fs::create_dir_all(project.join("Assets")).unwrap();
        fs::create_dir_all(project.join("Packages")).unwrap();
        fs::create_dir_all(project.join("ProjectSettings")).unwrap();
        fs::write(
            project.join("ProjectSettings/ProjectVersion.txt"),
            "m_EditorVersion: 2022.3.0f1\n",
        )
        .unwrap();

        let initial = run_cli(
            &data,
            [
                "--json".to_string(),
                "init".to_string(),
                path_text(&project),
            ],
        );
        assert_success(&initial, "project init");
        let view: Value = serde_json::from_slice(&initial.stdout).unwrap();
        let storage_root = PathBuf::from(view["storageRootPath"].as_str().unwrap());
        let project_id = view["projectId"].as_str().unwrap();
        let repo = storage_root.join("repos").join(project_id);

        Self {
            root,
            project,
            data,
            repo,
        }
    }

    fn json(&self, arguments: impl IntoIterator<Item = String>) -> Value {
        let output = run_cli(
            &self.data,
            std::iter::once("--json".to_string()).chain(arguments),
        );
        assert_success(&output, "JSON CLI operation");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn save_plan(&self, name: &str, plan: &Value) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, serde_json::to_vec_pretty(plan).unwrap()).unwrap();
        path
    }

    fn apply_gc(&self, plan: &Path) -> Output {
        run_cli(
            &self.data,
            [
                "storage".to_string(),
                "gc".to_string(),
                "apply".to_string(),
                path_text(&self.project),
                "--expected-plan".to_string(),
                path_text(plan),
                "--yes".to_string(),
            ],
        )
    }

    fn apply_temp_files(&self, plan: &Path) -> Output {
        run_cli(
            &self.data,
            [
                "maintenance".to_string(),
                "temp-files".to_string(),
                "apply".to_string(),
                path_text(&self.project),
                "--expected-plan".to_string(),
                path_text(plan),
                "--yes".to_string(),
            ],
        )
    }

    fn create_checkpoint(&self) {
        fs::write(self.project.join("Assets/Inventory.asset"), "inventory\n").unwrap();
        let output = run_cli(
            &self.data,
            [
                "checkpoint".to_string(),
                "create".to_string(),
                path_text(&self.project),
                "--name".to_string(),
                "inventory-change".to_string(),
            ],
        );
        assert_success(&output, "checkpoint create");
    }

    fn orphan_object(&self, hex: char, contents: &[u8]) -> PathBuf {
        let id = hex.to_string().repeat(64);
        let path = self.repo.join("objects/loose").join(&id[..2]).join(&id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    fn temp_file(&self, hex: char, contents: &[u8]) -> PathBuf {
        let name = format!(".checkpo-{}.tmp", hex.to_string().repeat(32));
        let path = self.project.join("Assets").join(name);
        fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn path_text(path: &Path) -> String {
    path.to_str().unwrap().to_string()
}

fn run_cli<I, S>(data: &Path, arguments: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_checkpo"))
        .args(arguments)
        .env("CHECKPO_DATA_DIR", data)
        .output()
        .unwrap()
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_plan_changed(output: &Output) {
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("changed after preview"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn analyze_gc(harness: &Harness) -> Value {
    harness.json([
        "storage".to_string(),
        "gc".to_string(),
        "analyze".to_string(),
        path_text(&harness.project),
    ])
}

fn analyze_temp_files(harness: &Harness) -> Value {
    harness.json([
        "maintenance".to_string(),
        "temp-files".to_string(),
        "analyze".to_string(),
        path_text(&harness.project),
    ])
}

#[test]
fn gc_cli_analyze_plan_can_be_applied_once() {
    let harness = Harness::new("gc-success");
    let object = harness.orphan_object('a', b"orphan");
    let plan = harness.save_plan("gc-plan.json", &analyze_gc(&harness));

    assert_success(&harness.apply_gc(&plan), "storage GC apply");
    assert!(!object.exists());
}

#[test]
fn gc_cli_apply_rejects_a_new_candidate_without_deleting_anything() {
    let harness = Harness::new("gc-new");
    let first = harness.orphan_object('a', b"first");
    let plan = harness.save_plan("gc-plan.json", &analyze_gc(&harness));
    let second = harness.orphan_object('b', b"second");

    assert_plan_changed(&harness.apply_gc(&plan));
    assert!(first.is_file());
    assert!(second.is_file());
}

#[test]
fn gc_cli_apply_rejects_a_same_size_replacement_without_deleting_it() {
    let harness = Harness::new("gc-replacement");
    let object = harness.orphan_object('a', b"first");
    let plan = harness.save_plan("gc-plan.json", &analyze_gc(&harness));
    fs::remove_file(&object).unwrap();
    fs::write(&object, b"other").unwrap();

    assert_plan_changed(&harness.apply_gc(&plan));
    assert_eq!(fs::read(&object).unwrap(), b"other");
}

#[test]
fn gc_cli_apply_rejects_an_inventory_head_change_without_deleting_candidates() {
    let harness = Harness::new("gc-inventory");
    let object = harness.orphan_object('a', b"orphan");
    let plan = harness.save_plan("gc-plan.json", &analyze_gc(&harness));
    harness.create_checkpoint();

    assert_plan_changed(&harness.apply_gc(&plan));
    assert!(object.is_file());
}

#[test]
fn temp_cli_analyze_plan_can_be_applied_once() {
    let harness = Harness::new("temp-success");
    let temporary = harness.temp_file('a', b"temporary");
    let plan = harness.save_plan("temp-plan.json", &analyze_temp_files(&harness));

    assert_success(
        &harness.apply_temp_files(&plan),
        "temporary file cleanup apply",
    );
    assert!(!temporary.exists());
}

#[test]
fn temp_cli_apply_rejects_a_new_candidate_without_deleting_anything() {
    let harness = Harness::new("temp-new");
    let first = harness.temp_file('a', b"first");
    let plan = harness.save_plan("temp-plan.json", &analyze_temp_files(&harness));
    let second = harness.temp_file('b', b"second");

    assert_plan_changed(&harness.apply_temp_files(&plan));
    assert!(first.is_file());
    assert!(second.is_file());
}

#[test]
fn temp_cli_apply_rejects_a_same_size_replacement_without_deleting_it() {
    let harness = Harness::new("temp-replacement");
    let temporary = harness.temp_file('a', b"first");
    let plan = harness.save_plan("temp-plan.json", &analyze_temp_files(&harness));
    fs::remove_file(&temporary).unwrap();
    fs::write(&temporary, b"other").unwrap();

    assert_plan_changed(&harness.apply_temp_files(&plan));
    assert_eq!(fs::read(&temporary).unwrap(), b"other");
}

#[test]
fn temp_cli_apply_rejects_an_inventory_head_change_without_deleting_candidates() {
    let harness = Harness::new("temp-inventory");
    let temporary = harness.temp_file('a', b"temporary");
    let plan = harness.save_plan("temp-plan.json", &analyze_temp_files(&harness));
    harness.create_checkpoint();

    assert_plan_changed(&harness.apply_temp_files(&plan));
    assert!(temporary.is_file());
}

#[test]
fn maintenance_cli_rejects_unknown_fields_and_unsupported_schema_before_deletion() {
    let harness = Harness::new("plan-schema");
    let temporary = harness.temp_file('a', b"temporary");
    let mut plan = analyze_temp_files(&harness);
    plan.as_object_mut()
        .unwrap()
        .insert("unexpected".to_string(), Value::Bool(true));
    let unknown = harness.save_plan("unknown-plan.json", &plan);
    let output = harness.apply_temp_files(&unknown);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown field"));
    assert!(temporary.is_file());

    plan.as_object_mut().unwrap().remove("unexpected");
    plan["schemaVersion"] = Value::from(999);
    let unsupported = harness.save_plan("unsupported-plan.json", &plan);
    let output = harness.apply_temp_files(&unsupported);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported"));
    assert!(temporary.is_file());
}
