use std::ffi::OsStr;
use std::fmt;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug)]
pub struct Refused(pub String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<std::io::Error> for Refused {
    fn from(e: std::io::Error) -> Self {
        Refused(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Refused>;

macro_rules! refuse {
    ($($t:tt)*) => { return Err($crate::run::Refused(format!($($t)*))) };
}
pub(crate) use refuse;

pub struct Out {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

pub struct Cmd {
    cmd: Command,
    input: Option<Vec<u8>>,
    label: String,
}

impl Cmd {
    pub fn new(program: &str) -> Self {
        Cmd {
            cmd: Command::new(program),
            input: None,
            label: program.into(),
        }
    }

    pub fn git(dir: &Path) -> Self {
        let mut c = Self::new("git");
        c.cmd
            .arg("--no-optional-locks")
            .current_dir(dir)
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env_remove("GIT_INDEX_FILE");
        drop_injected_hooks_path(&mut c.cmd);
        c
    }

    pub fn arg(mut self, a: impl AsRef<OsStr>) -> Self {
        if self.label.len() < 60 {
            self.label.push(' ');
            self.label.push_str(&a.as_ref().to_string_lossy());
        }
        self.cmd.arg(a);
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(mut self, items: I) -> Self {
        for a in items {
            self = self.arg(a);
        }
        self
    }

    pub fn cwd(mut self, dir: &Path) -> Self {
        self.cmd.current_dir(dir);
        self
    }

    pub fn env(mut self, key: &str, value: impl AsRef<OsStr>) -> Self {
        self.cmd.env(key, value);
        self
    }

    pub fn env_remove(mut self, key: &str) -> Self {
        self.cmd.env_remove(key);
        self
    }

    pub fn input(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.input = Some(data.into());
        self
    }

    pub fn out(mut self) -> Result<Out> {
        self.cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        self.cmd.stdin(if self.input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = self
            .cmd
            .spawn()
            .map_err(|e| Refused(format!("{}: {e}", self.label)))?;
        let feeder = self.input.take().map(|data| {
            let mut stdin = child.stdin.take().expect("piped stdin");
            std::thread::spawn(move || {
                let _ = stdin.write_all(&data);
            })
        });
        let o = child.wait_with_output()?;
        if let Some(f) = feeder {
            let _ = f.join();
        }
        Ok(Out {
            code: o.status.code().unwrap_or(-1),
            stdout: o.stdout,
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        })
    }

    fn checked(self) -> Result<Out> {
        let label = self.label.clone();
        let o = self.out()?;
        if !o.ok() {
            refuse!("{label}… failed ({}): {}", o.code, o.stderr.trim());
        }
        Ok(o)
    }

    pub fn text(self) -> Result<String> {
        Ok(self.checked()?.text())
    }

    pub fn raw(self) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.checked()?.stdout).into_owned())
    }

    pub fn bytes(self) -> Result<Vec<u8>> {
        Ok(self.checked()?.stdout)
    }

    /// Runs with the caller's stdout and stderr, so hook output reaches whoever ran git-mux.
    pub fn status(mut self) -> Result<bool> {
        self.cmd.stdin(Stdio::null());
        let s = self
            .cmd
            .status()
            .map_err(|e| Refused(format!("{}: {e}", self.label)))?;
        Ok(s.success())
    }
}

fn is_hooks_path(key: &str) -> bool {
    key.eq_ignore_ascii_case("core.hookspath")
}

/// An agent that injects `core.hooksPath` through GIT_CONFIG_COUNT/KEY_n/VALUE_n (Gemini CLI
/// sets it empty) would switch off the repository's hooks, which are what verify a candidate.
/// Git-mux's git commands get the other pairs, renumbered; the agent's own commands keep all.
fn drop_injected_hooks_path(cmd: &mut Command) {
    let var = |k: String| std::env::var(k).ok();
    let Some(count) = var("GIT_CONFIG_COUNT".into()).and_then(|n| n.parse::<usize>().ok()) else {
        return;
    };
    let pairs: Vec<(Option<String>, Option<String>)> = (0..count)
        .map(|i| {
            (
                var(format!("GIT_CONFIG_KEY_{i}")),
                var(format!("GIT_CONFIG_VALUE_{i}")),
            )
        })
        .collect();
    if !pairs
        .iter()
        .any(|(k, _)| k.as_deref().is_some_and(is_hooks_path))
    {
        return;
    }
    let kept: Vec<_> = pairs
        .into_iter()
        .filter(|(k, _)| !k.as_deref().is_some_and(is_hooks_path))
        .collect();
    cmd.env("GIT_CONFIG_COUNT", kept.len().to_string());
    for i in 0..count {
        match kept.get(i) {
            Some((key, value)) => {
                cmd.env(format!("GIT_CONFIG_KEY_{i}"), key.as_deref().unwrap_or(""));
                cmd.env(
                    format!("GIT_CONFIG_VALUE_{i}"),
                    value.as_deref().unwrap_or(""),
                );
            }
            None => {
                cmd.env_remove(format!("GIT_CONFIG_KEY_{i}"));
                cmd.env_remove(format!("GIT_CONFIG_VALUE_{i}"));
            }
        }
    }
}

/// `git -c core.hooksPath=… mux <command>` reaches git-mux through GIT_CONFIG_PARAMETERS, which
/// every git command inherits and git-mux cannot cleanly edit: every command but `hook` refuses.
pub fn refuse_hooks_path_parameter() -> Result<()> {
    let Ok(parameters) = std::env::var("GIT_CONFIG_PARAMETERS") else {
        return Ok(());
    };
    let quoted = parameters.split('\'').skip(1).step_by(2);
    for item in quoted {
        let key = item.split('=').next().unwrap_or(item);
        if is_hooks_path(key) {
            refuse!(
                "core.hooksPath is set for this command (git -c core.hooksPath=…); git-mux verifies a commit with the repository's own hooks, so run it without that option"
            );
        }
    }
    Ok(())
}

pub fn zsplit(s: &str) -> Vec<String> {
    s.split('\0')
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
}
