use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::Duration,
};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_deep-swarm"))
}

fn run(args: &[&str]) -> Output {
    Command::new(binary()).args(args).output().unwrap()
}

fn write_scenario(directory: &Path, body: &str) -> PathBuf {
    let path = directory.join("scenario.yaml");
    fs::write(&path, body).unwrap();
    path
}

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn run_writes_three_reports_and_failed_case_is_nonzero() {
    let directory = tempfile::tempdir().unwrap();
    let fixtures = directory.path().join("fixtures");
    let reports = directory.path().join("reports");
    fs::create_dir(&fixtures).unwrap();
    let scenario = write_scenario(
        directory.path(),
        r#"
version: 1
suites:
  - name: smoke
    cases:
      - name: git
        steps:
          - id: status
            tool: git_status
            with: {}
            assertions:
              - {id: clean, type: exists, actual: "${steps.status.value.clean}"}
"#,
    );
    let output = run(&[
        "run",
        path(&scenario),
        "--fixtures",
        path(&fixtures),
        "--report-dir",
        path(&reports),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let files = fs::read_dir(&reports)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    for extension in ["json", "xml", "html"] {
        assert!(
            files
                .iter()
                .any(|file| file.extension().unwrap() == extension)
        );
    }
    let json_path = files
        .iter()
        .find(|file| file.extension().unwrap() == "json")
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&fs::read(json_path).unwrap()).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["status"], "succeeded");

    let failed = write_scenario(
        directory.path(),
        r#"
version: 1
suites:
  - name: smoke
    cases:
      - name: git
        steps:
          - id: status
            tool: git_status
            with: {}
            assertions:
              - {id: branch, type: contains, actual: "${steps.status.value.branch}", expected: absent}
"#,
    );
    let failed_reports = directory.path().join("failed-reports");
    let output = run(&[
        "run",
        path(&failed),
        "--fixtures",
        path(&fixtures),
        "--report-dir",
        path(&failed_reports),
    ]);
    assert!(!output.status.success());
    assert_eq!(fs::read_dir(failed_reports).unwrap().count(), 3);
}

#[test]
fn record_rejects_secrets_and_replay_rejects_a_bad_hash() {
    let directory = tempfile::tempdir().unwrap();
    let fixtures = directory.path().join("fixtures");
    fs::create_dir(&fixtures).unwrap();
    let sensitive = write_scenario(
        directory.path(),
        r#"
version: 1
suites:
  - name: record
    cases:
      - name: secret
        steps:
          - {id: create, tool: task_create, with: {name: "sk-12345678"}}
"#,
    );
    let unsafe_recording = directory.path().join("unsafe.json");
    let output = run(&[
        "record",
        path(&sensitive),
        path(&unsafe_recording),
        "--fixtures",
        path(&fixtures),
    ]);
    assert!(!output.status.success());
    assert!(!unsafe_recording.exists());

    let safe = write_scenario(
        directory.path(),
        r#"
version: 1
suites:
  - name: record
    cases:
      - name: safe
        steps:
          - {id: status, tool: git_status, with: {}}
"#,
    );
    let recording = directory.path().join("safe.json");
    let output = run(&[
        "record",
        path(&safe),
        path(&recording),
        "--fixtures",
        path(&fixtures),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = run(&[
        "replay",
        path(&recording),
        path(&safe),
        "--fixtures",
        path(&fixtures),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&recording).unwrap()).unwrap();
    value["events"][0]["event_hash"] = serde_json::Value::String("0".repeat(64));
    fs::write(&recording, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    let output = run(&[
        "replay",
        path(&recording),
        path(&safe),
        "--fixtures",
        path(&fixtures),
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ReplayMismatch"));
}

#[test]
fn mock_listens_on_loopback_and_requires_the_key() {
    let port = unused_port();
    let key = "integration-key";
    let mut child = Command::new(binary())
        .args(["mock", "--port", &port.to_string(), "--api-key", key])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let response = request_when_ready(&mut child, port, key);
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let response = request(port, None).unwrap();
    assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    child.kill().unwrap();
    child.wait().unwrap();
}

fn unused_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn request_when_ready(child: &mut Child, port: u16, key: &str) -> String {
    for _ in 0..100 {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("Mock 提前退出: {status}");
        }
        if let Ok(response) = request(port, Some(key)) {
            return response;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("Mock 未在回环端点启动");
}

fn request(port: u16, key: Option<&str>) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    write!(stream, "GET /models HTTP/1.1\r\nHost: 127.0.0.1\r\n")?;
    if let Some(key) = key {
        write!(stream, "Authorization: Bearer {key}\r\n")?;
    }
    write!(stream, "Connection: close\r\n\r\n")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}
