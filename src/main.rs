use std::{os::unix::fs::PermissionsExt as _, process::Command};

use actix_multipart::Multipart;
use actix_web::{App, HttpResponse, HttpServer, post};
use futures_util::TryStreamExt as _; // Commonly used alias for stream processing
use once_cell::sync::Lazy;
use rand::seq::IndexedRandom;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::fs;
mod utils;
use crate::utils::Db;

#[derive(Serialize, Deserialize)]
struct Config {
    id: String,
    sub: String,
    port: u64,
    build_commands: Vec<String>,
    run_command: String,
}

static ADJ: Lazy<Vec<&str>> = Lazy::new(move || {
    vec![
        "impeccable",
        "ubiquitous",
        "catchy",
        "slippery",
        "overbearing",
        "quick",
        "nimble",
        "simple",
        "complex",
        "golden",
        "cooked",
    ]
});
static NOUN: Lazy<Vec<&str>> = Lazy::new(move || {
    vec![
        "octopus", "project", "waste", "fox", "car", "place", "gold", "silver", "diamond", "slinky",
    ]
});

#[post("/deploy")]
async fn deploy(mut payload: Multipart) -> actix_web::Result<HttpResponse> {
    println!("Deploy hit!");
    let db = Db {};
    let mut rng = rand::rng();
    let mut fin = vec![];
    let mut project_name = None;
    let mut tld = None;
    let mut build_commands = None;
    let mut run_command = None;
    for _ in 0..11 {
        let candidate = format!(
            "{}-{}",
            ADJ.choose(&mut rng).unwrap(),
            NOUN.choose(&mut rng).unwrap()
        );
        if db.get(&candidate).await.ok().flatten().is_none() {
            tld = Some(candidate);
            break;
        }
    }

    while let Some(mut field) = payload.try_next().await? {
        let Some(content_disposition) = field.content_disposition() else {
            return Ok(HttpResponse::BadRequest()
                .json(json!({"error": "Error with file upload. Please try again later"})));
        };
        let Some(name) = content_disposition.get_name() else {
            return Ok(HttpResponse::BadRequest()
                .json(json!({"error": "Error with File upload. Please try again later"})));
        };
        if name == "metadata" {
            let mut data = Vec::new();
            while let Some(chunk) = field.try_next().await? {
                data.extend_from_slice(&chunk);
            }
            let meta: serde_json::Value = serde_json::from_slice(&data)?;
            project_name = Some(
                meta["name"]
                    .as_str()
                    .unwrap_or("Sparklane Cloud Project")
                    .to_string(),
            );
            if let Some(t) = meta["project"].as_str() {
                if db.get(t).await.ok().flatten().is_none() {
                    tld = Some(t.into());
                }
            }
            if let Some(r) = meta["build"].as_array() {
                build_commands = Some(
                    r.iter()
                        .filter_map(move |i| i.as_str())
                        .map(move |i| i.to_string())
                        .collect(),
                );
            }
            run_command = meta["run"].as_str().map(move |o| o.to_string());
        } else if name == "file" {
            while let Some(chunk) = field.try_next().await? {
                fin.extend_from_slice(&chunk);
            }
        }
    }
    let files = match utils::extract_zip_to_vec(&fin) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Exteacting error: {e}");
            return Err(actix_web::error::ErrorBadRequest(
                "Couldn't process your code files, please try again later.",
            ));
        }
    };
    let id = uuid::Uuid::new_v4().to_string();
    let config = Config {
        id,
        port: 8080,
        sub: match tld {
            Some(r) => r,
            None => {
                return Err(actix_web::error::ErrorBadRequest(
                    "Your project identifier was taken and we couldn't generate a new one, please try again later.",
                ));
            }
        },
        build_commands: match build_commands {
            Some(o) => o,
            None => {
                return Err(actix_web::error::ErrorBadRequest(
                    "No build command in config.",
                ));
            }
        },
        run_command: match run_command {
            Some(r) => r,
            None => {
                return Err(actix_web::error::ErrorBadRequest(
                    "No run command in config.",
                ));
            }
        },
    };

    utils::spin(&config, files).await?;

    println!("Vm spinning at {}", config.id);

    Ok(HttpResponse::Ok().json(json!({})))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let db = unsafe { foundationdb::boot() };

    println!("Server starting on http://localhost:8096/");
    HttpServer::new(move || App::new().service(deploy))
        .bind(("0.0.0.0", 8096))?
        .run()
        .await
        .ok();
    std::mem::drop(db);
    Ok(())
}
