//! End-to-end tests over an in-memory LSP connection.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use serde_json::{Value, json};

use crate::util::path_to_uri;

struct Client {
    conn: Connection,
    server: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
    next_id: i32,
    /// Notifications received so far.
    notifications: Vec<Notification>,
}

impl Client {
    fn start(root: &Path, options: Value, pull: bool) -> Client {
        let (server_conn, client_conn) = Connection::memory();
        let server = std::thread::spawn(move || super::run(server_conn));
        let mut c = Client { conn: client_conn, server: Some(server), next_id: 0, notifications: Vec::new() };
        let mut caps = json!({
            "window": { "workDoneProgress": true },
            "workspace": { "configuration": true, "didChangeWatchedFiles": { "dynamicRegistration": true }, "workspaceFolders": true },
            "general": { "positionEncodings": ["utf-16"] },
        });
        if pull {
            caps["textDocument"] = json!({ "diagnostic": { "dynamicRegistration": false } });
        }
        let result = c.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": path_to_uri(root).as_str(),
                "capabilities": caps,
                "initializationOptions": options,
                "workspaceFolders": [{ "uri": path_to_uri(root).as_str(), "name": "test" }],
            }),
        );
        assert!(result["capabilities"]["documentSymbolProvider"].as_bool().unwrap());
        assert_eq!(result["capabilities"]["textDocumentSync"]["change"], 2);
        c.notify("initialized", json!({}));
        c
    }

    fn notify(&self, method: &str, params: Value) {
        self.conn.sender.send(Message::Notification(Notification::new(method.into(), params))).unwrap();
    }

    fn send_request(&mut self, method: &str, params: Value) -> RequestId {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.conn.sender.send(Message::Request(Request::new(id.clone(), method.into(), params))).unwrap();
        id
    }

    /// Pump messages, answering server→client requests, until `pred` returns a value.
    fn pump<T>(&mut self, timeout: Duration, mut pred: impl FnMut(&Message, &[Notification]) -> Option<T>) -> T {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = self.conn.receiver.recv_timeout(left).unwrap_or_else(|_| {
                let methods: Vec<&str> = self.notifications.iter().map(|n| n.method.as_str()).collect();
                panic!("timed out; notifications so far: {methods:?}")
            });
            if let Message::Request(req) = &msg {
                let result = match req.method.as_str() {
                    "workspace/configuration" => json!([null]),
                    _ => Value::Null,
                };
                self.conn.sender.send(Message::Response(Response::new_ok(req.id.clone(), result))).unwrap();
            }
            if let Message::Notification(n) = &msg {
                self.notifications.push(n.clone());
            }
            if let Some(v) = pred(&msg, &self.notifications) {
                return v;
            }
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send_request(method, params);
        self.pump(Duration::from_secs(60), |m, _| match m {
            Message::Response(r) if r.id == id => Some(r.response_result.clone().unwrap_or_else(|e| panic!("{method} failed: {}", e.message))),
            _ => None,
        })
    }

    fn wait_for_import(&mut self) {
        self.pump(Duration::from_secs(120), |m, _| match m {
            Message::Notification(n) if n.method == "$/progress" && n.params["value"]["kind"] == "end" => Some(()),
            _ => None,
        });
    }

    fn diagnostics_for(&mut self, path: &Path, version: i64) -> Vec<Value> {
        let uri = path_to_uri(path).as_str().to_string();
        self.pump(Duration::from_secs(60), |m, _| match m {
            Message::Notification(n) if n.method == "textDocument/publishDiagnostics" && n.params["uri"] == uri.as_str() && n.params["version"] == version => {
                Some(n.params["diagnostics"].as_array().unwrap().clone())
            }
            _ => None,
        })
    }

    fn shutdown(mut self) {
        let r = self.request("shutdown", Value::Null);
        assert!(r.is_null());
        self.notify("exit", Value::Null);
        self.server.take().unwrap().join().unwrap().unwrap();
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn stdlib() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".sdkman/candidates/kotlin/current/lib/kotlin-stdlib.jar")).filter(|p| p.is_file())
}

