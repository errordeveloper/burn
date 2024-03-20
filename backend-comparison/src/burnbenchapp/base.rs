use super::{
    auth::{save_token, CLIENT_ID},
    App,
};
use crate::burnbenchapp::auth::{get_token_from_cache, verify_token};
use crate::persistence::{BenchmarkCollection, BenchmarkRecord};
use arboard::Clipboard;
use clap::{Parser, Subcommand, ValueEnum};
use github_device_flow::{self, DeviceFlow};
use serde_json;
use std::fs;
use std::io::{BufRead, BufReader, Result as ioResult};
use std::{
    process::{Command, ExitStatus, Stdio},
    thread, time,
};

use strum::IntoEnumIterator;
use strum_macros::{Display, EnumIter};
const FIVE_SECONDS: time::Duration = time::Duration::new(5, 0);
const BENCHMARKS_TARGET_DIR: &str = "target/benchmarks";
const USER_BENCHMARK_SERVER_URL: &str = if cfg!(debug_assertions) {
    // development
    "http://localhost:8000/benchmarks"
} else {
    // production
    "https://user-benchmark-server-gvtbw64teq-nn.a.run.app/benchmarks"
};

/// Base trait to define an application
pub(crate) trait Application {
    fn init(&mut self) {}

    #[allow(unused)]
    fn run(
        &mut self,
        benches: &[BenchmarkValues],
        backends: &[BackendValues],
        token: Option<&str>,
    ) {
    }

    fn cleanup(&mut self) {}
}

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Authenticate using GitHub
    Auth,
    /// List all available benchmarks and backends
    List,
    /// Runs benchmarks
    Run(RunArgs),
}

#[derive(Parser, Debug)]
struct RunArgs {
    /// Share the benchmark results by uploading them to Burn servers
    #[clap(short = 's', long = "share")]
    share: bool,

    /// Space separated list of backends to include
    #[clap(short = 'B', long = "backends", value_name = "BACKEND BACKEND ...", num_args(1..), required = true)]
    backends: Vec<BackendValues>,

    /// Space separated list of benches to run
    #[clap(short = 'b', long = "benches", value_name = "BENCH BENCH ...", num_args(1..), required = true)]
    benches: Vec<BenchmarkValues>,
}

#[derive(Debug, Clone, PartialEq, Eq, ValueEnum, Display, EnumIter)]
pub(crate) enum BackendValues {
    #[strum(to_string = "candle-cpu")]
    CandleCpu,
    #[strum(to_string = "candle-cuda")]
    CandleCuda,
    #[strum(to_string = "candle-metal")]
    CandleMetal,
    #[strum(to_string = "ndarray")]
    Ndarray,
    #[strum(to_string = "ndarray-blas-accelerate")]
    NdarrayBlasAccelerate,
    #[strum(to_string = "ndarray-blas-netlib")]
    NdarrayBlasNetlib,
    #[strum(to_string = "ndarray-blas-openblas")]
    NdarrayBlasOpenblas,
    #[strum(to_string = "tch-cpu")]
    TchCpu,
    #[strum(to_string = "tch-gpu")]
    TchGpu,
    #[strum(to_string = "wgpu")]
    Wgpu,
    #[strum(to_string = "wgpu-fusion")]
    WgpuFusion,
    #[strum(to_string = "wgpu-dawn")]
    WgpuDawn,
    #[strum(to_string = "wgpu-dawn-fusion")]
    WgpuDawnFusion,
}

#[derive(Debug, Clone, PartialEq, Eq, ValueEnum, Display, EnumIter)]
pub(crate) enum BenchmarkValues {
    #[strum(to_string = "binary")]
    Binary,
    #[strum(to_string = "custom_gelu")]
    CustomGelu,
    #[strum(to_string = "data")]
    Data,
    #[strum(to_string = "matmul")]
    Matmul,
    #[strum(to_string = "unary")]
    Unary,
    #[strum(to_string = "max_pool2d")]
    MaxPool2d,
}

pub fn execute() {
    let args = Args::parse();
    match args.command {
        Commands::Auth => command_auth(),
        Commands::List => command_list(),
        Commands::Run(run_args) => command_run(run_args),
    }
}

