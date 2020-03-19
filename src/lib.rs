use crate::config::*;
use crate::errors::*;
use crate::process_handling::*;
use crate::report::report_coverage;
use crate::source_analysis::LineAnalysis;
use crate::statemachine::*;
use crate::test_loader::*;
use crate::traces::*;
use log::{info, trace, warn};
use nix::unistd::*;
use std::collections::HashMap;
use std::env;
use std::ffi::CString;
use std::path::{Path, PathBuf};

pub mod breakpoint;
mod cargo;
pub mod config;
pub mod errors;
mod process_handling;
pub mod report;
mod source_analysis;
mod statemachine;
pub mod test_loader;
pub mod traces;

mod ptrace_control;

pub fn trace(configs: &[Config]) -> Result<TraceMap, RunError> {
    let mut tracemap = TraceMap::new();
    let mut ret = 0i32;
    let mut failure = Ok(());

    for config in configs.iter() {
        if config.name == "report" {
            continue;
        }
        match launch_tarpaulin(config) {
            Ok((t, r)) => {
                tracemap.merge(&t);
                ret |= r;
            }
            Err(e) => {
                info!("Failure {}", e);
                if failure.is_ok() {
                    failure = Err(e);
                }
            }
        }
    }
    tracemap.dedup();
    if ret == 0 {
        Ok(tracemap)
    } else {
        Err(RunError::TestFailed)
    }
}

pub fn run(configs: &[Config]) -> Result<(), RunError> {
    let tracemap = trace(configs)?;
    if configs.len() == 1 {
        report_coverage(&configs[0], &tracemap)?;
    } else if !configs.is_empty() {
        let mut reported = false;
        for c in configs.iter() {
            if c.name == "report" {
                reported = true;
                report_coverage(c, &tracemap)?;
            }
        }
        if !reported {
            report_coverage(&configs[0], &tracemap)?;
        }
    }

    Ok(())
}

/// Launches tarpaulin with the given configuration.
pub fn launch_tarpaulin(config: &Config) -> Result<(TraceMap, i32), RunError> {
    if !config.name.is_empty() {
        info!("Running config {}", config.name);
    }

    info!("Running Tarpaulin");

    let mut result = TraceMap::new();
    let mut return_code = 0i32;
    let project_analysis = source_analysis::get_line_analysis(config);
    info!("Building project");
    let executables = cargo::get_tests(config)?;
    for exe in &executables {
        let coverage = get_test_coverage(exe.path(), &project_analysis, config, false)?;
        if let Some(res) = coverage {
            result.merge(&res.0);
            return_code |= res.1;
        }
        if config.run_ignored && exe.run_type() == RunType::Tests {
            let coverage = get_test_coverage(exe.path(), &project_analysis, config, true)?;
            if let Some(res) = coverage {
                result.merge(&res.0);
                return_code |= res.1;
            }
        }
    }
    result.dedup();
    Ok((result, return_code))
}

/// Returns the coverage statistics for a test executable in the given workspace
pub fn get_test_coverage(
    test: &Path,
    analysis: &HashMap<PathBuf, LineAnalysis>,
    config: &Config,
    ignored: bool,
) -> Result<Option<(TraceMap, i32)>, RunError> {
    if !test.exists() {
        return Ok(None);
    }
    #[cfg(target_os = "linux")] {
        if let Err(e) = limit_affinity() {
            warn!("Failed to set processor affinity {}", e);
        }
    }
    match fork() {
        Ok(ForkResult::Parent { child }) => match collect_coverage(test, child, analysis, config) {
            Ok(t) => Ok(Some(t)),
            Err(e) => Err(RunError::TestCoverage(e.to_string())),
        },
        Ok(ForkResult::Child) => {
            info!("Launching test");
            execute_test(test, ignored, config)?;
            Ok(None)
        }
        Err(err) => Err(RunError::TestCoverage(format!(
            "Failed to run test {}, Error: {}",
            test.display(),
            err.to_string()
        ))),
    }
}

/// Collects the coverage data from the launched test
fn collect_coverage(
    test_path: &Path,
    test: Pid,
    analysis: &HashMap<PathBuf, LineAnalysis>,
    config: &Config,
) -> Result<(TraceMap, i32), RunError> {
    let mut ret_code = 0;
    let mut traces = generate_tracemap(test_path, analysis, config)?;
    {
        trace!("Test PID is {}", test);
        let (mut state, mut data) = create_state_machine(test, &mut traces, config);
        loop {
            state = state.step(&mut data, config)?;
            if state.is_finished() {
                if let TestState::End(i) = state {
                    ret_code = i;
                }
                break;
            }
        }
    }
    Ok((traces, ret_code))
}

/// Launches the test executable
fn execute_test(test: &Path, ignored: bool, config: &Config) -> Result<(), RunError> {
    let exec_path = CString::new(test.to_str().unwrap()).unwrap();
    info!("running {}", test.display());
    let _ = env::set_current_dir(&config.root());

    let mut envars: Vec<CString> = Vec::new();

    for (key, value) in env::vars() {
        let mut temp = String::new();
        temp.push_str(key.as_str());
        temp.push('=');
        temp.push_str(value.as_str());
        envars.push(CString::new(temp).unwrap());
    }
    let mut argv = if ignored {
        vec![exec_path.clone(), CString::new("--ignored").unwrap()]
    } else {
        vec![exec_path.clone()]
    };
    if config.verbose {
        envars.push(CString::new("RUST_BACKTRACE=1").unwrap());
    }
    for s in &config.varargs {
        argv.push(CString::new(s.as_bytes()).unwrap_or_default());
    }

    execute(exec_path, &argv, envars.as_slice())
}