/// A Maven project whose only dependency (kotlin-stdlib) comes from a file:// repository.
fn maven_fixture(dir: &Path, stdlib: &Path) -> (PathBuf, Value) {
    let repo = dir.join("repo");
    let base = repo.join("org/jetbrains/kotlin/kotlin-stdlib/2.0.0");
    write(&base.join("kotlin-stdlib-2.0.0.pom"), "<project><modelVersion>4.0.0</modelVersion><groupId>org.jetbrains.kotlin</groupId><artifactId>kotlin-stdlib</artifactId><version>2.0.0</version></project>");
    std::fs::copy(stdlib, base.join("kotlin-stdlib-2.0.0.jar")).unwrap();
    let project = dir.join("project");
    write(
        &project.join("pom.xml"),
        r#"<project><modelVersion>4.0.0</modelVersion><groupId>com.example</groupId><artifactId>demo</artifactId><version>1.0</version>
        <dependencies><dependency><groupId>org.jetbrains.kotlin</groupId><artifactId>kotlin-stdlib</artifactId><version>2.0.0</version></dependency></dependencies>
        </project>"#,
    );
    write(&project.join("src/main/kotlin/demo/Model.kt"), "package demo\n\ndata class User(val name: String)\n\ninterface Repository<T> { fun all(): List<T> }\n");
    let options = json!({ "kotlin": {
        "cacheDir": dir.join("cache"),
        "project": { "offline": true, "extraRepositories": [format!("file://{}", repo.display())] },
        "diagnostics": { "debounceMs": 10 },
    }});
    (project, options)
}

#[test]
fn end_to_end_session() {
    let Some(stdlib) = stdlib() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (project, options) = maven_fixture(tmp.path(), &stdlib);
    let mut c = Client::start(&project, options, false);
    c.wait_for_import();

    // Open a file with one unresolved type and one syntax error.
    let app = project.join("src/main/kotlin/demo/App.kt");
    let text = "package demo\n\nclass App(private val repo: Repository<User>) {\n    fun users(): List<User> = repo.all()\n    fun broken(x: Missing) {}\n}\n";
    write(&app, text);
    c.notify("textDocument/didOpen", json!({ "textDocument": { "uri": path_to_uri(&app).as_str(), "languageId": "kotlin", "version": 1, "text": text } }));
    let diags = c.diagnostics_for(&app, 1);
    let messages: Vec<&str> = diags.iter().map(|d| d["message"].as_str().unwrap()).collect();
    assert_eq!(messages, vec!["Unresolved reference 'Missing'."], "{diags:?}");
    assert_eq!(diags[0]["range"]["start"], json!({ "line": 4, "character": 18 }));
    assert_eq!(diags[0]["code"], "UNRESOLVED_REFERENCE");

    // Document symbols.
    let syms = c.request("textDocument/documentSymbol", json!({ "textDocument": { "uri": path_to_uri(&app).as_str() } }));
    assert_eq!(syms[0]["name"], "App");
    let children: Vec<&str> = syms[0]["children"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(children, vec!["constructor", "repo", "users", "broken"]);
    assert_eq!(syms[0]["children"][2]["detail"], "fun users(): List<User>");

    // Workspace symbols (sources on disk + open file).
    let ws = c.request("workspace/symbol", json!({ "query": "Repo" }));
    let names: Vec<&str> = ws.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names.first(), Some(&"Repository"), "{names:?}");

    // Fix the error with an incremental edit: `Missing` -> `String`.
    c.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": path_to_uri(&app).as_str(), "version": 2 },
            "contentChanges": [{ "range": { "start": { "line": 4, "character": 18 }, "end": { "line": 4, "character": 25 } }, "text": "String" }],
        }),
    );
    assert!(c.diagnostics_for(&app, 2).is_empty());

    // Introduce a syntax error.
    c.notify(
        "textDocument/didChange",
        json!({ "textDocument": { "uri": path_to_uri(&app).as_str(), "version": 3 }, "contentChanges": [{ "text": "package demo\nfun f( {\n" }] }),
    );
    let d = c.diagnostics_for(&app, 3);
    assert!(d.iter().all(|d| d["code"] == "SYNTAX") && !d.is_empty(), "{d:?}");

    // The project model command.
    let model = c.request("workspace/executeCommand", json!({ "command": "kotlin.projectModel", "arguments": [] }));
    let module = &model["projects"][0]["modules"][0];
    assert_eq!(module["name"], "com.example:demo");
    assert_eq!(module["complete"], true);
    assert_eq!(module["dependencies"][0]["coordinate"], "org.jetbrains.kotlin:kotlin-stdlib:2.0.0");

    // Unknown requests get MethodNotFound, not a hang.
    let id = c.send_request("textDocument/unknownThing", json!({}));
    let err = c.pump(Duration::from_secs(10), |m, _| match m {
        Message::Response(r) if r.id == id => Some(r.response_result.clone().unwrap_err()),
        _ => None,
    });
    assert_eq!(err.code, lsp_server::ErrorCode::MethodNotFound as i32);

    // Closing clears diagnostics.
    c.notify("textDocument/didClose", json!({ "textDocument": { "uri": path_to_uri(&app).as_str() } }));
    let uri = path_to_uri(&app).as_str().to_string();
    let cleared = c.pump(Duration::from_secs(10), |m, _| match m {
        Message::Notification(n) if n.method == "textDocument/publishDiagnostics" && n.params["uri"] == uri.as_str() && n.params["version"].is_null() => {
            Some(n.params["diagnostics"].as_array().unwrap().len())
        }
        _ => None,
    });
    assert_eq!(cleared, 0);
    c.shutdown();
}