/// Create an access token from GitHub Burnbench application and store it
/// to be used with the user benchmark backend.
fn command_auth() {
    let mut flow = match DeviceFlow::start(CLIENT_ID, None) {
        Ok(flow) => flow,
        Err(e) => {
            eprintln!("Error authenticating: {}", e);
            return;
        }
    };
    println!("🌐 Please visit for following URL in your browser (CTRL+click if your terminal supports it):");
    println!("\n    {}\n", flow.verification_uri.clone().unwrap());
    let user_code = flow.user_code.clone().unwrap();
    println!("👉 And enter code: {}", &user_code);
    if let Ok(mut clipboard) = Clipboard::new() {
        if clipboard.set_text(user_code).is_ok() {
            println!("📋 Code has been successfully copied to clipboard.")
        };
    };
    // Wait for the minimum allowed interval to poll for authentication update
    // see: https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#step-3-app-polls-github-to-check-if-the-user-authorized-the-device
    thread::sleep(FIVE_SECONDS);
    match flow.poll(20) {
        Ok(creds) => {
            save_token(&creds.token);
        }
        Err(e) => eprint!("Authentication error: {}", e),
    };
}

fn command_list() {
    println!("Available Backends:");
    for backend in BackendValues::iter() {
        println!("- {}", backend);
    }
    println!("\nAvailable Benchmarks:");
    for bench in BenchmarkValues::iter() {
        println!("- {}", bench);
    }
}

fn command_run(run_args: RunArgs) {
    let token = get_token_from_cache();
    if run_args.share {
        // Verify if a token is saved
        if token.is_none() {
            eprintln!("You need to be authenticated to be able to share benchmark results.");
            eprintln!("Run the command 'burnbench auth' to authenticate.");
            return;
        }
        // TODO refresh the token when it is expired
        // Check for the validity of the saved token
        if !verify_token(token.as_deref().unwrap()) {
            eprintln!("Your access token is no longer valid.");
            eprintln!("Run the command 'burnbench auth' again to get a new token.");
            return;
        }
    }
    let total_combinations = run_args.backends.len() * run_args.benches.len();
    println!(
        "Executing benchmark and backend combinations in total: {}",
        total_combinations
    );
    let mut app = App::new();
    app.init();
    println!("Running benchmarks...\n");
    app.run(
        &run_args.benches,
        &run_args.backends,
        token.as_deref().filter(|_| run_args.share),
    );
    app.cleanup();
}

#[allow(unused)] // for tui as this is WIP
pub(crate) fn run_cargo(command: &str, params: &[&str]) -> ioResult<ExitStatus> {
    let mut cargo = Command::new("cargo")
        .arg(command)
        .arg("--color=always")
        .args(params)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("cargo process should run");
    cargo.wait()
}

pub(crate) fn run_backend_comparison_benchmarks(
    benches: &[BenchmarkValues],
    backends: &[BackendValues],
    token: Option<&str>,
) {
    // Prefix and postfix for titles
    let filler = ["="; 10].join("");

    // Delete the file containing file paths to benchmark results, if existing
    let benchmark_results_file = dirs::home_dir()
        .expect("Home directory should exist")
        .join(".cache")
        .join("burn")
        .join("backend-comparison")
        .join("benchmark_results.txt");

    fs::remove_file(benchmark_results_file.clone()).ok();

    // Iterate through every combination of benchmark and backend
    for bench in benches.iter() {
        for backend in backends.iter() {
            let bench_str = bench.to_string();
            let backend_str = backend.to_string();
            println!(
                "{}Benchmarking {} on {}{}",
                filler, bench_str, backend_str, filler
            );
            let mut args = vec![
                "-p",
                "backend-comparison",
                "--bench",
                &bench_str,
                "--features",
                &backend_str,
                "--target-dir",
                BENCHMARKS_TARGET_DIR,
            ];
            if let Some(t) = token {
                args.push("--");
                args.push("--sharing-url");
                args.push(USER_BENCHMARK_SERVER_URL);
                args.push("--sharing-token");
                args.push(t);
            }
            let status = run_cargo("bench", &args).unwrap();
            if !status.success() {
                println!(
                    "Benchmark {} didn't ran successfully on the backend {}",
                    bench_str, backend_str
                );
                continue;
            }
        }
    }

    // Iterate though each benchmark result file present in backend-comparison/benchmark_results.txt
    // and print them in a single table.
    let mut benchmark_results = BenchmarkCollection::default();
    if let Ok(file) = fs::File::open(benchmark_results_file.clone()) {
        let file_reader = BufReader::new(file);
        for file in file_reader.lines() {
            let file_path = file.unwrap();
            if let Ok(br_file) = fs::File::open(file_path.clone()) {
                let benchmarkrecord =
                    serde_json::from_reader::<_, BenchmarkRecord>(br_file).unwrap();
                benchmark_results.records.push(benchmarkrecord)
            } else {
                println!("Cannot find the benchmark-record file: {}", file_path);
            };
        }
        println!(
            "{}Benchmark Results{}\n\n{}",
            filler, filler, benchmark_results
        );
        fs::remove_file(benchmark_results_file).ok();
    }
}
