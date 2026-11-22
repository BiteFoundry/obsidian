use std::process::ExitCode;

use obsidian_cli::{
    cli::{self, Cli},
    models::CodedError,
};

#[tokio::main]
async fn main() -> ExitCode {
    let parsed = Cli::parse_args();
    let json = parsed.json;
    match cli::run(parsed).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if json {
                let (code, candidates) = match err.downcast_ref::<CodedError>() {
                    Some(coded) => (coded.code, coded.candidates.as_slice()),
                    None => ("error", &[][..]),
                };
                let mut error_obj = serde_json::Map::new();
                error_obj.insert(
                    "code".to_string(),
                    serde_json::Value::String(code.to_string()),
                );
                error_obj.insert(
                    "message".to_string(),
                    serde_json::Value::String(format!("{err:#}")),
                );
                if !candidates.is_empty() {
                    error_obj.insert(
                        "candidates".to_string(),
                        serde_json::Value::Array(
                            candidates
                                .iter()
                                .cloned()
                                .map(serde_json::Value::String)
                                .collect(),
                        ),
                    );
                }
                let payload = serde_json::json!({
                    "ok": false,
                    "error": error_obj,
                });
                println!("{payload}");
            } else {
                eprintln!("Error: {err:?}");
            }
            ExitCode::FAILURE
        }
    }
}
