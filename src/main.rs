use std::env;
use std::io::Write;
use std::str::FromStr;
use actix_web::{web, get, App, HttpServer, HttpResponse, Error, HttpRequest, Responder};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde_json::{json, Value};
use std::time::Duration;
use base64::prelude::*;
use futures::{pin_mut, stream, StreamExt};
use reqwest::{Response, StatusCode};
// use sqlite::{Connection, State};
use sha2::{Sha256, Digest};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
use serde::Deserialize;
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
            let response = client.post(to)
                .headers(headers)
                .json(&req_body)
                .send()
                .await.unwrap();

            if response.status() != StatusCode::OK {
                panic!("something went wrong!")
            };

            return do_stream(response).await
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
    let options = SqliteConnectOptions::from_str(db_path).unwrap()
        .create_if_missing(true);

    let pool = SqlitePool::connect_with(options).await.unwrap();

    let query = sqlx::query("CREATE TABLE IF NOT EXISTS embeddings (model TEXT, dimensions INTEGER, hash BYTEA, value BYTEA);");
    query.execute(&pool).await.unwrap();

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

#[derive(Debug, Deserialize)]
struct ChatChunkDelta {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatChunkChoice {
    delta: ChatChunkDelta,
    index: usize,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChunk {
    id: String,
    object: String,
    created: usize,
    model: String,
    choices: Vec<ChatChunkChoice>,
}

#[derive(Debug, Deserialize)]
struct APIError{
    message: String,
}

#[derive(Debug, Deserialize)]
struct APIErrorResponse {
    error: APIError,
}

async fn do_stream(response: Response) -> Result<HttpResponse, Error> {
    let stream = response.bytes_stream();
    pin_mut!(stream);

    let streaming_completion_marker = "[DONE]";
    let mut previous_chunk_buffer = "".to_owned();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        let chunk_string = match std::str::from_utf8(&chunk) {
            Ok(value) => value,
            Err(error) => panic!("Invalid UTF-8 sequence: {}", error),
        };

        let chunk_string = previous_chunk_buffer + chunk_string;
        previous_chunk_buffer = "".to_owned();

        let split_data = chunk_string.trim().split("data:");
        for (index, data_chunk) in split_data.to_owned().enumerate() {
            let data_chunk = data_chunk.trim();
            if data_chunk.is_empty() {
                continue;
            }
            if data_chunk == streaming_completion_marker {
                break;
            }
            // println!("{:?}", data_chunk);
            let data_value = serde_json::from_str::<ChatCompletionChunk>(data_chunk);
            let data_value = match data_value {
                Ok(value) => value,
                Err(_) => {
                    let _ = match serde_json::from_str::<APIErrorResponse>(data_chunk) {
                        Ok(e) => panic!("{}", e.error.message),
                        Err(_) => {
                            // Chunk ends in a partial JSON
                            if index == split_data.to_owned().count() - 1 {
                                previous_chunk_buffer = "data: ".to_owned() + &data_chunk;
                                break;
                            } else {
                                panic!("Unknown error!")
                            }
                        }
                    };
                }
            };
            // println!("{:?}", data_value);
            let choice = data_value.choices.first().expect("No choices available");
            if let Some(content) = &choice.delta.content {
                print!("{}", content);
            }
            std::io::stdout().flush()?;
        }
    };

    Ok(HttpResponse::Ok().json(json!("hello")))
}