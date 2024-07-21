use std::env;
use std::str::FromStr;
use actix_web::{web, get, App, HttpServer, HttpResponse, Error, HttpRequest, Responder};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde_json::{json, Value};
use std::time::Duration;
use base64::prelude::*;
// use sqlite::{Connection, State};
use sha2::{Sha256, Digest};
use sqlx::{ConnectOptions, Executor, Row};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
/*
use futures::StreamExt;
use reqwest_eventsource::{Event, EventSource};
 */


const PORT: u16 = 4567;

struct AppState {
    pool: SqlitePool,
}

#[get("/status")]
async fn handle_status() -> impl Responder {
    HttpResponse::Ok().body("up")
}


pub struct Hit {
    pub data: Vec<u8>,
}

pub struct Miss {
    pub where_to_find: usize,
}

pub enum CacheHitOrMiss {
    Hit(Vec<u8>),
    Miss(usize),
}

fn hash(input: String) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.finalize().to_vec()
}

async fn proxy_request(
    req: HttpRequest,
    req_body: web::Json<Value>,
    state: web::Data<AppState>,
    client: web::Data<reqwest::Client>,
) -> Result<HttpResponse, Error> {
    let pool = &state.pool;
    // NOTE: Leaving this line here if we end up needing...
    // let connection = state.pool.acquire().await.map_err(|e| actix_web::error::ErrorInternalServerError(e))?;

    /* Start handling */
    let to_base = "api.openai.com";
    let from = req.full_url();
    let mut to = from.clone();
    to.set_host(Option::from(to_base)).unwrap();
    to.set_scheme("https").unwrap();
    to.set_port(Some(443)).unwrap();

    print!("Got request: {:?} -> {:?}", from.as_str(), to.as_str());

    let openai_api_key = req.headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Missing API key"))?
        .to_string();


    if from.path().to_string() == "/v1/embeddings" {
        println!();

        let old_format = req_body["encoding_format"].as_str().map(|x| x.to_string()).unwrap_or("text".into());

        let inputs = match req_body["input"].as_array() {
            Some(x) => x,
            None => return Err(actix_web::error::ErrorBadRequest("Embeddings call missing input")),
        };

        // TODO: Test what happens if these are not provided
        let model = req_body["model"].as_str();
        let dimensions = req_body["dimensions"].as_i64();

        let mut should_query = false;
        let mut to_query = vec![];
        let mut hits_and_misses= vec![];
        for i in 0..inputs.len() {
            let input = inputs[i].as_str().unwrap();
            let cache_key = hash(input.to_string());

            let search= sqlx::query("SELECT * FROM embeddings WHERE hash = ?")
                .bind(cache_key)
                .fetch_one(pool)
                .await;

            let result = if let Ok(row) = search {
                let cached_embedding = row.try_get("value").unwrap();
                CacheHitOrMiss::Hit(cached_embedding)
            } else {
                should_query = true;
                let where_to_find = to_query.len();
                to_query.push(input);
                CacheHitOrMiss::Miss(where_to_find)
            };

            hits_and_misses.push(result)
        }

        let (successes, failures): (Vec<Result<Vec<u8>, Error>>, Vec<Result<Vec<u8>, Error>>) = if should_query {
            println!("Call to embeddings API. Querying {} non-cached items out of {} requested.", to_query.len(), inputs.len());

            let mut b = req_body.clone();
            let req_body_object_mut = b.as_object_mut().unwrap();
            req_body_object_mut.insert("encoding_format".into(), json!("base64"));
            req_body_object_mut.insert("input".into(), json!(to_query));

            let resp = client.post(to)
                .header(AUTHORIZATION, &openai_api_key)
                .timeout(Duration::from_secs(2))
                .json(&req_body)
                .send()
                .await
                .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;

            let resp_json: Value = resp.json().await
                .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;

            // println!("Coe: {:?}", resp_json);
            let mut ret: Vec<_> = vec![];
            let errors: Vec<_> = vec![];
            for hit_or_miss in hits_and_misses {
                let e = match hit_or_miss {
                    CacheHitOrMiss::Hit(embedding) => Ok(embedding),
                    CacheHitOrMiss::Miss(where_to_find) => {
                        let embedding = BASE64_STANDARD.decode(
                            resp_json["data"][where_to_find]["embedding"].as_str().unwrap()
                        )
                            .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;

                        let input = to_query[where_to_find].to_string();
                        let cache_key = hash(input);

                        let query = sqlx::query("INSERT INTO embeddings (model, dimensions, hash, value) VALUES (?, ?, ?, ?)")
                            .bind(&model)
                            .bind(&dimensions)
                            .bind(&cache_key)
                            .bind(&embedding)
                            .execute(pool)
                            .await;

                        match query {
                            Ok(_) => {
                                Ok(embedding)
                            }
                            Err(e) => {
                                println!("Failed to insert into cache: {:?}", e);
                                Err(actix_web::error::ErrorInternalServerError(e))
                            }
                        }
                    }
                };
                ret.push(e)
            }
            (ret, errors)
        } else {
            println!("All cache hits ({})!", inputs.len());
            hits_and_misses.into_iter().map(|hit_or_miss| {
                match hit_or_miss {
                    CacheHitOrMiss::Hit(embedding) => Ok(embedding),
                    CacheHitOrMiss::Miss(_) => unreachable!(),
                }
            }).partition(Result::is_ok)
        };

        if !failures.is_empty() {
            return Err(actix_web::error::ErrorInternalServerError("Failed to get embeddings"));
        }

        let ret: Vec<Vec<u8>> = successes.into_iter().map(Result::unwrap).collect();

        let data: Vec<_> = if old_format == "base64" {
            (0..ret.len()).map(|i| {
                json!({
                "embedding": BASE64_STANDARD.encode(&ret[i]),
                "index": i,
                "object": "embedding"
                })
            }).collect()
        } else if old_format == "float" {
            fn to_float(x: Vec<u8>) -> Vec<f32> {
                x
                .chunks_exact(4)
                .map(|chunk| {
                    let arr: [u8; 4] = chunk.try_into().unwrap();
                    f32::from_le_bytes(arr)
                })
                .collect()
            }
            (0..ret.len()).map(|i| {
                json!({
                    "embedding": to_float(ret[i].clone()),
                    "index": i,
                    "object": "embedding"
                })
            }).collect()
        } else {
            todo!()
        };

        let total_len: usize = ret.iter().map(|x| x.len()).sum();
        return Ok(HttpResponse::Ok().json(json!({
            "data": data,
            "model": req_body["model"],
            "object": "list",
            "usage": {
                "prompt_tokens": total_len / 4,
                "total_tokens": total_len / 4,
            }
        })))
    } else {
        println!(" - Just proxying...");
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_str(&openai_api_key)
            .map_err(|e| actix_web::error::ErrorInternalServerError(e))?);
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));

        let stream = req_body["stream"].as_bool().unwrap_or(false);

        if stream {
            todo!();
            /*
            let es = EventSource::new(
                reqwest::Client::builder()
                    .default_headers(headers)
                    .build()
                    .unwrap()
                    .post("https://api.openai.com/v1/chat/completions")
                    .json(&req_body)
            ).unwrap();

            let (tx, rx) = actix_web::web::Bytes::new_channel();

            actix_web::rt::spawn(async move {
                let mut es = es;
                while let Some(event) = es.next().await {
                    match event {
                        Ok(Event::Message(message)) => {
                            if message.data == "[DONE]" {
                                break;
                            }
                            let _ = tx.send(actix_web::web::Bytes::from(message.data + "\n"));
                        }
                        Ok(Event::Open) => continue,
                        Err(err) => {
                            eprintln!("Error: {:?}", err);
                            break;
                        }
                    }
                }
            });

            Ok(HttpResponse::Ok()
                .content_type("text/event-stream")
                .streaming(rx))
             */
        } else {
            let resp = client.post(to)
                .headers(headers)
                .json(&req_body)
                .send()
                .await
                .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;

            let resp_json: Value = resp.json().await
                .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
            Ok(HttpResponse::Ok().json(resp_json))
        }
    }
}

