use anyhow::Result;
use containerd_shim_wasm::sandbox::instance::YoukiInstance;
use containerd_shim_wasm::sandbox::instance_utils::maybe_open_stdio;
use libcontainer::container::builder::ContainerBuilder;
use libcontainer::container::Container;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::Context;
use chrono::{DateTime, Utc};
use containerd_shim_wasm::sandbox::error::Error;
use containerd_shim_wasm::sandbox::{EngineGetter, Instance, InstanceConfig};
use libc::{dup2, STDERR_FILENO, STDIN_FILENO, STDOUT_FILENO};
use libcontainer::syscall::syscall::create_syscall;

use wasmtime::Engine;

use crate::executor::WasmtimeExecutor;

static DEFAULT_CONTAINER_ROOT_DIR: &str = "/run/containerd/wasmtime";
type ExitCode = Arc<(Mutex<Option<(u32, DateTime<Utc>)>>, Condvar)>;

static mut STDIN_FD: Option<RawFd> = None;
static mut STDOUT_FD: Option<RawFd> = None;
static mut STDERR_FD: Option<RawFd> = None;

pub struct Wasi {
    exit_code: ExitCode,
    engine: wasmtime::Engine,
    stdin: String,
    stdout: String,
    stderr: String,
    bundle: String,
    rootdir: PathBuf,
    id: String,
}

