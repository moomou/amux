//! Runtime (sockets) and state (persistence) locations.

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Paths {
    pub name: String,
    pub run: PathBuf,
    pub state: PathBuf,
}

impl Paths {
    pub fn new(name: &str) -> Paths {
        let run = match std::env::var("AMUX_RUNTIME_DIR") {
            Ok(d) if !d.is_empty() => PathBuf::from(d).join(name),
            _ => match std::env::var("XDG_RUNTIME_DIR") {
                Ok(d) if !d.is_empty() && std::path::Path::new(&d).is_dir() => PathBuf::from(d).join("amux").join(name),
                _ => PathBuf::from(format!("/tmp/amux-{}", crate::util::uid())).join(name),
            },
        };
        let state = match std::env::var("AMUX_STATE_DIR") {
            Ok(d) if !d.is_empty() => PathBuf::from(d).join(name),
            _ => match std::env::var("XDG_STATE_HOME") {
                Ok(d) if !d.is_empty() => PathBuf::from(d).join("amux").join(name),
                _ => PathBuf::from(crate::util::home()).join(".local/state/amux").join(name),
            },
        };
        Paths { name: name.to_string(), run, state }
    }

    pub fn server_sock(&self) -> PathBuf {
        self.run.join("server.sock")
    }
    pub fn lock(&self) -> PathBuf {
        self.run.join("server.lock")
    }
    pub fn keeper_dir(&self) -> PathBuf {
        self.run.join("k")
    }
    pub fn keeper_sock(&self, id: u32) -> PathBuf {
        self.keeper_dir().join(format!("{}.sock", id))
    }
    pub fn shim_dir(&self) -> PathBuf {
        self.run.join("shim")
    }
    pub fn state_file(&self) -> PathBuf {
        self.state.join("state.json")
    }
    pub fn snap_dir(&self) -> PathBuf {
        self.state.join("panes")
    }
    pub fn snap_file(&self, id: u32) -> PathBuf {
        self.snap_dir().join(format!("{}.snap", id))
    }
    pub fn log_file(&self) -> PathBuf {
        self.state.join("amux.log")
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true).mode(0o700);
        b.create(&self.run)?;
        b.create(self.keeper_dir())?;
        b.create(&self.state)?;
        b.create(self.snap_dir())?;
        Ok(())
    }
}
