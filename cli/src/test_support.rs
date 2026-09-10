//! In-process command harness: SQLite connections, JSON/config bytes and terminal
//! streams live entirely in thread-local memory. No subprocesses or temp files.
use crate::persist::{SqliteBackend, StorageBackend};
use clap::Parser;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fmt,
    io::{self, BufRead, Cursor, Write},
    path::{Path, PathBuf},
    rc::Rc,
};

#[derive(Default)]
struct State {
    files: BTreeMap<PathBuf, Vec<u8>>,
    databases: BTreeMap<PathBuf, Rc<SqliteBackend>>,
    input: Cursor<Vec<u8>>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}
thread_local! { static STATE: RefCell<State> = RefCell::new(State::default()); }

pub fn print(args: fmt::Arguments<'_>) {
    STATE.with(|state| state.borrow_mut().stdout.write_fmt(args).unwrap());
}
pub fn eprint(args: fmt::Arguments<'_>) {
    STATE.with(|state| state.borrow_mut().stderr.write_fmt(args).unwrap());
}
pub struct Stdin;
pub fn stdin() -> Stdin {
    Stdin
}
impl Stdin {
    pub fn read_line(&self, buffer: &mut String) -> io::Result<usize> {
        STATE.with(|state| state.borrow_mut().input.read_line(buffer))
    }
}
pub struct Status(bool);
impl Status {
    pub fn success(&self) -> bool {
        self.0
    }
}
pub struct Output {
    pub status: Status,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub fn run(path: &Path, arguments: &[&str], input: &str) -> Output {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.stdout.clear();
        state.stderr.clear();
        state.input = Cursor::new(input.as_bytes().to_vec());
    });
    let cli = crate::Cli::try_parse_from(
        ["quick-ubu", "--store", path.to_str().unwrap()]
            .into_iter()
            .chain(arguments.iter().copied()),
    );
    let result = cli.map_err(|error| error.to_string()).and_then(|cli| {
        let backend = STATE.with(|state| {
            let mut state = state.borrow_mut();
            state
                .databases
                .entry(path.to_owned())
                .or_insert_with(|| Rc::new(SqliteBackend::in_memory().unwrap()))
                .clone()
        });
        crate::run_with_backend(cli, backend.as_ref())
    });
    if let Err(error) = &result {
        eprint(format_args!("quick-ubu: {error}\n"));
    }
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        Output {
            status: Status(result.is_ok()),
            stdout: std::mem::take(&mut state.stdout),
            stderr: std::mem::take(&mut state.stderr),
        }
    })
}

/// Explicit fixture seeding, separate from production loading (no JSON migration).
pub fn seed(path: &Path, store: &ubu_core::Store) {
    let backend = SqliteBackend::in_memory().unwrap();
    backend.save(store).unwrap();
    STATE.with(|state| {
        state
            .borrow_mut()
            .databases
            .insert(path.to_owned(), Rc::new(backend));
    });
}

pub mod fs {
    use super::*;
    fn missing() -> io::Error {
        io::Error::from(io::ErrorKind::NotFound)
    }
    pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
        let path = path.as_ref();
        STATE.with(|state| {
            let state = state.borrow();
            if let Some(backend) = state.databases.get(path) {
                return serde_json::to_string(&backend.load().map_err(io::Error::other)?)
                    .map_err(io::Error::other);
            }
            // The canonical checked-in fixture is embedded, so tests never read disk.
            if path.ends_with("docs/example-routine.json") {
                return Ok(include_str!("../../docs/example-routine.json").to_owned());
            }
            String::from_utf8(state.files.get(path).ok_or_else(missing)?.clone())
                .map_err(io::Error::other)
        })
    }
    pub fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> io::Result<()> {
        STATE.with(|state| {
            state
                .borrow_mut()
                .files
                .insert(path.as_ref().to_owned(), bytes.as_ref().to_vec());
        });
        Ok(())
    }
    pub fn create_dir_all(_path: impl AsRef<Path>) -> io::Result<()> {
        Ok(())
    }
    pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.files.retain(|key, _| !key.starts_with(path.as_ref()));
            state
                .databases
                .retain(|key, _| !key.starts_with(path.as_ref()));
        });
        Ok(())
    }
    pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
        STATE.with(|state| {
            state
                .borrow_mut()
                .files
                .remove(path.as_ref())
                .map(|_| ())
                .ok_or_else(missing)
        })
    }
    pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            let bytes = state.files.remove(from.as_ref()).ok_or_else(missing)?;
            state.files.insert(to.as_ref().to_owned(), bytes);
            Ok(())
        })
    }
    pub fn exists(path: impl AsRef<Path>) -> bool {
        STATE.with(|state| {
            let state = state.borrow();
            state
                .files
                .keys()
                .chain(state.databases.keys())
                .any(|key| key.starts_with(path.as_ref()))
        })
    }
}
