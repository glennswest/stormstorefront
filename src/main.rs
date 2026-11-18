//! `stormstorefront <role> [args]`: one static binary, every container's role.
//! See `storefront::ROLES` and the README's table of containers.

use std::process::ExitCode;
use storefront::{api, cache, content, edge, exporter, sidecars, store, validate, web, ROLES};

fn usage() -> ExitCode {
    eprintln!("usage: stormstorefront <role> [args]\nroles: {}", ROLES.join(" "));
    ExitCode::from(64)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let Some(role) = args.get(1).map(String::as_str) else { return usage() };
    let rest = &args[2..];
    let result = match role {
        "edge" => edge::run(),
        "config-reloader" | "reloader" => sidecars::config_reloader(),
        "validate-config" => sidecars::validate_config(),
        "web" => web::run(),
        "content-init" => content::run(true),
        "content-sync" => content::run(false),
        "exporter" => exporter::run(rest),
        "api" => api::run(),
        "access-log" => sidecars::access_log(),
        "cache-warmer" => sidecars::cache_warmer(),
        "wait-for-deps" => sidecars::wait_for_deps(),
        "store" => store::run(),
        "backup" => sidecars::backup(),
        "cache" => cache::run(),
        "sleep" => {
            // A preStop hook's drain: the image has no shell and no sleep(1).
            let secs: u64 = rest.first().and_then(|s| s.parse().ok()).unwrap_or(5);
            std::thread::sleep(std::time::Duration::from_secs(secs));
            Ok(())
        }
        "validate" => {
            let dir = rest.first().map(String::as_str).unwrap_or("manifests");
            match validate::validate_dir(std::path::Path::new(dir), ROLES) {
                Ok(report) => {
                    print!("{}", report.render());
                    return if report.ok() { ExitCode::SUCCESS } else { ExitCode::FAILURE };
                }
                Err(e) => Err(e),
            }
        }
        "version" => {
            println!("stormstorefront {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "-h" | "--help" | "help" => return usage(),
        other => {
            eprintln!("unknown role {other:?}");
            return usage();
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[{role}] {e}");
            ExitCode::FAILURE
        }
    }
}