#[test]
fn pull_diagnostics_and_plain_folders() {
    // A folder without build files: syntax-only checks, symbols still work.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("plain");
    let file = root.join("src/Main.kt");
    write(&file, "fun main() {\n    println(\"hi\")\n}\nclass Foo { val x = }\n");
    let options = json!({ "kotlin": { "cacheDir": tmp.path().join("cache"), "project": { "offline": true } } });
    let mut c = Client::start(&root, options, true);
    c.wait_for_import();
    let text = std::fs::read_to_string(&file).unwrap();
    c.notify("textDocument/didOpen", json!({ "textDocument": { "uri": path_to_uri(&file).as_str(), "languageId": "kotlin", "version": 1, "text": text } }));
    let report = c.request("textDocument/diagnostic", json!({ "textDocument": { "uri": path_to_uri(&file).as_str() } }));
    assert_eq!(report["kind"], "full");
    let items = report["items"].as_array().unwrap();
    assert!(!items.is_empty() && items.iter().all(|d| d["code"] == "SYNTAX"), "{items:?}");
    let syms = c.request("textDocument/documentSymbol", json!({ "textDocument": { "uri": path_to_uri(&file).as_str() } }));
    assert_eq!(syms.as_array().unwrap().len(), 2);

    // Pull clients must never also get pushed diagnostics (they would show up twice).
    c.notify(
        "textDocument/didChange",
        json!({ "textDocument": { "uri": path_to_uri(&file).as_str(), "version": 2 }, "contentChanges": [{ "text": "fun f( {\n" }] }),
    );
    std::thread::sleep(Duration::from_millis(600));
    c.request("textDocument/documentSymbol", json!({ "textDocument": { "uri": path_to_uri(&file).as_str() } }));
    assert!(!c.notifications.iter().any(|n| n.method == "textDocument/publishDiagnostics"), "pushed diagnostics to a pull client");
    let report = c.request("textDocument/diagnostic", json!({ "textDocument": { "uri": path_to_uri(&file).as_str() } }));
    assert_eq!(report["items"].as_array().unwrap().len(), 1, "one error per line: {report}");
    c.shutdown();
}

#[test]
fn lint_diagnostics_and_code_actions() {
    let Some(stdlib) = stdlib() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (project, mut options) = maven_fixture(tmp.path(), &stdlib);
    options["kotlin"]["lint"] = json!({ "rules": { "CanBeVal": "off" } });
    let mut c = Client::start(&project, options, false);
    c.wait_for_import();

    let app = project.join("src/main/kotlin/demo/Lints.kt");
    let text = "package demo\n\nimport kotlin.math.max\n\nfun f(): Int {\n    var x = 1\n    return x;\n}\n";
    write(&app, text);
    let uri = path_to_uri(&app).as_str().to_string();
    c.notify("textDocument/didOpen", json!({ "textDocument": { "uri": uri, "languageId": "kotlin", "version": 1, "text": text } }));
    let diags = c.diagnostics_for(&app, 1);
    let codes: Vec<&str> = diags.iter().map(|d| d["code"].as_str().unwrap()).collect();
    assert_eq!(codes, vec!["UnusedImport", "RedundantSemicolon"], "{diags:?}");
    assert_eq!(diags[0]["severity"], 2);
    assert_eq!(diags[0]["tags"], json!([1]));

    // Quick fix + suppressions on the import line.
    let range = json!({ "start": { "line": 2, "character": 3 }, "end": { "line": 2, "character": 3 } });
    let actions = c.request("textDocument/codeAction", json!({ "textDocument": { "uri": uri }, "range": range, "context": { "diagnostics": [] } }));
    let titles: Vec<&str> = actions.as_array().unwrap().iter().map(|a| a["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["Remove unused import", "Suppress 'UnusedImport' for file", "Fix all unused and redundant code"], "{actions:#}");
    let edit = &actions[0]["edit"]["changes"][uri.as_str()][0];
    assert_eq!(edit["range"], json!({ "start": { "line": 2, "character": 0 }, "end": { "line": 3, "character": 0 } }));
    assert_eq!(edit["newText"], "");
    assert_eq!(actions[0]["kind"], "quickfix");

    // Source action on save: fix all.
    let all = c.request("textDocument/codeAction", json!({ "textDocument": { "uri": uri }, "range": range, "context": { "diagnostics": [], "only": ["source.fixAll"] } }));
    assert_eq!(all.as_array().unwrap().len(), 1);
    assert_eq!(all[0]["edit"]["changes"][uri.as_str()].as_array().unwrap().len(), 2);
    c.shutdown();
}
