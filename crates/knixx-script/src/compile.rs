//! The compiler of the scripts, on the host: from source to the bytecode the
//! board runs, with the checks the board cannot make.

use std::fmt;
use std::string::{String, ToString};
use std::vec::Vec;

use knixx_knx::tp1::Telegram;
use rhai::Engine;
use rhai::grain::{Compiler, Sidecar};

use crate::{FUNCTIONS, Host, Info, Runtime, Script};

/// What the compiler fills in for the placeholders `{{compiled}}`,
/// `{{version}}` and `{{hash}}` of a script.
#[derive(Debug, Clone, Copy)]
pub struct Meta<'a> {
    /// When it was compiled, in UTC.
    pub compiled: &'a str,
    /// The short commit hash of the script's repository, or `unversioned`.
    pub version: &'a str,
    /// The md5 of the script's source, before the placeholders are filled.
    pub hash: &'a str,
}

/// A compiled script.
pub struct Build {
    /// The bytecode for the board, without diagnostics.
    pub artifact: Vec<u8>,
    /// The diagnostics, which turn a fault's addresses into source lines.
    pub sidecar: Sidecar,
    /// What its on_info returns.
    pub info: Info,
}

/// Why a script does not compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    /// It does not parse, with Rhai's message and position.
    Parse(String),
    /// It lacks one of the four functions, or defines it with another
    /// number of parameters: the name and the number expected.
    Function(&'static str, usize),
    /// Part of it does not lower to bytecode: the construct and its position.
    Residual(String),
    /// The bytecode cannot be written.
    Write(String),
    /// on_info fails or does not return what the compiler filled in.
    Info(String),
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CompileError::Parse(e) => f.write_str(e),
            CompileError::Function(name, n) => {
                write!(f, "the script must define fn {name}() with {n} parameter(s)")
            }
            CompileError::Residual(e) => write!(f, "does not compile to bytecode: {e}"),
            CompileError::Write(e) => write!(f, "cannot write the bytecode: {e}"),
            CompileError::Info(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for CompileError {}

/// A host for on_info's run on the host: it sends nothing.
struct NoHost;

impl Host for NoHost {
    fn send(&mut self, _: &Telegram) -> bool {
        false
    }

    fn print(&mut self, _: &str) {}
}

/// Compiles source, with meta filled in for its placeholders, to the
/// bytecode of the board, and checks it: the four functions with their
/// parameters, no for loops (the board has no iterators), all of it
/// lowered to bytecode, and on_info returning the values filled in.
pub fn compile(source: &str, meta: &Meta) -> Result<Build, CompileError> {
    let source = source
        .replace("{{compiled}}", meta.compiled)
        .replace("{{version}}", meta.version)
        .replace("{{hash}}", meta.hash);
    let mut engine = Engine::new_raw();
    // The board has no iterators: a for loop would fail there at run time.
    engine.disable_symbol("for");
    // Rhai's defaults for the nesting of expressions are lower in a debug
    // build; these, its release defaults, make a script compile the same in
    // any build of the compiler.
    engine.set_max_expr_depths(64, 32);
    let ast = engine
        .compile(&source)
        .map_err(|e| CompileError::Parse(e.to_string()))?;
    for (name, n) in FUNCTIONS {
        if !ast
            .iter_functions()
            .any(|f| f.name == name && f.params.len() == n)
        {
            return Err(CompileError::Function(name, n));
        }
    }
    let program = Compiler::new().compile(&ast);
    if program.residual_count() != 0 {
        let what = match program.first_unsupported() {
            Some((what, pos)) => std::format!("{what} at {pos}"),
            None => "an AST fragment".to_string(),
        };
        return Err(CompileError::Residual(what));
    }
    let stripped = program
        .write_stripped()
        .map_err(|e| CompileError::Write(std::format!("{e:?}")))?;

    // on_info, run as on the board, from the artifact.
    let rt = Runtime::new(NoHost);
    let mut script = Script::load(&stripped.artifact).map_err(|e| CompileError::Write(e.to_string()))?;
    let info = script.info(&rt).map_err(|e| CompileError::Info(e.to_string()))?;
    for (key, got, want) in [
        ("compiled", &info.compiled, meta.compiled),
        ("version", &info.version, meta.version),
        ("hash", &info.hash, meta.hash),
    ] {
        if got != want {
            return Err(CompileError::Info(std::format!(
                "on_info: {key} is {got:?}, not \"{{{{{key}}}}}\""
            )));
        }
    }
    Ok(Build {
        artifact: stripped.artifact,
        sidecar: stripped.sidecar,
        info,
    })
}
