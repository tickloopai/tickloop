//! A private Herdr 0.9 server and an attached client for tests: its own config, state, home and
//! socket, stopped when dropped. Tests never reach the user's own Herdr.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::{Value, json};
use uuid::Uuid;

pub const TIMEOUT: Duration = Duration::from_secs(20);
pub const PLUGIN_ID: &str = "tickloop.monitor";
pub const HERDR_CONFIG: &str = r#"onboarding = false
[terminal]
default_shell = "/bin/sh"
shell_mode = "non_login"
[update]
version_check = false
manifest_check = false
[ui.sound]
enabled = false
[ui.toast]
delivery = "off"
[ui.toast.clipboard]
enabled = false
[session]
resume_agents_on_restore = false
"#;
pub const HERDR_SESSION: &str = "p";

pub fn wait_until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub fn herdr_executable() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("herdr"))
        .find(|candidate| candidate.is_file())
        .expect("the popup tests run a private Herdr 0.9 server; put herdr on PATH")
}

/// A Herdr server of its own: private config, state, home and socket, stopped when dropped.
pub struct PrivateHerdr {
    pub root: tempfile::TempDir,
    pub server: Child,
}

impl PrivateHerdr {
    pub fn start() -> Self {
        let root = tempfile::Builder::new()
            .prefix("tmh")
            .tempdir_in(tickloop_agent_test_support::SHORT_TEMP_ROOT)
            .unwrap();
        std::fs::write(root.path().join("config.toml"), HERDR_CONFIG).unwrap();
        std::fs::create_dir(root.path().join("home")).unwrap();
        let log = std::fs::File::create(root.path().join("server.log")).unwrap();
        let server = Command::new(herdr_executable())
            .args(["--session", HERDR_SESSION, "server"])
            .env_clear()
            .envs(Self::environment(root.path()))
            .current_dir(root.path())
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let herdr = Self { root, server };
        wait_until("the private Herdr socket", || {
            std::fs::symlink_metadata(herdr.socket())
                .ok()
                .filter(|_| herdr.request("ping", json!({})).is_ok())
        });
        eprintln!(
            "private Herdr server pid {} socket {} ping {}",
            herdr.server.id(),
            herdr.socket().display(),
            herdr.request("ping", json!({})).unwrap()
        );
        herdr
    }

    pub fn environment(root: &Path) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("PATH", "/usr/bin:/bin".into()),
            ("HOME", root.join("home")),
            ("TERM", "xterm-256color".into()),
            ("SHELL", "/bin/sh".into()),
            ("XDG_CONFIG_HOME", root.to_path_buf()),
            ("XDG_STATE_HOME", root.join("state")),
            ("HERDR_CONFIG_PATH", root.join("config.toml")),
        ]
    }

    pub fn socket(&self) -> PathBuf {
        self.root
            .path()
            .join("herdr/sessions")
            .join(HERDR_SESSION)
            .join("herdr.sock")
    }

    /// One API request; `Err` carries Herdr's error body.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, Value> {
        let mut stream = std::os::unix::net::UnixStream::connect(self.socket())
            .map_err(|error| json!({"code": "connect", "message": error.to_string()}))?;
        let request = json!({"id": Uuid::new_v4().to_string(), "method": method, "params": params});
        stream.write_all(format!("{request}\n").as_bytes()).unwrap();
        let mut response = Vec::new();
        let mut byte = [0];
        while stream.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {
            response.push(byte[0]);
        }
        let response: Value = serde_json::from_slice(&response).unwrap();
        match response.get("error") {
            Some(error) => Err(error.clone()),
            None => Ok(response["result"].clone()),
        }
    }

    /// Creates a focused Workspace and returns its first Pane, where a View inside Herdr runs.
    pub fn create_workspace(&self) -> String {
        let created = self
            .request(
                "workspace.create",
                json!({"cwd": self.root.path(), "label": "tickloop-test", "focus": true}),
            )
            .unwrap();
        created["root_pane"]["pane_id"].as_str().unwrap().to_owned()
    }

    /// The plugins this server lists under Tickloop's plugin id.
    pub fn plugins(&self) -> Vec<Value> {
        self.plugins_with_id(PLUGIN_ID)
    }

    pub fn plugins_with_id(&self, id: &str) -> Vec<Value> {
        self.request("plugin.list", json!({"plugin_id": id}))
            .unwrap()["plugins"]
            .as_array()
            .unwrap()
            .clone()
    }

    /// Links a plugin Tickloop did not write under Tickloop's plugin id.
    pub fn link_foreign_plugin(&self) -> PathBuf {
        self.link_other_plugin(PLUGIN_ID)
    }

    /// Links a minimal plugin Tickloop did not write under `id` and returns its root.
    pub fn link_other_plugin(&self, id: &str) -> PathBuf {
        let directory = self.root.path().join(format!("plugin-{id}"));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("herdr-plugin.toml"),
            format!(
                "id = \"{id}\"\nname = \"Someone else\"\nversion = \"1.0.0\"\nmin_herdr_version = \"0.9.0\"\n\n[[panes]]\nid = \"attach\"\ntitle = \"Other\"\nplacement = \"popup\"\ncommand = [\"/bin/sh\", \"-c\", \"exit 0\"]\n"
            ),
        )
        .unwrap();
        let root = directory.canonicalize().unwrap();
        self.request("plugin.link", json!({"path": root})).unwrap();
        root
    }
}

impl Drop for PrivateHerdr {
    fn drop(&mut self) {
        let _ = self.request("server.stop", json!({}));
        let deadline = Instant::now() + Duration::from_secs(8);
        while self.server.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = self.server.kill();
                let _ = self.server.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        eprintln!("private Herdr server pid {} stopped", self.server.id());
    }
}

pub struct HerdrClient {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
}

pub const CLIENT_ROWS: u16 = 44;
pub const CLIENT_COLUMNS: u16 = 140;

impl HerdrClient {
    pub fn attach(herdr: &PrivateHerdr) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: CLIENT_ROWS,
                cols: CLIENT_COLUMNS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(herdr_executable());
        command.args(["--session", HERDR_SESSION]);
        command.env_clear();
        for (name, value) in PrivateHerdr::environment(herdr.root.path()) {
            command.env(name, value);
        }
        command.cwd(herdr.root.path());
        let child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let output = capture(pty.master.try_clone_reader().unwrap());
        Self {
            writer: pty.master.take_writer().unwrap(),
            child,
            output,
        }
    }

    pub fn screen(&self) -> String {
        let mut parser = vt100::Parser::new(CLIENT_ROWS, CLIENT_COLUMNS, 0);
        parser.process(&self.output.lock().unwrap());
        parser.screen().contents()
    }

    pub fn shows(&self, text: &str) -> String {
        wait_until(&format!("{text:?} on the Herdr client"), || {
            Some(self.screen()).filter(|screen| screen.contains(text))
        })
    }

    pub fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }
}

impl Drop for HerdrClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn capture(mut reader: Box<dyn Read + Send>) -> Arc<Mutex<Vec<u8>>> {
    let output = Arc::new(Mutex::new(Vec::new()));
    let captured = output.clone();
    std::thread::spawn(move || {
        let mut chunk = [0; 8192];
        while let Ok(count) = reader.read(&mut chunk) {
            if count == 0 {
                break;
            }
            captured.lock().unwrap().extend_from_slice(&chunk[..count]);
        }
    });
    output
}