/* pub struct Embedding<'a> {
} */

#[tokio::main] // By default, tokio_postgres uses the tokio crate as its runtime.
async fn main() -> std::io::Result<()>{
    /* DB setup Code */
    let db_path = &env::var("DATABASE_PATH").unwrap_or("sqlite::memory:".to_string());

    /* Create db and table if needed */
    let mut conn = SqliteConnectOptions::from_str(db_path).unwrap()
        .create_if_missing(true)
        .connect().await.unwrap();
    let query = sqlx::query("CREATE TABLE IF NOT EXISTS embeddings (model TEXT, dimensions INTEGER, hash BYTEA, value BYTEA);");
    conn.execute(query).await.unwrap();

    /* Build connection pool for the app */
    let pool = SqlitePool::connect(db_path).await.unwrap();
    if db_path == "sqlite::memory:" {
        print!("Using in-memory database. ");
    } else {
        print!("Using database at {}. ", db_path);
    }

    let app_state = web::Data::new(AppState {
        pool,
    });


    let client = web::Data::new(reqwest::Client::new());

    println!("Server listening on port: {}", PORT);
    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .app_data(client.clone())
            .service(handle_status)
            .route("/v1/{endpoint:.*}", web::post().to(proxy_request))
    })
        .bind(("0.0.0.0", PORT))?
        .run()
        .await
}
