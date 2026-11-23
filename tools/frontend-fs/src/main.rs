#![deny(unsafe_code)]

#[path = "../../../src-tauri/src/file_io.rs"]
#[allow(dead_code)]
mod file_io;
#[allow(unsafe_code, dead_code)]
mod windows_fs {
    include!("../../../src-tauri/src/windows_fs.rs");
    include!("guarded_paths.rs");
    include!("guard.rs");
}

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, Read, Write},
    path::Path,
};
use windows_fs::frontend::Session;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", deny_unknown_fields)]
enum Request {
    Read {
        name: String,
    },
    Make {
        kind: String,
    },
    Check {
        directory: String,
    },
    Begin {
        directory: String,
        name: String,
        size: usize,
    },
    Append {
        write: String,
        data: String,
    },
    Commit {
        write: String,
    },
    Abort {
        write: String,
    },
    Remove {
        directory: String,
    },
    Seal {
        directory: String,
    },
    Publish {
        directory: String,
    },
    Close,
}

fn execute(session: &mut Session, request: Request) -> io::Result<Value> {
    if !matches!(request, Request::Close) {
        session.validate_root()?;
    }
    match request {
        Request::Read { name } => {
            let (bytes, binary) = session.read(&name)?;
            let text = if binary {
                STANDARD.encode(bytes)
            } else {
                String::from_utf8(bytes).map_err(|_| io::ErrorKind::InvalidData)?
            };
            Ok(json!(text))
        }
        Request::Make { kind } => {
            let (id, path) = session.make(&kind)?;
            Ok(json!({ "id": id, "path": path }))
        }
        Request::Check { directory } => {
            session.check(&directory)?;
            Ok(Value::Null)
        }
        Request::Begin {
            directory,
            name,
            size,
        } => Ok(json!(session.begin(&directory, &name, size)?)),
        Request::Append { write, data } => {
            const MAX_CHUNK: usize = 256 * 1024;
            let result = (|| {
                if data.len() > MAX_CHUNK.div_ceil(3) * 4 {
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                let bytes = STANDARD
                    .decode(data)
                    .map_err(|_| io::ErrorKind::InvalidInput)?;
                if bytes.len() > MAX_CHUNK {
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                session.append(&write, &bytes)
            })();
            if result.is_err() {
                session.abort(&write);
            }
            result?;
            Ok(Value::Null)
        }
        Request::Commit { write } => {
            session.commit(&write)?;
            Ok(Value::Null)
        }
        Request::Abort { write } => {
            session.abort(&write);
            Ok(Value::Null)
        }
        Request::Remove { directory } => {
            session.remove(&directory)?;
            Ok(Value::Null)
        }
        Request::Seal { directory } => Ok(json!(session.seal(&directory)?)),
        Request::Publish { directory } => {
            session.publish(&directory)?;
            Ok(Value::Null)
        }
        Request::Close => Ok(Value::Null),
    }
}

fn reply(output: &mut impl Write, result: io::Result<Value>) -> io::Result<()> {
    let value = match result {
        Ok(value) => json!({ "ok": true, "value": value }),
        Err(error) => json!({ "ok": false, "error": error.to_string() }),
    };
    serde_json::to_writer(&mut *output, &value)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn run(arguments: &[std::ffi::OsString]) -> io::Result<()> {
    if arguments.len() != 4 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let number = |index: usize| -> io::Result<u64> {
        arguments[index]
            .to_str()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| io::ErrorKind::InvalidInput.into())
    };
    windows_fs::frontend::watch_parent(
        u32::try_from(number(3)?).map_err(|_| io::ErrorKind::InvalidInput)?,
    )?;
    let mut session = Session::new(Path::new(&arguments[0]), number(1)?, number(2)?)?;
    let input = io::stdin();
    let mut input = input.lock();
    let mut output = io::stdout().lock();
    reply(
        &mut output,
        Ok(json!({ "version": 2, "root": session.root_path() })),
    )?;
    loop {
        let mut line = Vec::new();
        let count = input
            .by_ref()
            .take(1024 * 1024 + 1)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        if count > 1024 * 1024 || !line.ends_with(b"\n") {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let request: Request =
            serde_json::from_slice(&line).map_err(|_| io::ErrorKind::InvalidInput)?;
        let close = matches!(request, Request::Close);
        reply(&mut output, execute(&mut session, request))?;
        if close {
            break;
        }
    }
    Ok(())
}

fn main() {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let result = if arguments.first().is_some_and(|arg| arg == "--release") {
        windows_fs::frontend::run_release(&arguments[1..])
    } else {
        run(&arguments).map(|()| 0)
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("Frontend filesystem guard: {error}");
            std::process::exit(1);
        }
    }
}