pub fn reset_stdio() {
    unsafe {
        if STDIN_FD.is_some() {
            dup2(STDIN_FD.unwrap(), STDIN_FILENO);
        }
        if STDOUT_FD.is_some() {
            dup2(STDOUT_FD.unwrap(), STDOUT_FILENO);
        }
        if STDERR_FD.is_some() {
            dup2(STDERR_FD.unwrap(), STDERR_FILENO);
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Options {
    root: Option<PathBuf>,
}

fn determine_rootdir<P: AsRef<Path>>(bundle: P, namespace: String) -> Result<PathBuf, Error> {
    log::info!(
        "determining rootdir for bundle: {}",
        bundle.as_ref().display()
    );
    let mut file = match File::open(bundle.as_ref().join("options.json")) {
        Ok(f) => f,
        Err(err) => match err.kind() {
            ErrorKind::NotFound => {
                return Ok(<&str as Into<PathBuf>>::into(DEFAULT_CONTAINER_ROOT_DIR).join(namespace))
            }
            _ => return Err(err.into()),
        },
    };
    let mut data = String::new();
    file.read_to_string(&mut data)?;
    let options: Options = serde_json::from_str(&data)?;
    let path = options
        .root
        .unwrap_or(PathBuf::from(DEFAULT_CONTAINER_ROOT_DIR))
        .join(namespace);
    log::info!("youki root path is: {}", path.display());
    Ok(path)
}

impl Instance for Wasi {
    type E = wasmtime::Engine;
    fn new(id: String, cfg: Option<&InstanceConfig<Self::E>>) -> Self {
        // TODO: there are failure cases e.x. parsing cfg, loading spec, etc.
        // thus should make `new` return `Result<Self, Error>` instead of `Self`
        log::info!("creating new instance: {}", id);
        let cfg = cfg.unwrap();
        let bundle = cfg.get_bundle().unwrap_or_default();
        let rootdir = determine_rootdir(bundle.as_str(), cfg.get_namespace()).unwrap();
        Wasi {
            id,
            exit_code: Arc::new((Mutex::new(None), Condvar::new())),
            engine: cfg.get_engine(),
            stdin: cfg.get_stdin().unwrap_or_default(),
            stdout: cfg.get_stdout().unwrap_or_default(),
            stderr: cfg.get_stderr().unwrap_or_default(),
            bundle,
            rootdir,
        }
    }

    fn start(&self) -> std::result::Result<u32, Error> {
        self.start_youki()
    }

    fn kill(&self, signal: u32) -> std::result::Result<(), Error> {
        self.kill_youki(signal)
    }

    fn delete(&self) -> std::result::Result<(), Error> {
        self.delete_youki()
    }

    fn wait(
        &self,
        waiter: &containerd_shim_wasm::sandbox::instance::Wait,
    ) -> std::result::Result<(), Error> {
        self.wait_youki(waiter)
    }
}

impl YoukiInstance for Wasi {
    fn get_exit_code(&self) -> ExitCode {
        self.exit_code.clone()
    }

    fn get_id(&self) -> String {
        self.id.clone()
    }

    fn get_root_dir(&self) -> std::result::Result<PathBuf, Error> {
        Ok(self.rootdir.clone())
    }

    fn build_container(&self) -> std::result::Result<Container, Error> {
        let engine = self.engine.clone();
        let syscall = create_syscall();
        let stdin = maybe_open_stdio(&self.stdin).context("could not open stdin")?;
        let stdout = maybe_open_stdio(&self.stdout).context("could not open stdout")?;
        let stderr = maybe_open_stdio(&self.stderr).context("could not open stderr")?;
        let err_msg = |err| format!("failed to create container: {}", err);
        let container = ContainerBuilder::new(self.id.clone(), syscall.as_ref())
            .with_executor(vec![Box::new(WasmtimeExecutor {
                stdin,
                stdout,
                stderr,
                engine,
            })])
            .map_err(|err| Error::Others(err_msg(err)))?
            .with_root_path(self.rootdir.clone())
            .map_err(|err| Error::Others(err_msg(err)))?
            .as_init(&self.bundle)
            .with_systemd(false)
            .build()
            .map_err(|err| Error::Others(err_msg(err)))?;
        Ok(container)
    }
}

impl EngineGetter for Wasi {
    type E = wasmtime::Engine;
    fn new_engine() -> Result<Engine, Error> {
        Ok(Engine::default())
    }
}

#[cfg(test)]
mod wasitest {
    use std::fs::{create_dir, read_to_string, File, OpenOptions};
    use std::io::prelude::*;
    use std::os::unix::prelude::OpenOptionsExt;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use containerd_shim_wasm::function;
    use containerd_shim_wasm::sandbox::instance::Wait;
    use containerd_shim_wasm::sandbox::testutil::{has_cap_sys_admin, run_test_with_sudo};
    use libc::SIGKILL;
    use oci_spec::runtime::{ProcessBuilder, RootBuilder, SpecBuilder};
    use tempfile::{tempdir, TempDir};

    use super::*;

    // This is taken from https://github.com/bytecodealliance/wasmtime/blob/6a60e8363f50b936e4c4fc958cb9742314ff09f3/docs/WASI-tutorial.md?plain=1#L270-L298
    const WASI_HELLO_WAT: &[u8]= r#"(module
        ;; Import the required fd_write WASI function which will write the given io vectors to stdout
        ;; The function signature for fd_write is:
        ;; (File Descriptor, *iovs, iovs_len, nwritten) -> Returns number of bytes written
        (import "wasi_unstable" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))

        (memory 1)
        (export "memory" (memory 0))

        ;; Write 'hello world\n' to memory at an offset of 8 bytes
        ;; Note the trailing newline which is required for the text to appear
        (data (i32.const 8) "hello world\n")

        (func $main (export "_start")
            ;; Creating a new io vector within linear memory
            (i32.store (i32.const 0) (i32.const 8))  ;; iov.iov_base - This is a pointer to the start of the 'hello world\n' string
            (i32.store (i32.const 4) (i32.const 12))  ;; iov.iov_len - The length of the 'hello world\n' string

            (call $fd_write
                (i32.const 1) ;; file_descriptor - 1 for stdout
                (i32.const 0) ;; *iovs - The pointer to the iov array, which is stored at memory location 0
                (i32.const 1) ;; iovs_len - We're printing 1 string stored in an iov - so one.
                (i32.const 20) ;; nwritten - A place in memory to store the number of bytes written
            )
            drop ;; Discard the number of bytes written from the top of the stack
        )
    )
    "#.as_bytes();

    #[test]
    fn test_delete_after_create() -> Result<()> {
        let dir = tempdir()?;
        let cfg = prepare_cfg(&dir)?;

        let i = Wasi::new("".to_string(), Some(&cfg));
        i.delete()?;
        reset_stdio();
        Ok(())
    }

    #[test]
    fn test_wasi() -> Result<(), Error> {
        if !has_cap_sys_admin() {
            println!("running test with sudo: {}", function!());
            return run_test_with_sudo(function!());
        }
        // start logging
        let _ = env_logger::try_init();

        let dir = tempdir()?;
        let cfg = prepare_cfg(&dir)?;

        let wasi = Wasi::new("test".to_string(), Some(&cfg));

        wasi.start()?;

        let (tx, rx) = channel();
        let waiter = Wait::new(tx);
        wasi.wait(&waiter).unwrap();

        let res = match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(res) => res,
            Err(e) => {
                wasi.kill(SIGKILL as u32).unwrap();
                return Err(Error::Others(format!(
                    "error waiting for module to finish: {0}",
                    e
                )));
            }
        };
        assert_eq!(res.0, 0);

        let output = read_to_string(dir.path().join("stdout"))?;
        assert_eq!(output, "hello world\n");

        wasi.delete()?;

        reset_stdio();
        Ok(())
    }

    fn prepare_cfg(dir: &TempDir) -> Result<InstanceConfig<Engine>> {
        create_dir(dir.path().join("rootfs"))?;

        let opts = Options {
            root: Some(dir.path().join("runwasi")),
        };
        let opts_file = OpenOptions::new()
            .read(true)
            .create(true)
            .truncate(true)
            .write(true)
            .open(dir.path().join("options.json"))?;
        write!(&opts_file, "{}", serde_json::to_string(&opts)?)?;

        let wasm_path = dir.path().join("rootfs/hello.wat");
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(wasm_path)?;
        f.write_all(WASI_HELLO_WAT)?;

        let stdout = File::create(dir.path().join("stdout"))?;
        let stderr = File::create(dir.path().join("stderr"))?;
        drop(stdout);
        drop(stderr);
        let spec = SpecBuilder::default()
            .root(RootBuilder::default().path("rootfs").build()?)
            .process(
                ProcessBuilder::default()
                    .cwd("/")
                    .args(vec!["./hello.wat".to_string()])
                    .build()?,
            )
            .build()?;
        spec.save(dir.path().join("config.json"))?;
        let mut cfg = InstanceConfig::new(
            Engine::default(),
            "test_namespace".into(),
            "/containerd/address".into(),
        );
        let cfg = cfg
            .set_bundle(dir.path().to_str().unwrap().to_string())
            .set_stdout(dir.path().join("stdout").to_str().unwrap().to_string())
            .set_stderr(dir.path().join("stderr").to_str().unwrap().to_string());
        Ok(cfg.to_owned())
    }
}
