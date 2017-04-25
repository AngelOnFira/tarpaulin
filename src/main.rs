extern crate cargo_tarpaulin;
extern crate nix;
extern crate docopt;
extern crate cargo;
extern crate rustc_serialize;
extern crate gimli;
extern crate object;
extern crate memmap;
extern crate fallible_iterator;
extern crate rustc_demangle;

use cargo_tarpaulin::tracer;
use std::io;
use std::ffi::CString;
use docopt::Docopt;
use std::path::Path;
use nix::sys::signal;
use nix::unistd::*;
use nix::libc::{pid_t, c_void};
use nix::sys::wait::*;
use nix::sys::ptrace::*;
use nix::sys::ptrace::ptrace::*;
use cargo::util::Config;
use cargo::core::Workspace;
use cargo::ops;
use std::ptr;

const USAGE: &'static str = "
Tarpaulin - a cargo code coverage tool

Usage: 
    cargo-tarpaulin [options]
    cargo-tarpaulin (-h | --help)

Options:
    -h, --help                  Show this message.
    -l, --line                  Collect line coverage.
    -b, --branch                Collect branch coverage.
    -c, --condition             Collect condition coverage.
    --out ARG                   Specify output type [default: Report].
    -v, --verbose               Show extra output.
    -m ARG, --manifest ARG      Path to a cargo.toml to execute tarpaulin on. 
                                Default is current directory

";

#[derive(RustcDecodable, Debug)]
enum Out {
    Json,
    Toml,
    Report
}

#[derive(RustcDecodable, Debug)]
struct Args {
    flag_line: bool,
    flag_branch: bool,
    flag_condition:bool,
    flag_verbose: bool,
    flag_out: Option<Out>,
    flag_manifest: Option<String>,
}

fn main() {
    let args:Args = Docopt::new(USAGE)
                           .and_then(|d| d.decode())
                           .unwrap_or_else(|e| e.exit());
   
    let mut path = std::env::current_dir().unwrap();

    if let Some(p) = args.flag_manifest {
        path.push(p);
    };
    path.push("Cargo.toml");
    
    let config = Config::default().unwrap();
    let workspace =match  Workspace::new(path.as_path(), &config) {
        Ok(w) => w,
        Err(_) => panic!("Invalid project directory specified"),
    };
    for m in workspace.members() {
        println!("{:?}", m.manifest_path());
    }

    let filter = ops::CompileFilter::Everything;

    let copt = ops::CompileOptions {
        config: &config,
        jobs: None,
        target: None,
        features: &[],
        all_features: true,
        no_default_features:false ,
        spec: ops::Packages::All,
        release: false,
        mode: ops::CompileMode::Test,
        filter: filter,
        message_format: ops::MessageFormat::Human,
        target_rustdoc_args: None,
        target_rustc_args: None,
    };
    // Do I need to clean beforehand?
    if let Ok(comp) = ops::compile(&workspace, &copt) {
        for c in comp.tests.iter() {
            match fork() {
                Ok(ForkResult::Parent{ child }) => {
                    match collect_coverage(workspace.root(), 
                                           c.2.as_path(), child) {
                        Ok(_) => println!("Coverage successful"),
                        Err(e) => println!("Error occurred: \n{}", e),
                    }
                }
                Ok(ForkResult::Child) => {
                    execute_test(c.2.as_path(), true);
                }
                Err(err) => { 
                    println!("Failed to run {}", c.2.display());
                    println!("Error {}", err);
                }
            }
        }
    }
}

fn collect_coverage(project_path: &Path, 
                    test_path: &Path, 
                    test: pid_t) -> io::Result<()> {
    let traces = tracer::generate_tracer_data(project_path, test_path)?;
    
    match waitpid(test, None) {
        Ok(WaitStatus::Stopped(child, signal::SIGTRAP)) => {
            println!("Running test without analysing for now");
            // Use PTRACE_POKETEXT here to attach software breakpoints to lines 
            // we need to cover
            for trace in traces.iter() {
                let raw_addr = trace.address as * mut c_void;
                match ptrace(PTRACE_POKETEXT, child, raw_addr, ptr::null_mut()) {
                    Ok(_) => println!("Added trace"),
                    Err(e) => println!("Failed to add trace:\n {}", e),
                }
                    
            }
            ptrace(PTRACE_CONT, child, ptr::null_mut(), ptr::null_mut())
                .ok()
                .expect("Failed to continue test");
        }
        Ok(_) => {
            println!("Unexpected grab");
        }
        Err(err) => println!("{}", err)
    }
    // Now we start hitting lines!
    loop {
        match waitpid(test, None) {
            Ok(WaitStatus::Stopped(child, signal::SIGTRAP)) => {
                println!("Hit an instrumentation point");
                ptrace(PTRACE_CONT, child, ptr::null_mut(), ptr::null_mut())
                    .ok()
                    .expect("Failed to continue test");
                   
            },
            Ok(WaitStatus::Exited(child, code)) => {
                println!("Test finished");
                break;
            },
            _ => {},
        }
    }
    Ok(())
}

fn execute_test(test: &Path, backtrace_on: bool) {
    
    let exec_path = CString::new(test.to_str().unwrap()).unwrap();

    ptrace(PTRACE_TRACEME, 0, ptr::null_mut(), ptr::null_mut())
        .ok()
        .expect("Failed to trace");

    let envars: Vec<CString> = if backtrace_on {
        vec![CString::new("RUST_BACKTRACE=1").unwrap()]
    } else {
        vec![]
    };
    execve(&exec_path, &[exec_path.clone()], envars.as_slice())
        .unwrap();
}
